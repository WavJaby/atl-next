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
	CStd, CppStd                         string   // CStdVersion, CppStdVersion
	ExperimentalCStd, ExperimentalCppStd string   // ExperimentalCStdVersion, ExperimentalCppStdVersion: what c_std / cpp_std "experimental" selects
	GlobalIncludes                       []string // commonGlobalIncludes that exist in the tree (Soong skips missing ones)
	// commonGlobalCflags' -D/-U entries, in order: Soong gives them to every module, device and host (cc/compiler.go "${config.CommonGlobalCflags}"), ahead of the module's own flags.
	// Only the preprocessor part: they decide which code compiles (-DANDROID selects skia's SK_BUILD_FOR_ANDROID); its warning and codegen flags are distro-clang choices here, except AutoVarInit.
	GlobalDefines []string
	// CommonGlobalCflags' -ftrivial-auto-var-init (readAutoVarInit): every module, both sides. Not a choice but semantics AOSP code relies on: zero-filled locals.
	// Measured 2026-11-02: without it ART's class linker gives java.lang.String 80 vtable slots instead of 81 (String.isBlank in Object.clone's slot) and aborts in InitWithoutImage.
	AutoVarInit string
	// commonGlobalAsflags' -D/-U entries (-D__ASSEMBLY__): Soong adds them to every .S/.s compile (cc/compiler.go "${config.CommonGlobalAsflags}"); same preprocessor-only rule as GlobalDefines.
	GlobalAsDefines []string
	APILevel        string // device-side -D__ANDROID_API__ (apiLevel)
	// per Side: the toolchain's YasmFlags, the format flags of every .asm compile (readYasmFlags)
	Yasm [2][]string
	// per Side: why Soong has no toolchain there ("" = it has one): linux_glibc exists for x86/x86_64 only, so an arm64 or riscv64 host's host side has none, and an .asm source there is a problem, not a run failure
	NoYasm [2]string
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

// DeviceLink: the part of Soong's device (bionic toolchain) defaults that decides what a shared library or binary exports and what its link may drop; host (linux_glibc) modules get none of it, as in Soong.
// Only that part: the rest of deviceGlobalCflags / deviceGlobalLdflags is hardening or warnings; the lld flags both sides share are SoongConfig.Lldflags.
// Together with version_script it is what makes a library's unexported dead code disappear: local: * hides a symbol, -ffunction-sections puts it in its own section, --gc-sections drops the section with its undefined references (libhwui's whole-archived libskia SkFontMgr_android_ndk.cpp -> AFont_*).
type DeviceLink struct {
	// global.go deviceGlobalCflags' -f*-sections: one section per function / data object, the unit --gc-sections drops
	Cflags []string
	// global.go deviceGlobalCppflags' -fvisibility* (-fvisibility-inlines-hidden): inline functions are not exported
	Cppflags []string
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
	if d.Cflags, err = pick("deviceGlobalCflags", func(s string) bool { return strings.HasPrefix(s, "-f") && strings.HasSuffix(s, "-sections") }); err != nil {
		return d, err
	}
	if d.Cppflags, err = pick("deviceGlobalCppflags", func(s string) bool { return strings.HasPrefix(s, "-fvisibility") }); err != nil {
		return d, err
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
					case "ExperimentalCStdVersion":
						cfg.ExperimentalCStd = str(x.Values[i])
					case "ExperimentalCppStdVersion":
						cfg.ExperimentalCppStd = str(x.Values[i])
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
	if cfg.CStd == "" || cfg.CppStd == "" || cfg.ExperimentalCStd == "" || cfg.ExperimentalCppStd == "" {
		return cfg, fmt.Errorf("%s: CStdVersion/CppStdVersion/ExperimentalCStdVersion/ExperimentalCppStdVersion not found", path)
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
	if cfg.AutoVarInit, err = readAutoVarInit(path); err != nil {
		return cfg, err
	}
	cfg.SystemSharedLibs, err = systemSharedLibs(tree)
	return cfg, err
}

// autoVarInitDefault: global.go's CommonGlobalCflags VariableFunc, the branch taken when none of AUTO_ZERO_INITIALIZE / AUTO_PATTERN_INITIALIZE / AUTO_UNINITIALIZE is set (this build sets none).
var autoVarInitDefault = regexp.MustCompile(`\}\s*else\s*\{\s*//\s*Default to zero initialization\.\s*flags = append\(flags, "(-ftrivial-auto-var-init=[a-z]+)"\)`)

// readAutoVarInit: the -ftrivial-auto-var-init flag Soong gives every compile by default; fails when the default branch is no longer that statement.
func readAutoVarInit(path string) (string, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return "", err
	}
	m := autoVarInitDefault.FindSubmatch(b)
	if m == nil {
		return "", fmt.Errorf("%s: CommonGlobalCflags' default -ftrivial-auto-var-init branch not found: re-derive soongcfg.go readAutoVarInit", path)
	}
	return string(m[1]), nil
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
	return "-D__ANDROID_API__=" + soong, nil
}
