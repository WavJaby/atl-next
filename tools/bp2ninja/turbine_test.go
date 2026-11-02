// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"testing"
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
	depHeader := filepath.Join(g.out, "device", "dep", "javac-header", "dep.jar")
	if e == nil || e["bootcp"] != "--system "+esc(filepath.Join(g.out, "device", "sm", "system")) || e["cp"] != esc(depHeader) || e["ver"] != "21" {
		t.Errorf("turbine edge %v", e)
	}
	// header and implementation are the Turbine jar; no javac edge
	if n.header != out || n.implRes != out || ninjaEdgeVars(g.java.b.String(), filepath.Join(dir, "javac", "hdr.jar")) != nil {
		t.Errorf("header %s implRes %s", n.header, n.implRes)
	}
}
