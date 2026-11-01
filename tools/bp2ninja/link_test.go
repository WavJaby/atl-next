// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"testing"
)

// soongDeviceLink: what readDeviceLink takes from the pinned android-16.0.0_r1 Soong (build/soong 533c3c2): cc/config/global.go deviceGlobalLdflags (191-204) -> its four --exclude-libs, then library.go:502 / binary.go:278,316's --gc-sections.
var soongDeviceLink = DeviceLink{
	Ldflags: []string{"-Wl,--exclude-libs,libgcc.a", "-Wl,--exclude-libs,libgcc_stripped.a", "-Wl,--exclude-libs,libunwind_llvm.a",
		"-Wl,--exclude-libs,libunwind.a", "-Wl,--gc-sections"},
}

// soongLldflags: what readLldflags takes from the same Soong: global.go commonGlobalLldflags (lines 180-184), whole.
var soongLldflags = []string{"-fuse-ld=lld", "-Wl,--icf=safe", "-Wl,--no-demangle"}

// linkTree writes the Soong files readDeviceLink and readLldflags read, shaped like the pinned ones (global.go's var block, the literal inside library.go's / binary.go's `if toolchain.Bionic()`); binary = binary.go's body, lld = global.go's commonGlobalLldflags spec.
func linkTree(t *testing.T, binary, lld string) string {
	tree := t.TempDir()
	files := map[string]string{
		"build/soong/cc/config/global.go": "package config\n\nvar (\n\tdeviceGlobalCflags = []string{\n\t\t\"-ffunction-sections\",\n\t\t\"-fdata-sections\",\n\t\t\"-fno-short-enums\",\n\t\t\"-funwind-tables\",\n\t\t\"-fstack-protector-strong\",\n\t\t\"-Wa,--noexecstack\",\n\t\t\"-D_FORTIFY_SOURCE=2\",\n\t\t\"-Werror=return-type\",\n\t}\n\n" +
			"\tdeviceGlobalCppflags = []string{\n\t\t\"-fvisibility-inlines-hidden\",\n\t}\n\n" +
			"\tdeviceGlobalLdflags = []string{\n\t\t\"-Wl,-z,noexecstack\",\n\t\t\"-Wl,-z,relro\",\n\t\t\"-Wl,--no-undefined-version\",\n\t\t\"-Wl,--exclude-libs,libgcc.a\",\n\t\t\"-Wl,--exclude-libs,libgcc_stripped.a\",\n\t\t\"-Wl,--exclude-libs,libunwind_llvm.a\",\n\t\t\"-Wl,--exclude-libs,libunwind.a\",\n\t}\n\n" + lld + ")\n",
		"build/soong/cc/library.go": "package cc\n\nfunc f(bionic bool) (out []string) {\n\tif bionic {\n\t\tout = append(out, \"-nostdlib\", \"-Wl,--gc-sections\")\n\t}\n\treturn\n}\n",
		"build/soong/cc/binary.go":  "package cc\n\n" + binary,
	}
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

// pinnedLld: global.go's commonGlobalLldflags spec as the pinned Soong writes it.
const pinnedLld = "\tcommonGlobalLldflags = []string{\n\t\t\"-fuse-ld=lld\",\n\t\t\"-Wl,--icf=safe\",\n\t\t\"-Wl,--no-demangle\",\n\t}\n"

func TestReadLldflags(t *testing.T) {
	const binary = "func g() []string { return nil }\n"
	got, err := readLldflags(linkTree(t, binary, pinnedLld))
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(got, soongLldflags) {
		t.Errorf("Lldflags %q, want %q", got, soongLldflags)
	}
	// Soong stops linking with lld: the converter's linker choice and trace format go with it, so it stops
	if _, err := readLldflags(linkTree(t, binary, "\tcommonGlobalLldflags = []string{\"-Wl,--icf=safe\"}\n")); err == nil || !strings.Contains(err.Error(), "lacks -fuse-ld=lld") {
		t.Errorf("no -fuse-ld=lld: err %v", err)
	}
}

func TestReadDeviceLink(t *testing.T) {
	d, err := readDeviceLink(linkTree(t, "func g() []string { return []string{\"-pie\", \"-Wl,--gc-sections\"} }\n", pinnedLld))
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(d.Ldflags, soongDeviceLink.Ldflags) {
		t.Errorf("Ldflags %q, want %q", d.Ldflags, soongDeviceLink.Ldflags)
	}
	// Soong stops garbage-collecting binaries: the assumption is gone, the converter stops
	if _, err := readDeviceLink(linkTree(t, "func g() []string { return []string{\"-pie\"} }\n", pinnedLld)); err == nil || !strings.Contains(err.Error(), "binary.go: no") {
		t.Errorf("binary.go without --gc-sections: err %v", err)
	}
}

// linkEdge: the mode and implicit inputs of out's link edge in a ninja file.
func linkEdge(t *testing.T, ninja, out string) (mode, implicit string) {
	m := regexp.MustCompile(`(?m)^build ` + regexp.QuoteMeta(esc(out)) + `: link [^|\n]*\| (.*)\n  mode = (.*)\n`).FindStringSubmatch(ninja)
	if m == nil {
		t.Fatalf("no link edge for %s", out)
	}
	return m[2], m[1]
}

// Expected from cc/linker.go linkerFlags: -Wl,--version-script,<file> on shared libraries and binaries (on host too: only darwin lacks it), the file a dependency of the link (LdFlagsDeps), after target blocks; none on an archive.
// DeviceLink's flags on the device side only (cc/library.go, cc/binary.go: `if toolchain.Bionic()`; global.go device* lists); Lldflags on both (CommonLinkerFlags' ${config.<Device|Host>GlobalLldflags}).
func TestVersionScriptLink(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_library {
    name: "libv",
    host_supported: true,
    srcs: ["v.cc"],
    target: { android: { version_script: "v.map" }, host: { version_script: "host.map" } },
}
cc_binary { name: "bin", srcs: ["v.cc"], version_script: ":gen_map" }
genrule { name: "gen_map", cmd: "echo '{ local: *; };' > $(out)", out: ["gen.map"] }
`})
	g.soong.DeviceLink, g.soong.Lldflags = soongDeviceLink, soongLldflags
	for _, c := range []struct {
		name string
		side Side
		kind Kind
	}{{"libv", Device, Shared}, {"libv", Device, Static}, {"libv", Host, Shared}, {"bin", Device, Binary}} {
		if g.node(c.name, c.side, c.kind, "goal") == nil {
			t.Fatal(g.problems)
		}
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	devMap, hostMap := filepath.Join(g.tree, "p/v.map"), filepath.Join(g.tree, "p/host.map")
	genMap := filepath.Join(g.out, "gen/device/gen_map/gen.map")
	for _, c := range []struct {
		out, script string
		device      bool
	}{
		{filepath.Join(g.out, "device/libv/libv.so"), devMap, true},
		{filepath.Join(g.out, "host/libv/libv.so"), hostMap, false},
		{filepath.Join(g.out, "device/bin/bin"), genMap, true},
	} {
		mode, implicit := linkEdge(t, ninja, c.out)
		args := shWords(t, mode)
		if !slices.Contains(args, "-Wl,--version-script,"+c.script) || !slices.Contains(strings.Fields(implicit), esc(c.script)) {
			t.Errorf("%s: mode %q, implicit %q: want version script %s as flag and input", c.out, mode, implicit, c.script)
		}
		for _, f := range soongDeviceLink.Ldflags {
			if slices.Contains(args, f) != c.device {
				t.Errorf("%s: %s present = %v, want %v", c.out, f, !c.device, c.device)
			}
		}
		for _, f := range soongLldflags {
			if !slices.Contains(args, f) {
				t.Errorf("%s: %q lacks %s", c.out, args, f)
			}
		}
	}
	// an archive has no linker, so neither the property nor the flags
	if n := g.nodes[key{"libv", Device, Static}]; n.versionScript != "" {
		t.Errorf("static libv: version script %q", n.versionScript)
	}
}

// Expected from cc/linker.go linkerFlags: a module's ldflags (arch_variant: target and arch blocks, defaults' too) follow the global flags and precede the version script; cc/compiler.go's escaping splits a space-only entry into words (`-z max-page-size=…`, art_hugepage_defaults).
// They pass unchanged, lld-only ones included (libpdfium's -Wl,-icf=all), after the global --icf=safe, so the module's choice wins as under Soong.
func TestModuleLdflags(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_defaults {
    name: "huge",
    arch: { x86_64: { ldflags: ["-z max-page-size=0x200000"] }, arm64: { ldflags: ["-Wl,--arm64-only"] } },
}
cc_library {
    name: "libl",
    host_supported: true,
    defaults: ["huge"],
    srcs: ["l.cc"],
    ldflags: ["-Wl,-Bsymbolic", "-Wl,-icf=all"],
    target: { android: { ldflags: ["-Wl,--exclude-libs=libx.a"] }, linux_glibc: { ldflags: ["-static-libgcc"] } },
    version_script: "l.map",
}
`})
	g.soong.DeviceLink, g.soong.Lldflags = soongDeviceLink, soongLldflags
	for _, side := range []Side{Device, Host} {
		if g.node("libl", side, Shared, "goal") == nil {
			t.Fatal(g.problems)
		}
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	script := "-Wl,--version-script," + filepath.Join(g.tree, "p/l.map")
	for _, c := range []struct {
		side    string
		want    []string
		without []string
	}{
		{"device", []string{"-Wl,-Bsymbolic", "-Wl,-icf=all", "-z", "max-page-size=0x200000", "-Wl,--exclude-libs=libx.a"}, []string{"-static-libgcc"}},
		{"host", []string{"-Wl,-Bsymbolic", "-Wl,-icf=all", "-z", "max-page-size=0x200000", "-static-libgcc"}, []string{"-Wl,--exclude-libs=libx.a"}},
	} {
		mode, _ := linkEdge(t, ninja, filepath.Join(g.out, c.side, "libl/libl.so"))
		args := shWords(t, mode)
		from, to := slices.Index(args, "-Wl,--no-undefined"), slices.Index(args, script)
		if from < 0 || to != len(args)-1 {
			t.Fatalf("%s: %q: want --no-undefined, the module's ldflags, the version script last", c.side, args)
		}
		local := args[from+1 : to]
		for _, f := range c.want {
			if !slices.Contains(local, f) {
				t.Errorf("%s: module flags %q lack %s", c.side, local, f)
			}
		}
		for _, f := range append(c.without, "-Wl,--arm64-only") {
			if slices.Contains(args, f) {
				t.Errorf("%s: %q has %s", c.side, args, f)
			}
		}
		// the global --icf=safe precedes the module's -icf=all: lld takes the last
		if g, m := slices.Index(args, "-Wl,--icf=safe"), slices.Index(args, "-Wl,-icf=all"); g < 0 || g > m {
			t.Errorf("%s: %q: want the global --icf=safe before the module's -icf=all", c.side, args)
		}
	}
}

// The libhwui shape, compiled and linked by the converter's own edges: a whole-archived object's exported function calls an NDK symbol nothing defines (libskia SkFontMgr_android_ndk.cpp -> AFont_*); the library's version script exports only its API.
// The undefined reference goes away only with both: the version script makes the function local, and --gc-sections (with -ffunction-sections) then drops its section.
// Either alone leaves it (exported = a GC root; local but not collected = still linked), which --no-undefined reports.
func TestVersionScriptDropsDeadReference(t *testing.T) {
	for _, tool := range []string{"clang-19", "clang++-19", "ld.lld", "ninja"} {
		if _, err := exec.LookPath(tool); err != nil {
			t.Skipf("no %s: the flags are checked by TestVersionScriptLink, not their effect", tool)
		}
	}
	for _, c := range []struct {
		script, deviceLink, links bool
	}{{false, false, false}, {true, false, false}, {false, true, false}, {true, true, true}} {
		script := ""
		if c.script {
			script = `version_script: "api.map",`
		}
		g := graphFor(t, map[string]string{
			"p/Android.bp": `
cc_library_shared { name: "libhw", srcs: ["api.c"], whole_static_libs: ["libsk"], system_shared_libs: [], ` + script + ` }
cc_library_static { name: "libsk", srcs: ["font.c"], system_shared_libs: [] }
`,
			"p/api.c":   "int hw_api(void) { return 1; }\n",
			"p/font.c":  "extern int AFont_getWeight(const void *);\nint sk_font_weight(const void *f) { return AFont_getWeight(f); }\nint sk_used(void) { return 2; }\n",
			"p/api.map": "{\n  global:\n    hw_api;\n  local:\n    *;\n};\n",
		})
		// global.go CStdVersion / CppStdVersion
		g.soong.CStd, g.soong.CppStd = "gnu23", "gnu++20"
		g.soong.Lldflags = soongLldflags
		if c.deviceLink {
			g.soong.DeviceLink = soongDeviceLink
		}
		// the force-included device headers (deviceForced): their content does not matter here
		for _, h := range deviceForced {
			p := filepath.Join(g.out, "compat/device", h)
			if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(p, nil, 0o644); err != nil {
				t.Fatal(err)
			}
		}
		if g.node("libhw", Device, Shared, "goal") == nil {
			t.Fatal(g.problems)
		}
		ninja := g.emit(Tools{CC: "clang-19", CXX: "clang++-19", AR: "llvm-ar-19"})
		if err := os.WriteFile(filepath.Join(g.out, "build.ninja"), []byte(ninja), 0o644); err != nil {
			t.Fatal(err)
		}
		out, err := exec.Command("ninja", "-C", g.out).CombinedOutput()
		if (err == nil) != c.links {
			t.Errorf("version script %v, device link flags %v: linked = %v, want %v\n%s", c.script, c.deviceLink, err == nil, c.links, out)
			continue
		}
		if !c.links && !strings.Contains(string(out), "AFont_getWeight") {
			t.Errorf("version script %v, device link flags %v: failed for another reason\n%s", c.script, c.deviceLink, out)
		}
	}
}
