// SPDX-License-Identifier: MIT
//! The seccomp target arch of the host this crate is built for: the only per-arch row of the app filter (seccompiler names an arch's audit value).
//! seccompiler 0.5 knows x86_64 and aarch64 only; riscv64 (a Tier 1 host later) needs seccompiler support or another backend before its build compiles.

use seccompiler::TargetArch;

#[cfg(target_arch = "x86_64")]
pub const HOST: TargetArch = TargetArch::x86_64;
#[cfg(target_arch = "aarch64")]
pub const HOST: TargetArch = TargetArch::aarch64;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("atl-broker's app seccomp filter has no seccompiler target arch for this host");
