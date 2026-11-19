// SPDX-License-Identifier: MIT
//! Thread churn check (qemu_thread `c`, qemu_main `c`): guest threads created and joined give back every `Active::reserve` block (engine `unmap_window`, break `qt_keep_windows`) and leave no host heap or resident growth past [`CHURN_BOUND`] per thread (QEMU patch 0007, the ARM CPU finalizer; engine break `keep-cpu`).

use std::time::Instant;

use atl_native_bridge::engine::Guest;

/// Guest threads the check creates and joins after its warm-up (~1.5 s). Enough that a leak above [`CHURN_BOUND`] lifts the totals far past their noise: a 64 MiB malloc arena / 2000 = 32 KiB, and the leak QEMU's ARM CPU finalizer had (31.8 KB/thread) grew in-use heap by 64 MB here.
const CHURN: usize = 2_000;
/// Threads before the baseline: glibc's stack cache and QEMU's first-use allocations settle in them.
const CHURN_WARMUP: usize = 200;
/// Largest growth per joined thread accepted, of glibc's in-use heap (`mallinfo2().uordblks`) and of `VmRSS`, in bytes. Measured (aarch64 guest) with the teardown complete: heap 0 net over 9000 threads, one window up to 529 B/thread (a ~1 MB transient), resident up to 624 B/thread; the smallest per-thread leak seen, the ARM CPU's cpreg lists, was 5.8 KB/thread (the whole ARM finalizer leak 31.8 KB). 2 KiB = 3x the noise and under half that leak.
const CHURN_BOUND: i128 = 2048;

/// glibc's in-use heap bytes, all arenas (`mallinfo2().uordblks`; struct mallinfo2, malloc.h: ten size_t fields, uordblks the eighth).
fn heap_in_use() -> usize {
    #[repr(C)]
    struct Mallinfo2 {
        fields: [usize; 10],
    }
    unsafe extern "C" {
        fn mallinfo2() -> Mallinfo2;
    }
    // SAFETY: glibc's query, no arguments; returns the struct by value.
    unsafe { mallinfo2() }.fields[7]
}

/// `VmRSS` and `VmSize` of this process in bytes (`/proc/self/status`, kB fields).
fn vm() -> Result<(u64, u64), String> {
    let s = std::fs::read_to_string("/proc/self/status").map_err(|e| e.to_string())?;
    let field = |name: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map(|kb| kb * 1024)
            .ok_or(format!("/proc/self/status: no {name}"))
    };
    Ok((field("VmRSS:")?, field("VmSize:")?))
}

/// The check: `one(guest, k)` creates and joins guest thread `k` (its own value check inside); `RESERVED` returns to its start value, in-use heap and resident grow at most [`CHURN_BOUND`] per thread. VmSize is printed, not judged: glibc may add an arena when threads overlap. Prints `<prefix>: churn ...` with the deltas.
pub fn churn(
    guest: &mut Guest,
    prefix: &str,
    mut one: impl FnMut(&mut Guest, usize) -> Result<(), String>,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    let mut run = |guest: &mut Guest, n: usize| (0..n).try_for_each(|k| one(guest, k));
    run(guest, CHURN_WARMUP)?;
    let reserved = atl_native_bridge::engine::RESERVED.load(Ordering::Relaxed);
    let heap = heap_in_use();
    let (rss, size) = vm()?;
    let t = Instant::now();
    run(guest, CHURN)?;
    let ms = t.elapsed().as_millis();
    let left = atl_native_bridge::engine::RESERVED.load(Ordering::Relaxed);
    let heap2 = heap_in_use();
    let (rss2, size2) = vm()?;
    let per = |a: u64, b: u64| (i128::from(b) - i128::from(a)) / CHURN as i128;
    let heap_per = per(heap as u64, heap2 as u64);
    let rss_per = per(rss, rss2);
    println!(
        "{prefix}: churn {CHURN} threads in {ms} ms: reserved {reserved} -> {left}, heap {heap_per} B/thread, rss {rss_per} B/thread, vmsize {} B/thread (bound {CHURN_BOUND})",
        per(size, size2)
    );
    let mut why = Vec::new();
    if left != reserved {
        why.push(format!(
            "{} reserve blocks not given back",
            left as i128 - reserved as i128
        ));
    }
    if heap_per > CHURN_BOUND {
        why.push(format!("in-use heap grew {heap_per} B/thread"));
    }
    if rss_per > CHURN_BOUND {
        why.push(format!("resident grew {rss_per} B/thread"));
    }
    if why.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "after {CHURN} joined threads: {} (bound {CHURN_BOUND} B/thread)",
            why.join(", ")
        ))
    }
}
