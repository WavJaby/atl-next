// SPDX-License-Identifier: MIT

// Command bp2ninja converts the Android.bp modules some goals need into one ninja file that builds them
// for a Linux glibc host (plan D7-2; M2). Runtime libraries come from each module's android source set,
// build tools (aidl, aprotoc, genrule tools) from their host source set. Only the goals' dependency closure is emitted.
//
// Usage: bp2ninja -tree <aosp tree> -out <build dir> -arch x86_64 -pointer-bits 64 -cc clang -cxx clang++ -ar llvm-ar -rustc rustc -rust-version x.y.z -libclang <dir> goal...
//
// Device modules link Soong's implicit system_shared_libs; a module in the -provided file (built by xtask: our crate, an upstream) stands in for its
// Android.bp module: linked first, its headers on the dependents' system include path, its defines on their compiles. A module in the -srcs file is built
// without the sources its row lists, linking the row's provider (if any) first for their symbols. A device Rust module in -rust-android takes its android target blocks and --cfg atl_android_userspace (rust.go).
//
// -release builds aconfig flag libraries (aconfig.go: cc_aconfig_library, java_aconfig_library) from the release's values, with the tree's aconfig, and
// writes <out>/gen/build.prop (buildprop.go).
// Java goals (java.go: java_library and the types its closure needs, dex jars for device) also need them, -jdk and -build-tools; with them, a
// goal that names a Java module builds as one (its host variant for a host-only type). Host Java tools, metalava included, are built from the tree. -measure writes <out>/measure.txt (measure.go) instead of a build.
// -device-cxx-flags <dir> (no goals) writes <out>/compat and prints the C++ command line every device compile in <dir> shares (probeCxxFlags), for xtask check step libcxx, instead of a build.
//
// Writes <out>/build.ninja, <out>/problems.txt (missing modules, unsupported module types and properties), <out>/missing.txt (the
// module references behind the "not found" problems) and <out>/missing-paths.txt (tree directories named by path that do not
// exist), both one per line, unique and sorted: what the caller may fetch; and <out>/graph.json (graph.go), the cc graph by module directory for
// the caller's failure triage. Exits 0 even with problems, which the caller reports.
package main

import (
	"encoding/json"
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
	abi := flag.String("abi", "", "the first android_abis entry of -arch's arches.toml guest row: lib/<abi>/ in the apks of android apps that have jni_libs")
	nbArch := flag.String("native-bridge", "", "guest arch of the native bridge side (arches.toml row with [guest]): android properties + target.native_bridge built against bionic into <out>/nb/<arch>; goals `nb:<module>` build there; empty = no native bridge side")
	nbBits := flag.Int("native-bridge-bits", 0, "pointer width of -native-bridge (its arches.toml pointer_bits)")
	nbArchVariant := flag.String("native-bridge-arch-variant", "", "Soong arch variant of the native bridge side (its arches.toml soong_arch_variant; empty = Soong's baseline, which arm and arm64 lack); needs -native-bridge")
	buildProvidedFile := flag.String("build-provided", "", "cross host: JSON file like -provided: the same rows built for the build machine, which the build side links; given exactly with the -host-* flags")
	nbProvidedFile := flag.String("nb-provided", "", "JSON file like -provided: the native bridge side's provided modules (the LLVM runtimes built for the guest: libc++_static_noexcept, libc++demangle_noexcept, libunwind, libclang_rt.builtins)")
	rtInclude := flag.String("clang-rt-include", "", "compiler-rt's include dir (sanitizer/, xray/ headers) for the -native-bridge side's compiles: prebuilts/clang's resource dir carries it, the distro clang's lacks it (bionic's heap_tagging.cpp includes sanitizer/hwasan_interface.h)")
	bionicStaticRust := flag.String("bionic-static-rust", "", "crate root (crates/atl-guest-dlsym/src/lib.rs) compiled into the Rust staticlib of every static bionic executable: the dlsym rustup's std panic_abort references, which a static bionic executable has no libdl for")
	cc := flag.String("cc", "clang", "C compiler")
	cxx := flag.String("cxx", "clang++", "C++ compiler and linker driver")
	ar := flag.String("ar", "llvm-ar", "archiver")
	bison := flag.String("bison", "bison", "parser generator for .yy/.y sources")
	flex := flag.String("flex", "flex", "lexer generator for .ll/.l sources")
	m4 := flag.String("m4", "m4", "macro processor flex runs, set as its M4 (Soong's lex rule sets M4= beside flex)")
	yasm := flag.String("yasm", "yasm", "assembler for .asm (NASM syntax) sources, Soong's yasm rule")
	licenceGate := flag.String("licence-gate", "", "command of the licence gate edge over the device link outputs (licence.go): reads them from $out.rsp, writes its report to $out and its depfile to $out.d; empty = no edge")
	launcher := flag.String("launcher", "", "compiler cache run in front of every C/C++ compile (sccache); empty = compile directly")
	rustc := flag.String("rustc", "rustc", "Rust compiler of Rust modules (rust.go), with its own sysroot")
	rustVersion := flag.String("rust-version", "", "-rustc's version x.y.z (Soong's ANDROID_RUST_VERSION)")
	libclang := flag.String("libclang", "", "directory of -cc's libclang.so, which rust_bindgen's bindgen loads (LIBCLANG_PATH)")
	llvm := flag.String("llvm", "", "install prefix of -cc's LLVM tools (bin/llvm-addr2line): ART_CLANG_PATH of the host ART flags (artplugin.go)")
	omit := flag.String("omit", "", "comma-separated modules whose dependencies are dropped (providers.toml `none`)")
	providedFile := flag.String("provided", "", "JSON file: module -> {libs, include, defines} of modules built outside the tree (providers.toml `crate:`, `upstream:` and `system:`, provided.go)")
	isaTiersFile := flag.String("isa-tiers", "", "JSON file: module -> {src, march, gate} of isa-tiers.toml rows (isatier.go): one source of the module compiled with -march, on a cross host's product sides; needs the -host-* flags; the tier files' compiles go to isa-tier-probes.json in -out")
	aidlCratesFile := flag.String("aidl-crates", "", "JSON file: interface name -> {include_dirs, types, parcelables}: unstable aidl_interfaces defined at the tree root over the import closure of types, whose -rust backends workspace packages link (Cargo.toml `[package.metadata.atl.aidl]`, aidlcrates.go)")
	srcsFile := flag.String("srcs", "", "JSON file: module -> {srcs, provider} of modules built without some of their sources (providers.toml rows with `srcs`, provided.go Partial); a listed file that is not a source of the module fails")
	rustAndroid := flag.String("rust-android", "", "comma-separated device Rust modules compiled as Android userspace: android target blocks + --cfg "+rustAndroidCfg+" (providers.toml `rust_os = \"android\"`, rust.go rustOSSide)")
	defines := flag.String("defines", "", "comma-separated module:MACRO (NAME or NAME=VALUE): macros the module's device C/C++ compiles get as -D, read by a patch to its source (providers.toml `defines`, provided.go parseDefines)")
	standIn := flag.String("stand-in", "", "comma-separated module=tree-module pairs: the tree module is built wherever the module is named, and must install under its name (stem + suffix) (providers.toml `module:`)")
	var jt JavaTools
	flag.StringVar(&jt.JDK, "jdk", "", "Java goals: JDK home (Soong's ANDROID_JAVA_HOME, prebuilts/jdk/jdk21/<host tag>)")
	flag.StringVar(&jt.BuildTools, "build-tools", "", "Java goals: directory holding soong_zip, merge_zips, zip2zip, zipalign")
	flag.StringVar(&jt.Keys, "keys", "", "Java goals: key set dir android apps are signed with, <role>.x509.pem + <role>.pk8 per Soong certificate role, testkey the default role (Soong's DefaultAppCertificateDir); its testkey also decides ro.build.tags (buildprop.go)")
	release := flag.String("release", "", "build/release config (TARGET_RELEASE): build flags and aconfig values (aconfig flag libraries; Java goals)")
	measure := flag.Bool("measure", false, "write <out>/measure.txt (module types and source kinds the goals reach, measure.go) instead of a build")
	cxxProbe := flag.String("device-cxx-flags", "", "tree dir: instead of a build, write <out>/compat (the libc provider from -provided) and print as JSON {argv, android} the C++ command line a device module there gets with nothing of its own, and the -D__ANDROID__ define in it (xtask check step libcxx); takes no goals")
	upstreamToolchain := flag.Bool("upstream-toolchain", false, "print as JSON {libcxx, compile, link} and exit, for builds outside the converter that must compile and link like its host modules (xtask upstream.rs): the libc++ headers (stl.go libcxxProvider: the one -cxx searches, or a cross host's -host-libcxx) and the host target flags (soongflags.go HostCross); takes -cxx and the -host-* flags too")
	hostTarget := flag.String("host-target", "", "cross host (the build machine is another arch): the clang triple of the host's code (arches.toml clang); with -host-sysroot, -host-isa, -host-libcxx and -host-rust-target, all five or none")
	hostSysroot := flag.String("host-sysroot", "", "cross host: the target's sysroot directory (a fetched sysroot_debs row, deps.toml)")
	hostISA := flag.String("host-isa", "", "cross host: -march of the host's code (arches.toml isa)")
	hostLibcxx := flag.String("host-libcxx", "", "cross host: the sysroot's libc++ headers directory, derived by xtask (cross::libcxx_dir: the one c++/v1 holding __config); under -host-sysroot")
	hostRustTarget := flag.String("host-rust-target", "", "cross host: rustc --target of the host's product crates (arches.toml rust; its std is a rustup_target row)")
	hostRustFeatures := flag.String("host-rust-features", "", "cross host: rustc -C target-feature of the host's isa past the base (xtask arches::rustc_target_features, e.g. +zba,+zbb); may be empty")
	buildArch := flag.String("build-arch", "", "cross host: Soong arch name of the build machine (its arches.toml row, xtask arches::build_machine): the build side, which compiles every node a build rule executes or loads; given exactly with the -host-* flags")
	buildBits := flag.Int("build-pointer-bits", 0, "cross host: pointer width of -build-arch (its arches.toml pointer_bits)")
	nbExports := flag.String("nb-exports", "", "module: instead of a build, print as JSON the include flags its -native-bridge variant hands dependents (exportsOf), for builds outside the converter that compile against bionic's headers (xtask runtimes: libc_headers); takes no goals")
	flag.Usage = func() {
		fmt.Fprintf(flag.CommandLine.Output(), "usage: bp2ninja [flags] goal...\n       bp2ninja %s|%s <file>   (the flags and goals, one per line; build.ninja regenerates itself: regen.go)\n", argsFlag, bootstrapFlag)
		flag.PrintDefaults()
	}
	if len(os.Args) > 1 && os.Args[1] == checkGlobsFlag {
		if len(os.Args) != 4 {
			fmt.Fprintf(os.Stderr, "bp2ninja: %s <list> <result>\n", checkGlobsFlag)
			os.Exit(2)
		}
		if err := checkGlobs(os.Args[2], os.Args[3]); err != nil {
			fmt.Fprintln(os.Stderr, "bp2ninja:", err)
			os.Exit(1)
		}
		return
	}
	if len(os.Args) > 1 && os.Args[1] == scanLoadsFlag {
		if len(os.Args) != 4 {
			fmt.Fprintf(os.Stderr, "bp2ninja: %s <list> <out>\n", scanLoadsFlag)
			os.Exit(2)
		}
		if err := scanLoads(os.Args[2], os.Args[3]); err != nil {
			fmt.Fprintln(os.Stderr, "bp2ninja:", err)
			os.Exit(1)
		}
		return
	}
	argv, regen, err := expandArgs(os.Args[1:])
	if err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja:", err)
		os.Exit(2)
	}
	if err := flag.CommandLine.Parse(argv); err != nil {
		os.Exit(2)
	}
	if abs, err := filepath.Abs(*out); err == nil && *out != "" {
		buildDir = abs
	}
	if regen != nil && regen.bootstrap {
		if err := regen.writeBootstrap(*out); err != nil {
			fmt.Fprintln(os.Stderr, "bp2ninja:", err)
			os.Exit(1)
		}
		return
	}
	cross := HostCross{Target: *hostTarget, Sysroot: *hostSysroot, ISA: *hostISA, Libcxx: *hostLibcxx, RustTarget: *hostRustTarget, RustFeatures: *hostRustFeatures}
	if cross.set() && (cross.Target == "" || cross.Sysroot == "" || cross.ISA == "" || cross.Libcxx == "" || cross.RustTarget == "") {
		fmt.Fprintln(os.Stderr, "bp2ninja: -host-target, -host-sysroot, -host-isa, -host-libcxx and -host-rust-target go together")
		os.Exit(2)
	}
	if cross.set() != (*buildArch != "") || (*buildArch != "") != (*buildBits != 0) {
		fmt.Fprintln(os.Stderr, "bp2ninja: -build-arch and -build-pointer-bits go together, given exactly when the -host-* flags are")
		os.Exit(2)
	}
	if *upstreamToolchain {
		tc, err := upstreamToolchainOf(*cxx, cross)
		if err != nil {
			fmt.Fprintln(os.Stderr, "bp2ninja:", err)
			os.Exit(1)
		}
		b, err := json.Marshal(tc)
		if err != nil {
			fmt.Fprintln(os.Stderr, "bp2ninja:", err)
			os.Exit(1)
		}
		fmt.Println(string(b))
		return
	}
	if *tree == "" || *out == "" || *arch == "" || (*bits != 32 && *bits != 64) || (flag.NArg() == 0) == (*cxxProbe == "" && *nbExports == "") || (*nbExports != "" && *nbArch == "") {
		flag.Usage()
		os.Exit(2)
	}
	set := func(list string) map[string]bool {
		out := map[string]bool{}
		for _, m := range strings.Split(list, ",") {
			if m != "" {
				out[m] = true
			}
		}
		return out
	}
	omitted, android := set(*omit), set(*rustAndroid)
	stand := map[string]string{}
	for _, pair := range strings.Split(*standIn, ",") {
		if pair == "" {
			continue
		}
		m, alt, ok := strings.Cut(pair, "=")
		if !ok || m == "" || alt == "" {
			fmt.Fprintf(os.Stderr, "bp2ninja: -stand-in: %q is not module=tree-module\n", pair)
			os.Exit(2)
		}
		stand[m] = alt
	}
	provided, err := readJSON[Provided](*providedFile)
	if err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja: -provided:", err)
		os.Exit(1)
	}
	nbProvided, err := readJSON[Provided](*nbProvidedFile)
	if err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja: -nb-provided:", err)
		os.Exit(1)
	}
	if (*nbArch == "") != (*nbBits == 0) {
		fmt.Fprintln(os.Stderr, "bp2ninja: -native-bridge and -native-bridge-bits go together")
		os.Exit(1)
	}
	if *nbArch == "" && *nbArchVariant != "" {
		fmt.Fprintln(os.Stderr, "bp2ninja: -native-bridge-arch-variant needs -native-bridge")
		os.Exit(1)
	}
	cfg := Config{Arch: *arch, PointerBits: *bits, ArchVariant: *variant, ABI: *abi, NativeBridge: *nbArch, NativeBridgeBits: *nbBits, NativeBridgeArchVariant: *nbArchVariant, HostCross: cross, BuildArch: *buildArch, BuildBits: *buildBits}
	// the probe compiles, links nothing: its libc provider may be headers only, so checkLibs does not apply
	if *cxxProbe != "" {
		if err := deviceCxxFlags(*tree, *out, cfg, provided, *cxx, *cxxProbe); err != nil {
			fmt.Fprintln(os.Stderr, "bp2ninja:", err)
			os.Exit(1)
		}
		return
	}
	partial, err := readJSON[Partial](*srcsFile)
	if err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja: -srcs:", err)
		os.Exit(1)
	}
	isaTiers, err := readJSON[isaTier](*isaTiersFile)
	if err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja: -isa-tiers:", err)
		os.Exit(1)
	}
	if isaTiers != nil && !cross.set() {
		fmt.Fprintln(os.Stderr, "bp2ninja: -isa-tiers needs the -host-* flags (a cross host)")
		os.Exit(2)
	}
	aidlCrates, err := readJSON[AidlCrate](*aidlCratesFile)
	if err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja: -aidl-crates:", err)
		os.Exit(1)
	}
	moduleDefines, err := parseDefines(*defines)
	if err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja: -defines:", err)
		os.Exit(2)
	}
	if err := checkLibs(provided, partial); err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja: -provided / -srcs:", err)
		os.Exit(1)
	}
	// the build side exists only on a cross host, and links nothing of the host's provided set
	if cross.set() != (*buildProvidedFile != "") {
		fmt.Fprintln(os.Stderr, "bp2ninja: -build-provided is given exactly with the -host-* flags")
		os.Exit(1)
	}
	buildProvided, err := readJSON[Provided](*buildProvidedFile)
	if err == nil {
		err = checkLibs(buildProvided, nil)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja: -build-provided:", err)
		os.Exit(1)
	}
	if err := run(*tree, *out, cfg, Tools{*cc, *cxx, *ar, *bison, *flex, *m4, *yasm, *rustc, *rustVersion, *libclang, *llvm, *launcher, *rtInclude, *bionicStaticRust, *licenceGate}, javaSetup{jt, *release, *measure}, omitted, provided, buildProvided, nbProvided, partial, isaTiers, android, moduleDefines, stand, aidlCrates, *nbExports, regen, flag.Args()); err != nil {
		fmt.Fprintln(os.Stderr, "bp2ninja:", err)
		os.Exit(1)
	}
}

// loadSoong: SoongConfig for cfg, every part read from the pinned tree or computed by the compiled pinned Soong (readGlobalFlags).
func loadSoong(tree string, cfg Config) (SoongConfig, error) {
	soong, err := readSoongConfig(tree)
	if err != nil {
		return soong, err
	}
	if soong.Device, err = readDeviceArch(tree, cfg); err != nil {
		return soong, err
	}
	if soong.Flags, err = readGlobalFlags(cfg, soong.Device); err != nil {
		return soong, err
	}
	if soong.RustFeatures, err = readRustFeatureGate(tree); err != nil {
		return soong, err
	}
	if soong.DeviceLink, err = readDeviceLink(tree); err != nil {
		return soong, err
	}
	soong.Lldflags, err = readLldflags(tree)
	return soong, err
}

// deviceCxxFlags: -device-cxx-flags. Writes <out>/compat, as a build would, and prints {"argv": probeCxxFlags(dir), "android": androidDefine}: the probe's command line and the define xtask check step libcxx toggles in it.
func deviceCxxFlags(tree, out string, cfg Config, provided map[string]Provided, cxx, dir string) error {
	soong, err := loadSoong(tree, cfg)
	if err != nil {
		return err
	}
	if err := writeCompat(tree, out, provided); err != nil {
		return err
	}
	g := &Graph{tree: tree, out: out, cfg: cfg, soong: soong, nodes: map[key]*Node{}}
	if g.libcxx, err = libcxxProvider(cxx, HostCross{}); err != nil {
		return err
	}
	b, err := json.Marshal(struct {
		Argv    []string `json:"argv"`
		Android string   `json:"android"`
	}{g.probeCxxFlags(dir), androidDefine})
	if err != nil {
		return err
	}
	fmt.Println(string(b))
	return nil
}

// javaSetup: the Java tools (read only when given) and the release config.
type javaSetup struct {
	tools   JavaTools
	release string
	measure bool
}

func run(tree, out string, cfg Config, tools Tools, js javaSetup, omit map[string]bool, provided, buildProvided, nbProvided map[string]Provided, partial map[string]Partial, isaTiers map[string]isaTier, rustAndroid map[string]bool, defines map[string][]string, stand map[string]string, aidlCrates map[string]AidlCrate, nbExports string, regen *regenSetup, goals []string) error {
	files, err := discover(tree)
	if err != nil {
		return err
	}
	soong, err := loadSoong(tree, cfg)
	if err != nil {
		return err
	}
	// not passed (toolchainLocation, keepFlag): reported on every run, so a change in what is dropped shows
	fmt.Printf("bp2ninja: Soong global flags not passed (diagnostics, debug info, prebuilt toolchain location): %s\n", strings.Join(soong.Flags.Skipped, " "))
	g := &Graph{tree: tree, out: out, cfg: cfg, soong: soong, omit: omit, provided: provided, buildProvided: buildProvided, nbProvided: nbProvided, partial: partial, rustAndroid: rustAndroid, defines: defines, stand: stand, rtInclude: tools.RtInclude, bionicStaticRust: tools.BionicStaticRust, nodes: map[key]*Node{}, tiers: isaTiers, isaProbes: map[string]isaTierProbe{}}
	if g.libcxx, err = libcxxProvider(tools.CXX, cfg.HostCross); err != nil {
		return err
	}
	g.buildLibcxx = g.libcxx
	if cfg.HostCross.set() {
		if g.buildLibcxx, err = libcxxProvider(tools.CXX, HostCross{}); err != nil {
			return err
		}
	}
	// the release first: select(release_flag(…)) and the soong config variables it sets are evaluated with it
	if js.release != "" {
		if g.release, err = readRelease(tree, js.release); err != nil {
			return err
		}
		id, err := readBuildID(tree)
		if err != nil {
			return err
		}
		prop, err := buildProp(g.release, id, js.tools.Keys)
		if err != nil {
			return err
		}
		g.genFile(filepath.Join(out, buildPropFile), prop)
		g.acfg = newAconfigGraph("")
	}
	g.mods = map[Side]map[string][]*Module{}
	for _, side := range cfg.sides() {
		ecfg := cfg
		ecfg.Arch, ecfg.PointerBits = cfg.archOf(side)
		ev := &evaluator{cfg: ecfg, os: side.evalOS(), rel: g.release}
		mods, problems := ev.modules(files)
		g.mods[side] = mods
		for _, p := range problems {
			g.problem("%s eval: %s", side, p)
		}
	}
	g.baseTypes = map[string]string{}
	g.soongConfigTypes = map[string]*soongConfigType{}
	for _, list := range g.mods[Device] {
		for _, m := range list {
			if m.Type == "soong_config_module_type" {
				g.baseTypes[m.Name] = m.Props.str("module_type")
				g.soongConfigTypes[m.Name] = readSoongConfigType(m)
			}
		}
	}
	art, err := readArtPlugin(tree, cfg, tools.LLVM)
	if err != nil {
		return err
	}
	g.loadArt(art)
	if err := g.addAidlCrates(aidlCrates); err != nil {
		return err
	}
	// providers.toml's own check cannot see the tree; this one can
	names := sortedKeys(omit)
	for m := range provided {
		names = append(names, m)
	}
	for m := range partial {
		names = append(names, m)
	}
	names = append(names, sortedKeys(rustAndroid)...)
	names = append(names, sortedKeys(keysOf(defines))...)
	// a `module:` row's key may be absent (libartpalette-system: system/libartpalette is not fetched); its stand-in must exist, and node checks it installs under the key
	for _, alt := range stand {
		names = append(names, alt)
	}
	for _, m := range names {
		if len(g.mods[Device][m])+len(g.mods[Host][m]) == 0 {
			g.problem("providers.toml: %s names no module in the tree", m)
		}
	}
	g.checkRustAndroid()
	g.checkDefines()
	if nbExports != "" {
		n := g.node(nbExports, NativeBridge(cfg.NativeBridge), Headers, "goal")
		if n == nil {
			return fmt.Errorf("-nb-exports %s: no native bridge variant: %s", nbExports, strings.Join(g.problems, "; "))
		}
		b, err := json.Marshal(n.exports)
		if err != nil {
			return err
		}
		fmt.Println(string(b))
		return nil
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
		// nb:<module>: the module's native bridge variant (a C/C++ library or binary)
		if name, ok := strings.CutPrefix(goal, nbGoalPrefix); ok {
			if cfg.NativeBridge == "" {
				return fmt.Errorf("goal %s: no -native-bridge side", goal)
			}
			if n := g.node(name, NativeBridge(cfg.NativeBridge), Shared, "goal"); n != nil {
				ccGoals = append(ccGoals, n)
			}
			continue
		}
		if goal == bootclasspathGoal && g.java != nil {
			if err := g.bootclasspath(); err != nil {
				return err
			}
			continue
		}
		if goal == bootJniGoal {
			libs, err := g.bootJniLibs()
			if err != nil {
				return err
			}
			for _, lib := range libs {
				if n := g.node(lib, Device, Shared, "goal"); n != nil {
					ccGoals = append(ccGoals, n)
				}
			}
			continue
		}
		if g.java != nil {
			if m := g.lookupJava(goal, Device); m != nil && g.isJava(m.Type) {
				// a Java goal builds its device variant; a host-only type (metalava's java_binary_host) its host one
				side := Device
				if javaHostOnly(g.baseType(m.Type)) {
					side = Host
				}
				if n := g.jnode(goal, side, "goal"); n != nil {
					g.java.outs = append(g.java.outs, n.output()...)
				}
				continue
			}
		}
		if n := g.node(goal, g.ccGoalSide(goal), Shared, "goal"); n != nil {
			ccGoals = append(ccGoals, n)
		}
	}
	if err := g.checkPartial(); err != nil {
		return err
	}
	if err := g.checkISATiers(); err != nil {
		return err
	}
	// build.ninja is written part by part as each is made (it is ~0.8 GB: one concatenated string plus its copies set the peak RSS), into a temporary file renamed at the end, so a failed run leaves the previous one
	if err := os.MkdirAll(out, 0o755); err != nil {
		return err
	}
	ninja, err := newNinjaFile(filepath.Join(out, "build.ninja"))
	if err != nil {
		return err
	}
	defer ninja.discard()
	ninja.WriteString(g.emit(tools))
	if err := g.writeISATierProbes(out); err != nil {
		return err
	}
	if g.java != nil {
		ninja.WriteString(g.java.ninja(g))
	}
	ninja.WriteString(g.licenceNinja(tools.LicenceGate))
	// compat/ has one reader, the device-side compile (ninja.go compileFlags): a plan without one writes nothing and needs no libc provider for api-level.h
	if g.hasDeviceObjs() {
		if err := writeCompat(tree, out, provided); err != nil {
			return err
		}
	}
	if err := g.writeGenFiles(); err != nil {
		return err
	}
	if regen != nil {
		if s := g.bootLoadsNinja(regen.exe); s != "" {
			ninja.WriteString(s)
			regen.implicit = append(regen.implicit, filepath.Join(buildDir, bootLoadsFile))
		}
		m, err := regen.manifest(out)
		if err != nil {
			return err
		}
		ninja.WriteString(m)
	}
	// before the commit: a graph that crosses machines keeps the previous build.ninja
	if err := g.checkMachines(); err != nil {
		return err
	}
	if err := ninja.commit(); err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(out, "problems.txt"), []byte(strings.Join(g.problems, "\n")+"\n"), 0o644); err != nil {
		return err
	}
	graph, err := g.graphJSON(ccGoals, tools.CXX, tools.CC)
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
	// last: graphJSON reads too, so every input is recorded by now
	if regen != nil {
		if err := regen.writeInputs(out); err != nil {
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
