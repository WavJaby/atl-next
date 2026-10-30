// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"slices"
	"strings"
)

// Source generators of cc modules: one table keyed by extension, mirroring the switch of build/soong/cc/gen.go genSources (spec) and
// cc/proto.go + android/proto.go. Each generator turns one source into one ninja edge and says what the node gets: compile sources,
// headers its compiles wait for, include dirs and cflags for its compiles, include dirs it exports, runtime libraries it needs.
//   - Output layout is ours, not Soong's, for aidl and bison/flex (gen/<side>/<module>/{src,include}); protobuf follows Soong's gen/<module>/proto.
//   - Not supported, reported as problems: .sysprop, .rscript, .fs (gen.go has arms for them), proto.plugin, nanopb proto types, proto.output_params.
//   - bison/flex take no module flags (yacc.flags, lex.flags, gen_location_hh are ignored), as before this table existed.
//   - Compiles wait on generated headers only; a generated source is a plain input of its own compile (Soong: aidlOrderOnlyDeps, protoOrderOnlyDeps).

// srcGen is one generator.
type srcGen struct {
	// tool: host cc binary (module name, also the ninja variable holding its path) the edge runs; "" = a system tool (bison, flex).
	tool string
	// export: `block`, `prop` of the bool property that hands the generated include dirs to dependents ("" = never exported).
	export [2]string
	// gen builds the edge of one source. peers: all sources of the node this generator handles (aidl roots span them).
	gen func(g *Graph, n *Node, f srcFile, peers []srcFile) genOut
}

// genEdge is one build edge of a generator, written by emit once tool paths are known.
type genEdge struct {
	rule     string
	outs     []string
	in       string
	toolVar  string // = srcGen.tool
	toolNode *Node  // nil with toolVar set: the tool is missing (problem recorded); the edge fails
	vars     [][2]string
}

// genLib is a runtime library a generator's output needs (Soong cc/proto.go protoDeps): linked, and its exports re-exported.
type genLib struct {
	name   string
	static bool
}

type genOut struct {
	edge    genEdge // rule == "": nothing generated (unsupported configuration, problem recorded)
	srcs    []string
	waits   []string
	incs    []string
	cflags  []string
	exports []string
	libs    []genLib
}

var (
	aidlGen  = &srcGen{tool: "aidl", export: [2]string{"aidl", "export_aidl_headers"}, gen: genAidl}
	yaccGen  = &srcGen{gen: genYacc}
	lexGen   = &srcGen{gen: genLex}
	protoGen = &srcGen{tool: "aprotoc", export: [2]string{"proto", "export_proto_headers"}, gen: genProto}
)

// srcGens: extension -> generator (gen.go genSources' cases).
var srcGens = map[string]*srcGen{
	".aidl": aidlGen, ".y": yaccGen, ".yy": yaccGen, ".l": lexGen, ".ll": lexGen, ".proto": protoGen,
}

func isCompileSrc(ext string) bool {
	switch ext {
	case ".c", ".cc", ".cpp", ".cxx", ".S", ".s":
		return true
	}
	return false
}

func isHeaderSrc(path string) bool {
	switch filepath.Ext(path) {
	case ".h", ".hh", ".hpp", ".hxx", ".inc", ".def":
		return true
	}
	return false
}

// genSources replaces n's generated sources by what their generators produce, in place (n.srcs keeps source order), and
// records the edges, waits, flags, exports and libraries on n. A source of no known kind is a problem.
func (g *Graph) genSources(n *Node, files []srcFile) {
	peers := map[*srcGen][]srcFile{}
	for _, f := range files {
		if sg := srcGens[filepath.Ext(f.path)]; sg != nil {
			peers[sg] = append(peers[sg], f)
		}
	}
	var libs []genLib
	for _, f := range files {
		ext := filepath.Ext(f.path)
		if isCompileSrc(ext) {
			n.srcs = append(n.srcs, f.path)
			continue
		}
		sg := srcGens[ext]
		if sg == nil {
			g.problem("%s: source %s of an unsupported kind", n.mod.Name, f.path)
			continue
		}
		r := sg.gen(g, n, f, peers[sg])
		if r.edge.rule == "" {
			continue
		}
		r.edge.toolVar = sg.tool
		if sg.tool != "" {
			r.edge.toolNode = g.hostTool(sg.tool, n)
		}
		n.edges = append(n.edges, r.edge)
		n.srcs = append(n.srcs, r.srcs...)
		n.gen = append(n.gen, r.waits...)
		n.genIncs = append(n.genIncs, r.incs...)
		n.genCflags = appendMissing(n.genCflags, r.cflags)
		if sg.export != [2]string{} && n.props.obj(sg.export[0]).boolean(sg.export[1], false) {
			n.genExports = append(n.genExports, r.exports...)
		}
		libs = append(libs, r.libs...)
	}
	for _, l := range libs {
		kind, list := Shared, &n.deps.shared
		if l.static {
			kind, list = Static, &n.deps.static
		}
		d := g.node(l.name, n.side, kind, n.mod.Name)
		if d == nil || slices.Contains(*list, d) {
			continue
		}
		*list = append(*list, d)
		n.reexport = append(n.reexport, d)
	}
}

func appendMissing(list, add []string) []string {
	for _, a := range add {
		if !slices.Contains(list, a) {
			list = append(list, a)
		}
	}
	return list
}

// hostTool: the host cc binary a generator runs, built once per graph (nil when the module is missing: the problem is recorded then).
func (g *Graph) hostTool(name string, n *Node) *Node {
	if t, ok := g.tools[name]; ok {
		return t
	}
	t := g.node(name, Host, Binary, n.mod.Name+" (sources for "+name+")")
	if g.tools == nil {
		g.tools = map[string]*Node{}
	}
	g.tools[name] = t
	return t
}

// emitEdge writes one generator edge.
func (g *Graph) emitEdge(b *strings.Builder, e genEdge) {
	fmt.Fprintf(b, "build %s: %s %s", join(e.outs), e.rule, esc(e.in))
	if e.toolVar != "" {
		tool := ""
		if e.toolNode != nil {
			tool = e.toolNode.out
		}
		fmt.Fprintf(b, " | %s\n  %s = %s", esc(tool), e.toolVar, esc(tool))
	}
	b.WriteString("\n")
	for _, v := range e.vars {
		fmt.Fprintf(b, "  %s = %s\n", v[0], v[1])
	}
}

// genAidl: x.aidl -> src/x.cpp + include/x.h, relative to the source's import root; every root of the node's aidl sources and
// aidl.local_include_dirs / include_dirs are import paths.
func genAidl(g *Graph, n *Node, f srcFile, peers []srcFile) genOut {
	rel := strings.TrimSuffix(strings.TrimPrefix(f.path, f.root+"/"), ".aidl")
	srcDir, hdrDir := g.genDir(n, "src"), g.genDir(n, "include")
	cpp, hdr := filepath.Join(srcDir, rel+".cpp"), filepath.Join(hdrDir, rel+".h")
	var roots []string
	for _, p := range peers {
		roots = appendMissing(roots, []string{p.root})
	}
	a := n.props.obj("aidl")
	for _, d := range a.strs("local_include_dirs") {
		roots = appendMissing(roots, []string{filepath.Join(n.mod.Dir, d)})
	}
	roots = appendMissing(roots, a.strs("include_dirs"))
	flags := make([]string, len(roots))
	for i, r := range roots {
		flags[i] = "-I" + g.abs(r)
	}
	return genOut{
		edge: genEdge{rule: "aidl", outs: []string{cpp, hdr}, in: g.abs(f.path), vars: [][2]string{
			{"flags", shellQuote(flags)}, {"hdir", esc(hdrDir)}, {"sdir", esc(srcDir)},
		}},
		srcs: []string{cpp}, waits: []string{hdr}, incs: []string{"-I" + hdrDir}, exports: []string{"-I" + hdrDir},
	}
}

// genYacc: x.yy -> x.cpp + x.h, x.y -> x.c + x.h (Soong's GenPathWithExt naming), under src/<path below the module dir>, which is
// on the include path (aidl's sources include "aidl_language_y.h").
func genYacc(g *Graph, n *Node, f srcFile, _ []srcFile) genOut {
	ext := filepath.Ext(f.path)
	outExt := map[string]string{".yy": ".cpp", ".y": ".c"}[ext]
	gen := g.grammarOut(n, f, outExt)
	hdr := strings.TrimSuffix(gen, outExt) + ".h"
	return genOut{
		edge: genEdge{rule: "yacc", outs: []string{gen, hdr}, in: g.abs(f.path), vars: [][2]string{{"hdr", esc(hdr)}, {"src", esc(gen)}}},
		srcs: []string{gen}, waits: []string{hdr}, incs: []string{"-I" + filepath.Dir(gen)},
	}
}

// genLex: x.ll -> x.cpp, x.l -> x.c, same directory scheme as genYacc.
func genLex(g *Graph, n *Node, f srcFile, _ []srcFile) genOut {
	outExt := map[string]string{".ll": ".cpp", ".l": ".c"}[filepath.Ext(f.path)]
	gen := g.grammarOut(n, f, outExt)
	return genOut{
		edge: genEdge{rule: "lex", outs: []string{gen}, in: g.abs(f.path)},
		srcs: []string{gen}, incs: []string{"-I" + filepath.Dir(gen)},
	}
}

func (g *Graph) grammarOut(n *Node, f srcFile, outExt string) string {
	rel := strings.TrimPrefix(f.path, n.mod.Dir+"/")
	return filepath.Join(g.genDir(n, "src"), strings.TrimSuffix(rel, filepath.Ext(rel))+outExt)
}

// genProto: x.proto -> x.pb.cc + x.pb.h under <gen>/proto, by aprotoc (cc/proto.go genProto, android/proto.go ProtoRule).
//   - proto.canonical_path_from_root (default true): the output path is the source's tree-relative path and protoc's import root is the tree
//     root; false: path below the module dir (filegroup `path`), import root that dir. A genrule-produced .proto (outside the tree) always takes
//     the second form: Soong would nest the build-dir path there.
//   - proto.type: lite (default) -> `--cpp_out=lite:`, full -> `--cpp_out=:`; unset -> `--cpp_out=:` too, with the lite library (cc/proto.go TODO
//     b/119714316, modules rely on it). The runtime library is a shared dependency, static with proto.static; sdk_version's *-ndk libraries are not used.
//   - -I flags: the import root, then proto.local_include_dirs (below the module dir), then proto.include_dirs (tree-relative).
//   - Include dirs for compiles (and for dependents with proto.export_proto_headers): <gen>/proto/<module dir> (canonical only), <gen>/proto.
//   - Not generated, problem once per module: proto.plugin, nanopb-* types.
func genProto(g *Graph, n *Node, f srcFile, peers []srcFile) genOut {
	p := n.props.obj("proto")
	typ := p.str("type")
	if plugin := p.str("plugin"); plugin != "" || typ != "" && typ != "lite" && typ != "full" {
		if f == peers[0] {
			if plugin != "" {
				g.problem("%s: proto.plugin %q is unsupported", n.mod.Name, plugin)
			} else {
				g.problem("%s: proto.type %q is unsupported", n.mod.Name, typ)
			}
		}
		return genOut{}
	}
	canonical := p.boolean("canonical_path_from_root", true)
	rel, base := f.path, g.tree
	if !canonical || filepath.IsAbs(f.path) {
		rel, base = strings.TrimPrefix(f.path, f.root+"/"), g.abs(f.root)
	}
	dir := g.genDir(n, "proto")
	stem := filepath.Join(dir, strings.TrimSuffix(rel, ".proto"))
	cc, hdr := stem+".pb.cc", stem+".pb.h"
	var flags []string
	for _, d := range p.strs("local_include_dirs") {
		flags = append(flags, "-I"+g.abs(filepath.Join(n.mod.Dir, d)))
	}
	for _, d := range p.strs("include_dirs") {
		flags = append(flags, "-I"+g.abs(d))
	}
	params, lib := "", "libprotobuf-cpp-full"
	if typ != "full" {
		lib = "libprotobuf-cpp-lite"
	}
	if typ == "lite" {
		params = "lite"
	}
	var incs []string
	if canonical {
		incs = append(incs, "-I"+filepath.Join(dir, n.mod.Dir))
	}
	incs = append(incs, "-I"+dir)
	return genOut{
		edge: genEdge{rule: "protoc", outs: []string{cc, hdr}, in: g.abs(f.path), vars: [][2]string{
			{"params", esc(params)}, {"outdir", esc(dir)}, {"base", esc(base)}, {"dep", esc(stem + ".pb.d")}, {"flags", shellQuote(flags)},
		}},
		srcs: []string{cc}, waits: []string{hdr}, incs: incs, exports: incs,
		cflags: []string{"-DGOOGLE_PROTOBUF_NO_RTTI"},
		libs:   []genLib{{lib, p.boolean("static", false)}},
	}
}
