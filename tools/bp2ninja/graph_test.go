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
	b, err := g.graphJSON([]*Node{goal}, "c++", "cc")
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

// artifacts: what a cargo package links of the graph, as the converter's own links take it: an rlib with its crate name, crate dirs and its cc libraries' link arguments (rust_test.go rustGraph:
// libffi carries libcc whole, libdep only a proc macro, so no link arguments); shared libraries, static archives and binaries by output; never headers, proc macros or generators.
func TestGraphJSONArtifacts(t *testing.T) {
	g, _ := rustGraph(t)
	goal := g.node("libuser", Device, Shared, "goal")
	b, err := g.graphJSON([]*Node{goal}, "c++", "cc")
	if err != nil {
		t.Fatal(err)
	}
	var f graphFile
	if err := json.Unmarshal(b, &f); err != nil {
		t.Fatal(err)
	}
	if f.LinkDriver != "c++" {
		t.Errorf("link driver %q", f.LinkDriver)
	}
	out := func(p string) string { return filepath.Join(g.out, p) }
	ffi := f.Artifacts["device/libffi"]
	if ffi.Kind != "rlib" || ffi.Out != out("device/libffi/libffi.rlib") || ffi.Crate != "ffi" {
		t.Errorf("libffi %+v", ffi)
	}
	// its own dir first, then the closure's: crateClosure order, the proc macro's dir included (rustc loads it with libdep's metadata)
	if !slices.Equal(ffi.Search, []string{out("device/libffi"), out("device/libdep"), out("host/libmac")}) {
		t.Errorf("libffi search %q", ffi.Search)
	}
	// the cc library the rlib names whole, linked as a final link of libffi would (linkLibs: a static closure member in the group)
	if !slices.Contains(ffi.Link, out("device/libcc/libcc.a")) || !containsSeq(ffi.Link, []string{"-Wl,--start-group"}) {
		t.Errorf("libffi link %q", ffi.Link)
	}
	if !slices.Equal(ffi.Native, []string{out("device/libcc")}) {
		t.Errorf("libffi native %q", ffi.Native)
	}
	if dep := f.Artifacts["device/libdep"]; dep.Kind != "rlib" || dep.Crate != "dep" || len(dep.Link) != 0 {
		t.Errorf("libdep %+v", dep)
	}
	if u := f.Artifacts["device/libuser"]; u.Kind != "shared" || u.Out != out("device/libuser/libuser.so") || u.Crate != "" || u.Search != nil {
		t.Errorf("libuser %+v", u)
	}
	// a shared artifact carries the C++ argv its own compiles take (a package compiling a C++ wrapper against it uses it): the language standard is among them
	if u := f.Artifacts["device/libuser"]; !slices.ContainsFunc(u.Cxx, func(s string) bool { return strings.HasPrefix(s, "-std=") }) {
		t.Errorf("libuser cxx %q", u.Cxx)
	}
	if tool := f.Artifacts["host/tool"]; tool.Kind != "binary" || tool.Out != out("host/tool/tool") {
		t.Errorf("tool %+v", tool)
	}
	// a static archive is an artifact at its own path: libcc, the cc library libffi carries whole
	if cc := f.Artifacts["device/libcc"]; cc.Kind != "static" || cc.Out != out("device/libcc/libcc.a") {
		t.Errorf("libcc %+v", cc)
	}
	for _, none := range []string{"host/libmac", "device/bridge_h"} {
		if a, ok := f.Artifacts[none]; ok {
			t.Errorf("%s: artifact %+v", none, a)
		}
	}
}

// static archives and provided modules: a static artifact is its archive with the link arguments a final link of it takes beyond it (linkLibs: here the provided library the module names); a provided module is an artifact at its real path.
// A shared artifact names the provided shared libraries its own link takes (needs); a module with a shared and a static node is the shared artifact.
func TestGraphJSONStaticAndProvided(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_library_static { name: "libsub", srcs: ["s.cc"] }
cc_library { name: "libboth", srcs: ["b.cc"] }
cc_library_static { name: "libaudioprocessing_base", srcs: ["a.cc"], static_libs: ["libsub", "libboth"], shared_libs: ["libcrypto"] }
cc_library_shared { name: "libuser", srcs: ["u.cc"], static_libs: ["libaudioprocessing_base"], shared_libs: ["libcrypto", "libboth"] }
`})
	crypto := "/upstream/boringssl/prefix/lib/libcrypto.so"
	g.provided = map[string]Provided{"libcrypto": {Lib: crypto}}
	goal := g.node("libuser", Device, Shared, "goal")
	if goal == nil {
		t.Fatal(g.problems)
	}
	g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"}) // sets node outs (main.go: emit, then graphJSON)
	b, err := g.graphJSON([]*Node{goal}, "c++", "cc")
	if err != nil {
		t.Fatal(err)
	}
	var f graphFile
	if err := json.Unmarshal(b, &f); err != nil {
		t.Fatal(err)
	}
	out := func(p string) string { return filepath.Join(g.out, p) }
	ap := f.Artifacts["device/libaudioprocessing_base"]
	if ap.Kind != "static" || ap.Out != out("device/libaudioprocessing_base/libaudioprocessing_base.a") {
		t.Errorf("static artifact %+v", ap)
	}
	// its link: the static closure's archives and the shared library it names, as a final link of it takes them
	for _, want := range []string{out("device/libsub/libsub.a"), out("device/libboth/libboth.a"), crypto} {
		if !slices.Contains(ap.Link, want) {
			t.Errorf("link %q lacks %s", ap.Link, want)
		}
	}
	if len(ap.Needs) != 0 {
		t.Errorf("static needs %q", ap.Needs)
	}
	if want := []string{"device/libboth", "device/libcrypto", "device/libsub"}; !slices.Equal(f.Deps["device/libaudioprocessing_base"], want) {
		t.Errorf("deps %q, want %q", f.Deps["device/libaudioprocessing_base"], want)
	}
	if c := f.Artifacts["device/libcrypto"]; c.Kind != "shared" || c.Out != crypto || len(c.Needs) != 0 {
		t.Errorf("provided artifact %+v", c)
	}
	// libboth has a shared and a static node: the shared artifact stands
	if lib := f.Artifacts["device/libboth"]; lib.Kind != "shared" || lib.Out != out("device/libboth/libboth.so") {
		t.Errorf("libboth %+v", lib)
	}
	if u := f.Artifacts["device/libuser"]; !slices.Equal(u.Needs, []string{"device/libcrypto"}) {
		t.Errorf("libuser needs %q", u.Needs)
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
	b, err := g.graphJSON([]*Node{goal}, "c++", "cc")
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

// The device record carries the libc provider (headers, library + its rpath), the platform defines and the stl, not a module's own flags.
func TestDevicePlatform(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `cc_library { name: "libc", srcs: ["c.cc"] }`})
	g.soong = SoongConfig{CStd: "gnu11", CppStd: "gnu++20", APILevel: "-D__ANDROID_API__=10000", SystemSharedLibs: []string{"libc", "libm", "libdl"}}
	g.provided = map[string]Provided{"libc": {Lib: "/c/lib/libatl_bionic.so", Include: []string{"/c/include"}}}
	p := g.devicePlatform("clang-19")
	if p.CC != "clang-19" {
		t.Errorf("cc %q", p.CC)
	}
	for _, flags := range [][]string{p.C, p.Cxx} {
		for _, want := range []string{androidDefine, lfs64Define, defaultSourceDefine, "-D__ANDROID_API__=10000", "/c/include"} {
			if !slices.Contains(flags, want) {
				t.Errorf("flags lack %s: %v", want, flags)
			}
		}
	}
	if !slices.Contains(p.Cxx, "-nostdinc++") {
		t.Errorf("cxx lacks the stl flags: %v", p.Cxx)
	}
	// libc++ is the default stl of a shared module (stl.go)
	want := slices.Concat([]string{"/c/lib/libatl_bionic.so"}, stlLinkFlags("libc++"), systemLinkFlags)
	if !slices.Equal(p.Link, want) {
		t.Errorf("link %v, want %v", p.Link, want)
	}
}
