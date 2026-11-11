// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"
)

// Expected strings are derived by hand from genrule.go (Soong) and android/expand.go, never from the code under test.

func TestExpandVars(t *testing.T) {
	// android.Expand: `$(name)` maps the trimmed name, `$$` is one `$`, text around stays
	got, err := expandVars("a $(x) b $$(y) c $( z )", func(name string) (string, error) { return "<" + name + ">", nil })
	if err != nil {
		t.Fatal(err)
	}
	if want := "a <x> b $(y) c <z>"; got != want {
		t.Errorf("got %q, want %q", got, want)
	}
	for in, msg := range map[string]string{
		"x $":     "expected character after '$'",
		"x $(foo": "missing )",
		"x $foo ": "expected '(' after '$', did you mean $(foo)?",
		"x $ y":   "unexpected character ' ' after '$'",
	} {
		if _, err := expandVars(in, func(string) (string, error) { return "", nil }); err == nil || err.Error() != msg {
			t.Errorf("%q: error %v, want %q", in, err, msg)
		}
	}
}

func TestShellEscape(t *testing.T) {
	// proptools.ShellEscape: letters, digits and _+-=.,/ pass, a space too; anything else quotes the string
	for in, want := range map[string]string{
		"/a/b-c_d.e,f=g+h": "/a/b-c_d.e,f=g+h",
		"a b":              "a b",
		"a$b":              "'a$b'",
		"it's":             `'it'"'"'s'`,
		"x(y)":             "'x(y)'",
	} {
		if got := shellEscape(in); got != want {
			t.Errorf("shellEscape(%q) = %q, want %q", in, got, want)
		}
	}
}

// expander of a rule with a cc tool (out path known), a script tool, a tool file, two srcs and two outs
func testExpander() *genExpander {
	r := &genRule{
		hasTools: true,
		labels: []genLabel{
			{name: "aprotoc", tool: &Node{out: "/out/host/aprotoc/aprotoc"}},
			{name: "gen.py", files: []string{"/tree/x/gen.py"}, interp: "python3"},
			{name: "data.txt", files: []string{"/tree/x/data.txt"}},
			{name: ":protos", files: []string{"/tree/p/a.proto", "/tree/p/b.proto"}},
			{name: "one.proto", files: []string{"/tree/p/one.proto"}},
			{name: "o/a.h", files: []string{"/out/gen/device/g/o/a.h"}},
			{name: "o/a.cc", files: []string{"/out/gen/device/g/o/a.cc"}},
		},
		inputs: []string{"/tree/p/a.proto", "/tree/p/b.proto", "/tree/p/one.proto"},
		outs:   []string{"/out/gen/device/g/o/a.h", "/out/gen/device/g/o/a.cc"},
	}
	return &genExpander{r: r, genDir: "/out/gen/device/g", outs: r.outs}
}

func TestExpandCmd(t *testing.T) {
	e := testExpander()
	for cmd, want := range map[string]string{
		"$(in)":                      "/tree/p/a.proto /tree/p/b.proto /tree/p/one.proto",
		"$(out)":                     "/out/gen/device/g/o/a.h /out/gen/device/g/o/a.cc",
		"mkdir -p $(genDir)/o":       "mkdir -p /out/gen/device/g/o",
		"$(location)":                "/out/host/aprotoc/aprotoc", // first label = first tool
		"$(location aprotoc) -x":     "/out/host/aprotoc/aprotoc -x",
		"$(location gen.py)":         "python3 /tree/x/gen.py",
		"$(location data.txt)":       "/tree/x/data.txt",
		"$(locations :protos)":       "/tree/p/a.proto /tree/p/b.proto",
		"$(location o/a.h)":          "/out/gen/device/g/o/a.h",
		"$(locations one.proto)":     "/tree/p/one.proto",
		"echo $$HOME $$(date)":       "echo $HOME $(date)",
		"$(location  aprotoc )":      "/out/host/aprotoc/aprotoc",
		"CC_ARCH=$$CC_ARCH && true":  "CC_ARCH=$CC_ARCH && true",
		"$(in) > $(location o/a.cc)": "/tree/p/a.proto /tree/p/b.proto /tree/p/one.proto > /out/gen/device/g/o/a.cc",
	} {
		got, err := e.expand(cmd)
		if err != nil || got != want {
			t.Errorf("expand(%q) = %q, %v; want %q", cmd, got, err, want)
		}
	}
}

func TestExpandCmdErrors(t *testing.T) {
	e := testExpander()
	for cmd, want := range map[string]string{
		"$(location :protos)":    `cmd: label ":protos" has multiple files, use $(locations :protos) to reference it`,
		"$(location nope)":       `cmd: unknown location label "nope" is not in srcs, out, tools or tool_files.`,
		"$(locations nope)":      `cmd: unknown locations label "nope" is not in srcs, out, tools or tool_files.`,
		"$(depfile)":             "cmd: unknown variable '$(depfile)'",
		"$(build_number_file)":   "cmd: $(build_number_file) is unsupported",
		"ok $(out) $(location)x": "",
	} {
		_, err := e.expand(cmd)
		if want == "" {
			if err != nil {
				t.Errorf("expand(%q): unexpected %v", cmd, err)
			}
			continue
		}
		if err == nil || err.Error() != want {
			t.Errorf("expand(%q) error %v, want %q", cmd, err, want)
		}
	}
	// $(location) needs tools or tool_files; a rule without them says so, though srcs exist
	noTools := &genExpander{r: &genRule{labels: []genLabel{{name: "a", files: []string{"/a"}}}}, genDir: "/g"}
	if _, err := noTools.expand("$(location)"); err == nil || !strings.Contains(err.Error(), "at least one `tools` or `tool_files` is required") {
		t.Errorf("$(location) without tools: %v", err)
	}
	// an unresolved tool fails the command that names it
	bad := &genExpander{r: &genRule{hasTools: true, labels: []genLabel{{name: "t", err: "tool t is a rust_binary_host (unsupported)"}}}, genDir: "/g"}
	if _, err := bad.expand("$(location t)"); err == nil || err.Error() != "cmd: tool t is a rust_binary_host (unsupported)" {
		t.Errorf("unresolved tool: %v", err)
	}
}

func TestMultilib(t *testing.T) {
	// Soong's multilib names (android/arch.go Multilib: "lib32" / "lib64") follow the pointer width
	for bits, want := range map[int]string{32: "lib32", 64: "lib64"} {
		if got := (Config{Arch: "any", PointerBits: bits}).Multilib(); got != want {
			t.Errorf("Multilib(%d) = %s, want %s", bits, got, want)
		}
	}
}

// graphFor evaluates the Android.bp files of a fake tree (dir -> content) for both sides.
func graphFor(t *testing.T, files map[string]string) *Graph {
	t.Helper()
	return graphForCfg(t, Config{Arch: "x86_64", PointerBits: 64}, files)
}

// graphForCfg: graphFor with every side cfg names evaluated (a native bridge side with -native-bridge).
func graphForCfg(t *testing.T, cfg Config, files map[string]string) *Graph {
	t.Helper()
	tree, out := t.TempDir(), t.TempDir()
	for name, body := range files {
		path := filepath.Join(tree, name)
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(body), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	parsed, err := discover(tree)
	if err != nil {
		t.Fatal(err)
	}
	g := &Graph{tree: tree, out: out, cfg: cfg, libcxx: standInLibcxx, nodes: map[key]*Node{}, baseTypes: map[string]string{}, mods: map[Side]map[string][]*Module{}}
	for _, side := range cfg.sides() {
		ecfg := cfg
		ecfg.Arch, ecfg.PointerBits = cfg.archOf(side)
		mods, problems := (&evaluator{cfg: ecfg, os: side.evalOS()}).modules(parsed)
		if len(problems) > 0 {
			t.Fatal(problems)
		}
		g.mods[side] = mods
	}
	g.soong.Flags = newGlobalFlags()
	return g
}

func TestGenruleGraph(t *testing.T) {
	g := graphFor(t, map[string]string{
		"p/Android.bp": `
filegroup { name: "protos", srcs: ["a.proto", "sub/*.proto"], exclude_srcs: ["sub/skip.proto"] }
genrule {
    name: "gen_a",
    srcs: [":protos", "extra.txt"],
    tool_files: ["run.sh"],
    tools: ["pytool", "jtool"],
    cmd: "$(location) $(location pytool) $(in) $(genDir)/x",
    out: ["x/a.cc", "x/a.h"],
    export_include_dirs: ["x"],
}
genrule { name: "gen_b", srcs: [":gen_a"], cmd: "cat $(in) > $(out)", out: ["b.cc"] }
cc_genrule { name: "gen_c", cmd: "echo $$CC_ARCH", out: ["c.h"] }
python_binary_host { name: "pytool", main: "tool.py", srcs: ["tool.py"] }
java_binary_host { name: "jtool" }
cc_library_static {
    name: "lib",
    srcs: [":gen_b"],
    generated_headers: ["gen_a", "gen_c"],
    export_generated_headers: ["gen_c"],
    generated_sources: ["gen_a"],
}
cc_library_static { name: "user", header_libs: ["lib"], static_libs: ["lib"] }
`,
		"p/a.proto": "", "p/sub/b.proto": "", "p/sub/skip.proto": "", "p/extra.txt": "", "p/run.sh": "", "p/tool.py": "",
	})
	lib := g.node("lib", Device, Static, "test")
	if lib == nil {
		t.Fatal("lib not built")
	}
	genDir := func(name string) string { return filepath.Join(g.out, "gen", "device", name) }
	a := g.nodes[key{"gen_a", Device, Gen}]
	if a == nil {
		t.Fatal("gen_a not built")
	}
	// srcs: the filegroup (a.proto, sub/b.proto; skip.proto excluded) then the plain file, labelled as written;
	// tree files are shown tree-relative (the command runs from the tree root, as Soong's does)
	wantIn := []string{"p/a.proto", "p/sub/b.proto", "p/extra.txt"}
	if strings.Join(a.rule.inputs, " ") != strings.Join(wantIn, " ") {
		t.Errorf("gen_a inputs %v, want %v", a.rule.inputs, wantIn)
	}
	// export_include_dirs: under the module dir and without it (genrule.go), not the gen dir itself
	wantInc := []string{"-I" + genDir("gen_a") + "/p/x", "-I" + genDir("gen_a") + "/x"}
	if strings.Join(a.rule.incs, " ") != strings.Join(wantInc, " ") {
		t.Errorf("gen_a incs %v, want %v", a.rule.incs, wantInc)
	}
	// Java tool: unsupported, the genrule is broken and its edge will fail with the reason
	if !strings.Contains(a.rule.broken, "tool jtool is a java_binary_host (unsupported)") {
		t.Errorf("gen_a broken = %q", a.rule.broken)
	}
	// a python_binary_host of one file runs as `python3 <main>`
	if got := (&genExpander{r: a.rule, genDir: genDir("gen_a")}).words(a.rule.labels[0]); len(got) != 1 || got[0] != "python3 p/tool.py" {
		t.Errorf("pytool words %v", got)
	}
	// :genrule in srcs: its output is the input, and the consumer compiles only .cc outputs it names
	b := g.nodes[key{"gen_b", Device, Gen}]
	if want := genDir("gen_a") + "/x/a.cc"; len(b.rule.inputs) != 2 || b.rule.inputs[0] != want || b.rule.inputs[1] != genDir("gen_a")+"/x/a.h" {
		t.Errorf("gen_b inputs %v", b.rule.inputs)
	}
	// lib compiles gen_b's b.cc (srcs) and gen_a's a.cc (generated_sources); headers are only waited on
	wantSrcs := []string{genDir("gen_b") + "/b.cc", genDir("gen_a") + "/x/a.cc"}
	if strings.Join(lib.srcs, " ") != strings.Join(wantSrcs, " ") {
		t.Errorf("lib srcs %v, want %v", lib.srcs, wantSrcs)
	}
	// waits: gen_b's output (srcs), gen_a's and gen_c's outputs (generated_*)
	for _, w := range []string{genDir("gen_b") + "/b.cc", genDir("gen_a") + "/x/a.h", genDir("gen_c") + "/c.h"} {
		found := false
		for _, f := range lib.gen {
			found = found || f == w
		}
		if !found {
			t.Errorf("lib does not wait for %s (gen %v)", w, lib.gen)
		}
	}
	// export_generated_headers: gen_c's include dir (default = its gen dir) reaches dependents, gen_a's does not
	if want := "-I" + genDir("gen_c"); len(lib.exports) != 1 || lib.exports[0] != want {
		t.Errorf("lib exports %v, want [%s]", lib.exports, want)
	}
	// a cc_genrule gets the CC_* shell variables of cc/genrule.go before its command
	c := g.nodes[key{"gen_c", Device, Gen}]
	if want := "CC_ARCH=x86_64 CC_NATIVE_BRIDGE= CC_MULTILIB=lib64 CC_OS=android && echo $$CC_ARCH"; c.rule.cmd != want {
		t.Errorf("gen_c cmd %q, want %q", c.rule.cmd, want)
	}
	// one genrule node per name and side
	if again := g.genNode("gen_a", Device, "test"); again != a {
		t.Error("gen_a built twice")
	}
	if host := g.genNode("gen_c", Host, "test"); host == c || !strings.Contains(host.rule.cmd, "CC_OS=linux_glibc") {
		t.Error("host side gen_c is not its own node")
	}
}

func TestGenruleOutputsAndPhony(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
genrule { name: "many", cmd: "true", out: ["1","2","3","4","5","6","7"] }
genrule { name: "few", cmd: "true", out: ["1","2","3","4","5","6"] }
`})
	many, few := g.genNode("many", Device, "t"), g.genNode("few", Device, "t")
	if want := filepath.Join(g.out, "gen/device/many/genrule-phony"); len(many.rule.deps) != 1 || many.rule.deps[0] != want {
		t.Errorf("7 outputs: deps %v, want phony %s", many.rule.deps, want)
	}
	if len(few.rule.deps) != 6 || few.rule.phony != "" {
		t.Errorf("6 outputs: deps %v", few.rule.deps)
	}
	var b strings.Builder
	g.emitGen(&b, many)
	if !strings.Contains(b.String(), "build "+g.out+"/gen/device/many/genrule-phony: phony "+g.out+"/gen/device/many/1 ") {
		t.Errorf("no phony edge in\n%s", b.String())
	}
}

func TestGenruleScriptAndEdge(t *testing.T) {
	g := graphFor(t, map[string]string{
		"p/Android.bp": `genrule { name: "g", srcs: ["in.txt"], cmd: "cp $(in) $(out)", out: ["o.txt"] }`,
		"p/in.txt":     "",
	})
	n := g.genNode("g", Device, "t")
	out := g.out + "/gen/device/g/o.txt"
	// the output dir emptied but for this script (sbox clearOutputDirectory, alwaysWrite), the command, then Soong's check that every
	// declared output exists
	gd := g.out + "/gen/device/g"
	wantScript := "mkdir -p " + gd + " && find " + gd + " -mindepth 1 -maxdepth 1 ! -name genrule.sh -exec rm -rf -- {} + && mkdir -p " + gd + " || exit 1\ncp p/in.txt " + out + "\nrc=$?\n[ $rc -eq 0 ] || exit $rc\nfor f in " + out +
		"; do\n  [ -e \"$f\" ] || { echo \"error: genrule g: $f was not created\" >&2; exit 1; }\ndone\n"
	if got := g.genScript(n, n.rule.taskList()[0]); got != wantScript {
		t.Errorf("script %q, want %q", got, wantScript)
	}
	// ninja edge: absolute input (ninja runs from the build dir), script as implicit input
	var b strings.Builder
	g.emitGen(&b, n)
	want := "build " + out + ": genrule | " + g.tree + "/p/in.txt " + g.out + "/gen/device/g/genrule.sh\n" +
		"  name = g\n  script = " + g.out + "/gen/device/g/genrule.sh\n"
	if b.String() != want {
		t.Errorf("edge %q, want %q", b.String(), want)
	}
	if g.genFiles[g.out+"/gen/device/g/genrule.sh"] != wantScript {
		t.Error("script not recorded for writing")
	}
}

// A rerun sees no earlier output (sbox): a command that mkdirs into $(genDir) passes twice; the script itself survives.
func TestGenruleRerun(t *testing.T) {
	g := graphFor(t, map[string]string{
		"p/Android.bp": `genrule { name: "g", cmd: "mkdir $(genDir)/res/ && echo x >> $(genDir)/res/a && cp $(genDir)/res/a $(out)", out: ["sub/o.txt"] }`,
	})
	n := g.genNode("g", Device, "t")
	var b strings.Builder
	g.emitGen(&b, n)
	if err := g.writeGenFiles(); err != nil {
		t.Fatal(err)
	}
	for run := 1; run <= 2; run++ {
		c := exec.Command("/bin/bash", n.rule.script)
		c.Dir = g.tree
		if out, err := c.CombinedOutput(); err != nil {
			t.Fatalf("run %d: %v\n%s", run, err, out)
		}
	}
	if got, _ := os.ReadFile(g.out + "/gen/device/g/sub/o.txt"); string(got) != "x\n" {
		t.Errorf("output after two runs %q, want one line", got)
	}
}

// sbox runs every command in a fresh sandbox (cmd/sbox/sbox.go), so an appending command run twice in the same out dir leaves one
// run's output, as on a clean build: "x\n", the command's own output once.
func TestGenruleRerunStartsFromNoOutputs(t *testing.T) {
	g := graphFor(t, map[string]string{
		"p/Android.bp": `genrule { name: "g", cmd: "echo x >> $(out)", out: ["o.txt"] }`,
	})
	n := g.genNode("g", Device, "t")
	out := g.out + "/gen/device/g/o.txt"
	if err := os.MkdirAll(filepath.Dir(out), 0o755); err != nil {
		t.Fatal(err)
	}
	script := filepath.Join(t.TempDir(), "genrule.sh")
	if err := os.WriteFile(script, []byte(g.genScript(n, n.rule.taskList()[0])), 0o644); err != nil {
		t.Fatal(err)
	}
	for run := 1; run <= 2; run++ {
		cmd := exec.Command("bash", script)
		cmd.Dir = g.tree
		if b, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("run %d: %v\n%s", run, err, b)
		}
	}
	got, err := os.ReadFile(out)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != "x\n" {
		t.Errorf("output after two runs %q, want %q", got, "x\n")
	}
}

// Soong installs a prebuilt_etc_host to $(HOST_OUT)/etc/<sub_dir>/<filename or basename>; a genrule tool's `required`
// installs are copied to <sandbox>/tools/out/<that path>, and the command runs from the sandbox.
func TestGenruleToolData(t *testing.T) {
	g := graphFor(t, map[string]string{
		"p/Android.bp": `
cc_binary_host { name: "tool", srcs: ["t.cpp"], required: ["tool-hdrs", "tool-conf", "libother"] }
prebuilt_etc_host { name: "tool-hdrs", srcs: ["hdrs/*.h"], sub_dir: "tool/cc_hdrs" }
prebuilt_etc_host { name: "tool-conf", src: "c.txt", filename: "conf" }
cc_library_host_shared { name: "libother" }
genrule { name: "g", tools: ["tool"], cmd: "$(location tool) $(out)", out: ["o.h"] }
genrule { name: "plain", cmd: "true", out: ["p"] }
`,
		"p/t.cpp": "", "p/hdrs/a.h": "", "p/hdrs/b.h": "", "p/c.txt": "",
	})
	n := g.genNode("g", Device, "t")
	want := []toolFile{{"p/hdrs/a.h", "etc/tool/cc_hdrs/a.h"}, {"p/hdrs/b.h", "etc/tool/cc_hdrs/b.h"}, {"p/c.txt", "etc/conf"}}
	if !slices.Equal(n.rule.toolData, want) {
		t.Errorf("toolData %v, want %v", n.rule.toolData, want)
	}
	sb := g.out + "/gen/device/g/sbox"
	s := g.genScript(n, n.rule.taskList()[0])
	for _, w := range []string{
		"ln -sf $PWD/p/hdrs/a.h " + sb + "/tools/out/etc/tool/cc_hdrs/a.h ",
		"ln -sf $PWD/p/c.txt " + sb + "/tools/out/etc/conf ",
		"cd " + sb + " || exit 1\n",
	} {
		if !strings.Contains(s, w) {
			t.Errorf("script lacks %q:\n%s", w, s)
		}
	}
	if !slices.Contains(n.rule.implicit, "p/c.txt") {
		t.Errorf("data not a rebuild trigger: %v", n.rule.implicit)
	}
	pn := g.genNode("plain", Device, "t")
	if p := g.genScript(pn, pn.rule.taskList()[0]); strings.Contains(p, "sbox") {
		t.Errorf("rule without tool data got a sandbox:\n%s", p)
	}
}

func TestGenruleMissingReferences(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
genrule { name: "g", srcs: [":absent"], cmd: "true", out: ["o"] }
cc_library_static { name: "l", generated_headers: ["nowhere"], srcs: [":g", ":l2"] }
cc_library_static { name: "l2" }
`})
	g.node("l", Device, Static, "t")
	// a name the tree lacks is recorded for fetching, with no `not supported yet` / `not a known filegroup`
	// problem of its own for the genrule case; a module of another type under a name is only a problem
	if got := strings.Join(sortedKeys(g.missing), ","); got != "absent,nowhere" {
		t.Errorf("missing %q", got)
	}
	n := g.nodes[key{"g", Device, Gen}]
	if !strings.Contains(n.rule.broken, "srcs :absent unresolved") {
		t.Errorf("g broken = %q", n.rule.broken)
	}
	script := g.genScript(n, n.rule.taskList()[0])
	if want := "echo 'error: genrule g: srcs :absent unresolved' >&2\nexit 1\n"; script != want {
		t.Errorf("script %q, want %q", script, want)
	}
	for _, p := range g.problems {
		if strings.Contains(p, "not supported yet") {
			t.Errorf("stale problem %q", p)
		}
	}
}

// A cc_object in a genrule's srcs is its object file, Soong's <out>/<side>/<name>/<name + ".o"> (cc/object.go link; ART's asm_defines.s -> asm_defines.s.o),
// copied from its one compile; a `none` generator (providers.toml) is dropped like a dropped library.
func TestGenruleObjectSrcAndNoneGenerator(t *testing.T) {
	g := graphFor(t, map[string]string{
		"p/Android.bp": `
cc_object { name: "defs.s", srcs: ["d.cc"], cflags: ["-S"] }
cc_genrule { name: "hdr", srcs: [":defs.s"], out: ["d.h"], cmd: "cat $(in) > $(out)" }
xsd_config { name: "xsd", srcs: ["x.xsd"] }
cc_library_static { name: "l", srcs: ["l.cc"], generated_headers: ["hdr"], generated_sources: ["xsd"] }
`,
		"p/d.cc": "", "p/l.cc": "", "p/x.xsd": "",
	})
	g.omit = map[string]bool{"xsd": true}
	if g.node("l", Device, Static, "goal") == nil {
		t.Fatal(g.problems)
	}
	if len(g.problems) != 0 {
		t.Errorf("problems %v", g.problems)
	}
	obj := g.out + "/device/defs.s/defs.s.o"
	h := g.nodes[key{"hdr", Device, Gen}]
	if !slices.Equal(h.rule.inputs, []string{obj}) || h.rule.broken != "" {
		t.Errorf("inputs %v, broken %q", h.rule.inputs, h.rule.broken)
	}
	if !slices.ContainsFunc(h.stepDeps(), func(n *Node) bool { return n.name == "defs.s" }) {
		t.Errorf("genrule does not wait for the cc_object")
	}
	if got := sortedKeys(g.dropped); !slices.Equal(got, []string{"device/l xsd"}) {
		t.Errorf("dropped %v", got)
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	if !strings.Contains(ninja, "build "+obj+": cp "+g.out+"/obj/device/defs.s/static/p/d.cc.o\n") {
		t.Errorf("no copy of the object:\n%s", ninja)
	}
}

// runScripts runs generated scripts from the tree in order, as ninja would (cwd = tree root).
func runScripts(t *testing.T, g *Graph, scripts ...string) {
	t.Helper()
	if err := g.writeGenFiles(); err != nil {
		t.Fatal(err)
	}
	for _, s := range scripts {
		c := exec.Command("/bin/bash", s)
		c.Dir = g.tree
		if out, err := c.CombinedOutput(); err != nil {
			t.Fatalf("%s: %v\n%s", s, err, out)
		}
	}
}

// gensrcs shards (genrule.go NewGenSrcs, ShardPaths): 3 inputs at shard_size 2 are shards [a b] and [c]. Shard i writes <gen>/<i>/<rel>
// with $(genDir) = <gen>/<i>, script genrule_<i>.sh, description "generate <i>"; every shard depends on all srcs (cmd.Implicits(srcFiles));
// one merge edge (gensrcsMerge) fills <gen>/gensrcs/<rel> from the shards' declared outputs only, after emptying it.
func TestGensrcsShards(t *testing.T) {
	g := graphFor(t, map[string]string{
		"p/Android.bp": `gensrcs { name: "s", srcs: ["a.txt", "b.txt", "c.txt"], cmd: "cp $(in) $(out) && touch $(genDir)/junk", output_extension: "out", shard_size: 2 }`,
		"p/a.txt":      "a", "p/b.txt": "b", "p/c.txt": "c",
	})
	n := g.genNode("s", Device, "t")
	gd := g.out + "/gen/device/s"
	ins := g.tree + "/p/a.txt " + g.tree + "/p/b.txt " + g.tree + "/p/c.txt"
	var b strings.Builder
	g.emitGen(&b, n)
	want := "build " + gd + "/0/p/a.out " + gd + "/0/p/b.out: genrule | " + ins + " " + gd + "/genrule_0.sh\n  name = s$ 0\n  script = " + gd + "/genrule_0.sh\n" +
		"build " + gd + "/1/p/c.out: genrule | " + ins + " " + gd + "/genrule_1.sh\n  name = s$ 1\n  script = " + gd + "/genrule_1.sh\n" +
		"build " + gd + "/gensrcs/p/a.out " + gd + "/gensrcs/p/b.out " + gd + "/gensrcs/p/c.out: genrule | " + gd + "/0/p/a.out " + gd + "/0/p/b.out " +
		gd + "/1/p/c.out " + gd + "/genrule_merge.sh\n  name = s$ merge$ shards\n  script = " + gd + "/genrule_merge.sh\n"
	if b.String() != want {
		t.Errorf("edges\n%s\nwant\n%s", b.String(), want)
	}
	wantMerge := "rm -rf " + gd + "/gensrcs && mkdir -p " + gd + "/gensrcs || exit 1\n" +
		"(cd " + gd + "/0 && cp --parents -- p/a.out p/b.out " + gd + "/gensrcs) || exit 1\n" +
		"(cd " + gd + "/1 && cp --parents -- p/c.out " + gd + "/gensrcs) || exit 1\n"
	if got := g.genFiles[gd+"/genrule_merge.sh"]; got != wantMerge {
		t.Errorf("merge script %q, want %q", got, wantMerge)
	}
	if s := g.genFiles[gd+"/genrule_1.sh"]; !strings.Contains(s, "bash -c 'cp p/c.txt "+gd+"/1/p/c.out && touch "+gd+"/1/junk'") {
		t.Errorf("shard 1 script:\n%s", s)
	}
	// a stale file in the final directory goes; the shards' undeclared junk never arrives
	if err := os.MkdirAll(gd+"/gensrcs", 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(gd+"/gensrcs/stale", nil, 0o644); err != nil {
		t.Fatal(err)
	}
	runScripts(t, g, gd+"/genrule_0.sh", gd+"/genrule_1.sh", gd+"/genrule_merge.sh")
	var got []string
	filepath.WalkDir(gd+"/gensrcs", func(p string, d os.DirEntry, err error) error {
		if err == nil && !d.IsDir() {
			body, _ := os.ReadFile(p)
			got = append(got, strings.TrimPrefix(p, gd+"/gensrcs/")+"="+string(body))
		}
		return nil
	})
	if want := []string{"p/a.out=a", "p/b.out=b", "p/c.out=c"}; !slices.Equal(got, want) {
		t.Errorf("final dir %v, want %v", got, want)
	}
	// 50 inputs (defaultShardSize) are one shard writing the final directory directly: no merge edge
	srcs, files := []string{}, map[string]string{}
	for i := range 51 {
		f := fmt.Sprintf("f%02d.txt", i)
		srcs = append(srcs, `"`+f+`"`)
		files["q/"+f] = ""
	}
	files["q/Android.bp"] = `gensrcs { name: "one", srcs: [` + strings.Join(srcs[:50], ",") + `], cmd: "cp $(in) $(out)", output_extension: "o" }
gensrcs { name: "two", srcs: [` + strings.Join(srcs, ",") + `], cmd: "cp $(in) $(out)", output_extension: "o" }`
	g = graphFor(t, files)
	if one := g.genNode("one", Device, "t"); len(one.rule.taskList()) != 1 || one.rule.merge != "" {
		t.Errorf("50 inputs: %d tasks, merge %q", len(one.rule.taskList()), one.rule.merge)
	}
	if two := g.genNode("two", Device, "t"); len(two.rule.tasks) != 2 || len(two.rule.tasks[0].outs) != 50 || len(two.rule.tasks[1].outs) != 1 {
		t.Errorf("51 inputs: tasks %d", len(two.rule.tasks))
	}
}

// write_if_changed (sbox --write-if-changed): the command writes into a sandbox ($(out), $(genDir), $(location <out>) name it); an output
// with the same bytes keeps its file and mtime, so `restat` stops consumers; a changed one replaces it; a non-output file is removed first.
func TestGenruleWriteIfChanged(t *testing.T) {
	g := graphFor(t, map[string]string{
		"p/Android.bp": `genrule { name: "w", srcs: ["in.txt"], cmd: "cp $(in) $(genDir)/tmp && cp $(genDir)/tmp $(location o.txt)", out: ["o.txt"], write_if_changed: true }`,
		"p/in.txt":     "v1",
	})
	n := g.genNode("w", Device, "t")
	gd := g.out + "/gen/device/w"
	sb := gd + "/.sbox-out"
	script := g.genScript(n, n.rule.taskList()[0])
	want := "mkdir -p " + gd + " && find " + gd + " -mindepth 1 ! -type d ! -path " + gd + "/genrule.sh ! -path '" + gd + "/genrule.sh/*' ! -path " + gd +
		"/o.txt -delete && rm -rf " + sb + " && mkdir -p " + sb + " || exit 1\n" +
		"cp p/in.txt " + sb + "/tmp && cp " + sb + "/tmp " + sb + "/o.txt\nrc=$?\n[ $rc -eq 0 ] || exit $rc\nfor f in " + sb + "/o.txt" +
		"; do\n  [ -e \"$f\" ] || { echo \"error: genrule w: $f was not created\" >&2; exit 1; }\ndone\n" +
		"cmp -s " + sb + "/o.txt " + gd + "/o.txt || { mkdir -p " + gd + " && mv -f " + sb + "/o.txt " + gd + "/o.txt && touch " + gd + "/o.txt; } || exit 1\n" +
		"rm -rf " + sb + "\n"
	if script != want {
		t.Errorf("script\n%s\nwant\n%s", script, want)
	}
	var b strings.Builder
	g.emitGen(&b, n)
	out := gd + "/o.txt"
	runScripts(t, g, n.rule.script)
	old := time.Unix(1_000_000, 0) // any time before the rerun
	if err := os.Chtimes(out, old, old); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(gd+"/stale", nil, 0o644); err != nil {
		t.Fatal(err)
	}
	runScripts(t, g, n.rule.script)
	if st, err := os.Stat(out); err != nil || !st.ModTime().Equal(old) {
		t.Errorf("unchanged output rewritten: %v %v", st.ModTime(), err)
	}
	if _, err := os.Stat(gd + "/stale"); err == nil {
		t.Error("non-output file kept")
	}
	if _, err := os.Stat(sb); err == nil {
		t.Error("sandbox left behind")
	}
	if err := os.WriteFile(g.tree+"/p/in.txt", []byte("v2"), 0o644); err != nil {
		t.Fatal(err)
	}
	runScripts(t, g, n.rule.script)
	if st, _ := os.Stat(out); st.ModTime().Equal(old) {
		t.Error("changed output not replaced")
	}
	if body, _ := os.ReadFile(out); string(body) != "v2" {
		t.Errorf("output %q", body)
	}
}
