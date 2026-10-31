// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
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
	return &genExpander{r, "/out/gen/device/g"}
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
	noTools := &genExpander{&genRule{labels: []genLabel{{name: "a", files: []string{"/a"}}}}, "/g"}
	if _, err := noTools.expand("$(location)"); err == nil || !strings.Contains(err.Error(), "at least one `tools` or `tool_files` is required") {
		t.Errorf("$(location) without tools: %v", err)
	}
	// an unresolved tool fails the command that names it
	bad := &genExpander{&genRule{hasTools: true, labels: []genLabel{{name: "t", err: "tool t is a rust_binary_host (unsupported)"}}}, "/g"}
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
	cfg := Config{Arch: "x86_64", PointerBits: 64}
	g := &Graph{tree: tree, out: out, cfg: cfg, nodes: map[key]*Node{}, configTypes: map[string]string{}}
	for side, osName := range []string{"android", "linux_glibc"} {
		mods, problems := (&evaluator{cfg: cfg, os: osName}).modules(parsed)
		if len(problems) > 0 {
			t.Fatal(problems)
		}
		g.mods[side] = mods
	}
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
	if got := (&genExpander{a.rule, genDir("gen_a")}).words(a.rule.labels[0]); len(got) != 1 || got[0] != "python3 p/tool.py" {
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
	// the command, then Soong's check that every declared output exists
	wantScript := "cp p/in.txt " + out + "\nrc=$?\n[ $rc -eq 0 ] || exit $rc\nfor f in " + out +
		"; do\n  [ -e \"$f\" ] || { echo \"error: genrule g: $f was not created\" >&2; exit 1; }\ndone\n"
	if got := g.genScript(n); got != wantScript {
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
	script := g.genScript(n)
	if want := "echo 'error: genrule g: srcs :absent unresolved' >&2\nexit 1\n"; script != want {
		t.Errorf("script %q, want %q", script, want)
	}
	for _, p := range g.problems {
		if strings.Contains(p, "not supported yet") {
			t.Errorf("stale problem %q", p)
		}
	}
}
