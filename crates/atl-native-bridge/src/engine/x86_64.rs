// SPDX-License-Identifier: MIT
//! x86_64 crossing. Thread pointer = FS base for glibc and bionic alike; FSGSBASE instructions (Linux 5.9+, `HWCAP2_FSGSBASE`) switch it without a syscall.
//! The engine never writes GS: ART owns it on every thread (`Thread*`, art thread_x86_64.cc `arch_prctl(ARCH_SET_GS)`), and host code -- JIT/nterp Java reached by an up call included -- reads `%gs:` for it.
//! The thread's `HostThread` is found: host side (FS = host) from host TLS (thread.rs `attach_current`); guest->host and signal entries (FS = guest) from the guest TCB's `TLS_SLOT_NATIVE_BRIDGE_GUEST_STATE` (bionic tls_defines.h, x86_64 slot 10: reserved by bionic for the native bridge, unused by guest bionic itself), written by `init_tcb`; across a crossing's own call, from callee-saved `%rbx` (caller's value kept in the shadow frame).
//! berberis needs no guest-side lookup (its translated code holds the guest state in a register; host side: `GetCurrentGuestThread`, a host pthread key); our guest code runs natively, hence the TCB slot.
//! Guest->host keeps the guest return address, guest FS and `%rbx` on the `HostThread` shadow stack, not the machine stack: stack arguments and `%al` (variadic vector count) reach the host function untouched, so one body serves every signature (bridge1-step4-plan section 3).

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use super::image::Image;

/// Guest->host frames one thread can nest (JNI down -> up -> Java -> down chains); overflow traps (`ud2`) instead of writing past the record.
const SHADOW_FRAMES: usize = 64;
/// Words per frame, both directions: return address, guest FS (up) / guest target (down), host target (up) / unused (down), the caller's `%rbx`.
const FRAME_WORDS: usize = 4;

/// Byte offset of `TLS_SLOT_NATIVE_BRIDGE_GUEST_STATE` from the guest thread pointer (generated from bionic's tls_defines.h).
const SLOT_OFFSET: usize = {
    let slot = super::thread::layout::TLS_SLOT_NATIVE_BRIDGE_GUEST_STATE;
    assert!(
        slot >= 0,
        "x86_64 bionic slots lie above the thread pointer"
    );
    slot as usize * std::mem::size_of::<usize>()
};

/// Per-thread crossing record, addressed through a register by the asm below; field offsets are fixed by it (asserted).
#[repr(C)]
pub struct HostThread {
    shadow_sp: *mut usize,
    host_fs: usize,
    guest_fs: usize,
    shadow_end: *mut usize,
    shadow: [usize; SHADOW_FRAMES * FRAME_WORDS],
}

const _: () = {
    assert!(std::mem::offset_of!(HostThread, shadow_sp) == 0);
    assert!(std::mem::offset_of!(HostThread, host_fs) == 8);
    assert!(std::mem::offset_of!(HostThread, guest_fs) == 16);
    assert!(std::mem::offset_of!(HostThread, shadow_end) == 24);
};

/// Bootstrap record: the starting thread's, while its `Tp::Bootstrap` call runs (`Identity::call`: `Guest::start`'s `__linker_init`).
/// Defends against: the NBS linker runs `__linker_init` and the guest libc constructors (which already call the host: vdso `native_bridge_intercept_symbol`) first on the host's FS, then on its own stack `temp_tcb` (bionic `__libc_init_main_thread_early`), whose slot the engine cannot write; the main thread's real TCB is the engine's (`Guest::start`, after `__native_bridge_config_static_tls`).
/// Delete when the guest main thread's first TCB carries the slot (e.g. NBS asks the host for it at `__libc_init_main_thread_early`).
#[unsafe(no_mangle)]
static atl_nb_bootstrap: AtomicUsize = AtomicUsize::new(0);
/// The bootstrap thread's tid: a signal on another thread during bootstrap must not get its host FS.
#[unsafe(no_mangle)]
static atl_nb_bootstrap_tid: AtomicI32 = AtomicI32::new(0);

impl HostThread {
    /// A record for the calling thread; leaked: its guest TCB keeps pointing at it.
    pub fn leak() -> &'static mut HostThread {
        let t = Box::leak(Box::new(HostThread {
            shadow_sp: std::ptr::null_mut(),
            host_fs: 0,
            guest_fs: 0,
            shadow_end: std::ptr::null_mut(),
            shadow: [0; SHADOW_FRAMES * FRAME_WORDS],
        }));
        let range = t.shadow.as_mut_ptr_range();
        t.shadow_sp = range.start;
        t.shadow_end = range.end;
        t
    }

    /// The guest thread pointer the last crossing back from the guest saw (0 before the first).
    pub fn guest_fs(&self) -> usize {
        self.guest_fs
    }

    /// Sets the guest thread pointer the next crossing into the guest installs (a guest TCB; step 4.2's attach makes one per thread).
    pub fn set_guest_fs(&mut self, v: usize) {
        self.guest_fs = v;
    }

    /// The host thread pointer the last crossing into the guest saved.
    pub fn host_fs(&self) -> usize {
        self.host_fs
    }
}

/// The guest TCB words the engine owns: `%fs:0` = the thread pointer itself (TLS variant II; bionic `TLS_SLOT_SELF` = 0, `__init_tcb`), and the native-bridge slot = `ht`, which the guest->host and signal entries read.
///
/// # Safety
/// `tp` must be a guest TCB of at least `SLOT_OFFSET + 8` bytes (bionic's TCB: `MAX_TLS_SLOT` + 1 words), live while `ht` is used on it.
pub unsafe fn init_tcb(tp: usize, ht: *mut HostThread) {
    // SAFETY: caller contract.
    unsafe {
        (tp as *mut usize).write(tp);
        ((tp + SLOT_OFFSET) as *mut usize).write(ht as usize);
    }
}

/// The guest ISA (atl-android-paths `native_bridge_bin` / `native_bridge_lib` directory): the identity guest is the host's own.
pub const GUEST_ISA: &str = std::env::consts::ARCH;

/// The kernel's auxv for this guest: the process's own, the identity guest runs on the real kernel.
pub fn kernel_auxv() -> Result<Vec<(usize, usize)>, String> {
    super::args::process_auxv()
}

/// Makes `images` reachable by guest code: nothing to do, guest memory is host memory here.
pub fn map_guest(_images: &[&Image]) -> Result<(), String> {
    Ok(())
}

/// Whether the CPU and kernel let user code run `rdfsbase`/`wrfsbase` (else they fault with SIGILL).
pub fn supported() -> bool {
    const HWCAP2_FSGSBASE: libc::c_ulong = 1 << 1;
    // SAFETY: getauxval reads the process's auxiliary vector; no pointers involved.
    unsafe { libc::getauxval(libc::AT_HWCAP2) & HWCAP2_FSGSBASE != 0 }
}

/// The thread pointer now.
pub fn fs_base() -> usize {
    let v: usize;
    // SAFETY: rdfsbase reads FS base into a register; `supported()` is checked by the engine before any crossing.
    unsafe { std::arch::asm!("rdfsbase {}", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Sets the thread pointer to `v`. Only for restoring a value `fs_base` read on this thread.
///
/// # Safety
/// `v` must be a valid thread pointer of this thread; every TLS access after it uses `v`.
pub unsafe fn set_fs_base(v: usize) {
    // SAFETY: caller contract above.
    unsafe { std::arch::asm!("wrfsbase {}", in(reg) v, options(nomem, nostack, preserves_flags)) };
}

#[cfg(not(atl_nb_no_fs_restore))]
macro_rules! restore_host_fs {
    () => {
        "    mov 8(%rbx), %rcx\n    wrfsbase %rcx\n"
    };
}
// deliberate break of gates g1/g2: the crossing returns to the host on the guest's thread pointer
#[cfg(atl_nb_no_fs_restore)]
macro_rules! restore_host_fs {
    () => {
        ""
    };
}

/// Host functions the thunks reach, by thunk index (`bind`). `atl_nb_cross_to_host` reads them.
#[unsafe(no_mangle)]
static atl_nb_host_targets: [AtomicUsize; THUNKS] = [const { AtomicUsize::new(0) }; THUNKS];
/// Next free thunk.
static NEXT: AtomicUsize = AtomicUsize::new(0);
/// Thunks: one per binding, derived from the generated binding table (bindings.rs `THUNKS`); the asm's `.rept` takes this count.
pub const THUNKS: usize = super::bindings::THUNKS;
/// Bytes per thunk: `call rel32` (5) padded to 8 (`.p2align 3` below).
const THUNK_BYTES: usize = 8;

/// The up crossing's call of its host target (`%r11` = frame end, `%rbx` = record).
#[cfg(not(atl_nb_foreign_gs))]
macro_rules! up_call {
    () => {
        "    call *-16(%r11)\n"
    };
}
// deliberate break of the JIT up-call gate: GS = the record (not ART's Thread*) while the host target runs, ART's GS kept in the frame's target word
#[cfg(atl_nb_foreign_gs)]
macro_rules! up_call {
    () => {
        concat!(
            "    rdgsbase %r10\n",
            "    xchg %r10, -16(%r11)\n",
            "    wrgsbase %rbx\n",
            "    call *%r10\n",
            "    mov 0(%rbx), %r10\n",
            "    mov -16(%r10), %r10\n",
            "    wrgsbase %r10\n",
        )
    };
}

std::arch::global_asm!(
    concat!(
        // atl_nb_call_guest(ht, f, a, b, c, d) -> f(a, b, c, d) on the guest thread pointer
        // hidden: rip-relative references must not be preemptible in the cdylib
        ".hidden atl_nb_host_targets, atl_nb_host_thunks, atl_nb_call_guest, atl_nb_cross_to_host, atl_nb_jni_up, atl_nb_jni_stubs, atl_nb_up, atl_nb_cross_to_guest, atl_nb_cross_to_guest_env, atl_nb_cross_to_guest_vm, atl_nb_attach_fn, atl_nb_signal_entry, atl_nb_signal_handlers, atl_nb_bootstrap, atl_nb_bootstrap_tid\n",
        ".text\n",
        ".globl atl_nb_call_guest\n",
        ".type atl_nb_call_guest,@function\n",
        "atl_nb_call_guest:\n",
        "    push %rbx\n",
        "    mov %rdi, %rbx\n",
        "    rdfsbase %rax\n",
        "    mov %rax, 8(%rbx)\n",
        "    mov 16(%rbx), %rax\n",
        "    test %rax, %rax\n",
        "    jz 1f\n",
        "    wrfsbase %rax\n",
        "1:\n",
        "    mov %rsi, %rax\n",
        "    mov %rdx, %rdi\n",
        "    mov %rcx, %rsi\n",
        "    mov %r8, %rdx\n",
        "    mov %r9, %rcx\n",
        "    call *%rax\n",
        "    rdfsbase %rcx\n",
        "    mov %rcx, 16(%rbx)\n",
        restore_host_fs!(),
        "    pop %rbx\n",
        "    ret\n",
        ".size atl_nb_call_guest, .-atl_nb_call_guest\n",
        // guest->host: entered from thunk i by `call`; [rsp] = thunk i + 5, [rsp+8] = guest return
        ".globl atl_nb_cross_to_host\n",
        ".type atl_nb_cross_to_host,@function\n",
        "atl_nb_cross_to_host:\n",
        "    pop %r11\n",
        "    lea atl_nb_host_thunks+5(%rip), %r10\n",
        "    sub %r10, %r11\n",
        "    lea atl_nb_host_targets(%rip), %r10\n",
        "    mov (%r10,%r11), %r11\n",
        "    jmp atl_nb_up\n",
        ".size atl_nb_cross_to_host, .-atl_nb_cross_to_host\n",
        // guest->host through a JNI table (jni.rs): entered from JNI stub i by `call`; %rdi = the guest JNIEnv/JavaVM object, whose word 1 is the host one (jni.rs GuestObj).
        // Arg rewrite only: %rdi = host object, target = its own function table's slot i (ART swaps an env's table for CheckJNI, so read per call). Everything else reaches the host function untouched (variadic Call*Method: stack args, %al).
        ".globl atl_nb_jni_up\n",
        ".type atl_nb_jni_up,@function\n",
        "atl_nb_jni_up:\n",
        "    pop %r11\n",
        "    lea atl_nb_jni_stubs+5(%rip), %r10\n",
        "    sub %r10, %r11\n",
        "    mov 8(%rdi), %rdi\n",
        "    mov (%rdi), %r10\n",
        "    mov (%r10,%r11), %r11\n",
        "    jmp atl_nb_up\n",
        ".size atl_nb_jni_up, .-atl_nb_jni_up\n",
        // the up crossing proper: %r11 = host target, [rsp] = guest return, FS = guest
        // record: the bootstrap one while it is set (only the starting thread runs guest code then), else the guest TCB's slot
        // frame: guest return, guest FS, host target, guest %rbx; %rbx = the record while the host target runs (callee-saved by it)
        ".type atl_nb_up,@function\n",
        "atl_nb_up:\n",
        "    mov atl_nb_bootstrap(%rip), %r10\n",
        "    test %r10, %r10\n",
        "    jnz 1f\n",
        "    mov %fs:{slot}, %r10\n",
        "    test %r10, %r10\n",
        "    jz 9f\n",
        "1:\n",
        "    push %r11\n",
        "    mov 0(%r10), %r11\n",
        "    cmp 24(%r10), %r11\n",
        "    jae 8f\n",
        "    mov %rbx, 24(%r11)\n",
        "    pop 16(%r11)\n",
        "    pop 0(%r11)\n",
        "    rdfsbase %rbx\n",
        "    mov %rbx, 8(%r11)\n",
        "    mov %r10, %rbx\n",
        "    add $32, %r11\n",
        "    mov %r11, 0(%rbx)\n",
        "    mov 8(%rbx), %r10\n",
        "    wrfsbase %r10\n",
        up_call!(),
        "    mov 0(%rbx), %r10\n",
        "    sub $32, %r10\n",
        "    mov %r10, 0(%rbx)\n",
        "    mov 8(%r10), %r11\n",
        "    wrfsbase %r11\n",
        "    push 0(%r10)\n",
        "    mov 24(%r10), %rbx\n",
        "    ret\n",
        // no way up: report (r10 = the record, 0 when the slot held none), then trap
        "8:  pop %r11\n",
        "9:  mov %r10, %rdi\n",
        "    mov %r11, %rsi\n",
        "    mov (%rsp), %rdx\n",
        "    mov %rsp, %rcx\n",
        "    rdfsbase %r8\n",
        "    and $-16, %rsp\n",
        "    call {up_failed}\n",
        "    ud2\n",
        ".size atl_nb_up, .-atl_nb_up\n",
        // host->guest (down call): entered by `jmp` from a down stub with %r10 = guest target; [rsp] = host return, then the caller's stack args.
        // Frame (4 words, as up): host return, guest target, host FS, the caller's %rbx; %rbx = the record while the guest runs (callee-saved by it).
        // Argument registers, %al and stack args reach the guest untouched; only %r10/%r11 (and %rcx after the call, not a return register) are scratch.
        // Three entries, by what the first argument is (`Down`): none to rewrite, a host JNIEnv, a host JavaVM; the kind rides in a pushed slot until attach has read it, which then carries the caller's %rbx.
        ".globl atl_nb_cross_to_guest\n",
        ".type atl_nb_cross_to_guest,@function\n",
        "atl_nb_cross_to_guest:\n",
        "    push $0\n",
        "    jmp 2f\n",
        ".globl atl_nb_cross_to_guest_env, atl_nb_cross_to_guest_vm\n",
        "atl_nb_cross_to_guest_env:\n",
        "    push $1\n",
        "    jmp 2f\n",
        "atl_nb_cross_to_guest_vm:\n",
        "    push $2\n",
        "2:\n",
        // the record always comes from host TLS (attach; FS is the host's here, nested or not): an incoming GS base is not trusted (a nonzero one that was no record trapped, app run 2026-11-06)
        // every argument register saved around the attach call; with the kind slot, 9 pushes + 128 keep the call 16-aligned
        "    push %r10\n",
        "    push %rdi\n",
        "    push %rsi\n",
        "    push %rdx\n",
        "    push %rcx\n",
        "    push %r8\n",
        "    push %r9\n",
        "    push %rax\n",
        "    sub $128, %rsp\n",
        "    movdqu %xmm0, 0(%rsp)\n",
        "    movdqu %xmm1, 16(%rsp)\n",
        "    movdqu %xmm2, 32(%rsp)\n",
        "    movdqu %xmm3, 48(%rsp)\n",
        "    movdqu %xmm4, 64(%rsp)\n",
        "    movdqu %xmm5, 80(%rsp)\n",
        "    movdqu %xmm6, 96(%rsp)\n",
        "    movdqu %xmm7, 112(%rsp)\n",
        // attach(&saved %rdi, kind): the record; rewrites the saved first argument to the guest object for kinds 1 and 2
        "    lea 176(%rsp), %rdi\n",
        "    mov 192(%rsp), %rsi\n",
        "    call *atl_nb_attach_fn(%rip)\n",
        "    mov %rax, %r11\n",
        "    movdqu 0(%rsp), %xmm0\n",
        "    movdqu 16(%rsp), %xmm1\n",
        "    movdqu 32(%rsp), %xmm2\n",
        "    movdqu 48(%rsp), %xmm3\n",
        "    movdqu 64(%rsp), %xmm4\n",
        "    movdqu 80(%rsp), %xmm5\n",
        "    movdqu 96(%rsp), %xmm6\n",
        "    movdqu 112(%rsp), %xmm7\n",
        "    add $128, %rsp\n",
        "    pop %rax\n",
        "    pop %r9\n",
        "    pop %r8\n",
        "    pop %rcx\n",
        "    pop %rdx\n",
        "    pop %rsi\n",
        "    pop %rdi\n",
        "    pop %r10\n",
        // [rsp] = the kind slot, now the caller's %rbx; %r11 = the record
        "    mov %rbx, (%rsp)\n",
        "    mov %r11, %rbx\n",
        "    mov 0(%rbx), %r11\n",
        "    cmp 24(%rbx), %r11\n",
        "    jae 9f\n",
        "    pop 24(%r11)\n",
        "    pop 0(%r11)\n",
        "    mov %r10, 8(%r11)\n",
        "    rdfsbase %r10\n",
        "    mov %r10, 16(%r11)\n",
        "    mov %r10, 8(%rbx)\n",
        "    add $32, %r11\n",
        "    mov %r11, 0(%rbx)\n",
        "    mov 16(%rbx), %r10\n",
        "    wrfsbase %r10\n",
        "    call *-24(%r11)\n",
        "    rdfsbase %r10\n",
        "    mov %r10, 16(%rbx)\n",
        "    mov 0(%rbx), %r11\n",
        "    sub $32, %r11\n",
        "    mov %r11, 0(%rbx)\n",
        "    mov 16(%r11), %r10\n",
        "    wrfsbase %r10\n",
        "    push 0(%r11)\n",
        "    mov 24(%r11), %rbx\n",
        "    ret\n",
        "9:  ud2\n",
        ".size atl_nb_cross_to_guest, .-atl_nb_cross_to_guest\n",
        // signal entry: the kernel delivers on whatever FS the thread had (guest or host) and sigreturn does not restore FS base; the interrupted FS decides:
        // a glibc TCB (tcbhead_t.self at %fs:16 == FS, sysdeps/x86_64/nptl/tls.h; bionic's slot 2 TLS_SLOT_APP never holds the thread pointer) = host, left alone;
        // else a guest TCB whose slot names the record -> run the host handler on its host FS; else (bootstrap: the linker's own TCB, slot 0) the bootstrap record if this is its thread. Interrupted FS put back after.
        ".globl atl_nb_signal_entry\n",
        ".type atl_nb_signal_entry,@function\n",
        "atl_nb_signal_entry:\n",
        "    push %rbx\n",
        "    push %r12\n",
        "    sub $8, %rsp\n",
        "    rdfsbase %rbx\n",
        "    test %rbx, %rbx\n",
        "    jz 4f\n",
        "    cmp %fs:16, %rbx\n",
        "    je 1f\n",
        "    mov %fs:{slot}, %r12\n",
        "    test %r12, %r12\n",
        "    jnz 3f\n",
        "    mov atl_nb_bootstrap(%rip), %r12\n",
        "    test %r12, %r12\n",
        "    jz 4f\n",
        "    push %rdi\n",
        "    push %rsi\n",
        "    push %rdx\n",
        "    mov ${gettid}, %eax\n",
        "    syscall\n",
        "    pop %rdx\n",
        "    pop %rsi\n",
        "    pop %rdi\n",
        "    cmp atl_nb_bootstrap_tid(%rip), %eax\n",
        "    jne 4f\n",
        "3:\n",
        "    mov 8(%r12), %r11\n",
        "    test %r11, %r11\n",
        "    jz 4f\n",
        "    wrfsbase %r11\n",
        "    jmp 1f\n",
        // no host FS found (FS 0, or a guest TCB without a record off the bootstrap thread): the host handler would run on a foreign thread pointer; report first (TLS-free), then run it as before
        "4:\n",
        "    push %rdi\n",
        "    push %rsi\n",
        "    push %rdx\n",
        "    sub $8, %rsp\n",
        "    mov %rbx, %rcx\n",
        "    call {no_host_fs}\n",
        "    add $8, %rsp\n",
        "    pop %rdx\n",
        "    pop %rsi\n",
        "    pop %rdi\n",
        "1:\n",
        "    movslq %edi, %rax\n",
        "    lea atl_nb_signal_handlers(%rip), %r11\n",
        "    call *(%r11,%rax,8)\n",
        "    wrfsbase %rbx\n",
        "    add $8, %rsp\n",
        "    pop %r12\n",
        "    pop %rbx\n",
        "    ret\n",
        ".size atl_nb_signal_entry, .-atl_nb_signal_entry\n",
        ".p2align 3\n",
        ".globl atl_nb_host_thunks\n",
        "atl_nb_host_thunks:\n",
        ".rept {thunks}\n",
        "    call atl_nb_cross_to_host\n",
        "    .p2align 3\n",
        ".endr\n",
        // JNI stub i = slot i of every guest JNI table (jni.rs); same 8-byte shape, so atl_nb_jni_up's index arithmetic yields slot * 8
        ".p2align 3\n",
        ".globl atl_nb_jni_stubs\n",
        "atl_nb_jni_stubs:\n",
        ".rept 256\n",
        "    call atl_nb_jni_up\n",
        "    .p2align 3\n",
        ".endr\n",
    ),
    slot = const SLOT_OFFSET,
    thunks = const THUNKS,
    gettid = const libc::SYS_gettid,
    up_failed = sym up_failed,
    no_host_fs = sym no_host_fs,
    options(att_syntax)
);

/// `atl_nb_signal_entry` found no host thread pointer for the interrupted FS `fs`: reports, then the host handler runs on `fs` anyway (it may well fault: a nested fault on a blocked signal kills the process with no further output, so this line is the last one).
extern "C" fn no_host_fs(
    sig: i32,
    info: *const libc::siginfo_t,
    uc: *const libc::c_void,
    fs: usize,
) {
    // SAFETY: the kernel's arguments to this delivery's handler, passed through by the entry.
    unsafe { fault_report("no host thread pointer for the handler", sig, info, uc, fs) };
}

/// Last-chance fault report on stderr: signal, code, fault address, pc + sp, thread name, the mappings of pc and fault address. TLS-free and heap-free (stack buffers, raw syscalls): callable on any thread pointer from a signal handler. `fs`: the interrupted FS base, 0 when the caller does not know it.
///
/// # Safety
/// `info` / `uc`: null or the kernel's siginfo / ucontext of the signal being handled.
pub unsafe fn fault_report(
    why: &str,
    sig: i32,
    info: *const libc::siginfo_t,
    uc: *const libc::c_void,
    fs: usize,
) {
    use std::io::Write;
    // SAFETY: the kernel's siginfo / ucontext for this delivery, or null.
    let (code, addr) = unsafe {
        info.as_ref()
            .map_or((0, 0), |i| (i.si_code, i.si_addr() as usize))
    };
    // SAFETY: as above; x86_64 glibc `ucontext_t` is the kernel frame's layout.
    let (pc, sp) = unsafe {
        uc.cast::<libc::ucontext_t>().as_ref().map_or((0, 0), |u| {
            let g = &u.uc_mcontext.gregs;
            (
                g[libc::REG_RIP as usize] as usize,
                g[libc::REG_RSP as usize] as usize,
            )
        })
    };
    let mut comm = [0u8; 32];
    let fd = raw_open(c"/proc/thread-self/comm");
    let comm_len = if fd < 0 {
        0
    } else {
        let n = raw_read(fd, &mut comm).max(0) as usize;
        raw_close(fd);
        n
    };
    let name = comm[..comm_len]
        .strip_suffix(b"\n")
        .unwrap_or(&comm[..comm_len]);
    let tid = raw_syscall3(libc::SYS_gettid, 0, 0, 0);
    let mut line = [0u8; 256];
    let mut w = &mut line[..];
    let _ = write!(
        w,
        "atl-native-bridge: fatal-signal report: {why}: sig {sig} code {code} addr {addr:#x} pc {pc:#x} sp {sp:#x} fs {fs:#x} tid {tid} thread \""
    );
    let _ = w.write_all(name);
    let _ = writeln!(w, "\"");
    let left = w.len();
    raw_write_stderr(&line[..line.len() - left]);
    emit_mappings(&[pc, addr]);
    // stack words: the caller chain of frame-pointerless code; skipped when the fault is on the stack itself (overflow), where reading it would fault again inside the handler
    let span = STACK_WORDS * 8;
    if sp == 0 || (sp.saturating_sub(PAGE)..sp + span).contains(&addr) {
        return;
    }
    for row in 0..STACK_WORDS / 4 {
        let mut w = &mut line[..];
        let _ = write!(w, "atl-native-bridge: stack +{:#x}", row * 32);
        for i in 0..4 {
            // SAFETY: the interrupted thread's live stack, read upward from its sp; STACK_WORDS stays inside the frames above it.
            let word = unsafe { ((sp + (row * 4 + i) * 8) as *const usize).read() };
            let _ = write!(w, " {word:#x}");
        }
        let _ = writeln!(w);
        let left = w.len();
        raw_write_stderr(&line[..line.len() - left]);
    }
}

/// Raw 3-argument syscall. The diagnostics below run on a thread pointer that may not be glibc's (guest TCB, FS 0): glibc's open/read/write/close are cancellation points that read
/// THREAD_SELF (%fs:0x10) and its `syscall` sets errno through TLS, so either faults there (g5 caught it). Returns the kernel's value (negative errno on failure).
fn raw_syscall3(nr: libc::c_long, a: usize, b: usize, c: usize) -> isize {
    let ret: isize;
    // SAFETY: the callers pass only open/read/write/close/gettid with pointers to live buffers / NUL-terminated paths; the kernel clobbers rcx and r11 only.
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") nr as isize => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// `open(path, O_RDONLY)` without glibc (see [`raw_syscall3`]); negative on failure.
fn raw_open(path: &std::ffi::CStr) -> i32 {
    raw_syscall3(
        libc::SYS_openat,
        libc::AT_FDCWD as usize,
        path.as_ptr() as usize,
        libc::O_RDONLY as usize,
    ) as i32
}

/// `read` into `buf` without glibc; negative on failure.
fn raw_read(fd: i32, buf: &mut [u8]) -> isize {
    raw_syscall3(
        libc::SYS_read,
        fd as usize,
        buf.as_mut_ptr() as usize,
        buf.len(),
    )
}

/// `write(2, buf)` without glibc; a failed write loses only these bytes.
fn raw_write_stderr(buf: &[u8]) {
    raw_syscall3(libc::SYS_write, 2, buf.as_ptr() as usize, buf.len());
}

/// `close` without glibc.
fn raw_close(fd: i32) {
    raw_syscall3(libc::SYS_close, fd as usize, 0, 0);
}

/// Guard below sp that `fault_report` treats as the stack (one x86_64 page: the smallest guard a fault past sp lands in).
const PAGE: usize = 4096;

/// Guest stack words `up_failed` reports: enough for a few frames of caller chain.
const STACK_WORDS: usize = 96;

/// Whether maps line `line` (`start-end perms ...`, hex) covers `addr`.
fn maps_line_covers(line: &[u8], addr: usize) -> bool {
    let hex = |s: &[u8]| {
        s.iter().try_fold(0usize, |v, c| {
            Some(v.checked_mul(16)? + (*c as char).to_digit(16)? as usize)
        })
    };
    let mut range = line
        .split(|c| *c == b' ')
        .next()
        .unwrap_or(&[])
        .split(|c| *c == b'-');
    match (range.next().and_then(hex), range.next().and_then(hex)) {
        (Some(start), Some(end)) => (start..end).contains(&addr),
        _ => false,
    }
}

/// Writes to stderr the `/proc/self/maps` lines covering `addrs`; streamed through a stack buffer (see `up_failed`).
fn emit_mappings(addrs: &[usize]) {
    let fd = raw_open(c"/proc/self/maps");
    if fd < 0 {
        return;
    }
    let mut buf = [0u8; 8192];
    let mut have = 0;
    loop {
        let n = raw_read(fd, &mut buf[have..]);
        if n <= 0 {
            break;
        }
        have += n as usize;
        let mut done = 0;
        while let Some(nl) = buf[done..have].iter().position(|c| *c == b'\n') {
            let line = &buf[done..=done + nl];
            if addrs.iter().any(|a| maps_line_covers(line, *a)) {
                raw_write_stderr(line);
            }
            done += nl + 1;
        }
        buf.copy_within(done..have, 0);
        have -= done;
    }
    raw_close(fd);
}

/// `atl_nb_up` found no way up: names why on stderr. Runs on the guest FS: no TLS, no heap, so only stack buffers and raw write.
/// `ht` null: the guest TCB slot held no record. Else the shadow stack is full; its frames name what never returned (up: return, guest FS, host target, rbx; down: host return, guest target, host FS, rbx).
/// `sp`: the guest stack at the crossing; its words go out with the mappings of those that are code, the caller chain a frame-pointerless guest leaves. `fs`: the FS base found.
extern "C" fn up_failed(
    ht: *const HostThread,
    target: usize,
    guest_return: usize,
    sp: *const usize,
    fs: usize,
) {
    use std::io::Write;
    let mut line = [0u8; 160];
    let emit = raw_write_stderr;
    let mut w = &mut line[..];
    let why = if ht.is_null() {
        "no crossing record"
    } else {
        "shadow stack full"
    };
    let _ = writeln!(
        w,
        "atl-native-bridge: guest->host: {why}: target {target:#x} guest return {guest_return:#x} fs {fs:#x}"
    );
    let left = w.len();
    emit(&line[..line.len() - left]);
    let mut words = [target; STACK_WORDS];
    words[1] = guest_return;
    for (i, word) in words.iter_mut().enumerate().skip(2) {
        // SAFETY: the live stack of this thread, read upward from its top of stack; STACK_WORDS stays inside any frame chain that reached a crossing.
        *word = unsafe { sp.add(i).read() };
        let mut w = &mut line[..];
        let _ = writeln!(w, "atl-native-bridge: stack +{:#x} {word:#x}", i * 8);
        let left = w.len();
        emit(&line[..line.len() - left]);
    }
    emit_mappings(&words);
    if ht.is_null() {
        return;
    }
    // SAFETY: non-null `ht` is the live record of this thread, which is stopped here.
    let frames = unsafe { &(*ht).shadow };
    for f in frames.as_chunks::<FRAME_WORDS>().0 {
        let mut w = &mut line[..];
        let _ = writeln!(
            w,
            "atl-native-bridge: frame {:#x} {:#x} {:#x}",
            f[0], f[1], f[2]
        );
        let left = w.len();
        emit(&line[..line.len() - left]);
    }
}

// the `.rept` counts above are THUNKS and JNI_STUBS; the index arithmetic in atl_nb_cross_to_host / atl_nb_jni_up is THUNK_BYTES = one table slot; frames are 32 bytes in the asm
const _: () = assert!(JNI_STUBS == 256 && THUNK_BYTES == 8 && FRAME_WORDS * 8 == 32);

/// JNI stubs (bindings.rs `JNI_STUBS`, from build.rs); the asm's `.rept` above is its literal value.
pub const JNI_STUBS: usize = super::bindings::JNI_STUBS;

/// The guest-callable address of JNI table slot `slot` (jni.rs): crosses to the host object's own slot with the object rewritten.
pub fn jni_stub(slot: usize) -> usize {
    assert!(slot < JNI_STUBS, "JNI slot {slot} past the stub array");
    (&raw const atl_nb_jni_stubs) as usize + slot * THUNK_BYTES
}

unsafe extern "C" {
    fn atl_nb_call_guest(
        ht: *mut HostThread,
        f: usize,
        a: usize,
        b: usize,
        c: usize,
        d: usize,
    ) -> usize;
    static atl_nb_host_thunks: u8;
    static atl_nb_jni_stubs: u8;
    fn atl_nb_signal_entry(sig: i32, info: *mut libc::siginfo_t, uc: *mut libc::c_void);
}

/// Host signal handlers by signal number, read by `atl_nb_signal_entry` (0..=64; kernel `_NSIG` = 64).
#[unsafe(no_mangle)]
static atl_nb_signal_handlers: [AtomicUsize; SIGNALS] = [const { AtomicUsize::new(0) }; SIGNALS];
pub const SIGNALS: usize = 65;

/// The address to give host `sigaction` (SA_SIGINFO): calls `handler` on the host thread pointer whatever the interrupted code ran on, restores that one after.
/// `handler` must be stored first (`set_signal_handler`); it may rewrite the ucontext, but must return (a longjmp out would leave the host FS on guest code).
pub fn signal_entry() -> usize {
    atl_nb_signal_entry as *const () as usize
}

/// Stores the host handler `signal_entry` calls for `sig`.
pub fn set_signal_handler(sig: usize, handler: usize) {
    atl_nb_signal_handlers[sig].store(handler, Ordering::Release);
}

/// The identity engine: the host arch's crossing, one `atl_nb_call_guest` per call.
pub struct Identity;

impl super::Engine for Identity {
    type Cpu = HostThread;

    unsafe fn call(
        &self,
        cpu: &mut HostThread,
        tp: super::Tp,
        f: usize,
        args: &[usize; 4],
    ) -> usize {
        let bootstrap = match tp {
            // host FS: the asm installs guest_fs only when it is nonzero, so 0 keeps the call on the host thread pointer
            super::Tp::Bootstrap => {
                cpu.set_guest_fs(0);
                true
            }
            super::Tp::Guest(t) => {
                // a guest call with no TCB would run on the host TLS; attach and `Guest::start` always install one
                assert!(t != 0, "guest call with no guest thread pointer");
                // deliberate break of gate g1 (`ATL_NB_BREAK=swap_tp`): the guest enters on the host thread pointer
                cpu.set_guest_fs(if cfg!(atl_nb_swap_tp) { fs_base() } else { t });
                false
            }
        };
        if bootstrap {
            // SAFETY: gettid has no memory effects.
            atl_nb_bootstrap_tid.store(unsafe { libc::gettid() }, Ordering::Release);
            atl_nb_bootstrap.store(cpu as *mut HostThread as usize, Ordering::Release);
        }
        // SAFETY: `f` is guest code per the trait contract and `cpu` is this thread's record; the asm keeps callee-saved registers, restores the host FS and stores the thread pointer the guest left into `cpu`.
        let ret = unsafe { atl_nb_call_guest(cpu, f, args[0], args[1], args[2], args[3]) };
        if bootstrap {
            atl_nb_bootstrap.store(0, Ordering::Release);
        }
        ret
    }
}

/// A guest-callable address that crosses to host function `target`; None when the thunks are used up.
pub fn bind(target: usize) -> Option<usize> {
    let i = NEXT.fetch_add(1, Ordering::Relaxed);
    if i >= THUNKS {
        return None;
    }
    atl_nb_host_targets[i].store(target, Ordering::Release);
    let base = (&raw const atl_nb_host_thunks) as usize;
    Some(base + i * THUNK_BYTES)
}

/// What a down call's first argument is, so the crossing hands the guest its own object for it (jni.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Down {
    /// passed through: plain C, `@CriticalNative`
    Plain = 0,
    /// a host `JNIEnv*` -> this thread's guest env (Regular / FastNative JNI methods)
    Env = 1,
    /// a host `JavaVM*` -> the guest vm (`JNI_OnLoad`)
    Vm = 2,
}

/// What `atl_nb_cross_to_guest` calls on every down call: the thread's record, attached on demand (thread.rs `attach_current`), after rewriting the saved first argument for its kind.
#[unsafe(no_mangle)]
static atl_nb_attach_fn: extern "C" fn(*mut usize, usize) -> *mut HostThread = attach_entry;

extern "C" fn attach_entry(first: *mut usize, kind: usize) -> *mut HostThread {
    let ht = super::thread::attach_current();
    if kind != Down::Plain as usize {
        // SAFETY: `first` is the crossing's saved %rdi slot on its own stack frame.
        unsafe {
            *first = if kind == Down::Env as usize {
                super::jni::guest_env(*first)
            } else {
                super::jni::guest_vm(*first)
            };
        }
    }
    ht
}

unsafe extern "C" {
    fn atl_nb_cross_to_guest();
    fn atl_nb_cross_to_guest_env();
    fn atl_nb_cross_to_guest_vm();
}

/// Down stub: `movabs $target, %r10; movabs $<entry of its kind>, %r11; jmp *%r11` (10 + 10 + 3 bytes).
const DOWN_STUB: usize = 24;

/// Bytes of a down stub calling guest `target` from host code on any thread.
fn down_stub(target: usize, kind: Down) -> [u8; DOWN_STUB] {
    let entry = match kind {
        Down::Plain => atl_nb_cross_to_guest as *const () as usize,
        Down::Env => atl_nb_cross_to_guest_env as *const () as usize,
        Down::Vm => atl_nb_cross_to_guest_vm as *const () as usize,
    };
    let mut s = [0xcc; DOWN_STUB];
    s[0..2].copy_from_slice(&[0x49, 0xba]);
    s[2..10].copy_from_slice(&target.to_le_bytes());
    s[10..12].copy_from_slice(&[0x49, 0xbb]);
    s[12..20].copy_from_slice(&entry.to_le_bytes());
    s[20..23].copy_from_slice(&[0x41, 0xff, 0xe3]);
    s
}

/// The down-stub pool: one memfd mapped twice, RW for writing and RX for running (no page is ever writable and executable, and a stub in use never changes protection), grown by a chunk at a time.
struct Pool {
    rw: usize,
    rx: usize,
    used: usize,
}
const CHUNK: usize = 64 * 1024;

static POOL: std::sync::Mutex<Option<Pool>> = std::sync::Mutex::new(None);

fn new_chunk() -> Result<Pool, String> {
    // SAFETY: memfd + two shared mappings of CHUNK bytes; the fd is closed once mapped (the mappings keep the file).
    unsafe {
        let fd = libc::memfd_create(c"atl-nb-down".as_ptr(), libc::MFD_CLOEXEC);
        if fd < 0 || libc::ftruncate(fd, CHUNK as libc::off_t) != 0 {
            return Err(format!(
                "down stubs: memfd: {}",
                std::io::Error::last_os_error()
            ));
        }
        let map = |prot| libc::mmap(std::ptr::null_mut(), CHUNK, prot, libc::MAP_SHARED, fd, 0);
        let rw = map(libc::PROT_READ | libc::PROT_WRITE);
        let rx = map(libc::PROT_READ | libc::PROT_EXEC);
        libc::close(fd);
        if rw == libc::MAP_FAILED || rx == libc::MAP_FAILED {
            return Err(format!(
                "down stubs: mmap: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Pool {
            rw: rw as usize,
            rx: rx as usize,
            used: 0,
        })
    }
}

/// A host-callable address that runs guest function `target` on the calling thread's guest thread pointer (attaching the thread first if needed), every argument as the caller passed it.
pub fn down(target: usize, kind: Down) -> Result<usize, String> {
    let mut pool = POOL.lock().map_err(|_| "down stubs: poisoned".to_owned())?;
    if pool.as_ref().is_none_or(|p| p.used + DOWN_STUB > CHUNK) {
        // the full chunk stays mapped: its stubs are in use
        *pool = Some(new_chunk()?);
    }
    let Some(p) = pool.as_mut() else {
        return Err("down stubs: no pool".into());
    };
    let bytes = down_stub(target, kind);
    // SAFETY: `used + DOWN_STUB <= CHUNK` bytes into the RW view; the RX view shows the same page.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), (p.rw + p.used) as *mut u8, DOWN_STUB) };
    let at = p.rx + p.used;
    p.used += DOWN_STUB;
    Ok(at)
}

/// x86_64 `movabs $imm64, %r11` opcode bytes (REX.W+B, B8+r): the NBS x86_64 interceptable stub starts with it, then `jmp *%r11`.
const STUB_PREFIX: [u8; 2] = [0x49, 0xbb];
/// x86_64 `movabs $imm64, %r11; jmp *%r11`: 13 bytes.
const JUMP_LEN: usize = 13;

/// Whether `addr` holds an unbound interceptable stub: the `movabs $0, %r11` prefix.
pub fn is_stub(addr: usize) -> bool {
    // SAFETY: `addr` is a guest libc text address the guest itself passed; its first two bytes are readable.
    unsafe { std::slice::from_raw_parts(addr as *const u8, 2) == STUB_PREFIX }
}

/// Points the stub at `addr` at `thunk` (from `bind`): rewrites its `movabs` immediate. The guest text is sealed, so the write goes through `image::write_sealed`. False when `addr` is no stub.
pub fn patch_site(addr: usize, thunk: usize) -> bool {
    if !is_stub(addr) {
        return false;
    }
    super::image::write_sealed(addr + STUB_PREFIX.len(), &thunk.to_ne_bytes());
    true
}

/// Rewrites the NBS linker hook at `at` (a naked `ud2`, then int3 padding up to the next 16-byte function) to jump to a thunk of `host`; before `seal`.
pub fn patch_hook(at: usize, host: usize) -> Result<(), &'static str> {
    // SAFETY: the hook's bytes in our still-writable mapping of the linker's text.
    let body = unsafe { std::slice::from_raw_parts_mut(at as *mut u8, JUMP_LEN) };
    // ud2, then alignment padding (int3) up to the next 16-byte function: room for the jump
    if body[..2] != [0x0f, 0x0b] || body[2..].iter().any(|&b| b != 0xcc) {
        return Err("not the 16-byte ud2 hook this engine rewrites");
    }
    let thunk = bind(host).ok_or("out of thunks")?;
    body[..2].copy_from_slice(&STUB_PREFIX);
    body[2..10].copy_from_slice(&thunk.to_ne_bytes());
    body[10..].copy_from_slice(&[0x41, 0xff, 0xe3]);
    Ok(())
}

/// Points the NB vdso entry whose `st_value` lies at `value_at` (the mapped copy) at a thunk of `host`: the value is the thunk minus the image bias, so the loader's `bias + value` lands on it. Before `seal`.
pub fn patch_entry(vdso: &Image, value_at: usize, host: usize) -> Result<(), &'static str> {
    let thunk = bind(host).ok_or("out of thunks")?;
    // SAFETY: the st_value field of a .dynsym entry in our still-writable mapping of the vdso.
    unsafe { std::ptr::write_unaligned(value_at as *mut usize, thunk.wrapping_sub(vdso.bias)) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::maps_line_covers;

    /// proc(5): the range is `start-end`, end exclusive; a line without one covers nothing.
    #[test]
    fn maps_range_is_half_open() {
        let line = b"7f00-7f10 r-xp 00000000 08:30 1 /lib/x.so\n";
        assert!(maps_line_covers(line, 0x7f00));
        assert!(maps_line_covers(line, 0x7f0f));
        assert!(!maps_line_covers(line, 0x7f10));
        assert!(!maps_line_covers(b"not a mapping\n", 0x7f00));
    }

    /// The guard in `Identity::call`: a guest call with no TCB panics before any guest code runs.
    #[test]
    #[should_panic(expected = "no guest thread pointer")]
    fn guest_call_without_tcb_is_refused() {
        use super::Identity;
        use crate::engine::{Engine, Tp};
        let ht = super::HostThread::leak();
        // SAFETY: the assert fires before the crossing; `ht` is leaked.
        unsafe { Identity.call(ht, Tp::Guest(0), 0, &[0; 4]) };
    }
}
