// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
)

// Source generators of Java modules, following build/soong/java/gen.go genSources (spec) and java/proto.go: .aidl and .proto sources become
// srcjars, sharded 50 sources per srcjar (android.ShardPaths) under gen/<side>/<module>/{aidl,proto}/<kind><i>.srcjar; .logtags become one .java
// each (gen/logtags/<path below the module dir>.java), kept in place among the .java sources.
//   - aidl: one edge per source (`aidl --ninja -d`, so each has its own depfile), then one soong_zip -srcjar edge per shard over its
//     <kind><i>.tmp dir, zipping the shard's own outputs that exist (a parcelable declaration generates none; a whole-dir zip would keep
//     files of an earlier shard layout). Soong runs a shard's sources in one rule; the generated files are the same.
//   - proto: one edge per shard, aprotoc once per source as Soong's ProtoRule; the per-source --dependency_out files are concatenated into the
//     edge's depfile (ninja's deps = gcc takes the union of every rule in it).
//   - Host tools (aidl, aprotoc, protoc-gen-*) are cc binaries of this graph (jhost).

// javaGen: the generator of a source extension, nil for none. Each returns the java files (logtags) and srcjars it adds. A function, not a
// map: the generators reach javaSrcs again through host tools (an initialization cycle for a package variable).
func javaGen(ext string) func(g *Graph, n *jNode, files []srcFile, d javaDeps) (java, srcjars []string) {
	switch ext {
	case ".aidl":
		return (*Graph).javaAidl
	case ".logtags":
		return (*Graph).javaLogtags
	case ".proto":
		return (*Graph).javaProto
	}
	return nil
}

// genOrder: the order genSources appends generated outputs, after the module's own .srcjar sources: protos, then aidl.
var genOrder = []string{".logtags", ".proto", ".aidl"}

// jhost: the path of host binary name (cc or Rust), built once per graph (nodeOut); a missing tool is a problem and the path is one nothing builds,
// so its users fail.
func (g *Graph) jhost(name, from string) string {
	t, ok := g.tools[name]
	if !ok {
		t = g.node(name, Host, Binary, from+" (tool "+name+")")
		if g.tools == nil {
			g.tools = map[string]*Node{}
		}
		g.tools[name] = t
	}
	if t == nil {
		return filepath.Join(g.out, "missing-tool", name)
	}
	return g.nodeOut(t)
}

// shards is android.ShardPaths.
func shards[T any](list []T, size int) [][]T {
	var out [][]T
	for len(list) > size {
		out = append(out, list[:size])
		list = list[size:]
	}
	if len(list) > 0 {
		out = append(out, list)
	}
	return out
}

// genSubPath: where a source's generated file goes below a generator dir: its tree path (Soong: aidlFile.String() / protoFile.String()), a
// generated input by its path below the build dir.
func (g *Graph) genSubPath(path string) string {
	if rel, ok := strings.CutPrefix(path, g.out+"/"); ok {
		return rel
	}
	return strings.TrimPrefix(path, "/")
}

// javaAidl: base.go aidlFlags + individualAidlFlags, gen.go genAidl.
//   - -I order: deps' exported aidl dirs (none with an SDK preprocessed file, -p instead), the module's aidl.export_include_dirs, then
//     aidl.local_include_dirs, export_include_dirs again, include_dirs, the module dir, <module dir>/src when it exists; then the root of
//     each source not listed yet (genAidlIncludeFlags).
//   - -t: aidl.generate_traces, default on for system_server / core / core_platform / module / system SDKs.
//   - aidl.enforce_permissions: each source not in enforce_permissions_exceptions gets -Wmissing-permission-annotation -Werror.
func (g *Graph) javaAidl(n *jNode, files []srcFile, d javaDeps) (java, srcjars []string) {
	a := n.props.obj("aidl")
	flags := append([]string{}, a.strs("flags")...)
	var incs []string
	if d.aidlPreprocess != "" {
		flags = append(flags, "-p"+d.aidlPreprocess)
	} else {
		incs = append(incs, d.aidlIncludes...)
	}
	for _, dir := range a.strs("export_include_dirs") {
		incs = append(incs, filepath.Join(n.mod.Dir, dir))
	}
	for _, dir := range a.strs("local_include_dirs") {
		incs = append(incs, filepath.Join(n.mod.Dir, dir))
	}
	for _, dir := range a.strs("export_include_dirs") {
		incs = append(incs, filepath.Join(n.mod.Dir, dir))
	}
	incs = append(incs, a.strs("include_dirs")...)
	incs = append(incs, n.mod.Dir)
	if st, err := os.Stat(g.abs(filepath.Join(n.mod.Dir, "src"))); err == nil && st.IsDir() {
		incs = append(incs, filepath.Join(n.mod.Dir, "src"))
	}
	// tree-relative, run from the tree root as Soong's: aidl writes the source path into the generated code (UnsupportedAppUsage
	// overrideSourcePosition), so an absolute one would differ from Soong's output and leak the build machine's path
	for _, i := range incs {
		flags = append(flags, "-I"+i)
	}
	var roots []string
	for _, f := range files {
		if r := filepath.Clean(f.root); !slices.Contains(roots, r) && !slices.Contains(incs, r) {
			roots = append(roots, r)
		}
	}
	for _, r := range roots {
		flags = append(flags, "-I"+r)
	}
	kind := parseSdk(n.props.str("sdk_version")).kind
	switch kind {
	case "system_server", "core", "core_platform", "module", "system":
		if a.boolean("generate_traces", true) {
			flags = append(flags, "-t")
		}
	default:
		if a.boolean("generate_traces", false) {
			flags = append(flags, "-t")
		}
	}
	if a.boolean("generate_get_transaction_name", false) {
		flags = append(flags, "--transaction_names")
	}
	flags = append(flags, "--min_sdk_version="+g.javaMinSdk(n))
	exempt := map[string]bool{}
	if a.boolean("enforce_permissions", false) {
		for _, e := range a.strs("enforce_permissions_exceptions") {
			fs, _ := g.resolveSrc(n.mod.Dir, n.side, e, n.name)
			for _, f := range fs {
				exempt[f.path] = true
			}
		}
	}
	tool := g.jhost("aidl", n.name)
	j := g.java
	for i, shard := range shards(files, 50) {
		srcjar := filepath.Join(n.dir, "gen", "aidl", "aidl"+strconv.Itoa(i)+".srcjar")
		tmp := strings.TrimSuffix(srcjar, ".srcjar") + ".tmp"
		var outs []string
		for _, f := range shard {
			out := filepath.Join(tmp, strings.TrimSuffix(g.genSubPath(f.path), ".aidl")+".java")
			fl := flags
			if a.boolean("enforce_permissions", false) && !exempt[f.path] {
				fl = append(append([]string{}, flags...), "-Wmissing-permission-annotation", "-Werror")
			}
			var order []string
			if f.gen != nil {
				order = f.gen.rule.deps
			}
			implicit := []string{tool}
			if d.aidlPreprocess != "" {
				implicit = append(implicit, d.aidlPreprocess)
			}
			j.edge("jaidl", []string{out}, []string{g.abs(f.path)}, implicit, order, [][2]string{{"flags", shellQuote(fl)}, {"src", shellEsc(f.path)}, {"aidl", esc(tool)}})
			outs = append(outs, out)
		}
		j.edge("srcjar", []string{srcjar}, outs, nil, nil, [][2]string{{"dir", esc(tmp)}})
		srcjars = append(srcjars, srcjar)
	}
	return nil, srcjars
}

// javaMinSdk is Module.MinSdkVersion as a string: min_sdk_version, else the sdk_version's API level ("current" for unnumbered SDKs).
func (g *Graph) javaMinSdk(n *jNode) string {
	if v := n.props.str("min_sdk_version"); v != "" {
		return v
	}
	if l := parseSdk(n.props.str("sdk_version")).level; l != "" {
		return l
	}
	return "current"
}

// javaLogtags: gen.go genLogtags, `java-event-log-tags -o out in` (a python_binary_host).
func (g *Graph) javaLogtags(n *jNode, files []srcFile, _ javaDeps) (java, srcjars []string) {
	tm := g.lookup("java-event-log-tags", Host)
	var script []string
	why := "not found"
	if tm != nil {
		script, why = pythonTool(g, tm)
	}
	if why != "" {
		g.problem("%s: java-event-log-tags %s; .logtags sources not generated", n.name, why)
		return nil, nil
	}
	for _, f := range files {
		rel := strings.TrimPrefix(f.path, f.root+"/")
		out := filepath.Join(n.dir, "gen", "logtags", strings.TrimSuffix(rel, ".logtags")+".java")
		g.java.edge("logtags", []string{out}, []string{g.abs(f.path)}, absAll(g, script), nil, [][2]string{{"script", esc(g.abs(script[0]))}})
		java = append(java, out)
	}
	return java, nil
}

// javaProto: java/proto.go protoFlags + genProto, android/proto.go GetProtoFlags + ProtoRule.
//   - proto.type: lite (default) `--java_out=lite:`, full `--java_out=:`, nano / micro / stream through protoc-gen-java<type> (`--plugin=`);
//     proto.output_params append to the out params.
//   - -I: the import root (`.` = the tree with canonical_path_from_root, the default; else the source's own root), then
//     proto.local_include_dirs, proto.include_dirs.
//   - Not generated, problem once per module: proto.plugin.
func (g *Graph) javaProto(n *jNode, files []srcFile, _ javaDeps) (java, srcjars []string) {
	p := n.props.obj("proto")
	if pl := p.str("plugin"); pl != "" {
		g.problem("%s: proto.plugin %q is unsupported", n.name, pl)
		return nil, nil
	}
	var flags, implicit []string
	for _, d := range p.strs("local_include_dirs") {
		flags = append(flags, "-I"+g.abs(filepath.Join(n.mod.Dir, d)))
	}
	for _, d := range p.strs("include_dirs") {
		flags = append(flags, "-I"+g.abs(d))
	}
	outFlag, params := "--java_out", []string{}
	switch t := p.str("type"); t {
	case "stream", "micro", "nano":
		outFlag = "--java" + t + "_out"
		tool := g.jhost("protoc-gen-java"+t, n.name)
		flags = append(flags, "--plugin=protoc-gen-java"+t+"="+tool)
		implicit = append(implicit, tool)
	case "lite", "":
		params = append(params, "lite")
	case "full":
	default:
		g.problem("%s: unknown proto type %q", n.name, t)
		return nil, nil
	}
	params = append(params, p.strs("output_params")...)
	canonical := p.boolean("canonical_path_from_root", true)
	aprotoc := g.jhost("aprotoc", n.name)
	implicit = append(implicit, aprotoc)
	for i, shard := range shards(files, 50) {
		srcjar := filepath.Join(n.dir, "gen", "proto", "proto"+strconv.Itoa(i)+".srcjar")
		tmp := strings.TrimSuffix(srcjar, ".srcjar") + ".tmp"
		var cmds []string
		var ins, order []string
		for k, f := range shard {
			base := g.tree
			if !canonical || filepath.IsAbs(f.path) {
				base = g.abs(f.root)
			}
			dep := filepath.Join(tmp+".d", strconv.Itoa(k)+".d")
			cmds = append(cmds, fmt.Sprintf("%s %s=%s:%s --dependency_out=%s -I %s %s %s", shellEsc(aprotoc), outFlag, strings.Join(params, ","), shellEsc(tmp),
				shellEsc(dep), shellEsc(base), shellQuote(flags), shellEsc(g.abs(f.path))))
			ins = append(ins, g.abs(f.path))
			if f.gen != nil {
				order = append(order, f.gen.rule.deps...)
			}
		}
		g.java.edge("jproto", []string{srcjar}, ins, implicit, order, [][2]string{{"tmp", esc(tmp)}, {"cmds", strings.Join(cmds, " && ")}})
		srcjars = append(srcjars, srcjar)
	}
	return nil, srcjars
}

// shellEsc: one word for /bin/sh inside a ninja command, `$` escaped for ninja.
func shellEsc(s string) string { return shellQuote([]string{s}) }

// javaGenRules: the rules of the generators.
// jaidl: $src is $in as Soong passes it (tree-relative from the tree root); the depfile's tree-relative lines (aidl writes one path per
// `  path \` line) get the root prefixed, as ninja resolves them from the build dir.
const javaGenRules = `rule jaidl
  command = cd "$root" && $aidl --ninja -d$out.d $flags $src $out && sed -i "s|^  \([^/]\)|  $root/\1|" $out.d
  depfile = $out.d
  deps = gcc
  description = aidl $in
rule srcjar
  command = (for f in $in; do [ ! -e $$f ] || echo $$f; done) > $out.list && $soongzip -srcjar -write_if_changed -o $out -C $dir -l $out.list
  description = srcjar $out
rule logtags
  command = python3 $script -o $out $in
  description = logtags $in
rule jproto
  command = rm -rf $tmp $tmp.d && mkdir -p $tmp $tmp.d && $cmds && cat $tmp.d/*.d > $out.d && $soongzip -srcjar -write_if_changed -o $out -C $tmp -D $tmp && rm -rf $tmp $tmp.d
  depfile = $out.d
  deps = gcc
  description = protoc $out
`
