// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/am/ProcessList.java, frameworks/base/core/java/android/os/FileUtils.java
//! IActivityManager.getMemoryInfo's figures (ProcessList.getMemoryInfo:1733): the host's memory (behaviour decision 3: MemTotal / MemAvailable as a native program reads them, the same sources
//! android_util_Process.cpp's getTotalMemory (sysinfo totalram) and getFreeMemory (MemAvailable) read on a device) run through AOSP's own thresholds, the low-memory killer levels
//! ProcessList.updateOomLevels:1042 derives from total memory and display size. Inputs as on a device without overrides: config_lowMemoryKillerMinFreeKbytesAbsolute -1 and
//! ...Adjust 0 (core/res/res/values/config.xml), 64-bit ABIs present (every ATL host arch is 64-bit), ro.boot.memory_ddr_size unset (advertisedMem falls back to roundStorageSize(totalMem)).

/// ProcessList.mOomAdj:404: FOREGROUND_APP_ADJ, VISIBLE_APP_ADJ, PERCEPTIBLE_APP_ADJ, PERCEPTIBLE_LOW_APP_ADJ, CACHED_APP_MIN_ADJ, CACHED_APP_LMK_FIRST_ADJ.
const OOM_ADJ: [i32; 6] = [0, 100, 200, 250, 900, 950];
/// ProcessList.mOomMinFreeLow:410 / mOomMinFreeHigh:416, KB: the levels for a <= 350 MB HVGA phone and for a >= 700 MB, >= 1280x800 device.
const MIN_FREE_LOW: [i32; 6] = [12288, 18432, 24576, 36864, 43008, 49152];
const MIN_FREE_HIGH: [i32; 6] = [73728, 92160, 110592, 129024, 147456, 184320];
const FOREGROUND_APP_ADJ: i32 = 0;
const VISIBLE_APP_ADJ: i32 = 100;
const SERVICE_ADJ: i32 = 500;
const HOME_APP_ADJ: i32 = 600;
const CACHED_APP_MIN_ADJ: i32 = 900;

/// ActivityManager.MemoryInfo's fields, bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryInfo {
    pub advertised_mem: i64,
    pub avail_mem: i64,
    pub total_mem: i64,
    pub threshold: i64,
    pub low_memory: bool,
    pub hidden_app_threshold: i64,
    pub secondary_server_threshold: i64,
    pub visible_app_threshold: i64,
    pub foreground_app_threshold: i64,
}

/// ProcessList.mOomMinFree after updateOomLevels(displayWidth, displayHeight), KB per OOM_ADJ slot. Java float arithmetic kept (f32, truncating int casts) so the levels match the device's.
fn min_free_kb(total_mem_mb: i64, display_width: i32, display_height: i32) -> [i32; 6] {
    let scale_mem = (total_mem_mb - 350) as f32 / (700 - 350) as f32;
    let min_size = 480 * 800;
    let max_size = 1280 * 800;
    let scale_disp = (display_width.wrapping_mul(display_height) as f32 - min_size as f32)
        / (max_size - min_size) as f32;
    let scale = scale_mem.max(scale_disp).clamp(0.0, 1.0);
    let mut out = [0; 6];
    for (i, slot) in out.iter_mut().enumerate() {
        let low = MIN_FREE_LOW[i];
        let mut high = MIN_FREE_HIGH[i];
        // 64-bit: the cached slots' high levels raised (updateOomLevels:1073-1077)
        if i == 4 {
            high = high * 3 / 2;
        } else if i == 5 {
            high = high * 7 / 4;
        }
        *slot = (low as f32 + (high - low) as f32 * scale) as i32;
    }
    out
}

/// ProcessList.getMemLevel:1515: the first slot whose adj is at least `adj`, else the last; bytes.
fn mem_level(levels: &[i32; 6], adj: i32) -> i64 {
    let i = OOM_ADJ.iter().position(|&a| adj <= a).unwrap_or(5);
    i64::from(levels[i]) * 1024
}

/// FileUtils.roundStorageSize:1413: the next power of two of a decimal unit (1, 2, .. 512, then 1000, 2000, ..) at or above `size`.
fn round_storage_size(size: i64) -> i64 {
    let (mut val, mut pow) = (1i64, 1i64);
    while val.saturating_mul(pow) < size {
        val <<= 1;
        if val > 512 {
            val = 1;
            pow = pow.saturating_mul(1000);
        }
    }
    val.saturating_mul(pow)
}

/// ProcessList.getMemoryInfo:1733 for a host with `total` / `available` bytes and a display of `display_width` x `display_height` px (ProcessList.applyDisplaySize: the base display size).
/// mTotalMemMb is MemTotal in whole MB (ProcessList:878, getTotalSize / (1024 * 1024)).
pub fn memory_info(
    total: u64,
    available: u64,
    display_width: i32,
    display_height: i32,
) -> MemoryInfo {
    let total = i64::try_from(total).unwrap_or(i64::MAX);
    let avail = i64::try_from(available).unwrap_or(i64::MAX);
    let levels = min_free_kb(total / (1024 * 1024), display_width, display_height);
    let home = mem_level(&levels, HOME_APP_ADJ);
    let cached = mem_level(&levels, CACHED_APP_MIN_ADJ);
    MemoryInfo {
        advertised_mem: round_storage_size(total),
        avail_mem: avail,
        total_mem: total,
        threshold: home,
        low_memory: avail < home + (cached - home) / 2,
        hidden_app_threshold: cached,
        secondary_server_threshold: mem_level(&levels, SERVICE_ADJ),
        visible_app_threshold: mem_level(&levels, VISIBLE_APP_ADJ),
        foreground_app_threshold: mem_level(&levels, FOREGROUND_APP_ADJ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KB: i64 = 1024;
    const MB: u64 = 1024 * 1024;

    #[test]
    fn desktop_host_takes_the_high_levels() {
        // >= 700 MB -> scale 1: mOomMinFreeHigh with the 64-bit raise, 147456 * 3 / 2 = 221184 and 184320 * 7 / 4 = 322560 KB.
        // HOME (600) and CACHED_MIN (900) both land on slot 4 (adj 900): threshold = hidden = 221184 KB; SERVICE (500) slot 4 too; VISIBLE (100) slot 1 = 92160; FOREGROUND (0) slot 0 = 73728.
        let total = 16_303_740 * 1024u64;
        let m = memory_info(total, 4 * 1024 * MB, 1920, 1080);
        assert_eq!(m.total_mem, 16_303_740 * KB);
        assert_eq!(m.avail_mem, 4 * 1024 * 1024 * KB);
        assert_eq!(m.threshold, 221_184 * KB);
        assert_eq!(m.hidden_app_threshold, 221_184 * KB);
        assert_eq!(m.secondary_server_threshold, 221_184 * KB);
        assert_eq!(m.visible_app_threshold, 92_160 * KB);
        assert_eq!(m.foreground_app_threshold, 73_728 * KB);
        assert!(!m.low_memory);
        // roundStorageSize(16694829056): 1, 2, .. 512 B, 1 .. 512 KB(1000), .. 16e9 < size -> 32e9
        assert_eq!(m.advertised_mem, 32_000_000_000);
    }

    #[test]
    fn low_memory_below_home_plus_half_the_cached_gap() {
        // home == cached here, so the line is home itself: 221184 KB
        let total = 8192 * MB;
        assert!(memory_info(total, 221_184 * 1024 - 1, 1920, 1080).low_memory);
        assert!(!memory_info(total, 221_184 * 1024, 1920, 1080).low_memory);
    }

    #[test]
    fn small_host_and_display_scale_between_the_tables() {
        // 525 MB -> scaleMem (525 - 350) / 350 = 0.5; 480x800 -> scaleDisp 0; scale 0.5: low + (high - low) / 2 per slot,
        // slot 4: 43008 + (221184 - 43008) / 2 = 132096, slot 5: 49152 + (322560 - 49152) / 2 = 185856.
        assert_eq!(
            min_free_kb(525, 480, 800),
            [43008, 55296, 67584, 82944, 132096, 185856]
        );
        // a large display lifts a small host to the high table (scale = max of the two)
        assert_eq!(
            min_free_kb(400, 1280, 800),
            [73728, 92160, 110592, 129024, 221184, 322560]
        );
        // below 350 MB and no display: the low table (64-bit raise applies to high only)
        assert_eq!(min_free_kb(300, 0, 0), MIN_FREE_LOW);
    }

    #[test]
    fn round_storage_size_steps() {
        // FileUtils.roundStorageSize: values from its own loop by hand
        assert_eq!(round_storage_size(1), 1);
        assert_eq!(round_storage_size(513), 1000);
        assert_eq!(round_storage_size(4_000_000_000), 4_000_000_000);
        assert_eq!(round_storage_size(4_000_000_001), 8_000_000_000);
    }
}
