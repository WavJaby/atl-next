// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/google/blueprint/parser"
)

// Val is an evaluated property value: string, bool, int64, []Val, *Obj or nil (unset).
type Val = any

// Obj is a property map that keeps declaration order.
type Obj struct {
	keys []string
	m    map[string]Val
}

func newObj() *Obj { return &Obj{m: map[string]Val{}} }

func (o *Obj) get(k string) Val { return o.m[k] }

func (o *Obj) set(k string, v Val) {
	if _, ok := o.m[k]; !ok {
		o.keys = append(o.keys, k)
	}
	o.m[k] = v
}

func (o *Obj) obj(k string) *Obj {
	if v, ok := o.m[k].(*Obj); ok {
		return v
	}
	return nil
}

func (o *Obj) str(k string) string {
	s, _ := o.m[k].(string)
	return s
}

func (o *Obj) strs(k string) []string {
	var out []string
	if l, ok := o.m[k].([]Val); ok {
		for _, v := range l {
			if s, ok := v.(string); ok {
				out = append(out, s)
			}
		}
	}
	return out
}

func (o *Obj) boolean(k string, def bool) bool {
	if b, ok := o.m[k].(bool); ok {
		return b
	}
	return def
}

// merge applies src onto dst the way Soong extends properties: lists append, maps merge, scalars
// replace. Used for defaults (defaults first, the module's own after) and for target/arch blocks.
func merge(dst, src *Obj) *Obj {
	out := newObj()
	for _, k := range dst.keys {
		out.set(k, dst.m[k])
	}
	for _, k := range src.keys {
		sv := src.m[k]
		switch d := out.m[k].(type) {
		case []Val:
			if s, ok := sv.([]Val); ok {
				out.set(k, append(append([]Val{}, d...), s...))
				continue
			}
		case *Obj:
			if s, ok := sv.(*Obj); ok {
				out.set(k, merge(d, s))
				continue
			}
		}
		if sv != nil {
			out.set(k, sv)
		}
	}
	return out
}

// Module is one Android.bp module definition.
type Module struct {
	Type  string
	Name  string
	Dir   string // tree-relative directory of its blueprint file
	Props *Obj
}

// Config is what select() conditions see. PointerBits comes from the arch's arches.toml row, never from its name.
type Config struct {
	Arch        string
	PointerBits int
}

// Multilib is Soong's multilib name for the arch: `multilib: { lib32/lib64 }` blocks, cc_genrule's CC_MULTILIB.
func (c Config) Multilib() string { return fmt.Sprintf("lib%d", c.PointerBits) }

// evaluator turns blueprint expressions into values for one side (os) of the build.
type evaluator struct {
	cfg Config
	os  string // "android" or "linux_glibc", the answer of select(os(), …)
}

func (e *evaluator) eval(x parser.Expression, sc *parser.Scope) (Val, error) {
	switch v := x.(type) {
	case nil:
		return nil, nil
	case *parser.String:
		return v.Value, nil
	case *parser.Bool:
		return v.Value, nil
	case *parser.Int64:
		return v.Value, nil
	case *parser.UnsetProperty:
		return nil, nil
	case *parser.List:
		out := []Val{}
		for _, item := range v.Values {
			iv, err := e.eval(item, sc)
			if err != nil {
				return nil, err
			}
			if l, ok := iv.([]Val); ok {
				out = append(out, l...)
			} else if iv != nil {
				out = append(out, iv)
			}
		}
		return out, nil
	case *parser.Map:
		o := newObj()
		for _, p := range v.Properties {
			pv, err := e.eval(p.Value, sc)
			if err != nil {
				return nil, fmt.Errorf("%s: %w", p.Name, err)
			}
			o.set(p.Name, pv)
		}
		return o, nil
	case *parser.Variable:
		a := sc.Get(v.Name)
		if a == nil {
			return nil, fmt.Errorf("undefined variable %s", v.Name)
		}
		return e.eval(a.Value, sc)
	case *parser.Operator:
		l, err := e.eval(v.Args[0], sc)
		if err != nil {
			return nil, err
		}
		r, err := e.eval(v.Args[1], sc)
		if err != nil {
			return nil, err
		}
		return plus(l, r)
	case *parser.Select:
		s := sc
		if v.Scope != nil {
			s = v.Scope
		}
		chosen, err := e.selectCase(v, s)
		if err != nil {
			return nil, err
		}
		if v.Append == nil {
			return chosen, nil
		}
		tail, err := e.eval(v.Append, s)
		if err != nil {
			return nil, err
		}
		return plus(chosen, tail)
	}
	return nil, fmt.Errorf("unsupported expression %T", x)
}

func plus(l, r Val) (Val, error) {
	switch a := l.(type) {
	case nil:
		return r, nil
	case string:
		if b, ok := r.(string); ok {
			return a + b, nil
		}
	case []Val:
		if b, ok := r.([]Val); ok {
			return append(append([]Val{}, a...), b...), nil
		}
		if r == nil {
			return a, nil
		}
	case *Obj:
		if b, ok := r.(*Obj); ok {
			return merge(a, b), nil
		}
		if r == nil {
			return a, nil
		}
	case int64:
		if b, ok := r.(int64); ok {
			return a + b, nil
		}
	}
	if r == nil {
		return l, nil
	}
	return nil, fmt.Errorf("cannot add %T and %T", l, r)
}

const (
	defaultBranch = "__soong_conditions_default__"
	anyBranch     = "__soong_conditions_any__"
)

// condition answers a select() condition; ok=false means unset. Only os() and arch() are known:
// soong_config_variable, release_flag and product_variable stay unset, so their default branch wins.
func (e *evaluator) condition(c parser.ConfigurableCondition) (string, bool) {
	switch c.FunctionName {
	case "os":
		return e.os, true
	case "arch":
		return e.cfg.Arch, true
	}
	return "", false
}

func (e *evaluator) selectCase(s *parser.Select, sc *parser.Scope) (Val, error) {
	values := make([]string, len(s.Conditions))
	set := make([]bool, len(s.Conditions))
	for i, c := range s.Conditions {
		values[i], set[i] = e.condition(c)
	}
	for _, cs := range s.Cases {
		if len(cs.Patterns) != len(values) {
			continue
		}
		match := true
		for i, p := range cs.Patterns {
			switch pv := p.Value.(type) {
			case *parser.String:
				switch pv.Value {
				case defaultBranch:
				case anyBranch:
					match = match && set[i]
				default:
					match = match && set[i] && values[i] == pv.Value
				}
			case *parser.Bool:
				match = match && set[i] && values[i] == fmt.Sprint(pv.Value)
			default:
				match = false
			}
		}
		if match {
			return e.eval(cs.Value, sc)
		}
	}
	return nil, nil
}

// parsed is one blueprint file, kept unevaluated: each side evaluates it again (select(os())).
type parsed struct {
	dir   string
	file  *parser.File
	scope *parser.Scope
}

// discover parses every Android.bp under tree (and the files a top-level `build = [...]` names),
// skipping .git and directories holding Soong's prune marker .find-ignore.
func discover(tree string) ([]parsed, error) {
	var files []string
	err := filepath.WalkDir(tree, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		if d.IsDir() {
			if d.Name() == ".git" || d.Name() == "out" && path == filepath.Join(tree, "out") {
				return filepath.SkipDir
			}
			if _, err := os.Stat(filepath.Join(path, ".find-ignore")); err == nil {
				return filepath.SkipDir
			}
			return nil
		}
		if d.Name() == "Android.bp" {
			files = append(files, path)
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	// parents first: a blueprint file sees the variables of the files in its ancestor directories
	sort.Slice(files, func(i, j int) bool {
		di, dj := strings.Count(files[i], "/"), strings.Count(files[j], "/")
		if di != dj {
			return di < dj
		}
		return files[i] < files[j]
	})
	scopes := map[string]*parser.Scope{}
	parentScope := func(dir string) *parser.Scope {
		for d := filepath.Dir(dir); len(d) >= len(tree); d = filepath.Dir(d) {
			if s, ok := scopes[d]; ok {
				return s
			}
			if d == "/" {
				break
			}
		}
		return nil
	}
	var out []parsed
	for len(files) > 0 {
		path := files[0]
		files = files[1:]
		f, err := os.Open(path)
		if err != nil {
			return nil, err
		}
		dir := filepath.Dir(path)
		parent := scopes[dir]
		if parent == nil {
			parent = parentScope(dir)
		}
		sc := parser.NewScope(parent)
		// as Blueprint does (context.go): these name a file's own sub-files, never a child's
		sc.DontInherit("subdirs")
		sc.DontInherit("optional_subdirs")
		sc.DontInherit("build")
		if filepath.Base(path) == "Android.bp" {
			scopes[dir] = sc
		}
		file, errs := parser.ParseAndEval(path, f, sc)
		f.Close()
		if len(errs) > 0 {
			fmt.Fprintf(os.Stderr, "bp2ninja: skip %s: %v\n", path, errs[0])
			continue
		}
		rel, _ := filepath.Rel(tree, filepath.Dir(path))
		out = append(out, parsed{dir: rel, file: file, scope: sc})
		if a := sc.GetLocal("build"); a != nil {
			if l, ok := a.Value.(*parser.List); ok {
				for _, v := range l.Values {
					if s, ok := v.(*parser.String); ok && !strings.ContainsAny(s.Value, "*?") {
						files = append(files, filepath.Join(filepath.Dir(path), s.Value))
					}
				}
			}
		}
	}
	return out, nil
}

// modules evaluates every module of the parsed files for one side.
func (e *evaluator) modules(files []parsed) (map[string][]*Module, []string) {
	byName := map[string][]*Module{}
	var problems []string
	for _, pf := range files {
		for _, def := range pf.file.Defs {
			m, ok := def.(*parser.Module)
			if !ok {
				continue
			}
			v, err := e.eval(&m.Map, pf.scope)
			if err != nil {
				problems = append(problems, fmt.Sprintf("%s: %s: %v", pf.dir, m.Type, err))
				continue
			}
			props := v.(*Obj)
			name := props.str("name")
			if name == "" {
				continue
			}
			byName[name] = append(byName[name], &Module{Type: m.Type, Name: name, Dir: pf.dir, Props: props})
		}
	}
	return byName, problems
}
