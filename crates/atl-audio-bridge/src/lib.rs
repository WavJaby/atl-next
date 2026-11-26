// SPDX-License-Identifier: MIT
//! The server side of AOSP's track shared memory and mixer, for atl-audioflinger (game audio A2). A track's control block and buffer are mapped here; AOSP's server proxy reads them (ServerProxy, AudioTrackServerProxy, StaticAudioTrackServerProxy); AudioMixerBase mixes every enabled track into one quantum of float output.
//! The quantum loop, the enabled set, the drains and the gains are Rust's ([`Mix`]); the AOSP calls go through wrap/mix.cpp, which only forwards (the AudioBufferProvider callbacks stay there: AOSP's process() calls them).
//! [`Mapping`] is the only place pointers into shared memory are made. [`Track`] and [`Mix`] own the C++ objects and the mappings they read. [`Mix`] is `Send`: its C++ state has no thread affinity. [`MixCell`] gives the mixer to one holder at a time (the output's realtime fill or one control call), so no lock is shared with the fill.
//! With feature `converter` (`xtask native` turns it on) it is compiled; without, it is empty.
#![cfg(feature = "converter")]

mod bridge;
mod engine;

pub use engine::PolicyEngine;

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::ops::{Deref, DerefMut};
use std::os::fd::AsFd;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use cxx::UniquePtr;
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};

/// sizeof(audio_track_cblk_t) on this host: the control block at the start of every track region; the streaming buffer follows it.
pub fn cblk_bytes() -> usize {
    bridge::ffi::cblk_bytes()
}

/// Frames the mixer took from tracks since the last call (resets).
pub fn take_track_frames() -> u64 {
    bridge::ffi::take_track_frames()
}

/// Enabled tracks whose client volume was zero in the last mixed quantum.
pub fn muted_tracks() -> u32 {
    MUTED.load(Ordering::Relaxed)
}

/// Tracks enabled in the mix, draining ones included.
pub fn active_tracks() -> u32 {
    ACTIVE.load(Ordering::Relaxed)
}

/// Underruns the tracks counted since the last call (resets): one per start of an underrun, the steps of the count each client reads (AudioTrack::getUnderrunCount, AAudioStream_getXRunCount).
pub fn take_underruns() -> u64 {
    bridge::ffi::take_underruns()
}

/// Frames the tracks tallied as underrun since the last call (resets): what the mixer asked of a playing track and got nothing for.
pub fn take_underrun_frames() -> u64 {
    bridge::ffi::take_underrun_frames()
}

/// What a track left behind when it was removed ([`Mix::remove`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Removed {
    /// Frames it never played: unread or flushed.
    pub dropped: u64,
    /// The underruns its client read over its life (0 for a static track).
    pub underruns: u32,
}

/// A VolumeShaper's state as AOSP's VolumeShaper::State holds it: the interpolated volume (y-axis) and the normalized time (x-axis).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShaperState {
    /// The volume the shaper gives at its position.
    pub volume: f32,
    /// The shaper's position on its curve (MIN_CURVE_TIME..MAX_CURVE_TIME).
    pub x_offset: f32,
}

/// A shared mapping of a file range, unmapped on drop. Pointers into it are valid while it lives.
pub struct Mapping {
    base: *mut c_void,
    map_len: usize,
    ptr: *mut u8,
    len: usize,
}

// SAFETY: a mapping is plain shared memory; which thread reads or writes it is the maintainer's decision (the mixer's lock).
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
    /// A static (shared-buffer) track: a stop leaves the mix at once (Tracks.cpp:1321-1325).
    is_static: bool,
    _cblk: Mapping,
    _buffer: Option<Mapping>,
}

impl Track {
    /// A streaming track: the client writes `frames` frames of `frame_bytes` in linear PCM `format` (an audio_format_t AudioMixerBase::isValidFormat takes; the mixer refuses others at [`Mix::add`]) after the control block, which `cblk` maps.
    pub fn streaming(
        cblk: Mapping,
        format: u32,
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
            bridge::ffi::new_track(
                cblk.as_ptr(),
                buffer,
                format,
                frames,
                frame_bytes,
                false,
                rate,
            )
        };
        Ok(Self {
            proxy,
            rate,
            is_static: false,
            _cblk: cblk,
            _buffer: None,
        })
    }

    /// A static (shared-buffer) track: `buffer` holds the client's `frames` frames in `format` (as [`Track::streaming`]), read in place and never written.
    pub fn shared(
        cblk: Mapping,
        buffer: Mapping,
        format: u32,
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
                format,
                frames,
                frame_bytes,
                true,
                rate,
            )
        };
        Ok(Self {
            proxy,
            rate,
            is_static: true,
            _cblk: cblk,
            _buffer: Some(buffer),
        })
    }
}

/// What an IAudioTrack playback call does to a track in the mix (AOSP Track::start / pause / stop / flush for a normal mixed track).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// Starts the server side, follows the client's volume and enables the track; a draining track plays on.
    Start,
    /// Disables the track; its unread frames stay.
    Pause,
    /// A streaming track plays out what the client wrote before its stop, then leaves the mix; a static one leaves at once.
    Stop,
    /// Discards the unread frames of a track the mixer is not reading; a draining one ends at its next quantum.
    Flush,
}

/// The output's mixer: AudioMixerBase at one quantum and the output's rate, with the tracks added to it by name. The mixer's state is here: the enabled tracks, the quantum and its position, the counters. The C++ side (wrap/mix.cpp) forwards each AOSP call.
pub struct Mix {
    inner: UniquePtr<bridge::ffi::Mix>,
    tracks: BTreeMap<i32, Track>,
    next_name: i32,
    /// Enabled tracks, draining ones included, unordered: a removal swaps the last one in, so the fill never allocates.
    enabled: Vec<i32>,
    /// One quantum of interleaved stereo: the mixer's main buffer (every track's MAIN_BUFFER), zeroed before each process.
    out: Vec<f32>,
    quantum: usize,
    /// The first frame of `out` not yet handed out.
    next: usize,
}

// SAFETY: the C++ mixer and its tracks have no thread affinity; the rest is plain data. Calls never overlap: [`MixCell`] hands the mixer to one holder at a time.
unsafe impl Send for Mix {}

/// Enabled tracks, draining ones included (the `active_tracks` counter).
static ACTIVE: AtomicU32 = AtomicU32::new(0);
/// Enabled tracks whose client volume was zero in the last quantum (the `muted_tracks` counter).
static MUTED: AtomicU32 = AtomicU32::new(0);
/// Interleaved channels of the output: stereo (AUDIO_CHANNEL_OUT_STEREO).
const CHANNELS: usize = 2;

impl Mix {
    /// A mixer of `quantum_frames` per [`Mix::render`] quantum at `rate`.
    pub fn new(quantum_frames: usize, rate: u32) -> Self {
        Self {
            inner: bridge::ffi::new_mix(quantum_frames, rate),
            tracks: BTreeMap::new(),
            next_name: 0,
            enabled: Vec::new(),
            out: vec![0.0; quantum_frames * CHANNELS],
            quantum: quantum_frames,
            next: 0,
        }
    }

    /// Adds `track`, disabled, in its format and `channel_mask` (mono or stereo) of `session`. Returns its name, or None when AOSP refuses the format (then `track` is dropped).
    pub fn add(&mut self, mut track: Track, session: i32, channel_mask: u32) -> Option<i32> {
        let name = self.next_name;
        // SAFETY: `out` is one quantum of stereo floats and is never resized; the mixer writes through this pointer only while this Mix is held (the fill or one control call). `inner` drops before `out` (field order), so no track outlives the buffer it adds into.
        let added = unsafe {
            bridge::ffi::mix_add(
                self.inner.pin_mut(),
                track.proxy.pin_mut(),
                name,
                session,
                channel_mask,
                track.rate,
                self.out.as_mut_ptr(),
            )
        };
        if !added {
            return None;
        }
        self.next_name += 1;
        self.tracks.insert(name, track);
        Some(name)
    }

    /// Removes track `name`: the mixer forgets it first, then the track and its mappings are dropped. Returns what it left behind; all zero for an unknown name.
    pub fn remove(&mut self, name: i32) -> Removed {
        let Some(mut track) = self.tracks.remove(&name) else {
            return Removed::default();
        };
        // Frames it never played: unread (framesReady applies a pending flush first) and flushed away (mix_remove's count).
        let dropped = if track.is_static {
            0
        } else {
            let unread = bridge::ffi::track_frames_ready(track.proxy.pin_mut());
            let flushed = bridge::ffi::track_frames_flushed(track.proxy.pin_mut()).max(0);
            unread + u64::try_from(flushed).unwrap_or(0)
        };
        if bridge::ffi::mix_exists(self.inner.pin_mut(), name) {
            self.disable(name);
            bridge::ffi::mix_destroy(self.inner.pin_mut(), name);
        }
        let underruns = track.proxy.as_ref().map_or(0, bridge::ffi::track_underruns);
        Removed { dropped, underruns }
    }

    /// Applies an IAudioTrack playback call to track `name` (see [`Control`]). False for an unknown name.
    pub fn control(&mut self, name: i32, call: Control) -> bool {
        let Some(is_static) = self.tracks.get(&name).map(|track| track.is_static) else {
            return false;
        };
        match call {
            Control::Start => {
                if let Some(track) = self.tracks.get_mut(&name) {
                    bridge::ffi::track_start(track.proxy.pin_mut());
                    bridge::ffi::track_set_draining(track.proxy.pin_mut(), false);
                }
                self.apply_gain(name);
                if !self.enabled.contains(&name) {
                    self.enabled.push(name);
                }
                bridge::ffi::mix_enable(self.inner.pin_mut(), name);
                self.publish_active();
            }
            Control::Pause => self.disable(name),
            Control::Stop => {
                if is_static || !self.enabled.contains(&name) {
                    self.disable(name);
                } else {
                    self.set_draining(name, true);
                }
            }
            Control::Flush => {
                if !self.enabled.contains(&name)
                    && let Some(track) = self.tracks.get_mut(&name)
                {
                    bridge::ffi::track_flush_if_needed(track.proxy.pin_mut());
                }
            }
        }
        true
    }

    /// Applies IAudioTrack.applyVolumeShaper to track `name`: the configuration and operation as AIDL parcel bytes, read as AOSP reads them. AOSP's status: a shaper id, or a negative status_t. None for an unknown name.
    pub fn apply_volume_shaper(
        &mut self,
        name: i32,
        configuration: &[u8],
        operation: &[u8],
    ) -> Option<i32> {
        let track = self.tracks.get_mut(&name)?;
        Some(bridge::ffi::track_apply_volume_shaper(
            track.proxy.pin_mut(),
            configuration,
            operation,
        ))
    }

    /// IAudioTrack.getVolumeShaperState on track `name`: shaper `id`'s state, or None when the track has no such shaper (AOSP's null). None too for an unknown track.
    pub fn volume_shaper_state(&mut self, name: i32, id: i32) -> Option<ShaperState> {
        let track = self.tracks.get_mut(&name)?;
        let (mut volume, mut x_offset) = (0.0f32, 0.0f32);
        bridge::ffi::track_volume_shaper_state(
            track.proxy.pin_mut(),
            id,
            &mut volume,
            &mut x_offset,
        )
        .then_some(ShaperState { volume, x_offset })
    }

    /// Fills `out` (interleaved stereo) with the next frames of the mix: one quantum at a time, any request size.
    pub fn render(&mut self, out: &mut [f32]) {
        let frames = out.len() / CHANNELS;
        let mut done = 0;
        while done < frames {
            if self.next == self.quantum {
                self.begin_quantum();
            }
            let n = (frames - done).min(self.quantum - self.next);
            let from = self.next * CHANNELS;
            out[done * CHANNELS..(done + n) * CHANNELS]
                .copy_from_slice(&self.out[from..from + n * CHANNELS]);
            self.next += n;
            done += n;
        }
    }

    /// The quantum step, the loop mix_render kept in C++ until now: a draining track whose written frames are all mixed leaves the mix (prepareTracks_l, Threads.cpp:6297-6309); each enabled track takes its gains; the mixer makes one quantum into `out`.
    fn begin_quantum(&mut self) {
        let mut at = 0;
        while at < self.enabled.len() {
            let name = self.enabled[at];
            let finished = match self.tracks.get_mut(&name) {
                Some(track) => {
                    bridge::ffi::track_draining(track.proxy.pin_mut())
                        && bridge::ffi::track_frames_ready(track.proxy.pin_mut()) == 0
                }
                None => false,
            };
            if finished {
                // disable moves the last enabled track into `at`: it is checked next
                self.disable(name);
            } else {
                at += 1;
            }
        }
        let mut muted = 0;
        for at in 0..self.enabled.len() {
            let name = self.enabled[at];
            muted += u32::from(self.apply_gain(name));
        }
        MUTED.store(muted, Ordering::Relaxed);
        self.out.fill(0.0);
        bridge::ffi::mix_process(self.inner.pin_mut());
        self.next = 0;
    }

    /// Track `name`'s gains for the next quantum: its client volume times its shaper gain, on both sides, at the track's released frame (Threads.cpp:5860-5862: `volume *= vh`, then the client's LR). True when the client's volume is zero on both sides; the shaper does not count.
    fn apply_gain(&mut self, name: i32) -> bool {
        let Some(track) = self.tracks.get_mut(&name) else {
            return false;
        };
        let (mut left, mut right) = (0.0f32, 0.0f32);
        bridge::ffi::track_client_gain(track.proxy.pin_mut(), &mut left, &mut right);
        let frame = bridge::ffi::track_frames_released(track.proxy.pin_mut());
        let shaper = bridge::ffi::track_shaper_gain(track.proxy.pin_mut(), frame);
        bridge::ffi::mix_set_volume(self.inner.pin_mut(), name, left * shaper, right * shaper);
        left == 0.0 && right == 0.0
    }

    /// Disables track `name` in the mixer, forgets it as enabled and ends its drain.
    fn disable(&mut self, name: i32) {
        bridge::ffi::mix_disable(self.inner.pin_mut(), name);
        if let Some(at) = self.enabled.iter().position(|&n| n == name) {
            self.enabled.swap_remove(at);
            self.set_draining(name, false);
        }
        self.publish_active();
    }

    fn set_draining(&mut self, name: i32, draining: bool) {
        if let Some(track) = self.tracks.get_mut(&name) {
            bridge::ffi::track_set_draining(track.proxy.pin_mut(), draining);
        }
    }

    fn publish_active(&self) {
        ACTIVE.store(self.enabled.len() as u32, Ordering::Relaxed);
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
