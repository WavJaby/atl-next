// SPDX-License-Identifier: MIT

// Command bp2ninja converts the Android.bp modules some goals need into one ninja file that builds them
// for a Linux glibc host (plan D7-2; M2). Runtime libraries come from each module's android source set,
// build tools (aidl) from their host source set. Only the goals' dependency closure is emitted.
//
// Usage: bp2ninja -tree <aosp tree> -out <build dir> -arch x86_64 -pointer-bits 64 -cc clang -cxx clang++ -ar llvm-ar goal...
//
// Writes <out>/build.ninja, <out>/problems.txt (missing modules, unsupported module types and
// properties) and <out>/missing.txt (the module references behind the "not found" problems, one per
// line, unique and sorted: what the caller may fetch); exits 0 even with problems, which the caller reports.
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
	if err := run(*tree, *out, Config{Arch: *arch, PointerBits: *bits}, Tools{*cc, *cxx, *ar, *bison, *flex}, omitted, flag.Args()); err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja:", err)
		os.Exit(1)
	}
}

func run(tree, out string, cfg Config, tools Tools, omit map[string]bool, goals []string) error {
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
	for _, goal := range goals {
		g.node(goal, Device, Shared, "goal")
	}
	ninja := g.emit(tools)
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
	if err := os.WriteFile(filepath.Join(out, "missing.txt"), []byte(strings.Join(g.missingRefs(), "\n")+"\n"), 0o644); err != nil {
		return err
	}
	srcs := 0
	for _, n := range g.order {
		srcs += len(n.srcs)
	}
	fmt.Printf("bp2ninja: %d blueprint files, %d nodes, %d sources, %d problems -> %s\n",
		len(files), len(g.order), srcs, len(g.problems), filepath.Join(out, "build.ninja"))
	return nil
}
