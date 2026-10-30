// SPDX-License-Identifier: MIT

package main

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"unicode"
)

// genrule / cc_genrule, following build/soong/genrule/genrule.go (the spec) and cc/genrule.go.
//   - One node per (name, side). Outputs are `out` under g.genDir(n, ""); Soong's PathForModuleGen has no module dir in it either.
//   - The command is a bash script at <genDir>/genrule.sh (Soong's sbox also runs `bash <script>`), run from the tree root: Soong runs
//     non-sandboxed-tools genrules from the build top, and commands name tree files relative to it (--proto_path=external/perfetto).
//     A script file instead of `bash -c`: ninja variable values cannot hold newlines.
//   - No sandbox: paths in $(in) / $(location) are tree-relative for tree files (as Soong's, relative to the build top = cwd) and absolute for
//     generated files and tools, which live outside the tree; $(out) / $(genDir) are absolute. Ninja creates the output directories.
//   - Anything that cannot be built (unsupported tool, unresolved input, bad cmd) still gets its build edge, whose script prints
//     `error: ...` and fails, so consumers see a failed genrule step, not a ninja "missing and no known rule" for every compile.
//   - gensrcs: a genrule whose command runs once per input (gensrcsCommands).
//   - Not supported, reported as problems: use_nsjail, device_first_srcs / device_common_srcs / common_os_srcs, $(build_number_file).
//     `$(depfile)` is no genrule variable in Soong ("unknown variable"), so it is none here.

// genOutLimit: more outputs than this go behind one phony target for consumers (genrule.go GenerateAndroidBuildActions).
const genOutLimit = 6

// genLabel is one entry of Soong's locationLabels: what `$(location X)` / `$(locations X)` name.
type genLabel struct {
	name   string   // as written in tools / tool_files / srcs / out
	files  []string // as the shell sees them: tree-relative for tree files, absolute for generated ones
	tool   *Node    // cc tool: its path exists after emit (n.out)
	interp string   // script tool: command that runs files[0], e.g. python3
	err    string   // label that could not be resolved; using it fails the command
}

// genRule is what a genrule node builds.
type genRule struct {
	cmd      string
	hasTools bool       // tools or tool_files set: $(location) without a label needs one
	labels   []genLabel // in Soong's order: tools, tool_files, srcs, out; the first is $(location)'s default
	inputs   []string   // $(in), as labels' files
	implicit []string   // tool_files and script tools: rebuild triggers beside inputs, as labels' files
	tools    []*Node    // cc tools, implicit inputs once their paths are known
	outs     []string
	incs     []string // -I flags consumers get (export_include_dirs, default the gen dir)
	deps     []string // what consumers wait for: the outputs, or one phony behind many
	phony    string
	script   string
	broken   string // first reason the command cannot run
}

func isGenType(t string) bool {
	return t == "genrule" || t == "cc_genrule" || t == "gensrcs" || t == "java_genrule" || t == "java_genrule_host"
}

// genNode builds (once) the genrule node a reference names. A reference of another type is a problem,
// a reference no module answers to is also recorded for notFound.
func (g *Graph) genNode(ref string, side Side, from string) *Node {
	m := g.lookup(ref, side)
	if m == nil {
		// made by a module type the tree has: synth recorded why there is none
		if !g.synthOwned(ref) {
			g.problem("%s: %s module %s not found", from, side, ref)
			g.notFound(ref)
		}
		return nil
	}
	if !isGenType(g.baseType(m.Type)) {
		g.problem("%s: %s is a %s, not a genrule (unsupported)", from, m.Name, m.Type)
		return nil
	}
	k := key{m.Name, side, Gen}
	if n, ok := g.nodes[k]; ok {
		return n
	}
	n := &Node{key: k, mod: m}
	g.nodes[k] = n
	n.props = g.flatten(m, side, Gen)
	if m.hidl != nil {
		n.rule = g.hidlRule(n)
	} else {
		n.rule = g.buildGenRule(n)
	}
	g.order = append(g.order, n)
	return n
}

// newGenRule: an empty rule of n whose first failure is a problem and the reason its script fails.
func (g *Graph) newGenRule(n *Node) (*genRule, func(string, ...any)) {
	r := &genRule{script: filepath.Join(g.genDir(n, ""), "genrule.sh")}
	return r, func(format string, args ...any) {
		msg := fmt.Sprintf(format, args...)
		g.problem("%s: %s", n.mod.Name, msg)
		if r.broken == "" {
			r.broken = msg
		}
	}
}

// consumerDeps sets what consumers wait for: the outputs, or one phony behind many (genrule.go GenerateAndroidBuildActions).
func (r *genRule) consumerDeps(genDir string) {
	if len(r.outs) <= genOutLimit {
		r.deps = r.outs
		return
	}
	r.phony = filepath.Join(genDir, "genrule-phony")
	r.deps = []string{r.phony}
}

func (g *Graph) buildGenRule(n *Node) *genRule {
	m, p := n.mod, n.props
	r, fail := g.newGenRule(n)
	r.cmd = p.str("cmd")
	addLabel := func(l genLabel) {
		for _, o := range r.labels {
			if o.name == l.name {
				fail("multiple locations for label %q (do you have duplicate srcs entries?)", l.name)
				return
			}
		}
		r.labels = append(r.labels, l)
	}
	if g.baseType(m.Type) == "cc_genrule" {
		// cc/genrule.go genruleCmdModifier: shell variables, not exported ones
		osName := map[Side]string{Device: "android", Host: "linux_glibc"}[n.side]
		r.cmd = fmt.Sprintf("CC_ARCH=%s CC_NATIVE_BRIDGE= CC_MULTILIB=%s CC_OS=%s && %s", g.cfg.Arch, g.cfg.Multilib(), osName, r.cmd)
	}
	for _, unsupported := range []string{"device_first_srcs", "device_common_srcs", "common_os_srcs", "dir_srcs"} {
		if len(p.strs(unsupported)) > 0 {
			fail("%s is unsupported", unsupported)
		}
	}
	if p.boolean("use_nsjail", false) {
		fail("use_nsjail is unsupported")
	}

	tools, toolFiles := p.strs("tools"), p.strs("tool_files")
	r.hasTools = len(tools)+len(toolFiles) > 0
	for _, t := range tools {
		addLabel(g.genTool(n, r, t, fail))
	}
	for _, tf := range toolFiles {
		files, ok := g.resolveSrc(m.Dir, n.side, tf, m.Name)
		l := genLabel{name: tf}
		for _, f := range files {
			l.files = append(l.files, f.path)
			r.implicit = append(r.implicit, f.path)
		}
		if !ok {
			l.err = "tool_files " + tf + " unresolved"
			fail("%s", l.err)
		}
		addLabel(l)
	}
	exclude := map[string]bool{}
	for _, e := range g.globs(m.Dir, p.strs("exclude_srcs")) {
		exclude[e] = true
	}
	var inputs []srcFile
	for _, s := range p.strs("srcs") {
		files, ok := g.resolveSrc(m.Dir, n.side, s, m.Name)
		l := genLabel{name: s}
		for _, f := range files {
			if exclude[f.path] {
				continue
			}
			if f.gen == nil {
				if _, err := os.Stat(g.abs(f.path)); err != nil {
					fail("input %s does not exist", f.path)
				}
			}
			l.files = append(l.files, f.path)
			r.inputs = append(r.inputs, f.path)
			inputs = append(inputs, f)
		}
		if !ok {
			l.err = "srcs " + s + " unresolved"
			fail("%s", l.err)
		}
		addLabel(l)
	}
	genDir := g.genDir(n, "")
	if g.baseType(m.Type) == "gensrcs" {
		r.outs, r.cmd = gensrcsCommands(genDir, r.cmd, p.str("output_extension"), p.str("trim_extension"), inputs, fail)
	}
	seen := map[string]bool{}
	for _, o := range p.strs("out") {
		path := filepath.Join(genDir, o)
		if seen[path] {
			continue
		}
		seen[path] = true
		r.outs = append(r.outs, path)
		addLabel(genLabel{name: o, files: []string{path}})
	}
	if len(r.outs) == 0 {
		fail("must have at least one output file")
	}

	if dirs := p.strs("export_include_dirs"); len(dirs) > 0 {
		for _, d := range dirs {
			// genrule.go: exported both under the module dir and without it
			r.incs = append(r.incs, "-I"+filepath.Join(genDir, m.Dir, d), "-I"+filepath.Join(genDir, d))
		}
	} else {
		r.incs = []string{"-I" + genDir}
	}
	r.consumerDeps(genDir)
	return r
}

// genTool resolves one `tools` entry (a host module) into a location label.
func (g *Graph) genTool(n *Node, r *genRule, ref string, fail func(string, ...any)) genLabel {
	l := genLabel{name: ref}
	bad := func(format string, args ...any) genLabel {
		l.err = fmt.Sprintf(format, args...)
		fail("%s", l.err)
		return l
	}
	tm := g.lookup(ref, Host)
	if tm == nil {
		g.notFound(ref)
		return bad("host tool %s not found", ref)
	}
	bt := g.baseType(tm.Type)
	if bt == "python_binary_host" {
		file, why := pythonTool(g, tm)
		if why != "" {
			return bad("tool %s is a python_binary_host %s (unsupported)", ref, why)
		}
		if _, err := os.Stat(g.abs(file)); err != nil {
			return bad("tool %s: script %s does not exist", ref, file)
		}
		l.files, l.interp = []string{file}, "python3"
		r.implicit = append(r.implicit, file)
		return l
	}
	if k, ok := kindOf(bt, Binary); !ok || k != Binary {
		return bad("tool %s is a %s (unsupported)", ref, tm.Type)
	}
	tn := g.node(ref, Host, Binary, n.name)
	if tn == nil {
		return bad("tool %s unavailable", ref)
	}
	l.tool = tn
	r.tools = append(r.tools, tn)
	return l
}

// pythonTool: the one script a python_binary_host runs, tree-relative. Only a binary of a single source
// file without libs or data is run as is (`python3 main.py`); Soong packages the others into a par with
// their library tree, which is not rebuilt here. why says what disqualifies the module.
func pythonTool(g *Graph, m *Module) (file, why string) {
	p := g.flatten(m, Host, Gen)
	if len(p.strs("libs")) > 0 || len(p.strs("data")) > 0 {
		return "", "with libs or data"
	}
	srcs := g.globs(m.Dir, p.strs("srcs"))
	main := ""
	if mn := p.str("main"); mn != "" {
		main = filepath.Join(m.Dir, mn)
	}
	switch {
	case len(srcs) == 0 && main != "":
		return main, ""
	case len(srcs) == 1 && (main == "" || main == srcs[0]):
		return srcs[0], ""
	}
	return "", fmt.Sprintf("with %d sources", len(srcs))
}

// genExpander substitutes Soong's cmd variables (genrule.go: the Expand callback of
// generateCommonBuildActions); paths are absolute, shell-escaped like proptools.ShellEscape.
type genExpander struct {
	r      *genRule
	genDir string
}

func (e *genExpander) words(l genLabel) []string {
	if l.tool != nil {
		return []string{shellEscape(l.tool.out)}
	}
	var out []string
	for _, f := range l.files {
		w := shellEscape(f)
		if l.interp != "" {
			w = l.interp + " " + w
		}
		out = append(out, w)
	}
	return out
}

func (e *genExpander) label(name string) (genLabel, bool) {
	for _, l := range e.r.labels {
		if l.name == name {
			return l, true
		}
	}
	return genLabel{}, false
}

func (e *genExpander) expand(cmd string) (string, error) {
	var first error
	// genrule.go reportError: the module fails but expansion goes on, so one run reports every error
	report := func(format string, args ...any) (string, error) {
		if first == nil {
			first = fmt.Errorf("cmd: "+format, args...)
		}
		return "SOONG_ERROR", nil
	}
	one := func(l genLabel, what string) (string, error) {
		if l.err != "" {
			return report("%s", l.err)
		}
		w := e.words(l)
		switch {
		case len(w) == 0:
			return report("%s has no files", what)
		case len(w) > 1:
			return report("%s has multiple files, use $(locations %s) to reference it", what, l.name)
		}
		return w[0], nil
	}
	out, err := expandVars(cmd, func(name string) (string, error) {
		switch name {
		case "location":
			if !e.r.hasTools {
				return report("at least one `tools` or `tool_files` is required if $(location) is used")
			}
			l := e.r.labels[0]
			return one(l, fmt.Sprintf("default label %q", l.name))
		case "in":
			return strings.Join(mapStrings(e.r.inputs, shellEscape), " "), nil
		case "out":
			return strings.Join(mapStrings(e.r.outs, shellEscape), " "), nil
		case "genDir":
			return shellEscape(e.genDir), nil
		case "build_number_file":
			return report("$(build_number_file) is unsupported")
		}
		switch {
		case strings.HasPrefix(name, "location "):
			label := strings.TrimSpace(strings.TrimPrefix(name, "location "))
			l, ok := e.label(label)
			if !ok {
				return report("unknown location label %q is not in srcs, out, tools or tool_files.", label)
			}
			return one(l, fmt.Sprintf("label %q", label))
		case strings.HasPrefix(name, "locations "):
			label := strings.TrimSpace(strings.TrimPrefix(name, "locations "))
			l, ok := e.label(label)
			if !ok {
				return report("unknown locations label %q is not in srcs, out, tools or tool_files.", label)
			}
			if l.err != "" {
				return report("%s", l.err)
			}
			w := e.words(l)
			if len(w) == 0 {
				return report("label %q has no files", label)
			}
			return strings.Join(w, " "), nil
		}
		return report("unknown variable '$(%s)'", name)
	})
	if err != nil {
		return "", fmt.Errorf("cmd: %w", err)
	}
	return out, first
}

// expandVars is android.Expand (android/expand.go): `$(name)` through mapping (name trimmed), `$$` is `$`,
// any other `$x` an error.
func expandVars(s string, mapping func(string) (string, error)) (string, error) {
	var buf strings.Builder
	i := 0
	for j := 0; j < len(s); j++ {
		if s[j] != '$' {
			continue
		}
		if j+1 >= len(s) {
			return "", errors.New("expected character after '$'")
		}
		buf.WriteString(s[i:j])
		rest := s[j+1:]
		var val string
		var w int
		switch rest[0] {
		case '(':
			end := strings.IndexByte(rest, ')')
			if end < 0 {
				return "", errors.New("missing )")
			}
			v, err := mapping(strings.TrimSpace(rest[1:end]))
			if err != nil {
				return "", err
			}
			val, w = v, end+1
		case '$':
			val, w = "$", 1
		default:
			word := rest
			if k := strings.IndexFunc(rest, unicode.IsSpace); k == 0 {
				return "", fmt.Errorf("unexpected character '%c' after '$'", rest[0])
			} else if k > 0 {
				word = rest[:k]
			}
			return "", fmt.Errorf("expected '(' after '$', did you mean $(%s)?", word)
		}
		buf.WriteString(val)
		j += w
		i = j + 1
	}
	return buf.String() + s[i:], nil
}

// shellEscape is proptools.ShellEscape: single quotes unless every rune is a letter, digit or one of
// `_+-=.,/` or a space (a space stays unquoted there too).
func shellEscape(s string) string {
	safe := func(r rune) bool {
		return r == ' ' || r == '_' || r == '+' || r == '-' || r == '=' || r == '.' || r == ',' || r == '/' ||
			'0' <= r && r <= '9' || 'a' <= r && r <= 'z' || 'A' <= r && r <= 'Z'
	}
	if strings.IndexFunc(s, func(r rune) bool { return !safe(r) }) < 0 {
		return s
	}
	return "'" + strings.ReplaceAll(s, "'", `'"'"'`) + "'"
}

func mapStrings(in []string, f func(string) string) []string {
	out := make([]string, len(in))
	for i, s := range in {
		out[i] = f(s)
	}
	return out
}

// genScript is the script of a genrule node: its expanded command, or the failure that stops it.
func (g *Graph) genScript(n *Node) string {
	r := n.rule
	msg := r.broken
	body := ""
	if msg == "" {
		var err error
		body, err = (&genExpander{r, g.genDir(n, "")}).expand(r.cmd)
		if err != nil {
			g.problem("%s: %v", n.mod.Name, err)
			msg = err.Error()
		}
	}
	if msg != "" {
		return "echo " + shellEscape(fmt.Sprintf("error: genrule %s: %s", n.name, msg)) + " >&2\nexit 1\n"
	}
	// sbox validates the outputs after the command (validateOutputFiles); ninja would only notice a missing
	// one in a consumer
	check := "\nrc=$?\n[ $rc -eq 0 ] || exit $rc\nfor f in " + strings.Join(mapStrings(r.outs, shellEscape), " ") +
		"; do\n  [ -e \"$f\" ] || { echo \"error: genrule " + n.name + ": $f was not created\" >&2; exit 1; }\ndone\n"
	return body + check
}

// emitGen writes the build edge of one genrule node and records its script for the caller to write.
func (g *Graph) emitGen(b *strings.Builder, n *Node) {
	r := n.rule
	if g.scripts == nil {
		g.scripts = map[string]string{}
	}
	g.scripts[r.script] = g.genScript(n)
	var implicit []string
	for _, f := range append(append([]string{}, r.inputs...), r.implicit...) {
		if !filepath.IsAbs(f) {
			f = g.abs(f)
		}
		implicit = append(implicit, f)
	}
	for _, t := range r.tools {
		implicit = append(implicit, t.out)
	}
	implicit = append(implicit, r.script)
	fmt.Fprintf(b, "build %s: genrule | %s\n  name = %s\n  script = %s\n", join(r.outs), join(implicit), esc(n.name), esc(r.script))
	if r.phony != "" {
		fmt.Fprintf(b, "build %s: phony %s\n", esc(r.phony), join(r.outs))
	}
}

// gensrcsCommands is gensrcs' task generator (genrule.go NewGenSrcs): one output per input under <genDir>/gensrcs, and the command run
// once per input with $(in) / $(out) bound, each as `bash -c '…'`, joined with &&; the other variables are expanded later as a genrule's.
// Output path (paths.go genPathWithExtAndTrimExt): a tree file's tree-relative path, a generated file's path below its generator's
// directory, with trim_extension cut and output_extension appended, else the extension replaced. Not sharded: shards only split the work.
func gensrcsCommands(genDir, raw, ext, trim string, inputs []srcFile, fail func(string, ...any)) (outs []string, cmd string) {
	var cmds []string
	for _, f := range inputs {
		rel := f.path
		if f.gen != nil {
			rel = strings.TrimPrefix(f.path, f.root+"/")
		}
		switch {
		case trim != "" && ext != "":
			rel = strings.TrimSuffix(rel, trim) + "." + ext
		case trim != "":
			rel = strings.TrimSuffix(rel, trim)
		default:
			rel = strings.TrimSuffix(rel, filepath.Ext(rel)) + "." + ext
		}
		out := filepath.Join(genDir, "gensrcs", rel)
		outs = append(outs, out)
		c, err := expandVars(raw, func(name string) (string, error) {
			switch name {
			case "in":
				return shellEscape(f.path), nil
			case "out":
				return shellEscape(out), nil
			}
			return "$(" + name + ")", nil
		})
		if err != nil {
			fail("cmd: %v", err)
		}
		cmds = append(cmds, "bash -c "+shellEscape(c))
	}
	return outs, strings.Join(cmds, " && ")
}

// writeScripts writes the genrule scripts, leaving unchanged ones alone: a newer script would rerun its genrule.
func (g *Graph) writeScripts() error {
	for path, body := range g.scripts {
		if old, err := os.ReadFile(path); err == nil && string(old) == body {
			continue
		}
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			return err
		}
		if err := os.WriteFile(path, []byte(body), 0o644); err != nil {
			return err
		}
	}
	return nil
}

// useGen records that n consumes genrule gn (once): its compiles wait for the outputs and see its include
// directories (cc.go: generated_headers and generated_sources both add GeneratedDeps and header dirs).
func (g *Graph) useGen(n, gn *Node) {
	if n.usedGens[gn] {
		return
	}
	if n.usedGens == nil {
		n.usedGens = map[*Node]bool{}
	}
	n.usedGens[gn] = true
	n.gen = append(n.gen, gn.rule.deps...)
	n.genIncs = append(n.genIncs, gn.rule.incs...)
}
