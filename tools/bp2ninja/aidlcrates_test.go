// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

var aidlCratesFiles = map[string]string{
	"inc/a/IFoo.aidl":   "package a;\nimport b.Baz;\n// Unused in a comment does not count\ninterface IFoo { Bar get(in P p, in b.L<Bar> l, in R r, in Map m); void set(Baz z); }",
	"inc/a/Bar.aidl":    "package a; parcelable Bar { int x; }",
	"inc/b/Baz.aidl":    "package b; enum Baz { ONE }",
	"inc/a/P.aidl":      "package a; /* parcelable Fake; */ parcelable P;",
	"inc/b/L.aidl":      "package b; parcelable L<T>;",
	"inc/a/R.aidl":      "package a; parcelable R rust_type \"x::R\";",
	"inc/a/Unused.aidl": "package a; interface Unused {}",
	"inc/c/type.aidl":   "package c.type; parcelable Q;",
	"inc/a/Outer.aidl":  "package a; parcelable Outer.Inner;",
	"inc/a/Option.aidl": "package a; parcelable Option;",
	// a parcelable with its own rust_type needs that crate: the tree's rust_library of the name (libx), or the crate nobody provides
	"inc/Android.bp": `rust_library { name: "libx", crate_name: "x", srcs: ["x.rs"] }`,
	"inc/a/IZ.aidl":  "package a; interface IZ { void f(in Z z); }",
	"inc/a/Z.aidl":   "package a; parcelable Z rust_type \"nocrate::Z\";",
}

// A frozen version (aidl_api/<iface>/<v>/, and the `current` dump) of an aidl_interface is not a source of its type names (aidl_interface.go srcsForVersion: the next version's sources are the
// module's `srcs`, a frozen version's are below aidl_api): the copy sorts before the source here, so scanning it would win the qualified name.
func TestAidlCratesSkipFrozenVersions(t *testing.T) {
	g := graphFor(t, map[string]string{
		"hw/Android.bp":                                `aidl_interface { name: "hwif", srcs: ["z/h/IHw.aidl"], versions_with_info: [{ version: "1", imports: [] }] }`,
		"hw/z/h/IHw.aidl":                              "package z.h; interface IHw { void now(); }",
		"hw/aidl_api/hwif/1/z/h/IHw.aidl":              "package z.h; interface IHw { void old(); }",
		"hw/aidl_api/hwif/current/z/h/IHw.aidl":        "package z.h; interface IHw { void dump(); }",
		"hw/aidl_api/hwif/1/z/h/OnlyFrozen.aidl":       "package z.h; parcelable OnlyFrozen { int x; }",
		"hw/aidl_api/hwif/1/z/h/OnlyFrozenOpaque.aidl": "package z.h; parcelable OnlyFrozenOpaque;",
		// not an aidl_interface's tree: aidl_api there is just a directory
		"plain/Android.bp":             `filegroup { name: "f", srcs: ["**/*.aidl"] }`,
		"plain/aidl_api/p/IPlain.aidl": "package p; interface IPlain {}",
	})
	x, err := g.indexAidl([]string{"hw", "plain"})
	if err != nil {
		t.Fatal(err)
	}
	if f := x.files["z.h.IHw"]; f == nil || f.path != "hw/z/h/IHw.aidl" {
		t.Errorf("z.h.IHw resolves to %+v, want hw/z/h/IHw.aidl", f)
	}
	for _, q := range []string{"z.h.OnlyFrozen", "z.h.OnlyFrozenOpaque"} {
		if x.files[q] != nil {
			t.Errorf("%s indexed from a frozen version", q)
		}
	}
	if len(x.decls) != 0 {
		t.Errorf("unstructured declarations %v taken from a frozen version", x.decls)
	}
	if x.files["p.IPlain"] == nil {
		t.Errorf("p.IPlain: below a directory named aidl_api of a tree without an aidl_interface, still a source")
	}
}

// Types another aidl_interface owns are imported, not compiled (aidl_interface.go: `imports`, the importer's aidl reads the import's types through -p): `hwif` is stable (vintf, frozen at 1 and 2,
// so `hwif-latest` is V2), `fw` AIDL belongs to no interface, `unst` is unstable with an additional rust lib. A crate asked for a `fw` type imports hwif at V2; one asked for an `unst`
// type stands in for it (compiles its files, takes its rustlibs) and still imports hwif; one asked for a stable interface's own type is refused (an unstable crate cannot hold @VintfStability).
func TestAidlCratesImportOtherInterfaces(t *testing.T) {
	g := graphFor(t, map[string]string{
		"hw/Android.bp": `aidl_interface { name: "hwif", stability: "vintf", srcs: ["z/h/IHw.aidl", "z/h/Data.aidl"],
			versions_with_info: [{ version: "1", imports: [] }, { version: "2", imports: [] }] }`,
		"hw/z/h/IHw.aidl":                  "package z.h; @VintfStability interface IHw { Data get(); }",
		"hw/z/h/Data.aidl":                 "package z.h; @VintfStability parcelable Data { int x; }",
		"hw/aidl_api/hwif/1/z/h/IHw.aidl":  "package z.h; interface IHw {}",
		"hw/aidl_api/hwif/2/z/h/IHw.aidl":  "package z.h; interface IHw { Data get(); }",
		"hw/aidl_api/hwif/2/z/h/Data.aidl": "package z.h; parcelable Data { int x; }",
		"fw/a/IFw.aidl":                    "package a; import z.h.IHw; interface IFw { IHw get(); }",
		"un/Android.bp":                    `aidl_interface { name: "unst", unstable: true, srcs: ["x/IUnst.aidl", "x/Own.aidl"], backend: { rust: { additional_rustlibs: ["libunst_types"] } } }`,
		"un/x/IUnst.aidl":                  "package x; import a.IFw; import x.Own; interface IUnst { Own own(); IFw fw(); }",
		"un/x/Own.aidl":                    "package x; parcelable Own { int y; }",
		"aidl/Android.bp":                  `cc_binary_host { name: "aidl", srcs: ["a.cc"] }`,
	})
	cases := []struct {
		name, types          string
		srcs, imports, rusts []string
		err                  string
	}{
		{"fw", "a.IFw", []string{"fw/a/IFw.aidl"}, []string{"hwif-V2"}, nil, ""},
		{"stand", "x.IUnst", []string{"un/x/IUnst.aidl", "fw/a/IFw.aidl", "un/x/Own.aidl"}, []string{"hwif-V2"}, []string{"libunst_types"}, ""},
		{"stable", "z.h.IHw", nil, nil, nil, "owned by the stable aidl_interface hwif"},
	}
	for _, c := range cases {
		if err := g.addAidlCrates(map[string]AidlCrate{c.name: {IncludeDirs: []string{"fw", "un", "hw"}, Types: []string{c.types}}}); err != nil {
			if c.err == "" || !strings.Contains(err.Error(), c.err) || !strings.Contains(err.Error(), "device/hwif-V2-rust") {
				t.Errorf("%s: %v, want error %q naming device/hwif-V2-rust", c.name, err, c.err)
			}
			continue
		} else if c.err != "" {
			t.Errorf("%s: no error, want %q", c.name, c.err)
			continue
		}
		m := g.synth(c.name+"-rust", Device)
		if m == nil || m.aidl == nil {
			t.Errorf("%s-rust: %+v, problems %v", c.name, m, g.problems)
			continue
		}
		var paths []string
		for _, s := range m.aidl.srcs {
			paths = append(paths, s.path)
		}
		if !slices.Equal(paths, c.srcs) {
			t.Errorf("%s: compiled %v, want %v", c.name, paths, c.srcs)
		}
		var imports []string
		for _, i := range m.aidl.imports {
			imports = append(imports, strings.TrimSuffix(i, "-rust"))
		}
		if !slices.Equal(imports, c.imports) || len(m.aidl.preprocessed) != len(c.imports) {
			t.Errorf("%s: imports %v with %d preprocessed files, want %v", c.name, imports, len(m.aidl.preprocessed), c.imports)
		}
		if got := m.Props.strs("rustlibs"); !slices.Equal(got, c.rusts) {
			t.Errorf("%s: rustlibs %v, want %v", c.name, got, c.rusts)
		}
	}
}

// A code file's own unstructured declaration (IPermissionManager.aidl: `parcelable PermissionManager.PermissionState;` after its interface) is one of the crate's parcelables like a separate
// file's: the preprocessed file names it with its rust_type, the parcelables crate gives it an opaque type, and the compiled copy drops the declaration (aidl's Rust backend refuses a document
// defining one). A comment naming a parcelable is no declaration; a declaration with its own rust_type is refused; a type the code file does not own is not a crate type.
func TestAidlCratesInFileParcelable(t *testing.T) {
	files := map[string]string{
		"inc/a/IMix.aidl": "package a;\nimport a.Mix.Part;\n/* parcelable Commented; is not one */\ninterface IMix { Part get(in Part p); }\n\n/** doc */\nparcelable Mix.Part;\n",
	}
	g := graphFor(t, files)
	dir := t.TempDir()
	for _, f := range []string{"src/lib.rs", "src/codecs/mod.rs"} {
		if err := os.MkdirAll(filepath.Join(dir, filepath.Dir(f)), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(dir, f), nil, 0o644); err != nil {
			t.Fatal(err)
		}
	}
	parcelables := &AidlParcelables{Module: "libm", Crate: "m", Dir: dir, Edition: "2024", Rustlibs: []string{"libbinder_rs"}, Lints: []string{"-D", "warnings"}}
	if err := g.addAidlCrates(map[string]AidlCrate{"mix": {IncludeDirs: []string{"inc"}, Types: []string{"a.IMix"}, Parcelables: parcelables}}); err != nil {
		t.Fatal(err)
	}
	m := g.synth("mix-rust", Device)
	if m == nil || m.aidl == nil {
		t.Fatalf("mix-rust: %+v, problems %v", m, g.problems)
	}
	gen := filepath.Join(g.out, "aidl", "device", "mix", "src")
	if len(m.aidl.srcs) != 1 || m.aidl.srcs[0].path != filepath.Join(gen, "a/IMix.aidl") || m.aidl.srcs[0].root != gen {
		t.Fatalf("srcs %+v, want the copy a/IMix.aidl under %s", m.aidl.srcs, gen)
	}
	src := g.genFiles[m.aidl.srcs[0].path]
	if strings.Contains(src, "parcelable Mix.Part") || !strings.Contains(src, "/* parcelable Commented; is not one */") || !strings.Contains(src, "interface IMix { Part get(in Part p); }") {
		t.Errorf("compiled copy keeps the declaration or loses the rest:\n%s", src)
	}
	if len(src) != len(files["inc/a/IMix.aidl"]) {
		t.Errorf("compiled copy is %d bytes, the original %d: offsets moved", len(src), len(files["inc/a/IMix.aidl"]))
	}
	if pre := g.genFiles[m.aidl.preprocessed[0]]; !strings.Contains(pre, "parcelable a.Mix.Part rust_type \"m::a::Mix_Part\";") {
		t.Errorf("preprocessed lacks the in-file parcelable:\n%s", pre)
	}
	if rs := g.mods[Device]["libm-types"][0].genText; !strings.Contains(rs, `crate::opaque!(Mix_Part, "a.Mix.Part");`) {
		t.Errorf("parcelables.rs lacks the opaque in-file parcelable:\n%s", rs)
	}
	if err := g.addAidlCrates(map[string]AidlCrate{"typed": {IncludeDirs: []string{"inc"}, Types: []string{"a.Mix.Part"}}}); err == nil || !strings.Contains(err.Error(), "type a.Mix.Part") {
		t.Errorf("a parcelable named as a crate type: %v", err)
	}

	own := graphFor(t, map[string]string{"inc/a/IOwn.aidl": "package a; interface IOwn {}\nparcelable Own rust_type \"x::Own\";"})
	if err := own.addAidlCrates(map[string]AidlCrate{"own": {IncludeDirs: []string{"inc"}, Types: []string{"a.IOwn"}}}); err == nil || !strings.Contains(err.Error(), "with its own rust_type") {
		t.Errorf("in-file parcelable with its own rust_type: %v, want a refusal", err)
	}
}

// An aidl crate compiles the closure of its types over its include dirs; every unstructured parcelable without its own rust_type gets one from the parcelables crate, whose generated
// parcelables.rs takes a hand-written codec where src/codecs/<path>.rs exists and an opaque type elsewhere; bad inputs are refused.
func TestAidlCrates(t *testing.T) {
	g := graphFor(t, aidlCratesFiles)
	dir := t.TempDir()
	for _, f := range []string{"src/lib.rs", "src/codecs/a/P.rs", "src/codecs/mod.rs"} {
		if err := os.MkdirAll(filepath.Join(dir, filepath.Dir(f)), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(dir, f), nil, 0o644); err != nil {
			t.Fatal(err)
		}
	}
	parcelables := &AidlParcelables{Module: "libp", Crate: "p", Dir: dir, Edition: "2024", Rustlibs: []string{"libbinder_rs"},
		Aconfig: []string{"flags-aconfig"}, Lints: []string{"-D", "warnings"}}
	hook := &AidlReplyHook{Path: "h::reply", Module: "libh", Crate: "h", Dir: dir, Edition: "2024", Lints: []string{"-D", "warnings"}}
	if err := g.addAidlCrates(map[string]AidlCrate{"pkg": {IncludeDirs: []string{"inc"}, Types: []string{"a.IFoo"}, Parcelables: parcelables, ReplyHook: hook}}); err != nil {
		t.Fatal(err)
	}
	m := g.synth("pkg-rust", Device)
	if m == nil || m.aidl == nil {
		t.Fatalf("pkg-rust: %+v, problems %v", m, g.problems)
	}
	var paths []string
	for _, s := range m.aidl.srcs {
		if s.root != "inc" {
			t.Errorf("%s: root %s, want inc", s.path, s.root)
		}
		paths = append(paths, s.path)
	}
	if want := []string{"inc/a/IFoo.aidl", "inc/b/Baz.aidl", "inc/a/Bar.aidl"}; !slices.Equal(paths, want) {
		t.Errorf("closure %v, want %v (Unused and the parcelable files not compiled)", paths, want)
	}
	if !slices.Contains(m.Props.strs("rustlibs"), "libx") {
		t.Errorf("rustlibs %v lack libx, the crate of IFoo's `R rust_type \"x::R\"`", m.Props.strs("rustlibs"))
	}
	if !slices.Contains(m.Props.strs("rustlibs"), "libp") || !slices.Contains(m.aidl.javaTypes, "Map=p::java::util::Map") || len(m.aidl.preprocessed) != 1 || !m.aidl.rustServed {
		t.Errorf("rustlibs %v, java types %v, preprocessed %v, served %v", m.Props.strs("rustlibs"), m.aidl.javaTypes, m.aidl.preprocessed, m.aidl.rustServed)
	}
	if !slices.Contains(m.Props.strs("rustlibs"), "libh") || m.aidl.replyHook != "h::reply" || len(g.mods[Device]["libh"]) != 1 {
		t.Errorf("reply hook: rustlibs %v, path %q, module %v", m.Props.strs("rustlibs"), m.aidl.replyHook, g.mods[Device]["libh"])
	}
	pre := g.genFiles[m.aidl.preprocessed[0]]
	for _, want := range []string{"parcelable a.P rust_type \"p::a::P\";", "parcelable b.L<T> rust_type \"p::b::L\";", "parcelable c.type.Q rust_type \"p::c::r#type::Q\";", "parcelable a.Outer.Inner rust_type \"p::a::Outer_Inner\";", "parcelable a.Option rust_type \"p::a::Option_\";"} {
		if !strings.Contains(pre, want) {
			t.Errorf("preprocessed lacks %q:\n%s", want, pre)
		}
	}
	if strings.Contains(pre, "a.R") || strings.Contains(pre, "Fake") {
		t.Errorf("preprocessed declares a.R (own rust_type) or a commented-out type:\n%s", pre)
	}
	rs := g.mods[Device]["libp-types"][0].genText
	for _, want := range []string{"pub use crate::codecs::a::P::P;", `crate::opaque!(L<T>, "b.L");`, `crate::opaque!(Map, "java.util.Map");`, "pub mod r#type {", `crate::opaque!(Outer_Inner, "a.Outer.Inner");`} {
		if !strings.Contains(rs, want) {
			t.Errorf("parcelables.rs lacks %q:\n%s", want, rs)
		}
	}
	lib := g.mods[Device]["libp"][0]
	if lib.Type != "rust_library" || lib.Props.str("crate_name") != "p" || !slices.Equal(lib.Props.strs("srcs"), []string{filepath.Join(dir, "src/lib.rs"), ":libp-types"}) {
		t.Errorf("libp: %s %v", lib.Type, lib.Props)
	}
	// our crate: its lints, not capped; the generator carries the aconfig dumps
	if !slices.Equal(lib.lints, []string{"-D", "warnings"}) || !slices.Equal(g.mods[Device]["libp-types"][0].aconfig, []string{"flags-aconfig"}) {
		t.Errorf("libp lints %v, aconfig %v", lib.lints, g.mods[Device]["libp-types"][0].aconfig)
	}
	noLints := *parcelables
	noLints.Module, noLints.Lints = "libn", nil
	if err := g.addAidlCrates(map[string]AidlCrate{"n": {IncludeDirs: []string{"inc"}, Types: []string{"a.IFoo"}, Parcelables: &noLints}}); err == nil || !strings.Contains(err.Error(), "lack -D warnings") {
		t.Errorf("crate without -D warnings: %v", err)
	}

	for _, c := range []struct {
		crate AidlCrate
		want  string
	}{
		{AidlCrate{IncludeDirs: []string{"inc"}, Types: []string{"a.Missing"}}, "type a.Missing"},
		{AidlCrate{IncludeDirs: []string{"inc"}, Types: []string{"a.P"}}, "type a.P"},
		{AidlCrate{IncludeDirs: []string{"inc"}, Types: []string{"a.IZ"}}, "rust_type crate nocrate"},
		{AidlCrate{IncludeDirs: []string{"nodir"}, Types: []string{"a.IFoo"}}, "include dir nodir"},
		{AidlCrate{IncludeDirs: []string{"inc"}}, "needs include_dirs and types"},
	} {
		err := g.addAidlCrates(map[string]AidlCrate{"x": c.crate})
		if err == nil || !strings.Contains(err.Error(), c.want) {
			t.Errorf("%+v: error %v, want %q", c.crate, err, c.want)
		}
	}
	if err := g.addAidlCrates(map[string]AidlCrate{"pkg": {IncludeDirs: []string{"inc"}, Types: []string{"a.IFoo"}}}); err == nil || !strings.Contains(err.Error(), "already has a module") {
		t.Errorf("name clash: %v", err)
	}
	// a codec file for a type nothing declares, seen by a later run (a run lists a directory once)
	if err := os.WriteFile(filepath.Join(dir, "src/codecs/a/Nope.rs"), nil, 0o644); err != nil {
		t.Fatal(err)
	}
	saved := rec
	defer func() { rec = saved }()
	rec = newRecorder()
	other := *parcelables
	other.Module = "libq"
	err := g.addAidlCrates(map[string]AidlCrate{"y": {IncludeDirs: []string{"inc"}, Types: []string{"a.IFoo"}, Parcelables: &other}})
	if err == nil || !strings.Contains(err.Error(), "src/codecs/a/Nope.rs names no") {
		t.Errorf("stray codec: %v", err)
	}
}
