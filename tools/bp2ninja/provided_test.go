// SPDX-License-Identifier: MIT

package main

import (
	"slices"
	"strings"
	"testing"
)

// A provided module replaces the tree's definition: no compile or link step of its own, its artifact first on the dependent's link line (shared_libs and static_libs alike), its include dirs (-isystem) and defines on the dependent's compiles, and the header libraries the tree module re-exports.
func TestProvidedModule(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_library_headers { name: "libh", export_include_dirs: ["hinc"] }
cc_library { name: "libcrypto", srcs: ["c.cc"], export_include_dirs: ["inc"], header_libs: ["libh"], export_header_lib_headers: ["libh"] }
cc_library_shared { name: "libother", srcs: ["o.cc"] }
cc_library { name: "libuser", srcs: ["u.cc"], shared_libs: ["libother", "libcrypto"] }
cc_library { name: "libstat", srcs: ["s.cc"], static_libs: ["libcrypto"] }
`})
	lib := "/prefix/lib/libcrypto.so"
	g.provided = map[string]Provided{"libcrypto": {Libs: []string{lib}, Include: []string{"/prefix/include"}, Defines: []string{"PFX=X"}}}
	for _, goal := range []string{"libuser", "libstat"} {
		if g.node(goal, Device, Shared, "goal") == nil {
			t.Fatalf("%s: problems %v", goal, g.problems)
		}
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	if strings.Contains(ninja, "/p/c.cc") {
		t.Error("the tree's libcrypto source is compiled")
	}
	for _, goal := range []string{"libuser", "libstat"} {
		var link, compile string
		for _, l := range strings.Split(ninja, "\n") {
			if strings.HasPrefix(l, "build ") && strings.Contains(l, "/"+goal+".so: link ") {
				link = l
			}
			if strings.HasPrefix(l, "build ") && strings.Contains(l, "/obj/device/"+goal+"/") {
				compile = l
			}
		}
		if !strings.Contains(link, "| "+lib) {
			t.Errorf("%s: link does not wait for %s: %q", goal, lib, link)
		}
		var flags []string
		for _, n := range g.order {
			if n.name == goal {
				flags = g.compileFlags(n).cxx
				// first although libuser names libother before it
				if l, _ := g.linkLibs(n); l[0] != lib || !slices.Contains(l, "-Wl,-rpath,/prefix/lib") {
					t.Errorf("%s: link libs %q", goal, l)
				}
			}
		}
		// the provided module's own -isystem: the libc++ provider's comes first in a C++ compile (stl.go)
		i := slices.Index(flags, "/prefix/include")
		if compile == "" || i < 1 || flags[i-1] != "-isystem" || !slices.Contains(flags, "-DPFX=X") {
			t.Errorf("%s: compile flags %q", goal, flags)
		}
		// the tree's own export dir is not; its re-exported header library's is
		if !slices.Contains(flags, "-I"+g.abs("p/hinc")) || slices.Contains(flags, "-I"+g.abs("p/inc")) {
			t.Errorf("%s: header library flags %q", goal, flags)
		}
	}
}

// A row with several providers (a host library + a crate shared by two rows): every artifact first on the link line, in the row's order; one shared by two modules is linked once.
func TestSeveralProviders(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_library_shared { name: "libEGL", srcs: ["e.cc"] }
cc_library_shared { name: "libGLESv2", srcs: ["g.cc"] }
cc_library_shared { name: "libbase", srcs: ["b.cc"] }
cc_library_shared { name: "libgui", srcs: ["u.cc"], shared_libs: ["libbase", "libEGL", "libGLESv2"] }
`})
	ext := "/crates/lib/libshared_ext.so"
	g.provided = map[string]Provided{
		"libEGL":    {Libs: []string{"/usr/lib/libEGL.so", ext}},
		"libGLESv2": {Libs: []string{"/usr/lib/libGLESv2.so", ext}},
	}
	if g.node("libgui", Device, Shared, "goal") == nil {
		t.Fatalf("problems %v", g.problems)
	}
	for _, n := range g.order {
		if n.name != "libgui" {
			continue
		}
		l, inputs := g.linkLibs(n)
		want := []string{"/usr/lib/libEGL.so", ext, "/usr/lib/libGLESv2.so"}
		if !slices.Equal(l[:len(want)], want) {
			t.Errorf("link libs %q", l)
		}
		// the tree's rpath before the providers': a distro dir must not shadow a library the tree builds (libz, libselinux)
		tree := slices.IndexFunc(l, func(s string) bool {
			return strings.HasPrefix(s, "-Wl,-rpath,") && s != "-Wl,-rpath,/usr/lib" && s != "-Wl,-rpath,/crates/lib"
		})
		if sys, crate := slices.Index(l, "-Wl,-rpath,/usr/lib"), slices.Index(l, "-Wl,-rpath,/crates/lib"); tree < 0 || sys < tree || crate < tree {
			t.Errorf("rpath order %q", l)
		}
		if c := slices.Index(inputs, ext); c < 0 || slices.Index(inputs[c+1:], ext) >= 0 {
			t.Errorf("%s not linked exactly once: %q", ext, inputs)
		}
	}
	// an archive is linked where it is named, so it is never one of several
	g.provided["libbase"] = Provided{Libs: []string{"/x/libbase.a", ext}}
	g.providedNode("libbase", Device, g.provided["libbase"])
	if !slices.ContainsFunc(g.problems, func(p string) bool { return strings.Contains(p, "one an archive") }) {
		t.Errorf("problems %v", g.problems)
	}
}

// A `srcs` row: the module is still built from the tree, without the listed files (both the plain srcs and target-block ones), and links its provider first; a `none` row links nothing. A listed file that is no source of the module fails the conversion.
func TestPartialModule(t *testing.T) {
	files := map[string]string{"p/Android.bp": `
cc_library_shared { name: "librt", srcs: ["keep.cc", "drop.cc"], target: { android: { srcs: ["dev.cc"] } } }
cc_library_static { name: "libsk", srcs: ["codec.cc", "raw.cc"] }
cc_library_shared { name: "libunbuilt", srcs: ["u.cc"] }
`, "p/u.cc": ""}
	lib := "/crates/lib/libatl_stubs.so"
	g := graphFor(t, files)
	g.partial = map[string]Partial{
		"librt":      {Srcs: []string{"drop.cc", "dev.cc"}, Provider: &Provided{Libs: []string{lib}}},
		"libsk":      {Srcs: []string{"raw.cc"}},
		"libunbuilt": {Srcs: []string{"u.cc"}},
	}
	for _, goal := range []string{"librt", "libsk"} {
		g.node(goal, Device, Shared, "goal")
	}
	if err := g.checkPartial(); err != nil {
		t.Fatal(err)
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	for _, src := range []string{"/p/drop.cc", "/p/dev.cc", "/p/raw.cc"} {
		if strings.Contains(ninja, src) {
			t.Errorf("%s is compiled", src)
		}
	}
	for _, src := range []string{"/p/keep.cc", "/p/codec.cc"} {
		if !strings.Contains(ninja, src) {
			t.Errorf("%s is not compiled", src)
		}
	}
	for _, n := range g.order {
		l, _ := g.linkLibs(n)
		switch n.name {
		case "librt":
			if l[0] != lib || !slices.Contains(l, "-Wl,-rpath,/crates/lib") {
				t.Errorf("librt link libs %q", l)
			}
		case "libsk":
			if slices.ContainsFunc(l, func(s string) bool { return strings.HasSuffix(s, ".so") }) {
				t.Errorf("none row links a provider: %q", l)
			}
		}
	}

	// a file the module does not compile (built), a file that does not exist (not built)
	g = graphFor(t, files)
	g.partial = map[string]Partial{
		"librt":      {Srcs: []string{"drop.cc", "gone.cc"}},
		"libunbuilt": {Srcs: []string{"moved.cc"}},
	}
	g.node("librt", Device, Shared, "goal")
	err := g.checkPartial()
	if err == nil || !strings.Contains(err.Error(), "librt: gone.cc is not one of its sources") || !strings.Contains(err.Error(), "libunbuilt: p/moved.cc does not exist") {
		t.Errorf("checkPartial: %v", err)
	}
}

// Soong's implicit system_shared_libs and atl-bionic standing in for libc (the providers.toml rows libc = crate:atl-bionic, libm / libdl = none).
func TestSystemSharedLibsAndProvidedLibc(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_library { name: "libc", srcs: ["c.cc"] }
cc_library { name: "libm", srcs: ["m.cc"] }
cc_library_shared { name: "libbase", srcs: ["b.cc"] }
cc_library_shared { name: "liba", srcs: ["a.cc"], shared_libs: ["libbase"] }
cc_library_static { name: "libnolibc", srcs: ["n.cc"], system_shared_libs: [] }
cc_library_shared { name: "libuser", srcs: ["u.cc"], static_libs: ["libnolibc"], whole_static_libs: ["libc"] }
cc_binary_host { name: "tool", srcs: ["t.cc"] }
`})
	// bionic.go's default and the providers.toml rows
	g.soong = SoongConfig{CStd: "gnu11", CppStd: "gnu++20", APILevel: "-D__ANDROID_API__=10000", SystemSharedLibs: []string{"libc", "libm", "libdl"}}
	g.omit = map[string]bool{"libm": true, "libdl": true}
	g.provided = map[string]Provided{"libc": {Libs: []string{"/c/lib/libatl_bionic.so"}, Include: []string{"/c/include"}}}
	for _, goal := range []string{"liba", "libuser", "libnolibc"} {
		g.node(goal, Device, Shared, "goal")
	}
	g.node("tool", Host, Binary, "goal")

	name := func(list []*Node) []string {
		var out []string
		for _, n := range list {
			out = append(out, n.name)
		}
		return strings.Fields(strings.Join(out, " "))
	}
	a := g.nodes[key{"liba", Device, Shared}]
	// the provided libc first, from system_shared_libs; libm / libdl dropped; no node of AOSP's libc
	if got := name(a.deps.shared); strings.Join(got, " ") != "libc libbase" || !a.deps.shared[0].provided {
		t.Errorf("liba shared %q", got)
	}
	if _, ok := g.nodes[key{"libc", Device, Shared}]; !ok || g.nodes[key{"libc", Device, Static}] != nil {
		t.Error("libc: want only the provided node")
	}
	// `[]` declines; a provided shared object asked for as a static library still links shared
	if got := name(g.nodes[key{"libnolibc", Device, Static}].deps.shared); len(got) != 0 {
		t.Errorf("libnolibc shared %q", got)
	}
	u := g.nodes[key{"libuser", Device, Shared}]
	if len(u.deps.whole) != 0 || name(u.deps.shared)[0] != "libc" {
		t.Errorf("libuser whole %q shared %q", name(u.deps.whole), name(u.deps.shared))
	}
	// system_shared_libs is device only: host tools link the host's C library
	if got := name(g.nodes[key{"tool", Host, Binary}].deps.shared); len(got) != 0 {
		t.Errorf("tool shared %q", got)
	}

	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	line := func(prefix string) string {
		for _, l := range strings.Split(ninja, "\n") {
			if strings.Contains(l, prefix) {
				return l
			}
		}
		t.Fatalf("no line with %q in\n%s", prefix, ninja)
		return ""
	}
	flagsOf := func(obj string) string {
		lines := strings.Split(ninja, "\n")
		for i, l := range lines {
			if strings.HasPrefix(l, "build ") && strings.Contains(l, obj) {
				return lines[i+2]
			}
		}
		t.Fatalf("no compile of %s", obj)
		return ""
	}
	// the provided libc leads the link, so it leads DT_NEEDED
	if l := line("liba.so: link"); !strings.Contains(l, "/c/lib/libatl_bionic.so") {
		t.Errorf("link inputs %q", l)
	}
	libs := ninja[strings.Index(ninja, "liba.so: link"):]
	libs = libs[strings.Index(libs, "libs = ")+len("libs = "):]
	if !strings.HasPrefix(libs, "/c/lib/libatl_bionic.so ") || !strings.Contains(libs, " -Wl,-rpath,/c/lib ") {
		t.Errorf("liba libs %q", libs[:strings.Index(libs, "\n")])
	}
	// bionic's headers as system headers; not where libc is declined, which still gets api-level.h, forced from compat/device (TestWriteCompat)
	if f := flagsOf("p/a.cc.o"); !strings.Contains(f, "-isystem /c/include") || !strings.Contains(f, "-include android/api-level.h") {
		t.Errorf("liba flags %q", f)
	}
	if f := flagsOf("p/n.cc.o"); !strings.Contains(f, "-include android/api-level.h") || strings.Contains(f, "/c/include") {
		t.Errorf("libnolibc flags %q", f)
	}
}

// providers.toml `defines`: the row's macros reach the module's device C and C++ compiles, after the device defines; not its host variant, not another module.
func TestModuleDefines(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_binary { name: "sm", srcs: ["a.cpp", "b.c"], host_supported: true }
cc_library { name: "libother", srcs: ["o.cc"] }
`})
	var err error
	if g.defines, err = parseDefines("sm:NO_X,sm:LEVEL=2"); err != nil {
		t.Fatal(err)
	}
	g.checkDefines()
	for _, n := range []*Node{g.node("sm", Device, Binary, "goal"), g.node("sm", Host, Binary, "goal"), g.node("libother", Device, Shared, "goal")} {
		if n == nil {
			t.Fatal(g.problems)
		}
	}
	if len(g.problems) > 0 {
		t.Fatal(g.problems)
	}
	flags := func(name string, side Side) langFlags {
		for _, n := range g.order {
			if n.name == name && n.side == side {
				return g.compileFlags(n)
			}
		}
		t.Fatalf("no node %s", name)
		return langFlags{}
	}
	dev := flags("sm", Device)
	for _, f := range [][]string{dev.c, dev.cxx} {
		i, j := slices.Index(f, androidDefine), slices.Index(f, "-DNO_X")
		if i < 0 || j < i || !slices.Contains(f, "-DLEVEL=2") {
			t.Errorf("device sm flags %q", f)
		}
	}
	for _, f := range [][]string{flags("sm", Host).cxx, flags("libother", Device).cxx} {
		if slices.Contains(f, "-DNO_X") {
			t.Errorf("flags %q carry sm's define", f)
		}
	}
}

// The rows checkDefines refuses, where the macros would reach no compile; and -defines entries that are not module:MACRO.
func TestModuleDefinesRefused(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
rust_library { name: "librs", crate_name: "rs", srcs: ["r.rs"] }
cc_library_headers { name: "libh" }
cc_binary_host { name: "tool", srcs: ["t.cc"] }
cc_library { name: "libdev", srcs: ["d.cc"], device_supported: false, host_supported: true }
`})
	g.defines = map[string][]string{"librs": {"A"}, "libh": {"A"}, "tool": {"A"}, "libdev": {"A"}}
	g.checkDefines()
	want := []string{
		"providers.toml: libdev: defines on a cc_library, which has no device variant",
		"providers.toml: libh: defines on a cc_library_headers, which compiles no C/C++",
		"providers.toml: librs: defines on a rust_library, which compiles no C/C++",
		"providers.toml: tool: defines on a cc_binary_host, which has no device variant",
	}
	if !slices.Equal(g.problems, want) {
		t.Errorf("problems %q, want %q", g.problems, want)
	}
	for _, bad := range []string{"sm", ":A", "sm:", "sm:=1"} {
		if _, err := parseDefines(bad); err == nil {
			t.Errorf("-defines %q accepted", bad)
		}
	}
}
