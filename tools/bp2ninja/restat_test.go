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

// Every write-if-changed rule, run through ninja with the real rule text and fake tools: a rerun that writes the same bytes leaves the output's
// mtime alone and its dependents do not run (fails when a rule loses `restat = 1` or its compare); a rerun that writes other bytes reruns them
// (fails when the compare keeps a changed output). Each fake tool writes the content of `data`; `stamp` and `data` are every edge's inputs, so
// changing `stamp` reruns each edge without changing what it writes.
func TestUnchangedOutputStopsDependents(t *testing.T) {
	dir := t.TempDir()
	root, out, jdk, bt := filepath.Join(dir, "tree"), filepath.Join(dir, "out"), filepath.Join(dir, "jdk"), filepath.Join(dir, "bt")
	write := func(path, text string, mode os.FileMode) {
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(text), mode); err != nil {
			t.Fatal(err)
		}
	}
	data, stamp, runs := filepath.Join(dir, "data"), filepath.Join(dir, "stamp"), filepath.Join(dir, "runs")
	write(data, "v1\n", 0o644)
	write(stamp, "s1\n", 0o644)
	write(filepath.Join(root, "A.java"), "class A {}\n", 0o644)
	write(filepath.Join(root, "res", "values", "strings.xml"), "<resources/>\n", 0o644)
	// fake <name>: logs its run, then `body` with $D = data; arg helpers find the value after a flag
	fake := func(path, name, body string) {
		write(path, "#!/bin/sh\necho "+name+" >> "+runs+"\nD="+data+"\nafter() { f=$1; shift; while [ $# -gt 0 ]; do [ \"$1\" = \"$f\" ] && { echo \"$2\"; return; }; shift; done; }\n"+body+"\n", 0o755)
	}
	fake(filepath.Join(jdk, "bin", "javac"), "javac", `cat $D > "$(after -d "$@")/A.class"`)
	fake(filepath.Join(jdk, "bin", "java"), "turbine", `cat $D > "$(after --output "$@")"`)
	fake(filepath.Join(bt, "soong_zip"), "soongzip", `cat $(find "$(after -C "$@")" -type f | sort) > "$(after -o "$@")"`)
	fake(filepath.Join(bt, "zip2zip"), "zip2zip", `cp "$(after -i "$@")" "$(after -o "$@")"`)
	sdkp, aapt2 := filepath.Join(dir, "sdkparcelables"), filepath.Join(dir, "aapt2")
	fake(sdkp, "sdkparcelables", `cat $D > "$2"`)
	// aapt2 compile -o <dir> <res>: one <type>_<name>.flat per resource
	fake(aapt2, "aapt2", `o=$(after -o "$@"); r=$(eval echo \${$#}); cat $D > "$o/$(basename $(dirname $r))_$(basename $r .xml).arsc.flat"`)

	j := &javaGraph{tools: JavaTools{JDK: jdk, BuildTools: bt}}
	o := func(p string) string { return filepath.Join(out, p) }
	flat := o("aapt2/res/values_strings.arsc.flat")
	deps := stamp + " " + data // every edge's implicit inputs
	// each rule under test: its output, consumed by a `consume` edge
	cases := map[string]string{
		"javac": "build " + o("A.jar") + ": javac | " + deps + "\n  srcs = A.java\n  outdir = " + o("cls") + "\n  annodir = " + o("anno") +
			"\n  srcjardir = " + o("srcjars") + "\n",
		"header":         "build " + o("H.jar") + ": header " + data + " | " + deps + "\n",
		"turbine":        "build " + o("T.jar") + ": turbine " + filepath.Join(root, "A.java") + " | " + deps + "\n  turbine = turbine.jar\n",
		"sdkparcelables": "build " + o("P.aidl") + ": sdkparcelables " + data + " | " + deps + " " + sdkp + "\n  tool = " + sdkp + "\n",
		"sortu":          "build " + o("S.aidl") + ": sortu " + data + " | " + deps + "\n",
		"aapt2compile": "build " + flat + ": aapt2compile " + filepath.Join(root, "res", "values", "strings.xml") + " | " + deps + " " + aapt2 +
			"\n  aapt2 = " + aapt2 + "\n  outdir = " + o("aapt2/res") + "\n  tmpdir = " + o("aapt2-tmp/res/0") + "\n",
	}
	outputs := map[string]string{"javac": o("A.jar"), "header": o("H.jar"), "turbine": o("T.jar"), "sdkparcelables": o("P.aidl"),
		"sortu": o("S.aidl"), "aapt2compile": flat}
	ninja := "root = " + root + "\n" + j.header(&Graph{out: out}) + "rule consume\n  command = cp $in $out\n"
	for name, edge := range cases {
		ninja += edge + "build " + outputs[name] + ".used: consume " + outputs[name] + "\n"
	}
	write(filepath.Join(out, "build.ninja"), ninja, 0o644)
	build := func(when string) {
		if b, err := exec.Command("ninja", "-C", out).CombinedOutput(); err != nil {
			t.Fatalf("%s: %v\n%s", when, err, b)
		}
	}
	mtimes := func() map[string]time.Time {
		m := map[string]time.Time{}
		for name, f := range outputs {
			st, err := os.Stat(f + ".used")
			if err != nil {
				t.Fatal(err)
			}
			m[name] = st.ModTime()
		}
		return m
	}
	build("first build")
	before := mtimes()
	// past the coarse kernel clock tick (see TestNoChangeRebuildRunsNothing), so a rewritten file is newer
	time.Sleep(50 * time.Millisecond)
	write(stamp, "s2\n", 0o644)
	_ = os.Remove(runs)
	build("rebuild with the same bytes")
	ran, _ := os.ReadFile(runs)
	for _, tool := range []string{"javac", "turbine", "zip2zip", "sdkparcelables", "aapt2"} {
		if !strings.Contains(string(ran), tool+"\n") {
			t.Errorf("%s did not rerun after its input changed: the probe does not respond", tool)
		}
	}
	for name, mt := range mtimes() {
		if !mt.Equal(before[name]) {
			t.Errorf("%s wrote the same bytes, yet its dependent reran", name)
		}
	}
	if b, err := exec.Command("ninja", "-C", out, "-n").CombinedOutput(); err != nil || !strings.Contains(string(b), "no work to do") {
		t.Errorf("a no-change rebuild after the restat runs steps (%v):\n%s", err, b)
	}
	time.Sleep(50 * time.Millisecond)
	write(data, "v2\n", 0o644)
	build("rebuild with other bytes")
	for name, mt := range mtimes() {
		if mt.Equal(before[name]) {
			t.Errorf("%s wrote other bytes, yet its dependent did not rerun", name)
		}
	}
}
