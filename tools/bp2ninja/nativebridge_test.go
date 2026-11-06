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
	g.provided = map[string]Provided{"libdep": {Lib: "/sys/libdep.so"}}
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
	g := graphForCfg(t, nbCfg, map[string]string{"p/Android.bp": `cc_library { name: "libz", srcs: ["a.c"], static_libs: ["libclang_rt.builtins"], system_shared_libs: [], nocrt: true }`})
	nb := NativeBridge("x86_64")
	g.provided = map[string]Provided{"libclang_rt.builtins": {Lib: "/glibc/builtins.a"}}
	g.nbProvided = map[string]Provided{"libclang_rt.builtins": {Lib: "/rt/x86_64/libclang_rt.builtins.a"}}
	n := g.node("libz", nb, Shared, "t")
	if n == nil || len(n.deps.static) != 1 || n.deps.static[0].out != "/rt/x86_64/libclang_rt.builtins.a" {
		t.Fatalf("nb static deps %+v, problems %q", n, g.problems)
	}
}

// Soong's implicit bionic link deps (bionic.go): crt objects around a shared library's link, builtins last, a static-PIE binary with -shared + prefix_symbols, a cc_object's objs by ld -r.
func TestBionicLinkDeps(t *testing.T) {
	g := graphForCfg(t, nbCfg, map[string]string{"p/Android.bp": `
cc_defaults { name: "crt", system_shared_libs: [], stl: "none" }
cc_object { name: "crtbrand", srcs: ["brand.S"], defaults: ["crt"] }
cc_object { name: "crtbegin_so", srcs: ["begin_so.c"], objs: ["crtbrand"], defaults: ["crt"] }
cc_object { name: "crtend_so", srcs: ["end_so.S"], defaults: ["crt"] }
cc_object { name: "crt_pad_segment", srcs: ["pad.S"], defaults: ["crt"] }
cc_library { name: "libz", srcs: ["a.c"], system_shared_libs: [], stl: "none" }
cc_binary {
    name: "linker",
    srcs: ["l.c"],
    static_executable: true,
    ldflags: ["-shared"],
    nocrt: true,
    prefix_symbols: "__dl_",
    static_libs: ["libz"],
    system_shared_libs: [],
    stl: "none",
}
`})
	nb := NativeBridge("x86_64")
	g.soong.Flags.Triple[nb] = "x86_64-linux-android"
	g.nbProvided = map[string]Provided{builtinsModule: {Libs: []string{"/rt/builtins.a"}}}
	lib, bin := g.node("libz", nb, Shared, "goal"), g.node("linker", nb, Binary, "goal")
	if lib == nil || bin == nil || len(g.problems) > 0 {
		t.Fatalf("nodes %v %v, problems %q", lib, bin, g.problems)
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar", LLVM: "/llvm"})
	obj := func(name string) string { return filepath.Join(g.out, "nb", "x86_64", name, name+".o") }
	for _, w := range []string{
		"crtbegin = " + obj("crtbegin_so") + "\n",
		"crtend = " + obj("crtend_so") + " " + obj("crt_pad_segment") + "\n",
		": partial_ld ",
		"build " + obj("crtbegin_so") + ": no_addrsig ",
		"objcopy = /llvm/bin/llvm-objcopy\n",
		": prefix_symbols ",
		"prefix = __dl_\n",
		"-Bstatic",
	} {
		if !strings.Contains(ninja, w) {
			t.Errorf("ninja lacks %q", w)
		}
	}
	if libs, _ := g.linkLibs(lib); !slices.Contains(libs, "/rt/builtins.a") {
		t.Errorf("libz link libs lack builtins: %q", libs)
	}
	ld := g.linkFlags(bin)
	if slices.Contains(ld, "-static") || !slices.Contains(ld, "-shared") || bin.crtBegin != nil {
		t.Errorf("linker: -shared replaces -static, nocrt drops crt: %q, crt %v", ld, bin.crtBegin)
	}
}

// -bionic-static-rust: a static bionic binary's Rust staticlib carries the guest dlsym crate (rustup std references dlsym; crates/atl-guest-dlsym); a shared library's does not. A cc_object is a graph.json "object" artifact (xtask runtimes pass 2 links crtbegin_so).
func TestBionicStaticRust(t *testing.T) {
	g := graphForCfg(t, nbCfg, map[string]string{"p/Android.bp": `
rust_ffi_static { name: "libffi", crate_name: "ffi", srcs: ["lib.rs"] }
cc_object { name: "crtbegin_so", srcs: ["begin_so.c"], system_shared_libs: [], stl: "none" }
cc_binary { name: "linker", srcs: ["l.c"], static_executable: true, static_libs: ["libffi"], system_shared_libs: [], stl: "none" }
cc_library_shared { name: "libuser", srcs: ["u.c"], static_libs: ["libffi"], system_shared_libs: [], stl: "none" }
`})
	nb := NativeBridge("x86_64")
	g.soong.Flags.Triple[nb] = "x86_64-linux-android"
	g.soong.Flags.RustTriple[nb] = "x86_64-linux-android"
	g.bionicStaticRust = "/ws/crates/atl-guest-dlsym/src/lib.rs"
	bin, lib, obj := g.node("linker", nb, Binary, "goal"), g.node("libuser", nb, Shared, "goal"), g.node("crtbegin_so", nb, Static, "goal")
	if bin == nil || lib == nil || obj == nil {
		t.Fatalf("nodes %v %v %v, problems %q", bin, lib, obj, g.problems)
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar", LLVM: "/llvm"})
	dir := func(m string) string { return filepath.Join(g.out, "nb/x86_64", m, "generated_rust_staticlib") }
	rlib := filepath.Join(dir("linker"), "libatl_guest_dlsym.rlib")
	if e := ninjaEdgeVars(ninja, rlib); e == nil || !strings.Contains(e[""], g.bionicStaticRust) || !strings.Contains(e["flags"], "--crate-name=atl_guest_dlsym") {
		t.Errorf("dlsym rlib step %v", e)
	}
	if body := g.genFiles[filepath.Join(dir("linker"), "librustlibs.rs")]; !strings.Contains(body, "extern crate atl_guest_dlsym;") {
		t.Errorf("linker staticlib root %q", body)
	}
	if body := g.genFiles[filepath.Join(dir("libuser"), "librustlibs.rs")]; strings.Contains(body, "atl_guest_dlsym") {
		t.Errorf("shared library staticlib root %q", body)
	}
	if a, ok := g.artifactOf(obj); !ok || a.Kind != "object" || a.Out != filepath.Join(g.out, "nb/x86_64/crtbegin_so/crtbegin_so.o") {
		t.Errorf("crtbegin_so artifact %v %v", a, ok)
	}
}
