// SPDX-License-Identifier: MIT
//! The allocation policy: which DRM modifiers a buffer may use, from its usage and atld's display probe (the capability oracle; gralloc stays a pure GBM allocator).

use atl_gralloc::format::{self, DRM_FORMAT_MOD_LINEAR};
use atl_gralloc::usage::{CPU_MASK, GPU_RENDER_TARGET};

/// The modifier set for a buffer of `usage` in `pixel_format`, from the probe's lists (`probe`: per fourcc, atld's `--modifiers` = renderable on the device ∩ imported by the presenter; a fourcc absent = linear tier). Empty = no modifier serves it (refused).
/// - any CPU bit, or BLOB / an unknown format: `[LINEAR]` (the CPU indexes rows); empty when GPU_RENDER_TARGET is also set and the probed set lacks LINEAR, i.e. LINEAR is not renderable on the device (NVIDIA: external-only) -- owner D2 (a)
/// - GPU-only: the probed set as is, GBM picks inside it (owner D1 (a): no "prefer LINEAR"); empty when the probe found none
/// - no probe list for the fourcc (WSL, headless): `[LINEAR]`, the baseline tier
pub fn modifiers_for(usage: u64, pixel_format: i32, probe: &[(u32, Vec<u64>)]) -> Vec<u64> {
    let linear = vec![DRM_FORMAT_MOD_LINEAR];
    let Some(fmt) = format::lookup(pixel_format).filter(|f| f.pixel_format != format::BLOB) else {
        return linear;
    };
    let Some((_, set)) = probe.iter().find(|(f, _)| *f == fmt.fourcc) else {
        return linear;
    };
    if usage & CPU_MASK == 0 {
        return set.clone();
    }
    if usage & GPU_RENDER_TARGET != 0 && !set.contains(&DRM_FORMAT_MOD_LINEAR) {
        return vec![];
    }
    linear
}

#[cfg(test)]
mod tests {
    use atl_gralloc::descriptor::{Descriptor, Refusal};
    use atl_gralloc::format::{BLOB, DRM_FORMAT_ABGR8888, RGBA_8888};
    use atl_gralloc::usage::GPU_TEXTURE;

    use super::*;

    /// AB24 sets atld's probe printed on the laptop (gralloc-modifiers S1, gpu-m1 laptop runs): Intel LNL renders LINEAR, X, 4, 4-CCS; NVIDIA's set is block-linear only (LINEAR external-only, so the probe dropped it). Two of NVIDIA's 12 suffice.
    const INTEL: [u64; 4] = [
        0,
        0x0100_0000_0000_0001,
        0x0100_0000_0000_0009,
        0x0100_0000_0000_0010,
    ];
    const NVIDIA: [u64; 2] = [0x0300_0000_0060_6010, 0x0300_0000_00e0_8014];
    const WINDOW: u64 = GPU_RENDER_TARGET | GPU_TEXTURE;
    const LIN: [u64; 1] = [DRM_FORMAT_MOD_LINEAR];

    fn probe(set: &[u64]) -> Vec<(u32, Vec<u64>)> {
        vec![(DRM_FORMAT_ABGR8888, set.to_vec())]
    }

    #[test]
    fn gpu_only_takes_the_probed_set() {
        assert_eq!(modifiers_for(WINDOW, RGBA_8888, &probe(&NVIDIA)), NVIDIA);
        // D1 (a): Intel's full set, LINEAR not preferred
        assert_eq!(modifiers_for(WINDOW, RGBA_8888, &probe(&INTEL)), INTEL);
        assert!(modifiers_for(GPU_TEXTURE, RGBA_8888, &probe(&[])).is_empty());
    }

    #[test]
    fn cpu_usage_is_linear_or_refused() {
        // CPU + sample on NVIDIA: LINEAR (external-only sampling works, S1)
        assert_eq!(
            modifiers_for(CPU_MASK | GPU_TEXTURE, RGBA_8888, &probe(&NVIDIA)),
            LIN
        );
        // CPU + render on Intel: LINEAR is in the renderable set
        assert_eq!(
            modifiers_for(CPU_MASK | WINDOW, RGBA_8888, &probe(&INTEL)),
            LIN
        );
        // D2 (a): CPU + render on NVIDIA: refused, named
        let mut d = Descriptor {
            name: b"cpu-rt".to_vec(),
            width: 64,
            height: 64,
            layer_count: 1,
            format: RGBA_8888,
            usage: CPU_MASK | WINDOW,
            reserved_size: 0,
            option_names: vec![],
            modifiers: vec![],
        };
        d.modifiers = modifiers_for(d.usage, d.format, &probe(&NVIDIA));
        let e = d.check().expect_err("refused");
        assert_eq!(e, Refusal::NoModifierForUsage(CPU_MASK | WINDOW));
        assert!(e.to_string().contains("no supported modifier for usage"));
    }

    /// No list for the fourcc (WSL, headless), or BLOB: `[LINEAR]` whatever the usage -- the pre-modifier allocation.
    #[test]
    fn linear_tier_without_a_probe_list() {
        for u in [WINDOW, CPU_MASK | WINDOW, GPU_TEXTURE] {
            assert_eq!(modifiers_for(u, RGBA_8888, &[]), LIN);
        }
        assert_eq!(modifiers_for(CPU_MASK, BLOB, &probe(&NVIDIA)), LIN);
    }
}
