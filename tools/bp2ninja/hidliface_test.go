// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
)

// Expected names, outputs, options and libraries are derived by hand from system/tools/hidl/build (hidl_interface.go hidlInterfaceMutator and
// hidlGenRule, fqName.go, hidl_package_root.go), never from the code under test; the comment on each case names the rule it follows.

func TestParseHidlRef(t *testing.T) {
	// fqName.go: sourcesName `<fq>_genc++`, headersName `<fq>_genc++_headers`, fileGroupName `<fq>_hal`, javaName `<pkg>-V<maj>.<min>-java` (sanitizedString),
	// javaSharedName `-java-shallow`, javaSourcesName `-java_gen_java`, javaConstantsName `-java-constants`, lintName, inheritanceHierarchyName
	for ref, want := range map[string][2]string{
		"android.hidl.base@1.0":                          {"android.hidl.base@1.0", hidlLib},
		"android.hidl.base@1.0_genc++":                   {"android.hidl.base@1.0", hidlSources},
		"android.hidl.base@1.0_genc++_headers":           {"android.hidl.base@1.0", hidlHeaders},
		":android.hidl.base@1.0_hal":                     {"android.hidl.base@1.0", hidlHal},
		"android.hidl.base-V1.0-java":                    {"android.hidl.base@1.0", hidlOther},
		"android.hidl.base-V1.0-java-shallow":            {"android.hidl.base@1.0", hidlOther},
		"android.hidl.base-V1.0-java_gen_java":           {"android.hidl.base@1.0", hidlOther},
		"android.hidl.base-V1.0-java-constants":          {"android.hidl.base@1.0", hidlOther},
		"android.hidl.base-V1.0-java-constants_gen_java": {"android.hidl.base@1.0", hidlOther},
		"android.hidl.base@1.0-hidl-lint":                {"android.hidl.base@1.0", hidlOther},
		"android.hidl.base@1.0-inheritance-hierarchy":    {"android.hidl.base@1.0", hidlOther},
	} {
		fq, kind, ok := parseHidlRef(ref)
		if !ok || [2]string{fq, kind} != want {
			t.Errorf("%s: got %q %q %v, want %v", ref, fq, kind, ok, want)
		}
	}
	// the version is `<digits>.<digits>`; a package is dotted identifiers
	for _, ref := range []string{"libhidlbase", "foo@1_genc++", "foo@1.0.2", "foo@x.0_hal", "@1.0", "1foo@1.0", "foo-V1-java"} {
		if _, _, ok := parseHidlRef(ref); ok {
			t.Errorf("%s: parsed", ref)
		}
	}
}

func TestHidlFqName(t *testing.T) {
	fq, ok := parseHidlFq("android.hardware.foo.bar@2.13")
	if !ok {
		t.Fatal("not parsed")
	}
	// fqName.go: dir = pkg components joined, then `<maj>.<min>/`; inPackage = a prefix of whole components
	if fq.dir() != "android/hardware/foo/bar/2.13/" || fq.String() != "android.hardware.foo.bar@2.13" {
		t.Errorf("%q %q", fq.dir(), fq.String())
	}
	for root, want := range map[string]bool{"android": true, "android.hardware": true, "android.hardware.foo.bar": true, "android.hard": false, "vendor": false, "android.hardware.foo.bar.baz": false} {
		if got := fq.inPackage(root); got != want {
			t.Errorf("inPackage(%s) = %v", root, got)
		}
	}
	// properties.go coreDependencyPackageNames
	for name, want := range map[string]bool{"android.hidl.base@1.0": true, "android.hidl.manager@1.2": true, "android.hidl.memory@1.0": false, "android.hardware.foo@1.0": false} {
		if f, _ := parseHidlFq(name); f.core() != want {
			t.Errorf("%s core = %v", name, f.core())
		}
	}
}

// base and manager are core packages; foo@1.0 has types.hal + two interfaces; foo@1.1 imports across roots; dep is in a frozen root; the rest are broken.
const hidlTree = `
cc_binary_host { name: "hidl-gen", srcs: ["main.cpp"] }
cc_defaults { name: "hidl-module-defaults", cflags: ["-Wall"] }
cc_library { name: "libhidlbase", srcs: ["hb.cc"], export_include_dirs: ["hidlbase/inc"] }
cc_library { name: "liblog", srcs: ["l.cc"], export_include_dirs: ["log/inc"] }
cc_library { name: "libutils", srcs: ["u.cc"], export_include_dirs: ["utils/inc"] }
cc_library { name: "libcutils", srcs: ["c.cc"], export_include_dirs: ["cutils/inc"] }
cc_library {
    name: "libcore",
    generated_sources: ["android.hidl.base@1.0_genc++"],
    generated_headers: ["android.hidl.base@1.0_genc++_headers"],
    export_generated_headers: ["android.hidl.base@1.0_genc++_headers"],
}

`

// every hidl_interface sits in its own directory with plain file names in srcs, as in AOSP (hardware/interfaces/foo/1.0). The package roots too (a
// root's path defaults to its directory, its current.txt is there): android.hidl plain; vendor.x with current.txt,
// use_current and require_frozen; vendor.z with an explicit path; bad.cur with use_current but no current.txt
func hidlGraph(t *testing.T) *Graph {
	return graphFor(t, map[string]string{
		"p/Android.bp":    hidlTree,
		"hw/Android.bp":   `hidl_package_root { name: "android.hidl" }`,
		"vx/Android.bp":   `hidl_package_root { name: "vendor.x", use_current: true, require_frozen: true }`,
		"vx/current.txt":  "hash\n",
		"vz/Android.bp":   `hidl_package_root { name: "vendor.z", path: "some/where" }`,
		"nc/Android.bp":   `hidl_package_root { name: "bad.cur", use_current: true }`,
		"base/Android.bp": `hidl_interface { name: "android.hidl.base@1.0", root: "android.hidl", srcs: ["types.hal", "IBase.hal"] }`,
		"mgr/Android.bp":  `hidl_interface { name: "android.hidl.manager@1.0", root: "android.hidl", srcs: ["IServiceManager.hal"], interfaces: ["android.hidl.base@1.0"] }`,
		"foo10/Android.bp": `hidl_interface {
    name: "android.hidl.foo@1.0",
    root: "android.hidl",
    srcs: ["types.hal", "IFoo.hal", "IBar.hal"],
    interfaces: ["android.hidl.base@1.0"],
}`,
		"foo11/Android.bp": `hidl_interface {
    name: "android.hidl.foo@1.1",
    root: "android.hidl",
    srcs: ["IFoo.hal"],
    interfaces: ["android.hidl.base@1.0", "android.hidl.foo@1.0", "vendor.x.dep@1.0", "vendor.z.w@1.0", "android.hidl.manager@1.0"],
}`,
		"dep/Android.bp": `hidl_interface { name: "vendor.x.dep@1.0", root: "vendor.x", srcs: ["IDep.hal"] }`,
		"w/Android.bp":   `hidl_interface { name: "vendor.z.w@1.0", root: "vendor.z", srcs: ["IW.hal"] }`,
		// the broken ones: an interfaces entry the tree lacks; a root without module (a prefix of the name) / not a prefix; a source that is no .hal;
		// no sources; a name without version; an interfaces entry without version; use_current without current.txt
		"bad/Android.bp": `
hidl_interface { name: "android.hidl.lost@1.0", root: "android.hidl", srcs: ["IL.hal"], interfaces: ["android.hidl.gone@1.0"] }
hidl_interface { name: "android.hidl.noroot@1.0", root: "android.hidl.noroot", srcs: ["IN.hal"] }
hidl_interface { name: "android.hidl.wrongroot@1.0", root: "vendor.y", srcs: ["IN.hal"] }
hidl_interface { name: "android.hidl.nonhal@1.0", root: "android.hidl", srcs: ["x.txt"] }
hidl_interface { name: "android.hidl.nosrc@1.0", root: "android.hidl" }
hidl_interface { name: "android.hidl.badfq", root: "android.hidl", srcs: ["IB.hal"] }
hidl_interface { name: "android.hidl.baddep@1.0", root: "android.hidl", srcs: ["IB.hal"], interfaces: ["nonsense"] }
hidl_interface { name: "bad.cur.x@1.0", root: "bad.cur", srcs: ["IB.hal"] }
`,
		"base/types.hal": "", "base/IBase.hal": "", "mgr/IServiceManager.hal": "",
		"foo10/types.hal": "", "foo10/IFoo.hal": "", "foo10/IBar.hal": "", "foo11/IFoo.hal": "",
		"dep/IDep.hal": "", "w/IW.hal": "",
	})
}

func hidlRuleOf(t *testing.T, g *Graph, ref string, side Side) *genRule {
	t.Helper()
	n := g.genNode(ref, side, "t")
	if n == nil {
		t.Fatalf("%s: no node; problems %v", ref, g.problems)
	}
	return n.rule
}

func TestHidlGenSources(t *testing.T) {
	g := hidlGraph(t)
	r := hidlRuleOf(t, g, "android.hidl.foo@1.0_genc++", Device)
	gd := filepath.Join(g.out, "gen", "device", "android.hidl.foo@1.0_genc++")
	// hidlInterfaceMutator: Outputs = <dir>+<X>All.cpp per I<X>.hal (srcs order), then <dir>+<name>.cpp per other .hal
	var want []string
	for _, f := range []string{"FooAll.cpp", "BarAll.cpp", "types.cpp"} {
		want = append(want, gd+"/android/hidl/foo/1.0/"+f)
	}
	if !reflect.DeepEqual(r.outs, want) {
		t.Errorf("outs %v, want %v", r.outs, want)
	}
	// hidlRule: `${hidl} -R -p . -o ${genDir} -L ${language} ${options} ${fqName}`; options = the root of the interface and of its `interfaces` (-r<root>:<path>,
	// the hidl_package_root's path), once each; `-d ${out}.d` and `rm -rf ${genDir}` are dropped (hidliface.go)
	wantCmd := "$(location hidl-gen) -R -p . -o $(genDir) -L c++-sources '-randroid.hidl:hw' 'android.hidl.foo@1.0'"
	if r.cmd != wantCmd {
		t.Errorf("cmd %q, want %q", r.cmd, wantCmd)
	}
	// inputs = the .hal files (GenerateAndroidBuildActions genInputs), tool = hidl-gen
	if wantIn := []string{"foo10/types.hal", "foo10/IFoo.hal", "foo10/IBar.hal"}; !reflect.DeepEqual(r.inputs, wantIn) {
		t.Errorf("inputs %v", r.inputs)
	}
	if len(r.tools) != 1 || r.tools[0].name != "hidl-gen" || r.tools[0].side != Host || r.broken != "" {
		t.Errorf("tools %v broken %q", r.tools, r.broken)
	}
	// GeneratedHeaderDirs = the gen dir
	if !reflect.DeepEqual(r.incs, []string{"-I" + gd}) {
		t.Errorf("incs %v", r.incs)
	}
	// the manifest of the sources; a core package gets them too (libhidlbase builds them)
	if base := hidlRuleOf(t, g, "android.hidl.base@1.0_genc++", Device); len(base.outs) != 2 ||
		!strings.HasSuffix(base.outs[0], "/android/hidl/base/1.0/BaseAll.cpp") || !strings.HasSuffix(base.outs[1], "/android/hidl/base/1.0/types.cpp") {
		t.Errorf("base outs %v", base.outs)
	}
}

func TestHidlGenHeaders(t *testing.T) {
	g := hidlGraph(t)
	r := hidlRuleOf(t, g, "android.hidl.foo@1.0_genc++_headers", Device)
	gd := filepath.Join(g.out, "gen", "device", "android.hidl.foo@1.0_genc++_headers")
	// Outputs = I<X>, Bs<X>, BnHw<X>, BpHw<X>, IHw<X> for each interface in turn, then <name>.h and hw<name>.h for types
	var want []string
	for _, f := range []string{"IFoo", "IBar", "BsFoo", "BsBar", "BnHwFoo", "BnHwBar", "BpHwFoo", "BpHwBar", "IHwFoo", "IHwBar", "types", "hwtypes"} {
		want = append(want, gd+"/android/hidl/foo/1.0/"+f+".h")
	}
	if !reflect.DeepEqual(r.outs, want) {
		t.Errorf("outs %v, want %v", r.outs, want)
	}
	if !strings.Contains(r.cmd, " -L c++-headers ") {
		t.Errorf("cmd %q", r.cmd)
	}
	// more outputs than genOutLimit: consumers wait for one phony (genrule.go)
	if r.phony != gd+"/genrule-phony" || !reflect.DeepEqual(r.deps, []string{r.phony}) {
		t.Errorf("phony %q deps %v", r.phony, r.deps)
	}
}

func TestHidlGenOptions(t *testing.T) {
	g := hidlGraph(t)
	// foo@1.1 depends on interfaces of three roots (android.hidl several times, once through a core package): the -r option of each root, in the order
	// the interfaces come (getFullPackageRoot = -r<name>:<path>, path defaulting to the root module's dir, here hw and vx, or as written), unique.
	// The own root android.hidl has no require_frozen, so no -F
	r := hidlRuleOf(t, g, "android.hidl.foo@1.1_genc++", Device)
	if want := "$(location hidl-gen) -R -p . -o $(genDir) -L c++-sources '-randroid.hidl:hw' '-rvendor.x:vx' '-rvendor.z:some/where' 'android.hidl.foo@1.1'"; r.cmd != want {
		t.Errorf("cmd %q, want %q", r.cmd, want)
	}
	if len(r.implicit) != 0 {
		t.Errorf("implicit %v", r.implicit)
	}
	// vendor.x: require_frozen -> -F; its current.txt (use_current) an input beside the .hal files (hidlGenRule: inputs + currentPath)
	r = hidlRuleOf(t, g, "vendor.x.dep@1.0_genc++", Device)
	if want := "$(location hidl-gen) -R -p . -o $(genDir) -L c++-sources '-rvendor.x:vx' -F 'vendor.x.dep@1.0'"; r.cmd != want {
		t.Errorf("cmd %q, want %q", r.cmd, want)
	}
	if !reflect.DeepEqual(r.implicit, []string{"vx/current.txt"}) {
		t.Errorf("implicit %v", r.implicit)
	}
}

func TestHidlLibrary(t *testing.T) {
	g := hidlGraph(t)
	n := g.node("android.hidl.foo@1.1", Device, Shared, "t")
	if n == nil {
		t.Fatalf("problems %v", g.problems)
	}
	// hidlInterfaceMutator: shared_libs = the interfaces without core packages (libhidlbase has those) + libhidlbase, liblog, libutils, libcutils
	if want := []string{"android.hidl.foo@1.0", "vendor.x.dep@1.0", "vendor.z.w@1.0", "libhidlbase", "liblog", "libutils", "libcutils"}; !reflect.DeepEqual(depNames(n.deps.shared), want) {
		t.Errorf("shared deps %v, want %v", depNames(n.deps.shared), want)
	}
	// compiled: the generated sources; they wait for the generated headers (Generated_headers) and see the include dirs of both genrules
	hd := filepath.Join(g.out, "gen", "device", "android.hidl.foo@1.1_genc++_headers")
	sd := filepath.Join(g.out, "gen", "device", "android.hidl.foo@1.1_genc++")
	if want := []string{sd + "/android/hidl/foo/1.1/FooAll.cpp"}; !reflect.DeepEqual(n.srcs, want) {
		t.Errorf("srcs %v, want %v", n.srcs, want)
	}
	// (5 headers: not more than genOutLimit, so no phony)
	if !slices.Contains(n.gen, hd+"/android/hidl/foo/1.1/IFoo.h") || !slices.Contains(n.gen, sd+"/android/hidl/foo/1.1/FooAll.cpp") {
		t.Errorf("gen %v", n.gen)
	}
	// exports: its own generated headers, then export_shared_lib_headers = the interfaces, libhidlbase, libutils (not liblog / libcutils)
	exp := n.exports
	for _, want := range []string{"-I" + hd, "-I" + g.abs("p/hidlbase/inc"), "-I" + g.abs("p/utils/inc"),
		"-I" + filepath.Join(g.out, "gen", "device", "android.hidl.foo@1.0_genc++_headers")} {
		if !slices.Contains(exp, want) {
			t.Errorf("exports %v lack %s", exp, want)
		}
	}
	for _, not := range []string{"-I" + g.abs("p/log/inc"), "-I" + g.abs("p/cutils/inc"), "-I" + sd} {
		if slices.Contains(exp, not) {
			t.Errorf("exports %v hold %s", exp, not)
		}
	}
	// hidl-module-defaults come with it
	if !slices.Contains(g.compileFlags(n).cxx, "-Wall") {
		t.Errorf("defaults not applied")
	}
	// host_supported: the host variant too, with the host hidl-gen
	if h := g.node("android.hidl.foo@1.0", Host, Shared, "t"); h == nil || len(h.deps.shared) != 4 {
		t.Errorf("host variant %v", h)
	}
	// the static archive of the same cc_library
	if s := g.node("android.hidl.foo@1.0", Device, Static, "t"); s == nil || s == g.node("android.hidl.foo@1.0", Device, Shared, "t") {
		t.Errorf("static %v", s)
	}
}

func TestHidlCorePackages(t *testing.T) {
	g := hidlGraph(t)
	// a core package is generated (libhidlbase's Android.bp consumes the genrules by name) but has no library
	n := g.node("libcore", Device, Shared, "t")
	if n == nil {
		t.Fatalf("problems %v", g.problems)
	}
	var names []string
	for _, s := range n.srcs {
		names = append(names, strings.TrimPrefix(s, filepath.Join(g.out, "gen", "device", "android.hidl.base@1.0_genc++")))
	}
	if want := []string{"/android/hidl/base/1.0/BaseAll.cpp", "/android/hidl/base/1.0/types.cpp"}; !reflect.DeepEqual(names, want) {
		t.Errorf("srcs %v, want %v", names, want)
	}
	if len(n.genExports) != 1 || n.genExports[0] != "-I"+filepath.Join(g.out, "gen", "device", "android.hidl.base@1.0_genc++_headers") {
		t.Errorf("exports %v", n.genExports)
	}
	if g.node("android.hidl.manager@1.0", Device, Shared, "t") != nil || !g.hasProblem("a core HIDL package has no library") {
		t.Errorf("library of a core package: %v", g.problems)
	}
	if len(g.missing) != 0 {
		t.Errorf("missing %v", sortedKeys(g.missing))
	}
}

func TestHidlFilegroup(t *testing.T) {
	g := hidlGraph(t)
	// fileGroupName `<fq>_hal`: a filegroup of srcs, usable as `:<fq>_hal`
	files, ok := g.resolveSrc("q", Device, ":android.hidl.foo@1.0_hal", "t")
	var paths []string
	for _, f := range files {
		paths = append(paths, f.path)
	}
	if want := []string{"foo10/types.hal", "foo10/IFoo.hal", "foo10/IBar.hal"}; !ok || !reflect.DeepEqual(paths, want) {
		t.Errorf("files %v ok %v problems %v", paths, ok, g.problems)
	}
}

func TestHidlRefusals(t *testing.T) {
	g := hidlGraph(t)
	for ref, want := range map[string]string{
		// other modules the load hook creates are not built
		"android.hidl.foo-V1.0-java":                 "only the C++ modules of hidl_interface android.hidl.foo@1.0 are built",
		"android.hidl.foo@1.0-hidl-lint":             "only the C++ modules of hidl_interface android.hidl.foo@1.0 are built",
		"android.hidl.foo@1.0-inheritance-hierarchy": "only the C++ modules of hidl_interface android.hidl.foo@1.0 are built",
		// hidlInterfaceMutator's PropertyErrorf cases
		"android.hidl.noroot@1.0":    "android.hidl.noroot@1.0: no hidl_package_root android.hidl.noroot",
		"android.hidl.wrongroot@1.0": `android.hidl.wrongroot@1.0: root "vendor.y" must be a prefix of the name`,
		"android.hidl.nonhal@1.0":    "android.hidl.nonhal@1.0: srcs: source must be a .hal file: x.txt",
		"android.hidl.nosrc@1.0":     "android.hidl.nosrc@1.0: no sources provided",
		"android.hidl.badfq":         "android.hidl.badfq: poorly formed hal interface name",
		"android.hidl.baddep@1.0":    "android.hidl.baddep@1.0: interfaces: poorly formed hal interface name nonsense",
		// hidl_package_root: use_current requires current.txt
		"bad.cur.x@1.0": "bad.cur.x@1.0: hidl_package_root bad.cur: use_current but nc/current.txt does not exist",
	} {
		if n := g.node(ref, Device, Shared, "t"); n != nil {
			t.Errorf("%s: built", ref)
		}
		if !g.hasProblem(want) {
			t.Errorf("%s: problems %v lack %q", ref, g.problems, want)
		}
	}
	// every interface here is defined, so none of its modules is one to fetch; only the root without a module is
	if want := []string{"android.hidl.noroot"}; !reflect.DeepEqual(sortedKeys(g.missing), want) {
		t.Errorf("missing %v, want %v", sortedKeys(g.missing), want)
	}
}

func TestHidlMissing(t *testing.T) {
	g := hidlGraph(t)
	// an interface the tree lacks: its generated modules are what the caller maps to a project
	for _, ref := range []string{"android.hidl.zzz@1.0_genc++", "android.hidl.zzz@1.0_genc++_headers"} {
		if g.genNode(ref, Device, "t") != nil || !g.missing[ref] {
			t.Errorf("%s: missing %v", ref, sortedKeys(g.missing))
		}
	}
	if g.node("android.hidl.zzz@1.0", Device, Shared, "t") != nil || !g.missing["android.hidl.zzz@1.0"] {
		t.Errorf("missing %v", sortedKeys(g.missing))
	}
	// an `interfaces` entry the tree lacks: that interface is the missing module, the genrule of this one is built without its root
	r := hidlRuleOf(t, g, "android.hidl.lost@1.0_genc++", Device)
	if !g.missing["android.hidl.gone@1.0"] || g.missing["android.hidl.lost@1.0_genc++"] || !g.hasProblem("hidl_interface android.hidl.gone@1.0 not found") {
		t.Errorf("missing %v problems %v", sortedKeys(g.missing), g.problems)
	}
	if want := "$(location hidl-gen) -R -p . -o $(genDir) -L c++-sources '-randroid.hidl:hw' 'android.hidl.lost@1.0'"; r.cmd != want {
		t.Errorf("cmd %q", r.cmd)
	}
	// an interface that is there but unusable: its generated modules are a problem each time they are asked for, never a module to fetch
	if g.genNode("android.hidl.nosrc@1.0_genc++", Device, "t") != nil || !g.synthOwned("android.hidl.nosrc@1.0_genc++") || g.missing["android.hidl.nosrc@1.0_genc++"] {
		t.Errorf("missing %v", sortedKeys(g.missing))
	}
}

func TestHidlEmit(t *testing.T) {
	g := hidlGraph(t)
	g.node("android.hidl.foo@1.0", Device, Shared, "t")
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	gd := filepath.Join(g.out, "gen", "device", "android.hidl.foo@1.0_genc++")
	// one genrule edge for the sources, ordered after the host hidl-gen; its script holds the command with the tool's path and the gen dir
	if !strings.Contains(ninja, "build "+esc(gd+"/android/hidl/foo/1.0/FooAll.cpp")+" ") || !strings.Contains(ninja, "script = "+esc(gd+"/genrule.sh")) {
		t.Errorf("genrule edge missing in\n%s", ninja)
	}
	// (the gen dir holds `@` and `+`, which proptools.ShellEscape quotes)
	want := filepath.Join(g.out, "host", "hidl-gen", "hidl-gen") + " -R -p . -o '" + gd + "' -L c++-sources '-randroid.hidl:hw' 'android.hidl.foo@1.0'"
	if got := g.genFiles[gd+"/genrule.sh"]; !strings.HasPrefix(got, want+"\n") {
		t.Errorf("script %q, want prefix %q", got, want)
	}
}
