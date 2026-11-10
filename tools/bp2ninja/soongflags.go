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
	"sort"
	"strconv"
	"strings"
)

// GlobalFlags: the compile flags Soong gives every C/C++/asm compile before and after the module's own (cc/compiler.go compilerFlags, cc/builder.go), per side, evaluated from the pinned build/soong/cc/config package by soongEval.
// Soong's command lines (builder.go): C = Common + Conly + module + NoOverride; C++ = Common + Cpp + module + NoOverride; asm = Common + As + module. Common is [instruction set, tc.Cflags, CommonGlobalCflags, <Hod>GlobalCflags, ExternalCflags if third-party, tc.ToolchainCflags].
// The device side takes the bionic toolchain for (Android, arch), the host side the linux_glibc one for (Linux, arch) (-fstack-protector-strong, -D_FORTIFY_SOURCE=3, -fno-omit-frame-pointer, -m64, ...).
// Every flag passes toolchainLocation and keepFlag; what they drop is in Skipped.
type GlobalFlags struct {
	Head, Tail     map[Side][]string // Common up to <Hod>GlobalCflags, and tc.ToolchainCflags: ExternalCflags goes between them
	External       []string          // ExternalCflags, for third-party module dirs (ThirdParty)
	Conly, Cpp, As map[Side][]string
	NoOverride     []string // NoOverrideGlobalCflags + NoOverride64GlobalCflags (64-bit arches)
	NoOverrideExt  []string // NoOverrideExternalGlobalCflags, third-party only
	ThirdParty     ThirdParty
	Skipped        []string // dropped by toolchainLocation (with its argument) or keepFlag, unique, sorted
	// per Side: the toolchain's YasmFlags, the format flags of every .asm compile (cc/compiler.go Global.YasmFlags: `-f elf64 -m amd64` for x86_64); toolchainBase's "" on arches without .asm sources
	Yasm map[Side][]string
	// per Side: why Soong has no toolchain there ("" = it has one): linux_glibc exists for x86/x86_64 only, so an arm64 or riscv64 host's host side has none, and an .asm source there is a problem, not a run failure
	NoToolchain map[Side]string
	// per bionic Side (native bridge): the toolchain's ClangTriple, which AddTargetFlags (cc/compiler.go) suffixes with the API level as `-target`; glibc sides compile for the distro clang's default triple
	Triple map[Side]string
	// per bionic Side: rust/config's RustTriple, rustc's --target there (rust/compiler.go); its std is rustup's for that target (rust-toolchain.toml toolchain). Glibc sides compile for rustc's default host triple
	RustTriple map[Side]string
	// the cross host's link flags (HostCross.linkFlags) on every host link; nil when the host is the build machine
	HostLink []string
}

// toolchainOS: the android.OsType identifier the side's toolchain factory is registered for (cc/config registerToolchainFactory); the host side is linux_glibc.
func toolchainOS(s Side) string {
	if s.android() {
		return "Android"
	}
	return "Linux"
}

// hod: the side's Host-or-Device prefix of Soong's global flag variables (<Hod>GlobalCflags).
func hod(s Side) string {
	if s.android() {
		return "Device"
	}
	return "Host"
}

// keepFlag: a Soong global flag the converter passes. Dropped are the flags that never change the object's code or data, only diagnostics or DWARF:
//   - -W* warnings and warning-errors (the converter compiles with -w -Wno-error: distro clang warns differently from AOSP's); -Wa, -Wl, -Wp are pass-throughs and kept
//   - -fcolor-diagnostics, -fmessage-length=*: diagnostic layout (ANSI codes would also reach the triage's log parsing)
//   - -g*, -fdebug-*: debug info (clang's -g never changes generated code); with it every compile bypasses the compiler cache (debugInfo) and every out dir path lands in the objects
//
// Everything else is kept, codegen and preprocessor alike (-ftrivial-auto-var-init=zero, -fno-strict-aliasing, -fno-exceptions, -D_FORTIFY_SOURCE=2, -march, ...).
func keepFlag(f string) bool {
	switch {
	case strings.HasPrefix(f, "-W"):
		return strings.HasPrefix(f, "-Wa,") || strings.HasPrefix(f, "-Wl,") || strings.HasPrefix(f, "-Wp,")
	case f == "-fcolor-diagnostics", strings.HasPrefix(f, "-fmessage-length="):
		return false
	case strings.HasPrefix(f, "-g"), strings.HasPrefix(f, "-fdebug-"):
		return false
	}
	return true
}

// toolchainLocation: how many argv words a Soong toolchain flag naming its prebuilt toolchain spans (0 = not one): --gcc-toolchain=<dir> and --sysroot <dir> (also --sysroot=<dir>, --gcc-toolchain <dir>).
// linux_glibc points clang at prebuilts/gcc/linux-x86/host/x86_64-linux-glibc2.17-4.8 (GCC 4.8 runtime, glibc 2.17 sysroot), the companion of Soong's prebuilt clang (not fetched: the flags naming it are dropped below).
// The converter's toolchain is the distro clang with the build machine's glibc, libc++ and GCC runtime, compile and link alike; 2.17 headers would compile against one C library and link another (and the distro libc++, built on the host's).
// A toolchain choice, like the compiler itself, not a flag AOSP code relies on; dropped with its argument and listed in Skipped.
func toolchainLocation(f string) int {
	switch {
	case f == "--sysroot", f == "--gcc-toolchain":
		return 2
	case strings.HasPrefix(f, "--sysroot="), strings.HasPrefix(f, "--gcc-toolchain="):
		return 1
	}
	return 0
}

// HostCross: the target of a host whose arch is not the build machine's (xtask native: the host row's clang, sysroot and isa). Zero = the build machine's own, no flags.
// The distro clang compiles and links for any target it knows given --target and --sysroot (the sysroot's libraries, crt objects and GCC runtime dir are found under it). -march pins the ISA baseline: no V or Zbs reaches an object (readelf -A Tag_RISCV_arch).
// No -fuse-ld here: Soong's lldflags give every host link -fuse-ld=lld already (readLldflags).
type HostCross struct {
	Target  string // clang --target
	Sysroot string // the sysroot directory (a fetched sysroot_debs row)
	ISA     string // -march
	Libcxx  string // the sysroot's libc++ headers (xtask cross::libcxx_dir): the one c++/v1 holding __config
}

// set: the host is cross. main.go requires its four fields together.
func (h HostCross) set() bool { return h != (HostCross{}) }

// compileFlags: the target flags of every host compile, ahead of the Soong toolchain's (instruction set first, compiler.go).
func (h HostCross) compileFlags() []string {
	if !h.set() {
		return nil
	}
	return []string{"--target=" + h.Target, "--sysroot=" + h.Sysroot, "-march=" + h.ISA}
}

// linkFlags: the target and sysroot of every host link. clang adds the sysroot's library and GCC runtime dirs itself (checked on riscv64: a link with no -L).
func (h HostCross) linkFlags() []string {
	if !h.set() {
		return nil
	}
	return []string{"--target=" + h.Target, "--sysroot=" + h.Sysroot}
}

// common: the side's Global.CommonFlags for a module in dir.
func (f *GlobalFlags) common(side Side, dir string) []string {
	out := slices.Clone(f.Head[side])
	if f.ThirdParty.is(dir) {
		out = append(out, f.External...)
	}
	return append(out, f.Tail[side]...)
}

// noOverride: the flags after the module's own on C and C++ compiles.
func (f *GlobalFlags) noOverride(dir string) []string {
	out := slices.Clone(f.NoOverride)
	if f.ThirdParty.is(dir) {
		out = append(out, f.NoOverrideExt...)
	}
	return out
}

// ThirdParty: android/paths.go IsThirdPartyPath, read from the pinned source: a dir under one of Prefixes and matching none of Exceptions.
type ThirdParty struct {
	Prefixes   []string
	Exceptions []*regexp.Regexp
}

func (t ThirdParty) is(dir string) bool {
	dir += "/"
	for _, p := range t.Prefixes {
		if strings.HasPrefix(dir, p) {
			return !slices.ContainsFunc(t.Exceptions, func(r *regexp.Regexp) bool { return r.MatchString(dir) })
		}
	}
	return false
}

// readThirdParty: IsThirdPartyPath's `thirdPartyDirPrefixes := []string{...}` and the package var thirdPartyDirPrefixExceptions (regexp.MustCompile literals).
func readThirdParty(tree string) (ThirdParty, error) {
	var t ThirdParty
	path := filepath.Join(tree, "build/soong/android/paths.go")
	f, err := parser.ParseFile(token.NewFileSet(), path, nil, 0)
	if err != nil {
		return t, err
	}
	for _, d := range f.Decls {
		if fn, ok := d.(*ast.FuncDecl); ok && fn.Recv == nil && fn.Name.Name == "IsThirdPartyPath" {
			ast.Inspect(fn.Body, func(n ast.Node) bool {
				if a, ok := n.(*ast.AssignStmt); ok && len(a.Lhs) == 1 && isIdent(a.Lhs[0], "thirdPartyDirPrefixes") {
					t.Prefixes, _ = stringList(a.Rhs[0])
				}
				return true
			})
		}
	}
	if lit, ok := valueSpecs(f)["thirdPartyDirPrefixExceptions"].(*ast.CompositeLit); ok {
		for _, e := range lit.Elts {
			c, ok := e.(*ast.CallExpr)
			if !ok || len(c.Args) != 1 || goString(c.Args[0]) == "" {
				return t, fmt.Errorf("%s: thirdPartyDirPrefixExceptions entry is not regexp.MustCompile(<literal>)", path)
			}
			r, err := regexp.Compile(goString(c.Args[0]))
			if err != nil {
				return t, err
			}
			t.Exceptions = append(t.Exceptions, r)
		}
	}
	if len(t.Prefixes) == 0 || len(t.Exceptions) == 0 {
		return t, fmt.Errorf("%s: IsThirdPartyPath's prefixes or thirdPartyDirPrefixExceptions not found", path)
	}
	return t, nil
}

func newGlobalFlags() GlobalFlags {
	return GlobalFlags{Head: map[Side][]string{}, Tail: map[Side][]string{}, Conly: map[Side][]string{}, Cpp: map[Side][]string{}, As: map[Side][]string{},
		Yasm: map[Side][]string{}, NoToolchain: map[Side]string{}, Triple: map[Side]string{}, RustTriple: map[Side]string{}}
}

// readGlobalFlags: GlobalFlags for the device arch cfg (its arches.toml variant and Soong's feature list for it, DeviceArch).
func readGlobalFlags(tree string, cfg Config, dev DeviceArch) (GlobalFlags, error) {
	g := newGlobalFlags()
	ev, err := newSoongEval(tree, soongCcConfig)
	if err != nil {
		return g, err
	}
	skipped := map[string]bool{}
	get := func(expr string) ([]string, error) {
		s, err := ev.expand(expr)
		if err != nil {
			return nil, err
		}
		var out []string
		fields := strings.Fields(s)
		for i := 0; i < len(fields); i++ {
			f := fields[i]
			if n := toolchainLocation(f); n > 0 {
				skipped[strings.Join(fields[i:min(i+n, len(fields))], " ")] = true
				i += n - 1
			} else if keepFlag(f) {
				out = append(out, f)
			} else {
				skipped[f] = true
			}
		}
		return out, nil
	}
	idents := map[Side]string{}
	for _, side := range cfg.sides() {
		arch, _ := cfg.archOf(side)
		if idents[side], err = archIdent(tree, arch); err != nil {
			return g, err
		}
	}
	// android.Arch per side (arch.go:1726-1731): the device its arches.toml variant and features, the native bridge guest its row's variant, the host Soong's baseline.
	// Known gap: Soong also sets NB features from androidArchFeatureMap[arch][variant] (arch.go:1874-1878; x86_64 "" = ssse3, sse4, sse4_1, sse4_2, popcnt), so this side misses their -m flags; fixing it changes x86 NB output.
	archFields := func(s Side) map[string]any {
		if s == Device {
			return map[string]any{"ArchVariant": dev.Variant, "CpuVariant": "", "ArchFeatures": dev.Features}
		}
		if s.kind == nativeBridgeSide {
			return map[string]any{"ArchVariant": cfg.NativeBridgeArchVariant, "CpuVariant": "", "ArchFeatures": []string(nil)}
		}
		return map[string]any{"ArchVariant": "", "CpuVariant": "", "ArchFeatures": []string(nil)}
	}
	type part struct {
		dst  map[Side][]string
		vars []string // ${config.X}, in order
	}
	for _, side := range cfg.sides() {
		for _, p := range []part{
			{g.Head, []string{"CommonGlobalCflags", hod(side) + "GlobalCflags"}},
			{g.Conly, []string{"CommonGlobalConlyflags"}},
			{g.Cpp, []string{"CommonGlobalCppflags", hod(side) + "GlobalCppflags"}},
			{g.As, []string{"CommonGlobalAsflags"}},
		} {
			for _, v := range p.vars {
				l, err := get("${config." + v + "}")
				if err != nil {
					return g, err
				}
				p.dst[side] = append(p.dst[side], l...)
			}
		}
	}
	// each side's toolchain around the global lists (compiler.go: instruction set first, tc.Cflags before CommonGlobalCflags, tc.ToolchainCflags last; tc.Cppflags after the global C++ lists, tc.Asflags before CommonGlobalAsflags)
	for _, side := range cfg.sides() {
		osName, ident := toolchainOS(side), idents[side]
		if !ev.hasToolchain(osName, ident) {
			if side.android() {
				return g, fmt.Errorf("%s: no toolchain factory registered for (%s, %s)", ev.dir, osName, ident)
			}
			g.NoToolchain[side] = "Soong registers no toolchain for (" + osName + ", " + ident + ")"
			continue
		}
		tc, err := ev.toolchain(osName, ident, archFields(side))
		if err != nil {
			return g, err
		}
		method := func(name string, args ...any) ([]string, error) {
			s, err := ev.method(tc, name, args...)
			if err != nil {
				return nil, err
			}
			return get(s)
		}
		isa, err := method("InstructionSetFlags", "")
		if err != nil {
			return g, err
		}
		cflags, err := method("Cflags")
		if err != nil {
			return g, err
		}
		g.Head[side] = slices.Concat(isa, cflags, g.Head[side])
		if g.Tail[side], err = method("ToolchainCflags"); err != nil {
			return g, err
		}
		cpp, err := method("Cppflags")
		if err != nil {
			return g, err
		}
		g.Cpp[side] = append(g.Cpp[side], cpp...)
		as, err := method("Asflags")
		if err != nil {
			return g, err
		}
		g.As[side] = append(as, g.As[side]...)
		if !side.glibc() {
			t, err := method("ClangTriple")
			if err != nil {
				return g, err
			}
			if len(t) != 1 {
				return g, fmt.Errorf("%s: ClangTriple for (%s, %s) is %q, not one word", ev.dir, osName, ident, t)
			}
			g.Triple[side] = t[0]
		}
		yasm, err := ev.method(tc, "YasmFlags")
		if err != nil {
			return g, err
		}
		// assembler arguments, not clang flags: keepFlag does not apply
		if yasm, err = ev.expand(yasm); err != nil {
			return g, err
		}
		g.Yasm[side] = strings.Fields(yasm)
	}
	// the cross host's target flags lead its compiles, ahead of the toolchain's (if it has one); its link flags go on every host link (HostLink)
	if cfg.HostCross.set() {
		g.Head[Host] = slices.Concat(cfg.HostCross.compileFlags(), g.Head[Host])
		g.HostLink = cfg.HostCross.linkFlags()
	}
	if err := readRustTriples(tree, cfg, idents, archFields, g.RustTriple); err != nil {
		return g, err
	}
	if g.External, err = get("${config.ExternalCflags}"); err != nil {
		return g, err
	}
	if g.NoOverride, err = get("${config.NoOverrideGlobalCflags}"); err != nil {
		return g, err
	}
	if cfg.PointerBits == 64 {
		l, err := get("${config.NoOverride64GlobalCflags}")
		if err != nil {
			return g, err
		}
		g.NoOverride = append(g.NoOverride, l...)
	}
	if g.NoOverrideExt, err = get("${config.NoOverrideExternalGlobalCflags}"); err != nil {
		return g, err
	}
	if g.ThirdParty, err = readThirdParty(tree); err != nil {
		return g, err
	}
	if !slices.Contains(g.Head[Device], weakAvailability) {
		return g, fmt.Errorf("${config.CommonGlobalCflags} lacks %s (deviceCompat assumes it)", weakAvailability)
	}
	g.Skipped = sortedKeys(skipped)
	return g, nil
}

// readRustTriples: rust/config's RustTriple per bionic side, from the toolchain rust/config registers for (Android, arch) with the same arch fields cc's gets.
func readRustTriples(tree string, cfg Config, idents map[Side]string, archFields func(Side) map[string]any, dst map[Side]string) error {
	var ev *soongEval
	for _, side := range cfg.sides() {
		if side.glibc() {
			continue
		}
		if ev == nil {
			var err error
			if ev, err = newSoongEval(tree, soongRustConfig); err != nil {
				return err
			}
		}
		tc, err := ev.toolchain(toolchainOS(side), idents[side], archFields(side))
		if err != nil {
			return err
		}
		t, err := ev.method(tc, "RustTriple")
		if err != nil {
			return err
		}
		if t == "" || strings.ContainsAny(t, " \t$") {
			return fmt.Errorf("%s: RustTriple for %s is %q, not one literal word", ev.dir, idents[side], t)
		}
		dst[side] = t
	}
	return nil
}

// soongEval interprets the part of Go that build/soong/cc/config computes its flags with, as Soong would run it in this build:
// package vars, then every init (pctx.StaticVariable / VariableFunc registrations, appends to package vars), then on demand a VariableFunc body, a toolchain factory and its methods.
// The build's environment: no Soong environment variable is set (IsEnvTrue / IsEnvFalse false, Getenv ""), product config methods take their unset default (configDefaults), runtime.GOOS is linux.
// Anything else it meets where a value is needed fails, naming the construct: a Soong change is re-derived, never guessed.
type soongEval struct {
	tree     string // the AOSP tree: pctx.SourcePathVariable paths are relative to it
	dir      string
	vars     map[string]any // package vars, evaluated; error values = could not be evaluated (fails only when read)
	funcs    map[string]*ast.FuncDecl
	methods  map[string]map[string]*ast.FuncDecl // receiver type -> method
	embeds   map[string][]string
	statics  map[string]any // pctx.StaticVariable; pctx.SourcePathVariable as its absolute path
	varFuncs map[string]*ast.FuncLit
	factory  map[[2]string]string // (os, arch) -> registerToolchainFactory's function
}

// configDefaults: android.Config methods the evaluated code calls, with the value this build gets. Env methods: no Soong environment variable is set.
// NoBionicPageSizeMacro: productVariables.DeviceNoBionicPageSizeMacro, set only by 16 KiB page-size products; the build has no product, so unset.
var configDefaults = map[string]any{
	"IsEnvTrue":             false,
	"IsEnvFalse":            false,
	"Getenv":                "",
	"NoBionicPageSizeMacro": false,
}

// soongCcConfig, soongRustConfig: the toolchain packages soongEval reads (both register their toolchains with registerToolchainFactory).
const (
	soongCcConfig   = "build/soong/cc/config"
	soongRustConfig = "build/soong/rust/config"
)

func newSoongEval(tree, pkg string) (*soongEval, error) {
	dir := filepath.Join(tree, pkg)
	e := &soongEval{tree: tree, dir: dir, vars: map[string]any{}, funcs: map[string]*ast.FuncDecl{}, methods: map[string]map[string]*ast.FuncDecl{}, embeds: map[string][]string{},
		statics: map[string]any{}, varFuncs: map[string]*ast.FuncLit{}, factory: map[[2]string]string{}}
	entries, err := os.ReadDir(dir)
	if err != nil {
		return nil, err
	}
	var files []*ast.File
	specs := map[string]ast.Expr{}
	for _, en := range entries {
		if !strings.HasSuffix(en.Name(), ".go") || strings.HasSuffix(en.Name(), "_test.go") {
			continue
		}
		f, err := parser.ParseFile(token.NewFileSet(), filepath.Join(dir, en.Name()), nil, 0)
		if err != nil {
			return nil, err
		}
		files = append(files, f)
		for k, v := range valueSpecs(f) {
			specs[k] = v
		}
		for _, d := range f.Decls {
			switch d := d.(type) {
			case *ast.FuncDecl:
				if d.Recv == nil {
					if d.Name.Name != "init" {
						e.funcs[d.Name.Name] = d
					}
				} else {
					t := typeName(d.Recv.List[0].Type)
					if e.methods[t] == nil {
						e.methods[t] = map[string]*ast.FuncDecl{}
					}
					e.methods[t][d.Name.Name] = d
				}
			case *ast.GenDecl:
				for _, s := range d.Specs {
					if ts, ok := s.(*ast.TypeSpec); ok {
						if st, ok := ts.Type.(*ast.StructType); ok {
							for _, fl := range st.Fields.List {
								if len(fl.Names) == 0 {
									e.embeds[ts.Name.Name] = append(e.embeds[ts.Name.Name], typeName(fl.Type))
								}
							}
						}
					}
				}
			}
		}
	}
	// Go initialises package vars before any init, each after the vars it reads
	var initVar func(name string) any
	visiting := map[string]bool{}
	initVar = func(name string) any {
		if v, ok := e.vars[name]; ok {
			return v
		}
		if visiting[name] {
			return fmt.Errorf("initialisation cycle at %s", name)
		}
		visiting[name] = true
		v, err := e.evalIn(specs[name], &frame{lookup: initVar})
		if err != nil {
			v = fmt.Errorf("package var %s: %v", name, err)
		}
		e.vars[name] = v
		return v
	}
	for _, name := range sortedAny(specs) {
		initVar(name)
	}
	// inits in file order (go/build sorts by name), statement by statement; a statement that cannot run leaves the package vars it assigns unreadable
	sort.Slice(files, func(i, j int) bool { return files[i].Name.Pos() < files[j].Name.Pos() })
	for _, f := range files {
		for _, d := range f.Decls {
			fn, ok := d.(*ast.FuncDecl)
			if !ok || fn.Recv != nil || fn.Name.Name != "init" {
				continue
			}
			for _, st := range fn.Body.List {
				if _, _, err := e.exec([]ast.Stmt{st}, &frame{locals: map[string]any{}}); err != nil {
					ast.Inspect(st, func(n ast.Node) bool {
						if a, ok := n.(*ast.AssignStmt); ok && a.Tok == token.ASSIGN {
							for _, l := range a.Lhs {
								if id, ok := l.(*ast.Ident); ok {
									if _, pkg := e.vars[id.Name]; pkg {
										e.vars[id.Name] = fmt.Errorf("package var %s: init: %v", id.Name, err)
									}
								}
							}
						}
						return true
					})
				}
			}
		}
	}
	return e, nil
}

// frame: one function activation's locals; lookup resolves package vars during their initialisation.
type frame struct {
	locals map[string]any
	lookup func(string) any
}

type evalError struct{ msg string }

func (e *evalError) Error() string { return e.msg }

func (e *soongEval) fail(n ast.Node, format string, a ...any) error {
	return &evalError{fmt.Sprintf("%s: %s", e.dir, fmt.Sprintf(format, a...))}
}

func (e *soongEval) evalIn(x ast.Expr, fr *frame) (any, error) {
	if x == nil {
		return nil, fmt.Errorf("no value")
	}
	if fr.locals == nil {
		fr.locals = map[string]any{}
	}
	return e.eval(x, fr)
}

// exec runs statements; returned = a return statement ran, with its first result.
func (e *soongEval) exec(list []ast.Stmt, fr *frame) (ret any, returned bool, err error) {
	for _, st := range list {
		switch s := st.(type) {
		case *ast.AssignStmt:
			// v, ok := m[k]
			if ix, ok := s.Rhs[0].(*ast.IndexExpr); ok && len(s.Lhs) == 2 && len(s.Rhs) == 1 && s.Tok == token.DEFINE {
				v, found, err := e.index(ix, fr)
				if err != nil {
					return nil, false, err
				}
				for i, val := range []any{v, found} {
					if id, ok := s.Lhs[i].(*ast.Ident); ok && id.Name != "_" {
						fr.locals[id.Name] = val
					}
				}
				continue
			}
			if len(s.Lhs) != len(s.Rhs) {
				return nil, false, e.fail(s, "multi-value assignment")
			}
			for i, l := range s.Lhs {
				id, ok := l.(*ast.Ident)
				if !ok {
					return nil, false, e.fail(s, "assignment to a non-identifier")
				}
				v, err := e.eval(s.Rhs[i], fr)
				if err != nil {
					return nil, false, err
				}
				if _, local := fr.locals[id.Name]; s.Tok == token.DEFINE || local {
					fr.locals[id.Name] = v
				} else if _, pkg := e.vars[id.Name]; pkg && s.Tok == token.ASSIGN {
					e.vars[id.Name] = v
				} else {
					return nil, false, e.fail(s, "assignment to unknown %s", id.Name)
				}
			}
		case *ast.ExprStmt:
			if _, err := e.eval(s.X, fr); err != nil {
				return nil, false, err
			}
		case *ast.IfStmt:
			// the init's names stay visible after the if: no evaluated code reuses one
			if s.Init != nil {
				if _, _, err := e.exec([]ast.Stmt{s.Init}, fr); err != nil {
					return nil, false, err
				}
			}
			c, err := e.eval(s.Cond, fr)
			if err != nil {
				return nil, false, err
			}
			b, ok := c.(bool)
			if !ok {
				return nil, false, e.fail(s, "if condition is not a bool")
			}
			var body []ast.Stmt
			if b {
				body = s.Body.List
			} else if s.Else != nil {
				body = []ast.Stmt{s.Else}
			}
			if ret, returned, err = e.exec(body, fr); err != nil || returned {
				return ret, returned, err
			}
		case *ast.BlockStmt:
			if ret, returned, err = e.exec(s.List, fr); err != nil || returned {
				return ret, returned, err
			}
		case *ast.SwitchStmt:
			if s.Init != nil || s.Tag == nil {
				return nil, false, e.fail(s, "switch without a tag")
			}
			tag, err := e.eval(s.Tag, fr)
			if err != nil {
				return nil, false, err
			}
			var body []ast.Stmt
			found := false
			for _, c := range s.Body.List {
				cc := c.(*ast.CaseClause)
				if cc.List == nil {
					if !found {
						body = cc.Body
					}
					continue
				}
				for _, x := range cc.List {
					v, err := e.eval(x, fr)
					if err != nil {
						return nil, false, err
					}
					if v == tag && !found {
						body, found = cc.Body, true
					}
				}
			}
			if ret, returned, err = e.exec(body, fr); err != nil || returned {
				return ret, returned, err
			}
		case *ast.RangeStmt:
			over, err := e.eval(s.X, fr)
			if err != nil {
				return nil, false, err
			}
			bind := func(x ast.Expr, v any) {
				if id, ok := x.(*ast.Ident); ok && id.Name != "_" {
					fr.locals[id.Name] = v
				}
			}
			switch o := over.(type) {
			case []string:
				for i, v := range o {
					bind(s.Key, i)
					bind(s.Value, v)
					if ret, returned, err = e.exec(s.Body.List, fr); err != nil || returned {
						return ret, returned, err
					}
				}
			case map[string]any:
				for _, k := range sortedAny(o) {
					v, err := e.force(o[k])
					if err != nil {
						return nil, false, err
					}
					bind(s.Key, k)
					bind(s.Value, v)
					if ret, returned, err = e.exec(s.Body.List, fr); err != nil || returned {
						return ret, returned, err
					}
				}
			default:
				return nil, false, e.fail(s, "range over %T", over)
			}
		case *ast.ReturnStmt:
			if len(s.Results) == 0 {
				return nil, true, nil
			}
			v, err := e.eval(s.Results[0], fr)
			return v, true, err
		default:
			return nil, false, e.fail(s, "statement %T", st)
		}
	}
	return nil, false, nil
}

func (e *soongEval) eval(x ast.Expr, fr *frame) (any, error) {
	switch x := x.(type) {
	case *ast.BasicLit:
		switch x.Kind {
		case token.STRING:
			return strconv.Unquote(x.Value)
		case token.INT:
			return strconv.Atoi(x.Value)
		}
		return nil, e.fail(x, "literal %s", x.Value)
	case *ast.ParenExpr:
		return e.eval(x.X, fr)
	case *ast.Ident:
		switch x.Name {
		case "true":
			return true, nil
		case "false":
			return false, nil
		case "nil":
			return nil, nil
		}
		if v, ok := fr.locals[x.Name]; ok {
			return v, nil
		}
		var v any
		if fr.lookup != nil {
			v = fr.lookup(x.Name)
		} else if pv, ok := e.vars[x.Name]; ok {
			v = pv
		} else {
			return nil, e.fail(x, "unknown identifier %s", x.Name)
		}
		if err, bad := v.(error); bad {
			return nil, err
		}
		return v, nil
	case *ast.UnaryExpr:
		v, err := e.eval(x.X, fr)
		if err != nil {
			return nil, err
		}
		switch x.Op {
		case token.NOT:
			b, ok := v.(bool)
			if !ok {
				return nil, e.fail(x, "! on %T", v)
			}
			return !b, nil
		case token.AND:
			return v, nil
		}
		return nil, e.fail(x, "unary %s", x.Op)
	case *ast.BinaryExpr:
		l, err := e.eval(x.X, fr)
		if err != nil {
			return nil, err
		}
		// && and || short-circuit as Go's
		if lb, ok := l.(bool); ok && (x.Op == token.LAND && !lb || x.Op == token.LOR && lb) {
			return lb, nil
		}
		r, err := e.eval(x.Y, fr)
		if err != nil {
			return nil, err
		}
		switch x.Op {
		case token.ADD:
			ls, lok := l.(string)
			rs, rok := r.(string)
			if !lok || !rok {
				return nil, e.fail(x, "+ on %T, %T", l, r)
			}
			return ls + rs, nil
		case token.EQL:
			return l == r, nil
		case token.NEQ:
			return l != r, nil
		case token.LAND, token.LOR:
			if rb, ok := r.(bool); ok {
				return rb, nil
			}
		case token.LSS, token.GTR, token.LEQ, token.GEQ:
			li, lok := l.(int)
			ri, rok := r.(int)
			if !lok || !rok {
				return nil, e.fail(x, "%s on %T, %T", x.Op, l, r)
			}
			return map[token.Token]bool{token.LSS: li < ri, token.GTR: li > ri, token.LEQ: li <= ri, token.GEQ: li >= ri}[x.Op], nil
		}
		return nil, e.fail(x, "binary %s", x.Op)
	case *ast.CompositeLit:
		switch t := x.Type.(type) {
		case *ast.ArrayType:
			out := []string{}
			for _, el := range x.Elts {
				v, err := e.eval(el, fr)
				if err != nil {
					return nil, err
				}
				if v == nil {
					// a missing map key (index): Go's zero string
					v = ""
				}
				s, ok := v.(string)
				if !ok {
					return nil, e.fail(el, "list element %T", v)
				}
				out = append(out, s)
			}
			return out, nil
		case *ast.MapType:
			out := map[string]any{}
			for _, el := range x.Elts {
				kv := el.(*ast.KeyValueExpr)
				k, err := e.eval(kv.Key, fr)
				if err != nil {
					return nil, err
				}
				// a value is evaluated only when read: map literals hold flags of every variant, some built from what this build never reads
				ks, ok := k.(string)
				if !ok {
					return nil, e.fail(kv, "map key %T", k)
				}
				out[ks] = lazy{kv.Value, fr}
			}
			return out, nil
		case *ast.Ident:
			out := map[string]any{structType: t.Name}
			for _, el := range x.Elts {
				kv, ok := el.(*ast.KeyValueExpr)
				if !ok {
					return nil, e.fail(el, "positional struct literal")
				}
				v, err := e.eval(kv.Value, fr)
				if err != nil {
					return nil, err
				}
				out[kv.Key.(*ast.Ident).Name] = v
			}
			return out, nil
		}
		return nil, e.fail(x, "composite literal %T", x.Type)
	case *ast.IndexExpr:
		v, _, err := e.index(x, fr)
		return v, err
	case *ast.SelectorExpr:
		if isIdent(x.X, "runtime") && x.Sel.Name == "GOOS" {
			return "linux", nil
		}
		v, err := e.eval(x.X, fr)
		if err != nil {
			return nil, err
		}
		s, ok := v.(map[string]any)
		if !ok {
			return nil, e.fail(x, "field %s of %T", x.Sel.Name, v)
		}
		f, ok := s[x.Sel.Name]
		if !ok {
			// an embedded struct the literal leaves unset: its zero value, which keeps its type for method calls (t.toolchainLinuxX8664.Cflags())
			if typ, _ := s[structType].(string); slices.Contains(e.embeds[typ], x.Sel.Name) {
				return map[string]any{structType: x.Sel.Name}, nil
			}
			return nil, e.fail(x, "no field %s", x.Sel.Name)
		}
		return f, nil
	case *ast.SliceExpr:
		v, err := e.eval(x.X, fr)
		if err != nil {
			return nil, err
		}
		l, ok := strs(v)
		if !ok || x.Slice3 {
			return nil, e.fail(x, "slice of %T", v)
		}
		lo, hi := 0, len(l)
		for _, b := range []struct {
			x   ast.Expr
			dst *int
		}{{x.Low, &lo}, {x.High, &hi}} {
			if b.x == nil {
				continue
			}
			i, err := e.eval(b.x, fr)
			if err != nil {
				return nil, err
			}
			n, ok := i.(int)
			if !ok {
				return nil, e.fail(x, "slice index %T", i)
			}
			*b.dst = n
		}
		if lo < 0 || lo > hi || hi > len(l) {
			return nil, e.fail(x, "slice [%d:%d] of %d elements (Go would panic)", lo, hi, len(l))
		}
		return l[lo:hi], nil
	case *ast.CallExpr:
		return e.call(x, fr)
	}
	return nil, e.fail(x, "expression %T", x)
}

// index: m[k] of a map literal and whether k is in it; a missing key gives nil, Go's zero value ("" or an empty list, as the reader uses it).
func (e *soongEval) index(x *ast.IndexExpr, fr *frame) (any, bool, error) {
	m, err := e.eval(x.X, fr)
	if err != nil {
		return nil, false, err
	}
	k, err := e.eval(x.Index, fr)
	if err != nil {
		return nil, false, err
	}
	mm, ok := m.(map[string]any)
	ks, kok := k.(string)
	if !ok || !kok {
		return nil, false, e.fail(x, "index of %T by %T", m, k)
	}
	v, ok := mm[ks]
	if !ok {
		return nil, false, nil
	}
	v, err = e.force(v)
	return v, true, err
}

// lazy: a map literal's value, evaluated when read.
type lazy struct {
	x  ast.Expr
	fr *frame
}

func (e *soongEval) force(v any) (any, error) {
	if l, ok := v.(lazy); ok {
		return e.eval(l.x, l.fr)
	}
	return v, nil
}

func sortedAny[V any](m map[string]V) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}

// structType: the key under which a struct literal's value keeps its type name (no Go field is named so).
const structType = " type"

func (e *soongEval) args(c *ast.CallExpr, fr *frame) ([]any, error) {
	var out []any
	for _, a := range c.Args {
		v, err := e.eval(a, fr)
		if err != nil {
			return nil, err
		}
		out = append(out, v)
	}
	return out, nil
}

func strs(v any) ([]string, bool) {
	switch v := v.(type) {
	case nil:
		return nil, true
	case []string:
		return v, true
	}
	return nil, false
}

func (e *soongEval) call(c *ast.CallExpr, fr *frame) (any, error) {
	switch fn := c.Fun.(type) {
	case *ast.Ident:
		switch fn.Name {
		case "append":
			args, err := e.args(c, fr)
			if err != nil {
				return nil, err
			}
			base, ok := strs(args[0])
			if !ok {
				return nil, e.fail(c, "append to %T", args[0])
			}
			out := slices.Clone(base)
			for i, a := range args[1:] {
				if c.Ellipsis.IsValid() && i == len(args)-2 {
					l, ok := strs(a)
					if !ok {
						return nil, e.fail(c, "append ... of %T", a)
					}
					out = append(out, l...)
				} else if s, ok := a.(string); ok {
					out = append(out, s)
				} else if a == nil {
					// a missing map key (index): Go's zero string, the only element type a one-value append takes here
					out = append(out, "")
				} else {
					return nil, e.fail(c, "append of %T", a)
				}
			}
			return out, nil
		case "len":
			args, err := e.args(c, fr)
			if err != nil {
				return nil, err
			}
			if s, ok := args[0].(string); ok {
				return len(s), nil
			}
			l, ok := strs(args[0])
			if !ok {
				return nil, e.fail(c, "len(%T)", args[0])
			}
			return len(l), nil
		case "panic":
			return nil, e.fail(c, "the evaluated code panics (Soong would stop here)")
		case "registerToolchainFactory":
			if len(c.Args) == 3 {
				f, fok := c.Args[2].(*ast.Ident)
				osSel, ook := c.Args[0].(*ast.SelectorExpr)
				arch, aok := c.Args[1].(*ast.SelectorExpr)
				if fok && ook && aok {
					e.factory[[2]string{osSel.Sel.Name, arch.Sel.Name}] = f.Name
					return nil, nil
				}
			}
			return nil, e.fail(c, "registerToolchainFactory(<os>, <arch>, <func>) of another shape")
		}
		if d := e.funcs[fn.Name]; d != nil {
			args, err := e.args(c, fr)
			if err != nil {
				return nil, err
			}
			return e.invoke(d, nil, args)
		}
	case *ast.SelectorExpr:
		pkg, _ := fn.X.(*ast.Ident)
		switch {
		case pkg != nil && pkg.Name == "strings" && fn.Sel.Name == "Join":
			args, err := e.args(c, fr)
			if err != nil {
				return nil, err
			}
			l, ok := strs(args[0])
			sep, sok := args[1].(string)
			if !ok || !sok {
				return nil, e.fail(c, "strings.Join(%T, %T)", args[0], args[1])
			}
			return strings.Join(l, sep), nil
		case pkg != nil && pkg.Name == "strings" && fn.Sel.Name == "Split":
			args, err := e.args(c, fr)
			if err != nil {
				return nil, err
			}
			s, ok := args[0].(string)
			sep, sok := args[1].(string)
			if !ok || !sok {
				return nil, e.fail(c, "strings.Split(%T, %T)", args[0], args[1])
			}
			return strings.Split(s, sep), nil
		case pkg != nil && pkg.Name == "slices" && fn.Sel.Name == "Clone":
			args, err := e.args(c, fr)
			if err != nil {
				return nil, err
			}
			l, ok := strs(args[0])
			if !ok {
				return nil, e.fail(c, "slices.Clone(%T)", args[0])
			}
			return slices.Clone(l), nil
		case pkg != nil && pkg.Name == "pctx":
			switch fn.Sel.Name {
			case "StaticVariable":
				args, err := e.args(c, fr)
				if err != nil {
					return nil, err
				}
				name, ok := args[0].(string)
				if !ok {
					return nil, e.fail(c, "pctx.StaticVariable name %T", args[0])
				}
				e.statics[name] = args[1]
			case "SourcePathVariable":
				// a tree-relative path Soong resolves against the source root; its value may reference other variables, expanded when read
				args, err := e.args(c, fr)
				if err != nil {
					return nil, err
				}
				name, ok := args[0].(string)
				path, pok := args[1].(string)
				if !ok || !pok {
					return nil, e.fail(c, "pctx.SourcePathVariable(%T, %T)", args[0], args[1])
				}
				e.statics[name] = filepath.Join(e.tree, path)
			case "VariableFunc":
				name, err := e.eval(c.Args[0], fr)
				if err != nil {
					return nil, err
				}
				lit, ok := c.Args[1].(*ast.FuncLit)
				if !ok {
					return nil, e.fail(c, "pctx.VariableFunc of a non-literal func")
				}
				e.varFuncs[name.(string)] = lit
			}
			// other pctx registrations (source paths, config methods, env overrides) hold no compile flags
			return nil, nil
		}
		// ctx.Config().<method>(...)
		if inner, ok := fn.X.(*ast.CallExpr); ok {
			if s, ok := inner.Fun.(*ast.SelectorExpr); ok && s.Sel.Name == "Config" {
				v, ok := configDefaults[fn.Sel.Name]
				if !ok {
					return nil, e.fail(c, "Config().%s: no known value for this build (soongflags.go configDefaults)", fn.Sel.Name)
				}
				return v, nil
			}
		}
		// <struct value>.<method>(...): a toolchain method calling one of an embedded struct's (t.toolchainGlibc.Cflags())
		if pkg == nil || fr.locals[pkg.Name] != nil {
			if recv, err := e.eval(fn.X, fr); err == nil {
				if s, ok := recv.(map[string]any); ok && s[structType] != nil {
					args, err := e.args(c, fr)
					if err != nil {
						return nil, err
					}
					return e.method(s, fn.Sel.Name, args...)
				}
			}
		}
	}
	return nil, e.fail(c, "call %s", exprString(c.Fun))
}

func exprString(x ast.Expr) string {
	switch x := x.(type) {
	case *ast.Ident:
		return x.Name
	case *ast.SelectorExpr:
		return exprString(x.X) + "." + x.Sel.Name
	case *ast.CallExpr:
		return exprString(x.Fun) + "()"
	}
	return fmt.Sprintf("%T", x)
}

// invoke runs a function or method declaration; recv = the receiver's struct value (nil for a function).
func (e *soongEval) invoke(d *ast.FuncDecl, recv map[string]any, args []any) (any, error) {
	fr := &frame{locals: map[string]any{}}
	if d.Recv != nil && len(d.Recv.List[0].Names) == 1 {
		fr.locals[d.Recv.List[0].Names[0].Name] = recv
	}
	i := 0
	for _, p := range d.Type.Params.List {
		for _, n := range p.Names {
			if i >= len(args) {
				return nil, e.fail(d, "%s: %d arguments", d.Name.Name, len(args))
			}
			fr.locals[n.Name] = args[i]
			i++
		}
	}
	v, returned, err := e.exec(d.Body.List, fr)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", d.Name.Name, err)
	}
	if !returned {
		return nil, e.fail(d, "%s returns nothing", d.Name.Name)
	}
	return v, nil
}

// expand: a ninja string with Soong's ${config.X} references resolved, recursively (StaticVariable values, VariableFunc results).
func (e *soongEval) expand(s string) (string, error) {
	var err error
	out := configRef.ReplaceAllStringFunc(s, func(ref string) string {
		name := configRef.FindStringSubmatch(ref)[1]
		var v any
		if st, ok := e.statics[name]; ok {
			v = st
		} else if lit, ok := e.varFuncs[name]; ok {
			var returned bool
			var ferr error
			v, returned, ferr = e.exec(lit.Body.List, &frame{locals: map[string]any{}})
			if ferr == nil && !returned {
				ferr = fmt.Errorf("VariableFunc %s returns nothing", name)
			}
			if ferr != nil {
				err = fmt.Errorf("${config.%s}: %w", name, ferr)
				return ""
			}
		} else {
			err = fmt.Errorf("%s: ${config.%s} is registered by no pctx.StaticVariable / SourcePathVariable / VariableFunc", e.dir, name)
			return ""
		}
		str, ok := v.(string)
		if !ok {
			err = fmt.Errorf("${config.%s} is %T, not a string", name, v)
			return ""
		}
		sub, serr := e.expand(str)
		if serr != nil {
			err = serr
		}
		return sub
	})
	// another package's variable (${android.X}) or a ninja rule variable: no value this evaluator knows, never passed on as text
	if err == nil && strings.Contains(out, "$") {
		err = fmt.Errorf("%s: %q keeps a reference this evaluator cannot resolve", e.dir, out)
	}
	return out, err
}

// configRef: a reference to a cc/config pctx variable: `${config.X}` from a toolchain method or another package, `${X}` inside the package's own variables (x86_linux_host.go's `--gcc-toolchain=${LinuxGccRoot}`). X may hold `-`: arm64's variant variables (Arm64armv8-aVariantCflags).
var configRef = regexp.MustCompile(`\$\{(?:config\.)?([\w-]+)\}`)

// hasToolchain: whether an init registers a toolchain factory for (os, arch identifier).
func (e *soongEval) hasToolchain(osName, arch string) bool {
	return e.factory[[2]string{osName, arch}] != ""
}

// toolchain: the value registerToolchainFactory's factory for (os, arch identifier) returns, run with arch = the given android.Arch fields.
func (e *soongEval) toolchain(osName, arch string, archFields map[string]any) (map[string]any, error) {
	name := e.factory[[2]string{osName, arch}]
	d := e.funcs[name]
	if d == nil {
		return nil, fmt.Errorf("%s: no toolchain factory registered for (%s, %s)", e.dir, osName, arch)
	}
	v, err := e.invoke(d, nil, []any{archFields})
	if err != nil {
		return nil, err
	}
	tc, ok := v.(map[string]any)
	if !ok || tc[structType] == nil {
		return nil, fmt.Errorf("%s: %s returns %T, not a toolchain literal", e.dir, name, v)
	}
	return tc, nil
}

// method: the toolchain's method result, Go's promotion through embedded structs (shallowest wins, ambiguity fails).
func (e *soongEval) method(tc map[string]any, name string, args ...any) (string, error) {
	typ := tc[structType].(string)
	for level := []string{typ}; len(level) > 0; {
		var found []*ast.FuncDecl
		var next []string
		for _, t := range level {
			if m := e.methods[t][name]; m != nil {
				found = append(found, m)
			}
			next = append(next, e.embeds[t]...)
		}
		switch len(found) {
		case 0:
			level = next
			continue
		case 1:
			v, err := e.invoke(found[0], tc, args)
			if err != nil {
				return "", err
			}
			s, ok := v.(string)
			if !ok {
				return "", fmt.Errorf("%s.%s returns %T", typ, name, v)
			}
			return s, nil
		}
		return "", fmt.Errorf("%s.%s ambiguous", typ, name)
	}
	return "", fmt.Errorf("%s has no method %s", typ, name)
}
