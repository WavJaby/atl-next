// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// The flag tests run the compiled pinned Soong (android-16.0.0_r1, build/soong 533c3c2); expected values are literals of its source, cited per check, under this build's environment (no env var, no product).
// Break: a Soong value changed under the converter (go test -overlay with an edited cc/config file) turns these red.

// mustFlags: readGlobalFlags or the test stops.
func mustFlags(t *testing.T, cfg Config, dev DeviceArch) GlobalFlags {
	t.Helper()
	g, err := readGlobalFlags(cfg, dev)
	if err != nil {
		t.Fatal(err)
	}
	return g
}

// Expected tails: x86_64_device.go x86_64ArchVariantCflags[""] = -march=x86-64, ["haswell"] = -march=core-avx2, x86_64ArchFeatureCflags ssse3 = -mssse3, popcnt = -mpopcnt, after -m64 (X86_64ToolchainCflags), features in DeviceArch order.
// Heads: global.go commonGlobalCflags holds -D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__ and, AUTO_* env unset, -ftrivial-auto-var-init=zero (VariableFunc CommonGlobalCflags); deviceGlobalCflags -ffunction-sections (device only).
// Host: x86_linux_host.go's --gcc-toolchain=${LinuxGccRoot} and --sysroot ${LinuxGccRoot}/sysroot are dropped (toolchainLocation, listed in Skipped), and its -m64 kept.
func TestReadGlobalFlags(t *testing.T) {
	g := mustFlags(t, Config{Arch: "x86_64", PointerBits: 64}, DeviceArch{Features: []string{"ssse3", "popcnt"}})
	if want := []string{"-m64", "-march=x86-64", "-mssse3", "-mpopcnt"}; !slices.Equal(g.Tail[Device], want) {
		t.Errorf("device tail %q, want %q", g.Tail[Device], want)
	}
	for _, c := range []struct {
		name  string
		list  []string
		flags []string
		in    bool
	}{
		{"device head", g.Head[Device], []string{weakAvailability, "-ftrivial-auto-var-init=zero", "-ffunction-sections"}, true},
		{"host head", g.Head[Host], []string{weakAvailability, "-ftrivial-auto-var-init=zero", "-m64"}, true},
		{"host head", g.Head[Host], []string{"-ffunction-sections"}, false},
		{"device cpp", g.Cpp[Device], []string{"-fvisibility-inlines-hidden"}, true},
		{"device as", g.As[Device], []string{"-D__ASSEMBLY__"}, true},
		{"no override external", g.NoOverrideExt, []string{"-fcommon"}, true},
	} {
		for _, f := range c.flags {
			if slices.Contains(c.list, f) != c.in {
				t.Errorf("%s: %s present = %v", c.name, f, !c.in)
			}
		}
	}
	for _, f := range slices.Concat(g.Head[Device], g.Head[Host], g.Tail[Device], g.Cpp[Device], g.NoOverride) {
		if !keepFlag(f) || toolchainLocation(f) > 0 || strings.Contains(f, "$") {
			t.Errorf("flag %q passed", f)
		}
	}
	gcc := "prebuilts/gcc/linux-x86/host/x86_64-linux-glibc2.17-4.8"
	if !slices.Contains(g.Skipped, "--gcc-toolchain="+gcc) || !slices.Contains(g.Skipped, "--sysroot "+gcc+"/sysroot") {
		t.Errorf("skipped %q lacks the linux_glibc toolchain location", g.Skipped)
	}
	if h := mustFlags(t, Config{Arch: "x86_64", PointerBits: 64, ArchVariant: "haswell"}, DeviceArch{Variant: "haswell", Features: []string{"ssse3"}}); !slices.Equal(h.Tail[Device], []string{"-m64", "-march=core-avx2", "-mssse3"}) {
		t.Errorf("haswell tail %q", h.Tail[Device])
	}
	// x86_64's factory panics on a variant x86_64ArchVariantCflags lacks: an error, never a guess
	if _, err := readGlobalFlags(Config{Arch: "x86_64", PointerBits: 64, ArchVariant: "nehalem"}, DeviceArch{Variant: "nehalem"}); err == nil || !strings.Contains(err.Error(), "panics") {
		t.Errorf("unknown variant: err %v", err)
	}
}

// IsThirdPartyPath (android/paths.go): external/, vendor/, hardware/ unless a thirdPartyDirPrefixExceptions regexp (^hardware/interfaces/, ^hardware/ril/, ^vendor/[^/]*google[^/]*/, ...) matches; the module's dir gets the external lists.
func TestThirdParty(t *testing.T) {
	for dir, want := range map[string]bool{"external/zlib": true, "hardware/qcom/x": true, "vendor/acme": true, "hardware/interfaces/x": false, "hardware/ril": false, "vendor/google_x": false, "frameworks/base": false} {
		if thirdParty(dir) != want {
			t.Errorf("third party %s: %v", dir, !want)
		}
	}
	g := GlobalFlags{Head: map[Side][]string{Device: {"-O2"}}, Tail: map[Side][]string{Device: {"-m64"}}, External: []string{"-DEXT"}}
	if f := g.common(Device, "external/zlib"); !slices.Equal(f, []string{"-O2", "-DEXT", "-m64"}) {
		t.Errorf("third-party common %q", f)
	}
	if f := g.common(Device, "frameworks/base"); !slices.Equal(f, []string{"-O2", "-m64"}) {
		t.Errorf("first-party common %q", f)
	}
}

// The native bridge side builds with its guest row's variant (Soong's TARGET_NATIVE_BRIDGE_ARCH_VARIANT, arch.go:1730). arm64_device.go's factory panics on a variant arm64ArchVariantCflags lacks ("" included); x86_64's "" key gives its guest the device's flags.
func TestNativeBridgeArchVariant(t *testing.T) {
	nb := NativeBridge("arm64")
	cfg := Config{Arch: "x86_64", PointerBits: 64, NativeBridge: "arm64", NativeBridgeBits: 64, NativeBridgeArchVariant: "armv8-a"}
	if g, err := readGlobalFlags(cfg, DeviceArch{}); err != nil || g.NoToolchain[nb] != "" || !slices.Contains(g.Tail[nb], "-march=armv8-a") {
		t.Errorf("armv8-a: tail %q, no toolchain %q, err %v", g.Tail[nb], g.NoToolchain[nb], err)
	}
	cfg.NativeBridgeArchVariant = ""
	if _, err := readGlobalFlags(cfg, DeviceArch{}); err == nil || !strings.Contains(err.Error(), "panics") {
		t.Errorf("empty variant: err %v, want Soong's factory panic", err)
	}
	x86 := NativeBridge("x86_64")
	g := mustFlags(t, Config{Arch: "x86_64", PointerBits: 64, NativeBridge: "x86_64", NativeBridgeBits: 64}, DeviceArch{})
	if len(g.Tail[x86]) == 0 || !slices.Equal(g.Tail[x86], g.Tail[Device]) {
		t.Errorf("x86_64 guest: tail %q, device %q", g.Tail[x86], g.Tail[Device])
	}
	// ClangTriple (x86_64_device.go, arm64_device.go): bionic sides only
	if g.Triple[x86] != "x86_64-linux-android" || g.Triple[Device] != "" {
		t.Errorf("triples %q", g.Triple)
	}
}

// Expected yasm flags: x86_64_device.go X86_64YasmFlags and x86_linux_host.go LinuxX8664YasmFlags, both `-f elf64 -m amd64`; arm64: toolchainBase's "" on the device side, and no (linux_glibc, arm64) factory on the host.
func TestToolchainYasm(t *testing.T) {
	g := mustFlags(t, Config{Arch: "x86_64", PointerBits: 64}, DeviceArch{})
	want := []string{"-f", "elf64", "-m", "amd64"}
	if !slices.Equal(g.Yasm[Device], want) || !slices.Equal(g.Yasm[Host], want) || len(g.NoToolchain) != 0 {
		t.Errorf("x86_64: %q, none %q", g.Yasm, g.NoToolchain)
	}
	g = mustFlags(t, Config{Arch: "arm64", PointerBits: 64, ArchVariant: "armv8-a"}, DeviceArch{Variant: "armv8-a"})
	if len(g.Yasm[Device]) != 0 || g.NoToolchain[Device] != "" || !strings.Contains(g.NoToolchain[Host], "(linux_glibc, arm64)") {
		t.Errorf("arm64: %q, none %q", g.Yasm, g.NoToolchain)
	}
}

// Expected triples: rust/config x86_64_device.go and arm64_device.go RustTriple literals; only bionic sides take one: device and host are glibc here.
func TestReadRustTriples(t *testing.T) {
	for guest, want := range map[string]string{"x86_64": "x86_64-linux-android", "arm64": "aarch64-linux-android"} {
		nb := NativeBridge(guest)
		cfg := Config{Arch: "x86_64", PointerBits: 64, NativeBridge: guest, NativeBridgeBits: 64, NativeBridgeArchVariant: map[string]string{"arm64": "armv8-a"}[guest]}
		g := mustFlags(t, cfg, DeviceArch{})
		if len(g.RustTriple) != 1 || g.RustTriple[nb] != want {
			t.Errorf("guest %s: triples %v, want only %s", guest, g.RustTriple, want)
		}
	}
}

// probeCxxFlags = compileFlags' C++ argv for a module with nothing of its own: only the shared parts, in compileFlags' order, -D__ANDROID__ once (xtask check step libcxx drops it for its other run).
func TestProbeCxxFlags(t *testing.T) {
	g := graphFor(t, nil)
	g.soong.Flags = GlobalFlags{Head: map[Side][]string{Device: {"-O2"}}, Tail: map[Side][]string{Device: {"-m64"}}, Cpp: map[Side][]string{Device: {"-fvisibility-inlines-hidden"}}, NoOverride: []string{"-fno-x"}}
	g.soong.CppStd, g.soong.APILevel = "gnu++20", "-D__ANDROID_API__=10000"
	compat := filepath.Join(g.out, "compat")
	want := []string{"-nostdinc++", "-isystem", standInLibcxx, "-fPIC", "-w", "-O2", "-m64", "-fvisibility-inlines-hidden", "-Wno-error", "-std=gnu++20",
		androidDefine, lfs64Define, defaultSourceDefine, "-D__ANDROID_API__=10000", "-I" + filepath.Join(compat, "device"), "-include", "android/versioning.h", "-include", "android/api-level.h",
		"-I" + filepath.Join(compat, "cxx"), "-idirafter", filepath.Join(compat, "libcxx"), "-fno-rtti", "-I" + g.abs("art/runtime"), "-fno-x"}
	if got := g.probeCxxFlags("art/runtime"); !slices.Equal(got, want) {
		t.Errorf("probe argv\n %q\nwant\n %q", got, want)
	}
}

// Soong's command-line order (cc/builder.go): global flags, then the language's global list, the module's flags, includes, no-override last; asm without the module's cflags or the no-override flags.
func TestGlobalFlagsOrder(t *testing.T) {
	g := graphFor(t, map[string]string{"external/z/Android.bp": `cc_library { name: "libz", srcs: ["a.c", "b.cpp", "c.S"], cflags: ["-fno-function-sections"], cppflags: ["-fexceptions"] }`})
	g.soong.Flags = GlobalFlags{
		Head:          map[Side][]string{Device: {"-ffunction-sections", "-fno-exceptions"}},
		Tail:          map[Side][]string{Device: {"-m64"}},
		External:      []string{"-DEXT"},
		Cpp:           map[Side][]string{Device: {"-fvisibility-inlines-hidden"}},
		As:            map[Side][]string{Device: {"-D__ASSEMBLY__"}},
		NoOverrideExt: []string{"-fcommon"},
	}
	n := g.node("libz", Device, Shared, "t")
	if n == nil {
		t.Fatal(g.problems)
	}
	f := g.compileFlags(n)
	at := slices.Index[[]string]
	if !(at(f.c, "-ffunction-sections") < at(f.c, "-DEXT") && at(f.c, "-DEXT") < at(f.c, "-m64") && at(f.c, "-m64") < at(f.c, "-fno-function-sections")) || f.c[len(f.c)-1] != "-fcommon" {
		t.Errorf("c %q", f.c)
	}
	if !(at(f.cxx, "-fno-exceptions") < at(f.cxx, "-fvisibility-inlines-hidden") && at(f.cxx, "-fvisibility-inlines-hidden") < at(f.cxx, "-fexceptions")) || at(f.c, "-fvisibility-inlines-hidden") >= 0 {
		t.Errorf("cxx %q", f.cxx)
	}
	if at(f.asm, "-D__ASSEMBLY__") < 0 || at(f.asm, "-fno-function-sections") >= 0 || at(f.asm, "-fcommon") >= 0 {
		t.Errorf("asm %q", f.asm)
	}
}
