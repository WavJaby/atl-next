// SPDX-License-Identifier: MIT
//! The server side of AOSP's track shared memory and mixer, for atl-audioflinger (game audio A2). A track's control block and buffer are mapped here; AOSP's server proxy reads them (ServerProxy, AudioTrackServerProxy, StaticAudioTrackServerProxy); AudioMixerBase mixes every enabled track into one quantum of float output (wrap/mix.cpp).
//! [`Mapping`] is the only place pointers into shared memory are made. [`Track`] and [`Mix`] own the C++ objects and the mappings they read. [`Mix`] is `Send`: its C++ state has no thread affinity, and the owner serializes every call (atl-audioflinger holds it under one mutex, which the output callback also takes).
//! With feature `converter` (`xtask native` turns it on) it is compiled; without, it is empty.
#![cfg(feature = "converter")]

mod bridge;

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::os::fd::AsFd;
use std::ptr;

use cxx::UniquePtr;
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};

/// sizeof(audio_track_cblk_t) on this host: the control block at the start of every track region; the streaming buffer follows it.
pub fn cblk_bytes() -> usize {
    bridge::ffi::cblk_bytes()
}

/// A shared mapping of a file range, unmapped on drop. Pointers into it are valid while it lives.
pub struct Mapping {
    base: *mut c_void,
    map_len: usize,
    ptr: *mut u8,
    len: usize,
}

// SAFETY: a mapping is plain shared memory; which thread reads or writes it is the owner's decision (the mixer's lock).
unsafe impl Send for Mapping {}

impl Mapping {
    /// Maps `len` bytes of `fd` at `offset`, of any alignment: the map starts at the page below it. `writable` is read-write (MAP_SHARED), else read-only.
    pub fn map(fd: impl AsFd, offset: u64, len: usize, writable: bool) -> Result<Self, String> {
        if len == 0 {
            return Err("empty mapping".to_owned());
        }
        let page = u64::try_from(rustix::param::page_size()).map_err(|e| e.to_string())?;
        let aligned = offset / page * page;
        let delta = usize::try_from(offset - aligned).map_err(|e| e.to_string())?;
        let map_len = len
            .checked_add(delta)
            .ok_or_else(|| "mapping length overflows".to_owned())?;
        let prot = if writable {
            ProtFlags::READ | ProtFlags::WRITE
        } else {
            ProtFlags::READ
        };
        // SAFETY: no address hint, so the kernel picks the range; `fd` is open for this call, and a shared mapping stays valid after the fd closes; `map_len` and `aligned` are the values checked above.
        let base = unsafe { mmap(ptr::null_mut(), map_len, prot, MapFlags::SHARED, fd, aligned) }
            .map_err(|e| format!("mmap of {map_len} bytes at {aligned}: {e}"))?;
        Ok(Self {
            base,
            map_len,
            ptr: base.cast::<u8>().wrapping_add(delta),
            len,
        })
    }

    /// The first byte of the mapped range.
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// The size of the mapped range in bytes.
    pub fn size(&self) -> usize {
        self.len
    }

    /// The address of the first byte: AOSP checks a shared buffer's alignment on it (Threads.cpp:2558).
    pub fn address(&self) -> usize {
        self.ptr as usize
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `base` and `map_len` are exactly what mmap returned for this mapping, and this is its only unmap.
        if let Err(e) = unsafe { munmap(self.base, self.map_len) } {
            tracing::error!(%e, "munmap of a track mapping");
        }
    }
}

/// One track's server side: AOSP's proxy over its control block and buffer. A track in a [`Mix`] leaves it through [`Mix::remove`] before it is dropped.
pub struct Track {
    proxy: UniquePtr<bridge::ffi::Track>,
    rate: u32,
    _cblk: Mapping,
    _buffer: Option<Mapping>,
}

impl Track {
    /// A streaming track: the client writes `frames` frames of `frame_bytes` after the control block, which `cblk` maps.
    pub fn streaming(
        cblk: Mapping,
        frames: usize,
        frame_bytes: usize,
        rate: u32,
    ) -> Result<Self, String> {
        let need = frames
            .checked_mul(frame_bytes)
            .and_then(|bytes| bytes.checked_add(cblk_bytes()))
            .ok_or_else(|| "track layout overflows".to_owned())?;
        if cblk.size() < need {
            return Err(format!(
                "region of {} bytes, the layout needs {need}",
                cblk.size()
            ));
        }
        let buffer = cblk.as_ptr().wrapping_add(cblk_bytes());
        // SAFETY: `cblk` maps `need` bytes (checked above), and the track owns the mapping, so the control block and the frames after it stay mapped while the proxy lives.
        let proxy = unsafe {
            bridge::ffi::new_track(cblk.as_ptr(), buffer, frames, frame_bytes, false, rate)
        };
        Ok(Self {
            proxy,
            rate,
            _cblk: cblk,
            _buffer: None,
        })
    }

    /// A static (shared-buffer) track: `buffer` holds the client's `frames` frames, read in place and never written.
    pub fn shared(
        cblk: Mapping,
        buffer: Mapping,
        frames: usize,
        frame_bytes: usize,
        rate: u32,
    ) -> Result<Self, String> {
        if cblk.size() < cblk_bytes() {
            return Err(format!(
                "control block of {} bytes, the layout needs {}",
                cblk.size(),
                cblk_bytes()
            ));
        }
        let need = frames
            .checked_mul(frame_bytes)
            .ok_or_else(|| "track layout overflows".to_owned())?;
        if buffer.size() < need {
            return Err(format!(
                "shared buffer of {} bytes, {frames} frames need {need}",
                buffer.size()
            ));
        }
        // SAFETY: both mappings are checked above and owned by the track; the C++ side only reads the buffer.
        let proxy = unsafe {
            bridge::ffi::new_track(cblk.as_ptr(), buffer.as_ptr(), frames, frame_bytes, true, rate)
        };
        Ok(Self {
            proxy,
            rate,
            _cblk: cblk,
            _buffer: Some(buffer),
        })
    }
}

/// The output's mixer: AudioMixerBase at one quantum and the output's rate, with the tracks added to it by name.
pub struct Mix {
    inner: UniquePtr<bridge::ffi::Mix>,
    tracks: BTreeMap<i32, Track>,
    next_name: i32,
}

// SAFETY: the C++ mixer and its tracks have no thread affinity. The owner serializes every call: atl-audioflinger holds the mixer under one mutex, which the output's realtime callback takes too.
unsafe impl Send for Mix {}

impl Mix {
    /// A mixer of `quantum_frames` per [`Mix::render`] quantum at `rate`.
    pub fn new(quantum_frames: usize, rate: u32) -> Self {
        Self {
            inner: bridge::ffi::new_mix(quantum_frames, rate),
            tracks: BTreeMap::new(),
            next_name: 0,
        }
    }

    /// Adds `track`, disabled, as PCM 16-bit in `channel_mask` (mono or stereo) of `session`. Returns its name, or None when AOSP refuses the format (then `track` is dropped).
    pub fn add(&mut self, mut track: Track, session: i32, channel_mask: u32) -> Option<i32> {
        let name = self.next_name;
        let added = bridge::ffi::mix_add(
            self.inner.pin_mut(),
            track.proxy.pin_mut(),
            name,
            session,
            channel_mask,
            track.rate,
        );
        if !added {
            return None;
        }
        self.next_name += 1;
        self.tracks.insert(name, track);
        Some(name)
    }

    /// Removes track `name`: the mixer forgets it first, then the track and its mappings are dropped.
    pub fn remove(&mut self, name: i32) {
        if self.tracks.contains_key(&name) {
            bridge::ffi::mix_remove(self.inner.pin_mut(), name);
        }
        self.tracks.remove(&name);
    }

    /// Enables (`on`) or disables track `name` in the mix; enabling starts its server side and follows the client's volume. False for an unknown name.
    pub fn set_active(&mut self, name: i32, on: bool) -> bool {
        let Some(track) = self.tracks.get_mut(&name) else {
            return false;
        };
        bridge::ffi::mix_enable(self.inner.pin_mut(), track.proxy.pin_mut(), name, on);
        true
    }

    /// Fills `out` (interleaved stereo) with the next frames of the mix: one AudioMixerBase quantum at a time, any request size.
    pub fn render(&mut self, out: &mut [f32]) {
        bridge::ffi::mix_render(self.inner.pin_mut(), out);
    }
}
