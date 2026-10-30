// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
)

// Expected names, properties, output paths and tool flags are derived by hand from build/soong/sysprop/sysprop_library.go (syspropLibraryHook) and
// build/soong/cc/{gen.go genSysprop + the sysprop rule, compiler.go, library.go}, never from the code under test.

const syspropTree = `
cc_binary_host { name: "sysprop_cpp", srcs: ["main.cpp"] }
cc_library_headers { name: "libbase_headers", export_include_dirs: ["base/inc"] }
cc_library { name: "libbase", srcs: ["b.cc"], export_include_dirs: ["base/inc"] }
cc_library { name: "liblog", srcs: ["l.cc"], export_include_dirs: ["log/inc"] }

sysprop_library {
    name: "PlatProps",
    srcs: ["A.sysprop", "sub/B.sysprop"],
    property_owner: "Platform",
    cpp: { cflags: ["-DPLAT"], min_sdk_version: "29" },
}
sysprop_library { name: "GlobProps", srcs: ["*.sysprop"], property_owner: "Platform", host_supported: true }
sysprop_library { name: "VendProps", srcs: ["V.sysprop"], property_owner: "Vendor", soc_specific: true }
sysprop_library { name: "EmptyProps", property_owner: "Platform" }
cc_library { name: "libplain", srcs: ["x.cpp", "P.sysprop"] }
cc_binary { name: "user", srcs: ["u.cc"], shared_libs: ["libPlatProps"] }
`

func syspropGraph(t *testing.T) *Graph {
	return graphFor(t, map[string]string{
		"p/Android.bp": syspropTree,
		"p/G1.sysprop": "", "p/G2.sysprop": "",
	})
}

func TestSyspropLibrary(t *testing.T) {
	g := syspropGraph(t)
	n := g.node("libPlatProps", Device, Shared, "t")
	if n == nil {
		t.Fatalf("problems %v", g.problems)
	}
	gd := filepath.Join(g.out, "gen", "device", "libPlatProps")
	// genSysprop: the source is Rel() (the path as written below the module dir) + `.cpp` below sysprop/, headers below sysprop/include and
	// sysprop/public/include; the rule's directories are those of the outputs, --include-name the header's path below its include dir
	if len(n.edges) != 2 {
		t.Fatalf("%d edges", len(n.edges))
	}
	for i, rel := range []string{"A.sysprop", "sub/B.sysprop"} {
		e := n.edges[i]
		wantOuts := []string{gd + "/sysprop/" + rel + ".cpp", gd + "/sysprop/include/" + rel + ".h", gd + "/sysprop/public/include/" + rel + ".h"}
		if e.rule != "sysprop" || e.toolVar != "sysprop_cpp" || !reflect.DeepEqual(e.outs, wantOuts) || e.in != g.abs("p/"+rel) {
			t.Errorf("edge %d: %+v", i, e)
		}
		reldir := filepath.Dir(rel)
		if reldir == "." {
			reldir = ""
		}
		want := map[string]string{
			"hdir":    filepath.Join(gd, "sysprop/include", reldir),
			"pdir":    filepath.Join(gd, "sysprop/public/include", reldir),
			"sdir":    filepath.Join(gd, "sysprop", reldir),
			"incname": rel + ".h",
		}
		if got := edgeVars(e); !reflect.DeepEqual(got, want) {
			t.Errorf("edge %d vars %v, want %v", i, got, want)
		}
	}
	if want := []string{gd + "/sysprop/A.sysprop.cpp", gd + "/sysprop/sub/B.sysprop.cpp"}; !reflect.DeepEqual(n.srcs, want) {
		t.Errorf("srcs %v, want %v", n.srcs, want)
	}
	// compiles see sysprop/include (compiler.go) and wait for both header sets (syspropOrderOnlyDeps)
	if !slices.Contains(n.genIncs, "-I"+gd+"/sysprop/include") || len(n.gen) != 4 {
		t.Errorf("incs %v gen %v", n.genIncs, n.gen)
	}
	// target.android: libbase_headers + liblog; cpp.cflags
	if !reflect.DeepEqual(depNames(n.deps.headers), []string{"libbase_headers"}) || !reflect.DeepEqual(depNames(n.deps.shared), []string{"liblog"}) || len(n.deps.static) != 0 {
		t.Errorf("deps headers %v shared %v static %v", depNames(n.deps.headers), depNames(n.deps.shared), depNames(n.deps.static))
	}
	if f := g.compileFlags(n).cxx; !slices.Contains(f, "-DPLAT") {
		t.Errorf("flags %v", f)
	}
	// library.go: the platform variant exports the internal include dir; a dependent includes it and waits for the headers
	if !reflect.DeepEqual(n.exports, []string{"-I" + gd + "/sysprop/include"}) {
		t.Errorf("exports %v", n.exports)
	}
	user := g.node("user", Device, Binary, "t")
	if f := g.compileFlags(user).cxx; !slices.Contains(f, "-I"+gd+"/sysprop/include") || slices.Contains(f, "-I"+gd+"/sysprop/public/include") {
		t.Errorf("user flags %v", f)
	}
	var waits []string
	genClosure(user, map[*Node]bool{}, &waits)
	if !slices.Contains(waits, gd+"/sysprop/include/A.sysprop.h") || !slices.Contains(waits, gd+"/sysprop/public/include/sub/B.sysprop.h") {
		t.Errorf("user waits %v", waits)
	}
	// sysprop_cpp is one host binary
	if tool := g.nodes[key{"sysprop_cpp", Host, Binary}]; tool == nil || n.edges[0].toolNode != tool || n.edges[1].toolNode != tool {
		t.Errorf("tool %v", tool)
	}
}

func TestSyspropLibraryVariants(t *testing.T) {
	g := syspropGraph(t)
	// srcs globs; the host variant of a host_supported library: static libbase + liblog instead of the headers + shared liblog (target.host)
	h := g.node("libGlobProps", Host, Shared, "t")
	if h == nil {
		t.Fatalf("problems %v", g.problems)
	}
	if !reflect.DeepEqual(depNames(h.deps.static), []string{"libbase", "liblog"}) || len(h.deps.shared) != 0 || len(h.deps.headers) != 0 {
		t.Errorf("host deps static %v shared %v headers %v", depNames(h.deps.static), depNames(h.deps.shared), depNames(h.deps.headers))
	}
	gd := filepath.Join(g.out, "gen", "host", "libGlobProps")
	got := slices.Clone(h.srcs)
	slices.Sort(got)
	if want := []string{gd + "/sysprop/G1.sysprop.cpp", gd + "/sysprop/G2.sysprop.cpp"}; !reflect.DeepEqual(got, want) {
		t.Errorf("srcs %v", h.srcs)
	}
	// without host_supported there is no host variant
	if n := g.node("libPlatProps", Host, Shared, "t"); n != nil || !g.hasProblem("libPlatProps: sysprop_library PlatProps has no host variant (host_supported unset)") {
		t.Errorf("host variant of a device-only library: %v", g.problems)
	}
	// the library of a vendor-specific sysprop_library exports the public headers: not built here
	if n := g.node("libVendProps", Device, Shared, "t"); n != nil || !g.hasProblem("libVendProps: sysprop_library VendProps is soc_specific") {
		t.Errorf("vendor library: %v", g.problems)
	}
	if n := g.node("libEmptyProps", Device, Shared, "t"); n != nil || !g.hasProblem("sysprop_library EmptyProps must specify srcs") {
		t.Errorf("empty library: %v", g.problems)
	}
	// every one of these has its sysprop_library in the tree: nothing to fetch
	if len(g.missing) != 0 {
		t.Errorf("missing %v", sortedKeys(g.missing))
	}
}

func TestSyspropMissing(t *testing.T) {
	g := syspropGraph(t)
	// the sysprop_library itself is absent: the library name is what the caller maps to a project
	if g.node("libAbsentProps", Device, Shared, "t") != nil || !g.missing["libAbsentProps"] {
		t.Errorf("missing %v", sortedKeys(g.missing))
	}
	// a plain cc library `lib...` that does not exist is no sysprop either
	if g.node("libnothing", Device, Shared, "t") != nil || !g.missing["libnothing"] {
		t.Errorf("missing %v", sortedKeys(g.missing))
	}
	// the sysprop_library's own name is its java library: no cc dependency
	if g.node("PlatProps", Device, Shared, "t") != nil || !g.hasProblem("PlatProps is a sysprop_library (unsupported)") {
		t.Errorf("problems %v", g.problems)
	}
}

func TestSyspropInPlainCcModule(t *testing.T) {
	g := syspropGraph(t)
	// gen.go: a .sysprop source of any cc module is generated the same way, its dir is on the include path, and a library exports it (library.go)
	n := g.node("libplain", Device, Shared, "t")
	if n == nil {
		t.Fatalf("problems %v", g.problems)
	}
	gd := filepath.Join(g.out, "gen", "device", "libplain")
	if want := []string{"p/x.cpp", gd + "/sysprop/P.sysprop.cpp"}; !reflect.DeepEqual(n.srcs, want) {
		t.Errorf("srcs %v, want %v", n.srcs, want)
	}
	if !reflect.DeepEqual(n.exports, []string{"-I" + gd + "/sysprop/include"}) {
		t.Errorf("exports %v", n.exports)
	}
}

func TestSyspropEmit(t *testing.T) {
	g := syspropGraph(t)
	g.node("libPlatProps", Device, Shared, "t")
	// the same library as an archive too: one set of generator edges
	g.node("libPlatProps", Device, Static, "t")
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	gd := filepath.Join(g.out, "gen", "device", "libPlatProps")
	want := "build " + gd + "/sysprop/sub/B.sysprop.cpp " + gd + "/sysprop/include/sub/B.sysprop.h " + gd + "/sysprop/public/include/sub/B.sysprop.h: sysprop " +
		g.tree + "/p/sub/B.sysprop | " + filepath.Join(g.out, "host", "sysprop_cpp", "sysprop_cpp") + "\n  sysprop_cpp = " + filepath.Join(g.out, "host", "sysprop_cpp", "sysprop_cpp") + "\n"
	if strings.Count(ninja, want) != 1 {
		t.Errorf("%d edges %q in\n%s", strings.Count(ninja, want), want, ninja)
	}
	if !strings.Contains(ninja, "rule sysprop\n  command = $sysprop_cpp --header-dir=$hdir --public-header-dir=$pdir --source-dir=$sdir --include-name=$incname $in\n") {
		t.Errorf("rule missing")
	}
}
