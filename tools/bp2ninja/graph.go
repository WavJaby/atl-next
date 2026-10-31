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
}

func (n *Node) dir() string { return n.side.String() + "/" + n.name }

// stepDeps: the nodes n's steps wait for: linked and header libraries, Rust crates, cc_object sources, consumed genrules, the host tools of its generators and of its genrule command.
func (n *Node) stepDeps() []*Node {
	deps := slices.Concat(n.deps.whole, n.deps.static, n.deps.shared, n.deps.headers, n.deps.crates, n.objSrcs)
	for gn := range n.usedGens {
		deps = append(deps, gn)
	}
	for _, e := range n.edges {
		if e.toolNode != nil {
			deps = append(deps, e.toolNode)
		}
	}
	if n.rule != nil {
		deps = append(deps, n.rule.tools...)
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

// graphJSON: graph.json for the emitted nodes and the goal nodes found.
func (g *Graph) graphJSON(goals []*Node) ([]byte, error) {
	f := graphFile{Goals: []string{}, Deps: map[string][]string{}, Dropped: map[string][]string{}, NoneExports: map[string][]string{}}
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
	}
	return json.MarshalIndent(f, "", " ")
}
