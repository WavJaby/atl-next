// SPDX-License-Identifier: MIT

package main

import (
	"slices"
	"testing"
)

// Expected orders: Soong's android/arch.go -- setOSProperties (host, linux, host_linux, bionic, glibc, <os>, not_windows for host classes, android64/32 for devices), then getArchProperties (arch.<arch>, its variant and features, multilib, linux_<arch>, bionic_<arch>, <os>_<arch>, glibc_<arch>).
// The libz shape: arch's -DCPU_NO_SIMD, then android_x86_64's -UCPU_NO_SIMD, so the device side builds its SIMD code.
func TestFlattenBlockOrder(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_library {
    name: "libz",
    host_supported: true,
    cflags: ["-DBASE"],
    soong_config_variables: { v: { conditions_default: { cflags: ["-DCONFIG"] } } },
    arch: { x86_64: { cflags: ["-DCPU_NO_SIMD"], ssse3: { cflags: ["-DFEATURE"] } } },
    multilib: { lib64: { cflags: ["-DLIB64"] } },
    target: {
        android_x86_64: { cflags: ["-UCPU_NO_SIMD"] },
        bionic_x86_64: { cflags: ["-DBIONIC_ARCH"] },
        linux_x86_64: { cflags: ["-DLINUX_ARCH"] },
        glibc_x86_64: { cflags: ["-DGLIBC_ARCH"] },
        linux_glibc_x86_64: { cflags: ["-DLINUX_GLIBC_ARCH"] },
        android64: { cflags: ["-DANDROID64"] },
        android32: { cflags: ["-DANDROID32"] },
        not_windows: { cflags: ["-DNOT_WINDOWS"] },
        linux_glibc: { cflags: ["-DLINUX_GLIBC"] },
        glibc: { cflags: ["-DGLIBC"] },
        android: { cflags: ["-DOS_ANDROID"] },
        bionic: { cflags: ["-DBIONIC"] },
        linux: { cflags: ["-DLINUX"] },
        host: { cflags: ["-DHOST"] },
    },
}
`})
	g.soong.Device = DeviceArch{Features: []string{"ssse3"}}
	for _, c := range []struct {
		side Side
		want []string
	}{
		{Device, []string{"-DBASE", "-DCONFIG", "-DLINUX", "-DBIONIC", "-DOS_ANDROID", "-DANDROID64", "-DCPU_NO_SIMD", "-DFEATURE", "-DLIB64", "-DLINUX_ARCH", "-DBIONIC_ARCH", "-UCPU_NO_SIMD"}},
		{Host, []string{"-DBASE", "-DCONFIG", "-DHOST", "-DLINUX", "-DGLIBC", "-DLINUX_GLIBC", "-DNOT_WINDOWS", "-DCPU_NO_SIMD", "-DLIB64", "-DLINUX_ARCH", "-DLINUX_GLIBC_ARCH", "-DGLIBC_ARCH"}},
	} {
		if got := g.flatten(g.lookup("libz", c.side), c.side, Shared).strs("cflags"); !slices.Equal(got, c.want) {
			t.Errorf("%s: cflags %q\nwant %q", c.side, got, c.want)
		}
	}
}
