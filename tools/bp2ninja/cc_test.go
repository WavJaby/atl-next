// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// hasRpath: a link line names a run-time search path (-rpath-link is link time only and lands in no output).
func hasRpath(libs []string) bool {
	return slices.ContainsFunc(libs, func(s string) bool { return strings.Contains(s, "-rpath,") })
}

// The libartpalette shape (art/libartpalette/Android.bp): the android block's runtime_libs names libartpalette-system, which providers.toml's `module:` row stands libartpalette_fake (stem libartpalette-system) in for; dalvikvm's lib64 suffix.
// Soong: stem + suffix name the output (and so the soname); a runtime lib is built but never linked; on a device side it is found through the stage, not an rpath.
func TestStemSuffixRuntimeLibs(t *testing.T) {
	const bp = `
cc_library { name: "libpal", srcs: ["p.cc"], target: { android: { runtime_libs: ["libpal-system", "libgone"], exclude_runtime_libs: ["libgone"] } } }
cc_library { name: "libpal_fake", srcs: ["f.cc"], stem: "libpal-system" }
cc_library { name: "libplain", srcs: ["w.cc"] }
cc_binary { name: "vm", srcs: ["v.cc"], shared_libs: ["libpal"], multilib: { lib32: { suffix: "32" }, lib64: { suffix: "64" } } }
`
	g := graphFor(t, map[string]string{"p/Android.bp": bp})
	g.stand = map[string]string{"libpal-system": "libpal_fake"}
	if g.node("vm", Device, Shared, "goal") == nil || len(g.problems) > 0 {
		t.Fatal(g.problems)
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	fake := filepath.Join(g.out, "device/libpal_fake/libpal-system.so")
	for _, want := range []string{
		"build " + filepath.Join(g.out, "device/vm/vm64") + ": link ",
		"build " + fake + ": link ",
		"-Wl,-soname,libpal-system.so",
	} {
		if !strings.Contains(ninja, want) {
			t.Errorf("ninja lacks %q", want)
		}
	}
	for _, n := range g.order {
		if n.name != "libpal" || n.kind != Shared {
			continue
		}
		libs, inputs := g.linkLibs(n)
		if slices.Contains(libs, fake) || slices.Contains(inputs, fake) {
			t.Errorf("runtime lib linked: %q", libs)
		}
		// device: no rpath at all (Soong: host only); the stage's LD_LIBRARY_PATH finds the runtime lib
		if hasRpath(libs) {
			t.Errorf("device link carries an rpath: %q", libs)
		}
	}

	// a stand-in that installs under another name is refused
	g = graphFor(t, map[string]string{"p/Android.bp": bp})
	g.stand = map[string]string{"libpal-system": "libplain"}
	g.node("libpal", Device, Shared, "goal")
	if !slices.ContainsFunc(g.problems, func(p string) bool {
		return strings.Contains(p, "module:libplain installs as libplain, not libpal-system")
	}) {
		t.Errorf("wrong stand-in accepted: %v", g.problems)
	}
}

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

// export_shared_lib_headers names a dependency as shared_libs does, variant tag included (ART libart-runtime_deps: `libdexfile#impl` in both): its exports reach dependents.
func TestReexportTaggedSharedLib(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_library { name: "libdex", export_include_dirs: ["inc"] }
cc_library { name: "librt", shared_libs: ["libdex#impl"], export_shared_lib_headers: ["libdex#impl"] }
`})
	n := g.node("librt", Device, Shared, "goal")
	if n == nil {
		t.Fatal(g.problems)
	}
	if want := "-I" + g.abs("p/inc"); !slices.Contains(n.exports, want) {
		t.Errorf("exports %q lack %s", n.exports, want)
	}
}

// Soong's static / shared properties are arch-variant (cc/library.go StaticProperties, SharedProperties): a target block's part merges into the module's own, after it, for that kind only.
func TestFlattenKindInTarget(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_library {
    name: "libk",
    cflags: ["-DBASE"],
    static: { cflags: ["-DSTATIC"] },
    target: { android: { static: { cflags: ["-DANDROID_STATIC"] }, shared: { cflags: ["-DANDROID_SHARED"] } } },
}
`})
	for kind, want := range map[Kind][]string{Static: {"-DBASE", "-DSTATIC", "-DANDROID_STATIC"}, Shared: {"-DBASE", "-DANDROID_SHARED"}} {
		p := g.flatten(g.lookup("libk", Device), Device, kind)
		if got := p.strs("cflags"); !slices.Equal(got, want) {
			t.Errorf("%s: cflags %q, want %q", kind, got, want)
		}
		if p.has("static") || p.has("shared") {
			t.Errorf("%s: static / shared blocks left in the properties", kind)
		}
	}
}

// A host-only goal (aapt2 is a cc_binary_host) builds its host variant: a device one would put a build tool and its closure under the licence gate.
func TestCcGoalSide(t *testing.T) {
	g := graphFor(t, map[string]string{
		"p/Android.bp": `cc_binary_host { name: "tool" }
cc_library { name: "lib" }
cc_library { name: "optout", device_supported: false }`,
	})
	for goal, want := range map[string]Side{"tool": Host, "lib": Device, "optout": Host} {
		if got := g.ccGoalSide(goal); got != want {
			t.Errorf("%s: side %v, want %v", goal, got, want)
		}
	}
}

// cc/linker.go linkerDeps (pinned build/soong 533c3c2, line 332): use_version_lib true appends libbuildversion to WholeStaticLibs; unset or false adds nothing.
func TestUseVersionLib(t *testing.T) {
	for _, c := range []struct {
		prop string
		want []string
	}{{"", nil}, {"use_version_lib: false,", nil}, {"use_version_lib: true,", []string{"libbuildversion"}}} {
		g := graphFor(t, map[string]string{"p/Android.bp": `
cc_binary { name: "adbd", srcs: ["a.c"], system_shared_libs: [], stl: "none", ` + c.prop + ` }
cc_library_static { name: "libbuildversion", srcs: ["a.c"], system_shared_libs: [], stl: "none" }
`})
		n := g.node("adbd", Device, Binary, "t")
		if n == nil {
			t.Fatalf("%q: no node, problems %q", c.prop, g.problems)
		}
		var got []string
		for _, d := range n.deps.whole {
			got = append(got, d.name)
		}
		if !slices.Equal(got, c.want) {
			t.Errorf("%q: whole %q, want %q", c.prop, got, c.want)
		}
	}
}
