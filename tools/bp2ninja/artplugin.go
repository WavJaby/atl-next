// SPDX-License-Identifier: MIT

package main

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"maps"
	"os"
	"path/filepath"
	"regexp"
	"runtime"
	"slices"
	"strings"
)

// ART's Soong plugin (art/build/*.go) registers the art_cc_* module types: each is a cc module type plus load hooks, which append properties to the module as written, before its defaults are applied (android.AddLoadHook).
// Mirrored in two parts:
//   - which types exist, the cc type each factory creates and its hooks in Soong's order: read from the pinned plugin's init() and factories (parseArtTypes), so a new type or hook arrives with an ART upgrade or fails the run;
//   - what each hook appends: translated below for the environment this build gives Soong (none of the plugin's variables set, no sanitizer, no product overrides). A translation drifts silently when ART edits a hook,
//     so artMirrored pins the source text of every function and type it was derived from; a mismatch fails the run, naming what to re-derive.
// Install hooks (AddInstallHook: test lists, testcases copies) run after the build and change nothing a compile or link sees.

const artPluginDir = "art/build"

// artMirrored: sha256 (first 12 hex digits) of the source text, at the pin, of each declaration the hook translations below follow; derived from art/build at the sources.toml pin.
// On a mismatch re-read the declaration, update its translation, then its digest here (the error prints the new one).
var artMirrored = map[string]string{
	"globalFlags":                        "a0e6d4e28b81",
	"deviceFlags":                        "e4350637f44d",
	"hostFlags":                          "4a3b777463ff",
	"globalDefaults":                     "695c6177423b",
	"addImplicitFlags":                   "7e843db5bf6c",
	"customLinker":                       "d0a28e62f054",
	"prefer32Bit":                        "a8c3328a0033",
	"codegen":                            "b9d155becdd1",
	"defaultDeviceCodegenArches":         "c75018422e09",
	"installCodegenCustomizer":           "952ac76aae07",
	"moduleType":                         "590d9f75698a",
	"codegenArchProperties":              "c46d9b8bbde7",
	"CodegenSourceArchProperties":        "5b681840f1f8",
	"CodegenCommonArchProperties":        "62c296964166",
	"CodegenLibraryArchProperties":       "79d440c9a598",
	"CodegenLibraryArchStaticProperties": "def89751705d",
	"CodegenLibraryArchSharedProperties": "00c8b17b6b0a",
}

// ccFactories: the cc package constructors ART's factories call -> the module type Soong registers for them (cc/library.go, binary.go, test.go, cc.go RegisterModuleType).
// cc_defaults' defaultsFactory and cc_test's TestFactory wrap DefaultsFactory and NewTest, which ART calls directly.
var ccFactories = map[string]string{
	"LibraryFactory":       "cc_library",
	"LibraryStaticFactory": "cc_library_static",
	"LibrarySharedFactory": "cc_library_shared",
	"BinaryFactory":        "cc_binary",
	"DefaultsFactory":      "cc_defaults",
	"NewTest":              "cc_test",
	"TestLibraryFactory":   "cc_test_library",
}

// artModuleType: codegen.go's moduleType, which codegen properties a module type takes (library ones, their static / shared parts).
type artModuleType struct{ library, static, shared bool }

type artHook struct {
	name    string
	codegen artModuleType // name == "codegen"
}

// artType: one art_* module type: the cc type it builds as and its load hooks, in the order Soong runs them.
type artType struct {
	base  string
	hooks []artHook
}

type artPlugin struct {
	types map[string]artType
	// codegen arches per side: device = defaultDeviceCodegenArches of the device arch, host = supportedArches (ART_TARGET_CODEGEN_ARCHS / ART_HOST_CODEGEN_ARCHS unset)
	codegenArches [2][]string
	// android/config.go LibartImgDeviceBaseAddress / LibartImgHostBaseAddress
	baseAddress [2]string
	// ART_CLANG_PATH: cc/config/global.go ClangDefaultBase / android/config.go PrebuiltOS() / ClangDefaultVersion
	clangPath string
}

// readArtPlugin: the plugin at the pin; nil without art/build (no art_* module in the tree then).
func readArtPlugin(tree string, cfg Config) (*artPlugin, error) {
	dir := filepath.Join(tree, artPluginDir)
	entries, err := os.ReadDir(dir)
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	src := map[string][]byte{}
	for _, e := range entries {
		if strings.HasSuffix(e.Name(), ".go") && !strings.HasSuffix(e.Name(), "_test.go") {
			if src[e.Name()], err = os.ReadFile(filepath.Join(dir, e.Name())); err != nil {
				return nil, err
			}
		}
	}
	art, err := parseArtPackage(src)
	if err != nil {
		return nil, err
	}
	if err := art.checkMirrored(artMirrored); err != nil {
		return nil, err
	}
	a := &artPlugin{}
	if a.types, err = art.moduleTypes(); err != nil {
		return nil, err
	}
	supported, ok := stringList(art.values["supportedArches"])
	if !ok {
		return nil, fmt.Errorf("%s: supportedArches is not a string list", artPluginDir)
	}
	a.codegenArches = [2][]string{Device: defaultDeviceCodegenArches([]string{cfg.Arch}), Host: supported}
	if a.baseAddress, a.clangPath, err = readArtConfig(tree); err != nil {
		return nil, err
	}
	return a, readBarrierDefault(tree)
}

// artPackage: the plugin's top-level declarations, all files parsed into one FileSet.
type artPackage struct {
	fset   *token.FileSet
	src    map[string][]byte // file name -> text
	funcs  map[string]*ast.FuncDecl
	inits  []*ast.FuncDecl // every file's init()
	types  map[string]*ast.TypeSpec
	values map[string]ast.Expr
}

func parseArtPackage(src map[string][]byte) (*artPackage, error) {
	p := &artPackage{fset: token.NewFileSet(), src: src, funcs: map[string]*ast.FuncDecl{}, types: map[string]*ast.TypeSpec{}, values: map[string]ast.Expr{}}
	for _, name := range slices.Sorted(maps.Keys(src)) {
		f, err := parser.ParseFile(p.fset, name, src[name], 0)
		if err != nil {
			return nil, fmt.Errorf("%s: %v", artPluginDir, err)
		}
		maps.Copy(p.values, valueSpecs(f))
		for _, decl := range f.Decls {
			switch d := decl.(type) {
			case *ast.FuncDecl:
				switch {
				case d.Recv != nil:
				case d.Name.Name == "init":
					p.inits = append(p.inits, d)
				default:
					p.funcs[d.Name.Name] = d
				}
			case *ast.GenDecl:
				for _, s := range d.Specs {
					if ts, ok := s.(*ast.TypeSpec); ok {
						p.types[ts.Name.Name] = ts
					}
				}
			}
		}
	}
	return p, nil
}

// source: a node's text as the file has it.
func (p *artPackage) source(n ast.Node) []byte {
	from, to := p.fset.Position(n.Pos()), p.fset.Position(n.End())
	return p.src[from.Filename][from.Offset:to.Offset]
}

func artDigest(text []byte) string {
	sum := sha256.Sum256(text)
	return hex.EncodeToString(sum[:])[:12]
}

// checkMirrored fails when a declaration a translation follows differs from the one it was derived from.
func (p *artPackage) checkMirrored(want map[string]string) error {
	var bad []string
	for _, name := range slices.Sorted(maps.Keys(want)) {
		var n ast.Node
		if f, ok := p.funcs[name]; ok {
			n = f
		} else if t, ok := p.types[name]; ok {
			n = t
		}
		switch {
		case n == nil:
			bad = append(bad, name+" (gone)")
		case artDigest(p.source(n)) != want[name]:
			bad = append(bad, fmt.Sprintf("%s (now %s)", name, artDigest(p.source(n))))
		}
	}
	if len(bad) > 0 {
		return fmt.Errorf("%s: declarations the converter's ART hooks mirror changed (artplugin.go artMirrored), re-derive: %s", artPluginDir, strings.Join(bad, ", "))
	}
	return nil
}

// moduleTypes: the module types the plugin's init() functions register (android.RegisterModuleType), each resolved through its factory.
func (p *artPackage) moduleTypes() (map[string]artType, error) {
	out := map[string]artType{}
	for _, fn := range p.inits {
		var err error
		ast.Inspect(fn.Body, func(n ast.Node) bool {
			c, ok := n.(*ast.CallExpr)
			if !ok || err != nil || selName(c.Fun) != "RegisterModuleType" || len(c.Args) != 2 {
				return true
			}
			factory, ok := c.Args[1].(*ast.Ident)
			if !ok {
				err = fmt.Errorf("%s: %s registers a non-function factory", artPluginDir, goString(c.Args[0]))
				return false
			}
			var t artType
			if t, err = p.factory(factory.Name, map[string]bool{}); err == nil {
				out[goString(c.Args[0])] = t
			}
			return false
		})
		if err != nil {
			return nil, err
		}
	}
	return out, nil
}

func selName(e ast.Expr) string {
	if s, ok := e.(*ast.SelectorExpr); ok {
		return s.Sel.Name
	}
	return ""
}

// factory: what a module factory makes, read statement by statement; any statement shape not listed fails, as an unread hook would be silently lost.
//   - `module := cc.X(…)` (or `cc.X(…).Init()`): the cc type ccFactories names; `module := localFactory()`: that factory's type and hooks, ahead of this one's
//   - `android.AddLoadHook(module, hook)` or with `func(ctx) { codegen(ctx, c, T) }`, `installCodegenCustomizer(module, T)`: a hook
//   - `android.AddInstallHook(…)`: skipped (install time); `c := &codegenProperties{}`, `return module`: nothing to read
func (p *artPackage) factory(name string, seen map[string]bool) (artType, error) {
	var t artType
	fn := p.funcs[name]
	if fn == nil || seen[name] {
		return t, fmt.Errorf("%s: factory %s not found", artPluginDir, name)
	}
	seen[name] = true
	bad := func(n ast.Node) error {
		return fmt.Errorf("%s: factory %s: statement the converter cannot read: %s", artPluginDir, name, p.source(n))
	}
	for _, st := range fn.Body.List {
		switch s := st.(type) {
		case *ast.ReturnStmt:
		case *ast.AssignStmt:
			if len(s.Rhs) != 1 {
				return t, bad(s)
			}
			call, ok := s.Rhs[0].(*ast.CallExpr)
			if !ok {
				// c := &codegenProperties{}: the struct a codegen hook reads
				continue
			}
			if selName(call.Fun) == "Init" {
				if inner, ok := call.Fun.(*ast.SelectorExpr).X.(*ast.CallExpr); ok {
					call = inner
				}
			}
			switch f := call.Fun.(type) {
			case *ast.SelectorExpr:
				if pkg, ok := f.X.(*ast.Ident); !ok || pkg.Name != "cc" || ccFactories[f.Sel.Name] == "" {
					return t, fmt.Errorf("%s: factory %s: module made by an unknown constructor %s", artPluginDir, name, f.Sel.Name)
				}
				t.base = ccFactories[f.Sel.Name]
			case *ast.Ident:
				inner, err := p.factory(f.Name, seen)
				if err != nil {
					return t, err
				}
				t.base, t.hooks = inner.base, append(t.hooks, inner.hooks...)
			default:
				return t, bad(s)
			}
		case *ast.ExprStmt:
			call, ok := s.X.(*ast.CallExpr)
			if !ok {
				return t, bad(s)
			}
			switch {
			case selName(call.Fun) == "AddInstallHook":
			case selName(call.Fun) == "AddLoadHook" && len(call.Args) == 2:
				h, err := p.loadHook(call.Args[1])
				if err != nil {
					return t, fmt.Errorf("%s: factory %s: %v", artPluginDir, name, err)
				}
				t.hooks = append(t.hooks, h)
			case isIdent(call.Fun, "installCodegenCustomizer") && len(call.Args) == 2:
				mt, err := p.moduleType(call.Args[1])
				if err != nil {
					return t, err
				}
				t.hooks = append(t.hooks, artHook{name: "codegen", codegen: mt})
			default:
				return t, bad(s)
			}
		default:
			return t, bad(s)
		}
	}
	if t.base == "" {
		return t, fmt.Errorf("%s: factory %s makes no module", artPluginDir, name)
	}
	return t, nil
}

// artHooks: the translated load hooks taking no arguments -> the properties each appends (nil: none in this environment); codegen, which takes the module's own, is artPlugin.codegen.
// A hook outside it fails the run.
var artHooks = map[string]func(*artPlugin) *Obj{
	"addImplicitFlags": (*artPlugin).implicitFlags,
	"globalDefaults":   (*artPlugin).globalDefaults,
	// CUSTOM_TARGET_LINKER unset -> no dynamic_linker
	"customLinker": nil,
	// HOST_PREFER_32_BIT unset -> no host compile_multilib
	"prefer32Bit": nil,
}

func (p *artPackage) loadHook(e ast.Expr) (artHook, error) {
	switch h := e.(type) {
	case *ast.Ident:
		if _, ok := artHooks[h.Name]; !ok {
			return artHook{}, fmt.Errorf("load hook %s has no translation (artplugin.go artHooks)", h.Name)
		}
		return artHook{name: h.Name}, nil
	case *ast.FuncLit:
		// func(ctx android.LoadHookContext) { codegen(ctx, c, T) }
		if len(h.Body.List) == 1 {
			if es, ok := h.Body.List[0].(*ast.ExprStmt); ok {
				if c, ok := es.X.(*ast.CallExpr); ok && isIdent(c.Fun, "codegen") && len(c.Args) == 3 {
					mt, err := p.moduleType(c.Args[2])
					return artHook{name: "codegen", codegen: mt}, err
				}
			}
		}
	}
	return artHook{}, fmt.Errorf("load hook of a shape the converter cannot read")
}

// moduleType: a codegen.go `x = moduleType{library, static, shared}` value.
func (p *artPackage) moduleType(e ast.Expr) (artModuleType, error) {
	id, ok := e.(*ast.Ident)
	if !ok {
		return artModuleType{}, fmt.Errorf("%s: codegen module type is not a named value", artPluginDir)
	}
	lit, ok := p.values[id.Name].(*ast.CompositeLit)
	var b []bool
	if ok {
		for _, el := range lit.Elts {
			if v, ok := el.(*ast.Ident); ok && (v.Name == "true" || v.Name == "false") {
				b = append(b, v.Name == "true")
			}
		}
	}
	if len(b) != 3 {
		return artModuleType{}, fmt.Errorf("%s: %s is not a moduleType{library, static, shared} literal", artPluginDir, id.Name)
	}
	return artModuleType{b[0], b[1], b[2]}, nil
}

// defaultDeviceCodegenArches (codegen.go): the device arches plus the 32-bit arch each 64-bit one runs (arm64 -> arm, x86_64 -> x86), sorted.
// The device side here builds one arch; an AOSP product's secondary arch is that same pair, so the result matches a two-arch product's.
func defaultDeviceCodegenArches(device []string) []string {
	set := map[string]bool{}
	for _, s := range device {
		set[s] = true
		switch s {
		case "arm64":
			set["arm"] = true
		case "x86_64":
			set["x86"] = true
		}
	}
	return sortedKeys(set)
}

// readArtConfig: the Soong config values the hooks read that live outside the plugin.
func readArtConfig(tree string) (base [2]string, clang string, err error) {
	path := filepath.Join(tree, "build/soong/android/config.go")
	f, err := parser.ParseFile(token.NewFileSet(), path, nil, 0)
	if err != nil {
		return base, "", err
	}
	methods := map[string]*ast.FuncDecl{}
	for _, d := range f.Decls {
		if fd, ok := d.(*ast.FuncDecl); ok && fd.Recv != nil {
			methods[fd.Name.Name] = fd
		}
	}
	for side, m := range [2]string{Device: "LibartImgDeviceBaseAddress", Host: "LibartImgHostBaseAddress"} {
		fd := methods[m]
		if fd != nil && len(fd.Body.List) == 1 {
			if r, ok := fd.Body.List[0].(*ast.ReturnStmt); ok && len(r.Results) == 1 {
				base[side] = goString(r.Results[0])
			}
		}
		if base[side] == "" {
			return base, "", fmt.Errorf("%s: %s does not return a string literal", path, m)
		}
	}
	prebuilt := prebuiltOS(methods["PrebuiltOS"], runtime.GOOS, runtime.GOARCH)
	if prebuilt == "" {
		return base, "", fmt.Errorf("%s: PrebuiltOS has no literal for %s/%s", path, runtime.GOOS, runtime.GOARCH)
	}
	gpath := filepath.Join(tree, "build/soong/cc/config/global.go")
	g, err := parser.ParseFile(token.NewFileSet(), gpath, nil, 0)
	if err != nil {
		return base, "", err
	}
	vals := valueSpecs(g)
	cbase, cver := goString(vals["ClangDefaultBase"]), goString(vals["ClangDefaultVersion"])
	if cbase == "" || cver == "" {
		return base, "", fmt.Errorf("%s: ClangDefaultBase / ClangDefaultVersion not found", gpath)
	}
	return base, filepath.Join(cbase, prebuilt, cver), nil
}

// prebuiltOS: what android/config.go PrebuiltOS returns on this build machine: `switch runtime.GOOS { case "linux": switch runtime.GOARCH { case "amd64": return "linux-x86" … } }`.
func prebuiltOS(fd *ast.FuncDecl, goos, goarch string) string {
	if fd == nil {
		return ""
	}
	var arm func(sw *ast.SwitchStmt, want string) []ast.Stmt
	arm = func(sw *ast.SwitchStmt, want string) []ast.Stmt {
		for _, s := range sw.Body.List {
			if cc, ok := s.(*ast.CaseClause); ok && slices.ContainsFunc(cc.List, func(e ast.Expr) bool { return goString(e) == want }) {
				return cc.Body
			}
		}
		return nil
	}
	for _, s := range fd.Body.List {
		if sw, ok := s.(*ast.SwitchStmt); ok {
			for _, s := range arm(sw, goos) {
				if inner, ok := s.(*ast.SwitchStmt); ok {
					for _, s := range arm(inner, goarch) {
						if r, ok := s.(*ast.ReturnStmt); ok && len(r.Results) == 1 {
							return goString(r.Results[0])
						}
					}
				}
			}
		}
	}
	return ""
}

// readBarrierExpr: build/make/core/soong_config.mk's derivation of Soong's ArtUseReadBarrier product variable (android/config.go ArtUseReadBarrier).
// This build has no product makefile, so PRODUCT_ART_USE_READ_BARRIER is unset and the expression is true: globalFlags takes its read-barrier branch.
var readBarrierExpr = regexp.MustCompile(`\$\(call add_json_bool,\s*ArtUseReadBarrier,\s*\$\(call invert_bool,\$\(filter false,\$\(PRODUCT_ART_USE_READ_BARRIER\)\)\)\)`)

func readBarrierDefault(tree string) error {
	path := filepath.Join(tree, "build/make/core/soong_config.mk")
	b, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	if !readBarrierExpr.Match(b) {
		return fmt.Errorf("%s: ArtUseReadBarrier is no longer `invert_bool(filter false, PRODUCT_ART_USE_READ_BARRIER)`: re-derive the read-barrier branch of artplugin.go globalFlags", path)
	}
	return nil
}

// loadArt registers the plugin's types as their cc types and runs the hooks of every module of one, on each side's copy (nil plugin: nothing to do).
func (g *Graph) loadArt(a *artPlugin) {
	if a == nil {
		return
	}
	for name, t := range a.types {
		g.baseTypes[name] = t.base
	}
	for _, mods := range g.mods {
		for _, list := range mods {
			for _, m := range list {
				if _, ok := a.types[m.Type]; ok {
					a.apply(m)
				}
			}
		}
	}
}

// apply runs a module's load hooks on the properties it was written with (Soong's AppendProperties = merge: lists append, blocks merge).
func (a *artPlugin) apply(m *Module) {
	t := a.types[m.Type]
	p := m.Props
	for _, h := range t.hooks {
		if h.name == "codegen" {
			p = a.codegen(p, h.codegen)
		} else if f := artHooks[h.name]; f != nil {
			p = merge(p, f(a))
		}
	}
	m.Props = p
}

// sideBlocks: the `target: {…}` block a hook's Target.Android / Target.Host struct fills.
var sideBlocks = [2]string{Device: "android", Host: "host"}

// block: props nested under the path of blocks, e.g. block(p, "target", "android") = { target: { android: p } }.
func block(p *Obj, path ...string) *Obj {
	for i := len(path) - 1; i >= 0; i-- {
		o := newObj()
		o.set(path[i], p)
		p = o
	}
	return p
}

// lists: an Obj of the given list properties, empty ones left out (appending nothing is no change).
func lists(kv ...any) *Obj {
	o := newObj()
	for i := 0; i+1 < len(kv); i += 2 {
		if l := kv[i+1].([]string); len(l) > 0 {
			vals := make([]Val, len(l))
			for j, s := range l {
				vals[j] = s
			}
			o.set(kv[i].(string), vals)
		}
	}
	return o
}

// implicitFlags: addImplicitFlags, ART_TARGET_LINUX unset -> ART's Android target.
// The device side is built from the android blocks against bionic's API, which is ART's Android target configuration; ART_TARGET_LINUX selects its non-Android Linux target (globals.h kIsTargetLinux: MemMap without ashmem), a choice for when libart is built.
func (a *artPlugin) implicitFlags() *Obj {
	return block(lists("cflags", []string{"-DART_TARGET", "-DART_TARGET_ANDROID"}), "target", sideBlocks[Device])
}

// globalDefaults: globalFlags + deviceFlags + hostFlags (art_global_defaults) for an environment that sets none of their variables and builds with no sanitizer:
//   - -O3: ART_NDEBUG_OPT_FLAG's default. GC type CMC: ART_DEFAULT_GC_TYPE's default; ART_TEST_DEBUG_GC, ART_USE_RESTRICTED_MODE, ART_USE_SIMULATOR, ART_HEAP_POISONING, ART_USE_CXX_INTERPRETER unset.
//   - read barrier: ArtUseReadBarrier is true (readBarrierExpr) and ART_USE_READ_BARRIER is not "false" -> Baker (ART_READ_BARRIER_TYPE's default), generational GC (ART_USE_GENERATIONAL_CC/GC not "false"), TLAB; not forced (ART_USE_READ_BARRIER not "true").
//   - ART_FORCE_TRY_PREDICATED_SIMD, ART_ENABLE_ADDRESS_SANITIZER, ART_DEX_FILE_ACCESS_TRACKING unset; USE_D8_DESUGAR not "false".
//   - no sanitizer: stack overflow gaps 8192 and frame size limit 1744 on both sides (the plugin's non-sanitizer constants).
//   - base address deltas: LIBART_IMG_{TARGET,HOST}_{MIN,MAX}_BASE_ADDRESS_DELTA defaults.
func (a *artPlugin) globalDefaults() *Obj {
	barrier := []string{"-DART_USE_READ_BARRIER=1", "-DART_READ_BARRIER_TYPE_IS_BAKER=1"}
	cflags := slices.Concat([]string{"-O3", "-DART_DEFAULT_GC_TYPE_IS_CMC"}, barrier, []string{"-DART_USE_GENERATIONAL_GC=1", "-DART_USE_TLAB=1"})
	for _, arch := range []string{"arm", "arm64", "riscv64", "x86", "x86_64"} {
		cflags = append(cflags, "-DART_STACK_OVERFLOW_GAP_"+arch+"=8192")
	}
	cflags = append(cflags, "-DART_PAGE_SIZE_AGNOSTIC=1", "-DUSE_D8_DESUGAR=1")
	side := func(s Side) []string {
		return []string{"-Wframe-larger-than=1744", "-DART_FRAME_SIZE_LIMIT=1744", "-DART_BASE_ADDRESS=" + a.baseAddress[s],
			"-DART_BASE_ADDRESS_MIN_DELTA=(-0x1000000)", "-DART_BASE_ADDRESS_MAX_DELTA=0x1000000"}
	}
	host := append(side(Host), fmt.Sprintf("-DART_CLANG_PATH=%q", a.clangPath))
	p := lists("cflags", cflags, "asflags", barrier)
	p = merge(p, block(lists("cflags", side(Device)), "target", sideBlocks[Device]))
	return merge(p, block(lists("cflags", host), "target", sideBlocks[Host]))
}

// codegen: the module's `codegen: { <arch>: {…} }` blocks of each side's codegen arches, appended to its target.android / target.host block, arch by arch; srcs once, after the arches, deduplicated (codegen.go addCodegenProperties).
// Only the properties codegen.go's structs declare are read: cflags, cppflags for every type; static_libs, export_static_lib_headers for libraries; static.whole_static_libs, shared.shared_libs, shared.export_shared_lib_headers for their static / shared parts.
func (a *artPlugin) codegen(p *Obj, t artModuleType) *Obj {
	cg := p.obj("codegen")
	p = removeKey(p, "codegen")
	for side, arches := range a.codegenArches {
		var srcs []string
		for _, arch := range arches {
			c := cg.obj(arch)
			add := lists("cflags", c.strs("cflags"), "cppflags", c.strs("cppflags"))
			if t.library {
				add = merge(add, lists("static_libs", c.strs("static_libs"), "export_static_lib_headers", c.strs("export_static_lib_headers")))
				if t.static {
					add = merge(add, block(lists("whole_static_libs", c.obj("static").strs("whole_static_libs")), "static"))
				}
				if t.shared {
					s := c.obj("shared")
					add = merge(add, block(lists("shared_libs", s.strs("shared_libs"), "export_shared_lib_headers", s.strs("export_shared_lib_headers")), "shared"))
				}
			}
			srcs = append(srcs, c.strs("srcs")...)
			p = merge(p, block(add, "target", sideBlocks[side]))
		}
		p = merge(p, block(lists("srcs", firstUnique(srcs)), "target", sideBlocks[side]))
	}
	return p
}

// firstUnique: android.FirstUniqueStrings.
func firstUnique(in []string) []string {
	seen := map[string]bool{}
	var out []string
	for _, s := range in {
		if !seen[s] {
			seen[s] = true
			out = append(out, s)
		}
	}
	return out
}
