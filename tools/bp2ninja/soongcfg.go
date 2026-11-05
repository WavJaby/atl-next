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
	CStd, CppStd                         string   // CStdVersion, CppStdVersion
	ExperimentalCStd, ExperimentalCppStd string   // ExperimentalCStdVersion, ExperimentalCppStdVersion: what c_std / cpp_std "experimental" selects
	GlobalIncludes                       []string // commonGlobalIncludes that exist in the tree (Soong skips missing ones)
	// Soong's global and device-toolchain compile flags, evaluated from its source (readGlobalFlags). Semantics AOSP code relies on, not choices: without CommonGlobalCflags' -ftrivial-auto-var-init=zero
	// ART's class linker gives java.lang.String 80 vtable slots instead of 81 (String.isBlank in Object.clone's slot) and aborts in InitWithoutImage (measured 2026-11-02).
	Flags    GlobalFlags
	APILevel string // device-side -D__ANDROID_API__ (apiLevel)
	// cc/config/bionic.go bionicDefaultSharedLibraries: what a device module links when it sets no system_shared_libs (cc/linker.go; `[]` opts out)
	SystemSharedLibs []string
	Device           DeviceArch      // readDeviceArch
	RustFeatures     RustFeatureGate // readRustFeatureGate
	DeviceLink       DeviceLink      // readDeviceLink
	Lldflags         []string        // readLldflags
}

// readLldflags: global.go commonGlobalLldflags, which Soong gives every link on both sides (cc/linker.go CommonLinkerFlags -> ${config.DeviceGlobalLldflags} / ${config.HostGlobalLldflags}, both built from it): the linker choice (-fuse-ld=lld) and lld's global flags (--icf=safe, --no-demangle).
// Fails when the list selects no linker: the converter's links take theirs from it, and xtask's licence gate parses lld's --trace format.
func readLldflags(tree string) ([]string, error) {
	path := filepath.Join(tree, "build/soong/cc/config/global.go")
	f, err := parser.ParseFile(token.NewFileSet(), path, nil, 0)
	if err != nil {
		return nil, err
	}
	list, ok := stringList(valueSpecs(f)["commonGlobalLldflags"])
	if !ok {
		return nil, fmt.Errorf("%s: commonGlobalLldflags not found (or not a string list)", path)
	}
	if !slices.Contains(list, "-fuse-ld=lld") {
		return nil, fmt.Errorf("%s: commonGlobalLldflags %q lacks -fuse-ld=lld: Soong no longer links with lld, the converter assumes it does", path, list)
	}
	return list, nil
}

// DeviceLink: the part of Soong's device (bionic toolchain) link defaults that decides what a shared library or binary exports and what its link may drop; host (linux_glibc) modules get none of it, as in Soong.
// Only that part: the rest of deviceGlobalLdflags is hardening; the lld flags both sides share are SoongConfig.Lldflags.
// Together with version_script and GlobalFlags' -ffunction-sections it is what makes a library's unexported dead code disappear: local: * hides a symbol, -ffunction-sections puts it in its own section, --gc-sections drops the section with its undefined references (libhwui's whole-archived libskia SkFontMgr_android_ndk.cpp -> AFont_*).
type DeviceLink struct {
	// global.go deviceGlobalLdflags' -Wl,--exclude-libs,* (archive members' symbols are not exported), then gcSections, which Soong adds for bionic shared libraries and dynamic binaries (cc/library.go, cc/binary.go: `if toolchain.Bionic()`)
	Ldflags []string
}

// gcSections: the literal Soong's cc/library.go and cc/binary.go give a bionic link; readDeviceLink fails when either file stops passing it, since DeviceLink's dead-code dropping rests on it.
const gcSections = "-Wl,--gc-sections"

// readDeviceLink: DeviceLink from the pinned Soong (global.go's lists, the gcSections literal in library.go and binary.go).
func readDeviceLink(tree string) (DeviceLink, error) {
	var d DeviceLink
	path := filepath.Join(tree, "build/soong/cc/config/global.go")
	f, err := parser.ParseFile(token.NewFileSet(), path, nil, 0)
	if err != nil {
		return d, err
	}
	specs := valueSpecs(f)
	pick := func(name string, keep func(string) bool) ([]string, error) {
		list, ok := stringList(specs[name])
		if !ok {
			return nil, fmt.Errorf("%s: %s not found (or not a string list)", path, name)
		}
		return slices.DeleteFunc(list, func(s string) bool { return !keep(s) }), nil
	}
	if d.Ldflags, err = pick("deviceGlobalLdflags", func(s string) bool { return strings.HasPrefix(s, "-Wl,--exclude-libs,") }); err != nil {
		return d, err
	}
	for _, rel := range []string{"build/soong/cc/library.go", "build/soong/cc/binary.go"} {
		src, err := parser.ParseFile(token.NewFileSet(), filepath.Join(tree, rel), nil, 0)
		if err != nil {
			return d, err
		}
		found := false
		ast.Inspect(src, func(n ast.Node) bool {
			if l, ok := n.(*ast.BasicLit); ok && goString(l) == gcSections {
				found = true
			}
			return !found
		})
		if !found {
			return d, fmt.Errorf("%s: no %q literal: Soong no longer garbage-collects bionic links, DeviceLink assumes it does", filepath.Join(tree, rel), gcSections)
		}
	}
	d.Ldflags = append(d.Ldflags, gcSections)
	return d, nil
}

// DeviceArch: the arch variant and features Soong's device side builds for (android/arch.go decodeArch): they select Android.bp arch blocks (blocks) and, through the toolchain factory, the ISA flags (GlobalFlags.Tail).
// Device-side sources rely on both: libz's android_x86_64 block defines ADLER32_SIMD_SSSE3 because Android's x86_64 ABI includes SSSE3, and its intrinsics need -mssse3.
// Host-side modules get none: Soong sets arch features for the Android OS only.
type DeviceArch struct {
	Variant  string
	Features []string // android/arch_list.go androidArchFeatureMap[arch][variant]
}

// blocks: the `arch: { <arch>: { … } }` sub-blocks Soong applies after the arch's own (android/arch.go archPropertiesValues): the variant (`-`, `.` -> `_`), then each feature.
func (d DeviceArch) blocks() []string {
	var out []string
	if d.Variant != "" {
		out = append(out, strings.NewReplacer("-", "_", ".", "_").Replace(d.Variant))
	}
	return append(out, d.Features...)
}

// readDeviceArch: DeviceArch for cfg from the pinned Soong. An unknown variant fails in readGlobalFlags, where Soong's toolchain factory panics.
func readDeviceArch(tree string, cfg Config) (DeviceArch, error) {
	d := DeviceArch{Variant: cfg.ArchVariant}
	parse := func(rel string) (*ast.File, error) {
		return parser.ParseFile(token.NewFileSet(), filepath.Join(tree, rel), nil, 0)
	}
	ident, err := archIdent(tree, cfg.Arch)
	if err != nil {
		return d, err
	}
	list, err := parse("build/soong/android/arch_list.go")
	if err != nil {
		return d, err
	}
	if m, ok := valueSpecs(list)["androidArchFeatureMap"].(*ast.CompositeLit); ok {
		for _, e := range m.Elts {
			if kv, ok := e.(*ast.KeyValueExpr); ok {
				if k, ok := kv.Key.(*ast.Ident); ok && k.Name == ident {
					d.Features = stringMap(kv.Value)[cfg.ArchVariant]
				}
			}
		}
	} else {
		return d, fmt.Errorf("build/soong/android/arch_list.go: androidArchFeatureMap not found")
	}
	return d, nil
}

// archIdent: Soong's ArchType identifier for the arch name: `X86_64 = newArch("x86_64", "lib64")` in android/arch.go.
func archIdent(tree, arch string) (string, error) {
	f, err := parser.ParseFile(token.NewFileSet(), filepath.Join(tree, "build/soong/android/arch.go"), nil, 0)
	if err != nil {
		return "", err
	}
	for name, v := range valueSpecs(f) {
		if c, ok := v.(*ast.CallExpr); ok && len(c.Args) > 0 {
			if f, ok := c.Fun.(*ast.Ident); ok && f.Name == "newArch" && goString(c.Args[0]) == arch {
				return name, nil
			}
		}
	}
	return "", fmt.Errorf("build/soong/android/arch.go: no newArch(%q)", arch)
}

// typeName: the identifier of a type expression, through one pointer.
func typeName(e ast.Expr) string {
	if s, ok := e.(*ast.StarExpr); ok {
		e = s.X
	}
	if id, ok := e.(*ast.Ident); ok {
		return id.Name
	}
	return ""
}

func isIdent(e ast.Expr, name string) bool {
	id, ok := e.(*ast.Ident)
	return ok && id.Name == name
}

// valueSpecs: a file's top-level `name = value` declarations.
func valueSpecs(f *ast.File) map[string]ast.Expr {
	out := map[string]ast.Expr{}
	for _, decl := range f.Decls {
		if g, ok := decl.(*ast.GenDecl); ok {
			for _, s := range g.Specs {
				if v, ok := s.(*ast.ValueSpec); ok {
					for i, name := range v.Names {
						if i < len(v.Values) {
							out[name.Name] = v.Values[i]
						}
					}
				}
			}
		}
	}
	return out
}

func goString(e ast.Expr) string {
	if l, ok := e.(*ast.BasicLit); ok && l.Kind == token.STRING {
		s, _ := strconv.Unquote(l.Value)
		return s
	}
	return ""
}

// stringMap: a `map[string][]string{…}` literal (nil for anything else).
func stringMap(e ast.Expr) map[string][]string {
	lit, ok := e.(*ast.CompositeLit)
	if !ok {
		return nil
	}
	out := map[string][]string{}
	for _, el := range lit.Elts {
		if kv, ok := el.(*ast.KeyValueExpr); ok {
			if l, ok := stringList(kv.Value); ok {
				out[goString(kv.Key)] = l
			}
		}
	}
	return out
}

// weakAvailability: Soong's global define (commonGlobalCflags) that puts bionic's versioning.h in weak mode: __INTRODUCED_IN becomes a plain availability attribute and __BIONIC_AVAILABILITY_GUARD is 1, so every declaration stays visible.
// Without it the guard reads __ANDROID_MIN_SDK_VERSION__, which only an android target triple defines, and hides every API introduced after level 0.
// readGlobalFlags fails when the pinned Soong no longer passes it: deviceCompat's semantics would then differ from a platform build.
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
					case "ExperimentalCStdVersion":
						cfg.ExperimentalCStd = str(x.Values[i])
					case "ExperimentalCppStdVersion":
						cfg.ExperimentalCppStd = str(x.Values[i])
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
	if cfg.CStd == "" || cfg.CppStd == "" || cfg.ExperimentalCStd == "" || cfg.ExperimentalCppStd == "" {
		return cfg, fmt.Errorf("%s: CStdVersion/CppStdVersion/ExperimentalCStdVersion/ExperimentalCppStdVersion not found", path)
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

// androidDefine: the other macro clang defines for Soong's device triple (owner 2026-11-01), on device compiles only: AOSP device code keys the Android userspace API on it, which this runtime provides.
// __BIONIC__, which bionic's sys/cdefs.h defines, stays undefined: glibc is the C library, and libc++ keys ABI-shaping config on it (regex word class, ctype masks).
// libc++ 19's __ANDROID__ sites leave its objects byte-identical on glibc given the xlocale.h shim (libcxxShim); xtask check step libcxx keeps that true across libc++ bumps.
const androidDefine = "-D__ANDROID__"

// lfs64Define: bionic declares the LFS64 interfaces (stat64, lseek64, off64_t, ...) unconditionally (libc/include/sys/stat.h, unistd.h); glibc only under _LARGEFILE64_SOURCE, which adds declarations and changes no type size (off_t stays as _FILE_OFFSET_BITS says).
// Device code written against bionic uses them without a feature macro (libopenjdk's ojluni natives; their glibc host variant passes the same define). Device compiles only; xtask check step libcxx probes libc++ with it.
const lfs64Define = "-D_LARGEFILE64_SOURCE"

// apiLevel: the -D flag for __ANDROID_API__ = FutureApiLevelInt: Soong gives platform code the triple <arch>-linux-android<FutureApiLevelInt> (cc/compiler.go AddTargetFlags), from which clang defines it; a linux-gnu triple defines nothing.
// The level names (`__ANDROID_API_U__`, …) come with atl-bionic's generated android/api-level.h, force-included into every device compile (deviceForced).
// Stays even with __ANDROID__ defined: below 21 libc++'s locale_base_api/android.h pulls __posix_l_fallback.h, whose *_l definitions clash with glibc's.
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
	return apiLevelPrefix + soong, nil
}

const apiLevelPrefix = "-D__ANDROID_API__="
