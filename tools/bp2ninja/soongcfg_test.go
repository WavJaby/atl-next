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
