// SPDX-License-Identifier: MIT
//! The guest architectures the QEMU engine runs (phase4-32bit P3): one row each -- ISA, pointer width, ELF machine, calling convention -- the one definition every guest-dependent part of the core reads (engine/mod.rs `Active::Qemu`, dispatch.rs placement, the engine `.so` and guest library paths).

use crate::engine::dispatch::{AAPCS32, AAPCS64, HostRules};

/// A guest architecture of the QEMU engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    /// arm64-v8a: LP64, AAPCS64
    Aarch64,
    /// armeabi-v7a: ILP32, AAPCS32 base standard (softfp)
    Arm,
}

/// One guest architecture's facts.
struct Row {
    isa: &'static str,
    pointer_bits: u32,
    machine: object::elf::Machine,
    rules: HostRules,
}

const AARCH64: Row = Row {
    isa: "arm64",
    pointer_bits: 64,
    machine: object::elf::EM_AARCH64,
    rules: AAPCS64,
};

const ARM: Row = Row {
    isa: "arm",
    pointer_bits: 32,
    machine: object::elf::EM_ARM,
    rules: AAPCS32,
};

impl Arch {
    /// Every guest architecture.
    pub const ALL: [Arch; 2] = [Arch::Aarch64, Arch::Arm];

    fn row(self) -> &'static Row {
        match self {
            Arch::Aarch64 => &AARCH64,
            Arch::Arm => &ARM,
        }
    }

    /// The architecture of ART's ISA name `isa` (arches.toml row name), if QEMU runs it.
    pub fn of_isa(isa: &str) -> Option<Arch> {
        Arch::ALL.into_iter().find(|a| a.isa() == isa)
    }

    /// The ISA name (ART's; atl-android-paths `native_bridge_bin` / `native_bridge_lib` / `engine_library` directory).
    pub fn isa(self) -> &'static str {
        self.row().isa
    }

    /// The pointer width in bits: picks the guest library dir (atl-android-paths `native_bridge_lib`).
    pub fn pointer_bits(self) -> u32 {
        self.row().pointer_bits
    }

    /// The ELF machine of the guest's libraries (elf.rs `classify` under this engine).
    pub fn machine(self) -> object::elf::Machine {
        self.row().machine
    }

    /// The guest's calling convention: how its trap lanes are filled and read (dispatch.rs `place`).
    pub(in crate::engine) fn rules(self) -> HostRules {
        self.row().rules
    }
}
