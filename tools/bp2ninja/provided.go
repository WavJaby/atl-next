// SPDX-License-Identifier: MIT

package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// Provided is a module providers.toml gives to a provider outside the tree (`crate:` and `upstream:` rows), built by xtask before the converter runs: it replaces the tree's definition, and dependents link its artifact and get its headers and defines, as from a module the converter built.
type Provided struct {
	Lib string `json:"lib"` // absolute path of the shared object (.so) or archive (.a) dependents link
	// header dirs dependents get as -isystem: searched after every -I, so compat/device keeps precedence over bionic's headers; system headers, as bionic's are
	Include []string `json:"include"`
	Defines []string `json:"defines"` // NAME=VALUE its headers depend on, given to dependents as -D (BoringSSL's symbol prefix)
}

// Partial is a providers.toml row with `srcs`: the module is built from the tree without those sources, and links the provider's artifact (nil for `none`) first, as a provided shared library, for the symbols they defined.
type Partial struct {
	Srcs     []string  `json:"srcs"` // relative to the module's directory, as its Android.bp writes them
	Provider *Provided `json:"provider"`
}

// readJSON reads a file xtask native writes (-provided, -srcs); an empty path = none given.
func readJSON[T any](path string) (map[string]T, error) {
	if path == "" {
		return nil, nil
	}
	text, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var out map[string]T
	return out, json.Unmarshal(text, &out)
}

// dropPartial removes the module's `srcs` row files from its sources and records each one found, for checkPartial.
func (g *Graph) dropPartial(n *Node, files []srcFile) []srcFile {
	part, ok := g.partial[n.mod.Name]
	if !ok {
		return files
	}
	drop := map[string]bool{}
	for _, s := range part.Srcs {
		drop[filepath.Join(n.mod.Dir, s)] = true
	}
	var out []srcFile
	for _, f := range files {
		if f.gen == nil && drop[f.path] {
			g.droppedSrcs = addKey(g.droppedSrcs, n.mod.Name+"\x00"+f.path)
			continue
		}
		out = append(out, f)
	}
	return out
}

// partialProvider: the provided node a `srcs` row's provider links into n, nil for none. Named after its artifact: the module's own name is n's.
func (g *Graph) partialProvider(n *Node) *Node {
	part, ok := g.partial[n.mod.Name]
	if !ok || part.Provider == nil {
		return nil
	}
	lib := filepath.Base(part.Provider.Lib)
	return g.providedNode(strings.TrimSuffix(lib, filepath.Ext(lib)), n.side, *part.Provider)
}

// checkPartial: every file a `srcs` row lists must be a source of its module at the pin, a typo or a file upstream moved would otherwise drop nothing, silently.
// Where the goals build the module, the file must have been dropped from at least one of its nodes (sides and target blocks differ in sources); where they do not, it must at least exist in the module's directory.
func (g *Graph) checkPartial() error {
	var bad []string
	for _, name := range sortedKeys(keysOf(g.partial)) {
		built := false
		for k := range g.nodes {
			if k.name == name && !g.nodes[k].provided {
				built = true
			}
		}
		mods := append(append([]*Module{}, g.mods[Device][name]...), g.mods[Host][name]...)
		if len(mods) == 0 {
			continue // "names no module in the tree", reported with the other rows
		}
		for _, s := range g.partial[name].Srcs {
			path := filepath.Join(mods[0].Dir, s)
			if built && !g.droppedSrcs[name+"\x00"+path] {
				bad = append(bad, fmt.Sprintf("%s: %s is not one of its sources", name, s))
			} else if _, err := os.Stat(g.abs(path)); !built && err != nil {
				bad = append(bad, fmt.Sprintf("%s: %s does not exist", name, path))
			}
		}
	}
	if len(bad) > 0 {
		return fmt.Errorf("providers.toml srcs: %s", strings.Join(bad, "; "))
	}
	return nil
}

func keysOf[T any](m map[string]T) map[string]bool {
	out := map[string]bool{}
	for k := range m {
		out[k] = true
	}
	return out
}

// providedNode: the node of a provided module, one per side; no sources, so it is not in g.order.
// Its only edges: the tree module's export_header_lib_headers, which still reach its dependents -- header-only tree modules the provider's headers include as the replaced ones did (async_safe/log.h -> liblog_headers' android/log.h).
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
	g.nodes[k] = n
	for _, dir := range p.Include {
		n.exports = append(n.exports, "-isystem", dir)
	}
	for _, d := range p.Defines {
		n.exports = append(n.exports, "-D"+d)
	}
	if m := g.lookup(name, side); m != nil {
		for _, r := range g.flatten(m, side, kind).strs("export_header_lib_headers") {
			// a header dependency, so the dependents' compiles wait for what it generates
			if h := g.node(r, side, Headers, name); h != nil {
				n.deps.headers = append(n.deps.headers, h)
				n.exports = append(n.exports, h.exports...)
			}
		}
		n.exports = dedup(n.exports)
	}
	return n
}
