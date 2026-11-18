// SPDX-License-Identifier: MIT
// rv-qemu unwinder check (rv_qemu.rs): the converter-built libunwindstack unwinds from a signal's ucontext, as ART's fatal-signal dump does (runtime_common.cc -> AndroidLocalUnwinder).
// RegsRiscv64::CreateFromUcontext reads vlenb (patch 0002: only when AT_HWCAP has V; before it, csrr vlenb raised SIGILL inside the handler on a core without V).
// Lines: `vlenb <n>` (the value the unwinder stored), `frames <n> main at #<k>` or `frames <n> no main: <names>`. Exit 0 when the unwind reached main.
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include <memory>
#include <string>

#include <unwindstack/AndroidUnwinder.h>
#include <unwindstack/MachineRiscv64.h>
#include <unwindstack/Regs.h>

static int g_result = 2;

static void handler(int, siginfo_t*, void* ucontext) {
  std::unique_ptr<unwindstack::Regs> regs(
      unwindstack::Regs::CreateFromUcontext(unwindstack::ARCH_RISCV64, ucontext));
  printf("vlenb %llu\n", static_cast<unsigned long long>(
                             reinterpret_cast<uint64_t*>(regs->RawData())[unwindstack::RISCV64_REG_VLENB]));
  // memcpy reads, not ART's default process_vm_readv: QEMU linux-user has no process_vm_readv (ENOSYS), so the default unwinder stops at frame 0 ("Memory Invalid") under qemu-user. The register and CFI path under test is the same.
  std::shared_ptr<unwindstack::Memory> memory = unwindstack::Memory::CreateProcessMemoryLocalUnsafe();
  unwindstack::AndroidLocalUnwinder unwinder(memory);
  unwindstack::AndroidUnwinderData data;
  if (!unwinder.Unwind(ucontext, data)) {
    printf("frames 0 unwind failed: %s\n", data.GetErrorString().c_str());
    return;
  }
  std::string names;
  for (const auto& frame : data.frames) {
    if (frame.function_name == "main") {
      printf("frames %zu main at #%zu\n", data.frames.size(), frame.num);
      g_result = 0;
      return;
    }
    names += " | ";
    names += unwinder.FormatFrame(frame);
  }
  printf("frames %zu no main (%s):%s\n", data.frames.size(), data.GetErrorString().c_str(), names.c_str());
  g_result = 1;
}

// a frame of its own between main and the signal, kept out of line so the unwind has a caller to step through
__attribute__((noinline)) static void raise_in_callee() {
  raise(SIGUSR1);
  __asm__ volatile("" ::: "memory");
}

int main() {
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_sigaction = handler;
  sa.sa_flags = SA_SIGINFO;
  sigaction(SIGUSR1, &sa, nullptr);
  raise_in_callee();
  fflush(stdout);
  return g_result;
}
