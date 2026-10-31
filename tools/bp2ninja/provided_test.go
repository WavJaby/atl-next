// SPDX-License-Identifier: MIT

package main

import (
	"slices"
	"strings"
	"testing"
)

// A provided module replaces the tree's definition: no compile or link step of its own, its artifact on the dependent's link line (shared_libs and static_libs alike), its cflags on the dependent's compiles.
func TestProvidedModule(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_library { name: "libcrypto", srcs: ["c.cc"], export_include_dirs: ["inc"] }
cc_library { name: "libuser", srcs: ["u.cc"], shared_libs: ["libcrypto"] }
cc_library { name: "libstat", srcs: ["s.cc"], static_libs: ["libcrypto"] }
`})
	lib := "/prefix/lib/libcrypto.so"
	g.provided = map[string]Provided{"libcrypto": {Lib: lib, Cflags: []string{"-I/prefix/include", "-DPFX=X"}}}
	for _, goal := range []string{"libuser", "libstat"} {
		if g.node(goal, Device, Shared, "goal") == nil {
			t.Fatalf("%s: problems %v", goal, g.problems)
		}
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	if strings.Contains(ninja, "/p/c.cc") {
		t.Error("the tree's libcrypto source is compiled")
	}
	for _, goal := range []string{"libuser", "libstat"} {
		var link, compile string
		for _, l := range strings.Split(ninja, "\n") {
			if strings.HasPrefix(l, "build ") && strings.Contains(l, "/"+goal+".so: link ") {
				link = l
			}
			if strings.HasPrefix(l, "build ") && strings.Contains(l, "/obj/device/"+goal+"/") {
				compile = l
			}
		}
		if !strings.Contains(link, "| "+lib) {
			t.Errorf("%s: link does not wait for %s: %q", goal, lib, link)
		}
		var flags []string
		for _, n := range g.order {
			if n.name == goal {
				flags = g.compileFlags(n).cxx
				l, _ := g.linkLibs(n)
				if !slices.Contains(l, lib) || !slices.Contains(l, "-Wl,-rpath,/prefix/lib") {
					t.Errorf("%s: link libs %q", goal, l)
				}
			}
		}
		if compile == "" || !slices.Contains(flags, "-I/prefix/include") || !slices.Contains(flags, "-DPFX=X") {
			t.Errorf("%s: compile flags %q", goal, flags)
		}
	}
}
