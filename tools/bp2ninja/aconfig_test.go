// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
)

// Expected commands, outputs and dependencies are derived by hand from build/soong/aconfig: init.go (aconfigRule, cppRule),
// aconfig_declarations.go, codegen/cc_aconfig_library.go (GeneratorDeps, GeneratorSources, GeneratorBuildActions) and aconfig's
// codegen_cpp.rs file names (<pkg with . -> _>.cc, include/<same>.h), never from the code under test.

const aconfigTree = `
cc_library { name: "libaconfig_storage_read_api_cc", srcs: ["s.cc"] }
cc_library { name: "libbase", srcs: ["b.cc"] }
cc_library { name: "liblog", srcs: ["l.cc"] }
aconfig_value_set { name: "vs", values: ["vals"] }
aconfig_values { name: "vals", package: "com.example.flags", srcs: ["v.textproto"] }
aconfig_declarations { name: "decl", package: "com.example.flags", container: "system", srcs: ["f.aconfig"] }
cc_aconfig_library { name: "libflags", aconfig_declarations: "decl" }
cc_aconfig_library { name: "libflags-test", aconfig_declarations: "decl", mode: "test" }
cc_aconfig_library { name: "libflags-ro", aconfig_declarations: "decl", mode: "force-read-only" }
cc_aconfig_library { name: "libflags-bad", aconfig_declarations: "decl", mode: "bogus" }
cc_binary { name: "user", srcs: ["u.cc"], shared_libs: ["libflags"] }
`

func aconfigGraphFor(t *testing.T) *Graph {
	g := graphFor(t, map[string]string{"p/Android.bp": aconfigTree, "p/f.aconfig": "", "p/v.textproto": ""})
	g.release = &Release{Name: "r", ValueSets: []string{"vs"}, flags: map[string]string{
		"RELEASE_ACONFIG_FLAG_DEFAULT_PERMISSION": "READ_WRITE", "RELEASE_ACONFIG_REQUIRE_ALL_READ_ONLY": "",
	}}
	g.acfg = &aconfigGraph{tool: "/aconfig", decls: map[string]*aconfigDecl{}}
	return g
}

func TestCcAconfigLibrary(t *testing.T) {
	g := aconfigGraphFor(t)
	n := g.node("libflags", Device, Shared, "t")
	if n == nil {
		t.Fatalf("problems %v", g.problems)
	}
	gd := filepath.Join(g.out, "gen", "device", "libflags", "aconfig")
	cache := filepath.Join(g.out, "aconfig", "decl", "intermediate.pb")
	// cppRule: input = the declarations' cache, outputs <gendir>/<pkg>.cc and <gendir>/include/<pkg>.h, args gendir + mode (default production)
	if len(n.edges) != 1 {
		t.Fatalf("%d edges", len(n.edges))
	}
	e := n.edges[0]
	if e.rule != "aconfig_cc" || e.in != cache || !reflect.DeepEqual(e.outs, []string{gd + "/com_example_flags.cc", gd + "/include/com_example_flags.h"}) {
		t.Errorf("edge %+v", e)
	}
	if got := edgeVars(e); !reflect.DeepEqual(got, map[string]string{"gendir": gd, "mode": "production"}) {
		t.Errorf("vars %v", got)
	}
	// CommandDeps: $aconfig
	if !slices.Contains(e.implicit, "/aconfig") {
		t.Errorf("implicit %v", e.implicit)
	}
	if !reflect.DeepEqual(n.srcs, []string{gd + "/com_example_flags.cc"}) {
		t.Errorf("srcs %v", n.srcs)
	}
	// IncludeDirs + ReexportedDirs: include/
	inc := "-I" + gd + "/include"
	if !slices.Contains(n.genIncs, inc) || !slices.Contains(n.exports, inc) {
		t.Errorf("incs %v exports %v", n.genIncs, n.exports)
	}
	// GeneratorDeps: not force-read-only -> shared_libs libaconfig_storage_read_api_cc, libbase, liblog
	if got := depNames(n.deps.shared); !reflect.DeepEqual(got, []string{"libaconfig_storage_read_api_cc", "libbase", "liblog"}) {
		t.Errorf("shared %v", got)
	}
	// a dependent sees the exported include dir and waits for the header
	u := g.node("user", Device, Binary, "t")
	var waits []string
	genClosure(u, map[*Node]bool{}, &waits)
	if !slices.Contains(waits, gd+"/include/com_example_flags.h") {
		t.Errorf("user waits %v", waits)
	}
	// ninja rejects a build statement whose rule is defined later in the file
	ninja := g.emit(Tools{})
	rule, use := strings.Index(ninja, "rule aconfig_cc\n"), strings.Index(ninja, ": aconfig_cc ")
	if rule < 0 || use < 0 || rule > use {
		t.Errorf("rule at %d, first use at %d", rule, use)
	}
}

func TestAconfigModes(t *testing.T) {
	g := aconfigGraphFor(t)
	test := g.node("libflags-test", Device, Shared, "t")
	if test == nil || len(test.edges) != 1 || edgeVars(test.edges[0])["mode"] != "test" {
		t.Fatalf("test mode: %+v %v", test, g.problems)
	}
	ro := g.node("libflags-ro", Device, Static, "t")
	if ro == nil || len(ro.edges) != 1 || edgeVars(ro.edges[0])["mode"] != "force-read-only" {
		t.Fatalf("force-read-only: %+v %v", ro, g.problems)
	}
	// force-read-only: no flag storage libraries
	if len(ro.deps.shared) != 0 {
		t.Errorf("force-read-only shared %v", depNames(ro.deps.shared))
	}
	// isModeSupported: PropertyErrorf -> nothing generated, one problem
	bad := g.node("libflags-bad", Device, Shared, "t")
	if bad == nil || len(bad.edges) != 0 || !g.hasProblem(`"bogus" is not a supported mode`) {
		t.Errorf("bogus mode: %+v %v", bad, g.problems)
	}
}

// One cache per aconfig_declarations, whichever libraries name it (IntermediateCacheOutputPath), with aconfigRule's arguments.
func TestAconfigCacheShared(t *testing.T) {
	g := aconfigGraphFor(t)
	g.node("libflags", Device, Shared, "t")
	g.node("libflags-test", Device, Shared, "t")
	ninja := g.acfg.ninja()
	cache := filepath.Join(g.out, "aconfig", "decl", "intermediate.pb")
	if c := strings.Count(ninja, "build "+esc(cache)+": aconfig_cache"); c != 1 {
		t.Fatalf("%d cache edges:\n%s", c, ninja)
	}
	vars := ninjaEdgeVars(ninja, esc(cache))
	want := map[string]string{
		"":             "build " + esc(cache) + ": aconfig_cache " + esc(g.abs("p/f.aconfig")) + " " + esc(g.abs("p/v.textproto")) + " | /aconfig",
		"package":      "com.example.flags",
		"declarations": "--declarations " + esc(g.abs("p/f.aconfig")),
		"values":       "--values " + esc(g.abs("p/v.textproto")),
		"permission":   "--default-permission READ_WRITE",
		"readwrite":    "--allow-read-write true",
		"container":    "--container system",
	}
	if !reflect.DeepEqual(vars, want) {
		t.Errorf("cache edge %v, want %v", vars, want)
	}
	// cppRule's command
	if !strings.Contains(ninja, "command = rm -rf $gendir && mkdir -p $gendir && $aconfig create-cpp-lib --mode $mode --cache $in --out $gendir\n") {
		t.Errorf("aconfig_cc rule:\n%s", ninja)
	}
}

func TestCcAconfigWithoutTool(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": aconfigTree, "p/f.aconfig": "", "p/v.textproto": ""})
	n := g.node("libflags", Device, Shared, "t")
	if n == nil || len(n.edges) != 0 || !g.hasProblem("needs -aconfig and -release") {
		t.Errorf("%+v %v", n, g.problems)
	}
}
