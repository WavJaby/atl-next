// SPDX-License-Identifier: MIT

package main

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strconv"
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
//   - Exception: a tool whose `required` modules install data runs in a symlink sandbox with that data under tools/out, as sbox
//     lays it out (genToolData, sandboxPrelude).
//   - Anything that cannot be built (unsupported tool, unresolved input, bad cmd) still gets its build edge, whose script prints
//     `error: ...` and fails, so consumers see a failed genrule step, not a ninja "missing and no known rule" for every compile.
//   - gensrcs: a genrule whose command runs once per input, in shards of 50 inputs (gensrcsTasks).
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
	out    bool     // an `out` entry: under write_if_changed it names the sandbox copy, as $(out) does (sbox PathForOutput)
}

// genRule is what a genrule node builds.
type genRule struct {
	cmd      string
	hasTools bool       // tools or tool_files set: $(location) without a label needs one
	labels   []genLabel // in Soong's order: tools, tool_files, srcs, out; the first is $(location)'s default
	inputs   []string   // $(in), as labels' files
	implicit []string   // tool_files and script tools: rebuild triggers beside inputs, as labels' files
	tools    []*Node    // cc tools, implicit inputs once their paths are known
	objs     []*Node    // cc_object modules srcs name; their object files (objectOut) are inputs
	toolData []toolFile // files the tools' `required` modules install, staged under tools/out (genToolData)
	outs     []string
	incs     []string // -I flags consumers get (export_include_dirs, default the gen dir)
	deps     []string // what consumers wait for: the outputs, or one phony behind many
	phony    string
	script   string
	edge     *genEdge  // a generator that is one tool edge, not a script (cc_preprocess_no_configuration, preprocess.go)
	broken   string    // first reason the command cannot run
	genDir   string    // the module's gen directory
	tasks    []genTask // gensrcs' shards; nil = one task running cmd into outs (taskList)
	merge    string    // sharded gensrcs: script of the edge that copies the shards' outputs to outs (genrule.go gensrcsMerge)
	// write_if_changed (genrule.go Write_if_changed -> sbox --write-if-changed): the command writes into a sandbox and an output replaces
	// its old file only when the bytes differ, so `restat` stops consumers of an unchanged output (cmd/sbox/sbox.go moveFiles)
	writeIfChanged bool
}

// genTask is one command edge of a genrule (genrule.go generateTask): a genrule has one, a gensrcs one per shard.
type genTask struct {
	cmd    string   // before genExpander
	outs   []string // what the command writes: $(out)
	copyTo []string // sharded gensrcs: final path of each of outs, under <genDir>/gensrcs
	genDir string   // $(genDir), emptied before the command runs (sbox clearOutputDirectory)
	script string
}

// taskList: the rule's command edges; a genrule's one task runs cmd in the module's gen directory.
func (r *genRule) taskList() []genTask {
	if r.tasks != nil {
		return r.tasks
	}
	return []genTask{{cmd: r.cmd, outs: r.outs, genDir: r.genDir, script: r.script}}
}

func isGenType(t string) bool {
	return t == "genrule" || t == "cc_genrule" || t == "gensrcs" || t == "java_genrule" || t == "java_genrule_host" || t == sysFeaturesType ||
		t == xsdConfigType
}

// genNode builds (once) the genrule node a reference names. A reference of another type is a problem,
// a reference no module answers to is also recorded for notFound. A providers.toml `none` generator is dropped (nil), as node drops a library.
func (g *Graph) genNode(ref string, side Side, from string) *Node {
	if name := strings.TrimPrefix(ref, ":"); g.omit[name] {
		g.dropped = addKey(g.dropped, side.String()+"/"+from+" "+name)
		return nil
	}
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
	switch {
	case m.hidl != nil:
		n.rule = g.hidlRule(n)
	case m.aidl != nil:
		n.rule = g.aidlJavaRule(n)
	case m.Type == sysFeaturesType:
		n.props = g.sysFeaturesProps(n)
		n.rule = g.buildGenRule(n)
	case m.Type == xsdConfigType:
		n.props = g.xsdProps(n)
		n.rule = g.buildGenRule(n)
	default:
		n.rule = g.buildGenRule(n)
	}
	g.order = append(g.order, n)
	return n
}

// newGenRule: an empty rule of n whose first failure is a problem and the reason its script fails.
func (g *Graph) newGenRule(n *Node) (*genRule, func(string, ...any)) {
	r := &genRule{genDir: g.genDir(n, ""), script: filepath.Join(g.genDir(n, ""), "genrule.sh")}
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
		// the variant's arch and OS (ctx.Arch(), ctx.Os()): a build-side genrule is the build machine's
		osName := map[Side]string{Device: "android", Host: "linux_glibc", Build: "linux_glibc"}[n.side]
		arch, bits := g.cfg.Arch, g.cfg.PointerBits
		if n.side == Build {
			arch, bits = g.cfg.archOf(Build)
		}
		r.cmd = fmt.Sprintf("CC_ARCH=%s CC_NATIVE_BRIDGE= CC_MULTILIB=lib%d CC_OS=%s && %s", arch, bits, osName, r.cmd)
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
			if f.obj != nil {
				r.objs = append(r.objs, f.obj)
			} else if f.gen == nil && !strings.HasPrefix(f.path, g.out+"/") {
				// a file of the build (an app's package, a Java module's jar) is made by an edge, not in the tree yet
				if _, err := statPath(g.abs(f.path)); err != nil {
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
	r.writeIfChanged = p.boolean("write_if_changed", false)
	if g.baseType(m.Type) == "gensrcs" {
		r.outs, r.tasks = gensrcsTasks(genDir, r.cmd, p.str("output_extension"), p.str("trim_extension"), int(p.integer("shard_size", gensrcsShardSize)),
			inputs, fail)
		if len(r.tasks) > 1 {
			r.merge = filepath.Join(genDir, "genrule_merge.sh")
		}
	}
	seen := map[string]bool{}
	for _, o := range p.strs("out") {
		path := filepath.Join(genDir, o)
		if seen[path] {
			continue
		}
		seen[path] = true
		r.outs = append(r.outs, path)
		addLabel(genLabel{name: o, files: []string{path}, out: true})
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

// genTool resolves one `tools` entry (a host module, built for the build machine: toolSide) into a location label.
func (g *Graph) genTool(n *Node, r *genRule, ref string, fail func(string, ...any)) genLabel {
	l := genLabel{name: ref}
	bad := func(format string, args ...any) genLabel {
		l.err = fmt.Sprintf(format, args...)
		fail("%s", l.err)
		return l
	}
	tm := g.lookup(ref, g.cfg.toolSide())
	if tm == nil {
		g.notFound(ref)
		return bad("host tool %s not found", ref)
	}
	bt := g.baseType(tm.Type)
	if bt == "python_binary_host" {
		files, why := pythonTool(g, tm)
		if why != "" {
			return bad("tool %s is a python_binary_host %s (unsupported)", ref, why)
		}
		for _, f := range files {
			if _, err := statPath(g.abs(f)); err != nil {
				return bad("tool %s: script %s does not exist", ref, f)
			}
		}
		l.files, l.interp = files[:1], "python3"
		r.implicit = append(r.implicit, files...)
		return l
	}
	switch {
	case bt == "blueprint_go_binary" && g.java != nil:
		// J2: Soong's Go host tools (soong_zip, merge_zips, …) come prebuilt from prebuilts/build-tools, the -build-tools directory
		p := filepath.Join(g.java.tools.BuildTools, tm.Name)
		if _, err := statPath(p); err != nil {
			return bad("tool %s is a blueprint_go_binary with no prebuilt in %s", ref, g.java.tools.BuildTools)
		}
		l.files = []string{p}
		r.implicit = append(r.implicit, p)
		return l
	case bt == "java_binary_host" && g.java != nil:
		// its installed wrapper (javaBinary), which runs the jar beside it
		p := g.jtool(tm.Name, n.name)
		l.files = []string{p}
		r.implicit = append(r.implicit, p)
		return l
	}
	if k, ok := kindOf(bt, Binary); !ok || k != Binary {
		return bad("tool %s is a %s (unsupported)", ref, tm.Type)
	}
	tn := g.node(ref, g.cfg.toolSide(), Binary, n.name)
	if tn == nil {
		return bad("tool %s unavailable", ref)
	}
	l.tool = tn
	r.tools = append(r.tools, tn)
	g.genToolData(r, tm, fail)
	return l
}

// toolFile: one file a tool's packaging specs install, rel = its path below $(HOST_OUT) (etc/<sub_dir>/<name>).
type toolFile struct{ src, rel string }

// genToolData collects what Soong's genrule puts beside a tool from its TransitivePackagingSpecs: the files its
// `required` modules install. sbox copies them to <sandbox>/tools/out/<rel> and runs the command from the sandbox, so a
// tool may find them by a cwd-relative path (stats-log-api-gen: tools/out/etc/stats-log-api-gen/cc_srcs/). Only
// prebuilt_etc_host installs data; other required types (shared libs, binaries) are reached through the tool's rpath
// or not at all, so they are skipped. `data` is no packaging spec of a cc_binary_host (installed with tests only).
func (g *Graph) genToolData(r *genRule, tm *Module, fail func(string, ...any)) {
	skipNone := func(string) bool { return false }
	for _, req := range g.flatten(tm, g.cfg.toolSide(), Binary).strs("required") {
		rm := g.lookupWith(req, g.cfg.toolSide(), skipNone)
		if rm == nil || g.baseType(rm.Type) != "prebuilt_etc_host" {
			continue
		}
		p := g.flatten(rm, g.cfg.toolSide(), Gen)
		srcs := p.strs("srcs")
		if s := p.str("src"); s != "" {
			srcs = append(srcs, s)
		}
		name := p.str("filename")
		if name != "" && len(srcs) != 1 {
			fail("%s: filename needs exactly one src", rm.Name)
			continue
		}
		for _, s := range srcs {
			files, ok := g.resolveSrc(rm.Dir, g.cfg.toolSide(), s, rm.Name)
			if !ok {
				fail("%s: src %s unresolved", rm.Name, s)
			}
			for _, f := range files {
				base := name
				if base == "" {
					base = filepath.Base(f.path)
				}
				r.toolData = append(r.toolData, toolFile{f.path, filepath.Join("etc", p.str("sub_dir"), base)})
				r.implicit = append(r.implicit, f.path)
			}
		}
	}
}

// sandboxPrelude: the script start of a rule whose tools have data. It builds <genDir>/sbox as Soong's sandbox looks
// to the command: the tree's top-level entries as symlinks (tree-relative paths keep working), tools/out/<rel> as
// symlinks to the data, then cds there. A tree `tools` dir becomes a real dir of links to its entries plus `out`.
func sandboxPrelude(sb string, data []toolFile) string {
	q := shellEscape(sb)
	var b strings.Builder
	fmt.Fprintf(&b, "rm -rf %s && mkdir -p %s || exit 1\nfor e in \"$PWD\"/*; do ln -s \"$e\" %s/; done\n", q, q, q)
	fmt.Fprintf(&b, "if [ -L %s/tools ]; then rm %s/tools && mkdir %s/tools && for e in \"$PWD\"/tools/*; do ln -s \"$e\" %s/tools/; done; fi\n", q, q, q, q)
	for _, f := range data {
		dst := shellEscape(filepath.Join(sb, "tools", "out", f.rel))
		src := f.src
		if !filepath.IsAbs(src) {
			src = "$PWD/" + shellEscape(src)
		}
		fmt.Fprintf(&b, "mkdir -p \"$(dirname %s)\" && ln -sf %s %s || exit 1\n", dst, src, dst)
	}
	fmt.Fprintf(&b, "cd %s || exit 1\n", q)
	return b.String()
}

// pythonTool: the script a python_binary_host runs and the sources it imports, tree-relative, main first. Run as is (`python3 main.py`) when
// there are no data or pkg_path and every source, its python_library_host libs' included (transitively, none with data or pkg_path), sits
// beside main: the par Soong builds puts them at its top level, which is the script's own directory on sys.path (python.go getPkgPath,
// binary.go getPyMainFile: main defaults to <name>.py). Soong packages the others with their library tree, which is not rebuilt here. why
// says what disqualifies the module.
func pythonTool(g *Graph, m *Module) (files []string, why string) {
	p := g.flatten(m, g.cfg.toolSide(), Gen)
	if len(p.strs("data")) > 0 || p.str("pkg_path") != "" {
		return nil, "with data or pkg_path"
	}
	var libSrcs []string
	seen := map[string]bool{}
	var addLibs func(libs []string) string
	addLibs = func(libs []string) string {
		for _, l := range libs {
			if seen[l] {
				continue
			}
			seen[l] = true
			lm := g.lookup(l, g.cfg.toolSide())
			if lm == nil || g.baseType(lm.Type) != "python_library_host" {
				return "with lib " + l + " that is no python_library_host"
			}
			lp := g.flatten(lm, g.cfg.toolSide(), Gen)
			if lp.str("pkg_path") != "" || len(lp.strs("data")) > 0 {
				return "with lib " + l + " that has data or pkg_path"
			}
			libSrcs = append(libSrcs, g.globs(lm.Dir, lp.strs("srcs"))...)
			if why := addLibs(lp.strs("libs")); why != "" {
				return why
			}
		}
		return ""
	}
	if why := addLibs(p.strs("libs")); why != "" {
		return nil, why
	}
	srcs := g.globs(m.Dir, p.strs("srcs"))
	main := ""
	if mn := p.str("main"); mn != "" {
		main = filepath.Join(m.Dir, mn)
	}
	switch {
	case len(libSrcs) > 0:
	case len(srcs) == 0 && main != "":
		return []string{main}, ""
	case len(srcs) == 1 && (main == "" || main == srcs[0]):
		return srcs, ""
	}
	if main == "" {
		main = filepath.Join(m.Dir, m.Name+".py")
	}
	if !slices.Contains(srcs, main) {
		return nil, fmt.Sprintf("whose main %s is not in its %d sources", main, len(srcs))
	}
	files = []string{main}
	for _, s := range append(srcs, libSrcs...) {
		if filepath.Dir(s) != filepath.Dir(main) {
			return nil, fmt.Sprintf("with source %s outside main's directory", s)
		}
		if s != main {
			files = append(files, s)
		}
	}
	return files, ""
}

// genExpander substitutes Soong's cmd variables (genrule.go: the Expand callback of
// generateCommonBuildActions); paths are absolute, shell-escaped like proptools.ShellEscape.
type genExpander struct {
	r      *genRule
	genDir string              // $(genDir)
	outs   []string            // $(out)
	at     func(string) string // where the command writes an output path: itself, or its sandbox copy under write_if_changed
}

// place: where the command writes output path p (at, default p itself).
func (e *genExpander) place(p string) string {
	if e.at == nil {
		return p
	}
	return e.at(p)
}

func (e *genExpander) words(l genLabel) []string {
	if l.tool != nil {
		return []string{shellEscape(l.tool.out)}
	}
	var out []string
	for _, f := range l.files {
		if l.out {
			f = e.place(f)
		}
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
			return strings.Join(mapStrings(mapStrings(e.outs, e.place), shellEscape), " "), nil
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

// genSandbox: the directory a write_if_changed task's command writes into, below its gen directory (sbox's sandbox is in the out dir too).
const genSandbox = ".sbox-out"

// genScript is the script of one task of a genrule node: its expanded command, or the failure that stops it.
func (g *Graph) genScript(n *Node, t genTask) string {
	r := n.rule
	msg := r.broken
	runDir, at := t.genDir, func(p string) string { return p }
	if r.writeIfChanged {
		runDir = filepath.Join(t.genDir, genSandbox)
		at = func(p string) string { return filepath.Join(runDir, strings.TrimPrefix(p, t.genDir+"/")) }
	}
	body := ""
	if msg == "" {
		var err error
		body, err = (&genExpander{r: r, genDir: runDir, outs: t.outs, at: at}).expand(t.cmd)
		if err != nil {
			g.problem("%s: %v", n.mod.Name, err)
			msg = err.Error()
		}
	}
	if msg != "" {
		return "echo " + shellEscape(fmt.Sprintf("error: genrule %s: %s", n.name, msg)) + " >&2\nexit 1\n"
	}
	// Soong's command never sees a previous run's outputs: sbox runs it in a fresh sandbox and removes the whole output dir first
	// (cmd/sbox/sbox.go runCommand -> clearOutputDirectory, alwaysWrite). Without this, `>> $(out)` appends to a reused file and
	// `mkdir $(genDir)/x` fails on a rerun (mimemap-res.jar). Kept: what the converter itself wrote there (this script, res.list, ...).
	// write_if_changed (onlyWriteIfChanged) removes only files that are no output; the command writes into the emptied sandbox.
	keep := map[string]bool{}
	for f := range g.genFiles {
		if rel, err := filepath.Rel(t.genDir, f); err == nil && !strings.HasPrefix(rel, "..") {
			keep[strings.Split(rel, string(filepath.Separator))[0]] = true
		}
	}
	if rel, err := filepath.Rel(t.genDir, t.script); err == nil && !strings.HasPrefix(rel, "..") {
		keep[rel] = true
	}
	var notKept []string
	for _, k := range sortedKeys(keep) {
		notKept = append(notKept, "! -name "+shellEscape(k))
	}
	// then the outputs' directories, which sbox's sandbox has before the command runs (commands write into them without mkdir)
	var outDirs []string
	for _, o := range t.outs {
		outDirs = append(outDirs, filepath.Dir(at(o)))
	}
	slices.Sort(outDirs)
	outDirs = slices.Compact(outDirs)
	gd := shellEscape(t.genDir)
	var clear string
	if r.writeIfChanged {
		var excl []string
		for _, k := range sortedKeys(keep) {
			excl = append(excl, "! -path "+shellEscape(filepath.Join(t.genDir, k)), "! -path "+shellEscape(filepath.Join(t.genDir, k)+"/*"))
		}
		for _, o := range t.outs {
			excl = append(excl, "! -path "+shellEscape(o))
		}
		clear = fmt.Sprintf("mkdir -p %s && find %s -mindepth 1 ! -type d %s -delete && rm -rf %s && mkdir -p %s || exit 1\n", gd, gd,
			strings.Join(excl, " "), shellEscape(runDir), strings.Join(mapStrings(outDirs, shellEscape), " "))
	} else {
		clear = fmt.Sprintf("mkdir -p %s && find %s -mindepth 1 -maxdepth 1 %s -exec rm -rf -- {} + && mkdir -p %s || exit 1\n", gd, gd,
			strings.Join(notKept, " "), strings.Join(mapStrings(outDirs, shellEscape), " "))
	}
	// sbox validates the outputs after the command (validateOutputFiles); ninja would only notice a missing
	// one in a consumer
	check := "\nrc=$?\n[ $rc -eq 0 ] || exit $rc\nfor f in " + strings.Join(mapStrings(mapStrings(t.outs, at), shellEscape), " ") +
		"; do\n  [ -e \"$f\" ] || { echo \"error: genrule " + n.name + ": $f was not created\" >&2; exit 1; }\ndone\n"
	if r.writeIfChanged {
		// sbox moveFiles(onlyWriteIfChanged): an output whose bytes are unchanged keeps its old file and mtime; a changed one is moved
		// into place and stamped now (a tool may write an old mtime)
		for _, o := range t.outs {
			check += fmt.Sprintf("cmp -s %s %s || { mkdir -p %s && mv -f %s %s && touch %s; } || exit 1\n", shellEscape(at(o)), shellEscape(o),
				shellEscape(filepath.Dir(o)), shellEscape(at(o)), shellEscape(o), shellEscape(o))
		}
		check += "rm -rf " + shellEscape(runDir) + "\n"
	}
	if len(r.toolData) > 0 {
		body = sandboxPrelude(filepath.Join(r.genDir, "sbox"), r.toolData) + body
	}
	return clear + body + check
}

// emitGen writes the build edges of one genrule node (one per task, then a sharded gensrcs' merge) and records their scripts for the
// caller to write. Every task depends on all inputs, as Soong's (cmd.Implicits(srcFiles): a command may name the other srcs).
func (g *Graph) emitGen(b *strings.Builder, n *Node) {
	r := n.rule
	if r.edge != nil {
		g.emitEdge(b, *r.edge)
		return
	}
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
	tasks := r.taskList()
	for i, t := range tasks {
		g.genFile(t.script, g.genScript(n, t))
		name := n.name
		if len(tasks) > 1 {
			name += " " + strconv.Itoa(i) // genrule.go: desc "generate <shard>"
		}
		fmt.Fprintf(b, "build %s: genrule | %s\n  name = %s\n  script = %s\n", join(t.outs), join(append(implicit[:len(implicit):len(implicit)], t.script)),
			esc(name), esc(t.script))
	}
	if r.merge != "" {
		g.genFile(r.merge, genMergeScript(r))
		var from []string
		for _, t := range tasks {
			from = append(from, t.outs...)
		}
		fmt.Fprintf(b, "build %s: genrule | %s\n  name = %s\n  script = %s\n", join(r.outs), join(append(from, r.merge)), esc(n.name+" merge shards"),
			esc(r.merge))
	}
	if r.phony != "" {
		fmt.Fprintf(b, "build %s: phony %s\n", esc(r.phony), join(r.outs))
	}
}

// gensrcsShardSize: inputs per gensrcs task unless shard_size says otherwise (genrule.go defaultShardSize).
const gensrcsShardSize = 50

// gensrcsTasks is gensrcs' task generator (genrule.go NewGenSrcs): the inputs split into shards of shardSize (android.ShardPaths), each
// one task whose command runs once per input with $(in) / $(out) bound, each as `bash -c '…'`, joined with &&; the other variables
// are expanded later as a genrule's, $(genDir) being the task's directory. One shard writes the final outputs, <genDir>/gensrcs/<rel>,
// directly; with more, shard i writes <genDir>/<i>/<rel> and copyTo names the final path, which the merge edge fills (genMergeScript).
// rel (paths.go genPathWithExtAndTrimExt): a tree file's tree-relative path, a generated file's path below its generator's directory,
// with trim_extension cut and output_extension appended, else the extension replaced.
// Differs from Soong: duplicate inputs are dropped before sharding (Soong shards them as listed; RuleBuilder keeps an output once
// within a shard, and a duplicate across shards would be two rules for one output there), so shard boundaries count distinct inputs.
func gensrcsTasks(genDir, raw, ext, trim string, shardSize int, inputs []srcFile, fail func(string, ...any)) (outs []string, tasks []genTask) {
	if shardSize < 1 {
		fail("shard_size %d is not positive", shardSize)
		shardSize = gensrcsShardSize
	}
	var uniq []srcFile
	seen := map[string]bool{}
	for _, f := range inputs {
		// an input listed twice (two filegroups naming one proto) is one command: RuleBuilder keeps an output once
		if !seen[f.path] {
			seen[f.path] = true
			uniq = append(uniq, f)
		}
	}
	shards := (len(uniq) + shardSize - 1) / shardSize
	final := filepath.Join(genDir, "gensrcs")
	for i := 0; i < shards; i++ {
		t := genTask{genDir: final, script: filepath.Join(genDir, "genrule.sh")}
		if shards > 1 {
			t.genDir = filepath.Join(genDir, strconv.Itoa(i))
			t.script = filepath.Join(genDir, "genrule_"+strconv.Itoa(i)+".sh")
		}
		var cmds []string
		for _, f := range uniq[i*shardSize : min((i+1)*shardSize, len(uniq))] {
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
			out := filepath.Join(t.genDir, rel)
			outs = append(outs, filepath.Join(final, rel))
			t.outs = append(t.outs, out)
			if shards > 1 {
				t.copyTo = append(t.copyTo, filepath.Join(final, rel))
			}
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
		t.cmd = strings.Join(cmds, " && ")
		tasks = append(tasks, t)
	}
	return outs, tasks
}

// genMergeScript is gensrcsMerge for a sharded gensrcs: the final directory emptied, then each shard's declared outputs copied into it at
// the same relative path. Soong zips the shards (soong_zip -C <shard> -f <out>…) and zipsync extracts the zip into the emptied final directory;
// same result, without the zip round trip and without soong_zip / zipsync, which only Java goals have (-build-tools).
func genMergeScript(r *genRule) string {
	final := filepath.Join(r.genDir, "gensrcs")
	var b strings.Builder
	fmt.Fprintf(&b, "rm -rf %s && mkdir -p %s || exit 1\n", shellEscape(final), shellEscape(final))
	for _, t := range r.tasks {
		rels := make([]string, len(t.outs))
		for i, o := range t.outs {
			rels[i] = shellEscape(strings.TrimPrefix(o, t.genDir+"/"))
		}
		fmt.Fprintf(&b, "(cd %s && cp --parents -- %s %s) || exit 1\n", shellEscape(t.genDir), strings.Join(rels, " "), shellEscape(final))
	}
	return b.String()
}

// genFile records a file emit generates, for writeGenFiles.
func (g *Graph) genFile(path, body string) {
	if g.genFiles == nil {
		g.genFiles = map[string]string{}
	}
	g.genFiles[path] = body
}

// writeGenFiles writes the generated files (genrule scripts, Rust staticlib roots), leaving unchanged ones alone: a newer file would rerun its step.
func (g *Graph) writeGenFiles() error {
	for path, body := range g.genFiles {
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
