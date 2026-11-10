// SPDX-License-Identifier: MIT
// Guest side of the Q5-6 thread gate (xtask `test qemu-thread`, QEMU bridge plan Q5-6): a bionic PIE executable that the guest linker loads; the host calls its qt_* functions through the crossing (tests/qemu_thread.rs).
// The properties are the gate's: xtask writes `atl.qt.p.<i>` and `atl.qt.expect` into the area the provider maps (ATL_PROP_AREA). The guest cannot set properties (the provider's set is ENOENT until the property service lands), so qt_props_count counts the store and qt_props_expected reads the seeded count.
// qt_routine adds QT_JOIN_OFFSET. Only the stub libc's intercepted stubs are called (pthread_*, __system_property_*): no other libc symbol resolves in the guest.
#include <pthread.h>
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

int main(void) {
    return 0;
}
