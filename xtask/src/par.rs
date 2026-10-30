// SPDX-License-Identifier: MIT
//! Bounded parallelism for independent jobs (git processes, mostly).

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// `f` over `items` on up to `workers` threads; results in input order.
pub fn map<T: Sync, R: Send>(items: &[T], workers: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new(items.iter().map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..workers.clamp(1, items.len().max(1)) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let r = f(item);
                    results.lock().unwrap_or_else(|p| p.into_inner())[i] = Some(r);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(|p| p.into_inner())
        .into_iter()
        .map(|r| r.expect("every index below len is taken by exactly one worker"))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn order_kept_and_all_run() {
        let items: Vec<u32> = (0..100).collect();
        let out = super::map(&items, 7, |x| x * 2);
        assert_eq!(out, items.iter().map(|x| x * 2).collect::<Vec<_>>());
        assert!(super::map(&Vec::<u32>::new(), 4, |x| *x).is_empty());
    }
}
