// SPDX-License-Identifier: MIT
//! Where a frame goes: the platform layer's presentation (M3 slice 6 puts the window here). Until an app process has a window, [`ReportSink`] is the sink: it reads every quad's buffer through atl-scene-buffer, the way the
//! window's presenter does, and reports what it saw as one line per quad, so the buffers a client posted can be checked against the pixels it drew (`xtask test composer`).

use std::io::Write;

use atl_compositor::scene::Scene;
use atl_scene_buffer::Pixels;

/// Called on the present thread (never the clock thread: a sink may take longer than a period), once per tick that changed anything visible.
pub trait SceneSink: Send {
    /// A new scene.
    fn present(&mut self, scene: &Scene);
}

/// Runs `on_first` once, at the first scene with quads (the app's first frame reaching the compositor), and passes every scene on to `inner`.
pub struct FirstFrame<S: SceneSink> {
    inner: S,
    on_first: Option<Box<dyn FnOnce() + Send>>,
}

impl<S: SceneSink> FirstFrame<S> {
    pub fn new(inner: S, on_first: impl FnOnce() + Send + 'static) -> Self {
        Self {
            inner,
            on_first: Some(Box::new(on_first)),
        }
    }
}

impl<S: SceneSink> SceneSink for FirstFrame<S> {
    fn present(&mut self, scene: &Scene) {
        if let Some(on_first) = self.on_first.take_if(|_| !scene.quads.is_empty()) {
            on_first();
        }
        self.inner.present(scene);
    }
}

/// FNV-1a, 64 bit: the digest a quad's pixels are reported by (offset basis 0xcbf29ce484222325, prime 0x100000001b3). Not a security hash: a short, order-sensitive fingerprint both sides compute alike.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes
        .iter()
        .fold(OFFSET_BASIS, |h, &b| (h ^ u64::from(b)).wrapping_mul(PRIME))
}

/// The digest of `pixels` as the picture, not the memory: the rows of `width` pixels (4 bytes each) in order, without the padding up to the stride.
pub fn pixels_digest(pixels: &Pixels) -> u64 {
    let row = pixels.width as usize * 4;
    let rows: Vec<u8> = pixels
        .bytes
        .chunks(pixels.stride_bytes)
        .take(pixels.height as usize)
        .flat_map(|r| r[..row].iter().copied())
        .collect();
    fnv1a64(&rows)
}

/// Writes `composer: scene` and `composer: quad` lines to `W`; the quad line carries the digest of its buffer ([`pixels_digest`]) or why the buffer could not be read.
pub struct ReportSink<W: Write + Send>(pub W);

impl<W: Write + Send> SceneSink for ReportSink<W> {
    fn present(&mut self, scene: &Scene) {
        let _ = writeln!(
            self.0,
            "composer: scene frame={} quads={}",
            scene.frame,
            scene.quads.len()
        );
        for q in &scene.quads {
            let b = &q.buffer;
            let what = match atl_scene_buffer::read_pixels(b) {
                Ok(p) => format!("digest={:016x}", pixels_digest(&p)),
                Err(e) => format!("unreadable={e:?}"),
            };
            let _ = writeln!(
                self.0,
                "composer: quad frame={} layer={} name={} buffer={}x{} frame_number={} dst={},{},{},{} {what}",
                scene.frame,
                q.layer_id,
                q.name,
                b.width,
                b.height,
                q.frame_number,
                q.dst.left,
                q.dst.top,
                q.dst.right,
                q.dst.bottom
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::sync::Arc;

    use atl_compositor::scene::{Quad, SceneBuffer, SceneFd};
    use atl_compositor::state::FloatRect;
    use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
    use rustix::io::write;

    use super::*;

    /// The published FNV-1a 64 test vectors (Fowler/Noll/Vo): empty input is the offset basis.
    #[test]
    fn fnv1a64_known_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// 2x2 pixels in rows of 3 pixels (stride 12 bytes): the digest is of the two 8-byte rows, not of the padding between them.
    #[test]
    fn digest_skips_the_stride_padding() {
        let bytes: Vec<u8> = (0..24).collect();
        let p = Pixels {
            width: 2,
            height: 2,
            stride_bytes: 12,
            bytes: bytes.clone(),
        };
        let picture: Vec<u8> = [&bytes[0..8], &bytes[12..20]].concat();
        assert_eq!(pixels_digest(&p), fnv1a64(&picture));
    }

    fn memfd_with(bytes: &[u8]) -> OwnedFd {
        let fd = memfd_create("t", MemfdFlags::CLOEXEC).expect("memfd");
        ftruncate(&fd, bytes.len() as u64).expect("size");
        assert_eq!(write(&fd, bytes).expect("write"), bytes.len());
        fd
    }

    /// A scene with one quad whose buffer is a memfd of 2x1 pixels: the report names the layer, the buffer, the placement and the digest of those 8 bytes.
    #[test]
    fn report_has_one_line_per_quad_with_its_digest() {
        let pixels = [10u8, 20, 30, 255, 40, 50, 60, 255];
        let fd = memfd_with(&pixels);
        let quad = Quad {
            layer_id: 7,
            name: "child".into(),
            buffer: Arc::new(SceneBuffer {
                width: 2,
                height: 1,
                stride: 2,
                format: 1,
                usage: 0,
                id: 5,
                fds: vec![SceneFd::new(fd.as_raw_fd())],
                ints: vec![],
            }),
            frame_number: 3,
            acquire_fence: None,
            src: FloatRect {
                left: 0.0,
                top: 0.0,
                right: 2.0,
                bottom: 1.0,
            },
            dst: FloatRect {
                left: 1.0,
                top: 2.0,
                right: 3.0,
                bottom: 3.0,
            },
            clip: None,
            alpha: 1.0,
            opaque: false,
            buffer_transform: 0,
        };
        let scene = Scene {
            frame: 9,
            layer_stack: 0,
            quads: vec![quad],
        };
        let mut sink = ReportSink(Vec::new());
        sink.present(&scene);
        let text = String::from_utf8(sink.0).expect("utf8");
        let digest = fnv1a64(&pixels);
        assert_eq!(
            text,
            format!(
                "composer: scene frame=9 quads=1\ncomposer: quad frame=9 layer=7 name=child buffer=2x1 frame_number=3 dst=1,2,3,3 digest={digest:016x}\n"
            )
        );
    }
}
