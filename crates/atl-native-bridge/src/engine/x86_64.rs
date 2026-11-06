// SPDX-License-Identifier: MIT
//! x86_64 crossing. Thread pointer = FS base for glibc and bionic alike; FSGSBASE instructions (Linux 5.9+, `HWCAP2_FSGSBASE`) switch it without a syscall.
//! GS base = this thread's `HostThread` while guest code runs: neither glibc nor bionic uses GS on x86_64, so a guest->host thunk finds the host FS without reading guest TLS.
//! Not the guest TCB's `NATIVE_BRIDGE_GUEST_STATE` slot (bionic tls_defines.h, x86_64 slot 10): that TCB does not exist yet while `__linker_init` runs, and the guest libc's constructors already call the host from inside it (vdso `native_bridge_intercept_symbol`).
//! Guest->host keeps the guest return address and guest FS on the `HostThread` shadow stack, not the machine stack: stack arguments and `%al` (variadic vector count) reach the host function untouched, so one body serves every signature (bridge1-step4-plan section 3).

use std::sync::atomic::{AtomicUsize, Ordering};

/// Guest->host frames one thread can nest (JNI down -> up -> Java -> down chains); overflow traps (`ud2`) instead of writing past the record.
const SHADOW_FRAMES: usize = 64;
/// Words per guest->host frame: guest return address, guest FS, host target.
const FRAME_WORDS: usize = 3;

/// Per-thread crossing record, addressed through GS by the asm below; field offsets are fixed by it (asserted).
#[repr(C)]
pub struct HostThread {
    shadow_sp: *mut usize,
    host_fs: usize,
    guest_fs: usize,
    shadow_end: *mut usize,
    /// RECORD_MAGIC: GS holds ART's `Thread*` on every ART thread (art thread_x86_64.cc `arch_prctl(ARCH_SET_GS)`), so the signal entry trusts GS as a record only when this word matches.
    magic: usize,
    shadow: [usize; SHADOW_FRAMES * FRAME_WORDS],
}

/// `HostThread::magic`, as hex digits: the asm compares it as an immediate.
macro_rules! record_magic {
    () => {
        "41544c4e42524543"
    };
}
const RECORD_MAGIC: usize = match usize::from_str_radix(record_magic!(), 16) {
    Ok(v) => v,
    Err(_) => panic!("record_magic! is not hex"),
};

const _: () = {
    assert!(std::mem::offset_of!(HostThread, shadow_sp) == 0);
    assert!(std::mem::offset_of!(HostThread, host_fs) == 8);
    assert!(std::mem::offset_of!(HostThread, guest_fs) == 16);
    assert!(std::mem::offset_of!(HostThread, shadow_end) == 24);
    assert!(std::mem::offset_of!(HostThread, magic) == 32);
};

impl HostThread {
    /// A record for the calling thread; leaked: guest TLS and the GS base keep pointing at it.
    pub fn leak() -> &'static mut HostThread {
        let t = Box::leak(Box::new(HostThread {
            shadow_sp: std::ptr::null_mut(),
            host_fs: 0,
            guest_fs: 0,
            shadow_end: std::ptr::null_mut(),
            magic: RECORD_MAGIC,
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

/// The x86-64 TCB word the ABI fixes: `%fs:0` holds the thread pointer itself (TLS variant II; bionic `TLS_SLOT_SELF` = 0, `__init_tcb`), so `mov %fs:0` yields the TCB address.
pub fn init_tcb(tp: usize) {
    // SAFETY: `tp` is the start of a guest TCB inside a live static-TLS block (thread::alloc_static_tls).
    unsafe { (tp as *mut usize).write(tp) };
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
/// Thunks: the guest libc's interceptable stubs (94, NBS `stubs_x86_64.cpp`) + the 4 vdso entries, rounded up to a power of two for libm/liblog stubs.
pub const THUNKS: usize = 128;
/// Bytes per thunk: `call rel32` (5) padded to 8 (`.p2align 3` below).
const THUNK_BYTES: usize = 8;

std::arch::global_asm!(
    concat!(
        // atl_nb_call_guest(ht, f, a, b, c) -> f(a, b, c) on the guest thread pointer
        // hidden: rip-relative references must not be preemptible in the cdylib
        ".hidden atl_nb_host_targets, atl_nb_host_thunks, atl_nb_call_guest, atl_nb_cross_to_host, atl_nb_jni_up, atl_nb_jni_stubs, atl_nb_up, atl_nb_cross_to_guest, atl_nb_cross_to_guest_env, atl_nb_cross_to_guest_vm, atl_nb_attach_fn, atl_nb_signal_entry, atl_nb_signal_handlers\n",
        ".text\n",
        ".globl atl_nb_call_guest\n",
        ".type atl_nb_call_guest,@function\n",
        "atl_nb_call_guest:\n",
        "    push %rbx\n",
        "    push %r12\n",
        "    sub $8, %rsp\n",
        "    mov %rdi, %rbx\n",
        "    rdgsbase %r12\n",
        "    wrgsbase %rbx\n",
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
        "    call *%rax\n",
        "    rdfsbase %rcx\n",
        "    mov %rcx, 16(%rbx)\n",
        restore_host_fs!(),
        "    wrgsbase %r12\n",
        "    add $8, %rsp\n",
        "    pop %r12\n",
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
        // the up crossing proper: %r11 = host target, [rsp] = guest return
        ".type atl_nb_up,@function\n",
        "atl_nb_up:\n",
        "    mov %gs:0, %r10\n",
        "    cmp %gs:24, %r10\n",
        "    jae 9f\n",
        "    mov %r11, 16(%r10)\n",
        "    pop 0(%r10)\n",
        "    rdfsbase %r11\n",
        "    mov %r11, 8(%r10)\n",
        "    add $24, %r10\n",
        "    mov %r10, %gs:0\n",
        "    mov %gs:8, %r11\n",
        "    wrfsbase %r11\n",
        "    call *-8(%r10)\n",
        "    mov %gs:0, %r10\n",
        "    sub $24, %r10\n",
        "    mov %r10, %gs:0\n",
        "    mov 8(%r10), %r11\n",
        "    wrfsbase %r11\n",
        "    push 0(%r10)\n",
        "    ret\n",
        "9:  ud2\n",
        ".size atl_nb_up, .-atl_nb_up\n",
        // host->guest (down call): entered by `jmp` from a down stub with %r10 = guest target; [rsp] = host return, then the caller's stack args.
        // Same shadow-stack frame as the up direction (3 words): host return, guest target (while calling), the GS the caller had (0 outermost, the record when nested inside an up call).
        // Argument registers, %al and stack args reach the guest untouched; only %r10/%r11 (and %rcx after the call, not a return register) are scratch.
        // Three entries, by what the first argument is (`Down`): none to rewrite, a host JNIEnv, a host JavaVM; the kind rides in the caller's-GS slot until attach has read it.
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
        "    rdgsbase %rax\n",
        "    mov %rax, 192(%rsp)\n",
        "    add $128, %rsp\n",
        "    pop %rax\n",
        "    pop %r9\n",
        "    pop %r8\n",
        "    pop %rcx\n",
        "    pop %rdx\n",
        "    pop %rsi\n",
        "    pop %rdi\n",
        "    pop %r10\n",
        // [rsp] = the caller's GS; GS = the record from here on
        "    wrgsbase %r11\n",
        "    mov %gs:0, %r11\n",
        "    cmp %gs:24, %r11\n",
        "    jae 9f\n",
        "    pop 16(%r11)\n",
        "    pop 0(%r11)\n",
        "    mov %r10, 8(%r11)\n",
        "    rdfsbase %r10\n",
        "    mov %r10, %gs:8\n",
        "    add $24, %r11\n",
        "    mov %r11, %gs:0\n",
        "    mov %gs:16, %r10\n",
        "    wrfsbase %r10\n",
        "    call *-16(%r11)\n",
        "    rdfsbase %r10\n",
        "    mov %r10, %gs:16\n",
        "    mov %gs:0, %r11\n",
        "    sub $24, %r11\n",
        "    mov %r11, %gs:0\n",
        "    mov %gs:8, %r10\n",
        "    wrfsbase %r10\n",
        "    push 0(%r11)\n",
        "    mov 16(%r11), %r10\n",
        "    wrgsbase %r10\n",
        "    ret\n",
        "9:  ud2\n",
        ".size atl_nb_cross_to_guest, .-atl_nb_cross_to_guest\n",
        // signal entry: the kernel delivers on whatever FS the thread had (guest or host) and sigreturn does not restore FS base;
        // GS != 0 = a crossing is active on this thread -> run the host handler on the record's host FS, put the interrupted FS back after
        ".globl atl_nb_signal_entry\n",
        ".type atl_nb_signal_entry,@function\n",
        "atl_nb_signal_entry:\n",
        "    push %rbx\n",
        "    push %r12\n",
        "    sub $8, %rsp\n",
        "    rdfsbase %rbx\n",
        "    rdgsbase %r12\n",
        "    test %r12, %r12\n",
        "    jz 1f\n",
        concat!("    movabs $0x", record_magic!(), ", %r11\n"),
        "    cmp %r11, 32(%r12)\n",
        "    jne 1f\n",
        "    mov 8(%r12), %r11\n",
        "    test %r11, %r11\n",
        "    jz 1f\n",
        "    wrfsbase %r11\n",
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
        ".rept 128\n",
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
    options(att_syntax)
);

// the `.rept` counts above are THUNKS and JNI_STUBS; the index arithmetic in atl_nb_cross_to_host / atl_nb_jni_up is THUNK_BYTES = one table slot
const _: () =
    assert!(THUNKS == 128 && JNI_STUBS == 256 && THUNK_BYTES == 8 && FRAME_WORDS * 8 == 24);

/// JNI stubs: one per slot of the largest JNI table (`JNINativeInterface`: 233 in jni.h today; jni.rs asserts the fit).
pub const JNI_STUBS: usize = 256;

/// The guest-callable address of JNI table slot `slot` (jni.rs): crosses to the host object's own slot with the object rewritten.
pub fn jni_stub(slot: usize) -> usize {
    assert!(slot < JNI_STUBS, "JNI slot {slot} past the stub array");
    (&raw const atl_nb_jni_stubs) as usize + slot * THUNK_BYTES
}

unsafe extern "C" {
    fn atl_nb_call_guest(ht: *mut HostThread, f: usize, a: usize, b: usize, c: usize) -> usize;
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

/// Calls guest function `f(a, b, c)` on the guest thread pointer (none before the first crossing: the guest linker installs its own).
///
/// # Safety
/// `f` must be guest code taking up to three integer arguments, `ht` this thread's record.
pub unsafe fn call_guest(ht: &mut HostThread, f: usize, a: usize, b: usize, c: usize) -> usize {
    // SAFETY: caller contract; the asm keeps callee-saved registers and restores GS/FS.
    unsafe { atl_nb_call_guest(ht, f, a, b, c) }
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
pub const STUB_PREFIX: [u8; 2] = [0x49, 0xbb];
