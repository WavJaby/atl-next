// SPDX-License-Identifier: MIT
//! A track's shared memory and its IAudioTrack (AOSP AudioFlinger's TrackBase and TrackHandle, Tracks.cpp / AudioFlinger.cpp).
//! The region is a memfd: the control block (`audio_track_cblk_t`) followed by the streaming buffer, the layout AOSP's TrackBase allocates (Tracks.cpp, ALLOC_CBLK). The client maps it through getCblk and writes into it; the server maps the same region (atl-audio-bridge), and the output's mixer reads it. A static track's buffer is the client's own (its sharedBuffer fd), mapped by createTrack.
//! start/stop/pause enable and disable the track in the mixer (output.rs). flush is not served yet: frames the mixer has not taken stay.

use std::os::fd::OwnedFd;
use std::sync::{Mutex, MutexGuard};

use atl_audio_bridge::{Mapping, Track as Server};
use atl_audioflinger_stubs::aidl::android::media::IAudioTrack::{BnAudioTrack, IAudioTrack};
use atl_services::counters;
use binder::{BinderFeatures, Interface, Status, StatusCode, Strong};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use shared_file_region_aidl::mangled::_7_android_5_media_16_SharedFileRegion as SharedFileRegion;

use crate::output::{self, Shared};

const INTERFACE: &str = "IAudioTrack";
/// status_t NO_ERROR.
const NO_ERROR: i32 = 0;
/// status_t BAD_VALUE (-EINVAL).
const BAD_VALUE: i32 = -22;
/// `sizeof(sample)` of PCM 16-bit, the only track format served.
const PCM16_BYTES: usize = 2;
/// Largest track in frames: 2^22 frames of 4 bytes is a 16 MiB region. AOSP only checks size overflow (Tracks.cpp:167, :183); this bound keeps a bad request from allocating the address space.
const MAX_FRAMES: usize = 1 << 22;

/// AOSP's streaming track: `roundup(frameCount)`, the next power of two (Tracks.cpp:162). `None` for a request AOSP would overflow on, or above [`MAX_FRAMES`].
pub(crate) fn round_frames(requested: i64) -> Option<usize> {
    let requested = usize::try_from(requested.max(1)).ok()?;
    let frames = requested.checked_next_power_of_two()?;
    (frames <= MAX_FRAMES).then_some(frames)
}

/// The track's region: a memfd of `len` bytes, the control block and the track's own buffer (none for a shared-buffer track).
pub(crate) struct Region {
    pub(crate) fd: OwnedFd,
    pub(crate) len: usize,
}

/// The region for a track whose buffer is `buffer_bytes`: 0 for a shared-buffer (static) track, whose buffer is the client's (Tracks.cpp:893). `break_cblk` allocates the control block alone, one buffer short of a streaming track (the audio gate's break); the size check then refuses it: a region shorter than the layout is never handed out.
pub(crate) fn allocate(buffer_bytes: usize, break_cblk: bool) -> Result<Region, StatusCode> {
    let cblk = atl_audio_bridge::cblk_bytes();
    let need = buffer_bytes
        .checked_add(cblk)
        .ok_or(StatusCode::BAD_VALUE)?;
    let len = if break_cblk { cblk } else { need };
    if len < need {
        tracing::error!(len, need, "track region smaller than its layout; refused");
        return Err(StatusCode::NO_MEMORY);
    }
    let fd = memfd_create("atl-audio-track", MemfdFlags::CLOEXEC).map_err(|e| {
        tracing::error!(%e, "track region memfd");
        StatusCode::NO_MEMORY
    })?;
    ftruncate(&fd, len as u64).map_err(|e| {
        tracing::error!(%e, len, "track region size");
        StatusCode::NO_MEMORY
    })?;
    Ok(Region { fd, len })
}

/// The server side of a streaming track: its region mapped read-write, the frames after the control block.
pub(crate) fn server_streaming(
    region: &Region,
    frames: usize,
    frame_bytes: usize,
    rate: u32,
) -> Result<Server, StatusCode> {
    let cblk = Mapping::map(&region.fd, 0, region.len, true).map_err(|e| {
        tracing::error!(%e, "track region mapping");
        StatusCode::NO_MEMORY
    })?;
    Server::streaming(cblk, frames, frame_bytes, rate).map_err(|e| {
        tracing::error!(%e, "streaming track layout");
        StatusCode::NO_MEMORY
    })
}

/// The server side of a static track: the client's shared buffer, mapped read-only, and the control block of `region`. The buffer's address must meet AudioFlinger's alignment (Threads.cpp:2558): the sample size, doubled for more than one channel.
pub(crate) fn server_shared(
    region: &Region,
    shared: &SharedFileRegion,
    channels: usize,
    frames: usize,
    frame_bytes: usize,
    rate: u32,
) -> Result<Server, StatusCode> {
    let fd = shared.r#fd.as_ref().ok_or(StatusCode::BAD_VALUE)?;
    let offset = u64::try_from(shared.r#offset).map_err(|_| StatusCode::BAD_VALUE)?;
    let size = usize::try_from(shared.r#size).map_err(|_| StatusCode::BAD_VALUE)?;
    let buffer = Mapping::map(fd.as_ref(), offset, size, false).map_err(|e| {
        tracing::error!(%e, "static buffer mapping");
        StatusCode::NO_MEMORY
    })?;
    let alignment = if channels > 1 {
        PCM16_BYTES << 1
    } else {
        PCM16_BYTES
    };
    if buffer.address() % alignment != 0 {
        tracing::error!(
            address = buffer.address(),
            channels,
            "invalid shared buffer alignment; refused"
        );
        return Err(StatusCode::BAD_VALUE);
    }
    let cblk = Mapping::map(&region.fd, 0, region.len, true).map_err(|e| {
        tracing::error!(%e, "track region mapping");
        StatusCode::NO_MEMORY
    })?;
    Server::shared(cblk, buffer, frames, frame_bytes, rate).map_err(|e| {
        tracing::error!(%e, "static track layout");
        StatusCode::NO_MEMORY
    })
}

/// AudioTrack's playback states as AOSP's Track keeps them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Active,
    Paused,
    Stopped,
}

/// One track as its IAudioTrack: the region the client maps, its name in the output's mixer (which owns its server side), and the state its start/stop/pause/flush drive.
struct Track {
    region: Region,
    mixer: Shared,
    name: i32,
    state: Mutex<State>,
}

impl Track {
    fn state(&self) -> MutexGuard<'_, State> {
        // a poisoned lock still holds a plain enum
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        // the mixer lets go of the track's server side before the region goes
        output::remove_track(&self.mixer, self.name);
    }
}

impl Interface for Track {}

/// AOSP's AIDL method names; the generated trait declares them raw.
#[allow(non_snake_case, reason = "AOSP's AIDL method names")]
impl IAudioTrack for Track {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(INTERFACE, method)
    }

    /// IAudioTrack.getCblk: a new fd of the region, as AOSP's TrackHandle returns a dup per call; the client maps it read-write.
    fn r#getCblk(&self) -> binder::Result<Option<SharedFileRegion>> {
        counters::answered(INTERFACE, "getCblk");
        let fd = self
            .region
            .fd
            .try_clone()
            .map_err(|_| StatusCode::NO_MEMORY)?;
        Ok(Some(SharedFileRegion {
            r#fd: Some(binder::ParcelFileDescriptor::new(fd)),
            r#offset: 0,
            r#size: self.region.len as i64,
            r#writeable: true,
        }))
    }

    /// Idle, stopped or paused -> active: enabled in the mixer. Starting an active track is a no-op (the client's own guard decides before it calls).
    fn r#start(&self) -> binder::Result<i32> {
        counters::answered(INTERFACE, "start");
        let mut state = self.state();
        if *state != State::Active {
            *state = State::Active;
            output::set_active(&self.mixer, self.name, true);
        }
        Ok(NO_ERROR)
    }

    fn r#stop(&self) -> binder::Result<()> {
        counters::answered(INTERFACE, "stop");
        let mut state = self.state();
        if matches!(*state, State::Active | State::Paused) {
            *state = State::Stopped;
            output::set_active(&self.mixer, self.name, false);
        }
        Ok(())
    }

    fn r#pause(&self) -> binder::Result<()> {
        counters::answered(INTERFACE, "pause");
        let mut state = self.state();
        if *state == State::Active {
            *state = State::Paused;
            output::set_active(&self.mixer, self.name, false);
        }
        Ok(())
    }

    /// Discards the frames of a stopped or paused track (AOSP: no effect on a playing one). Not served yet: the mixer keeps what it has not taken.
    fn r#flush(&self) -> binder::Result<()> {
        counters::answered(INTERFACE, "flush");
        Ok(())
    }

    /// AudioTrack's construction attaches effect 0 (none): NO_ERROR. A real effect would need an effect chain this server does not have.
    fn r#attachAuxEffect(&self, effect_id: i32) -> binder::Result<i32> {
        if effect_id == 0 {
            counters::answered(INTERFACE, "attachAuxEffect");
            Ok(NO_ERROR)
        } else {
            counters::refused(INTERFACE, "attachAuxEffect");
            Ok(BAD_VALUE)
        }
    }
}

/// The IAudioTrack for a new track over `region`, idle, named `name` in `mixer`.
pub(crate) fn new_binder(region: Region, mixer: Shared, name: i32) -> Strong<dyn IAudioTrack> {
    BnAudioTrack::new_binder(
        Track {
            region,
            mixer,
            name,
            state: Mutex::new(State::Idle),
        },
        BinderFeatures::default(),
    )
}
