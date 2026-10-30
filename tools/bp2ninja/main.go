// SPDX-License-Identifier: MIT

// Command bp2ninja converts the Android.bp modules some goals need into one ninja file that builds them
// for a Linux glibc host (plan D7-2; M2). Runtime libraries come from each module's android source set,
// build tools (aidl, aprotoc, genrule tools) from their host source set. Only the goals' dependency closure is emitted.
//
// Usage: bp2ninja -tree <aosp tree> -out <build dir> -arch x86_64 -pointer-bits 64 -cc clang -cxx clang++ -ar llvm-ar goal...
//
// Java goals (java.go: java_library and the types its closure needs, dex jars for device) also need -jdk, -build-tools, -aconfig and
// -release, and -metalava where API stubs are built; with them, a goal that names a Java module builds as one. -measure writes
// <out>/measure.txt (measure.go) instead of a build.
//
// Writes <out>/build.ninja, <out>/problems.txt (missing modules, unsupported module types and properties), <out>/missing.txt (the
// module references behind the "not found" problems) and <out>/missing-paths.txt (tree directories named by path that do not
// exist), both one per line, unique and sorted: what the caller may fetch. Exits 0 even with problems, which the caller reports.
package main

import (
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

func main() {
	tree := flag.String("tree", "", "AOSP source tree (absolute)")
	out := flag.String("out", "", "build directory (absolute)")
	arch := flag.String("arch", "", "Soong arch name of the host (arches.toml row key)")
	bits := flag.Int("pointer-bits", 0, "pointer width of -arch (its arches.toml pointer_bits)")
	cc := flag.String("cc", "clang", "C compiler")
	cxx := flag.String("cxx", "clang++", "C++ compiler and linker driver")
	ar := flag.String("ar", "llvm-ar", "archiver")
	bison := flag.String("bison", "bison", "parser generator for .yy/.y sources")
	flex := flag.String("flex", "flex", "lexer generator for .ll/.l sources")
	omit := flag.String("omit", "", "comma-separated modules whose dependencies are dropped (providers.toml `none`)")
	var jt JavaTools
	flag.StringVar(&jt.JDK, "jdk", "", "Java goals: JDK home (Soong's ANDROID_JAVA_HOME, prebuilts/jdk/jdk21/<host tag>)")
	flag.StringVar(&jt.BuildTools, "build-tools", "", "Java goals: directory holding soong_zip, merge_zips, zip2zip, zipalign")
	flag.StringVar(&jt.Metalava, "metalava", "", "Java goals: metalava.jar droidstubs run")
	flag.StringVar(&jt.Aconfig, "aconfig", "", "Java goals: aconfig binary")
	release := flag.String("release", "", "Java goals: build/release config (TARGET_RELEASE): build flags and aconfig values")
	measure := flag.Bool("measure", false, "write <out>/measure.txt (module types and source kinds the goals reach, measure.go) instead of a build")
	flag.Parse()
	if *tree == "" || *out == "" || *arch == "" || (*bits != 32 && *bits != 64) || flag.NArg() == 0 {
		flag.Usage()
		os.Exit(2)
	}
	omitted := map[string]bool{}
	for _, m := range strings.Split(*omit, ",") {
		if m != "" {
			omitted[m] = true
		}
	}
	if err := run(*tree, *out, Config{Arch: *arch, PointerBits: *bits}, Tools{*cc, *cxx, *ar, *bison, *flex}, javaSetup{jt, *release, *measure}, omitted, flag.Args()); err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja:", err)
		os.Exit(1)
	}
}

// javaSetup: the Java tools and release config; read only when a goal is a Java module.
type javaSetup struct {
	tools   JavaTools
	release string
	measure bool
}

func run(tree, out string, cfg Config, tools Tools, js javaSetup, omit map[string]bool, goals []string) error {
	files, err := discover(tree)
	if err != nil {
		return err
	}
	soong, err := readSoongConfig(tree)
	if err != nil {
		return err
	}
	g := &Graph{tree: tree, out: out, cfg: cfg, soong: soong, omit: omit, nodes: map[key]*Node{}}
	for side, osName := range []string{"android", "linux_glibc"} {
		ev := &evaluator{cfg: cfg, os: osName}
		mods, problems := ev.modules(files)
		g.mods[side] = mods
		for _, p := range problems {
			g.problem("%s eval: %s", Side(side), p)
		}
	}
	g.configTypes = map[string]string{}
	for _, list := range g.mods[Device] {
		for _, m := range list {
			if m.Type == "soong_config_module_type" {
				g.configTypes[m.Name] = m.Props.str("module_type")
			}
		}
	}
	// providers.toml's own check cannot see the tree; this one can
	for m := range omit {
		if len(g.mods[Device][m])+len(g.mods[Host][m]) == 0 {
			g.problem("providers.toml: %s names no module in the tree", m)
		}
	}
	// Java tools given: Java goals are built (java.go)
	if js.tools.JDK != "" {
		if err := g.initJava(js); err != nil {
			return err
		}
	}
	if js.measure {
		if g.java == nil {
			return fmt.Errorf("-measure needs the Java tools (-jdk …)")
		}
		if err := os.MkdirAll(out, 0o755); err != nil {
			return err
		}
		return os.WriteFile(filepath.Join(out, "measure.txt"), []byte(g.measure(goals)), 0o644)
	}
	for _, goal := range goals {
		if g.java != nil {
			if m := g.lookupJava(goal, Device); m != nil && g.isJava(m.Type) {
				if n := g.jnode(goal, Device, "goal"); n != nil {
					g.java.outs = append(g.java.outs, n.output()...)
				}
				continue
			}
		}
		g.node(goal, Device, Shared, "goal")
	}
	ninja := g.emit(tools)
	if g.java != nil {
		ninja += g.java.ninja(g)
	}
	compat := filepath.Join(out, "compat", "cxx")
	for name, h := range cxxCompat {
		link := filepath.Join(compat, name)
		if err := os.MkdirAll(filepath.Dir(link), 0o755); err != nil {
			return err
		}
		os.Remove(link)
		if err := os.Symlink(filepath.Join(tree, h), link); err != nil {
			return err
		}
	}
	if err := g.writeScripts(); err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(out, "build.ninja"), []byte(ninja), 0o644); err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(out, "problems.txt"), []byte(strings.Join(g.problems, "\n")+"\n"), 0o644); err != nil {
		return err
	}
	// always written, empty included: a stale list of an earlier run must never be read as current
	for file, set := range map[string]map[string]bool{"missing.txt": g.missing, "missing-paths.txt": g.missingPaths} {
		if err := os.WriteFile(filepath.Join(out, file), []byte(strings.Join(sortedKeys(set), "\n")+"\n"), 0o644); err != nil {
			return err
		}
	}
	srcs := 0
	for _, n := range g.order {
		srcs += len(n.srcs)
	}
	fmt.Printf("bp2ninja: %d blueprint files, %d nodes, %d sources, %d problems -> %s\n",
		len(files), len(g.order), srcs, len(g.problems), filepath.Join(out, "build.ninja"))
	return nil
}
