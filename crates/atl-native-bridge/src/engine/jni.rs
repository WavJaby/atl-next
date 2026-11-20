// SPDX-License-Identifier: MIT
//! The guest's `JNIEnv` / `JavaVM` (bridge1-step4-plan section 3, identity ABI): an object whose word 0 is a guest function table and word 1 the host object it stands for (`GuestObj`).
//! Two rows of the proxy object tables (objects.rs): slots from jni.h; each is an object stub that crosses to the host object's own slot with only the object argument rewritten, so variadic entries need no per-signature code. Under QEMU a `va_list` entry crosses to its `...A` sibling instead, its arguments read by the method's shorty (dispatch.rs `call_object`, va_list.rs).
//! Specials, whose results hand an env/vm back to the guest or whose arguments carry guest-layout structs, are host functions instead (the set Berberis `jni/jni_trampolines.cc` special-cases): `GetJavaVM`, `GetEnv`, `AttachCurrentThread`, `AttachCurrentThreadAsDaemon` (out-parameters written as a guest word, `JavaVMAttachArgs` read in guest layout), `RegisterNatives` (`JNINativeMethod[]` read in guest layout; ART still asks `isNativeBridgeFunctionPointer` of each `fnPtr`). Guest layouts are read by the guest's word (thread.rs `read_word`), one path for both widths.
//! Down calls rewrite the other way (arch `Down`): host env -> this thread's guest env, host vm -> the guest vm.
//! A 32-bit guest holds ART's references as they are, as a 32-bit ART hands them out, so ART stays the only owner of their identity and lifetime: globals are table indices, locals entries of tables ART maps below 4 GiB (patches/aosp/art 0006), method and field ids opaque indices (`-Xopaque-jni-ids:true`, atl-app-launcher); the one exception, a native method's reference arguments in its managed frame, is [`local_ref`]'d by the down call (dispatch.rs `local_refs`).

use std::sync::OnceLock;

use super::objects::{self, ENV as ENV_TABLE, Placed, VM as VM_TABLE, env, vm};
use super::thread::write_word;
use objects::host_slot;

/// jni.h `JNI_OK`.
const JNI_OK: i32 = 0;

/// Deliberate break `jni_swap` (dispatch.rs `call_object`): the one entry whose variadic lanes it exchanges. Other entries keep their order: GetStaticMethodID's x3 is its signature.
#[cfg(atl_nb_jni_swap)]
pub(super) const CALL_STATIC_INT_METHOD: usize = env::CallStaticIntMethod;

static VM: OnceLock<usize> = OnceLock::new();

/// The guest env standing for host env `host` on this thread (one per thread, as ART's, which keeps it in its `Thread`): held by the thread's crossing record (record.rs `HostThread::env`), so it stays valid in pthread key destructors and is freed with the record at `detach`.
pub fn guest_env(host: usize) -> usize {
    if host == 0 {
        return 0;
    }
    let ht = super::thread::attach_current();
    // SAFETY: this thread's record, leaked until its detach; no other borrow of it is live here.
    let slot = unsafe { &mut (*ht).env };
    let obj = slot.get_or_insert_with(|| {
        let functions = objects::functions(
            ENV_TABLE,
            &[
                (env::GetJavaVM, get_java_vm as *const () as usize),
                (env::RegisterNatives, register_natives as *const () as usize),
            ],
            host,
        );
        Placed::new(functions, host)
    });
    obj.set_host(host);
    obj.addr()
}

/// A local reference standing for host reference `obj` in env `host`'s current native frame (jni.h `NewLocalRef`): ART pops it with the frame, so it lives as long as the transition reference it replaces. 0 for 0.
///
/// # Safety
/// `host` is this thread's host JNIEnv inside a native method; `obj` a reference ART handed that method.
pub(super) unsafe fn local_ref(host: usize, obj: usize) -> usize {
    // SAFETY: caller contract; jni.h's NewLocalRef.
    unsafe {
        let f: extern "C" fn(usize, usize) -> usize =
            std::mem::transmute(host_slot(host, env::NewLocalRef));
        f(host, obj)
    }
}

/// The guest vm standing for host vm `host` (ART has one per process).
pub fn guest_vm(host: usize) -> usize {
    if host == 0 {
        return 0;
    }
    let p = *VM.get_or_init(|| {
        let functions = objects::functions(
            VM_TABLE,
            &[
                (vm::GetEnv, get_env as *const () as usize),
                (
                    vm::AttachCurrentThread,
                    attach_current_thread as *const () as usize,
                ),
                (
                    vm::AttachCurrentThreadAsDaemon,
                    attach_current_thread_as_daemon as *const () as usize,
                ),
            ],
            host,
        );
        Placed::new(functions, host).leak()
    });
    // SAFETY: leaked above for the process's life.
    if unsafe { objects::host_of(p) } != host {
        super::unsupported("a second JavaVM");
    }
    p
}

use objects::host_of;

/// ART's `NativeBridgeRuntimeCallbacks::getMethodShorty(env, mid)` (native_bridge.h): a method's shorty, result first.
pub type MethodShorty = unsafe extern "C" fn(usize, usize) -> *const std::ffi::c_char;
static METHOD_SHORTY: OnceLock<MethodShorty> = OnceLock::new();

/// Keeps the runtime's shorty callback (exports.rs `initialize`): a guest `...V` call reads its `va_list` by the method's shorty (dispatch.rs `call_object`). The first one set stays.
pub fn set_method_shorty(f: MethodShorty) {
    let _ = METHOD_SHORTY.set(f);
}

/// The parameter characters of method `mid`'s shorty (its result character dropped), asked of the runtime on host env `host`; `None` when no callback was set or it answered null.
///
/// # Safety
/// `host` is a host JNIEnv and `mid` a jmethodID of it; the shorty the runtime returns lives as long as the method.
pub(super) unsafe fn method_params(host: usize, mid: usize) -> Option<&'static [u8]> {
    let f = METHOD_SHORTY.get()?;
    // SAFETY: caller contract; native_bridge.h's getMethodShorty.
    let p = unsafe { f(host, mid) };
    // SAFETY: a non-null shorty is NUL-terminated and outlives the method (caller contract).
    let s = (!p.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(p) })?;
    s.to_bytes().get(1..)
}

/// The `...A` sibling of `...V` slot `id`: jni.h declares each `XV` directly before `XA` (unit test `a_slot_follows_each_v_slot`).
pub(super) const fn a_slot(id: usize) -> usize {
    id + 1
}

/// The host env behind guest env `guest` (0 = 0), for the NDK shims (ndk.rs): an NDK function takes a `JNIEnv*` and calls the host library through it.
/// Deliberate break `ATL_NB_BREAK=ndk_env`: the guest env itself, whose function table is the guest's.
///
/// # Safety
/// `guest` is 0 or a guest env made by `guest_env`.
#[cfg(not(atl_nb_ndk_env))]
pub(super) unsafe fn host_env(guest: usize) -> usize {
    if guest == 0 {
        return 0;
    }
    // SAFETY: caller contract.
    unsafe { host_of(guest) }
}

/// # Safety
/// As the non-break `host_env`; ignores its argument.
#[cfg(atl_nb_ndk_env)]
pub(super) unsafe fn host_env(guest: usize) -> usize {
    guest
}

/// The guest's word in bytes: 4 or 8.
fn guest_word() -> usize {
    super::active().guest_pointer_bits() as usize / 8
}

/// `n` consecutive guest words of `word` bytes at guest address `p`, zero-extended: the pointer fields of a guest-layout struct (jni.h `JNINativeMethod` = 3 pointers; `JavaVMAttachArgs` = `jint` then 2 pointers, the first at offset `word` on ILP32 and LP64 alike).
///
/// # Safety
/// `p` is readable for `n * word` bytes.
unsafe fn guest_words(p: usize, n: usize, word: usize) -> Vec<usize> {
    (0..n)
        .map(|i| {
            let at = (p + i * word) as *const u8;
            // SAFETY: caller contract.
            unsafe {
                if word == 4 {
                    at.cast::<u32>().read_unaligned() as usize
                } else {
                    at.cast::<u64>().read_unaligned() as usize
                }
            }
        })
        .collect()
}

/// `jint GetJavaVM(JNIEnv*, JavaVM**)`, on the host thread pointer (a bound thunk). The answer is written as a guest word.
extern "C" fn get_java_vm(env: usize, out: usize) -> i32 {
    // SAFETY: the guest's own env and out-parameter, as jni.h's GetJavaVM takes them.
    unsafe {
        let host = host_of(env);
        let f: extern "C" fn(usize, *mut usize) -> i32 =
            std::mem::transmute(host_slot(host, env::GetJavaVM));
        let mut vm = 0;
        let rc = f(host, &mut vm);
        if rc == JNI_OK {
            write_word(out as *mut u8, guest_vm(vm));
        }
        rc
    }
}

/// `jint GetEnv(JavaVM*, void**, jint)`.
extern "C" fn get_env(vm: usize, out: usize, version: i32) -> i32 {
    // SAFETY: as jni.h's GetEnv.
    unsafe {
        let host = host_of(vm);
        let f: extern "C" fn(usize, *mut usize, i32) -> i32 =
            std::mem::transmute(host_slot(host, vm::GetEnv));
        let mut env = 0;
        let rc = f(host, &mut env, version);
        if rc == JNI_OK {
            write_word(out as *mut u8, guest_env(env));
        }
        rc
    }
}

/// jni.h `JavaVMAttachArgs`, host layout.
#[repr(C)]
struct AttachArgs {
    version: i32,
    name: usize,
    group: usize,
}

/// `AttachCurrentThread(AsDaemon)(JavaVM*, JNIEnv**, void*)`: the host attach with the guest's `JavaVMAttachArgs` (or NULL) in host layout, then this thread's guest env.
///
/// # Safety
/// As jni.h's AttachCurrentThread.
unsafe fn attach(vm: usize, out: usize, args: usize, slot: usize) -> i32 {
    // SAFETY: caller contract.
    unsafe {
        let host = host_of(vm);
        let f: extern "C" fn(usize, *mut usize, *const AttachArgs) -> i32 =
            std::mem::transmute(host_slot(host, slot));
        let word = guest_word();
        let host_args = (args != 0).then(|| {
            let w = guest_words(args + word, 2, word);
            AttachArgs {
                version: (args as *const i32).read_unaligned(),
                name: w[0],
                group: w[1],
            }
        });
        let mut env = 0;
        let rc = f(
            host,
            &mut env,
            host_args
                .as_ref()
                .map_or(std::ptr::null(), |a| a as *const _),
        );
        if rc == JNI_OK {
            write_word(out as *mut u8, guest_env(env));
        }
        rc
    }
}

extern "C" fn attach_current_thread(vm: usize, out: usize, args: usize) -> i32 {
    // SAFETY: the guest's arguments to jni.h's AttachCurrentThread.
    unsafe { attach(vm, out, args, vm::AttachCurrentThread) }
}

extern "C" fn attach_current_thread_as_daemon(vm: usize, out: usize, args: usize) -> i32 {
    // SAFETY: as above.
    unsafe { attach(vm, out, args, vm::AttachCurrentThreadAsDaemon) }
}

/// `jint RegisterNatives(JNIEnv*, jclass, const JNINativeMethod*, jint)`: the guest's methods array (3 guest words each: name, signature, fnPtr) in host layout, then the host's RegisterNatives, which asks `isNativeBridgeFunctionPointer` of each `fnPtr` (a guest address) and registers its trampoline. A negative count is ART's to refuse.
extern "C" fn register_natives(env: usize, class: usize, methods: usize, n: i32) -> i32 {
    // SAFETY: the guest's arguments to jni.h's RegisterNatives: `methods` holds `n` entries (JNI contract).
    unsafe {
        let host = host_of(env);
        let f: extern "C" fn(usize, usize, *const usize, i32) -> i32 =
            std::mem::transmute(host_slot(host, env::RegisterNatives));
        let count = match usize::try_from(n) {
            Ok(count) if methods != 0 => count,
            // ART answers a NULL array and a negative count itself
            _ => return f(host, class, std::ptr::null(), n),
        };
        let table = guest_words(methods, 3 * count, guest_word());
        f(host, class, table.as_ptr(), n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `va_list` slot `XV` has `XA` at `a_slot`, with the same parameters up to the `va_list` (a `jvalue*` there) and the same result. jni.h has 31 V slots (10 types x Call, CallNonvirtual, CallStatic, plus NewObject).
    #[test]
    fn a_slot_follows_each_v_slot() {
        let mut n = 0;
        for (id, lane) in objects::LANES[ENV_TABLE].iter().enumerate() {
            let Some(v) = lane else { continue };
            let Some(k) = v.params.iter().position(|c| *c == objects::Class::VaList) else {
                continue;
            };
            n += 1;
            let a = objects::LANES[ENV_TABLE][a_slot(id)].expect("an A slot");
            let stem = v.name.strip_suffix('V').expect("a V name");
            assert_eq!(a.name.strip_suffix('A'), Some(stem), "{}", v.name);
            assert_eq!(a.params.len(), k + 1, "{}", v.name);
            assert_eq!(a.params[..k], v.params[..k], "{}", v.name);
            assert_eq!(a.ret, v.ret, "{}", v.name);
        }
        assert_eq!(n, 31);
    }

    /// jni.h `JNINativeMethod` is three pointers: 12 bytes per entry on ILP32 (armeabi-v7a), 24 on LP64. Two ILP32 entries laid out by hand, as the guest's compiler lays them, read back as six host words; the same bytes read as LP64 would pair words up (the red control).
    #[test]
    fn native_methods_are_read_in_the_guest_layout() {
        let words: [u32; 6] = [0x1000, 0x1010, 0x8001, 0x2000, 0x2010, 0x8101];
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(bytes.len(), 2 * 12);
        // SAFETY: `bytes` holds 6 four-byte words.
        let got = unsafe { guest_words(bytes.as_ptr() as usize, 6, 4) };
        assert_eq!(got, words.map(|w| w as usize));
        // SAFETY: 3 eight-byte words = the same 24 bytes.
        let lp64 = unsafe { guest_words(bytes.as_ptr() as usize, 3, 8) };
        assert_eq!(lp64[0], 0x1010_0000_1000);
    }

    /// jni.h `JavaVMAttachArgs` {jint version; const char* name; jobject group}: on ILP32 name at 4 and group at 8; on LP64 name at 8 (pointer alignment) and group at 16 -- the first pointer at offset `word` for both, as `attach` reads it.
    #[test]
    fn attach_args_pointers_follow_the_version_by_one_word() {
        let mut ilp32 = [0u8; 12];
        ilp32[0..4].copy_from_slice(&0x0001_0006i32.to_le_bytes());
        ilp32[4..8].copy_from_slice(&0x4000u32.to_le_bytes());
        ilp32[8..12].copy_from_slice(&0x5002u32.to_le_bytes());
        // SAFETY: 2 words from offset 4 of 12 bytes.
        let w = unsafe { guest_words(ilp32.as_ptr() as usize + 4, 2, 4) };
        assert_eq!(w, [0x4000, 0x5002]);
        assert_eq!(std::mem::offset_of!(AttachArgs, name), 8);
        assert_eq!(std::mem::offset_of!(AttachArgs, group), 16);
    }
}
