// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"testing"
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
			argv := strings.Fields(string(out))
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
