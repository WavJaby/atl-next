// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"strings"
	"testing"
)

// common_srcs (kotlin.go kotlinCompile's commonSrcFilesArg, scripts/gen-kotlin-build-file.py --common_srcs): each common source is a
// `<sources>` and a `<commonSources>` entry of the module file, absolute, after the module's own sources, and an input of the edge.
func TestKotlinCommonSrcs(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"kt/Android.bp": kotlinStdlib + `
java_library_host { name: "mp", srcs: ["src/B.kt"], common_srcs: ["common/**/*.kt"], kotlincflags: ["-Xmulti-platform"] }
`,
		"kt/src/B.kt": "", "kt/common/p/C.kt": "", "kt/s.jar": "", "kt/s7.jar": "", "kt/s8.jar": "", "kt/a.jar": "",
	}, nil)
	if n := g.jnode("mp", Host, "test"); n == nil {
		t.Fatalf("no node: %v", g.problems)
	}
	for _, p := range g.problems {
		if strings.Contains(p, "common_srcs") {
			t.Errorf("common_srcs reported: %s", p)
		}
	}
	ninja := g.java.b.String()
	kt := ninjaEdgeVars(ninja, filepath.Join(g.out, "host", "mp", "kotlin", "mp.jar"))
	common := g.abs("kt/common/p/C.kt")
	if kt == nil || kt["common"] != esc(common) || !strings.Contains(kt[""], " | ") || !strings.Contains(kt[""], esc(common)) {
		t.Fatalf("kotlinc edge %v, want common %s as a variable and an implicit input", kt, common)
	}
	if !strings.Contains(g.java.kotlincRule(g), `for f in $common; do printf '    <sources path="%s"/>\n    <commonSources path="%s"/>\n' "$$f" "$$f"; done;`) {
		t.Errorf("module file rule lacks the common sources entries")
	}
}
