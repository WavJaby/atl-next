// SPDX-License-Identifier: MIT
//! Q6-2 gate (QEMU bridge plan Q6-2), harness-less: guest JNI calls under QEMU, integer lanes only. The real arm64 guest linker starts the main executable (`xtask/testdata/qemu-jni/fixture.c`, PIE on libc.so); the host calls its `qj_run` with the guest env of a fake host JNIEnv (this file), and each JNI call the fixture makes crosses to the fake.
//! The fake records what each call got and answers with spec values: GetVersion gives JNI_VERSION_1_6 (jni.h), CallStaticIntMethod(a, b) gives a * 10 + b, so a swapped pair shows as 32, not 23. The inputs are the fixture's ADD_A and ADD_B.
//! Env (xtask qemu_jni.rs sets it; unset prints SKIP): ATL_ENGINE_SO (the engine `.so`), ATL_NB_BOOT (testdata/qemu-linker/boot.c, static), ATL_NB_JNIEXE (fixture.c, PIE), ATL_NB_LINKER, ATL_NB_VDSO (the arm64 NB images; `<nb>/libnative_bridge_guest_libc` among the guest library directories).
//! Checks: (s) started; (v) GetVersion returned JNI_VERSION_1_6; (e) all 8 calls reached the fake, each with the host env it was given; (c) FindClass and GetStaticMethodID got the names and the handles they issued, CallStaticIntMethod got (cls, mid, 2, 3), and the fixture got 23; (j) GetJavaVM, a special with its own host thunk, returned the guest vm of the fake vm; (t) NewStringUTF and GetStringUTFChars round-tripped "abc".
//! Output: `qemu-jni: (x) ok` per check, `(x) FAIL <why>` otherwise, exit 0 when all pass.
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]
#![allow(
    unsafe_code,
    reason = "a fake JNIEnv and JavaVM in the C ABI take and answer raw JNI pointers; calls the guest through Guest::call"
)]

use std::ffi::{CStr, c_char};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use atl_native_bridge::engine::jni::{guest_env, guest_vm};
use atl_native_bridge::engine::{Config, Guest, qemu};
use object::{Object, ObjectSymbol, read::elf::ElfFile64};

/// The fixture function the host calls.
const SYMBOL: &str = "qj_run";
/// fixture.c `ADD_A` and `ADD_B`: the arguments of CallStaticIntMethod.
const ADD_A: i32 = 2;
const ADD_B: i32 = 3;
/// The fake's answer for those arguments: a * 10 + b.
const ADD_EXPECT: i64 = (ADD_A * 10 + ADD_B) as i64;
/// jni.h `JNI_VERSION_1_6`: the fake's GetVersion answer.
const JNI_VERSION_1_6: i32 = 0x0001_0006;
/// jni.h slots of the entries the fixture calls (`JNINativeInterface`: reserved0..reserved3, then GetVersion at 4), and the table sizes (233 env entries, 8 vm entries).
const GET_VERSION: usize = 4;
const FIND_CLASS: usize = 6;
const GET_STATIC_METHOD_ID: usize = 113;
const CALL_STATIC_INT_METHOD: usize = 129;
const NEW_STRING_UTF: usize = 167;
const GET_STRING_UTF_CHARS: usize = 169;
const RELEASE_STRING_UTF_CHARS: usize = 170;
const GET_JAVA_VM: usize = 219;
const ENV_SLOTS: usize = 233;
const VM_SLOTS: usize = 8;
/// The JNI calls fixture.c makes: GetVersion, FindClass, GetStaticMethodID, CallStaticIntMethod, GetJavaVM, NewStringUTF, GetStringUTFChars, ReleaseStringUTFChars.
const CALLS: usize = 8;
/// The words fixture.c writes to `out`.
const OUT_WORDS: usize = 5;

/// The fake's handles and the string it hands out (NUL-terminated, so `GetStringUTFChars` returns its address).
const CLASS: usize = 0x0c_1a55;
const METHOD: usize = 0x3e7d;
const STRING: usize = 0x5712;
static STR_CHARS: &[u8] = b"abc\0";

/// Calls the fake got (every one must be CALLS), and those whose env was not the host env the fake gave.
static CALLS_SEEN: AtomicUsize = AtomicUsize::new(0);
static ENV_BAD: AtomicUsize = AtomicUsize::new(0);
/// Lookups with a name, signature or handle the fixture did not pass; CallStaticIntMethod with other arguments; string calls with another value.
static CLASS_BAD: AtomicUsize = AtomicUsize::new(0);
static CALL_BAD: AtomicUsize = AtomicUsize::new(0);
static STR_BAD: AtomicUsize = AtomicUsize::new(0);

/// The fake host objects: a JNIEnv and a JavaVM, each the address of a word holding its function table (jni.h's object layout). Leaked: the process's life.
struct Fake {
    env: usize,
    vm: usize,
}
static FAKE: OnceLock<Fake> = OnceLock::new();

/// What the checks compare against: the executable's entry and `qj_run`'s address, both from its ELF file (the load bias is their difference).
struct Expected {
    e_entry: u64,
    run: u64,
}
static EXPECTED: OnceLock<Expected> = OnceLock::new();

fn check(letter: char, result: Result<(), String>) -> bool {
    match result {
        Ok(()) => {
            println!("qemu-jni: ({letter}) ok");
            true
        }
        Err(why) => {
            println!("qemu-jni: ({letter}) FAIL {why}");
            false
        }
    }
}

fn expected(exe: &Path) -> Result<Expected, String> {
    let bytes = std::fs::read(exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let elf = ElfFile64::<object::Endianness>::parse(&*bytes).map_err(|e| e.to_string())?;
    let sym = elf
        .symbols()
        .find(|s| s.name().is_ok_and(|n| n == SYMBOL))
        .ok_or_else(|| format!("{}: no symbol {SYMBOL}", exe.display()))?;
    Ok(Expected {
        e_entry: elf.entry(),
        run: sym.address(),
    })
}

/// Counts a call, and counts it in `ENV_BAD` when its env is not the host env the fake gave (the route must pass the host object).
fn seen(env: usize) {
    CALLS_SEEN.fetch_add(1, Ordering::Relaxed);
    if env != fake().env {
        ENV_BAD.fetch_add(1, Ordering::Relaxed);
    }
}

/// Counts one wrong value in `counter` when `bad`.
fn wrong(counter: &AtomicUsize, bad: bool) {
    if bad {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

extern "C" fn get_version(env: usize) -> i32 {
    seen(env);
    JNI_VERSION_1_6
}

extern "C" fn find_class(env: usize, name: *const c_char) -> usize {
    seen(env);
    // SAFETY: the fixture's class name literal, NUL-terminated (jni.h FindClass).
    let name = unsafe { CStr::from_ptr(name) };
    wrong(&CLASS_BAD, name.to_bytes() != b"atl/Adder");
    CLASS
}

extern "C" fn get_static_method_id(
    env: usize,
    cls: usize,
    name: *const c_char,
    sig: *const c_char,
) -> usize {
    seen(env);
    // SAFETY: the fixture's method name and signature literals, NUL-terminated (jni.h GetStaticMethodID).
    let (name, sig) = unsafe { (CStr::from_ptr(name), CStr::from_ptr(sig)) };
    wrong(
        &CLASS_BAD,
        cls != CLASS || name.to_bytes() != b"add" || sig.to_bytes() != b"(II)I",
    );
    METHOD
}

/// `CallStaticIntMethod(env, cls, mid, ...)`: the variadic ints follow the three fixed arguments in order.
extern "C" fn call_static_int_method(env: usize, cls: usize, mid: usize, a: i32, b: i32) -> i32 {
    seen(env);
    wrong(
        &CALL_BAD,
        cls != CLASS || mid != METHOD || a != ADD_A || b != ADD_B,
    );
    a.wrapping_mul(10).wrapping_add(b)
}

extern "C" fn get_java_vm(env: usize, out: *mut usize) -> i32 {
    seen(env);
    // SAFETY: the fixture's `&vm` out-parameter (jni.h GetJavaVM), writable for the call.
    unsafe { *out = fake().vm };
    0
}

extern "C" fn new_string_utf(env: usize, chars: *const c_char) -> usize {
    seen(env);
    // SAFETY: the fixture's string literal, NUL-terminated (jni.h NewStringUTF).
    let chars = unsafe { CStr::from_ptr(chars) };
    wrong(&STR_BAD, chars.to_bytes() != b"abc");
    STRING
}

/// The fixture passes NULL for `is_copy`, so the fake does not write it.
extern "C" fn get_string_utf_chars(env: usize, s: usize, _is_copy: *mut u8) -> *const c_char {
    seen(env);
    wrong(&STR_BAD, s != STRING);
    STR_CHARS.as_ptr().cast()
}

extern "C" fn release_string_utf_chars(env: usize, s: usize, chars: *const c_char) {
    seen(env);
    wrong(&STR_BAD, s != STRING || chars != STR_CHARS.as_ptr().cast());
}

/// A table entry the fixture does not call: the route reached one the gate does not model, so the run ends here.
extern "C" fn unimplemented() -> usize {
    println!("qemu-jni: FAIL a JNI entry the fake does not implement was called");
    std::process::exit(2);
}

/// The fake objects, built on first use: each object's word 0 is its table, and an entry the fixture does not call is `unimplemented`.
fn fake() -> &'static Fake {
    FAKE.get_or_init(|| {
        let mut env_table = vec![unimplemented as *const () as usize; ENV_SLOTS];
        for (slot, f) in [
            (GET_VERSION, get_version as *const () as usize),
            (FIND_CLASS, find_class as *const () as usize),
            (
                GET_STATIC_METHOD_ID,
                get_static_method_id as *const () as usize,
            ),
            (
                CALL_STATIC_INT_METHOD,
                call_static_int_method as *const () as usize,
            ),
            (NEW_STRING_UTF, new_string_utf as *const () as usize),
            (
                GET_STRING_UTF_CHARS,
                get_string_utf_chars as *const () as usize,
            ),
            (
                RELEASE_STRING_UTF_CHARS,
                release_string_utf_chars as *const () as usize,
            ),
            (GET_JAVA_VM, get_java_vm as *const () as usize),
        ] {
            env_table[slot] = f;
        }
        let vm_table = vec![unimplemented as *const () as usize; VM_SLOTS];
        let env_table: &'static [usize] = Box::leak(env_table.into_boxed_slice());
        let vm_table: &'static [usize] = Box::leak(vm_table.into_boxed_slice());
        let env: &'static usize = Box::leak(Box::new(env_table.as_ptr() as usize));
        let vm: &'static usize = Box::leak(Box::new(vm_table.as_ptr() as usize));
        Fake {
            env: env as *const usize as usize,
            vm: vm as *const usize as usize,
        }
    })
}

/// BOOT's continuation: the started guest (or its error) runs `qj_run` with the guest env of the fake, then the checks. The answer is the boot executable's exit status.
fn started(r: Result<Guest, String>) -> u64 {
    let Some(x) = EXPECTED.get() else {
        return 2;
    };
    let mut guest = match r {
        Ok(g) => g,
        Err(e) => {
            println!("qemu-jni: (s) FAIL {e}");
            return 1;
        }
    };
    println!("qemu-jni: (s) ok");
    let bias = (guest.exe_entry as u64).wrapping_sub(x.e_entry);
    let run = bias.wrapping_add(x.run) as usize;
    let fake = fake();
    // the guest env standing for the fake host env, made on this thread after BOOT (jni.rs `guest_env`)
    let env = guest_env(fake.env);
    let mut out = [0i64; OUT_WORDS];
    // SAFETY: `run` is qj_run (ELF symbol plus the load bias), a function of a JNIEnv* and a long long[OUT_WORDS] (fixture.c); `env` is this thread's guest env and `out` stays writable for the call.
    unsafe { guest.call(run, env, out.as_mut_ptr() as usize, 0) };
    let mut ok = true;

    ok &= check(
        'v',
        if out[0] == i64::from(JNI_VERSION_1_6) {
            Ok(())
        } else {
            Err(format!(
                "GetVersion returned {:#x}, expected {JNI_VERSION_1_6:#x}",
                out[0]
            ))
        },
    );

    let calls = CALLS_SEEN.load(Ordering::Relaxed);
    let env_bad = ENV_BAD.load(Ordering::Relaxed);
    ok &= check(
        'e',
        if calls == CALLS && env_bad == 0 {
            Ok(())
        } else {
            Err(format!(
                "{calls} calls reached the fake (expected {CALLS}), {env_bad} with an env other than the host env"
            ))
        },
    );

    let class_bad = CLASS_BAD.load(Ordering::Relaxed);
    let call_bad = CALL_BAD.load(Ordering::Relaxed);
    ok &= check(
        'c',
        if class_bad == 0 && call_bad == 0 && out[1] == ADD_EXPECT {
            Ok(())
        } else {
            Err(format!(
                "lookups with wrong values {class_bad}, CallStaticIntMethod with wrong arguments {call_bad}, returned {} (expected {ADD_EXPECT})",
                out[1]
            ))
        },
    );

    let vm_want = guest_vm(fake.vm) as i64;
    ok &= check(
        'j',
        if out[2] == 0 && out[3] != 0 && out[3] == vm_want {
            Ok(())
        } else {
            Err(format!(
                "GetJavaVM returned {}, gave {:#x}, expected the guest vm {vm_want:#x}",
                out[2], out[3]
            ))
        },
    );

    let str_bad = STR_BAD.load(Ordering::Relaxed);
    ok &= check(
        't',
        if str_bad == 0 && out[4] == 1 {
            Ok(())
        } else {
            Err(format!(
                "string calls with wrong values {str_bad}, round trip {}",
                out[4]
            ))
        },
    );
    u64::from(!ok)
}

/// Loads the bionic provider every ART process loads (`libatl_bionic`, built next to the NB output at `<out>/crates/atl-bionic/lib`, `<nb>` = `<out>/nb/<arch>`). The stubs bind its property functions by name with RTLD_NOLOAD (bindings.rs `Host::Provider`), and the bionic linker calls them at its start: without it 10 stub bindings are absent and the start faults.
fn load_provider(nb: &Path) -> Result<(), String> {
    let lib = nb
        .ancestors()
        .nth(2)
        .ok_or("the NB directory is not <out>/nb/<arch>")?
        .join("crates/atl-bionic/lib/libatl_bionic.so.0.0.0");
    let path =
        std::ffi::CString::new(lib.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: dlopen of a NUL-terminated path; the provider's constructors are its own.
    let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if h.is_null() {
        return Err(format!("dlopen {}: failed", lib.display()));
    }
    Ok(())
}

fn main() -> ExitCode {
    let vars = [
        "ATL_ENGINE_SO",
        "ATL_NB_BOOT",
        "ATL_NB_JNIEXE",
        "ATL_NB_LINKER",
        "ATL_NB_VDSO",
    ]
    .map(|k| std::env::var_os(k).map(PathBuf::from));
    let [
        Some(so),
        Some(boot),
        Some(exe),
        Some(linker),
        Some(vdso_path),
    ] = vars
    else {
        println!(
            "qemu-jni: SKIP (ATL_ENGINE_SO / ATL_NB_BOOT / ATL_NB_JNIEXE / ATL_NB_LINKER / ATL_NB_VDSO unset)"
        );
        return ExitCode::SUCCESS;
    };
    match expected(&exe) {
        Ok(x) => {
            let _ = EXPECTED.set(x);
        }
        Err(e) => {
            println!("qemu-jni: FAIL fixture: {e}");
            return ExitCode::from(2);
        }
    }
    // the arm64 NB directory: the linker is at <nb>/native_bridge_guest_linker/linker64; the stub libc, libdl, ld-android and libm sit beside it
    let nb = linker
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let cfg = Config {
        linker,
        vdso: vdso_path,
        exe,
        library_path: vec![
            nb.join("libnative_bridge_guest_libc"),
            nb.join("libdl"),
            nb.join("ld-android"),
            nb.join("libm"),
        ],
    };
    if let Err(e) = load_provider(&nb) {
        println!("qemu-jni: FAIL {e}");
        return ExitCode::from(2);
    }
    let r = qemu::run(&so, &boot, cfg, started);
    println!("qemu-jni: FAIL QEMU main returned {r:?}");
    ExitCode::from(2)
}
