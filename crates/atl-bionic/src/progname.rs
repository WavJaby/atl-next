// SPDX-License-Identifier: MIT
//! `setprogname` (BSD; bionic's is OpenBSD's `gen/setprogname.c`): the program name becomes the part of the given path after its last `/`, pointing into the caller's string, not copied.
//! bionic keeps it in `__progname`; glibc keeps the same quantity in `program_invocation_short_name` (alias `__progname`), which its `err()` / `warn()` / `error()` print, so `ffi` stores it there, one program name per process as on bionic.

/// Offset of the name's start in `path` (no NUL): after its last `/`, 0 without one.
pub fn short_name_offset(path: &[u8]) -> usize {
    path.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1)
}

#[cfg(test)]
mod tests {
    use super::short_name_offset;

    // expected offsets: index of the last '/' plus one, counted by hand from the literals
    #[test]
    fn after_the_last_slash() {
        assert_eq!(short_name_offset(b"app_process"), 0);
        assert_eq!(short_name_offset(b"/system/bin/app_process64"), 12);
        assert_eq!(short_name_offset(b"dir/"), 4);
        assert_eq!(short_name_offset(b""), 0);
    }
}
