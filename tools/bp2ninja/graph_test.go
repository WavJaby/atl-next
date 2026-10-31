// SPDX-License-Identifier: MIT

package main

import (
	"encoding/json"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// graph.json: one entry per module directory, an edge for each kind of thing a step waits for, and directories that are the ones emitted steps write under.
func TestGraphJSON(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_binary_host { name: "gentool", srcs: ["t.cc"] }
genrule { name: "gen_h", tools: ["gentool"], cmd: "$(location gentool) > $(out)", out: ["h.h"] }
cc_library_headers { name: "hdrs", export_include_dirs: ["inc"] }
cc_library_static { name: "libs", srcs: ["s.cc"] }
cc_library_shared { name: "libd", srcs: ["d.cc"], static_libs: ["libs"] }
cc_library { name: "libgoal", srcs: ["g.cc"], static_libs: ["libs"], shared_libs: ["libd"], header_libs: ["hdrs"], generated_headers: ["gen_h"] }
`})
	goal := g.node("libgoal", Device, Shared, "goal")
	if goal == nil {
		t.Fatal(g.problems)
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	b, err := g.graphJSON([]*Node{goal})
	if err != nil {
		t.Fatal(err)
	}
	var f graphFile
	if err := json.Unmarshal(b, &f); err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(f.Goals, []string{"device/libgoal"}) {
		t.Errorf("goals %v", f.Goals)
	}
	want := map[string][]string{
		"device/libgoal": {"device/gen_h", "device/hdrs", "device/libd", "device/libs"},
		"device/libd":    {"device/libs"},
		"device/libs":    {},
		"device/hdrs":    {},
		"device/gen_h":   {"host/gentool"},
		"host/gentool":   {},
	}
	for dir, deps := range want {
		if got, ok := f.Deps[dir]; !ok || !slices.Equal(got, deps) {
			t.Errorf("%s: deps %v, want %v", dir, got, deps)
		}
	}
	if len(f.Deps) != len(want) {
		t.Errorf("entries %v", f.Deps)
	}
	// the directories ninja's FAILED lines name: obj/ for compiles, gen/ for genrules, the artifact dir for links
	for _, p := range []string{"obj/device/libgoal/", "obj/host/gentool/", "gen/device/gen_h/", "device/libgoal/libgoal.so", "host/gentool/gentool"} {
		if !strings.Contains(ninja, filepath.Join(g.out, p)) {
			t.Errorf("no step writes under %s", p)
		}
	}
}

// providers.toml `none` modules: each dropped edge by module directory, and the directories the omitted module's headers would come from (re-exported header libraries followed), without the omitted module's own lookups counting as the build's problems.
func TestGraphJSONDroppedNone(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_library_headers { name: "libh", export_include_dirs: ["h"] }
cc_library { name: "libnone", defaults: ["gone_defaults"], srcs: ["n.cc"], export_include_dirs: ["inc"], target: { android: { export_system_include_dirs: ["sys"] } }, header_libs: ["libh"], export_header_lib_headers: ["libh"] }
cc_library { name: "libuser", srcs: ["u.cc"], shared_libs: ["libnone"] }
cc_library { name: "libgoal", srcs: ["g.cc"], shared_libs: ["libuser"], static_libs: ["libnone"] }
`})
	g.omit = map[string]bool{"libnone": true}
	goal := g.node("libgoal", Device, Shared, "goal")
	if goal == nil {
		t.Fatal(g.problems)
	}
	b, err := g.graphJSON([]*Node{goal})
	if err != nil {
		t.Fatal(err)
	}
	var f graphFile
	if err := json.Unmarshal(b, &f); err != nil {
		t.Fatal(err)
	}
	if len(f.Dropped) != 2 || !slices.Equal(f.Dropped["device/libuser"], []string{"libnone"}) || !slices.Equal(f.Dropped["device/libgoal"], []string{"libnone"}) {
		t.Errorf("dropped %v", f.Dropped)
	}
	if got := f.NoneExports["device/libnone"]; !slices.Equal(got, []string{"p/h", "p/inc", "p/sys"}) || len(f.NoneExports) != 1 {
		t.Errorf("none exports %v", f.NoneExports)
	}
	if len(g.problems) != 0 || len(g.missing) != 0 {
		t.Errorf("libnone's missing defaults counted: %v %v", g.problems, g.missing)
	}
}
