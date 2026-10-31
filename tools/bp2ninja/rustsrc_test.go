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
		"libdup":    "proto filenames must be unique",
		"libnostem": "source_stem property is undefined",
		"libgrpc":   "grpc_protos is unsupported",
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

// Expected values come from build/soong/rust/bindgen.go (GenerateSource: `bindgen <flags> $(cat <flag files>) <wrapper> -o <stem>.rs -- <cflags>`, cflags ending in -std, -x and the two -Wno flags;
// getStdVersion; SourceProviderDeps: header_libs / static_libs / shared_libs, bionicDeps on device) and rust.go depsToPaths + builder.go (srcs providers' outputs copied into OUT_DIR); never from the code under test.
const rustBindgenTree = `
rust_binary_host { name: "bindgen", crate_name: "bindgen", srcs: ["main.rs"] }
cc_library_headers { name: "hdrs", export_include_dirs: ["hi"], host_supported: true }
genrule { name: "gh", cmd: "touch $(out)", out: ["g.h"] }
cc_library_static { name: "libst", srcs: ["s.c"], export_include_dirs: ["si"], generated_headers: ["gh"], export_generated_headers: ["gh"], host_supported: true }
cc_library { name: "libc", srcs: ["c.c"], export_include_dirs: ["ci"] }
cc_library { name: "libm", srcs: ["m.c"] }
cc_library { name: "libdl", srcs: ["d.c"] }
cc_library { name: "liblog", srcs: ["l.c"] }
rust_bindgen {
    name: "libfoo_bindgen",
    crate_name: "foo_bindgen",
    wrapper_src: "w/wrapper.h",
    source_stem: "bindings",
    bindgen_flags: ["--allowlist-function=foo_.*", "--raw-line", "use x::Y;", "--no-layout-tests --use-core"],
    bindgen_flag_files: ["flags.txt"],
    cflags: ["-DFOO=1"],
    local_include_dirs: ["li"],
    header_libs: ["hdrs"],
    static_libs: ["libst"],
    host_supported: true,
}
rust_bindgen { name: "libcpp_bindgen", crate_name: "cpp_bindgen", wrapper_src: "w.hpp", source_stem: "cpp", cppflags: ["-DCPP=1"], no_stdlibs: true }
rust_bindgen { name: "libforced_c", crate_name: "forced_c", wrapper_src: "w.hpp", source_stem: "c", c_std: "gnu11" }
rust_bindgen { name: "libforced_cpp", crate_name: "forced_cpp", wrapper_src: "w.h", source_stem: "cpp", cpp_std: "c++17" }
rust_library { name: "libsys", crate_name: "sys", srcs: ["src/lib.rs", ":libfoo_bindgen"] }
rust_library { name: "libuser", crate_name: "user", srcs: ["u.rs"], rustlibs: ["libfoo_bindgen"] }
rust_bindgen { name: "libnostem", crate_name: "nostem", wrapper_src: "w.h" }
rust_bindgen { name: "libstd", crate_name: "std_flag", wrapper_src: "w.h", source_stem: "s", cflags: ["-std=c11"] }
rust_bindgen { name: "libinline", crate_name: "inline", wrapper_src: "w.h", source_stem: "s", handle_static_inline: true, static_inline_library: "libx" }
rust_bindgen { name: "libboth", crate_name: "both", wrapper_src: "w.h", source_stem: "s", c_std: "gnu11", cpp_std: "gnu++17" }
rust_bindgen { name: "libnowrapper", crate_name: "nowrapper", source_stem: "s" }
rust_library { name: "libbadsrc", crate_name: "badsrc", srcs: ["b.rs", ":libnostem"] }
rust_protobuf { name: "libp_protos", crate_name: "p_protos", source_stem: "p", protos: ["a.proto"] }
rust_library { name: "libprotosrc", crate_name: "protosrc", srcs: ["r.rs", ":libp_protos"] }
`

func bindgenGraph(t *testing.T) *Graph {
	t.Helper()
	g := graphFor(t, map[string]string{"b/Android.bp": rustBindgenTree})
	// Soong's CStdVersion / CppStdVersion stand-ins: getStdVersion's defaults
	g.soong.CStd, g.soong.CppStd = "gnu17", "gnu++20"
	return g
}

func TestRustBindgen(t *testing.T) {
	g := bindgenGraph(t)
	sys := g.node("libsys", Device, Rlib, "goal")
	if sys == nil || sys.rust.broken != "" || len(g.problems) > 0 {
		t.Fatal(sys.rust.broken, g.problems)
	}
	// srcs names the source variant: generation only, no rlib built
	src := g.nodes[key{"libfoo_bindgen", Device, Gen}]
	if src == nil || g.nodes[key{"libfoo_bindgen", Device, Rlib}] != nil {
		t.Fatalf("source variant %v, rlib %v", src, g.nodes[key{"libfoo_bindgen", Device, Rlib}])
	}
	gd := filepath.Join(g.out, "gen", "device", "libfoo_bindgen")
	out := filepath.Join(gd, "bindings.rs")
	if len(src.edges) != 1 {
		t.Fatalf("edges %+v", src.edges)
	}
	e := src.edges[0]
	flagFile := g.abs("b/flags.txt")
	if e.rule != "bindgen" || e.in != g.abs("b/w/wrapper.h") || !slices.Equal(e.outs, []string{out}) || !slices.Equal(e.implicit, []string{flagFile}) {
		t.Errorf("edge %+v", e)
	}
	if len(e.tools) != 1 || e.tools[0].v != "bindgen" || e.tools[0].node != g.nodes[key{"bindgen", Host, Binary}] || e.tools[0].node == nil {
		t.Errorf("tools %+v", e.tools)
	}
	// the cc library's generated header is waited on
	if !slices.Contains(e.order, filepath.Join(g.out, "gen", "device", "gh", "g.h")) {
		t.Errorf("order %v", e.order)
	}
	vars := edgeVars(e)
	// bindgen_flags: an entry with shell-unsafe characters stays one quoted word, a safe one with spaces splits (proptools.NinjaAndShellEscapeList)
	if want := "'--allowlist-function=foo_.*' --raw-line 'use x::Y;' --no-layout-tests --use-core"; vars["flags"] != want {
		t.Errorf("flags %q, want %q", vars["flags"], want)
	}
	if vars["flagfiles"] != flagFile {
		t.Errorf("flagfiles %q", vars["flagfiles"])
	}
	// C header: the C -std, -x c, then the two suppressions; the module's cflags and local_include_dirs, the exports of header_libs, static_libs and bionicDeps' libc; not the module dir
	cflags := vars["cflags"]
	if !strings.HasSuffix(cflags, " -x c -Wno-unused-command-line-argument -Wno-unknown-warning-option") || !strings.Contains(cflags, "-std=gnu17 ") || strings.Contains(cflags, "-std=gnu++20") {
		t.Errorf("cflags %q", cflags)
	}
	for _, want := range []string{"-DFOO=1", "-I" + g.abs("b/li"), "-I" + g.abs("b/hi"), "-I" + g.abs("b/si"), "-I" + g.abs("b/ci")} {
		if !slices.Contains(strings.Fields(cflags), want) {
			t.Errorf("cflags %q lack %s", cflags, want)
		}
	}
	if slices.Contains(strings.Fields(cflags), "-I"+g.abs("b")) {
		t.Errorf("cflags %q hold the module dir", cflags)
	}
	// device: bionicDeps as shared libraries beside its own
	if got := depNames(src.deps.shared); !slices.Equal(got, []string{"liblog", "libc", "libm", "libdl"}) {
		t.Errorf("shared %v", got)
	}

	// the crate: its own root, the provider's output copied into OUT_DIR
	if sys.rust.root != "b/src/lib.rs" || !slices.Equal(sys.rust.srcDeps, []string{out}) || !slices.Equal(sys.rust.providers, []*Node{src}) {
		t.Errorf("libsys root %s srcDeps %v", sys.rust.root, sys.rust.srcDeps)
	}
	user := g.node("libuser", Device, Rlib, "goal")
	lib := g.nodes[key{"libfoo_bindgen", Device, Rlib}]
	if user == nil || lib == nil || lib.rust.root != out || len(lib.deps.crates) != 0 {
		t.Fatalf("rlib variant %+v", lib)
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar", Rustc: "/tc/bin/rustc", RustVersion: "1.2.3", LibClang: "/llvm/lib"})
	if n := strings.Count(ninja, "build "+esc(out)+": bindgen "); n != 1 {
		t.Errorf("%d bindgen edges for %s, want 1 (both variants write it)", n, out)
	}
	cp := filepath.Join(sys.rust.outDir, "bindings.rs")
	if !strings.Contains(ninja, "build "+esc(cp)+": cp "+esc(out)+"\n") {
		t.Errorf("no OUT_DIR copy %s", cp)
	}
	rustc := ninjaEdgeVars(ninja, filepath.Join(g.out, "device/libsys/libsys.rlib"))
	if rustc == nil || !strings.Contains(rustc[""], ": rustc "+g.abs("b/src/lib.rs")+" | "+esc(cp)) || !strings.Contains(rustc["env"], "OUT_DIR="+sys.rust.outDir) {
		t.Errorf("rustc step %v", rustc)
	}
	// bindgen.go's rule environment: CLANG_PATH, LIBCLANG_PATH, RUSTFMT from the Rust toolchain's bin dir
	for _, want := range []string{"\nclang = cc\n", "\nlibclang = /llvm/lib\n", "\nrustfmt = /tc/bin/rustfmt\n", "CLANG_PATH=$clang LIBCLANG_PATH=$libclang RUSTFMT=$rustfmt $bindgen"} {
		if !strings.Contains(ninja, want) {
			t.Errorf("ninja lacks %q", want)
		}
	}

	// host: no bionicDeps
	if h := g.node("libfoo_bindgen", Host, Rlib, "goal"); h == nil || len(h.deps.shared) != 0 {
		t.Errorf("host variant %+v", h)
	}
}

func TestRustBindgenLanguage(t *testing.T) {
	g := bindgenGraph(t)
	for name, want := range map[string]struct{ std, x string }{
		"libcpp_bindgen": {"-std=gnu++20", "c++"}, // .hpp
		"libforced_c":    {"-std=gnu11", "c"},     // c_std wins over .hpp
		"libforced_cpp":  {"-std=c++17", "c++"},   // cpp_std wins over .h
	} {
		n := g.node(name, Device, Rlib, "goal")
		if n == nil || n.rust.broken != "" || len(n.edges) != 1 {
			t.Fatalf("%s: %+v %v", name, n, g.problems)
		}
		f := strings.Fields(edgeVars(n.edges[0])["cflags"])
		if !slices.Contains(f, want.std) || !slices.Equal(f[len(f)-4:], []string{"-x", want.x, "-Wno-unused-command-line-argument", "-Wno-unknown-warning-option"}) {
			t.Errorf("%s cflags %v", name, f)
		}
	}
	// cppflags reach a C++ header; no_stdlibs: no bionicDeps
	n := g.nodes[key{"libcpp_bindgen", Device, Rlib}]
	if !slices.Contains(strings.Fields(edgeVars(n.edges[0])["cflags"]), "-DCPP=1") || len(n.deps.shared) != 0 {
		t.Errorf("libcpp_bindgen cflags %q shared %v", edgeVars(n.edges[0])["cflags"], depNames(n.deps.shared))
	}
}

func TestRustBindgenErrors(t *testing.T) {
	g := bindgenGraph(t)
	for name, want := range map[string]string{
		"libnostem":    "source_stem property is undefined",
		"libstd":       "-std should not be specified in cflags",
		"libinline":    "handle_static_inline is unsupported",
		"libboth":      "c_std and cpp_std cannot both be defined",
		"libnowrapper": "invalid path to wrapper source",
		"libbadsrc":    "source :libnostem: source_stem property is undefined",
	} {
		n := g.node(name, Device, Rlib, "goal")
		if n == nil || !strings.Contains(n.rust.broken, want) {
			t.Errorf("%s broken %q, want %q", name, n.rust.broken, want)
		}
	}
	// a broken crate's step fails; nothing references the outputs a broken provider would have
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar", Rustc: "rustc"})
	if v := ninjaEdgeVars(ninja, filepath.Join(g.out, "device/libbadsrc/libbadsrc.rlib")); v == nil || !strings.Contains(v[""], ": fail") {
		t.Errorf("libbadsrc step %v", v)
	}
}

// Any provider in srcs takes the same route: a rust_protobuf's source variant, its root and modules copied into OUT_DIR.
func TestRustProviderInSrcs(t *testing.T) {
	g := bindgenGraph(t)
	n := g.node("libprotosrc", Device, Rlib, "goal")
	gd := filepath.Join(g.out, "gen", "device", "libp_protos")
	if n == nil || n.rust.broken != "" || !slices.Equal(n.rust.srcDeps, []string{filepath.Join(gd, "mod_p.rs"), filepath.Join(gd, "a.rs")}) {
		t.Errorf("libprotosrc %+v", n.rust)
	}
}
