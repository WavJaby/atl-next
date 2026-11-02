// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"
)

// AIDL crates of workspace packages (-aidl-crates; Cargo.toml `[package.metadata.atl] aidl`, xtask packages.rs): each entry becomes an unstable
// aidl_interface defined at the tree root with srcs = the package's list (filegroups, tree-relative .aidl paths), so its `<name>-rust` backend is
// built by synthAidl + rustAidl like any tree interface: same aidl flags, same crate-root glue, each source's filegroup root as -I (Soong's import dirs).
// A name a tree module already has, or a src naming no module / no file, is an error: nothing would build what the package asked for.
func (g *Graph) addAidlCrates(crates map[string][]string) error {
	names := make([]string, 0, len(crates))
	for n := range crates {
		names = append(names, n)
	}
	sort.Strings(names)
	for _, name := range names {
		srcs := crates[name]
		if len(srcs) == 0 {
			return fmt.Errorf("aidl crate %s: no srcs", name)
		}
		if len(g.mods[Device][name]) > 0 {
			return fmt.Errorf("aidl crate %s: the tree already has a module of that name", name)
		}
		for _, s := range srcs {
			if isRef(s) {
				if g.lookup(s, Device) == nil {
					return fmt.Errorf("aidl crate %s: %s names no module in the tree", name, s)
				}
			} else if _, err := os.Stat(filepath.Join(g.tree, s)); err != nil {
				return fmt.Errorf("aidl crate %s: %w", name, err)
			}
		}
		p := newObj()
		p.set("unstable", true)
		p.set("srcs", strVals(srcs))
		g.mods[Device][name] = append(g.mods[Device][name], &Module{Type: "aidl_interface", Name: name, Props: p})
	}
	return nil
}
