// SPDX-License-Identifier: MIT

package main

import "sort"

// Modules a module type's load hook creates, defined in no Android.bp: lookup falls back to synth for a name no module answers to, and for a
// hidl_interface (named like its cc library). One synthesizer per module type, each returning nil without a problem when ref is none of its names
// and nil with one when it is but cannot be built; the result is memoized per ref and side.
//   - aidl_interface: `<iface>[-V<n>]-<cpp|ndk>` cc libraries (aidliface.go).
//   - hidl_interface: the cc library `<pkg>@<ver>`, genrules `<pkg>@<ver>_genc++[_headers]`, filegroup `<pkg>@<ver>_hal` (hidliface.go).
//   - sysprop_library: the cc library `lib<name>` (sysprop.go).
//
// A name a synthesizer owns (its base module is in the tree) never goes to missing.txt: lookup said why it failed. One whose base module is absent
// does, and the AOSP index (xtask aosp.rs) maps it to the project defining the base module.

func (g *Graph) synth(ref string, side Side) *Module {
	return g.synthMemo(ref, side, func() (m *Module) {
		for _, f := range []func(string, Side) *Module{g.synthAidl, g.synthHidl, g.synthSysprop} {
			if m = f(ref, side); m != nil {
				break
			}
		}
		return m
	})
}

// synthMemo: make's result for the module name ref on that side, made once.
func (g *Graph) synthMemo(ref string, side Side, make func() *Module) *Module {
	k := ref + "/" + side.String()
	if m, ok := g.synthMods[k]; ok {
		return m
	}
	m := make()
	if g.synthMods == nil {
		g.synthMods = map[string]*Module{}
	}
	g.synthMods[k] = m
	return m
}

// synthOwned: ref names a module some synthesizer would make from a module the tree has.
func (g *Graph) synthOwned(ref string) bool {
	return g.aidlOwned(ref) || g.hidlOwned(ref) || g.syspropOwned(ref)
}

// rawModule: the module of that exact name and type as an Android.bp defines it (lookup would return what a synthesizer makes of it).
func (g *Graph) rawModule(name string, side Side, typ string) *Module {
	var cands []*Module
	for _, m := range g.mods[side][name] {
		if m.Type == typ {
			cands = append(cands, m)
		}
	}
	if len(cands) == 0 {
		return nil
	}
	sort.Slice(cands, func(i, j int) bool { return cands[i].Dir < cands[j].Dir })
	return cands[0]
}
