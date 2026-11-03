// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"strings"
)

// cc_preprocess_no_configuration (build/soong/cc/cc_preprocess_no_configuration.go, the spec): the C preprocessor over one source file, with no arch or OS.
// A srcs entry naming it gets its output, <gen dir>/<source base name>, which a genrule's $(in) takes like any generated file.
//   - Flags: Soong allows only -D, -P and -E (required), and adds -I of the module dir; here every flag is checked, Soong stops at -E.
//   - Compiler: clang for .c, clang++ for .cc/.cpp/.cxx/.mm (Soong's ccCmd); the cc rule with no launcher and no global cc flags, as Soong passes none.
//   - -I is absolute: the depfile must name headers as ninja finds them from the out dir, and the cc rule's cd moves only the compile.
const ccPreprocessType = "cc_preprocess_no_configuration"

// preprocessNode builds (once) the node of a cc_preprocess_no_configuration reference: one cc edge, its rule's output the node's.
// nil with a problem recorded when Soong would reject the module (the consumer then fails as for any unresolved srcs).
func (g *Graph) preprocessNode(ref string, side Side, from string) *Node {
	m := g.lookup(ref, side)
	if m == nil {
		g.problem("%s: %s module %s not found", from, side, ref)
		g.notFound(ref)
		return nil
	}
	k := key{m.Name, side, Gen}
	if n, ok := g.nodes[k]; ok {
		return n
	}
	n := &Node{key: k, mod: m}
	n.props = g.flatten(m, side, Gen)
	r := g.preprocessRule(n)
	if r == nil {
		return nil
	}
	n.rule = r
	g.nodes[k] = n
	g.order = append(g.order, n)
	return n
}

// preprocessRule is the edge of n's single source (its srcs resolve to one file); nil with a problem recorded.
func (g *Graph) preprocessRule(n *Node) *genRule {
	m, p := n.mod, n.props
	srcs := p.strs("srcs")
	if len(srcs) != 1 {
		g.problem("%s: cc_preprocess_no_configuration only accepts 1 source file, found %d", m.Name, len(srcs))
		return nil
	}
	files, ok := g.resolveSrc(m.Dir, n.side, srcs[0], m.Name)
	if !ok || len(files) != 1 {
		g.problem("%s: srcs %s must name one file, names %d", m.Name, srcs[0], len(files))
		return nil
	}
	hasE := false
	flags := []string{}
	for _, f := range p.strs("cflags") {
		switch {
		case f == "-E":
			hasE = true
		case f == "-P", strings.HasPrefix(f, "-D"):
		default:
			g.problem("%s: cc_preprocess_no_configuration only allows -D and -P flags, found %q", m.Name, f)
			return nil
		}
		flags = append(flags, f)
	}
	if !hasE {
		g.problem("%s: cc_preprocess_no_configuration must have a -E cflag", m.Name)
		return nil
	}
	var cc string
	switch filepath.Ext(files[0].path) {
	case ".c":
		cc = "$clang"
	case ".cc", ".cpp", ".cxx", ".mm":
		cc = "$cxx"
	default:
		g.problem("%s: source %s has an unknown extension (.c, .cc, .cpp, .cxx, .mm)", m.Name, files[0].path)
		return nil
	}
	src := g.abs(files[0].path)
	genDir := g.genDir(n, "")
	out := filepath.Join(genDir, filepath.Base(src))
	flags = append(flags, "-I"+g.abs(m.Dir))
	r := &genRule{inputs: []string{src}, outs: []string{out}}
	r.consumerDeps(genDir)
	r.edge = &genEdge{rule: "cc", outs: []string{out}, in: src, vars: [][2]string{
		{"cc", cc}, {"flags", shellQuote(flags)}, {"launcher", ""},
	}}
	return r
}
