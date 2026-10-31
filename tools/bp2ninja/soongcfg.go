// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
)

// SoongConfig: the global C/C++ settings Soong applies to every module, read from the pinned tree's
// build/soong/cc/config/global.go so an AOSP upgrade brings its own values, never a copy that drifts.
type SoongConfig struct {
	CStd, CppStd   string   // CStdVersion, CppStdVersion
	GlobalIncludes []string // commonGlobalIncludes that exist in the tree (Soong skips missing ones)
	// commonGlobalCflags' -D/-U entries, in order: Soong gives them to every module, device and host (cc/compiler.go "${config.CommonGlobalCflags}"), ahead of the module's own flags.
	// Only the preprocessor part: they decide which code compiles (-DANDROID selects skia's SK_BUILD_FOR_ANDROID); its warning and codegen flags are distro-clang choices here.
	GlobalDefines []string
	APILevel      string // device-side -D__ANDROID_API__ (apiLevel)
	// cc/config/bionic.go bionicDefaultSharedLibraries: what a device module links when it sets no system_shared_libs (cc/linker.go; `[]` opts out)
	SystemSharedLibs []string
}

// weakAvailability: Soong's global define (commonGlobalCflags) that puts bionic's versioning.h in weak mode: __INTRODUCED_IN becomes a plain availability attribute and __BIONIC_AVAILABILITY_GUARD is 1, so every declaration stays visible.
// Without it the guard reads __ANDROID_MIN_SDK_VERSION__, which only an android target triple defines, and hides every API introduced after level 0.
// readSoongConfig fails when the pinned Soong no longer passes it: deviceCompat's semantics would then differ from a platform build.
const weakAvailability = "-D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__"

// futureLevel: api-level.h's `#define __ANDROID_API_FUTURE__ <n>`.
var futureLevel = regexp.MustCompile(`(?m)^#define __ANDROID_API_FUTURE__ (\d+)$`)

// bionicHeader: bionic's api-level.h in the tree, the level platform code falls back to.
const bionicHeader = "bionic/libc/include/android/api-level.h"

func readSoongConfig(tree string) (SoongConfig, error) {
	path := filepath.Join(tree, "build/soong/cc/config/global.go")
	f, err := parser.ParseFile(token.NewFileSet(), path, nil, 0)
	if err != nil {
		return SoongConfig{}, err
	}
	var cfg SoongConfig
	var cflags []string
	str := func(e ast.Expr) string {
		if l, ok := e.(*ast.BasicLit); ok && l.Kind == token.STRING {
			s, _ := strconv.Unquote(l.Value)
			return s
		}
		return ""
	}
	ast.Inspect(f, func(n ast.Node) bool {
		switch x := n.(type) {
		case *ast.ValueSpec:
			for i, name := range x.Names {
				if i < len(x.Values) {
					switch name.Name {
					case "CStdVersion":
						cfg.CStd = str(x.Values[i])
					case "CppStdVersion":
						cfg.CppStd = str(x.Values[i])
					case "commonGlobalCflags":
						if lit, ok := x.Values[i].(*ast.CompositeLit); ok {
							for _, e := range lit.Elts {
								cflags = append(cflags, str(e))
							}
						}
					}
				}
			}
		case *ast.AssignStmt:
			if len(x.Lhs) == 1 && len(x.Rhs) == 1 {
				if id, ok := x.Lhs[0].(*ast.Ident); ok && id.Name == "commonGlobalIncludes" {
					if lit, ok := x.Rhs[0].(*ast.CompositeLit); ok {
						for _, e := range lit.Elts {
							if d := str(e); d != "" {
								if _, err := os.Stat(filepath.Join(tree, d)); err == nil {
									cfg.GlobalIncludes = append(cfg.GlobalIncludes, d)
								}
							}
						}
					}
				}
			}
		}
		return true
	})
	if cfg.CStd == "" || cfg.CppStd == "" {
		return cfg, fmt.Errorf("%s: CStdVersion/CppStdVersion not found", path)
	}
	for _, f := range cflags {
		if strings.HasPrefix(f, "-D") || strings.HasPrefix(f, "-U") {
			cfg.GlobalDefines = append(cfg.GlobalDefines, f)
		}
	}
	if !slices.Contains(cfg.GlobalDefines, weakAvailability) {
		return cfg, fmt.Errorf("%s: commonGlobalCflags lacks %s (deviceCompat assumes it)", path, weakAvailability)
	}
	if cfg.APILevel, err = apiLevel(tree); err != nil {
		return cfg, err
	}
	cfg.SystemSharedLibs, err = systemSharedLibs(tree)
	return cfg, err
}

// stringList: the strings of a `[]string{…}` literal.
func stringList(e ast.Expr) ([]string, bool) {
	lit, ok := e.(*ast.CompositeLit)
	if !ok {
		return nil, false
	}
	var out []string
	for _, el := range lit.Elts {
		l, ok := el.(*ast.BasicLit)
		if !ok || l.Kind != token.STRING {
			return nil, false
		}
		s, _ := strconv.Unquote(l.Value)
		out = append(out, s)
	}
	return out, true
}

// systemSharedLibs: cc/config/bionic.go bionicDefaultSharedLibraries.
func systemSharedLibs(tree string) ([]string, error) {
	path := filepath.Join(tree, "build/soong/cc/config/bionic.go")
	f, err := parser.ParseFile(token.NewFileSet(), path, nil, 0)
	if err != nil {
		return nil, err
	}
	var libs []string
	found := false
	ast.Inspect(f, func(n ast.Node) bool {
		if v, ok := n.(*ast.ValueSpec); ok {
			for i, name := range v.Names {
				if i < len(v.Values) && name.Name == "bionicDefaultSharedLibraries" {
					libs, found = stringList(v.Values[i])
				}
			}
		}
		return true
	})
	if !found {
		return nil, fmt.Errorf("%s: bionicDefaultSharedLibraries not found", path)
	}
	return libs, nil
}

// apiLevel: the -D flag for __ANDROID_API__ = FutureApiLevelInt: Soong gives platform code the triple <arch>-linux-android<FutureApiLevelInt> (cc/compiler.go AddTargetFlags), from which clang defines it; a linux-gnu triple defines nothing.
// The level names (`__ANDROID_API_U__`, …) come with atl-bionic's generated android/api-level.h, force-included where bionic's headers are visible (bionicForced).
//
// FutureApiLevelInt must equal bionic's __ANDROID_API_FUTURE__, the level api-level.h falls back to (and atl-bionic reads headers at); otherwise the two sources disagree on what platform code is and the build stops.
func apiLevel(tree string) (string, error) {
	path := filepath.Join(tree, "build/soong/android/config.go")
	f, err := parser.ParseFile(token.NewFileSet(), path, nil, 0)
	if err != nil {
		return "", err
	}
	soong := ""
	ast.Inspect(f, func(n ast.Node) bool {
		if v, ok := n.(*ast.ValueSpec); ok {
			for i, name := range v.Names {
				if i >= len(v.Values) || name.Name != "FutureApiLevelInt" {
					continue
				}
				if l, ok := v.Values[i].(*ast.BasicLit); ok && l.Kind == token.INT {
					soong = l.Value
				}
			}
		}
		return true
	})
	hdr := filepath.Join(tree, bionicHeader)
	b, err := os.ReadFile(hdr)
	if err != nil {
		return "", err
	}
	future := ""
	if m := futureLevel.FindSubmatch(b); m != nil {
		future = string(m[1])
	}
	if soong == "" || future != soong {
		return "", fmt.Errorf("FutureApiLevelInt %q (%s) and __ANDROID_API_FUTURE__ %q (%s) differ or are missing", soong, path, future, hdr)
	}
	return "-D__ANDROID_API__=" + soong, nil
}
