// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"slices"
	"strings"
)

// Rust source providers, following build/soong/rust/source_provider.go (the spec): a Rust library module whose crate root a build step generates.
//   - Soong splits such a module into a source variant (the generation; BaseSourceProvider.OutputFiles, crate root first) and library variants compiling it.
//     Here the rlib node holds both: the provider puts its generation steps on n.edges (emitted with every generator's, their tools are step deps) and its outputs on rustCrate.source; rustc's root is the first output.
//     A crate whose srcs names a provider (`srcs: ["lib.rs", ":x_bindgen"]`) takes the source variant alone: a Kind Gen node, no rustc step, its outputs copied into that crate's OUT_DIR (rust.go).
//     Both nodes write the same edges, emitted once.
//   - A provider is one rustSourceProviders row: the crates it adds (SourceProviderDeps) and its generation (GenerateSource). Its module's srcs are not read.
//   - Rows: rust_protobuf[_host] (protobuf.go), rust_bindgen[_host] (bindgen.go), the aidl_interface rust backend (aidl_rust_source_provider.go; a module synthAidl makes, type aidlRustType). What a later one adds:
//     rust_aconfig_library: `aconfig create-rust-lib` over an aconfig_declarations cache (aconfig.go's graph) and its storage crates as rustlibs.

// rustSource: what a provider generated for its node.
type rustSource struct {
	outs []string // generated files, crate root first
	// rust_protobuf: exported_include_dirs (absolute), the -I paths of rust_protobuf modules naming this one in rustlibs (Soong: the source variant's FlagExporterInfo)
	protoIncludes []string
}

// rustSourceProvider is one provider module type (Soong's SourceProvider).
type rustSourceProvider interface {
	// deps: the rustlibs and proc_macros it adds to its module's (SourceProviderDeps)
	deps(n *Node) (crates, procMacros []string)
	// gen writes n's generation steps on n.edges (GenerateSource); n's cc libraries (whole_static_libs, static_libs, shared_libs) are resolved by then, in an rlib also its crates. fail marks the node broken.
	gen(g *Graph, n *Node, fail func(string, ...any)) rustSource
}

// rustSourceProviders: module type -> provider.
var rustSourceProviders = map[string]rustSourceProvider{
	"rust_protobuf": rustProtobuf{}, "rust_protobuf_host": rustProtobuf{},
	"rust_bindgen": rustBindgen{}, "rust_bindgen_host": rustBindgen{},
	aidlRustType: rustAidl{},
}

// rustProtobuf: rust_protobuf[_host].
type rustProtobuf struct{}

// rustProtobufPlugin: the protoc plugin rust_protobuf runs (protobuf.go HostToolPath "protoc-gen-rust"), a rust_binary_host of external/rust/android-crates-io/crates/protobuf-codegen built like any host tool.
const rustProtobufPlugin = "protoc-gen-rust"

func (rustProtobuf) deps(*Node) ([]string, []string) { return []string{"libprotobuf"}, nil }

// gen: protobuf.go GenerateSource. Each x.proto -> x.rs by `aprotoc --rust_out=:<dir>` with protoc-gen-rust, and the crate root mod_<source_stem>.rs (genModFileContents):
// `pub mod x;` per proto in protos order, then `pub use <crate>::*;` per rust_protobuf module of rustlibs (collectIncludedProtos), so their messages resolve from ours.
//   - Import root: the source's own (ProtoRule without CanonicalPathFromRoot: the path minus its module-relative Rel); proto_flags pass through unquoted, from the tree root as Soong runs them;
//     then -I per exported_include_dirs of the rust_protobuf modules in rustlibs (the source variant's depIncludePaths).
//   - The proto names share one output dir, so a repeated base name fails, as Soong's property error does.
//   - Not generated, reported: grpc_protos (needs grpc_rust_plugin), header_libs (no user in the tree).
func (rustProtobuf) gen(g *Graph, n *Node, fail func(string, ...any)) rustSource {
	p, m := n.props, n.mod
	for _, prop := range []string{"grpc_protos", "header_libs"} {
		if len(p.strs(prop)) > 0 {
			fail("%s is unsupported", prop)
		}
	}
	stem := p.str("source_stem")
	if stem == "" {
		fail("source_stem property is undefined but required for rust_bindgen modules")
		return rustSource{}
	}
	dir := g.genDir(n, "")
	var incs, crates []string
	for _, ref := range p.strs("rustlibs") {
		dm := g.lookup(ref, n.side)
		if dm == nil {
			continue
		}
		if _, ok := rustSourceProviders[g.baseType(dm.Type)].(rustProtobuf); !ok {
			continue
		}
		if d := g.node(ref, n.side, Rlib, m.Name); d != nil && d.rust != nil {
			crates = append(crates, d.rust.crateName)
			for _, inc := range d.rust.source.protoIncludes {
				incs = append(incs, "-I"+inc)
			}
		}
	}
	tools := []edgeTool{{"aprotoc", g.hostTool("aprotoc", n)}, {"plugin", g.hostTool(rustProtobufPlugin, n)}}
	src := rustSource{outs: []string{filepath.Join(dir, "mod_"+stem+".rs")}}
	var names []string
	for _, s := range p.strs("protos") {
		files, ok := g.resolveSrc(m.Dir, n.side, s, m.Name)
		if !ok {
			fail("protos %s unresolved", s)
		}
		for _, f := range files {
			if f.gen != nil {
				g.useGen(n, f.gen)
			}
			name := strings.TrimSuffix(filepath.Base(f.path), ".proto")
			if slices.Contains(names, name) {
				fail("proto filenames must be unique across 'protos' and 'grpc_protos' to be used in the same rust_protobuf module: %s", name)
				continue
			}
			names = append(names, name)
			out := filepath.Join(dir, name+".rs")
			n.edges = append(n.edges, genEdge{rule: "protoc_rust", outs: []string{out}, in: g.abs(f.path), tools: tools, vars: [][2]string{
				{"outdir", esc(dir)}, {"base", esc(g.abs(f.root))}, {"dep", esc(filepath.Join(dir, name+".d"))},
				{"flags", esc(strings.Join(p.strs("proto_flags"), " "))}, {"incs", shellQuote(incs)},
			}})
			src.outs = append(src.outs, out)
		}
	}
	if len(names) == 0 {
		fail("at least one protobuf must be defined in either protos or grpc_protos.")
	}
	lines := []string{fmt.Sprintf("// generated by bp2ninja from the protos of %s, do not edit", m.Name)}
	for _, name := range names {
		lines = append(lines, "pub mod "+name+";")
	}
	for _, c := range crates {
		lines = append(lines, "pub use "+c+"::*;")
	}
	g.genFile(src.outs[0], strings.Join(lines, "\n")+"\n")
	for _, d := range p.strs("exported_include_dirs") {
		src.protoIncludes = append(src.protoIncludes, g.abs(filepath.Join(m.Dir, d)))
	}
	return src
}

// rustBindgen: rust_bindgen[_host].
type rustBindgen struct{}

// rustBindgenTool: the binary rust_bindgen runs (bindgen.go bindgenCmd, HostBinToolVariable "bindgen"), the rust_binary_host of external/rust/android-crates-io/crates/bindgen-cli built like any host tool.
// Its clang-sys (feature runtime) loads libclang at run time from LIBCLANG_PATH: the cc side's clang's (Tools.LibClang), where Soong takes its own prebuilt clang's.
const rustBindgenTool = "bindgen"

// rustBionicLibs: rust/compiler.go bionicDeps, the cc libraries bindgen.go SourceProviderDeps adds on a bionic toolchain (device) unless no_stdlibs: bionic's headers reach clang through their exports.
var rustBionicLibs = []string{"liblog", "libc", "libm", "libdl"}

func (rustBindgen) deps(*Node) ([]string, []string) { return nil, nil }

// gen: bindgen.go GenerateSource. `bindgen <bindgen_flags> <flag files' words> <wrapper_src> -o <source_stem>.rs -- <clang flags>`, with clang's depfile.
//   - Clang flags: the cc compile's of the same side (compileFlags: global defines, device toolchain cflags, API level and forced bionic headers, cflags, cppflags, local_include_dirs, the exports of
//     header_libs, static_libs, shared_libs and bionicDeps, global includes), so the bindings' layouts are those of the C/C++ this build compiles; -target and -nostdlibinc are not mirrored, as cc does not.
//     Then the C or C++ -std (getStdVersion: cpp_std or c_std set, else the wrapper's extension .hpp/.hh = C++), `-x c` / `-x c++`, and Soong's two warning suppressions.
//   - The cc libraries' generated headers are waited on (depGeneratedHeaders); the depfile tracks every header read.
//   - Not supported, reported: custom_bindgen, handle_static_inline / static_inline_library (no user in the tree's native closure).
func (rustBindgen) gen(g *Graph, n *Node, fail func(string, ...any)) rustSource {
	p, m := n.props, n.mod
	if p.str("custom_bindgen") != "" {
		fail("custom_bindgen is unsupported")
	}
	if p.boolean("handle_static_inline", false) || p.str("static_inline_library") != "" {
		fail("handle_static_inline is unsupported")
	}
	stem := p.str("source_stem")
	if stem == "" {
		fail("source_stem property is undefined but required for rust_bindgen modules")
		return rustSource{}
	}
	wrapper := p.str("wrapper_src")
	if wrapper == "" || isRef(wrapper) {
		fail("wrapper_src: invalid path to wrapper source")
		return rustSource{}
	}
	for _, f := range p.strs("cflags") {
		if f == "-x c++" || f == "-xc++" {
			fail("cflags: -x c++ should not be specified in cflags; setting cpp_std specifies this is a C++ header, or change the file extension to '.hpp' or '.hh'")
		}
		if strings.HasPrefix(f, "-std=") {
			fail("cflags: -std should not be specified in cflags; instead use c_std or cpp_std")
		}
	}
	if p.str("c_std") != "" && p.str("cpp_std") != "" {
		fail("c_std: c_std and cpp_std cannot both be defined at the same time.")
	}
	for _, ref := range p.strs("header_libs") {
		if d := g.node(ref, n.side, Headers, m.Name); d != nil {
			n.deps.headers = append(n.deps.headers, d)
		}
	}
	if n.side == Device && !p.boolean("no_stdlibs", false) {
		for _, ref := range rustBionicLibs {
			if d := g.node(ref, n.side, Shared, m.Name); d != nil {
				n.deps.shared = append(n.deps.shared, d)
			}
		}
		n.deps.shared = uniqueNodes(n.deps.shared)
	}
	lang := g.compileFlags(n)
	cflags := append(slices.Clone(lang.c), "-x", "c")
	if p.has("cpp_std") || !p.has("c_std") && slices.Contains([]string{".hpp", ".hh"}, filepath.Ext(wrapper)) {
		cflags = append(slices.Clone(lang.cxx), "-x", "c++")
	}
	cflags = append(cflags, "-Wno-unused-command-line-argument", "-Wno-unknown-warning-option")
	var flagFiles []string
	for _, f := range p.strs("bindgen_flag_files") {
		flagFiles = append(flagFiles, g.abs(filepath.Join(m.Dir, f)))
	}
	var waits []string
	genClosure(n, map[*Node]bool{}, &waits)
	out := filepath.Join(g.genDir(n, ""), stem+".rs")
	n.edges = append(n.edges, genEdge{rule: "bindgen", outs: []string{out}, in: g.abs(filepath.Join(m.Dir, wrapper)), tools: []edgeTool{{"bindgen", g.hostTool(rustBindgenTool, n)}},
		implicit: flagFiles, order: waits, vars: [][2]string{
			{"flags", shellQuote(moduleFlags(p, "bindgen_flags"))}, {"flagfiles", shellQuote(flagFiles)}, {"cflags", shellQuote(cflags)},
		}})
	return rustSource{outs: []string{out}}
}

// rustAidl: an aidl_interface's `<iface>[-V<n>]-rust` (aidl_interface_backends.go addRustLibrary), a module synthAidl makes with its aidlSpec; Soong's `-rust-source` aidl_gen_rule is this node's own generation.
type rustAidl struct{}

// aidlRustType: the type of the module synthAidl makes for a rust backend (Soong's wrapLibraryFactory(aidlRustLibraryFactory), a source provider module no Android.bp can name).
const aidlRustType = "rust_aidl_interface_backend"

// deps: aidl_rust_source_provider.go SourceProviderDeps: libbinder_rs, the imports' rust backends (getImportsWithVersion), libstatic_assertions; proc macro libasync_trait.
func (rustAidl) deps(n *Node) ([]string, []string) {
	return slices.Concat([]string{"libbinder_rs"}, n.mod.aidl.imports, []string{"libstatic_assertions"}), []string{"libasync_trait"}
}

// gen: aidl_gen_rule.go (lang rust) + aidl_rust_source_provider.go GenerateSource. Each x.aidl -> <rel>.rs (rel: the path below its import root) by `aidl --lang=rust --ninja`, flags as the cpp backends (aidlSpec.flags);
// then the crate root <source_stem>.rs, aidl_rust_glue.py's layout: `pub mod aidl` mirroring the package dirs, one module per file, and `pub mod mangled` re-exporting each file's mangled names plus `pub(crate) use <import crate>::mangled::*;`.
//   - The glue script copies each file's text into its `pub mod <stem> { … }`; here the module is `#[path = "<abs>"] pub mod <stem>;`, same module contents (inner attributes included), and the root is written at conversion time, the .rs files are rustc's implicit inputs.
func (rustAidl) gen(g *Graph, n *Node, fail func(string, ...any)) rustSource {
	s := n.mod.aidl
	dir := g.genDir(n, "")
	tools := []edgeTool{{"aidl", g.hostTool("aidl", n)}}
	src := rustSource{outs: []string{filepath.Join(dir, n.props.str("source_stem")+".rs")}}
	root := &aidlRustMod{nested: map[string]*aidlRustMod{}}
	for _, f := range s.srcs {
		if f.gen != nil {
			g.useGen(n, f.gen)
		}
		rel := strings.TrimSuffix(strings.TrimPrefix(f.path, f.root+"/"), ".aidl")
		out := filepath.Join(dir, rel+".rs")
		flags, implicit := s.flags(g, f, g.abs)
		n.edges = append(n.edges, genEdge{rule: "aidl_rust", outs: []string{out}, in: g.abs(f.path), implicit: implicit, tools: tools, vars: [][2]string{
			{"flags", shellQuote(flags)}, {"outdir", esc(dir)}, {"dep", esc(out + ".d")},
		}})
		src.outs = append(src.outs, out)
		parts := strings.Split(rel, "/")
		m := root
		for _, p := range parts[:len(parts)-1] {
			m = m.child(p)
		}
		m.files = append(m.files, [2]string{parts[len(parts)-1], out})
	}
	b := &strings.Builder{}
	fmt.Fprintf(b, "// generated by bp2ninja from the aidl sources of %s, do not edit\n", n.mod.Name)
	b.WriteString("#![allow(non_snake_case)]\n#![allow(missing_docs)]\n#[deprecated(note = \"Please access via libbinder_rs binder::\")]\npub use binder;\n")
	b.WriteString("pub mod aidl {\n")
	root.emit(b, "  ")
	b.WriteString("}\npub mod mangled {\n")
	root.emitMangled(b, "super::aidl")
	for _, c := range s.mangled {
		fmt.Fprintf(b, "  pub(crate) use %s::mangled::*;\n", c)
	}
	b.WriteString("}\n")
	g.genFile(src.outs[0], b.String())
	return src
}

// aidlRustMod: one package dir of the glue's module tree (aidl_rust_glue.py RustModule): its files (stem, path) and sub-packages, both in source order.
type aidlRustMod struct {
	files  [][2]string
	nested map[string]*aidlRustMod
	order  []string
}

func (m *aidlRustMod) child(name string) *aidlRustMod {
	if c, ok := m.nested[name]; ok {
		return c
	}
	c := &aidlRustMod{nested: map[string]*aidlRustMod{}}
	m.nested[name] = c
	m.order = append(m.order, name)
	return c
}

// emit: RustModule.emit, files first, then sub-packages.
func (m *aidlRustMod) emit(b *strings.Builder, indent string) {
	for _, f := range m.files {
		// a Rust string literal: only \ and " need escaping (Go's %q writes \u escapes Rust reads differently)
		fmt.Fprintf(b, "%s#[path = \"%s\"]\n%spub mod %s;\n", indent, strings.NewReplacer(`\`, `\\`, `"`, `\"`).Replace(f[1]), indent, f[0])
	}
	for _, name := range m.order {
		fmt.Fprintf(b, "%spub mod %s {\n", indent, name)
		m.nested[name].emit(b, indent+"  ")
		fmt.Fprintf(b, "%s}\n", indent)
	}
}

// emitMangled: RustModule.emit_mangled, one flat list at the mangled module's indent.
func (m *aidlRustMod) emitMangled(b *strings.Builder, prefix string) {
	for _, f := range m.files {
		fmt.Fprintf(b, "  pub use %s::%s::mangled::*;\n", prefix, f[0])
	}
	for _, name := range m.order {
		m.nested[name].emitMangled(b, prefix+"::"+name)
	}
}
