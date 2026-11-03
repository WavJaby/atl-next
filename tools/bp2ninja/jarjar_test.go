// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

// Expected values are derived from Soong at the pinned tag, never from the code under test: java/base.go jarjarIfNecessary,
// repackageFlagsIfNecessary, collectJarJarRules / collectDirectDepsProviders, getJarJarRuleText; builder.go TransformJarJarWithShards;
// aconfig/codegen java_aconfig_library.go (exportable declarations add five rename rules), aconfig_declarations_group.go (.srcjars);
// frameworks/base/api/api.go createMergedFrameworkModuleLibStubs; android/filegroup.go; Soong's own repackaging.txt for
// framework-minus-apex (rule lines "rule <class> com.android.internal.hidden_from_bootclasspath.<class>", sorted).

const jarjarTree = `
aconfig_declarations { name: "decl", package: "com.x.flags", container: "system", srcs: ["f.aconfig"], exportable: true }
java_aconfig_library { name: "flags-java", aconfig_declarations: "decl", sdk_version: "none", system_modules: "none" }
aconfig_declarations_group { name: "grp", java_aconfig_libraries: ["flags-java"], aconfig_declarations_groups: ["decl"] }
java_library {
    name: "fw",
    srcs: ["A.java", ":grp{.srcjars}"],
    sdk_version: "none",
    system_modules: "none",
    jarjar_rules: ":rules",
    jarjar_shards: "3",
    jarjar_prefix: "com.android.internal.hidden_from_bootclasspath",
}
filegroup { name: "rules", srcs: ["rules.txt"] }
java_binary_host { name: "jarjar", srcs: ["J.java"] }
`

func jarjarGraph(t *testing.T) *Graph {
	g := javaTestGraph(t, map[string]string{"j/Android.bp": jarjarTree, "j/A.java": "", "j/J.java": "", "j/rules.txt": "", "j/f.aconfig": ""},
		map[string]string{"RELEASE_READ_FROM_NEW_STORAGE": "", "RELEASE_ACONFIG_NEW_EXPORTED": "", "RELEASE_ACONFIG_CHECK_API_LEVEL": "",
			"RELEASE_ACONFIG_FLAG_DEFAULT_PERMISSION": "READ_WRITE", "RELEASE_ACONFIG_REQUIRE_ALL_READ_ONLY": ""})
	g.release.ValueSets = nil
	return g
}

func TestJarjarRepackaging(t *testing.T) {
	g := jarjarGraph(t)
	n := g.jnode("fw", Device, "test")
	if n == nil {
		t.Fatalf("%v", g.problems)
	}
	dir := filepath.Join(g.out, "device", "fw")
	// collectJarJarRules: the group in srcs carries the aconfig library's five classes, jarjar_prefix names them
	want := ""
	for _, c := range []string{"CustomFeatureFlags", "FakeFeatureFlagsImpl", "FeatureFlags", "FeatureFlagsImpl", "Flags"} {
		want += "rule com.x.flags." + c + " com.android.internal.hidden_from_bootclasspath.com.x.flags." + c + "\n"
	}
	rules := filepath.Join(dir, "repackaged-jarjar", "repackaging.txt")
	if got := g.genFiles[rules]; got != want {
		t.Errorf("repackaging.txt:\n%s\nwant:\n%s", got, want)
	}
	ninja := g.java.b.String()
	// repackageFlagsIfNecessary: javac's jar through the rules, one shard
	rp := ninjaEdgeVars(ninja, filepath.Join(dir, "repackaged-jarjar", "javac", "fw.jar"))
	if rp == nil || rp["rules"] != esc(rules) || rp["total"] != "1" || rp["index"] != "0" {
		t.Errorf("repackage %v", rp)
	}
	// .srcjars of the group: the aconfig library's srcjar compiled into fw
	javac := ninjaEdgeVars(ninja, filepath.Join(dir, "javac", "fw.jar"))
	if javac == nil || javac["srcjars"] != esc(filepath.Join(g.out, "device", "flags-java", "gen", "flags-java.srcjar")) {
		t.Errorf("javac %v", javac)
	}
	// jarjarIfNecessary with jarjar_shards 3: <jar minus .jar>.-<i>.jar each `3 <i>`, merged without strip flags
	for i, s := range []string{"0", "1", "2"} {
		e := ninjaEdgeVars(ninja, filepath.Join(dir, "jarjar", "fw.-"+s+".jar"))
		if e == nil || e["total"] != "3" || e["index"] != s || e["rules"] != esc(g.abs("j/rules.txt")) {
			t.Errorf("shard %d: %v", i, e)
		}
	}
	merge := ninjaEdgeVars(ninja, filepath.Join(dir, "jarjar", "fw.jar"))
	if merge == nil || merge["args"] != "" || !strings.Contains(merge[""], ": combine ") {
		t.Errorf("merge %v", merge)
	}
	// dependents inherit the resolved rules
	if n.renames["com.x.flags.Flags"] != "com.android.internal.hidden_from_bootclasspath.com.x.flags.Flags" {
		t.Errorf("renames %v", n.renames)
	}
	// base.go compile (Turbine on, a device module): headerJarFile = jarjarIfNecessary(turbine-combined) then, repackaged,
	// repackagedHeaderJarFile = repackageFlagsIfNecessary(headerJarFile); localHeaderJars = both, libs dependents compile against both
	plain, repackaged := filepath.Join(dir, "javac-header", "fw.jar"), filepath.Join(dir, "repackaged-jarjar", "turbine", "fw.jar")
	if got := n.headers(); len(got) != 2 || got[0] != plain || got[1] != repackaged {
		t.Errorf("headers %v", got)
	}
	if e := ninjaEdgeVars(ninja, plain); e == nil || !strings.Contains(e[""], filepath.Join(dir, "jarjar", "turbine", "fw.jar")) {
		t.Errorf("plain header %v", e)
	}
	if e := ninjaEdgeVars(ninja, filepath.Join(dir, "jarjar", "turbine", "fw.jar")); e == nil || !strings.Contains(e[""], filepath.Join(dir, "javac", "fw.jar")) ||
		e["rules"] != esc(g.abs("j/rules.txt")) || e["total"] != "1" {
		t.Errorf("header jarjar %v", e)
	}
	if e := ninjaEdgeVars(ninja, repackaged); e == nil || e["rules"] != esc(rules) || !strings.Contains(e[""], plain) {
		t.Errorf("repackaged header %v", e)
	}
}

// repackagedTree: lib is repackaged by its jarjar_prefix (an exportable aconfig library in its srcs); user links it as a static lib or a
// lib, plain has no renames.
const repackagedTree = `
aconfig_declarations { name: "decl", package: "com.x.flags", container: "system", srcs: ["f.aconfig"], exportable: true }
java_aconfig_library { name: "flags-java", aconfig_declarations: "decl", sdk_version: "none", system_modules: "none" }
java_library { name: "lib", srcs: ["A.java"], static_libs: ["flags-java"], sdk_version: "none", system_modules: "none",
    jarjar_prefix: "com.android.internal.hidden_from_bootclasspath" }
java_library { name: "wrap", static_libs: ["lib", "plain"], sdk_version: "none", system_modules: "none" }
java_library { name: "user", srcs: ["U.java"], libs: ["wrap"], sdk_version: "none", system_modules: "none" }
java_library { name: "stat", srcs: ["S.java"], static_libs: ["lib"], sdk_version: "none", system_modules: "none" }
java_library_host { name: "hostlib", srcs: ["H.java"], static_libs: ["flags-java"], jarjar_prefix: "com.android.internal.hidden_from_bootclasspath" }
java_library { name: "plain", srcs: ["P.java"], sdk_version: "none", system_modules: "none" }
java_library { name: "hdr", srcs: ["U.java"], static_libs: ["lib"], headers_only: true, sdk_version: "none", system_modules: "none" }
java_binary_host { name: "turbine", srcs: ["T.java"] }
`

func repackagedGraph(t *testing.T) *Graph {
	g := javaTestGraph(t, map[string]string{"j/Android.bp": repackagedTree, "j/A.java": "", "j/U.java": "", "j/S.java": "", "j/H.java": "", "j/P.java": "", "j/T.java": "",
		"j/f.aconfig": ""},
		map[string]string{"RELEASE_READ_FROM_NEW_STORAGE": "", "RELEASE_ACONFIG_NEW_EXPORTED": "", "RELEASE_ACONFIG_CHECK_API_LEVEL": "",
			"RELEASE_ACONFIG_FLAG_DEFAULT_PERMISSION": "READ_WRITE", "RELEASE_ACONFIG_REQUIRE_ALL_READ_ONLY": ""})
	g.release.ValueSets = nil
	return g
}

func TestRepackagedStaticLibHeaders(t *testing.T) {
	g := repackagedGraph(t)
	wrap := g.jnode("wrap", Device, "test")
	if wrap == nil {
		t.Fatalf("%v", g.problems)
	}
	lib := g.java.nodes[jKey{"lib", Device}]
	ninja := g.java.b.String()
	wdir := filepath.Join(g.out, "device", "wrap")
	// collectDeps staticLibTag + compileJavaHeader: lib's TransitiveStaticLibsHeaderJars (its unmodified and its repackaged header) are merged
	// into wrap's header, unlike the implementation merge, which takes lib's repackaged implementation; wrap has no code of its own and
	// inherits lib's rules, so it exports both headers too
	hdr := ninjaEdgeVars(ninja, filepath.Join(wdir, "turbine-combined", "wrap.jar"))
	plainHdr := g.java.nodes[jKey{"plain", Device}].header
	if hdr == nil {
		t.Fatalf("no turbine-combined edge for wrap\n%s", ninja)
	}
	for _, want := range append(lib.headers(), plainHdr) {
		if !strings.Contains(hdr[""], want) {
			t.Errorf("wrap's header merge misses %s: %s", want, hdr[""])
		}
	}
	if strings.Contains(hdr[""], filepath.Join(g.out, "device", "lib", "combined")) {
		t.Errorf("wrap's header merges lib's implementation: %s", hdr[""])
	}
	if got := wrap.headers(); len(got) != 2 || got[0] != filepath.Join(wdir, "javac-header", "wrap.jar") || got[1] != filepath.Join(wdir, "repackaged-jarjar", "turbine", "wrap.jar") {
		t.Errorf("wrap headers %v", got)
	}
	// the implementation merge keeps lib's repackaged implementation only
	if want := lib.impl[0]; len(wrap.impl) < 1 || wrap.impl[0] != want {
		t.Errorf("wrap impl %v, want %v first", wrap.impl, want)
	}
}

func TestRepackagedClasspath(t *testing.T) {
	g := repackagedGraph(t)
	for _, name := range []string{"user", "stat"} {
		if g.jnode(name, Device, "test") == nil {
			t.Fatalf("%v", g.problems)
		}
	}
	ninja := g.java.b.String()
	lib := g.java.nodes[jKey{"lib", Device}]
	wrap := g.java.nodes[jKey{"wrap", Device}]
	// collectDeps libTag / staticLibTag: both put TransitiveStaticLibsHeaderJars (here: the unmodified and the repackaged header) on the classpath
	for name, dep := range map[string]*jNode{"user": wrap, "stat": lib} {
		e := ninjaEdgeVars(ninja, filepath.Join(g.out, "device", name, "javac", name+".jar"))
		if e == nil {
			t.Fatalf("%s: no javac edge", name)
		}
		for _, h := range dep.headers() {
			if !strings.Contains(e["cp"], h) {
				t.Errorf("%s: classpath %q misses %s", name, e["cp"], h)
			}
		}
	}
}

func TestHostModuleExportsOneHeader(t *testing.T) {
	// compile: only a device module with Turbine on exports RepackagedHeaderJars; the host path's header is the repackaged implementation's
	g := repackagedGraph(t)
	h := g.jnode("hostlib", Host, "test")
	if h == nil {
		t.Fatalf("%v", g.problems)
	}
	if h.repackagedHeader != "" || len(h.headers()) != 1 || h.header != filepath.Join(g.out, "host", "hostlib", "javac-header", "hostlib.jar") {
		t.Errorf("host headers %v (repackaged %q)", h.headers(), h.repackagedHeader)
	}
	// convertImplementationJarToHeaderJar: the combined implementation (repackaged classes + the static lib), not a merge of unrepackaged ones
	if e := ninjaEdgeVars(g.java.b.String(), h.header); e == nil || !strings.Contains(e[""], filepath.Join(g.out, "host", "hostlib", "combined", "hostlib.jar")) {
		t.Errorf("host header edge %v", e)
	}
}

func TestHeadersOnlyMergesStaticHeaders(t *testing.T) {
	// compileJavaHeader merges deps.transitiveStaticLibsHeaderJars: a repackaged static lib contributes its unmodified and its repackaged
	// header, not its implementation
	g := repackagedGraph(t)
	n := g.jnode("hdr", Device, "test")
	if n == nil {
		t.Fatalf("%v", g.problems)
	}
	lib := g.java.nodes[jKey{"lib", Device}]
	comb := filepath.Join(g.out, "device", "hdr", "turbine-combined", "hdr.jar")
	e := ninjaEdgeVars(g.java.b.String(), comb)
	if e == nil {
		t.Fatalf("no turbine-combined edge")
	}
	for _, h := range lib.headers() {
		if !strings.Contains(e[""], h) {
			t.Errorf("turbine-combined misses %s: %s", h, e[""])
		}
	}
	if strings.Contains(e[""], filepath.Join(g.out, "device", "lib", "combined")) {
		t.Errorf("turbine-combined merges lib's implementation: %s", e[""])
	}
}

func TestCombinedApis(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"fb/api/Android.bp": `combined_apis { name: "frameworks-base-api", bootclasspath: ["art.module.public.api", "framework-a", "framework-location"] }`,
		"fb/api/api.go": "package api\nconst art = \"art.module.public.api\"\nconst location = \"framework-location\"\n" +
			"var core_libraries_modules = []string{art}\nvar non_updatable_modules = []string{location}\n",
	}, nil)
	// createMergedFrameworkModuleLibStubs: bootclasspath minus core libraries minus non-updatable, .stubs.module_lib; module_current
	m := g.lookupJava("framework-updatable-stubs-module_libs_api", Device)
	if m == nil || !reflect.DeepEqual(m.Props.strs("static_libs"), []string{"framework-a.stubs.module_lib"}) || m.Props.str("sdk_version") != "module_current" {
		t.Fatalf("module %+v problems %v", m, g.problems)
	}
	// createMergedSystemStubs: non-updatable .stubs.system then the updatable merge
	if s := g.lookupJava("all-modules-system-stubs", Device); s == nil ||
		!reflect.DeepEqual(s.Props.strs("static_libs"), []string{"framework-location.stubs.system", "all-updatable-modules-system-stubs"}) {
		t.Errorf("system stubs %+v", s)
	}
	if p := g.lookupJava("all-modules-public-stubs", Device); p == nil ||
		!reflect.DeepEqual(p.Props.strs("static_libs"), []string{"art.module.public.api.stubs", "framework-a.stubs", "framework-location.stubs"}) {
		t.Errorf("public stubs %+v", p)
	}
}

func TestFilegroupDeviceSrcs(t *testing.T) {
	// filegroup.go: srcs less exclude_srcs, then device_first_srcs and device_common_srcs (exclude_srcs does not apply to them)
	g := graphFor(t, map[string]string{
		"f/Android.bp": `filegroup { name: "fg", srcs: ["a.java", "x.java"], exclude_srcs: ["x.java"], device_common_srcs: ["b.java", "x.java"] }`,
		"f/a.java":     "", "f/b.java": "", "f/x.java": "",
	})
	files, ok := g.resolveSrc("f", Host, ":fg", "t")
	var paths []string
	for _, f := range files {
		paths = append(paths, f.path)
	}
	if want := []string{"f/a.java", "f/b.java", "f/x.java"}; !ok || !reflect.DeepEqual(paths, want) {
		t.Errorf("files %v, want %v (%v)", paths, want, g.problems)
	}
}

func TestGenruleJ2Tools(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"g/Android.bp": `
blueprint_go_binary { name: "soong_zip" }
java_binary_host { name: "jt", srcs: ["T.java"] }
genrule { name: "z", tools: ["soong_zip", "jt"], cmd: "$(location soong_zip) -o $(out) && $(location jt)", out: ["z.zip"] }
`, "g/T.java": "",
	}, nil)
	bt := t.TempDir()
	if err := os.WriteFile(filepath.Join(bt, "soong_zip"), nil, 0o755); err != nil {
		t.Fatal(err)
	}
	g.java.tools.BuildTools = bt
	gn := g.genNode("z", Host, "test")
	if gn == nil || gn.rule.broken != "" {
		t.Fatalf("%+v %v", gn, g.problems)
	}
	// J2: the prebuilt soong_zip; a java_binary_host runs as its installed wrapper (java.go Binary: jar-wrapper.sh as bin/<name>)
	want := [][]string{{filepath.Join(bt, "soong_zip")}, {filepath.Join(g.out, "host", "bin", "jt")}}
	if got := [][]string{gn.rule.labels[0].files, gn.rule.labels[1].files}; !reflect.DeepEqual(got, want) {
		t.Errorf("tool labels %v, want %v", got, want)
	}
}

func TestSelectRelease(t *testing.T) {
	// release_flag(): bool as true / false; soong_config_variable() from the release's config vars; product_variable(build_from_text_stub)
	r := &Release{flags: map[string]string{"RELEASE_B": "", "RELEASE_S": "two"}, bools: map[string]bool{"RELEASE_B": true},
		configVars: map[string]string{"ANDROID/crash": "true"}}
	files := map[string]string{"s/Android.bp": `
java_library {
    name: "s",
    srcs: select(release_flag("RELEASE_B"), { true: ["t.java"], false: ["f.java"] }) +
        select(release_flag("RELEASE_S"), { "two": ["two.java"], default: [] }) +
        select(soong_config_variable("ANDROID", "crash"), { "true": ["c.java"], default: [] }) +
        select(product_variable("build_from_text_stub"), { true: ["p.java"], default: [] }),
}`}
	g := javaTestGraph(t, files, nil)
	parsed, err := discover(g.tree)
	if err != nil {
		t.Fatal(err)
	}
	mods, problems := (&evaluator{cfg: g.cfg, os: "android", rel: r}).modules(parsed)
	if len(problems) > 0 {
		t.Fatal(problems)
	}
	if got := mods["s"][0].Props.strs("srcs"); !reflect.DeepEqual(got, []string{"f.java", "two.java", "c.java", "p.java"}) {
		t.Errorf("srcs %v", got)
	}
}
