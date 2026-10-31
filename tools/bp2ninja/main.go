// SPDX-License-Identifier: MIT

// Command bp2ninja converts the Android.bp modules some goals need into one ninja file that builds them
// for a Linux glibc host (plan D7-2; M2). Runtime libraries come from each module's android source set,
// build tools (aidl, aprotoc, genrule tools) from their host source set. Only the goals' dependency closure is emitted.
//
// Usage: bp2ninja -tree <aosp tree> -out <build dir> -arch x86_64 -pointer-bits 64 -cc clang -cxx clang++ -ar llvm-ar -rustc rustc -rust-version x.y.z goal...
//
// Device modules link Soong's implicit system_shared_libs; a module in the -provided file (built by xtask: our crate, an upstream) stands in for its
// Android.bp module: linked first, its headers on the dependents' system include path, its defines on their compiles. A module in the -srcs file is built
// without the sources its row lists, linking the row's provider (if any) first for their symbols.
//
// -aconfig and -release (given together) build aconfig flag libraries (aconfig.go: cc_aconfig_library, java_aconfig_library) from the release's values.
// Java goals (java.go: java_library and the types its closure needs, dex jars for device) also need them, -jdk and -build-tools, and -metalava where
// API stubs are built; with them, a goal that names a Java module builds as one. -measure writes <out>/measure.txt (measure.go) instead of a build.
//
// Writes <out>/build.ninja, <out>/problems.txt (missing modules, unsupported module types and properties), <out>/missing.txt (the
// module references behind the "not found" problems) and <out>/missing-paths.txt (tree directories named by path that do not
// exist), both one per line, unique and sorted: what the caller may fetch; and <out>/graph.json (graph.go), the cc graph by module directory for
// the caller's failure triage. Exits 0 even with problems, which the caller reports.
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
	variant := flag.String("arch-variant", "", "Soong arch variant of the device side (its arches.toml soong_arch_variant; empty = Soong's baseline)")
	cc := flag.String("cc", "clang", "C compiler")
	cxx := flag.String("cxx", "clang++", "C++ compiler and linker driver")
	ar := flag.String("ar", "llvm-ar", "archiver")
	bison := flag.String("bison", "bison", "parser generator for .yy/.y sources")
	flex := flag.String("flex", "flex", "lexer generator for .ll/.l sources")
	yasm := flag.String("yasm", "yasm", "assembler for .asm (NASM syntax) sources, Soong's yasm rule")
	rustc := flag.String("rustc", "rustc", "Rust compiler of Rust modules (rust.go), with its own sysroot")
	rustVersion := flag.String("rust-version", "", "-rustc's version x.y.z (Soong's ANDROID_RUST_VERSION)")
	omit := flag.String("omit", "", "comma-separated modules whose dependencies are dropped (providers.toml `none`)")
	providedFile := flag.String("provided", "", "JSON file: module -> {lib, include, defines} of modules built outside the tree (providers.toml `crate:` and `upstream:`, provided.go)")
	srcsFile := flag.String("srcs", "", "JSON file: module -> {srcs, provider} of modules built without some of their sources (providers.toml rows with `srcs`, provided.go Partial); a listed file that is not a source of the module fails")
	var jt JavaTools
	flag.StringVar(&jt.JDK, "jdk", "", "Java goals: JDK home (Soong's ANDROID_JAVA_HOME, prebuilts/jdk/jdk21/<host tag>)")
	flag.StringVar(&jt.BuildTools, "build-tools", "", "Java goals: directory holding soong_zip, merge_zips, zip2zip, zipalign")
	flag.StringVar(&jt.Metalava, "metalava", "", "Java goals: metalava.jar droidstubs run")
	aconfig := flag.String("aconfig", "", "aconfig binary (aconfig flag libraries; Java goals)")
	release := flag.String("release", "", "build/release config (TARGET_RELEASE): build flags and aconfig values; given with -aconfig")
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
	provided, err := readJSON[Provided](*providedFile)
	if err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja: -provided:", err)
		os.Exit(1)
	}
	partial, err := readJSON[Partial](*srcsFile)
	if err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja: -srcs:", err)
		os.Exit(1)
	}
	if err := run(*tree, *out, Config{Arch: *arch, PointerBits: *bits, ArchVariant: *variant}, Tools{*cc, *cxx, *ar, *bison, *flex, *yasm, *rustc, *rustVersion}, javaSetup{jt, *aconfig, *release, *measure}, omitted, provided, partial, flag.Args()); err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja:", err)
		os.Exit(1)
	}
}

// javaSetup: the Java tools (read only when given), the aconfig tool and the release config.
type javaSetup struct {
	tools            JavaTools
	aconfig, release string
	measure          bool
}

func run(tree, out string, cfg Config, tools Tools, js javaSetup, omit map[string]bool, provided map[string]Provided, partial map[string]Partial, goals []string) error {
	files, err := discover(tree)
	if err != nil {
		return err
	}
	soong, err := readSoongConfig(tree)
	if err != nil {
		return err
	}
	if soong.Device, err = readDeviceArch(tree, cfg); err != nil {
		return err
	}
	if soong.Yasm, soong.NoYasm, err = readYasmFlags(tree, cfg.Arch); err != nil {
		return err
	}
	if soong.RustFeatures, err = readRustFeatureGate(tree); err != nil {
		return err
	}
	g := &Graph{tree: tree, out: out, cfg: cfg, soong: soong, omit: omit, provided: provided, partial: partial, nodes: map[key]*Node{}}
	for side, osName := range []string{"android", "linux_glibc"} {
		ev := &evaluator{cfg: cfg, os: osName}
		mods, problems := ev.modules(files)
		g.mods[side] = mods
		for _, p := range problems {
			g.problem("%s eval: %s", Side(side), p)
		}
	}
	g.baseTypes = map[string]string{}
	for _, list := range g.mods[Device] {
		for _, m := range list {
			if m.Type == "soong_config_module_type" {
				g.baseTypes[m.Name] = m.Props.str("module_type")
			}
		}
	}
	art, err := readArtPlugin(tree, cfg)
	if err != nil {
		return err
	}
	g.loadArt(art)
	// providers.toml's own check cannot see the tree; this one can
	names := sortedKeys(omit)
	for m := range provided {
		names = append(names, m)
	}
	for m := range partial {
		names = append(names, m)
	}
	for _, m := range names {
		if len(g.mods[Device][m])+len(g.mods[Host][m]) == 0 {
			g.problem("providers.toml: %s names no module in the tree", m)
		}
	}
	if (js.aconfig == "") != (js.release == "") {
		return fmt.Errorf("-aconfig and -release are given together")
	}
	if js.release != "" {
		if g.release, err = readRelease(tree, js.release); err != nil {
			return err
		}
		g.acfg = &aconfigGraph{tool: js.aconfig, decls: map[string]*aconfigDecl{}}
	}
	// Java tools given: Java goals are built (java.go)
	if js.tools.JDK != "" {
		if err := g.initJava(js.tools); err != nil {
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
	var ccGoals []*Node
	for _, goal := range goals {
		if g.java != nil {
			if m := g.lookupJava(goal, Device); m != nil && g.isJava(m.Type) {
				if n := g.jnode(goal, Device, "goal"); n != nil {
					g.java.outs = append(g.java.outs, n.output()...)
				}
				continue
			}
		}
		if n := g.node(goal, Device, Shared, "goal"); n != nil {
			ccGoals = append(ccGoals, n)
		}
	}
	if err := g.checkPartial(); err != nil {
		return err
	}
	ninja := g.emit(tools)
	if g.java != nil {
		ninja += g.java.ninja(g)
	}
	for dir, set := range map[string][]compatHeader{"cxx": cxxCompat, "device": deviceCompat} {
		for _, h := range set {
			link := filepath.Join(out, "compat", dir, h.name)
			if err := os.MkdirAll(filepath.Dir(link), 0o755); err != nil {
				return err
			}
			os.Remove(link)
			if err := os.Symlink(filepath.Join(tree, h.path), link); err != nil {
				return err
			}
		}
	}
	if err := g.writeGenFiles(); err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(out, "build.ninja"), []byte(ninja), 0o644); err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(out, "problems.txt"), []byte(strings.Join(g.problems, "\n")+"\n"), 0o644); err != nil {
		return err
	}
	graph, err := g.graphJSON(ccGoals)
	if err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(out, "graph.json"), graph, 0o644); err != nil {
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
