// SPDX-License-Identifier: MIT

// Command bp2ninja converts the Android.bp modules some goals need into one ninja file that builds them
// for a Linux glibc host (plan D7-2; M2). Runtime libraries come from each module's android source set,
// build tools (aidl) from their host source set. Only the goals' dependency closure is emitted.
//
// Usage: bp2ninja -tree <aosp tree> -out <build dir> -arch x86_64 -cc clang -cxx clang++ -ar llvm-ar goal...
//
// Writes <out>/build.ninja and <out>/problems.txt (missing modules, unsupported module types and
// properties); exits 0 even with problems, which the caller reports.
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
	arch := flag.String("arch", "x86_64", "Soong arch name of the host")
	cc := flag.String("cc", "clang", "C compiler")
	cxx := flag.String("cxx", "clang++", "C++ compiler and linker driver")
	ar := flag.String("ar", "llvm-ar", "archiver")
	bison := flag.String("bison", "bison", "parser generator for .yy/.y sources")
	flex := flag.String("flex", "flex", "lexer generator for .ll/.l sources")
	flag.Parse()
	if *tree == "" || *out == "" || flag.NArg() == 0 {
		flag.Usage()
		os.Exit(2)
	}
	if err := run(*tree, *out, Config{Arch: *arch}, Tools{*cc, *cxx, *ar, *bison, *flex}, flag.Args()); err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja:", err)
		os.Exit(1)
	}
}

func run(tree, out string, cfg Config, tools Tools, goals []string) error {
	files, err := discover(tree)
	if err != nil {
		return err
	}
	soong, err := readSoongConfig(tree)
	if err != nil {
		return err
	}
	g := &Graph{tree: tree, out: out, cfg: cfg, soong: soong, nodes: map[key]*Node{}}
	for side, osName := range []string{"android", "linux_glibc"} {
		ev := &evaluator{cfg: cfg, os: osName}
		mods, problems := ev.modules(files)
		g.mods[side] = mods
		for _, p := range problems {
			g.problem("%s eval: %s", Side(side), p)
		}
	}
	for _, goal := range goals {
		g.node(goal, Device, Shared, "goal")
	}
	ninja := g.emit(tools)
	compat := filepath.Join(out, "compat", "cxx")
	if err := os.MkdirAll(compat, 0o755); err != nil {
		return err
	}
	for _, h := range cxxCompat {
		link := filepath.Join(compat, filepath.Base(h))
		os.Remove(link)
		if err := os.Symlink(filepath.Join(tree, h), link); err != nil {
			return err
		}
	}
	if err := os.WriteFile(filepath.Join(out, "build.ninja"), []byte(ninja), 0o644); err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(out, "problems.txt"), []byte(strings.Join(g.problems, "\n")+"\n"), 0o644); err != nil {
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
