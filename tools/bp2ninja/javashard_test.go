// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"
)

// android.ShardPaths: consecutive chunks of at most size, in input order; expected chunks written out from that definition.
func TestShardPaths(t *testing.T) {
	in := []string{"a", "b", "c", "d", "e", "f", "g"}
	got := shardPaths(in, 3)
	want := [][]string{{"a", "b", "c"}, {"d", "e", "f"}, {"g"}}
	if len(got) != len(want) {
		t.Fatalf("shards %v, want %v", got, want)
	}
	for i := range want {
		if !slices.Equal(got[i], want[i]) {
			t.Errorf("shard %d = %v, want %v", i, got[i], want[i])
		}
	}
	// javac_shard_size 4 with srcjars: 4/5 = 0 per srcjar shard, taken as 1 (Soong divides by zero)
	if got := shardPaths(in[:2], 4/5); len(got) != 2 {
		t.Errorf("size 0: %v, want one per path", got)
	}
}

// base.go compile, enableSharding: javac_shard_size 2 over 5 sources = 3 javac edges javac/<stem><i>.jar ([A B] [C D] [E]), each with
// the module's turbine jar ahead of its classpath and its own classes<i> directory; no unsharded javac edge. A plugin that generates API
// turns Turbine off, and with it sharding (one javac/<stem>.jar).
func TestJavacShardSize(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"s/Android.bp": `
java_plugin { name: "gen", srcs: ["G.java"], processor_class: "p.G", generates_api: true }
java_library { name: "lib", srcs: ["A.java", "B.java", "C.java", "D.java", "E.java"], javac_shard_size: 2, libs: ["dep"], sdk_version: "none", system_modules: "none" }
java_library { name: "noturbine", srcs: ["A.java", "B.java", "C.java"], javac_shard_size: 2, plugins: ["gen"], sdk_version: "none", system_modules: "none" }
java_library { name: "dep", srcs: ["G.java"], sdk_version: "none", system_modules: "none" }
java_binary_host { name: "turbine", srcs: ["G.java"] }
`,
		"s/A.java": "", "s/B.java": "", "s/C.java": "", "s/D.java": "", "s/E.java": "", "s/G.java": "",
	}, nil)
	for _, name := range []string{"lib", "noturbine"} {
		if g.jnode(name, Device, "test") == nil {
			t.Fatalf("%v", g.problems)
		}
	}
	b := g.java.b.String()
	dir := filepath.Join(g.out, "device", "lib")
	hdr := filepath.Join(dir, "turbine", "lib.jar")
	dep := filepath.Join(g.out, "device", "dep", "turbine-combined", "dep.jar")
	for i, srcs := range []string{"s/A.java s/B.java", "s/C.java s/D.java", "s/E.java"} {
		idx := string(rune('0' + i))
		e := ninjaEdgeVars(b, filepath.Join(dir, "javac", "lib"+idx+".jar"))
		if e == nil || e["srcs"] != srcs || e["cp"] != "-classpath "+esc(hdr+":"+dep) || e["outdir"] != esc(filepath.Join(dir, "javac", "classes"+idx)) {
			t.Errorf("shard %d edge %v", i, e)
		}
	}
	if ninjaEdgeVars(b, filepath.Join(dir, "javac", "lib3.jar")) != nil || ninjaEdgeVars(b, filepath.Join(dir, "javac", "lib.jar")) != nil {
		t.Errorf("lib: a fourth shard or an unsharded javac edge")
	}
	nd := filepath.Join(g.out, "device", "noturbine", "javac")
	if ninjaEdgeVars(b, filepath.Join(nd, "noturbine.jar")) == nil || ninjaEdgeVars(b, filepath.Join(nd, "noturbine0.jar")) != nil {
		t.Errorf("noturbine: want one unsharded javac edge")
	}
}

// The point of sharding and dex pieces, through ninja with the real rule text and fake tools: a method-body edit of one source reruns
// that shard's javac (the probe responds) and d8 piece and the d8 merge, but no other shard's javac or d8 piece. Fake turbine keeps the
// API lines, so the header is unchanged; fake javac and d8 copy their inputs. Without sharding (javacShardSize 0) the module has one javac
// edge, so the shard checks fail.
func TestShardEditRerunsOneShard(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"s/Android.bp": `
java_library { name: "lib", srcs: ["A.java", "B.java", "C.java"], javac_shard_size: 1, installable: true, sdk_version: "none", system_modules: "none" }
java_binary_host { name: "turbine", srcs: ["T.java"] }
java_binary_host { name: "d8", srcs: ["T.java"], wrapper: "d8.sh" }
`,
		"s/A.java": "API a\nbody 1\n", "s/B.java": "API b\nbody 1\n", "s/C.java": "API c\nbody 1\n", "s/T.java": "",
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
	after := "#!/bin/sh\nafter() { f=$1; shift; while [ $# -gt 0 ]; do [ \"$1\" = \"$f\" ] && { echo \"$2\"; return; }; shift; done; }\n"
	fake := func(path, body string) { write(path, after+body+"\n", 0o755) }
	fake(filepath.Join(jdk, "bin", "javac"), `o=$(after -d "$@"); echo "javac $o" >> `+runs+`
for a in "$@"; do case $a in @*) cat $(cat "${a#@}") ;; esac; done > "$o/C.class"`)
	fake(filepath.Join(jdk, "bin", "java"), `s=$(after --sources "$@"); grep -h '^API' $(cat "${s#@}") > "$(after --output "$@")"`)
	// d8 --output <x.zip | dir> ... <input last>: logs "d8 <output>", copies the inputs (a merge: every input)
	write(filepath.Join(g.tree, "s", "d8.sh"), after+`o=$(after --output "$@"); echo "d8 $o" >> `+runs+`
eval "last=\${$#}"; case $o in *.zip) cat "$last" > "$o" ;; *) for a in "$@"; do case $a in *.zip) cat "$a" ;; esac; done > "$o/classes.dex" ;; esac
`, 0o755)
	fake(filepath.Join(bt, "soong_zip"), `cat $(find "$(after -C "$@")" -type f | sort) > "$(after -o "$@")"`)
	fake(filepath.Join(bt, "zipalign"), `exit 0`)
	fake(filepath.Join(bt, "merge_zips"), `while [ $# -gt 0 ]; do case $1 in -stripDir|-stripFile|-m) shift 2 ;; -*) shift ;; *) break ;; esac; done
o=$1; shift; cat "$@" > "$o"`)
	g.java.tools = JavaTools{JDK: jdk, BuildTools: bt}
	n := g.jnode("lib", Device, "test")
	if n == nil || n.dex == "" {
		t.Fatalf("no dex: %v", g.problems)
	}
	write(filepath.Join(g.out, "build.ninja"), "root = "+g.tree+"\nrule cp\n  command = cp $in $out\n"+g.java.ninja(g), 0o644)
	build := func(when string) string {
		_ = os.Remove(runs)
		if b, err := exec.Command("ninja", "-C", g.out, n.dex).CombinedOutput(); err != nil {
			t.Fatalf("%s: %v\n%s", when, err, b)
		}
		ran, _ := os.ReadFile(runs)
		return string(ran)
	}
	ld := filepath.Join(g.out, "device", "lib")
	shard := func(i string) string { return "javac " + filepath.Join(ld, "javac", "classes"+i) + "\n" }
	piece := func(i string) string { return "d8 " + filepath.Join(ld, "dex-pieces", i+".zip") + ".tmp.zip\n" }
	ran := build("first build")
	for _, i := range []string{"0", "1", "2"} {
		if !strings.Contains(ran, shard(i)) || !strings.Contains(ran, piece(i)) {
			t.Fatalf("first build lacks shard %s's javac or d8 piece:\n%s", i, ran)
		}
	}
	time.Sleep(50 * time.Millisecond)
	write(filepath.Join(g.tree, "s", "B.java"), "API b\nbody 2\n", 0o644)
	ran = build("body edit")
	if !strings.Contains(ran, shard("1")) || !strings.Contains(ran, piece("1")) || !strings.Contains(ran, "d8 "+filepath.Join(ld, "dex")+"\n") {
		t.Errorf("B.java's shard, d8 piece or the merge did not rerun:\n%s", ran)
	}
	for _, i := range []string{"0", "2"} {
		if strings.Contains(ran, shard(i)) || strings.Contains(ran, piece(i)) {
			t.Errorf("a body edit of B.java reran shard %s:\n%s", i, ran)
		}
	}
	if b, _ := os.ReadFile(n.dex); !strings.Contains(string(b), "body 2") || !strings.Contains(string(b), "API a") {
		t.Errorf("merged dex lacks the edit or another shard: %q", b)
	}
}
