// SPDX-License-Identifier: MIT
//! Q6-2 / Q6-3 gate (QEMU bridge plan Q6-2, Q6-3), harness-less: guest JNI calls under QEMU on the real arm64 guest linker. The main executable (`xtask-harness/testdata/qemu-jni/fixture.c`, PIE on libc.so) is started; the host calls its `qj_run` with the guest env of a fake host JNIEnv (this file), and each JNI call the fixture makes crosses to the fake.
//! The fake records what each call got and answers with spec values: GetVersion gives JNI_VERSION_1_6 (jni.h), CallStaticIntMethod(a, b) gives a * 10 + b, so a swapped pair shows as 32, not 23. CallStaticDoubleMethod and CallStaticFloatMethod check the lanes they were given (the stack spill, the fp arguments) and answer from them.
//! Driver: run without `ATL_QJ_CASE`, it first runs each refusal case in a child process (QEMU starts once per process), then the plain case in this process. A refusal case must abort by name: SIGABRT and its message on stderr.
//! Env (xtask qemu_jni.rs sets it; unset prints SKIP): ATL_ENGINE_SO (the engine `.so`), ATL_NB_BOOT (the engine boot executable, crates/atl-qemu-boot), ATL_NB_JNIEXE (fixture.c, PIE), ATL_NB_LINKER, ATL_NB_VDSO (the arm64 NB images; `<nb>/libnative_bridge_guest_libc` among the guest library directories).
//! Checks: (s) started; (v) GetVersion returned JNI_VERSION_1_6; (e) all 11 calls reached the fake, each with the host env it was given; (c) FindClass and GetStaticMethodID got the names and the handles they issued, CallStaticIntMethod got (cls, mid, 2, 3), and the fixture got 23; (j) GetJavaVM, a special with its own host thunk, returned the guest vm of the fake vm; (t) NewStringUTF and GetStringUTFChars round-tripped "abc"; (d) SetDoubleField got 2.5, CallStaticDoubleMethod got its lanes and returned DOUBLE_EXPECT; (f) CallStaticFloatMethod returned FLOAT_EXPECT; (a) glibc atof, bound with result class Fp, returned ATOF_EXPECT in v0; (p) glibc snprintf, bound through `qemu::bind` and called from the guest with a double vararg, wrote SNPRINTF_EXPECT (the `%al` row); (V) CallStaticDoubleMethodV, CallLongMethodV, CallNonvirtualIntMethodV and NewObjectV with 19 mixed arguments through a guest va_list reached the `...A` slots with each jvalue right, and their results came back; (r) a reserved slot, (l) a va_list entry with no shorty source, (g) a forged id past the table: each refused by name. Q6-4 down calls, each through `qemu::down` with the host env or vm: (o) JNI_OnLoad returned JNI_VERSION_1_6 and got the guest vm and the reserved pointer; (m) nativeMix (14 arguments, a double result) got the guest env and class and returned MIX_EXPECT; (x) nativeScale (a float result) got the guest env and class and returned SCALE_EXPECT; (R) a down call of 15 integers is refused by name in its child.
//! Output: `qemu-jni: (x) ok` per check, `(x) FAIL <why>` otherwise, exit 0 when all pass.
#![expect(
    clippy::disallowed_types,
    reason = "a test: its driver runs each refusal case as a child of this test binary"
)]
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]
#![allow(
    unsafe_code,
    reason = "a fake JNIEnv and JavaVM in the C ABI take and answer raw JNI pointers; calls the guest through Guest::call"
)]

use std::ffi::{CStr, c_char, c_int};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};

use atl_native_bridge::engine::identity::Down;
use atl_native_bridge::engine::jni::{guest_env, guest_vm, set_method_shorty};
use atl_native_bridge::engine::objects::Class;
use atl_native_bridge::engine::{Config, Guest, qemu};
use object::{Object, ObjectSymbol, read::elf::ElfFile64};

/// The fixture function the host calls.
const SYMBOL: &str = "qj_run";
/// The env variable that makes a process run one refusal case (the driver sets it on its children).
const CASE_ENV: &str = "ATL_QJ_CASE";
/// fixture.c `ADD_A` and `ADD_B`: the arguments of CallStaticIntMethod.
const ADD_A: i32 = 2;
const ADD_B: i32 = 3;
/// The fake's answer for those arguments: a * 10 + b.
const ADD_EXPECT: i64 = (ADD_A * 10 + ADD_B) as i64;
/// jni.h `JNI_VERSION_1_6`: the fake's GetVersion answer.
const JNI_VERSION_1_6: i32 = 0x0001_0006;
/// fixture.c `OBJ` and `FIELD`: the object and field SetDoubleField gets.
const OBJ: usize = 0xb7e;
const FIELD: usize = 0xf1d;
/// The value fixture.c passes to SetDoubleField.
const SET_EXPECT: f64 = 2.5;
/// CallStaticDoubleMethod's answer to the arguments fixture.c passes (see `call_static_double_method`): the weighted integer arguments give 260, the fp arguments (2.25 + 2 * 7.5) / 2 = 8.625.
const DOUBLE_EXPECT: f64 = 268.625;
/// CallStaticFloatMethod's answer to 7 (see `call_static_float_method`): 7 * 0.5.
const FLOAT_EXPECT: f32 = 3.5;
/// jni.h slots of the entries the fixture calls (`JNINativeInterface`: reserved0..reserved3, then GetVersion at 4), and the table sizes (233 env entries, 8 vm entries).
const GET_VERSION: usize = 4;
const FIND_CLASS: usize = 6;
const GET_STATIC_METHOD_ID: usize = 113;
const SET_DOUBLE_FIELD: usize = 112;
const CALL_STATIC_INT_METHOD: usize = 129;
const CALL_STATIC_FLOAT_METHOD: usize = 135;
const CALL_STATIC_DOUBLE_METHOD: usize = 138;
const NEW_STRING_UTF: usize = 167;
const GET_STRING_UTF_CHARS: usize = 169;
const RELEASE_STRING_UTF_CHARS: usize = 170;
const GET_JAVA_VM: usize = 219;
const ENV_SLOTS: usize = 233;
const VM_SLOTS: usize = 8;
/// A JNI id past the env table (233 entries), below the stub array (256): the forged-id refusal calls its stub.
const FORGED_SLOT: usize = 240;
/// The JNI calls fixture.c makes: GetVersion, FindClass, GetStaticMethodID, CallStaticIntMethod, GetJavaVM, NewStringUTF, GetStringUTFChars, ReleaseStringUTFChars, SetDoubleField, CallStaticDoubleMethod, CallStaticFloatMethod.
const CALLS: usize = 11;
/// The words of `out`: fixture.c's results at 0..7; the snprintf row (`p`) takes the pool address of its binding at 8, the buffer's address at 9, its size at 11 and returns snprintf's result at 10; the atof row (`a`) takes its binding at 12 and its argument at 13 and returns the result's bits at 14.
/// Q6-4: the down calls' records (fixture.c `qj_seen`, copied by qj_run): JNI_OnLoad's vm at 15 and reserved at 16, nativeMix's env at 17 and class at 18, nativeScale's env at 19 and class at 20.
/// Q6-7: the V calls' results at 21..24 (fixture.c `qj_run`).
const OUT_WORDS: usize = 25;
/// Q6-7, the `...V` calls (check V): fixture.c passes V_ARGS through a va_list with method V_MID; the engine reads them by V_MID's shorty and calls these `...A` slots (jni.h order), which check each jvalue and answer their V_RET_*.
const NEW_OBJECT_A: usize = 30;
const CALL_LONG_METHOD_A: usize = 54;
const CALL_NONVIRTUAL_INT_METHOD_A: usize = 81;
const CALL_STATIC_DOUBLE_METHOD_A: usize = 140;
/// fixture.c `V_MID`.
const V_MID: usize = 0x7a11;
/// One argument of fixture.c `V_ARGS`, by its JNI type.
#[derive(Clone, Copy)]
enum V {
    I(i32),
    J(i64),
    F(f32),
    D(f64),
    L(usize),
}
/// fixture.c `V_ARGS`, in order: 10 integer-class (I, J, L) and 9 fp (F, D) arguments, so both of the va_list's register save areas run out and `__stack` holds the rest. Each value fits its type exactly (the floats are binary fractions); the longs need their upper half.
const V_ARGS: [V; 19] = [
    V::I(11),
    V::D(1.25),
    V::J(0x1_0000_0002),
    V::F(0.5),
    V::L(0xb01),
    V::I(-12),
    V::D(2.5),
    V::J(-3),
    V::F(1.75),
    V::I(13),
    V::D(3.75),
    V::J(0x7_0000_0004),
    V::F(2.25),
    V::I(14),
    V::D(4.5),
    V::J(5),
    V::F(3.125),
    V::L(0xb02),
    V::D(5.875),
];
/// The answers of the four `...A` slots: arbitrary values of their result types, distinct from any argument.
const V_RET_D: f64 = 0.375;
const V_RET_J: i64 = 0x1234_5678_9abc_def0;
const V_RET_I: i32 = -77;
const V_RET_OBJ: usize = 0x0b0b;
/// The snprintf row (`p`): glibc's `snprintf("%d %.2f", 7, 2.5)` (C99 7.21.6.1: `%d` writes 7, `%.2f` writes 2.50), so the text and its length. The buffer has room for it and its NUL.
const SNPRINTF_EXPECT: &str = "7 2.50";
const SNP_CAP: usize = 16;
/// The atof row (`a`): glibc's `atof("2.5")`, a double result in xmm0 that the binding answers in v0 (class Fp).
const ATOF_TEXT: &[u8] = b"2.5\0";
const ATOF_EXPECT: f64 = 2.5;

/// The down calls (Q6-4; fixture.c `JNI_OnLoad`, `nativeMix`, `nativeScale`): their symbols, the reserved pointer JNI_OnLoad is given, and the shorties ART passes (the result, then each parameter: I int, J long, D double, F float).
const ON_LOAD: &str = "JNI_OnLoad";
const MIX: &str = "nativeMix";
const SCALE: &str = "nativeScale";
const RESERVED: usize = 0x0bad_f00d;
const MIX_SHORTY: &[u8] = b"DIDJIDJIDJIDJ";
const SCALE_SHORTY: &[u8] = b"FFI";
/// nativeMix's arguments after env and class, in order: int, double, long, int, double, long, int, double, long, int, double, long.
const MIX_A: i32 = 2;
const MIX_B: f64 = 0.5;
const MIX_C: i64 = 3;
const MIX_D: i32 = 5;
const MIX_E: f64 = 0.25;
const MIX_F: i64 = 7;
const MIX_G: i32 = 11;
const MIX_H: f64 = 0.125;
const MIX_I: i64 = 13;
const MIX_J: i32 = 17;
const MIX_K: f64 = 0.0625;
const MIX_L: i64 = 19;
/// nativeMix's answer: each argument times its weight 10^0 .. 10^11: 2 + 0.5*10 + 3*10^2 + 5*10^3 + 0.25*10^4 + 7*10^5 + 11*10^6 + 0.125*10^7 + 13*10^8 + 17*10^9 + 0.0625*10^10 + 19*10^11 = 1918937957807, exact in a double (below 2^53).
const MIX_EXPECT: f64 = 1_918_937_957_807.0;
/// nativeScale's arguments (1.5 and 6) and answer (1.5 * 6 + 0.25 = 9.25); all exact in a float.
const SCALE_X: f32 = 1.5;
const SCALE_N: i32 = 6;
const SCALE_EXPECT: f32 = 9.25;

// glibc's snprintf and atof, bound through the engine for their rows: the fixture calls them as guest functions
unsafe extern "C" {
    fn snprintf(buf: *mut c_char, n: usize, fmt: *const c_char, ...) -> c_int;
    fn atof(s: *const c_char) -> f64;
}

/// The fake's handles and the string it hands out (NUL-terminated, so `GetStringUTFChars` returns its address).
const CLASS: usize = 0x0c_1a55;
const METHOD: usize = 0x3e7d;
const STRING: usize = 0x5712;
static STR_CHARS: &[u8] = b"abc\0";

/// The values `qj_run` takes for the refusal cases (fixture.c REFUSE_*).
const REFUSE_RESERVED: i64 = 1;
const REFUSE_VALIST: i64 = 2;
const REFUSE_FORGED: i64 = 3;
/// The R-1 case (`downstack`): not a fixture value; its child refuses a down call of `R1_INTS` integers before the guest runs it.
const REFUSE_DOWN_STACK: i64 = 4;
const R1_INTS: usize = 15;
/// The refusal cases: name (`ATL_QJ_CASE`), the check letter, the value `qj_run` takes for it, and the words its named abort must carry.
const CASES: &[(&str, char, i64, &str)] = &[
    ("reserved", 'r', REFUSE_RESERVED, "is reserved"),
    ("valist", 'l', REFUSE_VALIST, "gave no getMethodShorty"),
    ("forged", 'g', REFUSE_FORGED, "past the JNINativeInterface table"),
    (
        "downstack",
        'R',
        REFUSE_DOWN_STACK,
        "more than eight stack words",
    ),
];

/// The refusal case this process runs (0: the plain run): `qj_run`'s third argument.
static CASE: AtomicI64 = AtomicI64::new(0);
/// The driver's verdict on the refusal children, folded into the plain run's exit status.
static REFUSALS_OK: AtomicBool = AtomicBool::new(true);

/// Calls the fake got (every one must be CALLS), and those whose env was not the host env the fake gave.
static CALLS_SEEN: AtomicUsize = AtomicUsize::new(0);
static ENV_BAD: AtomicUsize = AtomicUsize::new(0);
/// Lookups with a name, signature or handle the fixture did not pass; CallStaticIntMethod with other arguments; string calls with another value.
static CLASS_BAD: AtomicUsize = AtomicUsize::new(0);
static CALL_BAD: AtomicUsize = AtomicUsize::new(0);
static STR_BAD: AtomicUsize = AtomicUsize::new(0);
/// SetDoubleField with another object, field or value's bits (the value is recorded in SET_BITS); CallStaticDoubleMethod with other lanes; CallStaticFloatMethod with another argument.
static FIELD_BAD: AtomicUsize = AtomicUsize::new(0);
static SET_BITS: AtomicU64 = AtomicU64::new(0);
static DOUBLE_BAD: AtomicUsize = AtomicUsize::new(0);
static FLOAT_BAD: AtomicUsize = AtomicUsize::new(0);
/// Q6-7: `...A` calls the fake got, and those with a wrong env, object, class, method or jvalue.
static V_SEEN: AtomicUsize = AtomicUsize::new(0);
static V_BAD: AtomicUsize = AtomicUsize::new(0);

/// The fake host objects: a JNIEnv and a JavaVM, each the address of a word holding its function table (jni.h's object layout). Leaked: the process's life.
struct Fake {
    env: usize,
    vm: usize,
}
static FAKE: OnceLock<Fake> = OnceLock::new();

/// What the checks compare against: the executable's entry and the addresses of `qj_run` and the down calls' functions, all from its ELF file (the load bias is the entry's difference).
struct Expected {
    e_entry: u64,
    run: u64,
    on_load: u64,
    mix: u64,
    scale: u64,
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
    let address = |name: &str| {
        elf.symbols()
            .find(|s| s.name().is_ok_and(|n| n == name))
            .map(|s| s.address())
            .ok_or_else(|| format!("{}: no symbol {name}", exe.display()))
    };
    Ok(Expected {
        e_entry: elf.entry(),
        run: address(SYMBOL)?,
        on_load: address(ON_LOAD)?,
        mix: address(MIX)?,
        scale: address(SCALE)?,
    })
}

/// `qemu::down` for the guest function at `addr` in the ELF file, relocated by `bias`: the host-callable entry, bound as ART binds one.
fn bind_down(addr: u64, bias: u64, kind: Down, shorty: Option<&[u8]>) -> usize {
    qemu::down(bias.wrapping_add(addr) as usize, kind, shorty).expect("a free down shell")
}

/// The C prototypes of the down calls: JNI_OnLoad(JavaVM*, void*); nativeMix(JNIEnv*, jclass, then its 12 parameters); nativeScale(JNIEnv*, jclass, jfloat, jint); the R-1 call of 1 result and R1_INTS integers.
type OnLoadFn = extern "C" fn(usize, usize) -> i32;
type MixFn =
    extern "C" fn(usize, usize, i32, f64, i64, i32, f64, i64, i32, f64, i64, i32, f64, i64) -> f64;
type ScaleFn = extern "C" fn(usize, usize, f32, i32) -> f32;
type R1Fn = extern "C" fn(
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
) -> usize;

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

/// `SetDoubleField(env, obj, fid, value)`: a fixed fp parameter, in v0. The fake records its bits.
extern "C" fn set_double_field(env: usize, obj: usize, fid: usize, value: f64) {
    seen(env);
    wrong(&FIELD_BAD, obj != OBJ || fid != FIELD);
    SET_BITS.store(value.to_bits(), Ordering::Relaxed);
}

/// `CallStaticDoubleMethod(env, cls, mid, 1, 2.25, 3, 4, 5, 6, 7.5, 8, 9, 10)` as fixture.c makes it. The integers 1, 3, 4, 5, 6 take x3..x7 (a3..a7); 8, 9, 10 come after the eight integer registers, on the stack (s0..s2); 2.25 and 7.5 are v0 and v1. The host image puts the guest's x6 and x7 at the first two stack words, so a6 and a7 are those and s0..s2 follow.
/// The integers are `jint`: a stack slot's upper half is undefined (AAPCS64), so the callee reads the low 32 bits, as this signature does.
/// The answer is the weighted integers 1*a3 + 2*a4 + ... + 8*s2 (= 260 for these arguments) plus (v0 + 2 * v1) / 2 (= 8.625): a position error shows as another total.
#[allow(
    clippy::too_many_arguments,
    reason = "the C signature: the lanes of one call, in the order the host image lays them out"
)]
extern "C" fn call_static_double_method(
    env: usize,
    cls: usize,
    mid: usize,
    a3: i32,
    a4: i32,
    a5: i32,
    a6: i32,
    a7: i32,
    s0: i32,
    s1: i32,
    s2: i32,
    v0: f64,
    v1: f64,
) -> f64 {
    seen(env);
    wrong(
        &DOUBLE_BAD,
        cls != CLASS
            || mid != METHOD
            || a3 != 1
            || a4 != 3
            || a5 != 4
            || a6 != 5
            || a7 != 6
            || s0 != 8
            || s1 != 9
            || s2 != 10
            || v0.to_bits() != 2.25f64.to_bits()
            || v1.to_bits() != 7.5f64.to_bits(),
    );
    let ints = i64::from(a3)
        + 2 * i64::from(a4)
        + 3 * i64::from(a5)
        + 4 * i64::from(a6)
        + 5 * i64::from(a7)
        + 6 * i64::from(s0)
        + 7 * i64::from(s1)
        + 8 * i64::from(s2);
    ints as f64 + (v0 + 2.0 * v1) / 2.0
}

/// `CallStaticFloatMethod(env, cls, mid, 7)`: one integer, in x3; the float answer comes back in s0.
extern "C" fn call_static_float_method(env: usize, cls: usize, mid: usize, a3: i32) -> f32 {
    seen(env);
    wrong(&FLOAT_BAD, cls != CLASS || mid != METHOD || a3 != 7);
    a3 as f32 * 0.5
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

/// The fake runtime's `getMethodShorty` (native_bridge.h): V_MID's shorty, result first (`D`, any type: the engine reads only the parameters), then V_ARGS's types; null for any other method.
extern "C" fn method_shorty(env: usize, mid: usize) -> *const c_char {
    static SHORTY: OnceLock<Vec<u8>> = OnceLock::new();
    wrong(&V_BAD, env != fake().env);
    if mid != V_MID {
        return std::ptr::null();
    }
    SHORTY
        .get_or_init(|| {
            let mut s = vec![b'D'];
            s.extend(V_ARGS.iter().map(|a| match a {
                V::I(_) => b'I',
                V::J(_) => b'J',
                V::F(_) => b'F',
                V::D(_) => b'D',
                V::L(_) => b'L',
            }));
            s.push(0);
            s
        })
        .as_ptr()
        .cast()
}

/// One `...A` call: counts it, and counts it in V_BAD unless its env, object or class (0 = not passed), method and every jvalue are fixture.c's. A jvalue is 8 bytes, its member at offset 0 (jni.h union).
fn v_call(env: usize, obj: usize, cls: usize, mid: usize, args: *const u64) {
    V_SEEN.fetch_add(1, Ordering::Relaxed);
    let objs = (obj == 0 || obj == OBJ) && (cls == 0 || cls == CLASS);
    let mut bad = env != fake().env || !objs || mid != V_MID;
    for (i, want) in V_ARGS.iter().enumerate() {
        // SAFETY: the engine's jvalue array has one word per shorty parameter, V_ARGS.len() of them.
        let w = unsafe { *args.add(i) };
        bad |= match *want {
            V::I(v) => w as u32 as i32 != v,
            V::J(v) => w as i64 != v,
            V::F(v) => w as u32 != v.to_bits(),
            V::D(v) => w != v.to_bits(),
            V::L(v) => w as usize != v,
        };
    }
    wrong(&V_BAD, bad);
}

extern "C" fn call_static_double_method_a(
    env: usize,
    cls: usize,
    mid: usize,
    args: *const u64,
) -> f64 {
    v_call(env, 0, cls, mid, args);
    V_RET_D
}

extern "C" fn call_long_method_a(env: usize, obj: usize, mid: usize, args: *const u64) -> i64 {
    v_call(env, obj, 0, mid, args);
    V_RET_J
}

extern "C" fn call_nonvirtual_int_method_a(
    env: usize,
    obj: usize,
    cls: usize,
    mid: usize,
    args: *const u64,
) -> i32 {
    v_call(env, obj, cls, mid, args);
    V_RET_I
}

extern "C" fn new_object_a(env: usize, cls: usize, mid: usize, args: *const u64) -> usize {
    v_call(env, 0, cls, mid, args);
    V_RET_OBJ
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
            (SET_DOUBLE_FIELD, set_double_field as *const () as usize),
            (
                CALL_STATIC_DOUBLE_METHOD,
                call_static_double_method as *const () as usize,
            ),
            (
                CALL_STATIC_FLOAT_METHOD,
                call_static_float_method as *const () as usize,
            ),
            (
                CALL_STATIC_DOUBLE_METHOD_A,
                call_static_double_method_a as *const () as usize,
            ),
            (CALL_LONG_METHOD_A, call_long_method_a as *const () as usize),
            (
                CALL_NONVIRTUAL_INT_METHOD_A,
                call_nonvirtual_int_method_a as *const () as usize,
            ),
            (NEW_OBJECT_A, new_object_a as *const () as usize),
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

/// BOOT's continuation: the started guest (or its error) runs `qj_run` with the guest env of the fake, then the checks. A refusal case never returns from `qj_run` (its JNI call aborts the process), so reaching the end of it fails. The answer is the boot executable's exit status.
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
    let refuse = CASE.load(Ordering::Relaxed);
    // the runtime's shorty source, as ART gives it to `initialize`; the va_list refusal case runs without one
    if refuse != REFUSE_VALIST {
        set_method_shorty(method_shorty);
    }
    if refuse == REFUSE_DOWN_STACK {
        // R-1 (Q6-4): 15 integers take six host registers and nine stack words, one more than the lane image holds: the shell refuses the call by name before the guest runs it
        let mut shorty = vec![b'I'; R1_INTS + 1];
        shorty[0] = b'V';
        let entry = bind_down(x.on_load, bias, Down::Plain, Some(&shorty));
        // SAFETY: `entry` is a down shell with R1_INTS integer parameters (R1Fn); the shell refuses the call and the process aborts, so the guest never runs it
        let r1 = unsafe { std::mem::transmute::<usize, R1Fn>(entry) };
        let _ = r1(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        println!("qemu-jni: FAIL refusal case {refuse} returned: the down call was not refused");
        return 1;
    }
    let mut out = [0i64; OUT_WORDS];
    // the forged-id case calls the stub of a JNI id past the env table
    out[7] = if refuse == REFUSE_FORGED {
        qemu::object_stub(FORGED_SLOT) as i64
    } else {
        0
    };
    // the snprintf row (`p`): glibc's snprintf bound through the engine (a pool slot the guest calls), and the host buffer it writes
    let mut text = [0u8; SNP_CAP];
    out[8] = qemu::bind(snprintf as *const () as usize).expect("a free pool slot") as i64;
    out[9] = text.as_mut_ptr() as i64;
    out[11] = SNP_CAP as i64;
    // the atof row (`a`): glibc's atof bound with result class Fp (the proxy table's one fp row, glGetPathLengthNV, needs the GL front this gate does not load); its argument is host memory
    out[12] =
        qemu::bind_result(atof as *const () as usize, Class::Fp).expect("a free pool slot") as i64;
    out[13] = ATOF_TEXT.as_ptr() as i64;
    // the down calls (Q6-4): the host calls guest functions through `qemu::down`, as ART calls them: JNI_OnLoad with the host vm, nativeMix and nativeScale with the host env and class. They run before qj_run, which copies what the guest recorded to out[15..]
    // SAFETY: a down shell bound for JNI_OnLoad (Vm kind, no shorty); OnLoadFn is its prototype, rax its answer
    let on_load = unsafe {
        std::mem::transmute::<usize, OnLoadFn>(bind_down(x.on_load, bias, Down::Vm, None))
    };
    // SAFETY: a down shell bound for nativeMix (Env kind, MIX_SHORTY); MixFn is its prototype, xmm0 its answer
    let mix = unsafe {
        std::mem::transmute::<usize, MixFn>(bind_down(x.mix, bias, Down::Env, Some(MIX_SHORTY)))
    };
    // SAFETY: a down shell bound for nativeScale (Env kind, SCALE_SHORTY); ScaleFn is its prototype, xmm0 its answer
    let scale = unsafe {
        std::mem::transmute::<usize, ScaleFn>(bind_down(
            x.scale,
            bias,
            Down::Env,
            Some(SCALE_SHORTY),
        ))
    };
    let on_load_ret = on_load(fake.vm, RESERVED);
    let mix_ret = mix(
        fake.env, CLASS, MIX_A, MIX_B, MIX_C, MIX_D, MIX_E, MIX_F, MIX_G, MIX_H, MIX_I, MIX_J,
        MIX_K, MIX_L,
    );
    let scale_ret = scale(fake.env, CLASS, SCALE_X, SCALE_N);
    // SAFETY: `run` is qj_run (ELF symbol plus the load bias), a function of a JNIEnv*, a long long[OUT_WORDS] and a long long (fixture.c); `env` is this thread's guest env and `out` stays writable for the call.
    unsafe { guest.call(run, env, out.as_mut_ptr() as usize, refuse as usize) };
    if refuse != 0 {
        println!("qemu-jni: FAIL refusal case {refuse} returned: the call was not refused");
        return 1;
    }
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

    let field_bad = FIELD_BAD.load(Ordering::Relaxed);
    let set_bits = SET_BITS.load(Ordering::Relaxed);
    let double_bad = DOUBLE_BAD.load(Ordering::Relaxed);
    ok &= check(
        'd',
        if field_bad == 0
            && set_bits == SET_EXPECT.to_bits()
            && double_bad == 0
            && out[5] == DOUBLE_EXPECT.to_bits() as i64
        {
            Ok(())
        } else {
            Err(format!(
                "SetDoubleField: {field_bad} wrong args, value bits {set_bits:#x} (expected {:#x}); CallStaticDoubleMethod: {double_bad} wrong lanes, returned bits {:#x} (expected {:#x})",
                SET_EXPECT.to_bits(),
                out[5],
                DOUBLE_EXPECT.to_bits()
            ))
        },
    );

    let float_bad = FLOAT_BAD.load(Ordering::Relaxed);
    ok &= check(
        'f',
        if float_bad == 0 && out[6] == i64::from(FLOAT_EXPECT.to_bits()) {
            Ok(())
        } else {
            Err(format!(
                "CallStaticFloatMethod: {float_bad} wrong args, returned bits {:#x} (expected {:#x})",
                out[6],
                FLOAT_EXPECT.to_bits()
            ))
        },
    );
    let printed = CStr::from_bytes_until_nul(&text)
        .map_or_else(|_| String::new(), |c| c.to_string_lossy().into_owned());
    let atof_bits = ATOF_EXPECT.to_bits() as i64;
    ok &= check(
        'a',
        if out[14] == atof_bits {
            Ok(())
        } else {
            Err(format!(
                "atof through the Fp binding returned bits {:#x}, expected {atof_bits:#x} ({ATOF_EXPECT})",
                out[14]
            ))
        },
    );
    ok &= check(
        'p',
        if usize::try_from(out[10]).ok() == Some(SNPRINTF_EXPECT.len())
            && printed == SNPRINTF_EXPECT
        {
            Ok(())
        } else {
            Err(format!(
                "snprintf returned {}, wrote {printed:?}; expected {}, {SNPRINTF_EXPECT:?}",
                out[10],
                SNPRINTF_EXPECT.len()
            ))
        },
    );
    let v_seen = V_SEEN.load(Ordering::Relaxed);
    let v_bad = V_BAD.load(Ordering::Relaxed);
    let v_want = [
        V_RET_D.to_bits() as i64,
        V_RET_J,
        i64::from(V_RET_I),
        V_RET_OBJ as i64,
    ];
    ok &= check(
        'V',
        if v_seen == 4 && v_bad == 0 && out[21..25] == v_want {
            Ok(())
        } else {
            Err(format!(
                "{v_seen} A calls (expected 4), {v_bad} with a wrong env, handle or jvalue; results {:x?} (expected {v_want:x?})",
                &out[21..25]
            ))
        },
    );
    let vm = guest_vm(fake.vm);
    let on_load_ok =
        on_load_ret == JNI_VERSION_1_6 && out[15] == vm as i64 && out[16] == RESERVED as i64;
    ok &= check(
        'o',
        if on_load_ok {
            Ok(())
        } else {
            Err(format!(
                "JNI_OnLoad returned {on_load_ret:#x} (expected {JNI_VERSION_1_6:#x}), received vm {:#x} (expected {vm:#x}) and reserved {:#x} (expected {RESERVED:#x})",
                out[15], out[16]
            ))
        },
    );
    let mix_ok = mix_ret.to_bits() == MIX_EXPECT.to_bits()
        && out[17] == guest_env(fake.env) as i64
        && out[18] == CLASS as i64;
    ok &= check(
        'm',
        if mix_ok {
            Ok(())
        } else {
            Err(format!(
                "nativeMix returned {mix_ret} (expected {MIX_EXPECT}), received env {:#x} (expected {:#x}) and class {:#x} (expected {CLASS:#x})",
                out[17],
                guest_env(fake.env),
                out[18]
            ))
        },
    );
    let scale_ok = scale_ret.to_bits() == SCALE_EXPECT.to_bits()
        && out[19] == guest_env(fake.env) as i64
        && out[20] == CLASS as i64;
    ok &= check(
        'x',
        if scale_ok {
            Ok(())
        } else {
            Err(format!(
                "nativeScale returned {scale_ret} (expected {SCALE_EXPECT}), received env {:#x} (expected {:#x}) and class {:#x} (expected {CLASS:#x})",
                out[19],
                guest_env(fake.env),
                out[20]
            ))
        },
    );
    u64::from(!(ok && REFUSALS_OK.load(Ordering::Relaxed)))
}

/// One refusal case in a child process: the child must abort (SIGABRT) with the case's named message on stderr. Prints the message as evidence, then the check line.
fn refusal(exe: &Path, case: &str, letter: char, named: &str) -> bool {
    let verdict = match Command::new(exe).env(CASE_ENV, case).output() {
        Err(e) => Err(format!("could not start the child: {e}")),
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).into_owned();
            let line = err.lines().find(|l| l.contains(named)).map(str::to_owned);
            if let Some(line) = &line {
                println!("qemu-jni: refused ({case}): {line}");
            }
            if out.status.signal() == Some(libc::SIGABRT) && line.is_some() {
                Ok(())
            } else {
                Err(format!(
                    "child exited {} (expected SIGABRT), stderr without {named:?}: {err}",
                    out.status
                ))
            }
        }
    };
    check(letter, verdict)
}

/// Driver: every refusal case in its own child process; the verdict goes to `REFUSALS_OK`.
fn driver(exe: &Path) {
    let mut ok = true;
    for &(case, letter, _, named) in CASES {
        ok &= refusal(exe, case, letter, named);
    }
    REFUSALS_OK.store(ok, Ordering::Relaxed);
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
    // the driver's refusal children run first: each starts its own QEMU, and this process's QEMU start below ends the process
    match std::env::var(CASE_ENV) {
        Ok(name) => match CASES.iter().find(|c| c.0 == name) {
            Some(&(_, _, n, _)) => CASE.store(n, Ordering::Relaxed),
            None => {
                println!("qemu-jni: FAIL unknown {CASE_ENV} {name:?}");
                return ExitCode::from(2);
            }
        },
        Err(_) => match std::env::current_exe() {
            Ok(me) => driver(&me),
            Err(e) => {
                println!("qemu-jni: FAIL the test executable: {e}");
                return ExitCode::from(2);
            }
        },
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
