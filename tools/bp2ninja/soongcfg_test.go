// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// soongTree writes the four files readSoongConfig reads, shaped like the pinned android-16.0.0_r1 ones: global.go's var block (commonGlobalCflags a ValueSpec, commonGlobalIncludes an assignment), config.go's const, bionic.go's var block, api-level.h's #define.
// 10000 is both upstream values (android/config.go FutureApiLevelInt, api-level.h __ANDROID_API_FUTURE__); U = 34 and the symbolic __ANDROID_API__ default are api-level.h's lines 159 and 101; libc, libm, libdl is bionic.go's line 22.
func soongTree(t *testing.T, cflags, soongLevel, bionicLevel string) string {
	tree := t.TempDir()
	files := map[string]string{
		"build/soong/cc/config/global.go": "package config\n\nvar (\n\tcommonGlobalCflags = []string{\n\t\t\"-O2\",\n\t\t" + cflags + "\n\t}\n)\n\n" +
			"const (\n\tCStdVersion = \"gnu23\"\n\tCppStdVersion = \"gnu++20\"\n)\n\nfunc init() {\n\tcommonGlobalIncludes = []string{\"system/core/include\"}\n}\n",
		"build/soong/cc/config/bionic.go": "package config\n\nvar (\n\tbionicDefaultSharedLibraries = []string{\"libc\", \"libm\", \"libdl\"}\n\n\tbionicCrtBeginStaticBinary, bionicCrtEndStaticBinary = []string{\"crtbegin_static\"}, []string{\"crtend_android\"}\n)\n",
		"build/soong/android/config.go":   "package android\n\nconst FutureApiLevelInt = " + soongLevel + "\n",
		"bionic/libc/include/android/api-level.h": "#pragma once\n#define __ANDROID_API_FUTURE__ " + bionicLevel + "\n#ifndef __ANDROID_API__\n" +
			"#define __ANDROID_API__ __ANDROID_API_FUTURE__\n#endif\n#define __ANDROID_API_U__ 34\n",
		"system/core/include/.keep": "",
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

func TestSoongConfigAvailability(t *testing.T) {
	weak := `"-D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__",`
	// global.go:106-112 order, with a warning and a codegen flag around them
	cfg, err := readSoongConfig(soongTree(t, `"-Wall", "-DANDROID", "-DNDEBUG", "-UDEBUG", `+weak+` "-faddrsig",`, "10000", "10000"))
	if err != nil {
		t.Fatal(err)
	}
	if cfg.CStd != "gnu23" || cfg.CppStd != "gnu++20" || len(cfg.GlobalIncludes) != 1 {
		t.Fatalf("config %+v", cfg)
	}
	// only the preprocessor flags, in Soong's order
	defines := []string{"-DANDROID", "-DNDEBUG", "-UDEBUG", "-D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__"}
	if !slices.Equal(cfg.GlobalDefines, defines) {
		t.Errorf("GlobalDefines %q, want %q", cfg.GlobalDefines, defines)
	}
	// the triple's level only: level names come with the generated api-level.h
	if cfg.APILevel != "-D__ANDROID_API__=10000" {
		t.Errorf("APILevel %q", cfg.APILevel)
	}
	if want := []string{"libc", "libm", "libdl"}; !slices.Equal(cfg.SystemSharedLibs, want) {
		t.Errorf("SystemSharedLibs %q, want %q", cfg.SystemSharedLibs, want)
	}
	// the forced header is one of the linked ones
	compatPath(deviceCompat, deviceForced)
	for _, c := range []struct {
		name, cflags, soong, bionic, want string
	}{
		// Soong stops passing the weak-mode define: bionic's guard would hide every API introduced after level 0
		{"no weak define", `"-DANDROID",`, "10000", "10000", "lacks -D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__"},
		// platform code's level and api-level.h's default part ways
		{"levels differ", weak, "10000", "10001", "differ or are missing"},
		{"bionic level missing", weak, "10000", "", "differ or are missing"},
	} {
		_, err := readSoongConfig(soongTree(t, c.cflags, c.soong, c.bionic))
		if err == nil || !strings.Contains(err.Error(), c.want) {
			t.Errorf("%s: err %v, want %q", c.name, err, c.want)
		}
	}
}

// archTree writes the three files readDeviceArch reads, shaped like the pinned android-16.0.0_r1 ones (an excerpt of each declaration): android/arch.go's newArch vars, arch_list.go's androidArchFeatureMap, cc/config/x86_64_device.go and arm64_device.go (the latter without a feature map, as upstream).
func archTree(t *testing.T) string {
	tree := t.TempDir()
	files := map[string]string{
		"build/soong/android/arch.go": "package android\n\nvar (\n\tArm64 = newArch(\"arm64\", \"lib64\")\n\tX86_64 = newArch(\"x86_64\", \"lib64\")\n)\n",
		"build/soong/android/arch_list.go": "package android\n\nvar androidArchFeatureMap = map[ArchType]map[string][]string{\n" +
			"\tArm64: {\n\t\t\"armv9-a\": {\n\t\t\t\"dotprod\",\n\t\t},\n\t},\n" +
			"\tX86_64: {\n\t\t\"\" /*default */ : {\n\t\t\t\"ssse3\",\n\t\t\t\"sse4\",\n\t\t\t\"sse4_1\",\n\t\t\t\"sse4_2\",\n\t\t\t\"popcnt\",\n\t\t},\n\t\t\"haswell\": {\n\t\t\t\"ssse3\",\n\t\t\t\"avx2\",\n\t\t},\n\t},\n}\n",
		"build/soong/cc/config/x86_64_device.go": "package config\n\nvar (\n\tx86_64ArchVariantCflags = map[string][]string{\n\t\t\"\": []string{\n\t\t\t\"-march=x86-64\",\n\t\t},\n\t\t\"haswell\": []string{\n\t\t\t\"-march=core-avx2\",\n\t\t},\n\t}\n\n" +
			"\tx86_64ArchFeatureCflags = map[string][]string{\n\t\t\"ssse3\":  []string{\"-mssse3\"},\n\t\t\"sse4\":   []string{\"-msse4\"},\n\t\t\"sse4_1\": []string{\"-msse4.1\"},\n\t\t\"sse4_2\": []string{\"-msse4.2\"},\n\t\t\"popcnt\": []string{\"-mpopcnt\"},\n\t\t\"aes_ni\": []string{\"-maes\"},\n\t}\n)\n",
		"build/soong/cc/config/arm64_device.go": "package config\n\nvar (\n\tarm64ArchVariantCflags = map[string][]string{\n\t\t\"armv8-a\": []string{\n\t\t\t\"-march=armv8-a\",\n\t\t},\n\t\t\"armv9-a\": []string{\n\t\t\t\"-march=armv9-a\",\n\t\t},\n\t}\n)\n",
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

// Expected flags: Soong's x86_64ToolchainFactory = the variant's cflags, then each feature's, in androidArchFeatureMap order; avx2 has no entry in the feature cflags (upstream comments it out), so it adds nothing.
func TestDeviceArch(t *testing.T) {
	tree := archTree(t)
	for _, c := range []struct {
		arch, variant string
		features      []string
		cflags        []string
		blocks        []string
	}{
		{"x86_64", "", []string{"ssse3", "sse4", "sse4_1", "sse4_2", "popcnt"}, []string{"-march=x86-64", "-mssse3", "-msse4", "-msse4.1", "-msse4.2", "-mpopcnt"}, []string{"ssse3", "sse4", "sse4_1", "sse4_2", "popcnt"}},
		{"x86_64", "haswell", []string{"ssse3", "avx2"}, []string{"-march=core-avx2", "-mssse3"}, []string{"haswell", "ssse3", "avx2"}},
		// no features for the variant, no feature map: the variant's flags only; the block name is the variant with `-` -> `_`
		{"arm64", "armv8-a", nil, []string{"-march=armv8-a"}, []string{"armv8_a"}},
		{"arm64", "armv9-a", []string{"dotprod"}, []string{"-march=armv9-a"}, []string{"armv9_a", "dotprod"}},
	} {
		d, err := readDeviceArch(tree, Config{Arch: c.arch, PointerBits: 64, ArchVariant: c.variant})
		if err != nil {
			t.Fatalf("%s/%s: %v", c.arch, c.variant, err)
		}
		if !slices.Equal(d.Features, c.features) || !slices.Equal(d.Cflags, c.cflags) || !slices.Equal(d.blocks(), c.blocks) {
			t.Errorf("%s/%q: %+v blocks %q", c.arch, c.variant, d, d.blocks())
		}
	}
	// Soong's factories panic on an unknown variant; arm64 has no baseline
	for _, c := range []struct{ arch, variant, want string }{
		{"x86_64", "nehalem", `unknown x86_64 arch variant "nehalem"`},
		{"arm64", "", `unknown arm64 arch variant ""`},
		{"mips", "", `no newArch("mips")`},
	} {
		if _, err := readDeviceArch(tree, Config{Arch: c.arch, ArchVariant: c.variant}); err == nil || !strings.Contains(err.Error(), c.want) {
			t.Errorf("%s/%q: err %v, want %q", c.arch, c.variant, err, c.want)
		}
	}
}

// A device compile gets the toolchain's ISA flags ahead of its own and the arch's feature sub-block; a host compile neither.
func TestDeviceArchFlags(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_library {
    name: "libz",
    host_supported: true,
    srcs: ["a.c"],
    cflags: ["-mno-sse4"],
    arch: { x86_64: { cflags: ["-DARCH"], ssse3: { cflags: ["-DFEATURE"] }, avx2: { cflags: ["-DAVX2"] } } },
}
`})
	g.soong.Device = DeviceArch{Features: []string{"ssse3"}, Cflags: []string{"-march=x86-64", "-mssse3"}}
	dev, host := g.node("libz", Device, Shared, "t"), g.node("libz", Host, Shared, "t")
	if dev == nil || host == nil {
		t.Fatal(g.problems)
	}
	df, hf := g.compileFlags(dev).c, g.compileFlags(host).c
	at := func(list []string, f string) int { return slices.Index(list, f) }
	if i, j := at(df, "-mssse3"), at(df, "-mno-sse4"); i < 0 || j < i {
		t.Errorf("device: toolchain flag at %d, module flag at %d: %q", i, j, df)
	}
	if at(df, "-DFEATURE") < 0 || at(df, "-DARCH") < 0 || at(df, "-DAVX2") >= 0 {
		t.Errorf("device: arch blocks %q", df)
	}
	if at(hf, "-mssse3") >= 0 || at(hf, "-DFEATURE") >= 0 || at(hf, "-DARCH") < 0 {
		t.Errorf("host: %q", hf)
	}
}
