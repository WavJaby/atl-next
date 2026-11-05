// SPDX-License-Identifier: MIT
//! atl-gralloc-probe: the two-process client of `xtask test gralloc`, AOSP libui through the NDK `AHardwareBuffer` API (libnativewindow -> GraphicBuffer -> Gralloc5 -> our IAllocator + mapper.atl.so).
//!
//! `writer --sock N`: allocates a 64x64 RGBA_8888 buffer (CPU read + write), checks what libui reports of it (`describe`, `getId`, `lockPlanes`: libui decodes our standard metadata with AOSP's own reader), locks for write, fills the analytic pattern, unlocks, sends the handle over unix socket N followed by its id, waits for the reader, then locks again and expects the reader's write-back.
//! `reader --sock N`: the second process. Receives the handle (libui's `unflatten` calls `importBuffer` of mapper.atl.so in this process), checks `describe` and the id equal the writer's, locks for read and compares every pixel, then writes one pixel back.
//! Output: one line per case, `gralloc: pass <role>:<case>` or `gralloc: FAIL <role>:<case>: <why>`; exit status 1 on any FAIL. The harness (xtask gralloc.rs) holds the list of cases.

// the empty library carries the converter libraries build.rs links (libnativewindow, libbinder): linking it is what passes them to this binary's link
extern crate atl_gralloc_truth as _;

use std::io::Write;
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};

/// `AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM` (hardware_buffer.h).
const FORMAT_R8G8B8A8: u32 = 1;
/// `AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN`, `_CPU_WRITE_OFTEN`, `_GPU_SAMPLED_IMAGE`.
const CPU_READ_OFTEN: u64 = 3;
const CPU_WRITE_OFTEN: u64 = 3 << 4;
const GPU_SAMPLED_IMAGE: u64 = 1 << 8;

/// The buffer's edge. 64 x 64 x 4 B = 16 KiB, four pages: more than one so a short mapping would show; small enough to compare every byte.
const EDGE: u32 = 64;

/// `AHardwareBuffer_Desc` (hardware_buffer.h:365). Size asserted below against the header's field list: 4 x u32, u64, u32 + u32, u64.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Desc {
    width: u32,
    height: u32,
    layers: u32,
    format: u32,
    usage: u64,
    stride: u32,
    rfu0: u32,
    rfu1: u64,
}
const _: () = assert!(size_of::<Desc>() == 40 && align_of::<Desc>() == 8);

/// `AHardwareBuffer_Plane` / `AHardwareBuffer_Planes` (hardware_buffer.h:387, :397).
#[repr(C)]
#[derive(Clone, Copy)]
struct Plane {
    data: *mut u8,
    pixel_stride: u32,
    row_stride: u32,
}
#[repr(C)]
struct Planes {
    plane_count: u32,
    planes: [Plane; 4],
}
const _: () = assert!(size_of::<Plane>() == 16 && size_of::<Planes>() == 72);

/// `ARect` (android/rect.h).
#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct Buffer {
    _opaque: [u8; 0],
}

// resolved by the link arguments build.rs prints (libnativewindow)
unsafe extern "C" {
    fn AHardwareBuffer_allocate(desc: *const Desc, out: *mut *mut Buffer) -> i32;
    fn AHardwareBuffer_release(b: *mut Buffer);
    fn AHardwareBuffer_describe(b: *const Buffer, out: *mut Desc);
    fn AHardwareBuffer_lockPlanes(
        b: *mut Buffer,
        usage: u64,
        fence: i32,
        rect: *const Rect,
        out: *mut Planes,
    ) -> i32;
    fn AHardwareBuffer_unlock(b: *mut Buffer, fence: *mut i32) -> i32;
    fn AHardwareBuffer_sendHandleToUnixSocket(b: *const Buffer, socket: i32) -> i32;
    fn AHardwareBuffer_recvHandleFromUnixSocket(socket: i32, out: *mut *mut Buffer) -> i32;
    fn AHardwareBuffer_getId(b: *const Buffer, out: *mut u64) -> i32;
    // vndk/hardware_buffer.h (LLNDK): the buffer's native_handle_t, owned by the buffer
    fn AHardwareBuffer_getNativeHandle(b: *const Buffer) -> *const NativeHandle;
}

/// `native_handle_t` (cutils/native_handle.h): version, numFds, numInts, then `data[numFds + numInts]`.
#[repr(C)]
struct NativeHandle {
    version: i32,
    num_fds: i32,
    num_ints: i32,
    data: [i32; 0],
}

/// What a dma-buf importer reads: the layout accessor (`atl_gralloc::layout`, which `atl-scene-buffer`'s `DmabufView` carries) on the handle libui imported here, against libui's own `describe()` stride and the plane fd's real size.
fn check_layout(b: *const Buffer, d: &Desc) -> Result<()> {
    use atl_gralloc::format::{DRM_FORMAT_ABGR8888, DRM_FORMAT_MOD_LINEAR};
    // SAFETY: `b` is a live buffer; the handle it returns lives as long as `b`, which outlives this call.
    let h = unsafe { AHardwareBuffer_getNativeHandle(b) };
    ensure!(!h.is_null(), "getNativeHandle: null");
    // SAFETY: `h` points to a native_handle_t whose header is three ints, valid while `b` lives.
    let (num_fds, num_ints) = unsafe { ((*h).num_fds, (*h).num_ints) };
    let (Ok(nf), Ok(ni)) = (usize::try_from(num_fds), usize::try_from(num_ints)) else {
        bail!("handle counts {num_fds} / {num_ints}");
    };
    // SAFETY: `data` holds `numFds + numInts` ints after the header (native_handle.h), all within the handle's allocation.
    let data = unsafe { std::slice::from_raw_parts((*h).data.as_ptr(), nf + ni) };
    let layout = atl_gralloc::layout::from_handle(nf, &data[nf..])
        .map_err(|e| anyhow::anyhow!("layout: {e:?}"))?;
    ensure!(
        layout.pixel_stride() == d.stride,
        "layout stride {} pixels, describe() {}",
        layout.pixel_stride(),
        d.stride
    );
    ensure!(
        (layout.fourcc, layout.modifier) == (DRM_FORMAT_ABGR8888, DRM_FORMAT_MOD_LINEAR),
        "fourcc {:#x} modifier {:#x}",
        layout.fourcc,
        layout.modifier
    );
    let plane = layout.planes[0];
    let fd = data[plane.fd_index];
    ensure!(fd >= 0, "plane fd {fd}");
    // SAFETY: the handle's fds are open in this process while `b` lives (libui imported them); the borrow ends in this call.
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    let size = rustix::fs::fstat(fd)
        .context("fstat of the plane fd")?
        .st_size;
    let need = plane.offset + u64::from(plane.stride_bytes) * u64::from(layout.height);
    ensure!(
        u64::try_from(size).is_ok_and(|s| s >= need),
        "plane fd holds {size} bytes, the layout needs {need}"
    );
    Ok(())
}

/// `struct __android_log_message` is passed through untouched: only its address crosses, from liblog's stderr logger back to liblog.
#[repr(C)]
struct LogMessage {
    _opaque: [u8; 0],
}

// liblog (system/logging/liblog/include/android/log.h): libui's ALOGW lines are the only diagnostics of a Gralloc5 failure, and nothing prints them by default off Android
unsafe extern "C" {
    fn __android_log_set_logger(logger: Option<unsafe extern "C" fn(*const LogMessage)>);
    fn __android_log_stderr_logger(message: *const LogMessage);
}

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    role: Role,
}

#[derive(Subcommand)]
enum Role {
    Writer {
        /// An inherited unix socket to the reader.
        #[arg(long)]
        sock: i32,
    },
    Reader {
        #[arg(long)]
        sock: i32,
    },
}

/// The pattern byte `c` (0..4) of pixel (x, y): R = x, G = y, B = x ^ y, A = 0xFF. Analytic, not read from anything.
fn expected(x: u32, y: u32, c: usize) -> u8 {
    match c {
        0 => x as u8,
        1 => y as u8,
        2 => (x ^ y) as u8,
        _ => 0xFF,
    }
}

/// The pixel the reader writes back, and its value.
const WRITE_BACK: (u32, u32) = (EDGE - 1, EDGE - 1);
const WRITE_BACK_VALUE: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];

struct Report {
    role: &'static str,
    failed: bool,
}

impl Report {
    fn check(&mut self, case: &str, result: Result<()>) {
        let mut out = std::io::stdout();
        match result {
            Ok(()) => {
                let _ = writeln!(out, "gralloc: pass {}:{case}", self.role);
            }
            Err(e) => {
                self.failed = true;
                let _ = writeln!(out, "gralloc: FAIL {}:{case}: {e:#}", self.role);
            }
        }
        let _ = out.flush();
    }
}

fn describe(b: *const Buffer) -> Desc {
    let mut d = Desc::default();
    // SAFETY: `b` is a live buffer; `d` is a valid out-parameter of the header's layout (asserted above).
    unsafe { AHardwareBuffer_describe(b, &mut d) };
    d
}

fn id(b: *const Buffer) -> Result<u64> {
    let mut v = 0u64;
    // SAFETY: `b` is a live buffer, `v` a valid out-parameter.
    let r = unsafe { AHardwareBuffer_getId(b, &mut v) };
    ensure!(r == 0, "AHardwareBuffer_getId: {r}");
    Ok(v)
}

/// A locked buffer: its single plane's memory and strides, unlocked on drop.
struct Locked {
    b: *mut Buffer,
    plane: Plane,
    count: u32,
}

impl Locked {
    fn new(b: *mut Buffer, usage: u64) -> Result<Self> {
        let mut planes = Planes {
            plane_count: 0,
            planes: [Plane {
                data: std::ptr::null_mut(),
                pixel_stride: 0,
                row_stride: 0,
            }; 4],
        };
        // SAFETY: `b` is a live buffer, no fence (-1), whole buffer (null rect), `planes` a valid out-parameter of the header's layout (asserted above).
        let r = unsafe { AHardwareBuffer_lockPlanes(b, usage, -1, std::ptr::null(), &mut planes) };
        ensure!(r == 0, "AHardwareBuffer_lockPlanes: {r}");
        Ok(Self {
            b,
            plane: planes.planes[0],
            count: planes.plane_count,
        })
    }

    /// The four bytes of pixel (x, y).
    fn pixel(&self, x: u32, y: u32) -> *mut u8 {
        // SAFETY: (x, y) is inside the 64 x 64 buffer the caller allocated; the plane's data covers `rowStride * height` bytes while locked.
        unsafe {
            self.plane.data.add(
                y as usize * self.plane.row_stride as usize
                    + x as usize * self.plane.pixel_stride as usize,
            )
        }
    }
}

impl Locked {
    /// Unlocks and reports the NDK's status (a lock left open when a check fails is unlocked by `Drop`, status ignored).
    fn finish(self) -> Result<()> {
        let b = self.b;
        std::mem::forget(self);
        // SAFETY: `b` is locked by `new`; no release fence is wanted.
        let r = unsafe { AHardwareBuffer_unlock(b, std::ptr::null_mut()) };
        ensure!(r == 0, "AHardwareBuffer_unlock: {r}");
        Ok(())
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: `b` is locked by `new`; no release fence is wanted.
        unsafe { AHardwareBuffer_unlock(self.b, std::ptr::null_mut()) };
    }
}

fn check_desc(d: &Desc, what: &str) -> Result<()> {
    ensure!(
        (d.width, d.height, d.layers, d.format) == (EDGE, EDGE, 1, FORMAT_R8G8B8A8),
        "{what}: {}x{} layers {} format {}",
        d.width,
        d.height,
        d.layers,
        d.format
    );
    let want = CPU_READ_OFTEN | CPU_WRITE_OFTEN | GPU_SAMPLED_IMAGE;
    ensure!(
        d.usage & want == want,
        "{what}: usage {:#x} lacks {want:#x}",
        d.usage
    );
    // a row is at least EDGE pixels; pitch padding is the device's
    ensure!(
        d.stride >= EDGE,
        "{what}: stride {} pixels under the width",
        d.stride
    );
    Ok(())
}

fn check_plane(l: &Locked, stride_pixels: u32) -> Result<()> {
    ensure!(l.count == 1, "plane count {}", l.count);
    ensure!(
        l.plane.pixel_stride == 4,
        "pixelStride {}",
        l.plane.pixel_stride
    );
    ensure!(
        l.plane.row_stride == stride_pixels * 4,
        "rowStride {} but describe() stride is {stride_pixels} pixels",
        l.plane.row_stride
    );
    Ok(())
}

fn write_all(sock: BorrowedFd<'_>, bytes: &[u8]) -> Result<()> {
    let n = rustix::io::write(sock, bytes).context("socket write")?;
    ensure!(n == bytes.len(), "short socket write");
    Ok(())
}

fn read_exact(sock: BorrowedFd<'_>, len: usize) -> Result<Vec<u8>> {
    let mut v = vec![0u8; len];
    let mut got = 0;
    while got < len {
        let n = rustix::io::read(sock, &mut v[got..]).context("socket read")?;
        ensure!(n > 0, "peer closed the socket");
        got += n;
    }
    Ok(v)
}

fn writer(sock: OwnedFd) -> Result<bool> {
    let mut rep = Report {
        role: "writer",
        failed: false,
    };
    let want = Desc {
        width: EDGE,
        height: EDGE,
        layers: 1,
        format: FORMAT_R8G8B8A8,
        usage: CPU_READ_OFTEN | CPU_WRITE_OFTEN | GPU_SAMPLED_IMAGE,
        ..Desc::default()
    };
    let mut b: *mut Buffer = std::ptr::null_mut();
    // SAFETY: `want` is a valid descriptor, `b` a valid out-parameter.
    let r = unsafe { AHardwareBuffer_allocate(&want, &mut b) };
    rep.check(
        "allocate",
        if r == 0 && !b.is_null() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("AHardwareBuffer_allocate: {r}"))
        },
    );
    if b.is_null() {
        return Ok(true);
    }
    let d = describe(b);
    rep.check("describe", check_desc(&d, "writer"));
    let buffer_id = id(b);
    rep.check(
        "id",
        buffer_id
            .as_ref()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{e:#}")),
    );
    let buffer_id = buffer_id.unwrap_or(0);

    {
        let r = Locked::new(b, CPU_WRITE_OFTEN);
        rep.check(
            "lock-planes",
            r.as_ref()
                .map_err(|e| anyhow::anyhow!("{e:#}"))
                .and_then(|l| check_plane(l, d.stride)),
        );
        if let Ok(l) = r {
            for y in 0..EDGE {
                for x in 0..EDGE {
                    for c in 0..4 {
                        // SAFETY: inside the locked plane (see `pixel`), written once.
                        unsafe { l.pixel(x, y).add(c).write(expected(x, y, c)) };
                    }
                }
            }
            rep.check("unlock", l.finish());
        }
    }

    // SAFETY: `b` is live, the socket is open.
    let r = unsafe {
        AHardwareBuffer_sendHandleToUnixSocket(b, std::os::fd::AsRawFd::as_raw_fd(&sock))
    };
    rep.check(
        "send-handle",
        if r == 0 {
            Ok(())
        } else {
            Err(anyhow::anyhow!("sendHandleToUnixSocket: {r}"))
        },
    );
    write_all(std::os::fd::AsFd::as_fd(&sock), &buffer_id.to_ne_bytes())?;
    // the reader is done when it writes one byte back
    let ack = read_exact(std::os::fd::AsFd::as_fd(&sock), 1)?;
    rep.check(
        "reader-finished",
        if ack[0] == 1 {
            Ok(())
        } else {
            Err(anyhow::anyhow!("reader reported failure"))
        },
    );

    {
        let l = Locked::new(b, CPU_READ_OFTEN);
        let seen = l
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e:#}"))
            .and_then(|l| {
                let p = l.pixel(WRITE_BACK.0, WRITE_BACK.1);
                // SAFETY: inside the locked plane.
                let got: Vec<u8> = (0..4).map(|c| unsafe { p.add(c).read() }).collect();
                ensure!(
                    got == WRITE_BACK_VALUE,
                    "pixel {WRITE_BACK:?} is {got:02x?}, the reader wrote {WRITE_BACK_VALUE:02x?}"
                );
                // and the rest is still the writer's pattern
                let (x, y) = (3, 5);
                let q = l.pixel(x, y);
                for c in 0..4 {
                    // SAFETY: inside the locked plane.
                    let v = unsafe { q.add(c).read() };
                    ensure!(
                        v == expected(x, y, c),
                        "pixel ({x},{y}) byte {c} changed to {v:#x}"
                    );
                }
                Ok(())
            });
        rep.check("sees-reader-write", seen);
    }
    // SAFETY: the last reference this process holds.
    unsafe { AHardwareBuffer_release(b) };
    Ok(rep.failed)
}

fn reader(sock: OwnedFd) -> Result<bool> {
    let mut rep = Report {
        role: "reader",
        failed: false,
    };
    let mut b: *mut Buffer = std::ptr::null_mut();
    // SAFETY: the socket is open; `b` a valid out-parameter. libui unflattens the handle and imports it through the mapper.
    let r = unsafe {
        AHardwareBuffer_recvHandleFromUnixSocket(std::os::fd::AsRawFd::as_raw_fd(&sock), &mut b)
    };
    rep.check(
        "recv-handle",
        if r == 0 && !b.is_null() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("recvHandleFromUnixSocket: {r}"))
        },
    );
    if b.is_null() {
        write_all(std::os::fd::AsFd::as_fd(&sock), &[0])?;
        return Ok(true);
    }
    let writer_id = u64::from_ne_bytes(
        read_exact(std::os::fd::AsFd::as_fd(&sock), 8)?
            .try_into()
            .expect("8 bytes"),
    );
    let d = describe(b);
    rep.check("describe-imported", check_desc(&d, "reader"));
    rep.check(
        "id-matches",
        id(b).and_then(|mine| {
            ensure!(
                mine == writer_id,
                "id {mine:#x} here, {writer_id:#x} in the writer"
            );
            ensure!(mine != 0, "id 0");
            Ok(())
        }),
    );
    rep.check("layout-from-handle", check_layout(b, &d));

    {
        let l = Locked::new(b, CPU_READ_OFTEN);
        let same = l
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e:#}"))
            .and_then(|l| {
                check_plane(l, d.stride)?;
                for y in 0..EDGE {
                    for x in 0..EDGE {
                        let p = l.pixel(x, y);
                        for c in 0..4 {
                            // SAFETY: inside the locked plane.
                            let v = unsafe { p.add(c).read() };
                            if v != expected(x, y, c) {
                                bail!(
                                    "pixel ({x},{y}) byte {c}: {v:#x}, the writer wrote {:#x}",
                                    expected(x, y, c)
                                );
                            }
                        }
                    }
                }
                Ok(())
            });
        rep.check("reads-identical-bytes", same);
    }
    {
        let written = Locked::new(b, CPU_WRITE_OFTEN).and_then(|l| {
            let p = l.pixel(WRITE_BACK.0, WRITE_BACK.1);
            for (c, v) in WRITE_BACK_VALUE.iter().enumerate() {
                // SAFETY: inside the locked plane.
                unsafe { p.add(c).write(*v) };
            }
            l.finish()
        });
        rep.check("write-back", written);
    }
    write_all(std::os::fd::AsFd::as_fd(&sock), &[u8::from(!rep.failed)])?;
    // SAFETY: the last reference this process holds.
    unsafe { AHardwareBuffer_release(b) };
    Ok(rep.failed)
}

fn main() -> Result<ExitCode> {
    let args = Args::parse();
    // SAFETY: liblog accepts its own stderr logger as the process logger; it is set before any other thread exists.
    unsafe { __android_log_set_logger(Some(__android_log_stderr_logger)) };
    let adopt = |fd: i32| -> Result<OwnedFd> {
        ensure!(fd >= 3, "--sock {fd}: not an inherited descriptor");
        // SAFETY: the harness passes an open socket as this descriptor and nothing else in this process owns it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    };
    let failed = match args.role {
        Role::Writer { sock } => writer(adopt(sock)?)?,
        Role::Reader { sock } => reader(adopt(sock)?)?,
    };
    // a normal return: libui's process-wide Gralloc5 allocator proxy is released by the C++ exit handlers, after the thread-locals, which the driver must survive (atl-binder-driver `driver_call_after_thread_local_destruction`); a bad exit status here is the gate's failure
    let _ = std::io::stdout().flush();
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
