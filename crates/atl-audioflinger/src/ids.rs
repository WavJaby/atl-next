// SPDX-License-Identifier: MIT
//! AudioFlinger's unique ids, as AOSP's `AudioFlinger::nextUniqueId` makes them (AudioFlinger.cpp:3933): base | use, the base stepping by AUDIO_UNIQUE_ID_USE_MAX per id of one use, so the low bits name the use.
//! AOSP's counters start at USE_MAX, so every id is nonzero (AUDIO_SESSION_ALLOCATE and AUDIO_IO_HANDLE_NONE are 0).

use std::sync::atomic::{AtomicU32, Ordering};

/// `AUDIO_UNIQUE_ID_USE_MAX` (system/media/audio/include/system/audio.h:201): a power of two; the step between ids of one use.
const USE_MAX: u32 = 8;
/// `AUDIO_UNIQUE_ID_USE_SESSION` (audio.h:190): audio_session_t, the ids AudioManager.generateAudioSessionId and a track's session 0 get.
pub const USE_SESSION: u32 = 1;
/// `AUDIO_UNIQUE_ID_USE_OUTPUT` (audio.h AUDIO_UNIQUE_ID_USE_OUTPUT = 5).
pub const USE_OUTPUT: u32 = 5;
/// `AUDIO_UNIQUE_ID_USE_CLIENT` (audio.h:197): client-side players and recorders; a track's port id comes from this use.
pub const USE_CLIENT: u32 = 7;

static NEXT: [AtomicU32; USE_MAX as usize] = [const { AtomicU32::new(USE_MAX) }; USE_MAX as usize];

/// The next id of `use_`, or None for a use AOSP refuses (`>= AUDIO_UNIQUE_ID_USE_MAX`; newAudioUniqueId then answers AUDIO_UNIQUE_ID_ALLOCATE).
pub fn next(use_: u32) -> Option<i32> {
    let counter = NEXT.get(use_ as usize)?;
    let base = counter.fetch_add(USE_MAX, Ordering::AcqRel);
    Some((base | use_) as i32)
}
