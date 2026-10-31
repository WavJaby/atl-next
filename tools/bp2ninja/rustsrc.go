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
//     Here one rlib node holds both: the provider puts its generation steps on n.edges (emitted with every generator's, their tools are step deps) and its outputs on rustCrate.source; rustc's root is the first output.
//   - A provider is one rustSourceProviders row: the crates it adds (SourceProviderDeps) and its generation (GenerateSource). Its module's srcs are not read.
//   - rust_protobuf[_host] (protobuf.go) is the only row. What each later one adds:
//     rust_bindgen: the bindgen host tool, clang flags and include dirs from its cc deps (depClangFlags, depIncludePaths), and `srcs: [":x_bindgen"]` references from other crates
//     (the source variant's outputs copied into OUT_DIR for include!) -- rustNode refuses a provider in srcs until then.
//     rust_aconfig_library: `aconfig create-rust-lib` over an aconfig_declarations cache (aconfig.go's graph) and its storage crates as rustlibs.
//     aidl_interface's rust backend: `aidl --lang=rust` over the interface's sources (aidliface.go), a generated mod file of the packages, libbinder_rs.

// rustSource: what a provider generated for its node.
type rustSource struct {
	outs []string // generated files, crate root first
	// rust_protobuf: exported_include_dirs (absolute), the -I paths of rust_protobuf modules naming this one in rustlibs (Soong: the source variant's FlagExporterInfo)
	protoIncludes []string
}

// rustSourceProvider is one provider module type (Soong's SourceProvider).
type rustSourceProvider interface {
	// crates: rustlibs it adds to its module's (SourceProviderDeps)
	crates() []string
	// gen writes n's generation steps on n.edges (GenerateSource); n's crates are resolved by then. fail marks the crate broken.
	gen(g *Graph, n *Node, fail func(string, ...any)) rustSource
}

// rustSourceProviders: module type -> provider.
var rustSourceProviders = map[string]rustSourceProvider{"rust_protobuf": rustProtobuf{}, "rust_protobuf_host": rustProtobuf{}}

// rustProtobuf: rust_protobuf[_host].
type rustProtobuf struct{}

// rustProtobufPlugin: the protoc plugin rust_protobuf runs (protobuf.go HostToolPath "protoc-gen-rust"), a rust_binary_host of external/rust/android-crates-io/crates/protobuf-codegen built like any host tool.
const rustProtobufPlugin = "protoc-gen-rust"

func (rustProtobuf) crates() []string { return []string{"libprotobuf"} }

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
