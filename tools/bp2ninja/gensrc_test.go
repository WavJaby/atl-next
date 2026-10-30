// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
)

// Expected values are derived by hand from build/soong/cc/gen.go (extension -> output extension, header next to the source), cc/proto.go and
// android/proto.go (proto output path, protoc flags, include dirs, runtime library), never from the code under test. Only the directory
// scheme of aidl and bison/flex outputs (<gen>/{src,include}/...) is ours; it is kept from before the generator table.

const (
	tOut  = "/o"
	tTree = "/t"
	tGen  = "/o/gen/device/m" // genDir(m): <out>/gen/<side>/<module>
)

// tableNode: module m (device) in p/q with the given proto/aidl properties, for calling the generators directly.
func tableNode(props *Obj) (*Graph, *Node) {
	g := &Graph{tree: tTree, out: tOut}
	return g, &Node{key: key{"m", Device, Shared}, mod: &Module{Name: "m", Dir: "p/q"}, props: props}
}

func objOf(kv ...any) *Obj {
	o := newObj()
	for i := 0; i < len(kv); i += 2 {
		v := kv[i+1]
		if l, ok := v.([]string); ok {
			vals := make([]Val, len(l))
			for j, s := range l {
				vals[j] = s
			}
			v = vals
		}
		o.set(kv[i].(string), v)
	}
	return o
}

func TestGrammarGenerators(t *testing.T) {
	// gen.go: .y -> .c, .yy -> .cpp, .l -> .c, .ll -> .cpp, each yacc one with a .h beside it; below <gen>/src
	// by the path under the module dir, whose directory is a compile include dir
	for _, c := range []struct {
		src, cc, hdr string // hdr "" = no header
		rule         string
	}{
		{"p/q/sub/a.yy", "sub/a.cpp", "sub/a.h", "yacc"},
		{"p/q/a.y", "a.c", "a.h", "yacc"},
		{"p/q/sub/b.ll", "sub/b.cpp", "", "lex"},
		{"p/q/b.l", "b.c", "", "lex"},
		// a filegroup file outside the module dir keeps its whole tree path
		{"other/c.yy", "other/c.cpp", "other/c.h", "yacc"},
	} {
		g, n := tableNode(newObj())
		f := srcFile{path: c.src, root: "p/q"}
		r := srcGens[filepath.Ext(c.src)].gen(g, n, f, []srcFile{f})
		cc := tGen + "/src/" + c.cc
		if r.edge.rule != c.rule || r.edge.in != tTree+"/"+c.src || !reflect.DeepEqual(r.srcs, []string{cc}) {
			t.Errorf("%s: edge %+v srcs %v", c.src, r.edge, r.srcs)
		}
		wantOuts, wantWaits := []string{cc}, []string(nil)
		if c.hdr != "" {
			wantOuts, wantWaits = append(wantOuts, tGen+"/src/"+c.hdr), []string{tGen + "/src/" + c.hdr}
		}
		if !reflect.DeepEqual(r.edge.outs, wantOuts) || !reflect.DeepEqual(r.waits, wantWaits) {
			t.Errorf("%s: outs %v waits %v, want %v %v", c.src, r.edge.outs, r.waits, wantOuts, wantWaits)
		}
		if want := []string{"-I" + filepath.Dir(cc)}; !reflect.DeepEqual(r.incs, want) {
			t.Errorf("%s: incs %v, want %v", c.src, r.incs, want)
		}
	}
}

func TestAidlGenerator(t *testing.T) {
	// aidl names its outputs by the package path below the import root; roots of all sources, then aidl.local_include_dirs (below the
	// module dir), then aidl.include_dirs (tree-relative) are the -I flags of every edge
	g, n := tableNode(objOf("aidl", objOf("local_include_dirs", []string{"loc"}, "include_dirs", []string{"inc/dir"}, "export_aidl_headers", true)))
	a := srcFile{path: "r1/android/os/IFoo.aidl", root: "r1"}
	b := srcFile{path: "r2/com/Bar.aidl", root: "r2"}
	r := aidlGen.gen(g, n, a, []srcFile{a, b})
	if want := []string{tGen + "/src/android/os/IFoo.cpp", tGen + "/include/android/os/IFoo.h"}; !reflect.DeepEqual(r.edge.outs, want) {
		t.Errorf("outs %v, want %v", r.edge.outs, want)
	}
	if want := "-I/t/r1 -I/t/r2 -I/t/p/q/loc -I/t/inc/dir"; r.edge.vars[0][0] != "flags" || r.edge.vars[0][1] != want {
		t.Errorf("flags %v, want %s", r.edge.vars[0], want)
	}
	if want := []string{"-I" + tGen + "/include"}; !reflect.DeepEqual(r.incs, want) || !reflect.DeepEqual(r.exports, want) {
		t.Errorf("incs %v exports %v, want %v", r.incs, r.exports, want)
	}
}

func TestProtoGenerator(t *testing.T) {
	type want struct {
		stem, base, params, flags string
		incs                      []string
		lib                       genLib
	}
	canon := []string{"-I" + tGen + "/proto/p/q", "-I" + tGen + "/proto"}
	for name, c := range map[string]struct {
		props *Obj
		f     srcFile
		want  want
	}{
		// canonical_path_from_root defaults to true: output below proto/ by tree path, protoc imports from the tree root (`-I .`);
		// no proto.type: `--cpp_out=:` (cc/proto.go case ""), the library is still lite (protoTypeDefault)
		"default": {newObj(), srcFile{path: "p/q/sub/a.proto", root: "p/q"},
			want{tGen + "/proto/p/q/sub/a.pb", tTree, "", "", canon, genLib{"libprotobuf-cpp-lite", false}}},
		"lite": {objOf("proto", objOf("type", "lite")), srcFile{path: "p/q/a.proto", root: "p/q"},
			want{tGen + "/proto/p/q/a.pb", tTree, "lite", "", canon, genLib{"libprotobuf-cpp-lite", false}}},
		"full static": {objOf("proto", objOf("type", "full", "static", true)), srcFile{path: "p/q/a.proto", root: "p/q"},
			want{tGen + "/proto/p/q/a.pb", tTree, "", "", canon, genLib{"libprotobuf-cpp-full", true}}},
		// false: output by path below the source's root (module dir, filegroup path), imports from that dir, no module-dir include
		"not canonical": {objOf("proto", objOf("type", "lite", "canonical_path_from_root", false)), srcFile{path: "p/q/fg/x/a.proto", root: "p/q/fg"},
			want{tGen + "/proto/x/a.pb", tTree + "/p/q/fg", "lite", "", []string{"-I" + tGen + "/proto"}, genLib{"libprotobuf-cpp-lite", false}}},
		// android/proto.go GetProtoFlags: local_include_dirs (below the module dir), then include_dirs (tree-relative)
		"include dirs": {objOf("proto", objOf("local_include_dirs", []string{"inc"}, "include_dirs", []string{"external/protobuf/src"})), srcFile{path: "p/q/a.proto", root: "p/q"},
			want{tGen + "/proto/p/q/a.pb", tTree, "", "-I/t/p/q/inc -I/t/external/protobuf/src", canon, genLib{"libprotobuf-cpp-lite", false}}},
		// a proto made by a genrule lives outside the tree: relative to its genrule dir
		"generated": {newObj(), srcFile{path: "/o/gen/device/g/x/a.proto", root: "/o/gen/device/g", gen: &Node{}},
			want{tGen + "/proto/x/a.pb", "/o/gen/device/g", "", "", canon, genLib{"libprotobuf-cpp-lite", false}}},
	} {
		g, n := tableNode(c.props)
		r := protoGen.gen(g, n, c.f, []srcFile{c.f})
		if wantOuts := []string{c.want.stem + ".cc", c.want.stem + ".h"}; !reflect.DeepEqual(r.edge.outs, wantOuts) {
			t.Errorf("%s: outs %v, want %v", name, r.edge.outs, wantOuts)
		}
		vars := map[string]string{}
		for _, v := range r.edge.vars {
			vars[v[0]] = v[1]
		}
		if r.edge.rule != "protoc" || r.edge.in != g.abs(c.f.path) || vars["base"] != c.want.base || vars["params"] != c.want.params ||
			vars["flags"] != strings.ReplaceAll(c.want.flags, "$", "$$") || vars["outdir"] != tGen+"/proto" || vars["dep"] != c.want.stem+".d" {
			t.Errorf("%s: edge %+v", name, r.edge)
		}
		if !reflect.DeepEqual(r.srcs, []string{c.want.stem + ".cc"}) || !reflect.DeepEqual(r.waits, []string{c.want.stem + ".h"}) ||
			!reflect.DeepEqual(r.incs, c.want.incs) || !reflect.DeepEqual(r.exports, c.want.incs) {
			t.Errorf("%s: srcs %v waits %v incs %v exports %v", name, r.srcs, r.waits, r.incs, r.exports)
		}
		if !reflect.DeepEqual(r.cflags, []string{"-DGOOGLE_PROTOBUF_NO_RTTI"}) || !reflect.DeepEqual(r.libs, []genLib{c.want.lib}) {
			t.Errorf("%s: cflags %v libs %v", name, r.cflags, r.libs)
		}
	}
}

func TestProtoUnsupportedReportedOnce(t *testing.T) {
	for props, msg := range map[*Obj]string{
		objOf("proto", objOf("plugin", "javastream")):            `m: proto.plugin "javastream" is unsupported`,
		objOf("proto", objOf("type", "nanopb-c")):                `m: proto.type "nanopb-c" is unsupported`,
		objOf("proto", objOf("type", "nano")):                    `m: proto.type "nano" is unsupported`,
		objOf("proto", objOf("type", "full")):                    "",
		objOf("proto", objOf("type", "lite")):                    "",
		objOf("proto", objOf("plugin", "")):                      "",
		objOf("proto", objOf("canonical_path_from_root", false)): "",
	} {
		g, n := tableNode(props)
		fs := []srcFile{{path: "p/q/a.proto", root: "p/q"}, {path: "p/q/b.proto", root: "p/q"}}
		var edges int
		for _, f := range fs {
			if protoGen.gen(g, n, f, fs).edge.rule != "" {
				edges++
			}
		}
		switch {
		case msg == "" && (edges != 2 || len(g.problems) != 0):
			t.Errorf("%v: %d edges, problems %v", props.m, edges, g.problems)
		case msg != "" && (edges != 0 || len(g.problems) != 1 || g.problems[0] != msg):
			t.Errorf("%v: %d edges, problems %v, want one %q", props.m, edges, g.problems, msg)
		}
	}
}

func TestEmitEdge(t *testing.T) {
	g := &Graph{tree: tTree, out: tOut}
	tool := &Node{out: "/o/host/aprotoc/aprotoc"}
	var b strings.Builder
	g.emitEdge(&b, genEdge{rule: "protoc", outs: []string{"/o/a.pb.cc", "/o/a.pb.h"}, in: "/t/a.proto", toolVar: "aprotoc", toolNode: tool,
		vars: [][2]string{{"params", "lite"}, {"base", "/t"}}})
	want := "build /o/a.pb.cc /o/a.pb.h: protoc /t/a.proto | /o/host/aprotoc/aprotoc\n  aprotoc = /o/host/aprotoc/aprotoc\n  params = lite\n  base = /t\n"
	if b.String() != want {
		t.Errorf("edge %q, want %q", b.String(), want)
	}
	// a system tool (bison): no tool dependency and no tool variable
	b.Reset()
	g.emitEdge(&b, genEdge{rule: "lex", outs: []string{"/o/a.c"}, in: "/t/a.l"})
	if want := "build /o/a.c: lex /t/a.l\n"; b.String() != want {
		t.Errorf("edge %q, want %q", b.String(), want)
	}
}

// the generated-source world in a fake tree: tools, the protobuf runtime and users of every generator kind
const genTree = `
cc_binary_host { name: "aidl", srcs: ["aidl.cc"] }
cc_binary_host { name: "aprotoc", srcs: ["aprotoc.cc"] }
cc_library { name: "libprotobuf-cpp-lite", srcs: ["l.cc"], export_include_dirs: ["lite/inc"] }
cc_library { name: "libprotobuf-cpp-full", srcs: ["f.cc"], export_include_dirs: ["full/inc"] }
cc_library_shared {
    name: "libp",
    srcs: ["a.proto", "sub/b.proto", "c.cc"],
    proto: { type: "full", export_proto_headers: true },
}
cc_library_static { name: "libq", srcs: ["q.proto"], proto: { static: true } }
cc_binary { name: "user", srcs: ["u.cc"], shared_libs: ["libp"] }
cc_library_static {
    name: "mixed",
    srcs: ["a.cc", "x.yy", "y.ll", "z.aidl", "b.cc", "skip.yy", "x.yy", "w.sysprop"],
    exclude_srcs: ["skip.yy"],
    aidl: { export_aidl_headers: true },
}
`

func TestGeneratedSourcesOfModules(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": genTree})
	gd := func(name string) string { return filepath.Join(g.out, "gen", "device", name) }

	// proto, full, exported: the sources are compiled in order (c.cc stays where it was written), the full runtime is a
	// shared dependency whose include dirs travel on with ours (cc/proto.go: ReexportSharedLibHeaders), aprotoc is built as a host binary
	libp := g.node("libp", Device, Shared, "t")
	wantSrcs := []string{gd("libp") + "/proto/p/a.pb.cc", gd("libp") + "/proto/p/sub/b.pb.cc", "p/c.cc"}
	if !reflect.DeepEqual(libp.srcs, wantSrcs) {
		t.Errorf("libp srcs %v, want %v", libp.srcs, wantSrcs)
	}
	if len(libp.deps.shared) != 1 || libp.deps.shared[0].name != "libprotobuf-cpp-full" || libp.deps.shared[0].side != Device {
		t.Errorf("libp shared deps %v", libp.deps.shared)
	}
	wantExports := []string{"-I" + gd("libp") + "/proto/p", "-I" + gd("libp") + "/proto", "-I" + g.abs("p/full/inc")}
	if !reflect.DeepEqual(libp.exports, wantExports) {
		t.Errorf("libp exports %v, want %v", libp.exports, wantExports)
	}
	if len(libp.edges) != 2 || libp.edges[0].toolNode == nil || libp.edges[0].toolNode != g.nodes[key{"aprotoc", Host, Binary}] {
		t.Errorf("libp edges %+v", libp.edges)
	}
	if f := g.compileFlags(libp); !strings.Contains(strings.Join(f.cxx, " "), "-DGOOGLE_PROTOBUF_NO_RTTI") {
		t.Errorf("no -DGOOGLE_PROTOBUF_NO_RTTI in %v", f.cxx)
	}
	// a dependent sees the proto headers' dirs and waits for them
	user := g.node("user", Device, Binary, "t")
	if f := g.compileFlags(user).cxx; !slices.Contains(f, "-I"+gd("libp")+"/proto") {
		t.Errorf("user does not include libp's proto dir: %v", f)
	}
	var waits []string
	genClosure(user, map[*Node]bool{}, &waits)
	if !reflect.DeepEqual(waits, []string{gd("libp") + "/proto/p/a.pb.h", gd("libp") + "/proto/p/sub/b.pb.h"}) {
		t.Errorf("user waits %v", waits)
	}

	// proto.static: the lite runtime as a static dependency; without export_proto_headers nothing of ours is exported, the runtime's dirs are
	libq := g.node("libq", Device, Static, "t")
	if len(libq.deps.static) != 1 || libq.deps.static[0].name != "libprotobuf-cpp-lite" || len(libq.deps.shared) != 0 {
		t.Errorf("libq deps static %v shared %v", libq.deps.static, libq.deps.shared)
	}
	if want := []string{"-I" + g.abs("p/lite/inc")}; !reflect.DeepEqual(libq.exports, want) {
		t.Errorf("libq exports %v, want %v", libq.exports, want)
	}

	// srcs order kept in place; exclude_srcs reaches generated sources; a repeated source is generated once; .sysprop is unsupported
	m := g.node("mixed", Device, Static, "t")
	wantSrcs = []string{"p/a.cc", gd("mixed") + "/src/x.cpp", gd("mixed") + "/src/y.cpp", gd("mixed") + "/src/z.cpp", "p/b.cc"}
	if !reflect.DeepEqual(m.srcs, wantSrcs) {
		t.Errorf("mixed srcs %v, want %v", m.srcs, wantSrcs)
	}
	var rules []string
	for _, e := range m.edges {
		rules = append(rules, e.rule)
	}
	if !reflect.DeepEqual(rules, []string{"yacc", "lex", "aidl"}) {
		t.Errorf("mixed edges %v", rules)
	}
	if want := []string{"-I" + gd("mixed") + "/include"}; !reflect.DeepEqual(m.exports, want) {
		t.Errorf("mixed exports %v, want %v", m.exports, want)
	}
	if want := "mixed: source p/w.sysprop of an unsupported kind"; !slices.Contains(g.problems, want) {
		t.Errorf("problems %v lack %q", g.problems, want)
	}
	// the tool each generator needs is one host binary node, built once
	if n := g.nodes[key{"aidl", Host, Binary}]; n == nil || m.edges[2].toolNode != n {
		t.Errorf("aidl tool node %v, edge %+v", n, m.edges[2])
	}
	if len(g.tools) != 2 {
		t.Errorf("tools %v", g.tools)
	}
}

func TestMissingGeneratorTool(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `cc_library_static { name: "l", srcs: ["a.proto", "b.proto"] }`})
	l := g.node("l", Device, Static, "t")
	// the edge stays (it fails in ninja, not as a missing rule), the missing tool is one problem and one fetchable reference
	if len(l.edges) != 2 || l.edges[0].toolNode != nil || l.edges[0].toolVar != "aprotoc" {
		t.Errorf("edges %+v", l.edges)
	}
	if got := strings.Join(g.missingRefs(), ","); got != "aprotoc,libprotobuf-cpp-lite" {
		t.Errorf("missing %q", got)
	}
	n := 0
	for _, p := range g.problems {
		if strings.Contains(p, "aprotoc") {
			n++
		}
	}
	if n != 1 {
		t.Errorf("%d problems name aprotoc: %v", n, g.problems)
	}
}
