// SPDX-License-Identifier: MIT

package main

import (
	"encoding/json"
	"path/filepath"
	"slices"
	"strings"
)

// graphFile is <out>/graph.json: the cc build graph by module directory, for the caller's failure triage (which failed modules a goal reaches, which have no failed dependency).
// A module directory `<side>/<name>` is where a node's steps write (obj/<side>/<name>, gen/<side>/<name>, <side>/<name>): ninja's FAILED lines name it, so failures map onto nodes without re-parsing build.ninja.
// The static and shared nodes of one module share it.
type graphFile struct {
	Goals []string `json:"goals"`
	// module directory -> the module directories its steps wait for, sorted
	Deps map[string][]string `json:"deps"`
	// module directory -> the providers.toml `none` modules its Android.bp names, dropped (cc.go node), sorted
	Dropped map[string][]string `json:"dropped"`
	// `none` module directory -> tree directories its headers would reach dependents from: export_include_dirs, export_system_include_dirs, and those of the header libraries it re-exports.
	// For the caller's none gate: a compile failing to find a header these hold, in a module that dropped the module, shows the row hides a user still built.
	NoneExports map[string][]string `json:"none_exports"`
	// module directory -> what a build outside the converter takes of it: cargo packages `xtask native` builds after ninja (Cargo.toml `[package.metadata.atl] converter`, atl-bridgegen converter.rs).
	// Rust libraries, shared libraries, static archives and binaries the emitted graph builds (broken crates aside), and the provided modules by their real path.
	// A module with a shared and a static node: the shared one (putArtifact).
	Artifacts map[string]artifact `json:"artifacts"`
	// the cc link driver every link here runs (Tools.CXX): an rlib's Link arguments are its arguments (its stl's -nostdlib++ / -lc++), so an outside build links with it too
	LinkDriver string `json:"link_driver"`
	// the product variables this build sets (productvars.go), what an upstream built outside the converter for the same product is configured with (platform_sdk_version)
	ProductVariables map[string]Val `json:"product_variables"`
	// the device platform flags (devicePlatform)
	Device platform `json:"device"`
}

// artifact: one linkable output, as the converter's own links take it.
type artifact struct {
	Kind string `json:"kind"` // rlib | shared | static | binary (Kind.String) | object (a cc_object's object file: crtbegin_so, which xtask's guest libc++ link takes); a provided module: shared or static
	Out  string `json:"out"`
	// rlib: its crate name, the -L dependency= dirs of its crate closure (crateDirs), its cc link arguments (rustCcLibs; none without cc libraries) and their dirs (rustLinkDirs: -L native=)
	// static: its link arguments beyond the archive (linkLibs: static closure, shared and system libraries), what a final link of it takes; Out is the archive itself
	Crate  string   `json:"crate,omitempty"`
	Search []string `json:"search,omitempty"`
	Link   []string `json:"link,omitempty"`
	Native []string `json:"native,omitempty"`
	// shared and static: the C++ argv of the module's own C++ compiles (compileFlags cxx: libc++, device defines, include paths), what a Rust package compiling a C++ wrapper against the module takes; a static archive's wrapper needs it as much as a shared library's
	Cxx []string `json:"cxx,omitempty"`
	// shared only: the provided shared libraries its link names, by module directory (providedNeeds); they lie outside the stage, so an outside link finds them through their own artifacts
	Needs []string `json:"needs,omitempty"`
}

// artifactOf: n's graph.json artifact; ok=false for kinds no outside build links (headers, generators, proc macros) and for crates that cannot compile.
func (g *Graph) artifactOf(n *Node) (a artifact, ok bool) {
	switch {
	case n.kind == Rlib && n.rust.broken == "":
		link, _ := g.rustCcLibs(n)
		return artifact{Kind: n.kind.String(), Out: n.out, Crate: n.rust.crateName, Search: crateDirs([]*Node{n}), Link: link, Native: rustLinkDirs(link)}, true
	case n.kind == Shared:
		return artifact{Kind: n.kind.String(), Out: n.out, Cxx: g.compileFlags(n).cxx, Needs: providedNeeds(n)}, n.out != ""
	case n.kind == Binary:
		return artifact{Kind: n.kind.String(), Out: n.out}, n.out != ""
	case n.kind == Static && n.mod != nil && g.baseType(n.mod.Type) == "cc_object":
		return artifact{Kind: "object", Out: g.objectOut(n)}, true
	case n.kind == Static:
		link, _ := g.linkLibs(n)
		return artifact{Kind: n.kind.String(), Out: n.out, Cxx: g.compileFlags(n).cxx, Link: link}, n.out != ""
	}
	return artifact{}, false
}

// providedNeeds: the provided shared libraries n's link names (linkedShared), by module directory, sorted.
// Only n's own names: a tree shared library they need names them itself, and lib_env (xtask) takes every artifact's needs for the stage's search path.
func providedNeeds(n *Node) []string {
	var out []string
	for _, d := range linkedShared(n, linkClosure(n)) {
		if d.provided && d.kind == Shared {
			out = appendNew(out, d.dir())
		}
	}
	slices.Sort(out)
	return out
}

// putArtifact: a module's static and shared nodes share its directory. The shared output stands (an outside build links the shared library); an archive fills an empty slot only.
func putArtifact(m map[string]artifact, dir string, a artifact) {
	if old, ok := m[dir]; ok && a.Kind == Static.String() && old.Kind != Static.String() {
		return
	}
	m[dir] = a
}

// providedNodes: the provided modules this graph reaches, sorted by directory and kind (provided.go providedNode).
func (g *Graph) providedNodes() []*Node {
	var out []*Node
	for _, n := range g.nodes {
		if n.provided {
			out = append(out, n)
		}
	}
	slices.SortFunc(out, func(a, b *Node) int {
		return strings.Compare(a.dir()+a.kind.String(), b.dir()+b.kind.String())
	})
	return out
}

func (n *Node) dir() string { return n.side.String() + "/" + n.name }

// stepDeps: the nodes n's steps wait for: linked and header libraries, Rust crates and source providers, cc_object sources, consumed genrules, the host tools of its generators and of its genrule command.
func (n *Node) stepDeps() []*Node {
	deps := slices.Concat(n.deps.whole, n.deps.static, n.deps.shared, n.deps.headers, n.deps.crates, n.objSrcs, n.objDeps, n.crtBegin, n.crtEnd)
	if n.rust != nil {
		deps = append(deps, n.rust.providers...)
	}
	for gn := range n.usedGens {
		deps = append(deps, gn)
	}
	for _, e := range n.edges {
		for _, t := range e.tools {
			if t.node != nil {
				deps = append(deps, t.node)
			}
		}
	}
	if n.rule != nil {
		deps = append(deps, n.rule.tools...)
		deps = append(deps, n.rule.objs...)
	}
	return deps
}

// noneExports: the tree directories (sorted, never nil) an omitted module's headers would be exported from, header libraries it re-exports followed.
// It is not built: what its lookups miss is no problem of the build, so problems and missing references are restored.
func (g *Graph) noneExports(name string, side Side) []string {
	problems := len(g.problems)
	missing := map[string]bool{}
	for k := range g.missing {
		missing[k] = true
	}
	dirs := map[string]bool{}
	seen := map[string]bool{}
	var walk func(ref string)
	walk = func(ref string) {
		m := g.lookup(ref, side)
		if m == nil || seen[m.Name] {
			return
		}
		seen[m.Name] = true
		p := g.flatten(m, side, Headers)
		for _, d := range append(p.strs("export_include_dirs"), p.strs("export_system_include_dirs")...) {
			dirs[filepath.Join(m.Dir, d)] = true
		}
		for _, r := range p.strs("export_header_lib_headers") {
			walk(r)
		}
	}
	walk(name)
	g.problems = g.problems[:problems]
	for k := range g.missing {
		if !missing[k] {
			delete(g.missing, k)
		}
	}
	return append([]string{}, sortedKeys(dirs)...)
}

// platform: what every device module gets from the toolchain and its default system_shared_libs, without module flags or dependencies -- how a build outside the converter compiles and links device code.
type platform struct {
	CC   string   `json:"cc"` // the C compiler (Tools.CC); the C++ one is link_driver
	C    []string `json:"c"`
	Cxx  []string `json:"cxx"`
	Link []string `json:"link"`
}

// devicePlatform: an empty shared device module's compile flags (compileFlags), plus the include dirs, defines and libraries of the default system_shared_libs' providers (the libc provider) and the link flags linkLibs ends with.
func (g *Graph) devicePlatform(cc string) platform {
	n := &Node{key: key{name: "platform", side: Device, kind: Shared}, mod: &Module{Name: "platform"}}
	f := g.compileFlags(n)
	var sys, link []string
	for _, name := range g.soong.SystemSharedLibs {
		p, ok := g.provided[name]
		if !ok {
			continue
		}
		for _, i := range p.Include {
			sys = appendNew(appendNew(sys, "-isystem"), i)
		}
		for _, d := range p.Defines {
			sys = appendNew(sys, "-D"+d)
		}
		// no rpath: device code gets none (linkLibs)
		link = appendNew(link, p.Lib)
	}
	link = append(link, stlLinkFlags(g.stlOf(n))...)
	link = append(link, systemLinkFlags...)
	return platform{CC: cc, C: slices.Concat(f.c, sys), Cxx: slices.Concat(f.cxx, sys), Link: link}
}

// graphJSON: graph.json for the emitted nodes and the goal nodes found; linkDriver: the cc link driver the build links with (Tools.CXX); cc: Tools.CC.
func (g *Graph) graphJSON(goals []*Node, linkDriver, cc string) ([]byte, error) {
	f := graphFile{Goals: []string{}, Deps: map[string][]string{}, Dropped: map[string][]string{}, NoneExports: map[string][]string{}, Artifacts: map[string]artifact{}, LinkDriver: linkDriver, ProductVariables: g.productVariables(), Device: g.devicePlatform(cc)}
	if f.ProductVariables == nil {
		f.ProductVariables = map[string]Val{}
	}
	for _, e := range sortedKeys(g.dropped) {
		dir, name, _ := strings.Cut(e, " ")
		f.Dropped[dir] = append(f.Dropped[dir], name)
		side := Device
		if strings.HasPrefix(dir, Host.String()+"/") {
			side = Host
		}
		if k := side.String() + "/" + name; f.NoneExports[k] == nil {
			f.NoneExports[k] = g.noneExports(name, side)
		}
	}
	for _, n := range goals {
		if !slices.Contains(f.Goals, n.dir()) {
			f.Goals = append(f.Goals, n.dir())
		}
	}
	for _, n := range g.order {
		set := map[string]bool{}
		for _, d := range f.Deps[n.dir()] {
			set[d] = true
		}
		for _, d := range n.stepDeps() {
			if d.dir() != n.dir() {
				set[d.dir()] = true
			}
		}
		f.Deps[n.dir()] = sortedKeys(set)
		if a, ok := g.artifactOf(n); ok {
			putArtifact(f.Artifacts, n.dir(), a)
		}
	}
	for _, n := range g.providedNodes() {
		putArtifact(f.Artifacts, n.dir(), artifact{Kind: n.kind.String(), Out: n.out})
	}
	return json.MarshalIndent(f, "", " ")
}
