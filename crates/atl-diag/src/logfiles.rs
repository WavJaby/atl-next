// SPDX-License-Identifier: MIT
//! Layout of an app process's log as atld's capture writes it (atld applog.rs): `<name>.log` holds the head (start of the output), then a tail ring `<name>.log.1` (newest) .. `<name>.log.<k>`
//! (oldest), logrotate's numbering. Every reader of an app log reads it through [`read`], so a long run's last lines are seen and not only its head.

use std::path::{Path, PathBuf};

/// Tail segment `n` (1 = newest) of the log whose head is `head`.
pub fn tail_segment(head: &Path, n: usize) -> PathBuf {
    let mut s = head.as_os_str().to_owned();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

/// The files of one log in output order: the head, then the tail segments oldest first; only those that exist.
pub fn segments(head: &Path) -> Vec<PathBuf> {
    let mut tail: Vec<PathBuf> = (1..)
        .map(|n| tail_segment(head, n))
        .take_while(|p| p.exists())
        .collect();
    tail.reverse();
    let mut all: Vec<PathBuf> = vec![head.to_owned()];
    all.retain(|p| p.exists());
    all.extend(tail);
    all
}

/// The whole log whose head is `head` (head + tail ring in order); lossy UTF-8, empty when absent.
pub fn read(head: &Path) -> String {
    let mut text = String::new();
    for p in segments(head) {
        if let Ok(b) = std::fs::read(&p) {
            text.push_str(&String::from_utf8_lossy(&b));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_run_head_then_oldest_tail_first() {
        let dir = std::env::temp_dir().join(format!("atl-diag-logfiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let head = dir.join("p.log");
        std::fs::write(&head, "h\n").expect("head");
        std::fs::write(tail_segment(&head, 1), "new\n").expect("1");
        std::fs::write(tail_segment(&head, 2), "old\n").expect("2");
        assert_eq!(read(&head), "h\nold\nnew\n");
        assert_eq!(read(&dir.join("absent.log")), "");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
