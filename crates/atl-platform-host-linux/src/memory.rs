// SPDX-License-Identifier: MIT
//! The host's memory as a native Linux program reads it: /proc/meminfo's `MemTotal` and `MemAvailable` (fs/proc/meminfo.c, kB units). `MemTotal` equals sysinfo(2)'s totalram * mem_unit (both are totalram_pages).

use atl_platform_host_api::{HostMemory, MemoryError, MemorySource};

const MEMINFO: &str = "/proc/meminfo";

/// The host's memory through /proc/meminfo (module doc).
#[derive(Debug, Default)]
pub struct HostMemoryInfo;

impl MemorySource for HostMemoryInfo {
    fn memory(&self) -> Result<HostMemory, MemoryError> {
        let text =
            std::fs::read_to_string(MEMINFO).map_err(|e| MemoryError(format!("{MEMINFO}: {e}")))?;
        parse(&text)
    }
}

/// One `Name:   <n> kB` line's value in bytes.
fn field(text: &str, name: &str) -> Result<u64, MemoryError> {
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
        .ok_or_else(|| MemoryError(format!("{MEMINFO}: no {name}")))?;
    let mut parts = line.split_whitespace();
    let kb = parts
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .filter(|_| parts.next() == Some("kB"))
        .ok_or_else(|| MemoryError(format!("{MEMINFO}: unparsable {name}: {line:?}")))?;
    kb.checked_mul(1024)
        .ok_or_else(|| MemoryError(format!("{MEMINFO}: {name} overflows")))
}

fn parse(text: &str) -> Result<HostMemory, MemoryError> {
    Ok(HostMemory {
        total: field(text, "MemTotal")?,
        available: field(text, "MemAvailable")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kb_fields_become_bytes() {
        // the kernel's layout (fs/proc/meminfo.c show_val_kb: "%-16s%8lu kB"); MemFree is not MemAvailable
        let text = "MemTotal:       16303740 kB\nMemFree:          812344 kB\nMemAvailable:    9876544 kB\nBuffers:          123456 kB\n";
        assert_eq!(
            parse(text).expect("parses"),
            HostMemory {
                total: 16_303_740 * 1024,
                available: 9_876_544 * 1024
            }
        );
    }

    #[test]
    fn missing_or_unitless_field_is_an_error() {
        assert!(parse("MemTotal:       16303740 kB\n").is_err());
        assert!(parse("MemTotal:       16303740\nMemAvailable:    9876544 kB\n").is_err());
        // a longer name with the same prefix is not the field
        assert!(parse("MemTotalX:      1 kB\nMemAvailable:    1 kB\n").is_err());
    }
}
