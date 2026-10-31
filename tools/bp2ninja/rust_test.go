// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// The Soong shape of a Rust FFI library in a C++ library (minikin, libinput): cc_library_shared -> rust_ffi_static -> rust_library -> rust_proc_macro, the rlib carrying a cc whole_static_libs (libcxx's libcxxbridge05),
// and a genrule whose tool is a rust_binary_host (cxxbridge).
func rustGraph(t *testing.T) (*Graph, string) {
	t.Helper()
	g := graphFor(t, map[string]string{"p/Android.bp": `
rust_defaults { name: "ffi_defaults", edition: "2018", features: ["capi"], cfgs: ["soong_build"] }
rust_ffi_static {
    name: "libffi",
    crate_name: "ffi",
    defaults: ["ffi_defaults"],
    srcs: ["src/lib.rs"],
    rustlibs: ["libdep", "libstd"],
    whole_static_libs: ["libcc"],
    export_include_dirs: ["include"],
}
rust_library { name: "libdep", crate_name: "dep", srcs: ["dep.rs"], proc_macros: ["libmac"], aliases: ["mac:mac2"], host_supported: true }
rust_proc_macro { name: "libmac", crate_name: "mac", srcs: ["mac.rs"] }
cc_library_static { name: "libcc", srcs: ["cc.cc"] }
cc_library_shared { name: "libuser", srcs: ["u.cc"], static_libs: ["libffi"], generated_headers: ["bridge_h"] }
rust_binary_host { name: "tool", srcs: ["main.rs"], rustlibs: ["libdep"], cargo_env_compat: true, cargo_pkg_version: "1.2.3" }
genrule { name: "bridge_h", tools: ["tool"], cmd: "$(location tool) --header > $(out)", out: ["b.h"] }
`})
	if g.node("libuser", Device, Shared, "goal") == nil || len(g.problems) > 0 {
		t.Fatal(g.problems)
	}
	return g, g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar", Rustc: "rustc", RustVersion: "1.2.3"})
}

// Expected flags: rust/compiler.go (features as `feature="x"` cfgs, cfgs, edition, `-C metadata=<module>` for libraries, `--extern proc_macro` for proc macros), rust/config/global.go (device panic=abort), rust/builder.go (--extern per direct crate, -L dependency= per crate dir).
func TestRustCrateFlags(t *testing.T) {
	g, ninja := rustGraph(t)
	out := g.out
	words := func(v string) []string { return shWords(t, v) }
	ffi := ninjaEdgeVars(ninja, filepath.Join(out, "device/libffi/libffi.rlib"))
	if ffi == nil {
		t.Fatal("no rustc step for libffi")
	}
	flags := words(ffi["flags"])
	for _, want := range [][]string{
		{"--crate-type=rlib"}, {"--crate-name=ffi"}, {"--edition=2018"}, {"--cfg", `feature="capi"`}, {"--cfg", "soong_build"}, {"-C", "metadata=libffi"}, {"-C", "panic=abort"},
	} {
		if !containsSeq(flags, want) {
			t.Errorf("libffi flags %q lack %q", flags, want)
		}
	}
	// libstd is the sysroot's: no --extern for it
	if libs := words(ffi["libs"]); !slices.Equal(libs, []string{"--extern", "dep=" + filepath.Join(out, "device/libdep/libdep.rlib"),
		"-L", "dependency=" + filepath.Join(out, "device/libdep"), "-L", "dependency=" + filepath.Join(out, "host/libmac")}) {
		t.Errorf("libffi libs %q", libs)
	}
	// the proc macro is a host crate under its alias; the host side keeps unwinding
	dep := words(ninjaEdgeVars(ninja, filepath.Join(out, "device/libdep/libdep.rlib"))["libs"])
	if !containsSeq(dep, []string{"--extern", "mac2=" + filepath.Join(out, "host/libmac/libmac.so")}) {
		t.Errorf("libdep libs %q", dep)
	}
	mac := words(ninjaEdgeVars(ninja, filepath.Join(out, "host/libmac/libmac.so"))["flags"])
	if !containsSeq(mac, []string{"--extern", "proc_macro"}) || !containsSeq(mac, []string{"--crate-type=proc-macro"}) || slices.Contains(mac, "panic=abort") {
		t.Errorf("libmac flags %q", mac)
	}
	// builder.go rustEnvVars with cargo_env_compat: CARGO_BIN_NAME for a binary, the version split at its dots
	tool := ninjaEdgeVars(ninja, filepath.Join(out, "host/tool/tool"))
	if env := words(tool["env"]); !slices.Equal(env, []string{"OUT_DIR=out", "ANDROID_RUST_VERSION=1.2.3", "RUSTC_BOOTSTRAP=-1", "CARGO_BIN_NAME=tool", "CARGO_CRATE_NAME=tool", "CARGO_PKG_NAME=tool",
		"CARGO_PKG_VERSION=1.2.3", "CARGO_PKG_VERSION_MAJOR=1", "CARGO_PKG_VERSION_MINOR=2", "CARGO_PKG_VERSION_PATCH=3"}) {
		t.Errorf("tool env %q", env)
	}
	if f := words(tool["flags"]); !containsSeq(f, []string{"--crate-type=bin"}) || !containsSeq(f, []string{"-C", "linker=c++"}) {
		t.Errorf("tool flags %q", f)
	}
	// the genrule runs the binary rustc writes
	if s := g.genFiles[filepath.Join(out, "gen/device/bridge_h/genrule.sh")]; !strings.HasPrefix(s, filepath.Join(out, "host/tool/tool")+" --header > ") {
		t.Errorf("genrule script %q", s)
	}
}

// cc/builder.go GenerateRustStaticlib: one staticlib per final link, `#![no_std]` + `extern crate` per rlib its cc modules name; the rlibs' cc libraries link as static closure members, the rlibs themselves never reach the linker.
func TestRustStaticlibInFinalLink(t *testing.T) {
	g, ninja := rustGraph(t)
	out := g.out
	lib := filepath.Join(out, "device/libuser/generated_rust_staticlib/librustlibs.a")
	src := strings.TrimSuffix(lib, ".a") + ".rs"
	body := g.genFiles[src]
	if lines := strings.Split(strings.TrimSpace(body), "\n"); len(lines) < 2 || !slices.Equal(lines[1:], []string{"#![no_std]", "extern crate ffi;"}) {
		t.Errorf("staticlib root %q", body)
	}
	st := ninjaEdgeVars(ninja, lib)
	if st == nil {
		t.Fatal("no staticlib step")
	}
	if f := shWords(t, st["flags"]); !containsSeq(f, []string{"--crate-type=staticlib"}) || !containsSeq(f, []string{"-C", "panic=abort"}) {
		t.Errorf("staticlib flags %q", f)
	}
	if !strings.Contains(st[""], filepath.Join(out, "device/libdep/libdep.rlib")) {
		t.Errorf("staticlib step does not wait for the transitive crate: %s", st[""])
	}
	link := shWords(t, ninjaEdgeVars(ninja, filepath.Join(out, "device/libuser/libuser.so"))["libs"])
	if !slices.Contains(link, lib) || !slices.Contains(link, filepath.Join(out, "device/libcc/libcc.a")) {
		t.Errorf("libuser link %q lacks the staticlib or the rlib's cc library", link)
	}
	if slices.ContainsFunc(link, func(s string) bool { return strings.HasSuffix(s, ".rlib") }) {
		t.Errorf("libuser link %q names an rlib", link)
	}
	// the rlib's export_include_dirs reach the C++ compile
	cc := ninjaEdgeVars(ninja, filepath.Join(out, "obj/device/libuser"))
	if !slices.Contains(shWords(t, cc["flags"]), "-I"+filepath.Join(g.tree, "p/include")) {
		t.Errorf("libuser compile flags lack libffi's include dir: %s", cc["flags"])
	}
}

// rust/builder.go: generated srcs are copied into the module's OUT_DIR, which the crate reads through env!("OUT_DIR"); without any, OUT_DIR=out. The plain source stays the crate root.
func TestRustGeneratedSources(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
genrule { name: "gen_rs", cmd: "echo > $(out)", out: ["flags.rs"] }
rust_library { name: "libgen", crate_name: "gen", srcs: ["lib.rs", ":gen_rs"] }
`})
	if g.node("libgen", Device, Rlib, "goal") == nil || len(g.problems) > 0 {
		t.Fatal(g.problems)
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar", Rustc: "rustc"})
	outDir := filepath.Join(g.out, "obj/device/libgen/out")
	cp := ninjaEdgeVars(ninja, filepath.Join(outDir, "flags.rs"))
	if cp == nil || !strings.Contains(cp[""], ": cp "+filepath.Join(g.out, "gen/device/gen_rs/flags.rs")) {
		t.Fatalf("copy step %v", cp)
	}
	rs := ninjaEdgeVars(ninja, filepath.Join(g.out, "device/libgen/libgen.rlib"))
	if !strings.Contains(rs[""], "rustc "+filepath.Join(g.tree, "p/lib.rs")) || !strings.Contains(rs[""], filepath.Join(outDir, "flags.rs")) {
		t.Errorf("rustc step %s", rs[""])
	}
	if env := shWords(t, rs["env"]); env[0] != "OUT_DIR="+outDir {
		t.Errorf("env %q", env)
	}
}

// soongRustBuilder / soongPaths: the pinned android-16.0.0_r1 shapes readRustFeatureGate reads (rust/builder.go transformSrctoCrate's -Zallow-features condition; android/paths.go IsThirdPartyPath and its exceptions), trimmed to those statements.
const soongRustBuilder = `package rust

func transformSrctoCrate(ctx ModuleContext) {
	// Disallow experimental features
	modulePath := ctx.ModuleDir()
	if !(android.IsThirdPartyPath(modulePath) || strings.HasPrefix(modulePath, "prebuilts")) {
		rustcFlags = append(rustcFlags, "-Zallow-features=\"\"")
	}
}
`

const soongPaths = `package android

var thirdPartyDirPrefixExceptions = []*regexp.Regexp{
	regexp.MustCompile("^vendor/[^/]*google[^/]*/"),
	regexp.MustCompile("^hardware/google/"),
	regexp.MustCompile("^hardware/interfaces/"),
	regexp.MustCompile("^hardware/libhardware[^/]*/"),
	regexp.MustCompile("^hardware/ril/"),
}

func IsThirdPartyPath(path string) bool {
	thirdPartyDirPrefixes := []string{"external/", "vendor/", "hardware/"}

	if HasAnyPrefix(path, thirdPartyDirPrefixes) {
		for _, prefix := range thirdPartyDirPrefixExceptions {
			if prefix.MatchString(path) {
				return false
			}
		}
		return true
	}
	return false
}
`

func writeTree(t *testing.T, files map[string]string) string {
	t.Helper()
	tree := t.TempDir()
	for p, text := range files {
		full := filepath.Join(tree, p)
		if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(full, []byte(text), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	return tree
}

func soongRustGate(t *testing.T) RustFeatureGate {
	t.Helper()
	gate, err := readRustFeatureGate(writeTree(t, map[string]string{"build/soong/rust/builder.go": soongRustBuilder, "build/soong/android/paths.go": soongPaths}))
	if err != nil {
		t.Fatal(err)
	}
	return gate
}

// The gate is Soong's: third-party = an IsThirdPartyPath prefix not matching an exception, or the condition's own prebuilts prefix; everything else is first-party.
func TestRustFeatureGate(t *testing.T) {
	gate := soongRustGate(t)
	for dir, want := range map[string]string{
		"external/rust/android-crates-io/crates/proc-macro2": "1",
		"vendor/acme/lib":           "1",
		"hardware/acme/hal":         "1",
		"prebuilts/rust/linux-x86":  "1",
		"hardware/interfaces/audio": "-1", // an exception
		"vendor/google_devices/x":   "-1", // an exception
		"frameworks/base/libs":      "-1",
		"system/librustutils":       "-1",
		"external":                  "-1", // HasAnyPrefix takes "external/"
	} {
		if got := gate.bootstrap(dir); got != want {
			t.Errorf("bootstrap(%s) = %s, want %s", dir, got, want)
		}
	}
	// a Soong whose condition changed shape is reported, not guessed
	for name, builder := range map[string]string{
		"no IsThirdPartyPath": strings.Replace(soongRustBuilder, "android.IsThirdPartyPath(modulePath) || ", "", 1),
		"unknown call":        strings.Replace(soongRustBuilder, "android.IsThirdPartyPath(modulePath)", "android.IsVendor(modulePath)", 1),
		"not negated":         strings.Replace(soongRustBuilder, "if !(", "if (", 1),
	} {
		if _, err := readRustFeatureGate(writeTree(t, map[string]string{"build/soong/rust/builder.go": builder, "build/soong/android/paths.go": soongPaths})); err == nil {
			t.Errorf("%s: no error", name)
		}
	}
	if _, err := readRustFeatureGate(writeTree(t, map[string]string{"build/soong/rust/builder.go": soongRustBuilder,
		"build/soong/android/paths.go": strings.Replace(soongPaths, "thirdPartyDirPrefixes :=", "dirs :=", 1)})); err == nil {
		t.Error("renamed thirdPartyDirPrefixes: no error")
	}
}

// RUSTC_BOOTSTRAP is per rustc edge: 1 for a third-party crate, -1 for a first-party one (stable whatever the caller's environment), and rustc honours both.
func TestRustBootstrapPerCrate(t *testing.T) {
	g := graphFor(t, map[string]string{
		"external/crates/pm2/Android.bp": `rust_library_host { name: "libpm2", crate_name: "pm2", srcs: ["lib.rs"] }`,
		"system/first/Android.bp":        `rust_library_host { name: "libfirst", crate_name: "first", srcs: ["lib.rs"], rustlibs: ["libpm2"] }`,
	})
	g.soong.RustFeatures = soongRustGate(t)
	if g.node("libfirst", Host, Rlib, "goal") == nil || len(g.problems) > 0 {
		t.Fatal(g.problems)
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar", Rustc: "rustc"})
	for mod, want := range map[string]string{"libpm2/libpm2.rlib": "RUSTC_BOOTSTRAP=1", "libfirst/libfirst.rlib": "RUSTC_BOOTSTRAP=-1"} {
		if env := shWords(t, ninjaEdgeVars(ninja, filepath.Join(g.out, "host", mod))["env"]); !slices.Contains(env, want) {
			t.Errorf("%s env %q lacks %s", mod, env, want)
		}
	}
	if strings.Contains(ninja[:strings.Index(ninja, "\nbuild ")], "RUSTC_BOOTSTRAP") {
		t.Error("RUSTC_BOOTSTRAP set outside the rustc edges")
	}
	rustc, err := exec.LookPath("rustc")
	if err != nil {
		t.Skip("no rustc: the env values are checked, not their effect")
	}
	src := filepath.Join(t.TempDir(), "f.rs")
	if err := os.WriteFile(src, []byte("#![feature(rustc_attrs)]\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	for v, ok := range map[string]bool{"1": true, "-1": false} {
		cmd := exec.Command(rustc, "--crate-type=lib", "--emit=metadata", "--out-dir", filepath.Dir(src), src)
		cmd.Env = append(os.Environ(), "RUSTC_BOOTSTRAP="+v)
		if out, err := cmd.CombinedOutput(); (err == nil) != ok {
			t.Errorf("RUSTC_BOOTSTRAP=%s: #![feature] compiled = %v, want %v: %s", v, err == nil, ok, out)
		}
	}
}

// A Rust device crate takes the linux_glibc OS blocks (rustc's target_os), a cc device module the android ones; the crate keeps device codegen flags.
func TestRustDeviceOSBlocks(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
rust_library {
    name: "liblog_like",
    crate_name: "log_like",
    srcs: ["lib.rs"],
    target: {
        android: { rustlibs: ["libandroid_only"] },
        linux_glibc: { rustlibs: ["libglibc_only"] },
        linux_glibc_x86_64: { cfgs: ["glibc_x86_64"] },
        android_x86_64: { cfgs: ["android_x86_64"] },
    },
}
rust_library { name: "libandroid_only", crate_name: "android_only", srcs: ["a.rs"] }
rust_library { name: "libglibc_only", crate_name: "glibc_only", srcs: ["g.rs"] }
cc_library_static {
    name: "libcc_like",
    srcs: ["c.cc"],
    target: { android: { cflags: ["-DANDROID_BLOCK"] }, linux_glibc: { cflags: ["-DGLIBC_BLOCK"] } },
}
`})
	if g.node("liblog_like", Device, Rlib, "goal") == nil || g.node("libcc_like", Device, Static, "goal") == nil || len(g.problems) > 0 {
		t.Fatal(g.problems)
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar", Rustc: "rustc"})
	rs := ninjaEdgeVars(ninja, filepath.Join(g.out, "device/liblog_like/liblog_like.rlib"))
	libs, flags := shWords(t, rs["libs"]), shWords(t, rs["flags"])
	if !containsSeq(libs, []string{"--extern", "glibc_only=" + filepath.Join(g.out, "device/libglibc_only/libglibc_only.rlib")}) || slices.ContainsFunc(libs, func(s string) bool { return strings.Contains(s, "android_only") }) {
		t.Errorf("liblog_like libs %q: want the linux_glibc block's crate only", libs)
	}
	if !containsSeq(flags, []string{"--cfg", "glibc_x86_64"}) || slices.Contains(flags, "android_x86_64") || !containsSeq(flags, []string{"-C", "panic=abort"}) {
		t.Errorf("liblog_like flags %q: want linux_glibc_x86_64 cfgs and device panic=abort", flags)
	}
	cc := shWords(t, ninjaEdgeVars(ninja, filepath.Join(g.out, "obj/device/libcc_like"))["flags"])
	if !slices.Contains(cc, "-DANDROID_BLOCK") || slices.Contains(cc, "-DGLIBC_BLOCK") {
		t.Errorf("libcc_like flags %q: want the android block only", cc)
	}
}

// containsSeq: want appears in list as consecutive elements.
func containsSeq(list, want []string) bool {
	for i := 0; i+len(want) <= len(list); i++ {
		if slices.Equal(list[i:i+len(want)], want) {
			return true
		}
	}
	return false
}
