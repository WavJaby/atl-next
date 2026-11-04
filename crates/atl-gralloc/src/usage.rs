// SPDX-License-Identifier: MIT
//! `android.hardware.graphics.common.BufferUsage` bits (BufferUsage.aidl) and which of them the allocator honours.

/// `CPU_READ_MASK`, `CPU_WRITE_MASK`: the values 0 (never), 2 (rarely), 3 (often) in the low nibble / its shift by 4.
pub const CPU_READ_MASK: u64 = 0xf;
pub const CPU_WRITE_MASK: u64 = 0xf << 4;
pub const CPU_MASK: u64 = CPU_READ_MASK | CPU_WRITE_MASK;
pub const GPU_TEXTURE: u64 = 1 << 8;
pub const GPU_RENDER_TARGET: u64 = 1 << 9;
pub const COMPOSER_OVERLAY: u64 = 1 << 11;
pub const COMPOSER_CLIENT_TARGET: u64 = 1 << 12;
pub const COMPOSER_CURSOR: u64 = 1 << 15;

/// Usage bits the allocator accepts: CPU access, and the GPU / composer bits that libui and BufferQueue consumers add to every window buffer (BLASTBufferQueue consumers ask for texture + composer).
/// They constrain nothing here yet: the one memory kind is linear dma-buf memory every consumer can map, so they are honoured vacuously.
/// Everything else is refused: PROTECTED (no secure memory), VIDEO_ENCODER / VIDEO_DECODER / CAMERA_* / SENSOR_DIRECT_DATA / HW_IMAGE_ENCODER / RENDERSCRIPT (no such consumer), GPU_DATA_BUFFER / GPU_CUBE_MAP / GPU_MIPMAP_COMPLETE (not an image buffer), FRONT_BUFFER (needs a coherent-scanout memory kind), and the vendor masks (no vendor).
pub const SUPPORTED: u64 = CPU_MASK
    | GPU_TEXTURE
    | GPU_RENDER_TARGET
    | COMPOSER_OVERLAY
    | COMPOSER_CLIENT_TARGET
    | COMPOSER_CURSOR;

/// Whether `usage` reads the CPU way: the read nibble is not NEVER (0).
pub fn cpu_reads(usage: u64) -> bool {
    usage & CPU_READ_MASK != 0
}

/// Whether `usage` writes the CPU way.
pub fn cpu_writes(usage: u64) -> bool {
    usage & CPU_WRITE_MASK != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_and_flags() {
        // CPU_READ_OFTEN = 3, CPU_WRITE_OFTEN = 3 << 4 = 48 (BufferUsage.aidl)
        assert!(cpu_reads(3) && !cpu_writes(3));
        assert!(cpu_writes(48) && !cpu_reads(48));
        assert!(!cpu_reads(GPU_TEXTURE) && !cpu_writes(GPU_TEXTURE));
        assert_eq!(CPU_WRITE_MASK, 240);
    }

    #[test]
    fn protected_and_vendor_refused() {
        for bit in [
            1u64 << 14,
            1 << 16,
            1 << 22,
            1 << 32,
            0xf << 28,
            0xffff << 48,
        ] {
            assert_eq!(bit & SUPPORTED, 0, "{bit:#x}");
        }
    }
}
