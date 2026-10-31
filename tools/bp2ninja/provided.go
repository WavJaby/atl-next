// SPDX-License-Identifier: MIT

package main

import (
	"encoding/json"
	"os"
	"strings"
)

// Provided is a module providers.toml gives to a provider outside the tree (`crate:` and `upstream:` rows), built by xtask before the converter runs: it replaces the tree's definition, and dependents link its artifact and get its headers and defines, as from a module the converter built.
type Provided struct {
	Lib string `json:"lib"` // absolute path of the shared object (.so) or archive (.a) dependents link
	// header dirs dependents get as -isystem: searched after every -I, so compat/device keeps precedence over bionic's headers; system headers, as bionic's are
	Include []string `json:"include"`
	Defines []string `json:"defines"` // NAME=VALUE its headers depend on, given to dependents as -D (BoringSSL's symbol prefix)
}

// readProvided reads the -provided file: module name -> Provided, as xtask native writes it.
func readProvided(path string) (map[string]Provided, error) {
	if path == "" {
		return nil, nil
	}
	text, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var out map[string]Provided
	return out, json.Unmarshal(text, &out)
}

// providedNode: the node of a provided module, one per side; no sources and no edges of its own, so it is not in g.order.
func (g *Graph) providedNode(name string, side Side, p Provided) *Node {
	kind := Shared
	if strings.HasSuffix(p.Lib, ".a") {
		kind = Static
	}
	k := key{name, side, kind}
	if n, ok := g.nodes[k]; ok {
		return n
	}
	n := &Node{key: k, mod: &Module{Name: name}, props: newObj(), out: p.Lib, provided: true}
	for _, dir := range p.Include {
		n.exports = append(n.exports, "-isystem", dir)
	}
	for _, d := range p.Defines {
		n.exports = append(n.exports, "-D"+d)
	}
	g.nodes[k] = n
	return n
}
