// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"strings"
)

// xsd_config: system/tools/xsdc/build/xsdc.go (the spec). Its two rules are built here as genrules (GenerateAndroidBuildActions), each run by the host Java tool xsdc with the same args:
// -w (gen_writer), -n (nullability), -g (gen_has), -e (enums_only), -x (parser_only), -b (boolean_getter), -t (tinyxml), -r <elem> per root_elements, in that order (xsdArgs).
//   - Java (generateJavaSrcInSbox): `xsdc <xsd> -p <package_name> -o <dir> -j <args>` zipped by soong_zip -jar into java/<package with . -> _>_xsdcgen.srcjar; the module's node (genNode), what srcs name.
//   - C++ (generateCppSrcInSbox): `xsdc <xsd> -p <package_name> -o cpp -c <args>` -> cpp/<stem>.cpp, cpp/<stem>_enums.cpp, cpp/include/<stem>.h, cpp/include/<stem>_enums.h (enums_only: the _enums pair, parser_only: the other);
//     a second node of the module (xsdCppNode), what a cc module's generated_sources / generated_headers take (GeneratedSourceFiles, GeneratedDeps), with cpp/include as its header dir (GeneratedHeaderDirs).
//     It shares the module's gen dir, as Soong's two rules do; its script is cpp.sh.
//   - Not built: the copied xsd and the API check (droidstubs).

const xsdConfigType = "xsd_config"

// xsdArgs: xsdc's generator flags of an xsd_config, as GenerateAndroidBuildActions builds its args string.
func xsdArgs(p *Obj) []string {
	var args []string
	for _, f := range []struct{ prop, flag string }{{"gen_writer", "-w"}, {"nullability", "-n"}, {"gen_has", "-g"}, {"enums_only", "-e"},
		{"parser_only", "-x"}, {"boolean_getter", "-b"}, {"tinyxml", "-t"}} {
		if p.boolean(f.prop, false) {
			args = append(args, f.flag)
		}
	}
	for _, r := range p.strs("root_elements") {
		args = append(args, "-r", shellEscape(r))
	}
	return args
}

// xsdSrc: the xsd_config's one source; "" (problem recorded) when it has another count.
func (g *Graph) xsdSrc(n *Node) string {
	srcs := n.props.strs("srcs")
	if len(srcs) != 1 {
		g.problem("%s: xsd_config must be one src", n.mod.Name)
		return ""
	}
	return srcs[0]
}

// xsdGenProps: genrule properties running xsdc on the module's source with tools, out and cmd (after `$(location xsdc) $(location <xsd>) -p <pkg>`).
func (g *Graph) xsdGenProps(n *Node, tools, out []string, cmd func(src, pkg string) string) *Obj {
	p := n.props
	src := g.xsdSrc(n)
	props := newObj()
	props.set("tools", strVals(tools))
	props.set("srcs", strVals(append([]string{src}, p.strs("include_files")...)))
	props.set("cmd", cmd(src, shellEscape(p.str("package_name"))))
	props.set("out", strVals(out))
	return props
}

// xsdProps: the genrule properties of an xsd_config module's Java output.
func (g *Graph) xsdProps(n *Node) *Obj {
	out := "java/" + strings.ReplaceAll(n.props.str("package_name"), ".", "_") + "_xsdcgen.srcjar"
	return g.xsdGenProps(n, []string{"xsdc", "soong_zip"}, []string{out}, func(src, pkg string) string {
		return "rm -rf $(genDir)/xsdc && $(location xsdc) $(location " + src + ") -p " + pkg + " -o $(genDir)/xsdc -j " +
			strings.Join(xsdArgs(n.props), " ") + " && $(location soong_zip) -jar -o $(out) -C $(genDir)/xsdc -D $(genDir)/xsdc"
	})
}

// xsdCppProps: the genrule properties of an xsd_config module's C++ output.
func (g *Graph) xsdCppProps(n *Node) *Obj {
	p := n.props
	stem := strings.ReplaceAll(p.str("package_name"), ".", "_")
	parts := []string{stem, stem + "_enums"}
	switch {
	case p.boolean("enums_only", false):
		parts = parts[1:]
	case p.boolean("parser_only", false):
		parts = parts[:1]
	}
	var out []string
	for _, s := range parts {
		out = append(out, "cpp/"+s+".cpp")
	}
	for _, s := range parts {
		out = append(out, "cpp/include/"+s+".h")
	}
	props := g.xsdGenProps(n, []string{"xsdc"}, out, func(src, pkg string) string {
		return "$(location xsdc) $(location " + src + ") -p " + pkg + " -o $(genDir)/cpp -c " + strings.Join(xsdArgs(p), " ")
	})
	props.set("export_include_dirs", strVals([]string{"cpp/include"}))
	return props
}

// xsdCppNode builds (once) the C++ output node of xsd_config m on side.
func (g *Graph) xsdCppNode(m *Module, side Side) *Node {
	k := key{m.Name, side, Gen}
	if n, ok := g.xsdCpp[k]; ok {
		return n
	}
	n := &Node{key: k, mod: m}
	if g.xsdCpp == nil {
		g.xsdCpp = map[key]*Node{}
	}
	g.xsdCpp[k] = n
	n.props = g.flatten(m, side, Gen)
	n.props = g.xsdCppProps(n)
	n.rule = g.buildGenRule(n)
	// beside the Java node's genrule.sh in the shared gen dir
	n.rule.script = filepath.Join(g.genDir(n, ""), "cpp.sh")
	g.order = append(g.order, n)
	return n
}

// ccGenNode: genNode for a cc module's generated_headers / generated_sources, where an xsd_config gives its C++ output (xsdCppNode) instead of its srcjar.
func (g *Graph) ccGenNode(ref string, side Side, from string) *Node {
	if !g.omit[strings.TrimPrefix(ref, ":")] {
		if m := g.lookup(ref, side); m != nil && m.Type == xsdConfigType {
			return g.xsdCppNode(m, side)
		}
	}
	return g.genNode(ref, side, from)
}
