// SPDX-License-Identifier: MIT
//! What a user of the gates' headless compositor sees, as pixels: `zwlr_screencopy_manager_v1` on the nested labwc, into a `wl_shm` buffer of a memfd.
//! Why the compositor's output and not the buffer the proxy forwards: the output is the composited result (GTK chrome, subsurfaces, dialogs), whatever buffer type the client attached; a dmabuf the proxy forwards is unreadable from here, and it would be one buffer of several. The memfd is read with `read_at`, not mapped: xtask forbids unsafe.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{MemfdFlags, memfd_create};

use crate::nested_wayland::{Arg, NestedCompositor, REGISTRY_BIND, word};

const SCREENCOPY: &str = "zwlr_screencopy_manager_v1";
/// v3 announces `buffer_done`; the first `buffer` offer (shm) is all this client needs.
const SCREENCOPY_VERSION: u32 = 3;
const MANAGER_CAPTURE_OUTPUT: u16 = 0;
const FRAME_COPY: u16 = 0;
const FRAME_EVENT_BUFFER: u16 = 0;
const FRAME_EVENT_FLAGS: u16 = 1;
const FRAME_EVENT_READY: u16 = 2;
const FRAME_EVENT_FAILED: u16 = 3;
/// `zwlr_screencopy_frame_v1.flags`: rows arrive bottom up.
const FLAG_Y_INVERT: u32 = 1;
const SHM_CREATE_POOL: u16 = 0;
const POOL_CREATE_BUFFER: u16 = 0;
/// wl_shm formats: the two fixed codes, then the drm fourccs "XB24" / "AB24". Memory order is B G R X for the first two, R G B X for the others (little endian).
const SHM_ARGB8888: u32 = 0;
const SHM_XRGB8888: u32 = 1;
const SHM_XBGR8888: u32 = 0x3432_4258;
const SHM_ABGR8888: u32 = 0x3432_4241;
const BYTES_PER_PIXEL: usize = 4;
/// A frame comes at the output's next repaint, a period at 60 Hz; this only bounds a hang.
const CAPTURE_WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(10);

/// RGBA pixels, rows top down, alpha opaque.
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    /// The part at (x, y) of size w x h; it must lie inside the image.
    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Result<Image> {
        ensure!(
            w > 0 && h > 0 && x + w <= self.width && y + h <= self.height,
            "{w}x{h} at ({x}, {y}) is not inside the {}x{} output",
            self.width,
            self.height
        );
        let mut rgba = Vec::with_capacity(w as usize * h as usize * BYTES_PER_PIXEL);
        for row in y..y + h {
            let at = (row as usize * self.width as usize + x as usize) * BYTES_PER_PIXEL;
            rgba.extend_from_slice(&self.rgba[at..at + w as usize * BYTES_PER_PIXEL]);
        }
        Ok(Image {
            width: w,
            height: h,
            rgba,
        })
    }

    /// One colour everywhere: nothing was drawn, or nothing was captured.
    pub fn is_blank(&self) -> bool {
        let (pixels, _) = self.rgba.as_chunks::<BYTES_PER_PIXEL>();
        pixels.iter().all(|p| *p == pixels[0])
    }

    pub fn write_png(&self, file: &Path) -> Result<()> {
        if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let out = File::create(file).with_context(|| format!("creating {}", file.display()))?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(out), self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&self.rgba)?;
        writer.finish()?;
        Ok(())
    }

    pub fn read_png(file: &Path) -> Result<Image> {
        let input = File::open(file).with_context(|| format!("opening {}", file.display()))?;
        let mut reader = png::Decoder::new(std::io::BufReader::new(input)).read_info()?;
        let size = reader
            .output_buffer_size()
            .context("the PNG is too large")?;
        let mut rgba = vec![0; size];
        let info = reader.next_frame(&mut rgba)?;
        ensure!(
            info.color_type == png::ColorType::Rgba && info.bit_depth == png::BitDepth::Eight,
            "{}: not 8-bit RGBA",
            file.display()
        );
        rgba.truncate(info.buffer_size());
        Ok(Image {
            width: info.width,
            height: info.height,
            rgba,
        })
    }
}

/// The compositor's first output as it is at its next repaint. One connection per capture: a shot is rare, and no state outlives it.
pub fn capture(nested: &NestedCompositor) -> Result<Image> {
    let (mut c, registry, globals) = nested.connect()?;
    let mut bind = |iface: &str, version: u32| -> Result<u32> {
        let &(name, offered) = globals
            .get(iface)
            .with_context(|| format!("the compositor has no {iface}"))?;
        let id = c.new_id();
        c.send(
            registry,
            REGISTRY_BIND,
            &[
                Arg::U(name),
                Arg::S(iface),
                Arg::U(offered.min(version)),
                Arg::U(id),
            ],
        );
        Ok(id)
    };
    let shm = bind("wl_shm", 1)?;
    let output = bind("wl_output", 1)?;
    let manager = bind(SCREENCOPY, SCREENCOPY_VERSION)?;
    let frame = c.new_id();
    c.send(
        manager,
        MANAGER_CAPTURE_OUTPUT,
        &[Arg::U(frame), Arg::I(0), Arg::U(output)],
    );
    let deadline = Instant::now() + CAPTURE_WAIT;
    let mut offered = None;
    let (format, width, height, stride) = loop {
        for (object, opcode, body) in c.roundtrip()? {
            if object != frame {
                continue;
            }
            match opcode {
                FRAME_EVENT_BUFFER => {
                    let shape = (
                        word(&body, 0)?,
                        word(&body, 4)?,
                        word(&body, 8)?,
                        word(&body, 12)?,
                    );
                    offered.get_or_insert(shape);
                }
                FRAME_EVENT_FAILED => bail!("the compositor refused the capture"),
                _ => {}
            }
        }
        if let Some(shape) = offered {
            break shape;
        }
        ensure!(
            Instant::now() < deadline,
            "no buffer offer within {CAPTURE_WAIT:?}"
        );
        std::thread::sleep(POLL);
    };
    let size = stride as usize * height as usize;
    let file = File::from(memfd_create("atl-screencopy", MemfdFlags::CLOEXEC)?);
    file.set_len(size as u64)?;
    let pool = c.new_id();
    c.attach(file.try_clone()?.into());
    c.send(
        shm,
        SHM_CREATE_POOL,
        &[Arg::U(pool), Arg::I(i32::try_from(size)?)],
    );
    let buffer = c.new_id();
    c.send(
        pool,
        POOL_CREATE_BUFFER,
        &[
            Arg::U(buffer),
            Arg::I(0),
            Arg::I(i32::try_from(width)?),
            Arg::I(i32::try_from(height)?),
            Arg::I(i32::try_from(stride)?),
            Arg::U(format),
        ],
    );
    c.send(frame, FRAME_COPY, &[Arg::U(buffer)]);
    let mut flags = 0;
    'copy: loop {
        for (object, opcode, body) in c.roundtrip()? {
            if object != frame {
                continue;
            }
            match opcode {
                FRAME_EVENT_FLAGS => flags = word(&body, 0)?,
                FRAME_EVENT_READY => break 'copy,
                FRAME_EVENT_FAILED => bail!("the compositor failed the copy"),
                _ => {}
            }
        }
        ensure!(
            Instant::now() < deadline,
            "no frame within {CAPTURE_WAIT:?}"
        );
        std::thread::sleep(POLL);
    }
    let mut raw = vec![0; size];
    file.read_exact_at(&mut raw, 0)?;
    to_rgba(
        &raw,
        format,
        width,
        height,
        stride,
        flags & FLAG_Y_INVERT != 0,
    )
}

/// The shm buffer as [`Image`] pixels: channels to R G B A, stride padding dropped, alpha set opaque (X formats leave it undefined).
fn to_rgba(
    raw: &[u8],
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
    y_invert: bool,
) -> Result<Image> {
    let swap = match format {
        SHM_ARGB8888 | SHM_XRGB8888 => true,
        SHM_XBGR8888 | SHM_ABGR8888 => false,
        other => bail!("wl_shm format {other:#x} is not a 32-bit RGB one"),
    };
    let mut rgba = Vec::with_capacity(width as usize * height as usize * BYTES_PER_PIXEL);
    for y in 0..height as usize {
        let row = if y_invert { height as usize - 1 - y } else { y };
        let at = row * stride as usize;
        let (pixels, _) =
            raw[at..at + width as usize * BYTES_PER_PIXEL].as_chunks::<BYTES_PER_PIXEL>();
        for p in pixels {
            let (r, b) = if swap { (p[2], p[0]) } else { (p[0], p[2]) };
            rgba.extend_from_slice(&[r, p[1], b, u8::MAX]);
        }
    }
    Ok(Image {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(pixels: &[[u8; 4]], width: u32) -> Image {
        Image {
            width,
            height: pixels.len() as u32 / width,
            rgba: pixels.concat(),
        }
    }

    /// One red pixel in memory: B G R X for XRGB8888, R G B X for XBGR8888; the stride's padding is dropped, and y_invert swaps the rows.
    #[test]
    fn channels_stride_and_rows_are_converted() {
        let xrgb = [0, 0, 255, 9, 7, 7, 7, 7, 255, 0, 0, 9, 7, 7, 7, 7];
        let got = to_rgba(&xrgb, SHM_XRGB8888, 1, 2, 8, false).unwrap();
        assert_eq!(got.rgba, [255, 0, 0, 255, 0, 0, 255, 255]);
        let got = to_rgba(&xrgb, SHM_XRGB8888, 1, 2, 8, true).unwrap();
        assert_eq!(got.rgba, [0, 0, 255, 255, 255, 0, 0, 255]);
        let xbgr = [255, 0, 0, 9];
        let got = to_rgba(&xbgr, SHM_XBGR8888, 1, 1, 4, false).unwrap();
        assert_eq!(got.rgba, [255, 0, 0, 255]);
        assert!(to_rgba(&xbgr, 0x1234, 1, 1, 4, false).is_err());
    }

    #[test]
    fn blank_means_one_colour() {
        let (a, b) = ([1, 2, 3, 255], [1, 2, 4, 255]);
        assert!(image(&[a, a, a, a], 2).is_blank());
        assert!(!image(&[a, a, a, b], 2).is_blank());
    }

    #[test]
    fn crop_takes_the_rectangle_and_refuses_outside() {
        let (a, b, c, d) = ([1; 4], [2; 4], [3; 4], [4; 4]);
        let got = image(&[a, b, c, d], 2).crop(1, 0, 1, 2).unwrap();
        assert_eq!(got.rgba, [b, d].concat());
        assert!(image(&[a, b, c, d], 2).crop(1, 0, 2, 1).is_err());
    }

    #[test]
    fn png_round_trips() {
        let dir = std::env::temp_dir().join(format!("atl-png-{}", std::process::id()));
        let file = dir.join("a.png");
        let want = image(&[[1, 2, 3, 255], [4, 5, 6, 255]], 2);
        want.write_png(&file).unwrap();
        let got = Image::read_png(&file).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(got == want);
    }
}
