// SPDX-License-Identifier: MIT

package main

import (
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// Expected values come from build/soong/rust/protobuf.go and source_provider.go: the crate root mod_<source_stem>.rs (genModFileContents), one `aprotoc --rust_out` per proto with
// protoc-gen-rust as plugin (ProtoRule, import root = the source minus its module-relative path), libprotobuf as an added rustlib (SourceProviderDeps), the exported_include_dirs of
// rust_protobuf rustlibs as -I and their crates re-exported (collectIncludedProtos); never from the code under test.
const rustProtoTree = `
cc_binary_host { name: "aprotoc", srcs: ["aprotoc.cc"] }
rust_library { name: "libprotobuf", crate_name: "protobuf", srcs: ["pb.rs"], host_supported: true }
rust_binary_host { name: "protoc-gen-rust", crate_name: "protoc_gen_rust", crate_root: "plugin.rs", rustlibs: ["libprotobuf"] }
rust_protobuf {
    name: "libbase_protos",
    crate_name: "base_protos",
    source_stem: "base_protos",
    protos: ["b/base.proto"],
    exported_include_dirs: ["b"],
}
filegroup { name: "more_protos", srcs: ["fg/sub/more.proto"], path: "fg" }
rust_protobuf {
    name: "libtop_protos",
    crate_name: "top_protos",
    source_stem: "top",
    protos: ["protos/a.proto", ":more_protos"],
    rustlibs: ["libbase_protos"],
    proto_flags: ["-I external/protobuf/src"],
}
rust_library { name: "libuser", crate_name: "user", srcs: ["u.rs"], rustlibs: ["libtop_protos"] }
rust_protobuf { name: "libdup", crate_name: "dup", source_stem: "dup", protos: ["x/a.proto", "y/a.proto"] }
rust_protobuf { name: "libnostem", crate_name: "nostem", protos: ["a.proto"] }
rust_protobuf { name: "libgrpc", crate_name: "grpc", source_stem: "grpc", grpc_protos: ["g.proto"] }
rust_library { name: "libsrcuser", crate_name: "srcuser", srcs: ["s.rs", ":libbase_protos"] }
`

func TestRustProtobuf(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": rustProtoTree})
	user := g.node("libuser", Device, Rlib, "goal")
	if user == nil || len(g.problems) > 0 {
		t.Fatal(g.problems)
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar", Rustc: "rustc", RustVersion: "1.2.3"})
	top := g.nodes[key{"libtop_protos", Device, Rlib}]
	gd := filepath.Join(g.out, "gen", "device", "libtop_protos")

	// the crate root: one module per proto in protos order (a filegroup's protos named by base name), then the rust_protobuf rustlibs re-exported
	root := filepath.Join(gd, "mod_top.rs")
	if top.rust.root != root {
		t.Errorf("crate root %s, want %s", top.rust.root, root)
	}
	if err := g.writeGenFiles(); err != nil {
		t.Fatal(err)
	}
	body, err := os.ReadFile(root)
	if err != nil {
		t.Fatal(err)
	}
	if lines := strings.Split(strings.TrimSpace(string(body)), "\n"); !slices.Equal(lines[1:], []string{"pub mod a;", "pub mod more;", "pub use base_protos::*;"}) {
		t.Errorf("mod file %q", body)
	}

	// libprotobuf added to the crates; the generated modules are implicit inputs of rustc, whose input is the root
	if got := depNames(top.deps.crates); !slices.Equal(got, []string{"libbase_protos", "libprotobuf"}) {
		t.Errorf("crates %v", got)
	}
	rustc := ninjaEdgeVars(ninja, filepath.Join(g.out, "device/libtop_protos/libtop_protos.rlib"))
	if rustc == nil || !strings.Contains(rustc[""], ": rustc "+root+" | ") || !strings.Contains(rustc[""], filepath.Join(gd, "a.rs")) || !strings.Contains(rustc[""], filepath.Join(gd, "more.rs")) {
		t.Errorf("rustc step %q", rustc[""])
	}

	// one aprotoc step per proto: import root = the module dir, or the filegroup's path; proto_flags verbatim, shell-split as Soong's command line does (ninja `$ ` = space); the base crate's exported dir as -I;
	// aprotoc and the plugin as tools
	tools := []edgeTool{{"aprotoc", g.nodes[key{"aprotoc", Host, Binary}]}, {"plugin", g.nodes[key{"protoc-gen-rust", Host, Binary}]}}
	if tools[0].node == nil || tools[1].node == nil {
		t.Fatalf("tool nodes %+v", tools)
	}
	for i, c := range []struct{ src, base, name string }{{"p/protos/a.proto", "p", "a"}, {"p/fg/sub/more.proto", "p/fg", "more"}} {
		e := top.edges[i]
		want := map[string]string{"outdir": gd, "base": g.abs(c.base), "dep": filepath.Join(gd, c.name+".d"), "flags": "-I$ external/protobuf/src", "incs": "-I" + g.abs("p/b")}
		if e.rule != "protoc_rust" || e.in != g.abs(c.src) || !slices.Equal(e.outs, []string{filepath.Join(gd, c.name+".rs")}) || !slices.Equal(e.tools, tools) {
			t.Errorf("edge %d %+v", i, e)
		}
		if got := edgeVars(e); !maps.Equal(got, want) {
			t.Errorf("edge %d vars %v, want %v", i, got, want)
		}
	}
	// without rust_protobuf rustlibs: no -I, nothing re-exported
	base := g.nodes[key{"libbase_protos", Device, Rlib}]
	if len(base.edges) != 1 || edgeVars(base.edges[0])["incs"] != "" {
		t.Errorf("base edges %+v", base.edges)
	}
}

func TestRustProtobufErrors(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": rustProtoTree})
	for name, want := range map[string]string{
		"libdup":     "proto filenames must be unique",
		"libnostem":  "source_stem property is undefined",
		"libgrpc":    "grpc_protos is unsupported",
		"libsrcuser": "source :libbase_protos is a rust_protobuf (unsupported)",
	} {
		n := g.node(name, Device, Rlib, "goal")
		if n == nil || !strings.Contains(n.rust.broken, want) {
			t.Errorf("%s broken %q, want %q", name, n.rust.broken, want)
		}
	}
	// grpc_protos alone: no proto to generate either
	if !g.hasProblem("libgrpc: at least one protobuf must be defined") {
		t.Errorf("problems %v", g.problems)
	}
}
