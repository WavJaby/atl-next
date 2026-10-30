// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"slices"
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
	Gen // genrule / cc_genrule: outputs only, no cc artifact
)

func (k Kind) String() string { return [...]string{"static", "shared", "headers", "binary", "gen"}[k] }

type key struct {
	name string
	side Side
	kind Kind
}

// Node is one module built one way.
type Node struct {
	key
	mod   *Module
	props *Obj     // after defaults, target/arch/multilib and kind blocks
	srcs  []string // compiled sources: plain ones and the outputs of generators
	deps  struct{ whole, static, shared, headers []*Node }
	// libraries a source generator added to deps (protobuf runtime): their exports travel on, as export_*_lib_headers would
	reexport []*Node
	// exported include flags, transitive through export_*_lib_headers
	exports []string
	out     string // archive, shared object or binary
	objs    []string
	gen     []string // files generated for it (generator headers, genrule output): its compiles wait for them
	// source generators (gensrc.go): their edges, include flags and cflags for n's compiles
	edges     []genEdge
	genCflags []string
	// genrules: usedGens are those n consumes; genIncs their include flags and the generators'; genExports
	// those of its export_generated_headers and of generators with their export property set, handed to dependents
	usedGens   map[*Node]bool
	genIncs    []string
	genExports []string
	rule       *genRule // Kind Gen only
	// cc_object modules srcs name (`:bcm_object`)
	objSrcs []*Node
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

type Graph struct {
	tree, out string
	cfg       Config
	soong     SoongConfig
	omit      map[string]bool
	// soong_config_module_type name -> wrapped module type
	configTypes map[string]string
	mods        [2]map[string][]*Module
	nodes       map[key]*Node
	order       []*Node
	problems    []string
	// references no module of the tree answers to (see notFound)
	missing map[string]bool
	// tree-relative directories modules name by path (include_dirs) that do not exist
	missingPaths map[string]bool
	// genrule scripts by path, written by writeScripts after emit
	scripts map[string]string
	// host binaries source generators run (gensrc.go hostTool), nil entry = module missing
	tools map[string]*Node
	// aidl_interface modules by name/side (nil = none), and the backends synthesized from them by ref/side (aidliface.go)
	aidlIfaces map[string]*aidlIface
	aidlMods   map[string]*Module
	// Java modules (java.go)
	java *javaGraph
}

func (g *Graph) problem(format string, args ...any) {
	g.problems = append(g.problems, fmt.Sprintf(format, args...))
}

// notFound records a reference lookup that found no module: a module, defaults or filegroup the tree
// lacks, which the caller may fetch. Stored the way the AOSP index takes it (`name`, `//dir:name`: no
// leading `:`, no `#tag`), so a project can be looked up without re-parsing prose.
func (g *Graph) notFound(ref string) {
	ref = strings.TrimPrefix(ref, ":")
	if i := strings.IndexByte(ref, '#'); i >= 0 {
		ref = ref[:i]
	}
	g.missing = addKey(g.missing, ref)
}

func addKey(m map[string]bool, k string) map[string]bool {
	if m == nil {
		m = map[string]bool{}
	}
	m[k] = true
	return m
}

func sortedKeys(m map[string]bool) []string {
	out := make([]string, 0, len(m))
	for r := range m {
		out = append(out, r)
	}
	sort.Strings(out)
	return out
}

// lookup finds a module by Soong reference: `name`, `//dir:name`, `name#tag`; prebuilts, imports
// and NDK/LLNDK stubs lose to a source definition of the same name.
func (g *Graph) lookup(ref string, side Side) *Module { return g.lookupWith(ref, side, isPrebuiltType) }

// isPrebuiltType: module types lookup skips. fdo_profile: PGO profile data sharing a library's name, never its code.
func isPrebuiltType(t string) bool {
	return strings.Contains(t, "prebuilt") || strings.Contains(t, "_import") || strings.HasPrefix(t, "ndk_") ||
		strings.HasPrefix(t, "llndk") || strings.Contains(t, "stubs") || t == "fdo_profile"
}

// lookupWith is lookup with the module types to skip given; among several candidates the first by directory wins.
func (g *Graph) lookupWith(ref string, side Side, skip func(string) bool) *Module {
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
		if skip(m.Type) {
			continue
		}
		cands = append(cands, m)
	}
	if len(cands) == 0 {
		// `<iface>[-V<n>]-<cpp|ndk>`: made by the aidl_interface module type, defined in no Android.bp
		if dir == "" {
			return g.aidlBackend(ref, side)
		}
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
		// a defaults module never loses to a prebuilt; its (soong_config) type may contain "stubs"
		dm := g.lookupWith(d, side, func(t string) bool { return isPrebuiltType(t) && !strings.HasSuffix(t, "_defaults") })
		if dm == nil {
			g.problem("%s: defaults %s not found", m.Name, d)
			g.notFound(d)
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
	// soong_config_variables: no Soong config variable (nor the release flags some map to) is set for
	// this build yet, so each variable's conditions_default applies
	if sv := p.obj("soong_config_variables"); sv != nil {
		base = removeKey(base, "soong_config_variables")
		for _, v := range sv.keys {
			if o := sv.obj(v); o != nil {
				if d := o.obj("conditions_default"); d != nil {
					base = merge(base, d)
				}
			}
		}
	}
	if a := p.obj("arch"); a != nil {
		if o := a.obj(g.cfg.Arch); o != nil {
			base = merge(base, o)
		}
	}
	if ml := p.obj("multilib"); ml != nil {
		if o := ml.obj(g.cfg.Multilib()); o != nil {
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

// cxxCompat: bionic headers AOSP code relies on and glibc lacks or differs in, put first on the C++
// include path of device-side modules (never C: stdatomic.h's C branch needs bionic's bits/). Include
// name -> tree-relative file, linked unchanged into <out>/compat/cxx.
//   - stdatomic.h: in C++ bionic maps it onto <atomic>; clang's is incompatible with <atomic> before
//     C++23 (liblog, libbinder, libutils' LightRefBase.h include it from C++).
//   - linux/ashmem.h: Android's ashmem uapi, absent from distro kernel headers (libcutils
//     ashmem-dev.cpp; at run time a mainline kernel takes its memfd path).
var cxxCompat = map[string]string{
	"stdatomic.h":    "bionic/libc/include/stdatomic.h",
	"linux/ashmem.h": "bionic/libc/kernel/uapi/linux/ashmem.h",
}

// node builds (once) the node for a reference, depth first.
func (g *Graph) node(ref string, side Side, want Kind, from string) *Node {
	// providers.toml `provider = "none"`: the dependency is dropped (bionic itself, platform-only APIs)
	if g.omit[strings.TrimPrefix(ref, ":")] {
		return nil
	}
	m := g.lookup(ref, side)
	if m == nil {
		// a backend of an aidl_interface the tree has: lookup recorded why there is none, and the name is nothing to fetch
		if !g.aidlOwned(ref) {
			g.problem("%s: %s module %s not found", from, side, ref)
			g.notFound(ref)
		}
		return nil
	}
	kind, ok := kindOf(g.baseType(m.Type), want)
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
	var files []srcFile
	for _, s := range n.props.strs("srcs") {
		files = append(files, g.srcFiles(n, s)...)
	}
	if m.aidl != nil {
		files = append(files, m.aidl.srcs...)
	}
	dep := func(list string, want Kind) []*Node {
		var out []*Node
		for _, r := range n.props.strs(list) {
			if d := g.node(r, side, want, m.Name); d != nil {
				out = append(out, d)
			}
		}
		return out
	}
	// cc_object modules named in srcs (srcFiles) are linked whole, as Soong links their object
	n.deps.whole = append(dep("whole_static_libs", Static), n.objSrcs...)
	n.deps.static = dep("static_libs", Static)
	n.deps.shared = dep("shared_libs", Shared)
	n.deps.headers = dep("header_libs", Headers)
	exported := map[string]bool{}
	for _, r := range n.props.strs("export_generated_headers") {
		exported[r] = true
	}
	for _, r := range n.props.strs("generated_headers") {
		if gn := g.genNode(r, side, m.Name); gn != nil {
			g.useGen(n, gn)
			if exported[r] {
				n.genExports = append(n.genExports, gn.rule.incs...)
			}
		}
	}
	skip := map[string]bool{}
	for _, r := range n.props.strs("exclude_generated_sources") {
		skip[r] = true
	}
	for _, r := range n.props.strs("generated_sources") {
		if skip[r] {
			continue
		}
		if gn := g.genNode(r, side, m.Name); gn != nil {
			g.useGen(n, gn)
			for _, o := range gn.rule.outs {
				// headers are only waited on
				if !isHeaderSrc(o) {
					files = append(files, srcFile{path: o, root: g.genDir(gn, ""), gen: gn})
				}
			}
		}
	}
	g.genSources(n, dedupSrcs(files, g.expand(n, n.props.strs("exclude_srcs"))))
	n.exports = g.exportsOf(n)
	g.order = append(g.order, n)
	return n
}

// srcFiles expands one `srcs` entry of n; the genrules it names are consumed (n's compiles wait for them).
func (g *Graph) srcFiles(n *Node, s string) []srcFile {
	if isRef(s) {
		if m := g.lookup(s, n.side); m != nil && g.baseType(m.Type) == "cc_object" {
			if o := g.node(s, n.side, Static, n.mod.Name); o != nil {
				n.objSrcs = append(n.objSrcs, o)
			}
			return nil
		}
	}
	files, _ := g.resolveSrc(n.mod.Dir, n.side, s, n.mod.Name)
	var out []srcFile
	for _, f := range files {
		if f.gen != nil {
			g.useGen(n, f.gen)
			// headers a genrule produces are only waited on
			if isHeaderSrc(f.path) {
				continue
			}
		}
		out = append(out, f)
	}
	return out
}

// dedupSrcs drops excluded (exclude_srcs) and repeated sources; defaults and target blocks may list one file twice, Soong builds it once.
func dedupSrcs(files []srcFile, exclude []string) []srcFile {
	skip := map[string]bool{}
	for _, e := range exclude {
		skip[e] = true
	}
	var out []srcFile
	for _, f := range files {
		if !skip[f.path] {
			skip[f.path] = true
			out = append(out, f)
		}
	}
	return out
}

// srcFile is one file a `srcs`-style entry names: tree-relative, or absolute under the build dir when a
// genrule (gen) produces it. root is the import root aidl and proto sources need.
type srcFile struct {
	path, root string
	gen        *Node
}

// isRef: a srcs entry naming a module (`:name`, `//dir:name`) rather than a path.
func isRef(s string) bool { return strings.HasPrefix(s, ":") || strings.HasPrefix(s, "//") }

// resolveSrc expands one entry of srcs / tool_files written in dir: a path or glob, `:name` or
// `//dir:name` naming a filegroup (its srcs, recursively) or a genrule (its outputs). ok=false: a
// reference no known module of those types answers to (problem recorded).
func (g *Graph) resolveSrc(dir string, side Side, s, from string) (files []srcFile, ok bool) {
	if !isRef(s) {
		for _, f := range g.globs(dir, []string{s}) {
			files = append(files, srcFile{path: f, root: dir})
		}
		return files, true
	}
	m := g.lookup(s, side)
	switch {
	case m != nil && isGenType(g.baseType(m.Type)):
		gn := g.genNode(s, side, from)
		for _, o := range gn.rule.outs {
			files = append(files, srcFile{path: o, root: g.genDir(gn, ""), gen: gn})
		}
		return files, true
	case m != nil && m.Type == "filegroup":
		root := filepath.Join(m.Dir, m.Props.str("path"))
		ok = true
		exclude := map[string]bool{}
		for _, e := range g.globs(m.Dir, m.Props.strs("exclude_srcs")) {
			exclude[e] = true
		}
		for _, e := range m.Props.strs("srcs") {
			sub, subOK := g.resolveSrc(m.Dir, side, e, from)
			ok = ok && subOK
			for _, f := range sub {
				if f.gen == nil && exclude[f.path] {
					continue
				}
				// a nested reference keeps the root of the group that owns the file
				if f.gen == nil && !isRef(e) {
					f.root = root
				}
				files = append(files, f)
			}
		}
		return files, ok
	}
	g.problem("%s: source %s is not a known filegroup", from, s)
	// a module of another type under that name is defined, not missing
	if m == nil {
		g.notFound(s)
	}
	return nil, false
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

func removeKey(o *Obj, k string) *Obj {
	out := newObj()
	for _, key := range o.keys {
		if key != k {
			out.set(key, o.m[key])
		}
	}
	return out
}

// baseType: the module type behind a `soong_config_module_type` wrapper (libandroid_runtime is a
// `cc_library_shared_for_libandroid_runtime`), else the type itself.
func (g *Graph) baseType(t string) string {
	for i := 0; i < 8; i++ {
		b, ok := g.configTypes[t]
		if !ok {
			return t
		}
		t = b
	}
	return t
}

// exportsOf: the include flags a node hands to its dependents.
func (g *Graph) exportsOf(n *Node) []string {
	out := append([]string{}, n.genExports...)
	for _, d := range n.props.strs("export_include_dirs") {
		out = append(out, "-I"+g.abs(filepath.Join(n.mod.Dir, d)))
	}
	for _, d := range n.props.strs("export_system_include_dirs") {
		out = append(out, "-isystem", g.abs(filepath.Join(n.mod.Dir, d)))
	}
	reexport := map[string]bool{}
	for _, l := range []string{"export_header_lib_headers", "export_static_lib_headers", "export_shared_lib_headers"} {
		for _, r := range n.props.strs(l) {
			reexport[r] = true
		}
	}
	for _, d := range append(append(append(append([]*Node{}, n.deps.headers...), n.deps.static...), n.deps.shared...), n.deps.whole...) {
		if reexport[d.name] || d.kind == Headers && reexport[d.mod.Name] || slices.Contains(n.reexport, d) {
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

// abs: a tree-relative path under the tree, an absolute one (generated file) as is.
func (g *Graph) abs(rel string) string {
	if filepath.IsAbs(rel) {
		return rel
	}
	return filepath.Join(g.tree, rel)
}

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
