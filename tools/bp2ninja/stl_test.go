// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// standInLibcxx: the provider's headers in graphFor's graphs (the real ones come from libcxxProvider, tested below).
const standInLibcxx = "/libcxx-stand-in/c++/v1"

// Expected selections are Soong's cc/stl.go begin for a non-Windows, non-SDK module, by the stl value and whether the module is static.
func TestSTLSelection(t *testing.T) {
	for _, c := range []struct {
		stl         string
		set, static bool
		want        string
	}{
		{"", false, false, "libc++"},
		{"", false, true, "libc++_static"},
		{"", true, false, "libc++"},
		{"system", true, false, "libc++"},
		{"system", true, true, "libc++_static"},
		{"libc++", true, true, "libc++"},
		{"libc++_static", true, false, "libc++_static"},
		{"c++_shared", true, false, "libc++"},
		{"c++_static", true, false, "libc++_static"},
		{"none", true, false, ""},
		{"none", true, true, ""},
	} {
		got, err := selectSTL(c.stl, c.set, c.static)
		if err != nil || got != c.want {
			t.Errorf("stl %q set %v static %v: %q (%v), want %q", c.stl, c.set, c.static, got, err, c.want)
		}
	}
	// Soong's linux branch rejects these: libstdc++ is not selectable here, ndk_* names are internal
	for _, bad := range []string{"libstdc++", "ndk_libc++_shared", "gnustl"} {
		if _, err := selectSTL(bad, true, false); err == nil {
			t.Errorf("stl %q accepted, want an error", bad)
		}
	}
}

// stlCases: one module per stl value (set: the property is written; unset: no property). Each side builds the same modules.
var stlCases = []struct {
	name, stl string
	set       bool
}{
	{"unset", "", false},
	{"empty", "", true},
	{"libcxx", "libc++", true},
	{"libcxx_static", "libc++_static", true},
	{"cxx_shared", "c++_shared", true},
	{"none", "none", true},
	{"system", "system", true},
}

// stlCaseBp: the Android.bp of stlCases, each module a static library with one C++ source, and a shared library of the same stl.
func stlCaseBp() string {
	var b strings.Builder
	for _, c := range stlCases {
		prop := ""
		if c.set {
			prop = fmt.Sprintf(`, stl: %q`, c.stl)
		}
		fmt.Fprintf(&b, "cc_library_static { name: %q, srcs: [\"a.cc\"]%s }\n", "s_"+c.name, prop)
		fmt.Fprintf(&b, "cc_library_shared { name: %q, srcs: [\"a.cc\"]%s }\n", "d_"+c.name, prop)
	}
	return b.String()
}

// isLibcxx: the module's stl selects libc++ (its headers and library), i.e. is not "none"; an unset stl is libc++.
func isLibcxx(stl string) bool {
	return stl != "none"
}

// Per stl value and side: a C++ compile gets -nostdinc++, and the provider's headers as -isystem unless the module is "none".
// -stdlib=libc++ is gone everywhere.
func TestSTLCompileFlagsPerValue(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": stlCaseBp(), "p/a.cc": ""})
	for _, side := range []Side{Device, Host} {
		for _, c := range stlCases {
			n := g.node("s_"+c.name, side, Static, "test")
			if n == nil {
				t.Fatal(g.problems)
			}
			argv := g.compileFlags(n).cxx
			if !slices.Contains(argv, "-nostdinc++") {
				t.Errorf("%v %s: no -nostdinc++ in %q", side, c.name, argv)
			}
			if slices.Contains(argv, "-stdlib=libc++") {
				t.Errorf("%v %s: blanket -stdlib=libc++ still present", side, c.name)
			}
			hasProvider := slices.Contains(argv, standInLibcxx)
			if want := isLibcxx(c.stl); hasProvider != want {
				t.Errorf("%v %s: provider headers %v, want %v in %q", side, c.name, hasProvider, want, argv)
			}
			if hasProvider {
				if i := slices.Index(argv, standInLibcxx); i < 1 || argv[i-1] != "-isystem" {
					t.Errorf("%v %s: provider not an -isystem argument in %q", side, c.name, argv)
				}
			}
		}
	}
	if len(g.problems) != 0 {
		t.Errorf("problems %v", g.problems)
	}
}

// Per stl value: every link gets -nostdlib++; -lc++ only where the selection is libc++.
func TestSTLLinkFlagsPerValue(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": stlCaseBp(), "p/a.cc": ""})
	for _, c := range stlCases {
		n := g.node("d_"+c.name, Device, Shared, "test")
		if n == nil {
			t.Fatal(g.problems)
		}
		libs, _ := g.linkLibs(n)
		if !slices.Contains(libs, "-nostdlib++") {
			t.Errorf("%s: no -nostdlib++ in %q", c.name, libs)
		}
		if got := slices.Contains(libs, "-lc++"); got != isLibcxx(c.stl) {
			t.Errorf("%s: -lc++ %v, want %v in %q", c.name, got, isLibcxx(c.stl), libs)
		}
		if slices.Contains(libs, "-stdlib=libc++") {
			t.Errorf("%s: blanket -stdlib=libc++ still in the link", c.name)
		}
	}
}

// stl resolves through defaults like every property (cronet's stable_cronet_cc_defaults sets stl "none"); a module's own stl wins over them.
func TestSTLThroughDefaults(t *testing.T) {
	g := graphFor(t, map[string]string{
		"p/Android.bp": `
cc_defaults { name: "none_defaults", stl: "none" }
cc_library_static { name: "via_defaults", defaults: ["none_defaults"], srcs: ["a.cc"] }
cc_library_static { name: "own_wins", defaults: ["none_defaults"], srcs: ["a.cc"], stl: "libc++" }
cc_library_shared { name: "so_via_defaults", defaults: ["none_defaults"], srcs: ["a.cc"] }
`,
		"p/a.cc": "",
	})
	via := g.node("via_defaults", Device, Static, "test")
	if via == nil {
		t.Fatal(g.problems)
	}
	if argv := g.compileFlags(via).cxx; slices.Contains(argv, standInLibcxx) || !slices.Contains(argv, "-nostdinc++") {
		t.Errorf("defaults stl none: compile %q", argv)
	}
	own := g.node("own_wins", Device, Static, "test")
	if argv := g.compileFlags(own).cxx; !slices.Contains(argv, standInLibcxx) {
		t.Errorf("own stl libc++ over defaults: compile %q", argv)
	}
	so := g.node("so_via_defaults", Device, Shared, "test")
	if libs, _ := g.linkLibs(so); slices.Contains(libs, "-lc++") {
		t.Errorf("defaults stl none: link %q has -lc++", libs)
	}
}

// A stl value the selection rejects is a problem, and the module compiles with no library.
func TestSTLBadValueIsProblem(t *testing.T) {
	g := graphFor(t, map[string]string{
		"p/Android.bp": `cc_library_static { name: "bad", srcs: ["a.cc"], stl: "libstdc++" }`,
		"p/a.cc":       "",
	})
	n := g.node("bad", Device, Static, "test")
	if n == nil {
		t.Fatal(g.problems)
	}
	argv := g.compileFlags(n).cxx
	if slices.Contains(argv, standInLibcxx) || len(g.problems) != 1 || !strings.Contains(g.problems[0], "libstdc++") {
		t.Errorf("bad stl: argv %q, problems %v", argv, g.problems)
	}
}

// clang -v's search list for -stdlib=libc++ on the distro's clang++-19: the first c++/v1 entry is the provider.
const clangSearchLibcxx = `#include "..." search starts here:
#include <...> search starts here:
 /usr/lib/llvm-19/bin/../include/c++/v1
 /usr/lib/llvm-19/lib/clang/19/include
 /usr/local/include
 /usr/include/x86_64-linux-gnu
 /usr/include
End of search list.
`

func TestParseLibcxxSearch(t *testing.T) {
	if dir, ok := parseLibcxxSearch(clangSearchLibcxx); !ok || dir != "/usr/lib/llvm-19/include/c++/v1" {
		t.Errorf("provider %q (%v), want /usr/lib/llvm-19/include/c++/v1", dir, ok)
	}
	// libstdc++'s directories, the search list without libc++: no provider
	gnu := strings.Replace(clangSearchLibcxx, " /usr/lib/llvm-19/bin/../include/c++/v1\n", " /usr/include/c++/14\n", 1)
	if dir, ok := parseLibcxxSearch(gnu); ok {
		t.Errorf("search list without libc++ names %q", dir)
	}
}

// The derived provider is the directory the compiler's libc++ really reads: its __config header is there.
func TestLibcxxProviderIsCompilersHeaders(t *testing.T) {
	const cxx = "clang++-19"
	if _, err := exec.LookPath(cxx); err != nil {
		t.Skip(cxx + " not installed")
	}
	dir, err := libcxxProvider(cxx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(dir, "__config")); err != nil {
		t.Errorf("provider %s: %v", dir, err)
	}
}
