// SPDX-License-Identifier: MIT

package main

import (
	"encoding/json"
	"os"
	"strings"
)

// Provided is a module providers.toml gives to a provider outside the tree, built by xtask before the converter runs (`upstream:` rows): dependents link its artifact and get its compile flags, as from a module the converter built.
type Provided struct {
	Lib    string   `json:"lib"`    // absolute path of the shared object (.so) or archive (.a) dependents link
	Cflags []string `json:"cflags"` // compile flags its dependents need: include dirs, defines its headers depend on
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
	n := &Node{key: k, mod: &Module{Name: name}, props: newObj(), out: p.Lib, exports: p.Cflags}
	g.nodes[k] = n
	return n
}
