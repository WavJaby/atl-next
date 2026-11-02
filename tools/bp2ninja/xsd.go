// SPDX-License-Identifier: MIT

package main

import (
	"strings"
)

// xsd_config: system/tools/xsdc/build/xsdc.go (the spec), built here as the genrule its Java output amounts to (generateJavaSrcInSbox):
// `xsdc <xsd> -p <package_name> -o <dir> -j <args>` zipped by soong_zip -jar into java/<package with . -> _>_xsdcgen.srcjar. args: -w
// (gen_writer), -n (nullability), -g (gen_has), -e (enums_only), -x (parser_only), -b (boolean_getter), -t (tinyxml), -r <elem> per
// root_elements, in that order.
//   - Only the srcjar is an output: a Java module takes nothing else of the default outputs. The cpp sources and headers, the copied xsd
//     and the API check (droidstubs) are not built.

const xsdConfigType = "xsd_config"

// xsdProps: the genrule properties of an xsd_config module's Java output.
func (g *Graph) xsdProps(n *Node) *Obj {
	p := n.props
	srcs := p.strs("srcs")
	if len(srcs) != 1 {
		g.problem("%s: xsd_config must be one src", n.mod.Name)
	}
	pkg := p.str("package_name")
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
	out := "java/" + strings.ReplaceAll(pkg, ".", "_") + "_xsdcgen.srcjar"
	cmd := "rm -rf $(genDir)/xsdc && $(location xsdc) $(location " + srcs[0] + ") -p " + shellEscape(pkg) + " -o $(genDir)/xsdc -j " +
		strings.Join(args, " ") + " && $(location soong_zip) -jar -o $(out) -C $(genDir)/xsdc -D $(genDir)/xsdc"
	props := newObj()
	props.set("tools", strVals([]string{"xsdc", "soong_zip"}))
	props.set("srcs", strVals(append(append([]string{}, srcs...), p.strs("include_files")...)))
	props.set("cmd", cmd)
	props.set("out", strVals([]string{out}))
	return props
}
