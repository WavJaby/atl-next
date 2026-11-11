// SPDX-License-Identifier: MIT
// Guest side of the Q5-6 thread gate (xtask `test qemu-thread`, QEMU bridge plan Q5-6): a bionic PIE executable that the guest linker loads; the host calls its qt_* functions through the crossing (tests/qemu_thread.rs).
// The properties are the gate's: xtask writes `atl.qt.p.<i>` and `atl.qt.expect` into the area the provider maps (ATL_PROP_AREA). The guest cannot set properties (the provider's set is ENOENT until the property service lands), so qt_props_count counts the store and qt_props_expected reads the seeded count.
// qt_routine adds QT_JOIN_OFFSET. Libc calls: the stub libc's intercepted stubs (pthread_*, __system_property_*) and, for the Q7-7 checks, guest bionic code (the setjmp family; sigprocmask through the guest's rt_sigprocmask syscall).
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <sys/system_properties.h>

// qt_routine returns its argument plus this
#define QT_JOIN_OFFSET 1000

static const char qt_prefix[] = "atl.qt.p.";
#define QT_PREFIX_LEN (sizeof(qt_prefix) - 1)

static pthread_key_t qt_key;
static int qt_key_ready;
static long qt_last;

// foreach callback's name reader: a nested guest call on the same thread (read_callback)
static void qt_name(void *cookie, const char *name, const char *value, uint32_t serial) {
    (void)value;
    (void)serial;
    unsigned i = 0;
    while (i < QT_PREFIX_LEN && name[i] == qt_prefix[i]) {
        i++;
    }
    if (i == QT_PREFIX_LEN) {
        (*(long *)cookie)++;
    }
}

static void qt_on_prop(const struct prop_info *pi, void *cookie) {
    __system_property_read_callback(pi, qt_name, cookie);
}

// Counts the seeded properties (names under qt_prefix) through the provider's foreach.
long qt_props_count(void) {
    long count = 0;
    (void)__system_property_foreach(qt_on_prop, &count);
    return count;
}

// The count the gate seeded (`atl.qt.expect`), in decimal.
long qt_props_expected(void) {
    char value[PROP_VALUE_MAX];
    int n = __system_property_get("atl.qt.expect", value);
    long v = 0;
    for (int i = 0; i < n && value[i] >= '0' && value[i] <= '9'; i++) {
        v = v * 10 + (value[i] - '0');
    }
    return v;
}

static void *qt_routine(void *arg) {
    return (void *)((uintptr_t)arg + QT_JOIN_OFFSET);
}

// One thread runs qt_routine(x) and is joined; returns what join reports, or -1 when create fails.
long qt_join(long x) {
    pthread_t t;
    void *ret = 0;
    if (pthread_create(&t, 0, qt_routine, (void *)(uintptr_t)x) != 0) {
        return -1;
    }
    (void)pthread_join(t, &ret);
    return (long)(uintptr_t)ret;
}

// The key's destructor: stores the value its thread's key held at exit.
static void qt_dtor(void *v) {
    qt_last = (long)(uintptr_t)v;
}

static void *qt_key_routine(void *arg) {
    (void)pthread_setspecific(qt_key, arg);
    return 0;
}

// One thread sets the key to x and exits; the destructor stores x. Returns 0 when the thread ran.
long qt_key_run(long x) {
    if (!qt_key_ready) {
        if (pthread_key_create(&qt_key, qt_dtor) != 0) {
            return -1;
        }
        qt_key_ready = 1;
    }
    pthread_t t;
    if (pthread_create(&t, 0, qt_key_routine, (void *)(uintptr_t)x) != 0) {
        return -1;
    }
    (void)pthread_join(t, 0);
    return 0;
}

long qt_last_value(void) {
    return qt_last;
}

long qt_nop(void) {
    return 0;
}

// R10 S3 stack gate (check g): a thread created with this attr stack size must report exactly it
#define QT_SMALL_STACK (256 * 1024)
// bytes each concurrent down call (check i) fills on its own stack
#define QT_FILL_BYTES (64 * 1024)
// spin bound while waiting for the other filler: a lost partner is an error, not a hang
#define QT_SPIN_LIMIT 2000000000L

// pthread_getattr_np(pthread_self()) on the calling thread; 0 when a local lies inside the reported [base, base + size), else 1..3 by the step that failed. Stores base, size, guard.
static long qt_stack_check(uintptr_t *base_out, size_t *size_out, size_t *guard_out) {
    pthread_attr_t a;
    void *base = 0;
    size_t size = 0;
    size_t guard = 0;
    if (pthread_getattr_np(pthread_self(), &a) != 0) {
        return 1;
    }
    int rc = pthread_attr_getstack(&a, &base, &size);
    if (rc == 0) {
        rc = pthread_attr_getguardsize(&a, &guard);
    }
    (void)pthread_attr_destroy(&a);
    if (rc != 0) {
        return 2;
    }
    *base_out = (uintptr_t)base;
    *size_out = size;
    *guard_out = guard;
    volatile char local = 0;
    uintptr_t sp = (uintptr_t)&local;
    if (sp < (uintptr_t)base || sp >= (uintptr_t)base + size) {
        return 3;
    }
    return 0;
}

// what qt_stack_run's thread saw (read after join)
static uintptr_t qt_run_base;
static size_t qt_run_size;
static size_t qt_run_guard;

static void *qt_stack_routine(void *arg) {
    (void)arg;
    return (void *)(uintptr_t)qt_stack_check(&qt_run_base, &qt_run_size, &qt_run_guard);
}

// One thread, with a QT_SMALL_STACK attr when small != 0 (else no attr), runs qt_stack_check and is joined; returns its result (-1 when create fails). qt_size_seen / qt_guard_seen read what it saw.
long qt_stack_run(long small) {
    pthread_attr_t a;
    pthread_attr_t *ap = 0;
    qt_run_size = 0;
    qt_run_guard = 0;
    if (small) {
        if (pthread_attr_init(&a) != 0 || pthread_attr_setstacksize(&a, QT_SMALL_STACK) != 0) {
            return -1;
        }
        ap = &a;
    }
    pthread_t t;
    void *ret = (void *)(uintptr_t)-1;
    int rc = pthread_create(&t, ap, qt_stack_routine, 0);
    if (ap) {
        (void)pthread_attr_destroy(ap);
    }
    if (rc != 0) {
        return -1;
    }
    (void)pthread_join(t, &ret);
    return (long)(uintptr_t)ret;
}

long qt_small_stack(void) {
    return QT_SMALL_STACK;
}

long qt_size_seen(void) {
    return (long)qt_run_size;
}

long qt_guard_seen(void) {
    return (long)qt_run_guard;
}

// fillers that reached the barrier in qt_stack_fill
static long qt_arrived;
// each filler's reported stack, by slot (0 or 1)
static uintptr_t qt_fill_base[2];
static size_t qt_fill_size[2];

void qt_fill_reset(void) {
    __atomic_store_n(&qt_arrived, 0, __ATOMIC_SEQ_CST);
}

// One of two concurrent down calls (check i), slot 0 or 1: fills QT_FILL_BYTES of its own stack with a slot pattern, waits until both fillers arrived (both buffers live at once), then verifies. Returns mismatched bytes, plus (1 << 32) * the qt_stack_check step that failed; -1 when the partner never came.
long qt_stack_fill(long slot) {
    volatile unsigned char buf[QT_FILL_BYTES];
    long pattern = 0x11 + slot * 0x66;
    for (long i = 0; i < QT_FILL_BYTES; i++) {
        buf[i] = (unsigned char)(pattern + i);
    }
    __atomic_add_fetch(&qt_arrived, 1, __ATOMIC_SEQ_CST);
    long spins = 0;
    while (__atomic_load_n(&qt_arrived, __ATOMIC_SEQ_CST) < 2) {
        if (++spins > QT_SPIN_LIMIT) {
            return -1;
        }
    }
    long bad = 0;
    for (long i = 0; i < QT_FILL_BYTES; i++) {
        if (buf[i] != (unsigned char)(pattern + i)) {
            bad++;
        }
    }
    size_t guard = 0;
    long step = qt_stack_check(&qt_fill_base[slot & 1], &qt_fill_size[slot & 1], &guard);
    return bad + (step << 32);
}

// filler slot's reported stack (after qt_stack_fill), for the harness's disjointness check
long qt_fill_base_at(long slot) {
    return (long)qt_fill_base[slot & 1];
}

long qt_fill_size_at(long slot) {
    return (long)qt_fill_size[slot & 1];
}

// Q7-7 setjmp gate (checks k, m): the guest libc's own bionic setjmp.S, never a host stub
// the value longjmp hands back to setjmp
#define QT_JMP_VALUE 7
// the signal whose blocked state siglongjmp must restore
#define QT_JMP_SIG SIGUSR1

static __attribute__((noinline)) void qt_jump(jmp_buf b) {
    longjmp(b, QT_JMP_VALUE);
}

// setjmp, then longjmp(QT_JMP_VALUE) from a deeper frame; returns setjmp's second answer + 100 * the times the first-return path ran (expected QT_JMP_VALUE + 100: the volatile local survived, the jump happened once).
long qt_jmp(void) {
    jmp_buf b;
    volatile long firsts = 0;
    int r = setjmp(b);
    if (r == 0) {
        firsts++;
        qt_jump(b);
        return -1;
    }
    return r + 100 * firsts;
}

// sigsetjmp(b, 1) with QT_JMP_SIG unblocked, block it, siglongjmp back: 0 when the mask was restored (unblocked), 1 when still blocked, negative when a mask call failed.
long qt_sigjmp(void) {
    sigjmp_buf b;
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, QT_JMP_SIG);
    if (sigprocmask(SIG_UNBLOCK, &s, 0) != 0) {
        return -2;
    }
    if (sigsetjmp(b, 1) == 0) {
        if (sigprocmask(SIG_BLOCK, &s, 0) != 0) {
            return -3;
        }
        siglongjmp(b, 1);
    }
    sigset_t now;
    if (sigprocmask(SIG_SETMASK, 0, &now) != 0) {
        return -4;
    }
    return sigismember(&now, QT_JMP_SIG);
}

// Q7-7 pthread_exit gate (check x): the value a routine hands pthread_exit one frame down reaches join, and its key destructor runs
// pthread_exit's argument is the routine's argument plus this
#define QT_EXIT_OFFSET 2000

static __attribute__((noinline)) void qt_exit_deep(void *arg) {
    pthread_exit((void *)((uintptr_t)arg + QT_EXIT_OFFSET));
}

static void *qt_exit_routine(void *arg) {
    (void)pthread_setspecific(qt_key, arg);
    qt_exit_deep(arg);
    // reached only when pthread_exit returned: join then reports -1
    return (void *)(uintptr_t)-1;
}

// One thread sets the key to x and calls pthread_exit(x + QT_EXIT_OFFSET) from a callee; returns what join reports (-1 when create fails or pthread_exit returned). qt_last_value then holds what the destructor stored.
long qt_exit_run(long x) {
    if (!qt_key_ready) {
        if (pthread_key_create(&qt_key, qt_dtor) != 0) {
            return -1;
        }
        qt_key_ready = 1;
    }
    qt_last = 0;
    pthread_t t;
    void *ret = 0;
    if (pthread_create(&t, 0, qt_exit_routine, (void *)(uintptr_t)x) != 0) {
        return -1;
    }
    (void)pthread_join(t, &ret);
    return (long)(uintptr_t)ret;
}

int main(void) {
    return 0;
}
