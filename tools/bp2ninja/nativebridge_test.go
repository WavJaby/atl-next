// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

var nbCfg = Config{Arch: "x86_64", PointerBits: 64, NativeBridge: "x86_64", NativeBridgeBits: 64}

// The native bridge side is a third side after device and host, evaluated from the module's android properties.
func TestNativeBridgeSides(t *testing.T) {
	nb := NativeBridge("x86_64")
	if got := nbCfg.sides(); !slices.Equal(got, []Side{Device, Host, nb}) {
		t.Errorf("sides %v", got)
	}
	if got := (Config{Arch: "x86_64"}).sides(); !slices.Equal(got, []Side{Device, Host}) {
		t.Errorf("sides without -native-bridge %v", got)
	}
	if nb.String() != "nb/x86_64" || !nb.android() || nb.glibc() || nb.evalOS() != "android" {
		t.Errorf("nb side %s android %v glibc %v os %s", nb, nb.android(), nb.glibc(), nb.evalOS())
	}
	// Soong applies target.native_bridge after the OS+arch blocks (android/arch.go getArchProperties)
	if b := osArchBlocks(nb, "x86_64"); b[len(b)-1] != "native_bridge" || slices.Contains(osArchBlocks(Device, "x86_64"), "native_bridge") {
		t.Errorf("nb blocks %v", b)
	}
}

// target.native_bridge reaches the native bridge variant only, last; providers.toml rows replace modules on glibc sides only.
func TestNativeBridgeNode(t *testing.T) {
	g := graphForCfg(t, nbCfg, map[string]string{"p/Android.bp": `
cc_library {
    name: "libz",
    srcs: ["a.c"],
    shared_libs: ["libdep"],
    system_shared_libs: [],
    target: { android: { cflags: ["-DANDROID"] }, android_x86_64: { cflags: ["-DOSARCH"] }, native_bridge: { cflags: ["-DNB"] } },
}
cc_library { name: "libdep", srcs: ["b.c"], system_shared_libs: [] }
`})
	nb := NativeBridge("x86_64")
	g.soong.APILevel = apiLevelPrefix + "10000"
	g.soong.Flags.Triple[nb] = "x86_64-linux-android"
	g.provided = map[string]Provided{"libdep": {Libs: []string{"/sys/libdep.so"}}}
	g.nbProvided = map[string]Provided{}
	dev, n := g.node("libz", Device, Shared, "t"), g.node("libz", nb, Shared, "t")
	if dev == nil || n == nil {
		t.Fatalf("nodes %v %v, problems %q", dev, n, g.problems)
	}
	if !dev.deps.shared[0].provided || n.deps.shared[0].provided {
		t.Errorf("libdep: device provided %v, nb provided %v (want tree module)", dev.deps.shared[0].provided, n.deps.shared[0].provided)
	}
	fl := g.compileFlags(n).c
	osArch, nbFlag := slices.Index(fl, "-DOSARCH"), slices.Index(fl, "-DNB")
	if slices.Index(fl, "-DANDROID") < 0 || osArch < 0 || nbFlag < osArch {
		t.Errorf("nb cflags %q: want -DANDROID, then -DOSARCH, -DNB last", fl)
	}
	if slices.Contains(g.compileFlags(dev).c, "-DNB") {
		t.Error("device variant got target.native_bridge")
	}
	for _, w := range []string{"x86_64-linux-android10000", "-nostdlibinc", "-D__ANDROID_NATIVE_BRIDGE__"} {
		if !slices.Contains(fl, w) {
			t.Errorf("nb cflags lack %s: %q", w, fl)
		}
	}
	// glibc compat (LFS64, compat dirs, forced api-level headers) is the device side's
	if slices.Contains(fl, lfs64Define) || slices.Contains(fl, "-include") {
		t.Errorf("nb cflags carry glibc compat: %q", fl)
	}
	libs, _ := g.linkLibs(n)
	if slices.Contains(libs, "-lpthread") {
		t.Errorf("nb link libs take glibc's: %q", libs)
	}
	if ld := g.linkFlags(n); !slices.Contains(ld, "-nostdlib") || !slices.Contains(ld, "-Wl,--pack-dyn-relocs=android+relr") {
		t.Errorf("nb link flags %q", ld)
	}
	if out := g.nodeOut(n); !strings.HasPrefix(out, filepath.Join(g.out, "nb", "x86_64")+"/") {
		t.Errorf("nb out %s", out)
	}
}

// A bionic side takes its provided modules from -nb-provided, not providers.toml.
func TestNativeBridgeProvided(t *testing.T) {
	g := graphForCfg(t, nbCfg, map[string]string{"p/Android.bp": `cc_library { name: "libz", srcs: ["a.c"], static_libs: ["libclang_rt.builtins"], system_shared_libs: [] }`})
	nb := NativeBridge("x86_64")
	g.provided = map[string]Provided{"libclang_rt.builtins": {Libs: []string{"/glibc/builtins.a"}}}
	g.nbProvided = map[string]Provided{"libclang_rt.builtins": {Libs: []string{"/rt/x86_64/libclang_rt.builtins.a"}}}
	n := g.node("libz", nb, Shared, "t")
	if n == nil || len(n.deps.static) != 1 || n.deps.static[0].libs[0] != "/rt/x86_64/libclang_rt.builtins.a" {
		t.Fatalf("nb static deps %+v, problems %q", n, g.problems)
	}
}
