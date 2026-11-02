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
	if err := g.addAidlCrates(map[string]AidlCrate{"pkg": {IncludeDirs: []string{"inc"}, Types: []string{"a.IFoo"}, Parcelables: parcelables}}); err != nil {
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
	if !slices.Contains(m.Props.strs("rustlibs"), "libp") || !slices.Contains(m.aidl.javaTypes, "Map=p::java::util::Map") || len(m.aidl.preprocessed) != 1 || !m.aidl.unimplementedHook {
		t.Errorf("rustlibs %v, java types %v, preprocessed %v, unimplemented hook %v", m.Props.strs("rustlibs"), m.aidl.javaTypes, m.aidl.preprocessed, m.aidl.unimplementedHook)
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
	// a codec file for a type nothing declares
	if err := os.WriteFile(filepath.Join(dir, "src/codecs/a/Nope.rs"), nil, 0o644); err != nil {
		t.Fatal(err)
	}
	other := *parcelables
	other.Module = "libq"
	err := g.addAidlCrates(map[string]AidlCrate{"y": {IncludeDirs: []string{"inc"}, Types: []string{"a.IFoo"}, Parcelables: &other}})
	if err == nil || !strings.Contains(err.Error(), "src/codecs/a/Nope.rs names no") {
		t.Errorf("stray codec: %v", err)
	}
}
