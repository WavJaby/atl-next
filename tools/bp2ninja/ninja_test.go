// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"
)

// shWords: the argv /bin/sh makes of a ninja command fragment (ninja turns `$$` into `$` first).
func shWords(t *testing.T, fragment string) []string {
	out, err := exec.Command("/bin/sh", "-c", `printf '%s\n' `+strings.ReplaceAll(fragment, "$$", "$")).Output()
	if err != nil {
		t.Fatal(err)
	}
	return strings.Split(strings.TrimSuffix(string(out), "\n"), "\n")
}

// Expected values are Soong's ninja text for each cflags entry: blueprint proptools.NinjaAndShellEscape (`$` -> `$$`, then ShellEscape: unchanged when only shell-safe characters and spaces, else single-quoted whole).
// The shell decides the argv of both texts.
func TestModuleFlagsMatchSoong(t *testing.T) {
	for _, c := range []struct{ flag, soong string }{
		{"-include AndroidFixup.h", "-include AndroidFixup.h"},
		{"-mllvm  -enable-x", "-mllvm  -enable-x"},
		{`-DFOO="a b"`, `'-DFOO="a b"'`},
		{"-DX=$(Y)", "'-DX=$$(Y)'"},
		{"-Wall", "-Wall"},
	} {
		ours := shellQuote(moduleFlags(objOf("cflags", []string{c.flag}), "cflags"))
		if got, want := shWords(t, ours), shWords(t, c.soong); !slices.Equal(got, want) {
			t.Errorf("%q: argv %q (ninja %q), Soong's %q (ninja %q)", c.flag, got, ours, want, c.soong)
		}
	}
}

// -launcher: ninja's own expansion of a cc edge runs the launcher with the compiler as its first argument; without it the compiler runs directly (xtask native --no-compiler-cache).
// A compile with debug info (libdbg's -g) runs directly either way (debugInfo).
func TestLauncherPrefixesCompiles(t *testing.T) {
	for _, launcher := range []string{"/opt/cache/sccache", ""} {
		g := graphFor(t, map[string]string{"p/Android.bp": `
cc_library_static { name: "libs", srcs: ["s.cc", "c.c"], static_libs: ["libdbg"] }
cc_library_static { name: "libdbg", srcs: ["d.cc"], cflags: ["-g"] }
`})
		if g.node("libs", Device, Static, "goal") == nil {
			t.Fatal(g.problems)
		}
		ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar", Launcher: launcher})
		if err := os.WriteFile(filepath.Join(g.out, "build.ninja"), []byte(ninja), 0o644); err != nil {
			t.Fatal(err)
		}
		objs := regexp.MustCompile(`(?m)^build (\S+\.o): cc `).FindAllStringSubmatch(ninja, -1)
		if len(objs) != 3 {
			t.Fatalf("launcher %q: %d cc edges, want 3", launcher, len(objs))
		}
		for _, m := range objs {
			out, err := exec.Command("ninja", "-C", g.out, "-t", "commands", "-s", m[1]).Output()
			if err != nil {
				t.Fatal(err)
			}
			// the compile cds to the tree root first (cwd_test.go): `cd`, the quoted root and `&&` are 3 words
			argv := strings.Fields(string(out))[3:]
			want := []string{launcher, "c++"}
			if strings.HasSuffix(m[1], ".c.o") {
				want[1] = "cc"
			}
			if launcher == "" || strings.HasSuffix(m[1], "d.cc.o") {
				want = want[1:]
			}
			if !slices.Equal(argv[:len(want)], want) {
				t.Errorf("launcher %q: %s runs %q, want %q first", launcher, m[1], argv, want)
			}
		}
	}
}

// A ninja run with nothing changed since the last one runs no step. An edge ninja finds dirty on every run reruns its dependents too: measured on xtask native,
// ~4000 edges re-ran per no-change run. The causes, each faked as the real tool does it: depfile entries ninja cannot find (clang several paths per line, a tree-relative
// one among them, -fprofile-list=<tree path>; protoc's --dependency_out without a final newline); a declared output the tool never writes (aidl on `parcelable Foo;`);
// a write-if-changed output older than a rebuilt tool it depends on (soong_zip -write_if_changed after a relinked protoc).
func TestNoChangeRebuildRunsNothing(t *testing.T) {
	dir := t.TempDir()
	root, out := filepath.Join(dir, "tree"), filepath.Join(dir, "out")
	write := func(path, text string, mode os.FileMode) {
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(text), mode); err != nil {
			t.Fatal(err)
		}
	}
	for _, f := range []string{"a.c", "rel.list", "p.proto", "q.proto", "P.aidl", "tool"} {
		write(filepath.Join(root, f), "x\n", 0o644)
	}
	tool := filepath.Join(root, "tool")
	// argv as the cc rule passes it: -MD -MF <dep> -c <in> -o <out>
	cc := filepath.Join(dir, "fakecc")
	write(cc, "#!/bin/sh\nprintf '%s: %s rel.list \\\\\\n  %s\\n' \"$7\" \"$5\" \"$5\" > \"$3\" && : > \"$7\"\n", 0o755)
	// as jaidl passes it: --ninja -d<dep> <tree-relative src> <out>; a parcelable declaration: a depfile, no Java
	aidl := filepath.Join(dir, "fakeaidl")
	write(aidl, "#!/bin/sh\nprintf '%s : \\\\\\n  %s\\n' \"$4\" \"$3\" > \"${2#-d}\"\n", 0o755)
	// soong_zip -srcjar -write_if_changed -o <out> ...: the output exists, so it stays unchanged
	zip := filepath.Join(dir, "fakezip")
	write(zip, "#!/bin/sh\n[ -e \"$4\" ] || : > \"$4\"\n", 0o755)
	tmp := filepath.Join(out, "p.tmp")
	protoc := func(k int, java, proto string) string {
		return "printf '%s: %s' " + filepath.Join(tmp, java) + " " + filepath.Join(root, proto) + " > " + filepath.Join(tmp+".d", strconv.Itoa(k)+".d")
	}
	aidlOut := filepath.Join(out, "a.tmp", "P.java")
	ninja := "root = " + root + "\ncc = " + cc + "\nsoongzip = " + zip + "\naidl = " + aidl + "\n" + header + javaGenRules +
		"build " + filepath.Join(out, "a.o") + ": cc " + filepath.Join(root, "a.c") + "\n" +
		"build " + filepath.Join(out, "p.srcjar") + ": jproto " + filepath.Join(root, "p.proto") + " " + filepath.Join(root, "q.proto") + " | " + tool + "\n" +
		"  tmp = " + tmp + "\n  cmds = " + protoc(0, "P.java", "p.proto") + " && " + protoc(1, "Q.java", "q.proto") + "\n" +
		"build " + aidlOut + ": jaidl " + filepath.Join(root, "P.aidl") + " | " + tool + "\n  src = P.aidl\n" +
		"build " + filepath.Join(out, "a.srcjar") + ": srcjar " + aidlOut + "\n  dir = " + filepath.Join(out, "a.tmp") + "\n"
	write(filepath.Join(out, "build.ninja"), ninja, 0o644)
	if b, err := exec.Command("ninja", "-C", out).CombinedOutput(); err != nil {
		t.Fatalf("first build: %v\n%s", err, b)
	}
	// a relinked tool: its dependents rerun once, the write-if-changed srcjars keep their mtime. The sleep puts the tool's mtime past the outputs':
	// file timestamps come from the kernel's coarse clock (ticks of up to 10 ms), and an equal mtime is not newer, which hides the stale-output case.
	time.Sleep(50 * time.Millisecond)
	write(tool, "y\n", 0o644)
	if b, err := exec.Command("ninja", "-C", out).CombinedOutput(); err != nil {
		t.Fatalf("build after the tool changed: %v\n%s", err, b)
	}
	b, err := exec.Command("ninja", "-C", out, "-n", "-d", "explain").CombinedOutput()
	if err != nil || !strings.Contains(string(b), "no work to do") {
		t.Errorf("a no-change rebuild runs steps (%v):\n%s", err, b)
	}
}

// Every depfile rule folds into .ninja_deps: otherwise ninja loads the depfile at startup and refuses the whole build when it names an undeclared output
// (protoc's mod.rs, measured on a rebuild), so a rebuild of an existing out dir stops before any step.
func TestDepfileRulesUseDeps(t *testing.T) {
	rules := 0
	for _, r := range strings.Split(header, "\nrule ")[1:] {
		name, body, _ := strings.Cut(r, "\n")
		if strings.Contains(body, "  depfile = ") {
			rules++
			if !strings.Contains(body, "  deps = gcc\n") {
				t.Errorf("rule %s has a depfile but no `deps = gcc`", name)
			}
		}
	}
	if rules == 0 {
		t.Fatal("no depfile rule found: header parse broken")
	}
}
