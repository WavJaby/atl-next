// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"maps"
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
	// commonGlobalAsflags' -D/-U entries (-D__ASSEMBLY__): Soong adds them to every .S/.s compile (cc/compiler.go "${config.CommonGlobalAsflags}"); same preprocessor-only rule as GlobalDefines.
	GlobalAsDefines []string
	APILevel        string // device-side -D__ANDROID_API__ (apiLevel)
	// per Side: the toolchain's YasmFlags, the format flags of every .asm compile (readYasmFlags)
	Yasm [2][]string
	// per Side: why Soong has no toolchain there ("" = it has one): linux_glibc exists for x86/x86_64 only, so an arm64 or riscv64 host's host side has none, and an .asm source there is a problem, not a run failure
	NoYasm [2]string
	// cc/config/bionic.go bionicDefaultSharedLibraries: what a device module links when it sets no system_shared_libs (cc/linker.go; `[]` opts out)
	SystemSharedLibs []string
	Device           DeviceArch // readDeviceArch
}

// DeviceArch: the ISA Soong's device toolchain targets for the arch and its arches.toml variant (android/arch.go decodeArch, cc/config/<arch>_device.go <arch>ToolchainFactory).
// Device-side sources rely on it: libz's android_x86_64 block defines ADLER32_SIMD_SSSE3 because Android's x86_64 ABI includes SSSE3, and its intrinsics need -mssse3.
// Host-side modules get none: Soong sets arch features for the Android OS only.
type DeviceArch struct {
	Variant  string
	Features []string // android/arch_list.go androidArchFeatureMap[arch][variant]
	// <arch>ArchVariantCflags[variant] + <arch>ArchFeatureCflags[feature] per feature: the ISA part of the toolchain cflags.
	// riscv64's -march sits in riscv64Cflags with non-ISA flags instead; it is read when riscv64 hosts are built (plan S5 phase 6).
	Cflags []string
}

// blocks: the `arch: { <arch>: { … } }` sub-blocks Soong applies after the arch's own (android/arch.go archPropertiesValues): the variant (`-`, `.` -> `_`), then each feature.
func (d DeviceArch) blocks() []string {
	var out []string
	if d.Variant != "" {
		out = append(out, strings.NewReplacer("-", "_", ".", "_").Replace(d.Variant))
	}
	return append(out, d.Features...)
}

// readDeviceArch: DeviceArch for cfg from the pinned Soong; fails where Soong's toolchain factory would panic (an unknown variant).
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
	rel := "build/soong/cc/config/" + cfg.Arch + "_device.go"
	dev, err := parse(rel)
	if err != nil {
		return d, err
	}
	specs := valueSpecs(dev)
	v, ok := specs[cfg.Arch+"ArchVariantCflags"]
	if !ok {
		return d, fmt.Errorf("%s: %sArchVariantCflags not found", rel, cfg.Arch)
	}
	variants := stringMap(v)
	vflags, ok := variants[cfg.ArchVariant]
	if !ok && (cfg.ArchVariant != "" || len(variants) > 0) {
		return d, fmt.Errorf("%s: unknown %s arch variant %q", rel, cfg.Arch, cfg.ArchVariant)
	}
	d.Cflags = vflags
	// absent for arches whose features select no flags (arm64)
	features := stringMap(specs[cfg.Arch+"ArchFeatureCflags"])
	for _, f := range d.Features {
		d.Cflags = append(d.Cflags, features[f]...)
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

// toolchainOS: per side, the android.OsType identifier its toolchain factory is registered for (cc/config registerToolchainFactory); the host side is linux_glibc.
var toolchainOS = [...]string{Device: "Android", Host: "Linux"}

// configVar: a `${config.Name}` reference, how a toolchain method names its pctx variable.
var configVar = regexp.MustCompile(`^\$\{config\.(\w+)\}$`)

// readYasmFlags: per side, what Soong's yasm rule gets from the toolchain (cc/compiler.go Global.YasmFlags = tc.YasmFlags(): `-f elf64 -m amd64` for x86_64), resolved as Soong's Go would:
// the factory registerToolchainFactory names for (os, arch) -> the type it returns -> its YasmFlags method, promoted through embedded structs (shallowest wins) -> the pctx.StaticVariable that method's `${config.X}` names.
// toolchainBase's method returns "" (no flags: arches without .asm sources). A side with no registered factory gets a reason in none (SoongConfig.NoYasm) instead.
func readYasmFlags(tree, arch string) (out [2][]string, none [2]string, err error) {
	ident, err := archIdent(tree, arch)
	if err != nil {
		return out, none, err
	}
	dir := filepath.Join(tree, "build/soong/cc/config")
	entries, err := os.ReadDir(dir)
	if err != nil {
		return out, none, err
	}
	factories := map[[2]string]string{} // (os, arch) -> factory func
	funcs := map[string]*ast.FuncDecl{}
	methods := map[string]string{} // receiver type -> YasmFlags' returned string
	embeds := map[string][]string{}
	vars := map[string]ast.Expr{}
	statics := map[string]string{}
	sel := func(e ast.Expr) string {
		if s, ok := e.(*ast.SelectorExpr); ok {
			return s.Sel.Name
		}
		return ""
	}
	for _, e := range entries {
		if !strings.HasSuffix(e.Name(), ".go") || strings.HasSuffix(e.Name(), "_test.go") {
			continue
		}
		f, err := parser.ParseFile(token.NewFileSet(), filepath.Join(dir, e.Name()), nil, 0)
		if err != nil {
			return out, none, err
		}
		maps.Copy(vars, valueSpecs(f))
		for _, decl := range f.Decls {
			switch d := decl.(type) {
			case *ast.FuncDecl:
				if d.Recv == nil {
					funcs[d.Name.Name] = d
				} else if d.Name.Name == "YasmFlags" && d.Body != nil && len(d.Body.List) == 1 {
					if r, ok := d.Body.List[0].(*ast.ReturnStmt); ok && len(r.Results) == 1 {
						methods[typeName(d.Recv.List[0].Type)] = goString(r.Results[0])
					}
				}
			case *ast.GenDecl:
				for _, s := range d.Specs {
					if ts, ok := s.(*ast.TypeSpec); ok {
						if st, ok := ts.Type.(*ast.StructType); ok {
							for _, fl := range st.Fields.List {
								if len(fl.Names) == 0 {
									embeds[ts.Name.Name] = append(embeds[ts.Name.Name], typeName(fl.Type))
								}
							}
						}
					}
				}
			}
		}
		ast.Inspect(f, func(n ast.Node) bool {
			c, ok := n.(*ast.CallExpr)
			if !ok {
				return true
			}
			switch {
			case sel(c.Fun) == "StaticVariable" && len(c.Args) == 2:
				if l, ok := c.Args[1].(*ast.BasicLit); ok && l.Kind == token.STRING {
					statics[goString(c.Args[0])] = goString(l)
				}
			case isIdent(c.Fun, "registerToolchainFactory") && len(c.Args) == 3:
				if fn, ok := c.Args[2].(*ast.Ident); ok {
					factories[[2]string{sel(c.Args[0]), sel(c.Args[1])}] = fn.Name
				}
			}
			return true
		})
	}
	for side, osName := range toolchainOS {
		where := fmt.Sprintf("%s: toolchain for (%s, %s)", dir, osName, ident)
		fn := funcs[factories[[2]string{osName, ident}]]
		if fn == nil {
			none[side] = "Soong registers no toolchain for (" + osName + ", " + ident + ")"
			continue
		}
		typ := ""
		ast.Inspect(fn.Body, func(n ast.Node) bool {
			if r, ok := n.(*ast.ReturnStmt); ok && len(r.Results) == 1 {
				e := r.Results[0]
				if id, ok := e.(*ast.Ident); ok {
					e = vars[id.Name]
				}
				if u, ok := e.(*ast.UnaryExpr); ok && u.Op == token.AND {
					e = u.X
				}
				if lit, ok := e.(*ast.CompositeLit); ok {
					typ = typeName(lit.Type)
				}
			}
			return true
		})
		if typ == "" {
			return out, none, fmt.Errorf("%s: %s returns no toolchain literal", where, fn.Name.Name)
		}
		flags, err := promoted(typ, methods, embeds)
		if err != nil {
			return out, none, fmt.Errorf("%s: %v", where, err)
		}
		if m := configVar.FindStringSubmatch(flags); m != nil {
			v, ok := statics[m[1]]
			if !ok {
				return out, none, fmt.Errorf("%s: no pctx.StaticVariable(%q, <literal>)", where, m[1])
			}
			flags = v
		} else if flags != "" {
			return out, none, fmt.Errorf("%s: YasmFlags returns %q, not ${config.<var>}", where, flags)
		}
		out[side] = strings.Fields(flags)
	}
	return out, none, nil
}

// promoted: the YasmFlags result Go's method promotion gives typ: its own, else the one embedded struct at the shallowest depth that has it.
func promoted(typ string, methods map[string]string, embeds map[string][]string) (string, error) {
	for level := []string{typ}; len(level) > 0; {
		var found []string
		var next []string
		for _, t := range level {
			if _, ok := methods[t]; ok {
				found = append(found, t)
			}
			next = append(next, embeds[t]...)
		}
		switch len(found) {
		case 0:
			level = next
		case 1:
			return methods[found[0]], nil
		default:
			return "", fmt.Errorf("YasmFlags ambiguous between %q", found)
		}
	}
	return "", fmt.Errorf("%s has no YasmFlags method", typ)
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
	var cflags, asflags []string
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
						cflags, _ = stringList(x.Values[i])
					case "commonGlobalAsflags":
						asflags, _ = stringList(x.Values[i])
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
	defines := func(list []string) []string {
		return slices.DeleteFunc(list, func(f string) bool { return !strings.HasPrefix(f, "-D") && !strings.HasPrefix(f, "-U") })
	}
	cfg.GlobalDefines, cfg.GlobalAsDefines = defines(cflags), defines(asflags)
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
