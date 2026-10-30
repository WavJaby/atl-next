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
)

// SoongConfig: the global C/C++ settings Soong applies to every module, read from the pinned tree's
// build/soong/cc/config/global.go so an AOSP upgrade brings its own values, never a copy that drifts.
type SoongConfig struct {
	CStd, CppStd   string   // CStdVersion, CppStdVersion
	GlobalIncludes []string // commonGlobalIncludes that exist in the tree (Soong skips missing ones)
	APILevels      []string // device-side -D flags: bionic's level names and __ANDROID_API__ (apiLevels)
}

// weakAvailability: Soong's global define (commonGlobalCflags) that puts bionic's versioning.h in weak mode: __INTRODUCED_IN becomes a plain availability attribute and __BIONIC_AVAILABILITY_GUARD is 1, so every declaration stays visible.
// Without it the guard reads __ANDROID_MIN_SDK_VERSION__, which only an android target triple defines, and hides every API introduced after level 0.
// readSoongConfig fails when the pinned Soong no longer passes it: deviceCompat's semantics would then differ from a platform build.
const weakAvailability = "-D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__"

// apiLevel: an object-like level macro of api-level.h (`__ANDROID_API_FUTURE__`, `__ANDROID_API_U__`, …).
var apiLevel = regexp.MustCompile(`(?m)^#define (__ANDROID_API_[A-Z0-9_]+__) (\d+)$`)

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
	if !slices.Contains(cflags, weakAvailability) {
		return cfg, fmt.Errorf("%s: commonGlobalCflags lacks %s (deviceCompat assumes it)", path, weakAvailability)
	}
	cfg.APILevels, err = apiLevels(tree)
	return cfg, err
}

// apiLevels: -D flags that give every device-side compile what a platform build sees without including anything.
//   - __ANDROID_API__ = FutureApiLevelInt: Soong gives platform code the triple <arch>-linux-android<FutureApiLevelInt> (cc/compiler.go AddTargetFlags), from which clang defines it; a linux-gnu triple defines nothing.
//   - api-level.h's level names, verbatim: on bionic every libc include brings them (sys/cdefs.h), and NDK headers use them unincluded (hardware_buffer_aidl.h `__builtin_available(android __ANDROID_API_U__, *)`). The header itself cannot be forced (deviceForced); an explicit include later redefines them identically, which C allows.
//
// FutureApiLevelInt must equal __ANDROID_API_FUTURE__, the level api-level.h falls back to; otherwise the two sources disagree on what platform code is and the build stops.
func apiLevels(tree string) ([]string, error) {
	path := filepath.Join(tree, "build/soong/android/config.go")
	f, err := parser.ParseFile(token.NewFileSet(), path, nil, 0)
	if err != nil {
		return nil, err
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
	hdr := filepath.Join(tree, compatPath(deviceCompat, "android/api-level.h"))
	b, err := os.ReadFile(hdr)
	if err != nil {
		return nil, err
	}
	flags := []string{"-D__ANDROID_API__=" + soong}
	future := ""
	for _, m := range apiLevel.FindAllSubmatch(b, -1) {
		flags = append(flags, "-D"+string(m[1])+"="+string(m[2]))
		if string(m[1]) == "__ANDROID_API_FUTURE__" {
			future = string(m[2])
		}
	}
	if soong == "" || future != soong {
		return nil, fmt.Errorf("FutureApiLevelInt %q (%s) and __ANDROID_API_FUTURE__ %q (%s) differ or are missing", soong, path, future, hdr)
	}
	return flags, nil
}
