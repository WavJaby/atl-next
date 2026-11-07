// SPDX-License-Identifier: MIT
//! The server side of AOSP's track shared memory and mixer, for atl-audioflinger (game audio A2). A track's control block and buffer are mapped here; AOSP's server proxy reads them (ServerProxy, AudioTrackServerProxy, StaticAudioTrackServerProxy); AudioMixerBase mixes every enabled track into one quantum of float output (wrap/mix.cpp).
//! [`Mapping`] is the only place pointers into shared memory are made. [`Track`] and [`Mix`] own the C++ objects and the mappings they read. [`Mix`] is `Send`: its C++ state has no thread affinity. [`MixCell`] gives the mixer to one holder at a time (the output's realtime fill or one control call), so no lock is shared with the fill.
//! With feature `converter` (`xtask native` turns it on) it is compiled; without, it is empty.
#![cfg(feature = "converter")]

mod bridge;

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::ops::{Deref, DerefMut};
use std::os::fd::AsFd;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

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
        let base = unsafe {
            mmap(
                ptr::null_mut(),
                map_len,
                prot,
                MapFlags::SHARED,
                fd,
                aligned,
            )
        }
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
            bridge::ffi::new_track(
                cblk.as_ptr(),
                buffer.as_ptr(),
                frames,
                frame_bytes,
                true,
                rate,
            )
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

// SAFETY: the C++ mixer and its tracks have no thread affinity. Calls never overlap: [`MixCell`] hands the mixer to one holder at a time.
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

/// The output's mixer, given from holder to holder: the realtime fill, or one control call. Whoever holds the [`Mix`] has it alone; each handoff is one atomic swap, so the fill never waits for a lock.
/// The fill takes it, renders and stores it back. While a control call holds it, the fill finds the cell empty and plays silence for that cycle; a control call that finds the fill mixing waits (yielding) for the store, one quantum at most.
#[derive(Default)]
pub struct MixCell {
    /// The mixer (`Box::into_raw`) while in the cell; null while held.
    slot: AtomicPtr<Mix>,
    /// Set once by [`MixCell::install`]: before it, [`MixCell::hold`] refuses and the fill plays silence.
    installed: AtomicBool,
}

impl MixCell {
    /// Puts `mix` in the cell. False, and `mix` is dropped, when one is installed already.
    pub fn install(&self, mix: Mix) -> bool {
        if self.installed.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.slot
            .store(Box::into_raw(Box::new(mix)), Ordering::Release);
        true
    }

    /// Realtime fill: renders `out` from the mixer and stores it back. False, with `out` untouched, when no mixer is in the cell (not installed, or a control call holds it).
    /// Lock-free and allocation-free: one swap to take the mixer, one store to return it.
    pub fn fill(&self, out: &mut [f32]) -> bool {
        let p = self.slot.swap(ptr::null_mut(), Ordering::AcqRel);
        if p.is_null() {
            return false;
        }
        // SAFETY: the swap moved the pointer out of the cell, so this call holds the only reference to the mixer until the store below puts it back
        unsafe { (*p).render(out) };
        self.slot.store(p, Ordering::Release);
        true
    }

    /// Control call: takes the mixer for the guard's life. None before [`MixCell::install`]; otherwise waits, yielding, while the fill holds it.
    pub fn hold(&self) -> Option<MixGuard<'_>> {
        if !self.installed.load(Ordering::Acquire) {
            return None;
        }
        loop {
            let p = self.slot.swap(ptr::null_mut(), Ordering::AcqRel);
            if !p.is_null() {
                return Some(MixGuard { cell: self, mix: p });
            }
            std::thread::yield_now();
        }
    }
}

impl Drop for MixCell {
    fn drop(&mut self) {
        let p = *self.slot.get_mut();
        if !p.is_null() {
            // SAFETY: the pointer came from `Box::into_raw` in install and is in no guard (`&mut self`: no fill or guard is alive)
            drop(unsafe { Box::from_raw(p) });
        }
    }
}

/// A control call's hold on the mixer: gives it back to its [`MixCell`] on drop.
pub struct MixGuard<'a> {
    cell: &'a MixCell,
    mix: *mut Mix,
}

impl Deref for MixGuard<'_> {
    type Target = Mix;

    fn deref(&self) -> &Mix {
        // SAFETY: the guard's pointer was taken out of the cell, so this guard alone refers to the mixer
        unsafe { &*self.mix }
    }
}

impl DerefMut for MixGuard<'_> {
    fn deref_mut(&mut self) -> &mut Mix {
        // SAFETY: as in `deref`; `&mut self` makes this the only reference
        unsafe { &mut *self.mix }
    }
}

impl Drop for MixGuard<'_> {
    fn drop(&mut self) {
        self.cell.slot.store(self.mix, Ordering::Release);
    }
}
