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
// The device side takes the bionic toolchain for (Android, arch); the host side the global lists only (Soong's linux_glibc toolchain flags are not applied: host modules are build tools).
// Every flag passes keepFlag; what it drops is in Skipped.
type GlobalFlags struct {
	Head, Tail     [2][]string // Common up to <Hod>GlobalCflags, and tc.ToolchainCflags: ExternalCflags goes between them
	External       []string    // ExternalCflags, for third-party module dirs (ThirdParty)
	Conly, Cpp, As [2][]string
	NoOverride     []string // NoOverrideGlobalCflags + NoOverride64GlobalCflags (64-bit arches)
	NoOverrideExt  []string // NoOverrideExternalGlobalCflags, third-party only
	ThirdParty     ThirdParty
	Skipped        []string // dropped by keepFlag, unique, sorted
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

// readGlobalFlags: GlobalFlags for the device arch cfg (its arches.toml variant and Soong's feature list for it, DeviceArch).
func readGlobalFlags(tree string, cfg Config, dev DeviceArch) (GlobalFlags, error) {
	var g GlobalFlags
	ev, err := newSoongEval(filepath.Join(tree, "build/soong/cc/config"))
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
		for _, f := range strings.Fields(s) {
			if keepFlag(f) {
				out = append(out, f)
			} else {
				skipped[f] = true
			}
		}
		return out, nil
	}
	ident, err := archIdent(tree, cfg.Arch)
	if err != nil {
		return g, err
	}
	tc, err := ev.toolchain("Android", ident, map[string]any{"ArchVariant": dev.Variant, "CpuVariant": "", "ArchFeatures": dev.Features})
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
	type part struct {
		dst  *[]string
		vars []string // ${config.X}, in order
	}
	for side, hod := range [2]string{Device: "Device", Host: "Host"} {
		for _, p := range []part{
			{&g.Head[side], []string{"CommonGlobalCflags", hod + "GlobalCflags"}},
			{&g.Conly[side], []string{"CommonGlobalConlyflags"}},
			{&g.Cpp[side], []string{"CommonGlobalCppflags", hod + "GlobalCppflags"}},
			{&g.As[side], []string{"CommonGlobalAsflags"}},
		} {
			for _, v := range p.vars {
				l, err := get("${config." + v + "}")
				if err != nil {
					return g, err
				}
				*p.dst = append(*p.dst, l...)
			}
		}
	}
	// the device toolchain around the global lists (compiler.go: instruction set first, tc.Cflags before CommonGlobalCflags, tc.ToolchainCflags last; tc.Cppflags after the global C++ lists, tc.Asflags before CommonGlobalAsflags)
	isa, err := method("InstructionSetFlags", "")
	if err != nil {
		return g, err
	}
	cflags, err := method("Cflags")
	if err != nil {
		return g, err
	}
	g.Head[Device] = slices.Concat(isa, cflags, g.Head[Device])
	if g.Tail[Device], err = method("ToolchainCflags"); err != nil {
		return g, err
	}
	cpp, err := method("Cppflags")
	if err != nil {
		return g, err
	}
	g.Cpp[Device] = append(g.Cpp[Device], cpp...)
	as, err := method("Asflags")
	if err != nil {
		return g, err
	}
	g.As[Device] = append(as, g.As[Device]...)
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

// soongEval interprets the part of Go that build/soong/cc/config computes its flags with, as Soong would run it in this build:
// package vars, then every init (pctx.StaticVariable / VariableFunc registrations, appends to package vars), then on demand a VariableFunc body, a toolchain factory and its methods.
// The build's environment: no Soong environment variable is set (IsEnvTrue / IsEnvFalse false, Getenv ""), product config methods take their unset default (configDefaults), runtime.GOOS is linux.
// Anything else it meets where a value is needed fails, naming the construct: a Soong change is re-derived, never guessed.
type soongEval struct {
	dir      string
	vars     map[string]any // package vars, evaluated; error values = could not be evaluated (fails only when read)
	funcs    map[string]*ast.FuncDecl
	methods  map[string]map[string]*ast.FuncDecl // receiver type -> method
	embeds   map[string][]string
	statics  map[string]any // pctx.StaticVariable
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

func newSoongEval(dir string) (*soongEval, error) {
	e := &soongEval{dir: dir, vars: map[string]any{}, funcs: map[string]*ast.FuncDecl{}, methods: map[string]map[string]*ast.FuncDecl{}, embeds: map[string][]string{},
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
		if x.Kind != token.STRING {
			return nil, e.fail(x, "literal %s", x.Value)
		}
		return strconv.Unquote(x.Value)
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
			return nil, e.fail(x, "no field %s", x.Sel.Name)
		}
		return f, nil
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
				} else {
					return nil, e.fail(c, "append of %T", a)
				}
			}
			return out, nil
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
			err = fmt.Errorf("%s: ${config.%s} is registered by no pctx.StaticVariable / VariableFunc", e.dir, name)
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
	return out, err
}

var configRef = regexp.MustCompile(`\$\{config\.(\w+)\}`)

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
