// SPDX-License-Identifier: MIT
//! A track's shared memory and its IAudioTrack (AOSP AudioFlinger's TrackBase and TrackHandle, Tracks.cpp / AudioFlinger.cpp).
//! The region is a memfd: the control block (`audio_track_cblk_t`) followed by the streaming buffer, the layout AOSP's TrackBase allocates (Tracks.cpp, ALLOC_CBLK). The client maps it through getCblk and writes into it.
//! No data plane yet (A2): nothing reads the buffer. A client's write fills it and then waits for space, since no server pointer moves; start/stop/pause/flush change the state only.

use std::os::fd::OwnedFd;
use std::sync::{Mutex, MutexGuard};

use atl_audioflinger_stubs::aidl::android::media::IAudioTrack::{BnAudioTrack, IAudioTrack};
use atl_services::counters;
use binder::{BinderFeatures, Interface, ParcelFileDescriptor, Status, StatusCode, Strong};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use shared_file_region_aidl::mangled::_7_android_5_media_16_SharedFileRegion as SharedFileRegion;

const INTERFACE: &str = "IAudioTrack";
/// status_t NO_ERROR.
const NO_ERROR: i32 = 0;
/// status_t BAD_VALUE (-EINVAL).
const BAD_VALUE: i32 = -22;

/// `sizeof(audio_track_cblk_t)` on x86_64 (LP64): clang's record layout of AOSP's `frameworks/av/include/private/media/AudioTrackShared.h` at the pinned frameworks/av, measured 2026-11-07 on the host (232 bytes, align 8). The client places the buffer at `cblk + 1`, i.e. at this offset. The layout itself is not mirrored here: the server writes only through the bridge that owns it (A2).
pub(crate) const CBLK_BYTES: usize = 232;
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
    fd: OwnedFd,
    len: usize,
}

/// The region for a track whose buffer is `buffer_bytes`: 0 for a shared-buffer (static) track, whose buffer is the client's (Tracks.cpp:893). `break_cblk` allocates the control block alone, one buffer short of a streaming track (the audio gate's break); the size check then refuses it: a region shorter than the layout is never handed out.
pub(crate) fn allocate(buffer_bytes: usize, break_cblk: bool) -> Result<Region, StatusCode> {
    let need = buffer_bytes
        .checked_add(CBLK_BYTES)
        .ok_or(StatusCode::BAD_VALUE)?;
    let len = if break_cblk { CBLK_BYTES } else { need };
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

/// AudioTrack's playback states as AOSP's Track keeps them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Active,
    Paused,
    Stopped,
}

/// One track as its IAudioTrack: the region the client maps, and the state its start/stop/pause/flush drive.
struct Track {
    region: Region,
    state: Mutex<State>,
}

impl Track {
    fn state(&self) -> MutexGuard<'_, State> {
        // a poisoned lock still holds a plain enum
        self.state.lock().unwrap_or_else(|e| e.into_inner())
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
            r#fd: Some(ParcelFileDescriptor::new(fd)),
            r#offset: 0,
            r#size: self.region.len as i64,
            r#writeable: true,
        }))
    }

    /// Idle, stopped or paused -> active. Starting an active track is a no-op (the client's own guard decides before it calls).
    fn r#start(&self) -> binder::Result<i32> {
        counters::answered(INTERFACE, "start");
        *self.state() = State::Active;
        Ok(NO_ERROR)
    }

    fn r#stop(&self) -> binder::Result<()> {
        counters::answered(INTERFACE, "stop");
        let mut state = self.state();
        if matches!(*state, State::Active | State::Paused) {
            *state = State::Stopped;
        }
        Ok(())
    }

    fn r#pause(&self) -> binder::Result<()> {
        counters::answered(INTERFACE, "pause");
        let mut state = self.state();
        if *state == State::Active {
            *state = State::Paused;
        }
        Ok(())
    }

    /// Discards the frames of a stopped or paused track (AOSP: no effect on a playing one). No frame is held yet (A2), so only the count changes.
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

/// The IAudioTrack for a new track over `region`, idle.
pub(crate) fn new_binder(region: Region) -> Strong<dyn IAudioTrack> {
    BnAudioTrack::new_binder(
        Track {
            region,
            state: Mutex::new(State::Idle),
        },
        BinderFeatures::default(),
    )
}
