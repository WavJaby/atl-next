// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"strings"
	"testing"
)

// android_library_import: its one aar is unzipped and the aar's classes.jar merged into the implementation (aar.go unzipAAR);
// a dependent takes that jar on its classpath as its static library's implementation.
func TestAarImportClassesJar(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"lib/Android.bp": `
android_library_import { name: "annot", aars: ["a.aar"], sdk_version: "34" }
java_library { name: "user", srcs: ["U.java"], static_libs: ["annot"], sdk_version: "none", system_modules: "none" }
`,
		"lib/a.aar":  "",
		"lib/U.java": "",
	}, nil)
	if n := g.jnode("user", Device, "test"); n == nil {
		t.Fatalf("no node: %v", g.problems)
	}
	ninja := g.java.b.String()
	dir := filepath.Join(g.out, "device", "annot")
	jar := filepath.Join(dir, "aar", "annot.jar")
	aar := ninjaEdgeVars(ninja, jar)
	if aar == nil {
		t.Fatalf("no aar edge:\n%s", ninja)
	}
	if want := "build " + jar + ": aar " + filepath.Join(g.tree, "lib", "a.aar"); !strings.HasPrefix(aar[""], want) {
		t.Errorf("aar edge %q, want prefix %q", aar[""], want)
	}
	if want := esc(filepath.Join(dir, "aar", "unzipped")); aar["outdir"] != want {
		t.Errorf("outdir %q, want %q", aar["outdir"], want)
	}
	if n := g.jnode("annot", Device, "test"); n == nil || n.header != jar {
		t.Errorf("import header %v, want %s (problems %v)", n, jar, g.problems)
	}
	comb := ninjaEdgeVars(ninja, filepath.Join(g.out, "device", "user", "combined", "user.jar"))
	if comb == nil || !strings.Contains(comb[""], jar) {
		t.Errorf("user's combined jar %v does not take the aar's classes jar %s", comb, jar)
	}
}
