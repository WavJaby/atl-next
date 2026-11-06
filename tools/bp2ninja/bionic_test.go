// SPDX-License-Identifier: MIT

package main

import (
	"slices"
	"testing"
)

// Expected deps are Soong's cc/stl.go (*stl).deps for a device, non-SDK, Bionic-toolchain module: libc++ shared or libc++_static static, then libc++demangle static (ctx.Device() && !useSdk),
// and for a static binary libm, libc and staticUnwinder's libunwind (non-legacy arch). StaticUnwinderIfLegacy only matters on legacy arm32: no deps here. stl "none" -> "" -> nothing.
func TestBionicStlDeps(t *testing.T) {
	g := graphForCfg(t, nbCfg, map[string]string{"p/Android.bp": `
cc_library_shared { name: "libshared", srcs: ["a.cpp"] }
cc_library_static { name: "libstat", srcs: ["a.cpp"] }
cc_binary { name: "sbin", srcs: ["a.cpp"], static_executable: true }
cc_library_shared { name: "libnone", srcs: ["a.c"], stl: "none" }
`})
	nb := NativeBridge("x86_64")
	type want struct{ shared, static []string }
	for _, c := range []struct {
		name string
		kind Kind
		want want
	}{
		{"libshared", Shared, want{[]string{"libc++"}, []string{"libc++demangle"}}},
		{"libstat", Static, want{nil, []string{"libc++_static", "libc++demangle"}}},
		{"sbin", Binary, want{nil, []string{"libc++_static", "libc++demangle", "libm", "libc", "libunwind"}}},
		{"libnone", Shared, want{nil, nil}},
	} {
		n := g.node(c.name, nb, c.kind, "t")
		if n == nil {
			t.Fatalf("%s: no node, problems %q", c.name, g.problems)
		}
		n.deps.shared, n.deps.static = nil, nil
		got := map[Kind][]string{}
		g.bionicStlDeps(n, func(names []string, k Kind) []*Node {
			var out []*Node
			for _, s := range names {
				got[k] = append(got[k], s)
				out = append(out, &Node{key: key{name: s}})
			}
			return out
		})
		if !slices.Equal(got[Shared], c.want.shared) || !slices.Equal(got[Static], c.want.static) {
			t.Errorf("%s: shared %q static %q, want %q %q", c.name, got[Shared], got[Static], c.want.shared, c.want.static)
		}
	}
}

// Expected set is Soong's: cc.go depsToPaths takes only directly named static deps into the link (orderStaticModuleDeps orders, never adds; cc.go:3742),
// and a static library's whole_static_libs are objects of its own archive (library.go), so a whole member links wherever its container does. A static dep's static dep is not linked.
func TestDirectStatic(t *testing.T) {
	node := func(name string) *Node { return &Node{key: key{name: name}} }
	n, a, b, w, x, y := node("n"), node("a"), node("b"), node("w"), node("x"), node("y")
	n.deps.whole, n.deps.static = []*Node{x}, []*Node{a}
	a.deps.static, a.deps.whole = []*Node{b}, []*Node{w}
	x.deps.whole = []*Node{y}
	var out []*Node
	directStatic(n, &out)
	var got []string
	for _, d := range out {
		got = append(got, d.name)
	}
	// whole first (Soong links them inside --whole-archive before the static list), each container followed by its members
	if want := []string{"x", "y", "a", "w"}; !slices.Equal(got, want) {
		t.Errorf("link set %q, want %q", got, want)
	}
}
