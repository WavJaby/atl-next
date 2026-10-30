// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"sort"
	"strings"
)

// Side is which source set a module is built from. Every artifact is a Linux glibc binary; device =
// the module's android properties (runtime libraries of an app process), host = its host properties
// (build tools such as aidl that run on the build machine).
type Side int

const (
	Device Side = iota
	Host
)

func (s Side) String() string { return [...]string{"device", "host"}[s] }

// targetBlocks lists the `target: {…}` blocks a side applies, in Soong's order of generality.
func targetBlocks(s Side, arch string) []string {
	if s == Device {
		return []string{"linux", "bionic", "not_windows", "android", "linux_" + arch, "android_" + arch}
	}
	return []string{"host", "not_windows", "linux", "glibc", "host_linux", "linux_glibc", "linux_glibc_" + arch}
}

// Kind is the artifact a node produces.
type Kind int

const (
	Static Kind = iota
	Shared
	Headers
	Binary
)

func (k Kind) String() string { return [...]string{"static", "shared", "headers", "binary"}[k] }

type key struct {
	name string
	side Side
	kind Kind
}

// Node is one module built one way.
type Node struct {
	key
	mod   *Module
	props *Obj // after defaults, target/arch/multilib and kind blocks
	srcs  []string
	aidl  []aidlSrc
	gram  []string // .ll / .yy grammar sources, generated into C++ by flex / bison
	deps  struct{ whole, static, shared, headers []*Node }
	// exported include flags, transitive through export_*_lib_headers
	exports []string
	out     string // archive, shared object or binary
	objs    []string
	gen     []string // files generated for it (aidl, flex/bison output)
}

// genClosure: generated files n and its dependencies produce; n's compiles wait for them (headers).
// Not global: the aidl tool's own compiles must not wait for aidl output.
func genClosure(n *Node, seen map[*Node]bool, out *[]string) {
	if seen[n] {
		return
	}
	seen[n] = true
	*out = append(*out, n.gen...)
	for _, d := range append(append(append(append([]*Node{}, n.deps.headers...), n.deps.static...), n.deps.shared...), n.deps.whole...) {
		genClosure(d, seen, out)
	}
}

type aidlSrc struct{ file, root string } // tree-relative file and its import root

type Graph struct {
	tree, out string
	cfg       Config
	soong     SoongConfig
	mods      [2]map[string][]*Module
	nodes     map[key]*Node
	order     []*Node
	problems  []string
}

func (g *Graph) problem(format string, args ...any) {
	g.problems = append(g.problems, fmt.Sprintf(format, args...))
}

// lookup finds a module by Soong reference: `name`, `//dir:name`, `name#tag`; prebuilts, imports
// and NDK/LLNDK stubs lose to a source definition of the same name.
func (g *Graph) lookup(ref string, side Side) *Module {
	ref = strings.TrimPrefix(ref, ":")
	if i := strings.IndexByte(ref, '#'); i >= 0 {
		ref = ref[:i]
	}
	dir := ""
	if strings.HasPrefix(ref, "//") {
		if d, n, ok := strings.Cut(ref[2:], ":"); ok {
			dir, ref = d, n
		}
	}
	var cands []*Module
	for _, m := range g.mods[side][ref] {
		if dir != "" && m.Dir != dir && !strings.HasPrefix(m.Dir, dir+"/") {
			continue
		}
		t := m.Type
		if strings.Contains(t, "prebuilt") || strings.Contains(t, "_import") || strings.HasPrefix(t, "ndk_") ||
			strings.HasPrefix(t, "llndk") || strings.Contains(t, "stubs") {
			continue
		}
		cands = append(cands, m)
	}
	if len(cands) == 0 {
		return nil
	}
	sort.Slice(cands, func(i, j int) bool { return cands[i].Dir < cands[j].Dir })
	return cands[0]
}

// resolveDefaults merges a module's defaults (recursively) under its own properties.
func (g *Graph) resolveDefaults(m *Module, side Side, seen map[string]bool) *Obj {
	acc := newObj()
	for _, d := range m.Props.strs("defaults") {
		if seen[d] {
			continue
		}
		seen[d] = true
		dm := g.lookup(d, side)
		if dm == nil {
			g.problem("%s: defaults %s not found", m.Name, d)
			continue
		}
		acc = merge(acc, g.resolveDefaults(dm, side, seen))
	}
	return merge(acc, m.Props)
}

// flatten applies the blocks of one side, arch and kind onto the base properties.
func (g *Graph) flatten(m *Module, side Side, kind Kind) *Obj {
	p := g.resolveDefaults(m, side, map[string]bool{})
	base := newObj()
	for _, k := range p.keys {
		switch k {
		case "target", "arch", "multilib", "shared", "static", "defaults":
		default:
			base.set(k, p.m[k])
		}
	}
	if t := p.obj("target"); t != nil {
		for _, b := range targetBlocks(side, g.cfg.Arch) {
			if o := t.obj(b); o != nil {
				base = merge(base, o)
			}
		}
	}
	if a := p.obj("arch"); a != nil {
		if o := a.obj(g.cfg.Arch); o != nil {
			base = merge(base, o)
		}
	}
	if ml := p.obj("multilib"); ml != nil {
		if o := ml.obj("lib64"); o != nil {
			base = merge(base, o)
		}
	}
	if kind == Static || kind == Shared {
		if o := p.obj(kind.String()); o != nil {
			base = merge(base, o)
		}
	}
	return base
}

// kindOf is the artifact a module produces when used with `want` (Static/Shared as the dependent asks).
func kindOf(t string, want Kind) (Kind, bool) {
	switch {
	case strings.HasSuffix(t, "_headers"):
		return Headers, true
	case strings.HasPrefix(t, "cc_binary"):
		return Binary, true
	case t == "cc_library_static" || t == "cc_library_host_static":
		return Static, true
	case t == "cc_library_shared" || t == "cc_library_host_shared":
		return Shared, true
	case t == "cc_library" || t == "cc_object":
		if want == Headers || want == Binary {
			return Shared, true
		}
		return want, true
	}
	return 0, false
}

// bionic is the C library of an android build; on a glibc target the system's libc, kernel headers and
// libm/libdl take its place, so a dependency on bionic itself is dropped, never built or included
// (bionic's headers on the include path shadow glibc's: `__GLIBC_USE is not defined`).
var hostLibc = map[string]bool{
	"libc": true, "libm": true, "libdl": true,
	"libc_headers": true, "libc_uapi_headers": true, "bionic_libc_platform_headers": true,
}

// node builds (once) the node for a reference, depth first.
func (g *Graph) node(ref string, side Side, want Kind, from string) *Node {
	if hostLibc[strings.TrimPrefix(ref, ":")] {
		return nil
	}
	m := g.lookup(ref, side)
	if m == nil {
		g.problem("%s: %s module %s not found", from, side, ref)
		return nil
	}
	kind, ok := kindOf(m.Type, want)
	if !ok {
		g.problem("%s: %s is a %s (unsupported)", from, m.Name, m.Type)
		return nil
	}
	k := key{m.Name, side, kind}
	if n, ok := g.nodes[k]; ok {
		return n
	}
	n := &Node{key: k, mod: m}
	g.nodes[k] = n
	n.props = g.flatten(m, side, kind)
	for _, s := range n.props.strs("srcs") {
		g.addSrc(n, s)
	}
	// defaults and target blocks may list one file twice; Soong builds it once
	n.srcs = dedup(without(n.srcs, g.expand(n, n.props.strs("exclude_srcs"))))
	dep := func(list string, want Kind) []*Node {
		var out []*Node
		for _, r := range n.props.strs(list) {
			if d := g.node(r, side, want, m.Name); d != nil {
				out = append(out, d)
			}
		}
		return out
	}
	n.deps.whole = dep("whole_static_libs", Static)
	n.deps.static = dep("static_libs", Static)
	n.deps.shared = dep("shared_libs", Shared)
	n.deps.headers = dep("header_libs", Headers)
	for _, gname := range append(n.props.strs("generated_headers"), n.props.strs("generated_sources")...) {
		g.problem("%s: generated %s not supported yet", m.Name, gname)
	}
	n.exports = g.exportsOf(n)
	g.order = append(g.order, n)
	return n
}

func (g *Graph) addSrc(n *Node, s string) {
	if strings.HasPrefix(s, ":") {
		fg := g.lookup(s, n.side)
		if fg == nil || fg.Type != "filegroup" {
			g.problem("%s: source %s is not a known filegroup", n.mod.Name, s)
			return
		}
		root := filepath.Join(fg.Dir, fg.Props.str("path"))
		for _, f := range g.globs(fg.Dir, fg.Props.strs("srcs")) {
			g.addFile(n, f, root)
		}
		return
	}
	for _, f := range g.globs(n.mod.Dir, []string{s}) {
		g.addFile(n, f, n.mod.Dir)
	}
}

func (g *Graph) addFile(n *Node, f, root string) {
	switch filepath.Ext(f) {
	case ".c", ".cc", ".cpp", ".cxx", ".S", ".s":
		n.srcs = append(n.srcs, f)
	case ".aidl":
		n.aidl = append(n.aidl, aidlSrc{f, root})
	case ".ll", ".l", ".yy", ".y":
		n.gram = append(n.gram, f)
	default:
		g.problem("%s: source %s of an unsupported kind", n.mod.Name, f)
	}
}

func (g *Graph) expand(n *Node, pats []string) []string { return g.globs(n.mod.Dir, pats) }

// globs resolves paths and globs (`**` included) relative to dir; results are tree-relative.
func (g *Graph) globs(dir string, pats []string) []string {
	var out []string
	for _, p := range pats {
		if !strings.ContainsAny(p, "*?[") {
			out = append(out, filepath.Join(dir, p))
			continue
		}
		out = append(out, globTree(filepath.Join(g.tree, dir), p, dir)...)
	}
	return out
}

func without(list, drop []string) []string {
	skip := map[string]bool{}
	for _, d := range drop {
		skip[d] = true
	}
	var out []string
	for _, s := range list {
		if !skip[s] {
			out = append(out, s)
		}
	}
	return out
}

// exportsOf: the include flags a node hands to its dependents.
func (g *Graph) exportsOf(n *Node) []string {
	var out []string
	for _, d := range n.props.strs("export_include_dirs") {
		out = append(out, "-I"+g.abs(filepath.Join(n.mod.Dir, d)))
	}
	for _, d := range n.props.strs("export_system_include_dirs") {
		out = append(out, "-isystem", g.abs(filepath.Join(n.mod.Dir, d)))
	}
	if len(n.aidl) > 0 {
		if a := n.props.obj("aidl"); a != nil && a.boolean("export_aidl_headers", false) {
			out = append(out, "-I"+g.genDir(n, "include"))
		}
	}
	reexport := map[string]bool{}
	for _, l := range []string{"export_header_lib_headers", "export_static_lib_headers", "export_shared_lib_headers"} {
		for _, r := range n.props.strs(l) {
			reexport[r] = true
		}
	}
	for _, d := range append(append(append(append([]*Node{}, n.deps.headers...), n.deps.static...), n.deps.shared...), n.deps.whole...) {
		if reexport[d.name] || d.kind == Headers && reexport[d.mod.Name] {
			out = append(out, d.exports...)
		}
	}
	// whole_static_libs' exports travel with the merged archive
	for _, d := range n.deps.whole {
		out = append(out, d.exports...)
	}
	return dedup(out)
}

func dedup(in []string) []string {
	seen := map[string]bool{}
	var out []string
	for i := 0; i < len(in); i++ {
		s := in[i]
		if s == "-isystem" && i+1 < len(in) {
			pair := s + " " + in[i+1]
			if !seen[pair] {
				seen[pair] = true
				out = append(out, s, in[i+1])
			}
			i++
			continue
		}
		if !seen[s] {
			seen[s] = true
			out = append(out, s)
		}
	}
	return out
}

func (g *Graph) abs(rel string) string { return filepath.Join(g.tree, rel) }

func (g *Graph) genDir(n *Node, sub string) string {
	return filepath.Join(g.out, "gen", n.side.String(), n.name, sub)
}

// staticClosure: archives a final link needs for n's static (and whole static) dependencies.
func staticClosure(n *Node, seen map[*Node]bool, out *[]*Node) {
	for _, d := range append(append([]*Node{}, n.deps.whole...), n.deps.static...) {
		if seen[d] {
			continue
		}
		seen[d] = true
		*out = append(*out, d)
		staticClosure(d, seen, out)
	}
}
