// SPDX-License-Identifier: MIT

package main

import (
	"flag"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"testing"
	"time"
)

// fakeConverterEnv: the test binary, run with it set, is a converter whose conversion is discover + one -provided JSON (TestRegenerationIsExact): the real regen path (expandArgs, the recorder, the edge, the depfile) with a tree that needs no Soong sources.
const fakeConverterEnv = "BP2NINJA_TEST_CONVERTER"

func TestMain(m *testing.M) {
	if os.Getenv(fakeConverterEnv) == "1" {
		if err := fakeConvert(); err != nil {
			fmt.Fprintln(os.Stderr, "fake converter:", err)
			os.Exit(1)
		}
		os.Exit(0)
	}
	os.Exit(m.Run())
}

func fakeConvert() error {
	if len(os.Args) == 4 && os.Args[1] == scanLoadsFlag {
		return scanLoads(os.Args[2], os.Args[3])
	}
	argv, regen, err := expandArgs(os.Args[1:])
	if err != nil {
		return err
	}
	if regen == nil {
		return fmt.Errorf("want %s or %s", argsFlag, bootstrapFlag)
	}
	fs := flag.NewFlagSet("fake", flag.ContinueOnError)
	tree, out, provided := fs.String("tree", "", ""), fs.String("out", "", ""), fs.String("provided", "", "")
	scan := fs.String("scan", "", "tree file the loadLibrary scan edge reads, as bootJniLibs' boot jar sources")
	if err := fs.Parse(argv); err != nil {
		return err
	}
	buildDir = *out
	if regen.bootstrap {
		return regen.writeBootstrap(*out)
	}
	files, err := discover(*tree)
	if err != nil {
		return err
	}
	if _, err := readJSON[Provided](*provided); err != nil {
		return err
	}
	// a look into the build dir, as the real conversion's stats of provided and generated files: no input, though this run's own writes change the dir
	statPath(filepath.Join(*out, "conversions"))
	for _, name := range regenOutputs[1:] {
		if err := os.WriteFile(filepath.Join(*out, name), nil, 0o644); err != nil {
			return err
		}
	}
	ninja := fmt.Sprintf("rule touch\n  command = touch $out\nbuild product: touch\n# %d blueprint files, goals %v, loads %v\n", len(files), fs.Args(), sortedKeys(readBootLoads()))
	if *scan != "" {
		g := &Graph{bootLoads: []string{filepath.Join(*tree, *scan)}}
		ninja += g.bootLoadsNinja(regen.exe)
		regen.implicit = append(regen.implicit, filepath.Join(buildDir, bootLoadsFile))
	}
	ninja += regen.ninja()
	if err := os.WriteFile(filepath.Join(*out, "build.ninja"), []byte(ninja), 0o644); err != nil {
		return err
	}
	log, err := os.OpenFile(filepath.Join(*out, "conversions"), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return err
	}
	fmt.Fprintln(log, "converted")
	log.Close()
	return regen.writeDepfile(*out)
}

// Each input class of the conversion reruns it exactly once through ninja, and a no-change run or an edit to a file it never read reruns nothing.
// Classes: a blueprint file read, a new or removed one (a listed directory's entries), the argument file (flags, goals: the tables xtask turns into them), a JSON input (-provided), the converter binary.
func TestRegenerationIsExact(t *testing.T) {
	self, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	dir := t.TempDir()
	tree, out := filepath.Join(dir, "tree"), filepath.Join(dir, "out")
	conv := filepath.Join(dir, "conv")
	write := func(path, text string, mode os.FileMode) {
		t.Helper()
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(text), mode); err != nil {
			t.Fatal(err)
		}
	}
	bin, err := os.ReadFile(self)
	if err != nil {
		t.Fatal(err)
	}
	write(conv, string(bin), 0o755)
	write(filepath.Join(tree, "a/Android.bp"), "cc_library { name: \"a\" }\n", 0o644)
	write(filepath.Join(tree, "a/a.c"), "int a;\n", 0o644)
	// a directory name ninja's depfile cannot spell (blueprint's testdata has `escapes/**/b`): walked, so recorded
	write(filepath.Join(tree, "t/**/b/x.txt"), "x\n", 0o644)
	provided := filepath.Join(out, "provided.json")
	write(provided, "{}\n", 0o644)
	args := filepath.Join(out, "bp2ninja.args")
	javaSrc := filepath.Join(tree, "a/A.java")
	write(javaSrc, "class A { static { System.loadLibrary(\"x\"); } }\n", 0o644)
	argLines := "-tree\n" + tree + "\n-out\n" + out + "\n-provided\n" + provided + "\n-scan\na/A.java\n"
	write(args, argLines+"a\n", 0o644)
	env := append(os.Environ(), fakeConverterEnv+"=1")
	run := func(name string, argv ...string) string {
		t.Helper()
		c := exec.Command(name, argv...)
		c.Env = env
		b, err := c.CombinedOutput()
		if err != nil {
			t.Fatalf("%s %v: %v\n%s", name, argv, err, b)
		}
		return string(b)
	}
	conversions := func() int {
		b, _ := os.ReadFile(filepath.Join(out, "conversions"))
		return strings.Count(string(b), "\n")
	}
	run(conv, bootstrapFlag, args)
	if n := conversions(); n != 0 {
		t.Fatalf("bootstrap converted (%d): it only writes the edge", n)
	}
	run("ninja", "-C", out)
	// 2: the first conversion has no scan output yet (no loadLibrary names); its build.ninja holds the scan edge, whose output reruns it
	if n := conversions(); n != 2 {
		t.Fatalf("first ninja run: %d conversions, want 2", n)
	}
	loads := func(want string) {
		t.Helper()
		if b, _ := os.ReadFile(filepath.Join(out, "build.ninja")); !strings.Contains(string(b), "loads "+want+"\n") {
			t.Errorf("build.ninja does not hold loads %s", want)
		}
	}
	loads("[x]")
	noWork := func(after string) {
		t.Helper()
		if b := run("ninja", "-C", out, "-n", "-d", "explain"); !strings.Contains(b, "no work to do") {
			t.Errorf("after %s, a no-change ninja run would run steps:\n%s", after, b)
		}
	}
	noWork("the first run")
	// file timestamps come from the kernel's coarse clock (ticks of up to 10 ms): the sleep puts each edit's mtime past the last conversion's outputs
	step := func(what string, edit func(), want int) {
		t.Helper()
		time.Sleep(50 * time.Millisecond)
		before := conversions()
		edit()
		run("ninja", "-C", out)
		if got := conversions() - before; got != want {
			t.Errorf("%s: %d conversions, want %d", what, got, want)
		}
		noWork(what)
	}
	step("no change", func() {}, 0)
	step("a source file the conversion never read", func() { write(filepath.Join(tree, "a/a.c"), "int b;\n", 0o644) }, 0)
	step("a blueprint file read", func() { write(filepath.Join(tree, "a/Android.bp"), "cc_library { name: \"b\" }\n", 0o644) }, 1)
	step("a new blueprint file in a new directory", func() { write(filepath.Join(tree, "b/c/Android.bp"), "cc_library { name: \"c\" }\n", 0o644) }, 1)
	step("a removed blueprint file", func() {
		if err := os.Remove(filepath.Join(tree, "b/c/Android.bp")); err != nil {
			t.Fatal(err)
		}
	}, 1)
	step("the argument file (a goal added)", func() { write(args, argLines+"a\nb\n", 0o644) }, 1)
	step("a JSON input", func() { write(provided, "{\"x\": {}}\n", 0o644) }, 1)
	step("the converter binary", func() { write(conv, string(bin), 0o755) }, 1)
	// the boot jars' sources reach the conversion only through the scan edge's names (bootclasspath.go readBootLoads)
	step("a scanned source edit that keeps its loadLibrary names", func() {
		write(javaSrc, "class A { int f; static { System.loadLibrary(\"x\"); } }\n", 0o644)
	}, 0)
	step("a scanned source edit that adds a loadLibrary name", func() {
		write(javaSrc, "class A { static { System.loadLibrary(\"x\"); System.loadLibrary(\"y\"); } }\n", 0o644)
	}, 1)
	loads("[x y]")
}

// Every filesystem access of the conversion goes through the recorder (inputs.go), so the depfile cannot miss an input class: a raw call outside the listed sites fails here.
// Listed: the recorder itself, walkTree (records each listed directory itself), and the writes that read back their own output to write only on change (compat/, generated files), which are outputs, not inputs.
func TestFilesystemReadsAreRecorded(t *testing.T) {
	raw := regexp.MustCompile(`\bos\.(ReadFile|Open|Stat|Lstat|ReadDir|Readlink)\(|\bfilepath\.(Glob|EvalSymlinks|WalkDir|Walk)\(|\bexec\.Command\(|\bparser\.ParseFile\(`)
	allowed := map[string]int{
		"inputs.go":    12, // the wrappers
		"walk.go":      7,  // records each directory it lets WalkDir list
		"cc.go":        4,  // writeCompat: placeLink, placeFile, pruneCompat (outputs)
		"genrule.go":   1,  // writeGenFiles (outputs)
		"artplugin.go": 1,  // parses sources already read into memory
	}
	files, err := filepath.Glob("*.go")
	if err != nil {
		t.Fatal(err)
	}
	for _, f := range files {
		if strings.HasSuffix(f, "_test.go") {
			continue
		}
		b, err := os.ReadFile(f)
		if err != nil {
			t.Fatal(err)
		}
		var hits []string
		for i, line := range strings.Split(string(b), "\n") {
			if trimmed := strings.TrimSpace(line); !strings.HasPrefix(trimmed, "//") && raw.MatchString(line) {
				hits = append(hits, fmt.Sprintf("%s:%d: %s", f, i+1, trimmed))
			}
		}
		if len(hits) != allowed[f] {
			t.Errorf("%s: %d raw filesystem calls, %d listed: route a new read through inputs.go, or list a write-if-changed read here\n%s", f, len(hits), allowed[f], strings.Join(hits, "\n"))
		}
	}
}

// The recorder's rules (inputs.go): a read file is itself; a stat'ed path its parent; a missing one its nearest existing ancestor; a listing the directory; walkTree the directories it lists, not the ones fn skips.
func TestRecorderRules(t *testing.T) {
	dir := t.TempDir()
	for _, d := range []string{"a/skip", "a/keep"} {
		if err := os.MkdirAll(filepath.Join(dir, d), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	f := filepath.Join(dir, "a/keep/f.txt")
	if err := os.WriteFile(f, []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	saved := inputs
	defer func() { inputs = saved }()
	check := func(what string, call func(), want ...string) {
		t.Helper()
		inputs = map[string]bool{}
		call()
		var got []string
		for p := range inputs {
			got = append(got, strings.TrimPrefix(p, dir))
		}
		slices.Sort(got)
		slices.Sort(want)
		if !slices.Equal(got, want) {
			t.Errorf("%s: recorded %v, want %v", what, got, want)
		}
	}
	check("a file read", func() { readFile(f) }, "/a/keep/f.txt")
	buildDir = filepath.Join(dir, "a/keep")
	check("a file read in the build dir", func() { readFile(f) }, "/a/keep/f.txt")
	check("a stat in the build dir", func() { statPath(f) })
	buildDir = ""
	check("a missing file read", func() { readFile(filepath.Join(dir, "a/no/such")) }, "/a")
	check("a stat", func() { statPath(f) }, "/a/keep")
	check("a missing stat", func() { statPath(filepath.Join(dir, "a/keep/x/y")) }, "/a/keep")
	check("a listing", func() { readDir(filepath.Join(dir, "a")) }, "/a")
	check("a glob", func() { glob(filepath.Join(dir, "a/keep/*.txt")) }, "/a/keep")
	check("a walk", func() {
		walkTree(dir, func(path string, d os.DirEntry, err error) error {
			if d != nil && d.IsDir() && d.Name() == "skip" {
				return filepath.SkipDir
			}
			return nil
		})
	}, "", "/a", "/a/keep")
}
