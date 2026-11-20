// SPDX-License-Identifier: MIT
//! The per-thread crossing record both engines run guest code on (`Engine::Cpu`): the guest thread pointer a guest call installs and stores back, and the stack the thread's guest code runs on.
//! The shadow stack, host thread pointer and exit frame belong to the identity crossing, whose asm addresses them by fixed offsets (asserted below); QEMU keeps its thread pointer in the engine CPU state and leaves them unused.

/// Guest->host frames one thread can nest (JNI down -> up -> Java -> down chains); overflow traps instead of writing past the record.
pub(in crate::engine) const SHADOW_FRAMES: usize = 64;
/// Words per frame, both directions: return address, guest thread pointer (up) / guest target (down), host target (up) / unused (down), the caller's callee-saved scratch register.
pub(in crate::engine) const FRAME_WORDS: usize = 4;

/// Per-thread crossing record, addressed through a register by the identity asm; field offsets are fixed by it (asserted).
#[repr(C)]
pub struct HostThread {
    pub(in crate::engine) shadow_sp: *mut usize,
    host_tp: usize,
    guest_tp: usize,
    pub(in crate::engine) shadow_end: *mut usize,
    /// The innermost running identity guest call's saved-register frame (its sp after the saves), where its end-call resumes; the outer call's is kept in that frame. 0 outside any call.
    pub(in crate::engine) exit_sp: usize,
    pub(in crate::engine) shadow: [usize; SHADOW_FRAMES * FRAME_WORDS],
    /// The stack this thread's guest code runs on (`Engine::attach`'s answer); the guest's `pthread_getattr_np` reports it.
    stack: super::Stack,
    /// This thread's guest `JNIEnv` (jni.rs `guest_env`), made on first use. Owned here, not by a `thread_local!`: glibc and bionic run thread_local destructors before pthread key destructors, and guest key destructors run by `detach` may call JNI (ART's `Thread::ThreadExitCallback` allows it), so the env must live until the record is freed. Guest-addressable (`Placed`), given back with the record.
    pub(in crate::engine) env: Option<super::objects::Placed>,
}

const _: () = {
    assert!(std::mem::offset_of!(HostThread, shadow_sp) == 0);
    assert!(std::mem::offset_of!(HostThread, host_tp) == 8);
    assert!(std::mem::offset_of!(HostThread, guest_tp) == 16);
    assert!(std::mem::offset_of!(HostThread, shadow_end) == 24);
    assert!(std::mem::offset_of!(HostThread, exit_sp) == 32);
};

impl HostThread {
    /// A record for the calling thread; leaked: its guest TCB keeps pointing at it.
    pub fn leak() -> &'static mut HostThread {
        let t = Box::leak(Box::new(HostThread {
            shadow_sp: std::ptr::null_mut(),
            host_tp: 0,
            guest_tp: 0,
            shadow_end: std::ptr::null_mut(),
            exit_sp: 0,
            shadow: [0; SHADOW_FRAMES * FRAME_WORDS],
            stack: super::Stack::default(),
            env: None,
        }));
        let range = t.shadow.as_mut_ptr_range();
        t.shadow_sp = range.start;
        t.shadow_end = range.end;
        t
    }

    /// The guest thread pointer the last crossing back from the guest saw (0 before the first).
    pub fn guest_tp(&self) -> usize {
        self.guest_tp
    }

    /// Sets the guest thread pointer the next crossing into the guest installs (a guest TCB; attach makes one per thread).
    pub fn set_guest_tp(&mut self, v: usize) {
        self.guest_tp = v;
    }

    /// The stack this thread's guest code runs on.
    pub fn stack(&self) -> super::Stack {
        self.stack
    }

    pub fn set_stack(&mut self, s: super::Stack) {
        self.stack = s;
    }

    /// The host thread pointer the last identity crossing into the guest saved.
    pub fn host_tp(&self) -> usize {
        self.host_tp
    }
}
