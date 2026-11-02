// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

// Expected values are derived from Soong at the pinned tag, never from the code under test: java/kotlin.go kotlinCompile and base.go compile's
// Kotlin branch, java/gen.go genAidl / genLogtags, base.go aidlFlags / individualAidlFlags, java/proto.go protoFlags, android/proto.go
// ProtoRule, sysprop/sysprop_library.go syspropLibraryHook, system/tools/hidl/build/hidl_interface.go hidlInterfaceMutator, and
// system/tools/aidl/build aidl_interface_backends.go addJavaLibrary / aidl_gen_rule.go.

// kotlinStdlib: the libraries deps() adds for .kt sources, as java_import modules on both sides.
const kotlinStdlib = `
java_import { name: "kotlin-stdlib", host_supported: true, jars: ["s.jar"] }
java_import { name: "kotlin-stdlib-jdk7", host_supported: true, jars: ["s7.jar"] }
java_import { name: "kotlin-stdlib-jdk8", host_supported: true, jars: ["s8.jar"] }
java_import { name: "kotlin-annotations", host_supported: true, jars: ["a.jar"] }
`

func TestKotlinEdges(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"kt/Android.bp": kotlinStdlib + `
java_library_host { name: "k", srcs: ["src/**/*.java", "src/**/*.kt"], kotlincflags: ["-Xjvm-default=all"] }
`,
		"kt/src/p/A.java": "", "kt/src/p/B.kt": "", "kt/s.jar": "", "kt/s7.jar": "", "kt/s8.jar": "", "kt/a.jar": "",
	}, nil)
	n := g.jnode("k", Host, "test")
	if n == nil {
		t.Fatalf("%v", g.problems)
	}
	ninja := g.java.b.String()
	dir := filepath.Join(g.out, "host", "k")
	kt := ninjaEdgeVars(ninja, filepath.Join(dir, "kotlin", "k.jar"))
	if kt == nil {
		t.Fatalf("no kotlinc edge:\n%s", ninja)
	}
	// kotlincflags first, then the default language version 1.9, -Xsam-conversions=class, -no-stdlib; no -no-jdk on the host
	if want := "-Xjvm-default=all -language-version 1.9 -Xsam-conversions=class -no-stdlib"; kt["flags"] != want {
		t.Errorf("kotlinc flags %q, want %q", kt["flags"], want)
	}
	// kotlinName: <dir>/<variant>/<name> with / -> __; -jvm-target from the java version (21 with RELEASE_TARGET_JAVA_21)
	if kt["name"] != "kt__linux_glibc_common__k" || kt["target"] != "21" {
		t.Errorf("kotlinc name %q target %q", kt["name"], kt["target"])
	}
	// the header jar is a second output; inputs are every .java and .kt source
	if !strings.Contains(kt[""], esc(filepath.Join(dir, "kotlin_headers", "k.jar"))+": kotlinc ") ||
		!strings.Contains(kt[""], esc(g.abs("kt/src/p/A.java"))) || !strings.Contains(kt[""], esc(g.abs("kt/src/p/B.kt"))) {
		t.Errorf("kotlinc statement %q", kt[""])
	}
	// javac: kotlinc's header jar first on the classpath
	javac := ninjaEdgeVars(ninja, filepath.Join(dir, "javac", "k.jar"))
	if javac == nil || !strings.HasPrefix(javac["cp"], "-classpath "+esc(filepath.Join(dir, "kotlin_headers", "k.jar")+":")) {
		t.Errorf("javac %v", javac)
	}
	// implementation: kotlinc's classes, then javac's, then the static kotlin stdlib (static_kotlin_stdlib default true)
	comb := ninjaEdgeVars(ninja, filepath.Join(dir, "combined", "k.jar"))
	want := strings.Join([]string{esc(filepath.Join(dir, "kotlin", "k.jar")), esc(filepath.Join(dir, "javac", "k.jar")), esc(g.abs("kt/s.jar")),
		esc(g.abs("kt/s7.jar")), esc(g.abs("kt/s8.jar")), esc(g.abs("kt/a.jar"))}, " ")
	if comb == nil || !strings.HasSuffix(comb[""], want) {
		t.Errorf("combined %v, want inputs %s", comb, want)
	}
}

func TestKotlinStdlibNotStatic(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"kt/Android.bp": kotlinStdlib + `java_library_host { name: "k", srcs: ["B.kt"], static_kotlin_stdlib: false }`,
		"kt/B.kt":       "", "kt/s.jar": "", "kt/s7.jar": "", "kt/s8.jar": "", "kt/a.jar": "",
	}, nil)
	n := g.jnode("k", Host, "test")
	if n == nil {
		t.Fatalf("%v", g.problems)
	}
	// libs, not static_libs: the implementation is kotlinc's jar alone
	if want := []string{filepath.Join(g.out, "host", "k", "kotlin", "k.jar")}; !reflect.DeepEqual(n.impl, want) {
		t.Errorf("impl %v, want %v", n.impl, want)
	}
}

func TestKotlinLangVersion2(t *testing.T) {
	// the build flag at "2" (bp2a's value): -Xsuppress-version-warnings -Xconsistent-data-class-copy-visibility instead of -language-version;
	// -no-jdk on device
	g := javaTestGraph(t, map[string]string{
		"kt/Android.bp": kotlinStdlib + `java_library { name: "k", srcs: ["B.kt"], sdk_version: "none", system_modules: "none" }`,
		"kt/B.kt":       "", "kt/s.jar": "", "kt/s7.jar": "", "kt/s8.jar": "", "kt/a.jar": "",
	}, map[string]string{"RELEASE_KOTLIN_LANG_VERSION": "2"})
	if g.jnode("k", Device, "test") == nil {
		t.Fatalf("%v", g.problems)
	}
	kt := ninjaEdgeVars(g.java.b.String(), filepath.Join(g.out, "device", "k", "kotlin", "k.jar"))
	if want := "-Xsuppress-version-warnings -Xconsistent-data-class-copy-visibility -Xsam-conversions=class -no-stdlib -no-jdk"; kt == nil || kt["flags"] != want {
		t.Errorf("kotlinc %v, want flags %q", kt, want)
	}
}

// javaGenTree: a java library with every generated source kind, the host tools, the logtags script and the protobuf runtime.
var javaGenTree = map[string]string{
	"lib/Android.bp": `
java_library {
    name: "gen",
    srcs: ["a/IFoo.aidl", "a/IBar.aidl", "b/x.logtags", "p/m.proto", "J.java"],
    sdk_version: "none",
    system_modules: "none",
    libs: ["dep"],
    aidl: {
        include_dirs: ["inc"],
        generate_get_transaction_name: true,
        enforce_permissions: true,
        enforce_permissions_exceptions: ["a/IFoo.aidl"],
    },
}
java_library { name: "dep", srcs: ["D.java"], sdk_version: "none", system_modules: "none", aidl: { export_include_dirs: ["ex"] } }
java_library { name: "libprotobuf-java-lite", srcs: ["P.java"], sdk_version: "none", system_modules: "none" }
`,
	"lib/a/IFoo.aidl": "", "lib/a/IBar.aidl": "", "lib/b/x.logtags": "", "lib/p/m.proto": "", "lib/J.java": "", "lib/D.java": "", "lib/P.java": "",
	"tools/Android.bp": `cc_binary_host { name: "aidl", srcs: ["a.cc"] } cc_binary_host { name: "aprotoc", srcs: ["p.cc"] }`,
	"tools/a.cc":       "", "tools/p.cc": "",
	"build/make/tools/Android.bp":             `python_binary_host { name: "java-event-log-tags", srcs: ["event_log_tags.py", "java-event-log-tags.py"] }`,
	"build/make/tools/event_log_tags.py":      "",
	"build/make/tools/java-event-log-tags.py": "",
}

func TestJavaGenSources(t *testing.T) {
	g := javaTestGraph(t, javaGenTree, nil)
	n := g.jnode("gen", Device, "test")
	if n == nil {
		t.Fatalf("%v", g.problems)
	}
	ninja := g.java.b.String()
	dir := filepath.Join(g.out, "device", "gen")
	tmp := filepath.Join(dir, "gen", "aidl", "aidl0.tmp")
	// aidlFlags: deps' exported dirs, include_dirs, the module dir (the sources' root is listed already); -t by default for an sdk "none"? no:
	// only system_server / core / core_platform / module / system; --transaction_names; --min_sdk_version of the unnumbered sdk = current
	base := "-I" + g.abs("lib/ex") + " -I" + g.abs("inc") + " -I" + g.abs("lib") + " --transaction_names --min_sdk_version=current"
	foo := ninjaEdgeVars(ninja, filepath.Join(tmp, "lib", "a", "IFoo.java"))
	bar := ninjaEdgeVars(ninja, filepath.Join(tmp, "lib", "a", "IBar.java"))
	if foo == nil || bar == nil {
		t.Fatalf("aidl edges missing:\n%s", ninja)
	}
	// individualAidlFlags: every source but the exceptions gets -Wmissing-permission-annotation -Werror
	if foo["flags"] != base || bar["flags"] != base+" -Wmissing-permission-annotation -Werror" {
		t.Errorf("aidl flags %q / %q, want %q (+ permission flags for IBar)", foo["flags"], bar["flags"], base)
	}
	if foo["aidl"] != esc(filepath.Join(g.out, "host", "aidl", "aidl")) {
		t.Errorf("aidl tool %q", foo["aidl"])
	}
	// genLogtags: gen/logtags/<path below the module dir>.java, java-event-log-tags run as its main script
	lt := ninjaEdgeVars(ninja, filepath.Join(dir, "gen", "logtags", "b", "x.java"))
	if lt == nil || lt["script"] != esc(g.abs("build/make/tools/java-event-log-tags.py")) {
		t.Errorf("logtags %v", lt)
	}
	// protoFlags: lite -> --java_out=lite:<dir>; ProtoRule: -I . (canonical_path_from_root default true), the source tree-relative is not
	// needed: aprotoc gets it absolute below the -I root
	pr := ninjaEdgeVars(ninja, filepath.Join(dir, "gen", "proto", "proto0.srcjar"))
	if pr == nil || !strings.Contains(pr["cmds"], "--java_out=lite:"+filepath.Join(dir, "gen", "proto", "proto0.tmp")+" ") || !strings.Contains(pr["cmds"], "-I "+g.tree+" ") {
		t.Errorf("proto %v", pr)
	}
	// javac: the logtags java among the sources, then the proto and aidl srcjars; the lite runtime is a static lib (protoDeps)
	javac := ninjaEdgeVars(ninja, filepath.Join(dir, "javac", "gen.jar"))
	srcjars := esc(filepath.Join(dir, "gen", "proto", "proto0.srcjar")) + " " + esc(filepath.Join(dir, "gen", "aidl", "aidl0.srcjar"))
	if javac == nil || javac["srcjars"] != srcjars || !strings.Contains(javac["srcs"], filepath.Join(dir, "gen", "logtags", "b", "x.java")) {
		t.Errorf("javac %v, want srcjars %s", javac, srcjars)
	}
	if !strings.Contains(javac["cp"], filepath.Join(g.out, "device", "libprotobuf-java-lite")) {
		t.Errorf("classpath %q lacks the protobuf runtime", javac["cp"])
	}
}

func TestPythonToolBesideMain(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"t/Android.bp": `
python_binary_host { name: "two", srcs: ["lib.py", "two.py"] }
python_binary_host { name: "away", srcs: ["away.py", "sub/lib.py"] }
python_binary_host { name: "nomain", srcs: ["a.py", "b.py"] }
`,
	}, nil)
	// binary.go getPyMainFile: main defaults to <name>.py; the par puts sources at its top level = main's directory
	files, why := pythonTool(g, g.lookup("two", Host))
	if why != "" || !reflect.DeepEqual(files, []string{"t/two.py", "t/lib.py"}) {
		t.Errorf("two: %v %q", files, why)
	}
	if _, why := pythonTool(g, g.lookup("away", Host)); !strings.Contains(why, "outside main's directory") {
		t.Errorf("away: %q", why)
	}
	if _, why := pythonTool(g, g.lookup("nomain", Host)); !strings.Contains(why, "is not in its") {
		t.Errorf("nomain: %q", why)
	}
}

func TestSyspropJava(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"sp/Android.bp": `
sysprop_library { name: "com.x.prop", srcs: ["a/P.sysprop"], property_owner: "Platform", installable: true }
java_library { name: "sysprop-library-stub-platform", srcs: ["S.java"], sdk_version: "none", system_modules: "none" }
cc_binary_host { name: "sysprop_java", srcs: ["j.cc"] }
`,
		"sp/a/P.sysprop": "", "sp/S.java": "", "sp/j.cc": "",
	}, nil)
	n := g.jnode("com.x.prop", Device, "test")
	if n == nil {
		t.Fatalf("%v", g.problems)
	}
	ninja := g.java.b.String()
	dir := filepath.Join(g.out, "device", "com.x.prop")
	// syspropJavaGenRule: GenPathWithExt(sysprop, <src>, srcjar), --scope internal for a platform library
	sj := ninjaEdgeVars(ninja, filepath.Join(dir, "gen", "sysprop", "a", "P.srcjar"))
	if sj == nil || sj["scope"] != "internal" || sj["tool"] != esc(filepath.Join(g.out, "host", "sysprop_java", "sysprop_java")) {
		t.Fatalf("sysprop_java edge %v:\n%s", sj, ninja)
	}
	// the java library: core_current, libs the platform stub, compiled from the srcjar; installable -> dex
	javac := ninjaEdgeVars(ninja, filepath.Join(dir, "javac", "com.x.prop.jar"))
	if javac == nil || javac["srcjars"] != esc(filepath.Join(dir, "gen", "sysprop", "a", "P.srcjar")) ||
		!strings.Contains(javac["cp"], filepath.Join(g.out, "device", "sysprop-library-stub-platform")) {
		t.Errorf("javac %v", javac)
	}
	if n.props.str("sdk_version") != "core_current" || n.dex == "" {
		t.Errorf("sdk %q dex %q", n.props.str("sdk_version"), n.dex)
	}
}

func TestHidlJava(t *testing.T) {
	files := map[string]string{
		"hw/Android.bp":   `hidl_package_root { name: "android.hidl" }`,
		"base/Android.bp": `hidl_interface { name: "android.hidl.base@1.0", root: "android.hidl", srcs: ["types.hal", "IBase.hal"] }`,
		"foo/Android.bp":  `hidl_interface { name: "android.hidl.foo@1.0", root: "android.hidl", srcs: ["IFoo.hal"], interfaces: ["android.hidl.base@1.0"], gen_java_constants: true }`,
		"base/types.hal":  "", "base/IBase.hal": "", "foo/IFoo.hal": "",
		"t/Android.bp": `cc_binary_host { name: "hidl-gen", srcs: ["h.cc"] }
java_library { name: "hwbinder.stubs", srcs: ["H.java"], sdk_version: "none", system_modules: "none" }`,
		"t/h.cc": "", "t/H.java": "",
	}
	g := javaTestGraph(t, files, nil)
	// javaName: the `interfaces`' -java libraries as static_libs, hwbinder.stubs as libs, core_current, installable
	m := g.lookupJava("android.hidl.foo-V1.0-java", Device)
	if m == nil || m.Type != "java_library" {
		t.Fatalf("module %+v, problems %v", m, g.problems)
	}
	p := m.Props
	if !reflect.DeepEqual(p.strs("static_libs"), []string{"android.hidl.base-V1.0-java"}) || !reflect.DeepEqual(p.strs("libs"), []string{"hwbinder.stubs"}) ||
		p.str("sdk_version") != "core_current" || !p.boolean("installable", false) || !reflect.DeepEqual(p.strs("srcs"), []string{":android.hidl.foo-V1.0-java_gen_java"}) {
		t.Errorf("-java props %+v", p)
	}
	// javaSharedName: the same, the interfaces as libs
	if sh := g.lookupJava("android.hidl.foo-V1.0-java-shallow", Device); sh == nil ||
		!reflect.DeepEqual(sh.Props.strs("libs"), []string{"hwbinder.stubs", "android.hidl.base-V1.0-java"}) || len(sh.Props.strs("static_libs")) != 0 {
		t.Errorf("-java-shallow %+v", sh)
	}
	// hidlSrcJarRule: hidl-gen -o <genDir>/srcs -L java, zipped to <genDir>/srcs.srcjar
	gn := g.genNode("android.hidl.foo-V1.0-java_gen_java", Device, "test")
	if gn == nil {
		t.Fatalf("%v", g.problems)
	}
	gd := g.genDir(gn, "")
	if !reflect.DeepEqual(gn.rule.outs, []string{filepath.Join(gd, "srcs.srcjar")}) || !strings.Contains(gn.rule.cmd, "-o $(genDir)/srcs -L java '-randroid.hidl:hw' 'android.hidl.foo@1.0' && ") {
		t.Errorf("java gen outs %v cmd %q", gn.rule.outs, gn.rule.cmd)
	}
	// javaConstantsSourcesName: <sanitizedDir>Constants.java = <pkg path>/V<maj>_<min>/Constants.java
	cn := g.genNode("android.hidl.foo-V1.0-java-constants_gen_java", Device, "test")
	if cn == nil || !reflect.DeepEqual(cn.rule.outs, []string{filepath.Join(g.genDir(cn, ""), "android/hidl/foo/V1_0/Constants.java")}) ||
		!strings.Contains(cn.rule.cmd, "-o $(genDir) -L java-constants ") {
		t.Errorf("constants gen %+v", cn)
	}
	// the java library compiles the srcjar
	n := g.jnode("android.hidl.foo-V1.0-java", Device, "test")
	if n == nil {
		t.Fatalf("%v", g.problems)
	}
	javac := ninjaEdgeVars(g.java.b.String(), filepath.Join(g.out, "device", "android.hidl.foo-V1.0-java", "javac", "android.hidl.foo-V1.0-java.jar"))
	if javac == nil || javac["srcjars"] != esc(filepath.Join(gd, "srcs.srcjar")) {
		t.Errorf("javac %v", javac)
	}
}

func TestAidlJavaBackend(t *testing.T) {
	files := map[string]string{
		"p/Android.bp": aidlTree,
		"p/aidl_api/common/1/com/common/IThing.aidl": "", "p/aidl_api/common/1/.hash": "hash-c1\n",
		"p/aidl_api/common/2/com/common/IThing.aidl": "", "p/aidl_api/common/2/.hash": "hash-c2\n",
		"p/aidl_api/app/1/com/app/IApp.aidl": "", "p/aidl_api/app/1/com/app/Data.aidl": "", "p/aidl_api/app/1/.hash": "hash-a1\n",
	}
	g := javaTestGraph(t, files, nil)
	n := g.jnode("app-V1-java", Device, "test")
	if n == nil {
		t.Fatalf("%v", g.problems)
	}
	// addJavaLibrary: sdk_version system_current (no platform_apis), installable, static_libs the import's java backend
	if n.props.str("sdk_version") != "system_current" || !n.props.boolean("installable", false) ||
		!reflect.DeepEqual(n.props.strs("static_libs"), []string{"common-V1-java"}) {
		t.Errorf("props %+v", n.props)
	}
	// aidl_gen_rule.go for java: no -t (gen_trace defaults on only with platform_apis), --min_sdk_version current, -I imported roots +
	// include_dirs, -N own root; one .java per source below the root
	ninja := g.java.b.String()
	e := ninjaEdgeVars(ninja, filepath.Join(g.out, "device", "app-V1-java", "gen", "aidl-java", "com", "app", "IApp.java"))
	want := "--structured --version 1 --hash hash-a1 --min_sdk_version current -I" + g.tree + "/p/aidl_api/common/1 -I" + g.tree + "/inc/aidl -N" + g.tree + "/p/aidl_api/app/1"
	if e == nil || e["flags"] != want {
		t.Errorf("aidl java edge %v, want flags %q", e, want)
	}
}
