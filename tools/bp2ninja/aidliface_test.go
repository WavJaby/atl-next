// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
)

// Expected names, versions, libraries and aidl flags are derived by hand from system/tools/aidl/build (aidl_interface.go, aidl_interface_backends.go,
// aidl_gen_rule.go), never from the code under test; the comment on each case names the rule it follows.

func TestParseAidlRef(t *testing.T) {
	// aidl_interface.go parseModuleWithVersion: a `-V<digits>` suffix after the backend suffix; backend names are those of the lang* constants
	for ref, want := range map[string][3]string{
		"foo-cpp":                            {"foo", "", "cpp"},
		"foo-V2-ndk":                         {"foo", "2", "ndk"},
		"android.hardware.common.fmq-V1-ndk": {"android.hardware.common.fmq", "1", "ndk"},
		"foo-V12-cpp":                        {"foo", "12", "cpp"},
		"foo-V2-java":                        {"foo", "2", "java"},
		"foo-V2-rust":                        {"foo", "2", "rust"},
		"foo-cpp-analyzer":                   {"foo", "", "cpp-analyzer"},
		"foo-V2-ndk_platform":                {"foo", "2", "ndk_platform"},
		// -V without digits is part of the name
		"foo-Vx-cpp": {"foo-Vx", "", "cpp"},
		"foo-V-cpp":  {"foo-V", "", "cpp"},
	} {
		iface, ver, lang, ok := parseAidlRef(ref)
		if !ok || [3]string{iface, ver, lang} != want {
			t.Errorf("%s: got %q %q %q %v, want %v", ref, iface, ver, lang, ok, want)
		}
	}
	for _, ref := range []string{"foo", "foo-V2", "-cpp", "libfoo.so"} {
		if _, _, _, ok := parseAidlRef(ref); ok {
			t.Errorf("%s: parsed as a backend", ref)
		}
	}
}

// the interfaces: `common` frozen at 1, 2 (next 3); `app` frozen at 1, importing common; `unst` unstable; `vint` unversioned with backend options;
// `frz` frozen: true; `nohash` has a frozen version without .hash
const aidlTree = `
cc_binary_host { name: "aidl", srcs: ["aidl.cc"] }
cc_library { name: "libbinder", srcs: ["b.cc"], export_include_dirs: ["binder/inc"] }
cc_library { name: "libutils", srcs: ["u.cc"], export_include_dirs: ["utils/inc"] }
cc_library { name: "libcutils", srcs: ["c.cc"], export_include_dirs: ["cutils/inc"] }
cc_library { name: "libbinder_ndk", srcs: ["n.cc"], export_include_dirs: ["ndk/inc"] }
cc_library { name: "libextra", srcs: ["e.cc"] }

aidl_interface {
    name: "common",
    host_supported: true,
    local_include_dir: "aidl",
    srcs: ["aidl/com/common/IThing.aidl"],
    versions_with_info: [{ version: "1", imports: [] }, { version: "2", imports: [] }],
}
aidl_interface {
    name: "app",
    local_include_dir: "aidl",
    srcs: ["aidl/com/app/IApp.aidl", "aidl/com/app/Data.aidl"],
    imports: ["common"],
    flags: ["-Werror"],
    include_dirs: ["inc/aidl"],
    versions_with_info: [{ version: "1", imports: ["common-V1"] }],
}
aidl_interface {
    name: "unst",
    unstable: true,
    host_supported: true,
    local_include_dir: "aidl",
    srcs: ["aidl/x/IUnst.aidl"],
    imports: ["common"],
}
aidl_interface {
    name: "vint",
    stability: "vintf",
    min_sdk_version: "29",
    local_include_dir: "aidl",
    srcs: ["aidl/v/IVint.aidl"],
    backend: {
        cpp: { gen_trace: false, gen_log: true, additional_shared_libraries: ["libextra"], min_sdk_version: "31" },
        ndk: { enabled: false },
    },
}
aidl_interface {
    name: "frz",
    frozen: true,
    local_include_dir: "aidl",
    srcs: ["aidl/f/IFrz.aidl"],
    versions_with_info: [{ version: "1", imports: [] }],
}
aidl_interface {
    name: "nohash",
    local_include_dir: "aidl",
    srcs: ["aidl/n/IA.aidl", "aidl/n/IB.aidl"],
    versions_with_info: [{ version: "1", imports: [] }],
}
aidl_interface {
    name: "hdr",
    local_include_dir: "aidl",
    srcs: ["aidl/h/IH.aidl"],
    headers: ["hdrlib"],
}
aidl_library { name: "hdrlib", hdrs: ["inc/x/X.aidl"], strip_import_prefix: "inc", deps: ["hdrdep", "f.aidl"] }
aidl_library { name: "hdrdep", srcs: ["d/D.aidl"] }
aidl_interface {
    name: "nohdr",
    local_include_dir: "aidl",
    srcs: ["aidl/h/IH.aidl"],
    headers: ["nolib", "app"],
}
aidl_interface {
    name: "lost",
    unstable: true,
    local_include_dir: "aidl",
    srcs: ["aidl/l/IL.aidl"],
    imports: ["gone"],
}
cc_binary { name: "user", srcs: ["u.cc"], shared_libs: ["app-V1-cpp", "unst-cpp"] }
`

func aidlGraph(t *testing.T) *Graph {
	files := map[string]string{
		"p/Android.bp": aidlTree,
		// `tail -1`: only the last line is the hash
		"p/aidl_api/common/1/com/common/IThing.aidl": "", "p/aidl_api/common/1/.hash": "junk\nhash-c1\n",
		"p/aidl_api/common/2/com/common/IThing.aidl": "", "p/aidl_api/common/2/.hash": "hash-c2\n",
		"p/aidl_api/app/1/com/app/IApp.aidl": "", "p/aidl_api/app/1/com/app/Data.aidl": "", "p/aidl_api/app/1/.hash": "hash-a1\n",
		"p/aidl_api/frz/1/f/IFrz.aidl": "", "p/aidl_api/frz/1/.hash": "hash-f1\n",
		"p/aidl_api/nohash/1/n/IA.aidl": "", "p/aidl_api/nohash/1/n/IB.aidl": "",
	}
	return graphFor(t, files)
}

func edgeVars(e genEdge) map[string]string {
	vars := map[string]string{}
	for _, v := range e.vars {
		vars[v[0]] = v[1]
	}
	return vars
}

func depNames(list []*Node) []string {
	var out []string
	for _, d := range list {
		out = append(out, d.name)
	}
	return out
}

func (g *Graph) hasProblem(sub string) bool {
	return slices.ContainsFunc(g.problems, func(p string) bool { return strings.Contains(p, sub) })
}

func TestAidlBackendFrozenVersion(t *testing.T) {
	g := aidlGraph(t)
	tree := g.tree
	// aidl_interface_backends.go: app-V1-cpp, built from aidl_api/app/1 (aidlRoot = that dir); the import `common-V1` is as written
	n := g.node("app-V1-cpp", Device, Shared, "t")
	if n == nil {
		t.Fatalf("no node; problems %v", g.problems)
	}
	gd := filepath.Join(g.out, "gen", "device", "app-V1-cpp")
	if len(n.edges) != 2 {
		t.Fatalf("%d edges, want one per .aidl", len(n.edges))
	}
	// srcs are globbed in path order: Data.aidl, IApp.aidl; outputs are named by the path below the root
	if want := []string{gd + "/src/com/app/Data.cpp", gd + "/src/com/app/IApp.cpp"}; !reflect.DeepEqual(n.srcs, want) {
		t.Errorf("srcs %v, want %v", n.srcs, want)
	}
	e := n.edges[1]
	wantOuts := []string{gd + "/src/com/app/IApp.cpp", gd + "/include/com/app/IApp.h", gd + "/include/com/app/BpApp.h", gd + "/include/com/app/BnApp.h"}
	if !reflect.DeepEqual(e.outs, wantOuts) || e.in != tree+"/p/aidl_api/app/1/com/app/IApp.aidl" {
		t.Errorf("edge %+v, want outs %v", e, wantOuts)
	}
	v := edgeVars(e)
	// aidl_gen_rule.go: --structured --version 1 --hash <last line of .hash>, -t (cpp gen_trace default), --min_sdk_version current (default),
	// -I imported roots + include_dirs, -N own roots. A frozen version has no warning flags and, being version 1, no previous version
	wantFlags := "--structured --version 1 --hash hash-a1 -t --min_sdk_version current -I" + tree + "/p/aidl_api/common/1 -I" + tree + "/inc/aidl -N" + tree + "/p/aidl_api/app/1"
	if v["flags"] != wantFlags || v["lang"] != "cpp" {
		t.Errorf("flags %q lang %q, want %q cpp", v["flags"], v["lang"], wantFlags)
	}
	if !reflect.DeepEqual(e.implicit, []string{tree + "/p/aidl_api/app/1/.hash"}) {
		t.Errorf("implicit %v", e.implicit)
	}
	// libbinder, libutils; libcutils because of tracing; then the import's backend (addLanguageLibraries). Exported: all but libcutils
	if want := []string{"libbinder", "libutils", "libcutils", "common-V1-cpp"}; !reflect.DeepEqual(depNames(n.deps.shared), want) {
		t.Errorf("shared deps %v, want %v", depNames(n.deps.shared), want)
	}
	for _, inc := range []string{"-I" + gd + "/include", "-I" + g.abs("p/binder/inc"), "-I" + g.abs("p/utils/inc"), "-I" + filepath.Join(g.out, "gen", "device", "common-V1-cpp", "include")} {
		if !slices.Contains(n.exports, inc) {
			t.Errorf("exports %v lack %s", n.exports, inc)
		}
	}
	if slices.Contains(n.exports, "-I"+g.abs("p/cutils/inc")) {
		t.Errorf("libcutils is not exported: %v", n.exports)
	}
	// the aidl tool is the host binary, built once
	if e.toolNode == nil || e.toolNode != g.nodes[key{"aidl", Host, Binary}] || e.toolVar != "aidl" {
		t.Errorf("tool %+v", e)
	}
	// frozen common-V2 has its own .hash: no --previous_api_dir although version > 1
	c2 := g.node("common-V2-cpp", Device, Shared, "t")
	f := edgeVars(c2.edges[0])["flags"]
	if !strings.Contains(f, "--version 2 --hash hash-c2 ") || strings.Contains(f, "--previous") {
		t.Errorf("common-V2 flags %q", f)
	}
	// the last line of .hash wins
	c1 := edgeVars(g.node("common-V1-cpp", Device, Shared, "t").edges[0])["flags"]
	if !strings.Contains(c1, "--hash hash-c1 ") {
		t.Errorf("common-V1 flags %q", c1)
	}
}

func TestAidlBackendNextVersion(t *testing.T) {
	g := aidlGraph(t)
	tree := g.tree
	// app has frozen version 1: its next version is 2, built from the module's srcs (root local_include_dir) with hash notfrozen
	n := g.node("app-V2-cpp", Device, Shared, "t")
	if n == nil || len(n.edges) != 2 {
		t.Fatalf("node %v, problems %v", n, g.problems)
	}
	// srcs keep the order written
	gd := filepath.Join(g.out, "gen", "device", "app-V2-cpp")
	if want := []string{gd + "/src/com/app/IApp.cpp", gd + "/src/com/app/Data.cpp"}; !reflect.DeepEqual(n.srcs, want) {
		t.Errorf("srcs %v, want %v", n.srcs, want)
	}
	// flagsForAidlGenRule: -Weverything and the interface's flags on the unfrozen version; --previous_api_dir / --previous_hash because the
	// previous version is frozen and release_aidl_use_unfrozen is false; the bare import `common` of the next version means common's next (3)
	wantFlags := "-Weverything -Wno-missing-permission-annotation -Werror --structured --version 2 --hash notfrozen -t --min_sdk_version current" +
		" --previous_api_dir=" + tree + "/p/aidl_api/app/1 --previous_hash hash-a1 -I" + tree + "/p/aidl -I" + tree + "/inc/aidl -N" + tree + "/p/aidl"
	e := n.edges[0]
	if got := edgeVars(e)["flags"]; got != wantFlags {
		t.Errorf("flags %q\nwant  %q", got, wantFlags)
	}
	if !reflect.DeepEqual(e.implicit, []string{tree + "/p/aidl_api/app/1/.hash"}) {
		t.Errorf("implicit %v", e.implicit)
	}
	if want := []string{"libbinder", "libutils", "libcutils", "common-V3-cpp"}; !reflect.DeepEqual(depNames(n.deps.shared), want) {
		t.Errorf("shared deps %v, want %v", depNames(n.deps.shared), want)
	}
	// common-V3 is common's unfrozen version: module srcs, hash notfrozen, previous version 2
	c3 := edgeVars(g.node("common-V3-cpp", Device, Shared, "t").edges[0])["flags"]
	if !strings.Contains(c3, "--version 3 --hash notfrozen ") || !strings.Contains(c3, "--previous_api_dir="+tree+"/p/aidl_api/common/2 --previous_hash hash-c2") {
		t.Errorf("common-V3 flags %q", c3)
	}
}

func TestAidlBackendNdk(t *testing.T) {
	g := aidlGraph(t)
	n := g.node("app-V1-ndk", Device, Shared, "t")
	if n == nil {
		t.Fatalf("problems %v", g.problems)
	}
	gd := filepath.Join(g.out, "gen", "device", "app-V1-ndk")
	e := n.edges[1]
	// ndk headers live below aidl/ (aidl_gen_rule.go prefix); Bp/Bn naming as cpp
	wantOuts := []string{gd + "/src/com/app/IApp.cpp", gd + "/include/aidl/com/app/IApp.h", gd + "/include/aidl/com/app/BpApp.h", gd + "/include/aidl/com/app/BnApp.h"}
	if !reflect.DeepEqual(e.outs, wantOuts) || edgeVars(e)["lang"] != "ndk" {
		t.Errorf("outs %v lang %s, want %v ndk", e.outs, edgeVars(e)["lang"], wantOuts)
	}
	// libbinder_ndk and the import's ndk backend; no libutils / libcutils
	if want := []string{"libbinder_ndk", "common-V1-ndk"}; !reflect.DeepEqual(depNames(n.deps.shared), want) {
		t.Errorf("shared deps %v, want %v", depNames(n.deps.shared), want)
	}
	// target.platform.cflags: -DBINDER_STABILITY_SUPPORT
	if f := g.compileFlags(n).cxx; !slices.Contains(f, "-DBINDER_STABILITY_SUPPORT") {
		t.Errorf("no -DBINDER_STABILITY_SUPPORT in %v", f)
	}
	if f := g.compileFlags(g.node("app-V1-cpp", Device, Shared, "t")).cxx; slices.Contains(f, "-DBINDER_STABILITY_SUPPORT") {
		t.Errorf("cpp backend has -DBINDER_STABILITY_SUPPORT")
	}
	// the generated headers are exported and waited on
	if !slices.Contains(n.exports, "-I"+gd+"/include") {
		t.Errorf("exports %v", n.exports)
	}
	var waits []string
	genClosure(n, map[*Node]bool{}, &waits)
	if !slices.Contains(waits, gd+"/include/aidl/com/app/BnApp.h") {
		t.Errorf("waits %v", waits)
	}
}

func TestAidlBackendUnstable(t *testing.T) {
	g := aidlGraph(t)
	tree := g.tree
	// an unstable interface has one unversioned backend: no --structured / --version / --hash; its version "" is its next version, so the
	// warning flags apply and the bare import `common` means common's next version (3)
	n := g.node("unst-cpp", Device, Shared, "t")
	if n == nil {
		t.Fatalf("problems %v", g.problems)
	}
	wantFlags := "-Weverything -Wno-missing-permission-annotation -t --min_sdk_version current -I" + tree + "/p/aidl -N" + tree + "/p/aidl"
	if got := edgeVars(n.edges[0])["flags"]; got != wantFlags {
		t.Errorf("flags %q, want %q", got, wantFlags)
	}
	if want := []string{"libbinder", "libutils", "libcutils", "common-V3-cpp"}; !reflect.DeepEqual(depNames(n.deps.shared), want) {
		t.Errorf("shared deps %v, want %v", depNames(n.deps.shared), want)
	}
	gd := filepath.Join(g.out, "gen", "device", "unst-cpp")
	if want := []string{gd + "/src/x/IUnst.cpp"}; !reflect.DeepEqual(n.srcs, want) {
		t.Errorf("srcs %v, want %v", n.srcs, want)
	}
	// host_supported: the host variant exists, and imports host_supported common
	if h := g.node("unst-ndk", Host, Shared, "t"); h == nil || h.side != Host {
		t.Errorf("host variant: %v, problems %v", h, g.problems)
	}
	// the other direction: an unstable interface imported by a stable one is bare (getImportWithVersion), checked through app's closure
	if len(g.node("user", Device, Binary, "t").deps.shared) != 2 {
		t.Errorf("user: %v", g.problems)
	}
}

func TestAidlBackendOptions(t *testing.T) {
	g := aidlGraph(t)
	// vint: no versions -> next version 1, `-V1`; backend.cpp.min_sdk_version over the interface's; gen_trace false (no -t, no libcutils);
	// gen_log -> --log; additional_shared_libraries after libbinder / libutils and not exported; stability -> --stability
	n := g.node("vint-V1-cpp", Device, Shared, "t")
	if n == nil {
		t.Fatalf("problems %v", g.problems)
	}
	wantFlags := "-Weverything -Wno-missing-permission-annotation --structured --version 1 --hash notfrozen --stability vintf --min_sdk_version 31 --log -N" + g.tree + "/p/aidl"
	if got := edgeVars(n.edges[0])["flags"]; got != wantFlags {
		t.Errorf("flags %q, want %q", got, wantFlags)
	}
	if want := []string{"libbinder", "libutils", "libextra"}; !reflect.DeepEqual(depNames(n.deps.shared), want) {
		t.Errorf("shared deps %v, want %v", depNames(n.deps.shared), want)
	}
	if slices.Contains(n.exports, "-I"+g.abs("p/cutils/inc")) {
		t.Errorf("exports %v", n.exports)
	}
	// ndk backend disabled: no module, one problem, nothing to fetch
	if m := g.lookup("vint-V1-ndk", Device); m != nil || !g.hasProblem("vint-V1-ndk: backend.ndk.enabled is false") {
		t.Errorf("disabled ndk backend: %v, problems %v", m, g.problems)
	}
}

func TestAidlBackendRefusals(t *testing.T) {
	g := aidlGraph(t)
	for ref, want := range map[string]string{
		// backends other than cpp / ndk are not built
		"app-V1-java":         "app-V1-java: aidl_interface backend java is unsupported",
		"app-V2-rust":         "app-V2-rust: aidl_interface backend rust is unsupported",
		"app-cpp-analyzer":    "app-cpp-analyzer: aidl_interface backend cpp-analyzer is unsupported",
		"app-V1-ndk_platform": "app-V1-ndk_platform: aidl_interface backend ndk_platform is unsupported",
		// addLibrary names: a stable interface has only versioned backends, an unstable one only the unversioned
		"app-cpp":     "app-cpp: app is not unstable",
		"unst-V1-cpp": `unst-V1-cpp: unst has no version "1"`,
		"app-V3-cpp":  `app-V3-cpp: app has no version "3"`,
		// a host variant needs host_supported
		"app-V1-cpp@host": "app-V1-cpp: aidl_interface app has no host variant",
		// frozen: true makes the next version unusable (checkRequireFrozenAndReason), the frozen ones stay
		"frz-V2-cpp": "frz-V2-cpp: version 2 of frz is unfrozen but the interface is `frozen: true`",
	} {
		side := Device
		if r, ok := strings.CutSuffix(ref, "@host"); ok {
			ref, side = r, Host
		}
		if n := g.node(ref, side, Shared, "t"); n != nil {
			t.Errorf("%s: built", ref)
		}
		if !g.hasProblem(want) {
			t.Errorf("%s: problems %v lack %q", ref, g.problems, want)
		}
		// the interface exists: none of these is a module to fetch, and "module not found" would repeat the reason
		if g.hasProblem(ref + " not found") {
			t.Errorf("%s: reported as not found", ref)
		}
	}
	if g.node("frz-V1-cpp", Device, Shared, "t") == nil {
		t.Errorf("frozen version of a frozen: true interface refused: %v", g.problems)
	}
	if len(g.missing) != 0 {
		t.Errorf("missing %v", sortedKeys(g.missing))
	}
}

func TestAidlBackendHeaders(t *testing.T) {
	g := aidlGraph(t)
	// aidl_library.go: include dir = module dir + strip_import_prefix, then the dirs of its aidl_library deps (a dep naming an .aidl file adds
	// none); aidl_api.go getDeps puts them in -I after include_dirs
	n := g.node("hdr-V1-cpp", Device, Shared, "t")
	if n == nil {
		t.Fatalf("problems %v", g.problems)
	}
	wantFlags := "-Weverything -Wno-missing-permission-annotation --structured --version 1 --hash notfrozen -t --min_sdk_version current -I" + g.tree + "/p/inc -I" + g.tree + "/p -N" + g.tree + "/p/aidl"
	if got := edgeVars(n.edges[0])["flags"]; got != wantFlags {
		t.Errorf("flags %q, want %q", got, wantFlags)
	}
	// a header library that is absent is a module to fetch; one of another type is a problem
	if g.node("nohdr-V1-cpp", Device, Shared, "t") == nil || !g.missing["nolib"] || !g.hasProblem("nohdr-V1-cpp: aidl_library nolib not found") || !g.hasProblem("app is a aidl_interface, not an aidl_library") {
		t.Errorf("missing %v problems %v", sortedKeys(g.missing), g.problems)
	}
}

func TestAidlBackendMissingInterface(t *testing.T) {
	g := aidlGraph(t)
	// the interface itself is absent: the backend name is what the caller maps to a project
	if g.node("nothere-V1-cpp", Device, Shared, "t") != nil || !g.missing["nothere-V1-cpp"] {
		t.Errorf("missing %v", sortedKeys(g.missing))
	}
	// an import of an absent interface: the interface is the missing module, the backend is built without that library
	n := g.node("lost-cpp", Device, Shared, "t")
	if n == nil || !g.missing["gone"] || !g.hasProblem("aidl_interface import gone not found") {
		t.Errorf("node %v missing %v problems %v", n, sortedKeys(g.missing), g.problems)
	}
	if want := []string{"libbinder", "libutils", "libcutils"}; n != nil && !reflect.DeepEqual(depNames(n.deps.shared), want) {
		t.Errorf("shared deps %v", depNames(n.deps.shared))
	}
}

func TestAidlBackendMissingPreviousHash(t *testing.T) {
	g := aidlGraph(t)
	// version 2 of nohash has a previous version 1 without a .hash (aidl_gen_rule.go: ModuleErrorf): one problem for two sources, the flags
	// stay out because aidl rejects --previous_api_dir without --previous_hash
	n := g.node("nohash-V2-cpp", Device, Shared, "t")
	if n == nil || len(n.edges) != 2 {
		t.Fatalf("node %v problems %v", n, g.problems)
	}
	for _, e := range n.edges {
		if f := edgeVars(e)["flags"]; strings.Contains(f, "--previous") {
			t.Errorf("flags %q", f)
		}
	}
	count := 0
	for _, p := range g.problems {
		if strings.Contains(p, "no .hash of the previous version") {
			count++
		}
	}
	if count != 1 {
		t.Errorf("%d problems: %v", count, g.problems)
	}
}

func TestAidlBackendEmit(t *testing.T) {
	g := aidlGraph(t)
	g.node("app-V1-ndk", Device, Shared, "t")
	// the same library as a static archive too (cc_library): one node each, one generator edge
	g.node("app-V1-ndk", Device, Static, "t")
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	// one aidl edge per source: depfile var, lang, the hash as an implicit input after the tool
	gd := filepath.Join(g.out, "gen", "device", "app-V1-ndk")
	want := "build " + gd + "/src/com/app/IApp.cpp " + gd + "/include/aidl/com/app/IApp.h " + gd + "/include/aidl/com/app/BpApp.h " + gd +
		"/include/aidl/com/app/BnApp.h: aidl " + g.tree + "/p/aidl_api/app/1/com/app/IApp.aidl | " + filepath.Join(g.out, "host", "aidl", "aidl") + " " + g.tree + "/p/aidl_api/app/1/.hash\n"
	if strings.Count(ninja, want) != 1 {
		t.Errorf("%d edges %q in\n%s", strings.Count(ninja, want), want, ninja)
	}
	if !strings.Contains(ninja, "  dep = "+gd+"/src/com/app/IApp.cpp.d\n") || !strings.Contains(ninja, "rule aidl\n  command = $aidl --lang=$lang $flags --ninja -d $dep") {
		t.Errorf("rule or depfile variable missing")
	}
}

func TestAidlHashFileRead(t *testing.T) {
	g := &Graph{tree: t.TempDir()}
	write := func(name, body string) {
		if err := os.WriteFile(filepath.Join(g.tree, name), []byte(body), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	// `tail -1`
	write("a", "x\ny\n")
	write("b", "only")
	write("c", "")
	for name, want := range map[string]string{"a": "y", "b": "only", "c": ""} {
		if got, ok := g.readHash(name); !ok || got != want {
			t.Errorf("%s: %q %v, want %q", name, got, ok, want)
		}
	}
	if _, ok := g.readHash("none"); ok {
		t.Errorf("absent file read")
	}
}
