// SPDX-License-Identifier: MIT

package main

import (
	"encoding/json"
	"slices"
)

// graphFile is <out>/graph.json: the cc build graph by module directory, for the caller's failure triage (which failed modules a goal reaches, which have no failed dependency).
// A module directory `<side>/<name>` is where a node's steps write (obj/<side>/<name>, gen/<side>/<name>, <side>/<name>): ninja's FAILED lines name it, so failures map onto nodes without re-parsing build.ninja.
// The static and shared nodes of one module share it.
type graphFile struct {
	Goals []string `json:"goals"`
	// module directory -> the module directories its steps wait for, sorted
	Deps map[string][]string `json:"deps"`
}

func (n *Node) dir() string { return n.side.String() + "/" + n.name }

// stepDeps: the nodes n's steps wait for: linked and header libraries, cc_object sources, consumed genrules, the host tools of its generators and of its genrule command.
func (n *Node) stepDeps() []*Node {
	deps := slices.Concat(n.deps.whole, n.deps.static, n.deps.shared, n.deps.headers, n.objSrcs)
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

// graphJSON: graph.json for the emitted nodes and the goal nodes found.
func (g *Graph) graphJSON(goals []*Node) ([]byte, error) {
	f := graphFile{Goals: []string{}, Deps: map[string][]string{}}
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
