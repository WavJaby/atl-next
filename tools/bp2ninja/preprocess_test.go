// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// cronet's placeholder_build_config_srcjar shape: a genrule's output is preprocessed by cc_preprocess_no_configuration, and a genrule
// reads the result as $(in). The preprocess is one cc edge: -D/-P/-E cflags, then -I of the module dir, clang++ for a .cc source.
func TestPreprocessFeedsGenrule(t *testing.T) {
	g := graphFor(t, map[string]string{
		"p/Android.bp": `
genrule { name: "rename", srcs: ["T.template"], cmd: "cp $(in) $(out)", out: ["BuildConfig.cc"] }
cc_preprocess_no_configuration { name: "pp", srcs: [":rename"], cflags: ["-DANDROID", "-P", "-E"] }
genrule { name: "use", srcs: [":pp"], cmd: "cp $(in) $(genDir)/BuildConfig.java", out: ["BuildConfig.java"] }
`,
		"p/T.template": "",
	})
	use := g.genNode(":use", Device, "test")
	if use == nil {
		t.Fatal(g.problems)
	}
	renameOut := filepath.Join(g.out, "gen", "device", "rename", "BuildConfig.cc")
	ppOut := filepath.Join(g.out, "gen", "device", "pp", "BuildConfig.cc")
	if !slices.Equal(use.rule.inputs, []string{ppOut}) {
		t.Fatalf("genrule input %v, want the preprocess output %s (problems %v)", use.rule.inputs, ppOut, g.problems)
	}
	ninja := g.emit(Tools{CC: "clang", CXX: "clang++", AR: "ar"})
	edge := ninjaEdgeVars(ninja, ppOut)
	if edge == nil {
		t.Fatalf("no preprocess edge:\n%s", ninja)
	}
	if want := "build " + ppOut + ": cc " + renameOut; !strings.HasPrefix(edge[""], want) {
		t.Errorf("edge %q, want prefix %q", edge[""], want)
	}
	if edge["cc"] != "$cxx" {
		t.Errorf("compiler %q for a .cc source, want clang++ ($cxx)", edge["cc"])
	}
	if want := "-DANDROID -P -E -I" + filepath.Join(g.tree, "p"); edge["flags"] != want {
		t.Errorf("flags %q, want %q", edge["flags"], want)
	}
	if edge["launcher"] != "" {
		t.Errorf("launcher %q, want none: the preprocess is not cached", edge["launcher"])
	}
}

// Soong's checks: one source, -D/-P only, -E required; a rejected module names its problem and leaves the consumer unresolved.
func TestPreprocessRejectsWhatSoongRejects(t *testing.T) {
	for _, c := range []struct{ name, cflags, problem string }{
		{"noe", `"-DX"`, "must have a -E cflag"},
		{"flag", `"-E", "-include", "x.h"`, "only allows -D and -P"},
	} {
		g := graphFor(t, map[string]string{
			"p/Android.bp": `
cc_preprocess_no_configuration { name: "` + c.name + `", srcs: ["T.c"], cflags: [` + c.cflags + `] }
`,
			"p/T.c": "",
		})
		if n := g.preprocessNode(":"+c.name, Device, "test"); n != nil {
			t.Errorf("%s: node built, want none", c.name)
		}
		if len(g.problems) != 1 || !strings.Contains(g.problems[0], c.problem) {
			t.Errorf("%s: problems %v, want one containing %q", c.name, g.problems, c.problem)
		}
	}
}
