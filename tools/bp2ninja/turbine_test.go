// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// build/soong/java base.go compile (headers_only: Turbine, nothing else), builder.go TransformJavaToHeaderClasses / turbineFlags
// (--system <dir> for Java 9+, classpath space-separated), FormTurbineSystemModulesPath.
func TestTurbineHeadersOnly(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"h/Android.bp": `
java_library { name: "hdr", srcs: ["A.java"], sdk_version: "none", system_modules: "sm", headers_only: true, libs: ["dep"] }
java_library { name: "dep", srcs: ["D.java"], sdk_version: "none", system_modules: "none" }
java_system_modules { name: "sm", libs: ["dep"] }
java_binary_host { name: "turbine", srcs: ["T.java"] }
`,
		"h/A.java": "", "h/D.java": "", "h/T.java": "",
	}, nil)
	n := g.jnode("hdr", Device, "test")
	if n == nil {
		t.Fatalf("%v", g.problems)
	}
	dir := filepath.Join(g.out, "device", "hdr")
	out := filepath.Join(dir, "turbine", "hdr.jar")
	e := ninjaEdgeVars(g.java.b.String(), out)
	depHeader := filepath.Join(g.out, "device", "dep", "turbine-combined", "dep.jar")
	if e == nil || e["bootcp"] != "--system "+esc(filepath.Join(g.out, "device", "sm", "system")) || e["cp"] != esc(depHeader) || e["ver"] != "21" {
		t.Errorf("turbine edge %v", e)
	}
	// compileJavaHeader: turbine-combined (always, stripping META-INF/TRANSITIVE) is header and implementation; no javac edge
	comb := filepath.Join(dir, "turbine-combined", "hdr.jar")
	ce := ninjaEdgeVars(g.java.b.String(), comb)
	if n.header != comb || n.implRes != comb || ce == nil || ce["args"][:31] != "-stripDir META-INF/TRANSITIVE -" ||
		ninjaEdgeVars(g.java.b.String(), filepath.Join(dir, "javac", "hdr.jar")) != nil {
		t.Errorf("header %s implRes %s combine %v", n.header, n.implRes, ce)
	}
}

// base.go collectDeps: a plugin with generates_api (own, or exported by a libs dependency) disables turbine; the header is then the
// implementation jar without META-INF/services (convertImplementationJarToHeaderJar) and no turbine edge exists for the module.
func TestGeneratesApiPluginDisablesTurbine(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"p/Android.bp": `
java_plugin { name: "gen", srcs: ["G.java"], processor_class: "p.G", generates_api: true }
java_plugin { name: "plain", srcs: ["G.java"], processor_class: "p.P" }
java_library { name: "own", srcs: ["A.java"], plugins: ["gen"], sdk_version: "none", system_modules: "none" }
java_library { name: "exp", srcs: ["A.java"], exported_plugins: ["gen"], sdk_version: "none", system_modules: "none" }
java_library { name: "user", srcs: ["A.java"], libs: ["exp"], sdk_version: "none", system_modules: "none" }
java_library { name: "proc", srcs: ["A.java"], plugins: ["plain"], sdk_version: "none", system_modules: "none" }
java_binary_host { name: "turbine", srcs: ["G.java"] }
`,
		"p/A.java": "", "p/G.java": "",
	}, nil)
	ninja := func() string { return g.java.b.String() }
	for name, turbine := range map[string]bool{"own": false, "user": false, "exp": true, "proc": true} {
		n := g.jnode(name, Device, "test")
		if n == nil {
			t.Fatalf("%v", g.problems)
		}
		dir := filepath.Join(g.out, "device", name)
		want := filepath.Join(dir, "javac-header", name+".jar")
		if turbine {
			want = filepath.Join(dir, "turbine-combined", name+".jar")
		}
		if n.header != want || (ninjaEdgeVars(ninja(), filepath.Join(dir, "turbine", name+".jar")) != nil) != turbine {
			t.Errorf("%s: header %s, want %s (turbine %v)", name, n.header, want, turbine)
		}
	}
}

// The point of turbine headers, through ninja with the real rule text and fake tools: a method-body edit of a library reruns its turbine
// (the probe responds) but leaves the header bytes alone, so the dependent's javac does not rerun; an API edit reruns it. With the header
// taken from the implementation jar (javac-header, the pre-turbine model) the body edit reaches the dependent and the first check fails.
// Fake turbine keeps the lines starting with API (the "signatures"); fake javac keeps every line.
func TestBodyEditStopsAtTurbineHeader(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"l/Android.bp": `
java_library { name: "lib", srcs: ["L.java"], sdk_version: "none", system_modules: "none" }
java_library { name: "app", srcs: ["U.java"], libs: ["lib"], sdk_version: "none", system_modules: "none" }
java_binary_host { name: "turbine", srcs: ["T.java"] }
`,
		"l/L.java": "API void f();\nbody 1\n", "l/U.java": "use f\n", "l/T.java": "",
		"build/soong/scripts/jar-wrapper.sh": "",
	}, nil)
	dir := t.TempDir()
	jdk, bt, runs := filepath.Join(dir, "jdk"), filepath.Join(dir, "bt"), filepath.Join(dir, "runs")
	write := func(path, text string, mode os.FileMode) {
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(text), mode); err != nil {
			t.Fatal(err)
		}
	}
	fake := func(path, name, body string) {
		write(path, "#!/bin/sh\nafter() { f=$1; shift; while [ $# -gt 0 ]; do [ \"$1\" = \"$f\" ] && { echo \"$2\"; return; }; shift; done; }\n"+body+"\n", 0o755)
	}
	// javac: logs "javac <outdir>", every source named by an @file into <outdir>/C.class
	fake(filepath.Join(jdk, "bin", "javac"), "javac", `o=$(after -d "$@"); echo "javac $o" >> `+runs+`
for a in "$@"; do case $a in @*) cat $(cat "${a#@}") ;; esac; done > "$o/C.class"`)
	// java -jar turbine: logs "turbine", the API lines of the --sources @file
	fake(filepath.Join(jdk, "bin", "java"), "turbine", `echo turbine >> `+runs+`
s=$(after --sources "$@"); grep -h '^API' $(cat "${s#@}") > "$(after --output "$@")"`)
	fake(filepath.Join(bt, "soong_zip"), "soongzip", `cat $(find "$(after -C "$@")" -type f | sort) > "$(after -o "$@")"`)
	fake(filepath.Join(bt, "zip2zip"), "zip2zip", `cp "$(after -i "$@")" "$(after -o "$@")"`)
	// merge_zips [flags] [-stripDir|-stripFile|-m X] out in...: concatenation
	fake(filepath.Join(bt, "merge_zips"), "mergezips", `while [ $# -gt 0 ]; do case $1 in -stripDir|-stripFile|-m) shift 2 ;; -*) shift ;; *) break ;; esac; done
o=$1; shift; cat "$@" > "$o"`)
	g.java.tools = JavaTools{JDK: jdk, BuildTools: bt}
	if g.jnode("app", Device, "test") == nil {
		t.Fatalf("%v", g.problems)
	}
	appJar := filepath.Join(g.out, "device", "app", "javac", "app.jar")
	write(filepath.Join(g.out, "build.ninja"), "root = "+g.tree+"\nrule cp\n  command = cp $in $out\n"+g.java.ninja(g), 0o644)
	build := func(when string) string {
		_ = os.Remove(runs)
		if b, err := exec.Command("ninja", "-C", g.out, appJar).CombinedOutput(); err != nil {
			t.Fatalf("%s: %v\n%s", when, err, b)
		}
		ran, _ := os.ReadFile(runs)
		return string(ran)
	}
	appRan := func(ran string) bool { return strings.Contains(ran, "javac "+filepath.Join(g.out, "device", "app")) }
	build("first build")
	lib := filepath.Join(g.tree, "l", "L.java")
	// past the coarse kernel clock tick (see TestNoChangeRebuildRunsNothing), so a rewritten file is newer
	time.Sleep(50 * time.Millisecond)
	write(lib, "API void f();\nbody 2\n", 0o644)
	ran := build("body edit")
	// app's javac needs lib's header only, so lib's javac is not built here at all
	if !strings.Contains(ran, "turbine\n") {
		t.Errorf("lib's turbine did not rerun after its source changed: the probe does not respond\n%s", ran)
	}
	if appRan(ran) {
		t.Errorf("a method-body edit of lib reran app's javac:\n%s", ran)
	}
	time.Sleep(50 * time.Millisecond)
	write(lib, "API void f();\nAPI void g();\nbody 2\n", 0o644)
	if ran := build("API edit"); !appRan(ran) {
		t.Errorf("an API edit of lib did not rerun app's javac:\n%s", ran)
	}
}
