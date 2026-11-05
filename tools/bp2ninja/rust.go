// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
)

// Rust modules, following build/soong/rust (the spec): rustc run directly per crate, no cargo.
//   - Rlib: rust_library[_rlib], rust_ffi_static (Soong registers it as RustLibraryRlibFactory), rust_ffi's rlib variant, their _host forms.
//     Only rlibs: std links as an rlib everywhere (Soong's host default and rust_ffi_static's; device dylib std is not built).
//   - ProcMacro: rust_proc_macro, always a host crate. Binary: rust_binary[_host].
//   - A cc module naming an rlib in static_libs / whole_static_libs links it through one generated staticlib per final link (cc/builder.go GenerateRustStaticlib):
//     `#![no_std]` + `extern crate` of every rlib cc modules of the link name, so the link holds exactly one std however many Rust libraries it pulls in.
//     The rlibs' own cc dependencies (libcxx's whole_static_libs libcxxbridge05) link into that final link like any static closure member; Soong bundles them into the rlib instead, same objects.
//   - Toolchain: the repo's rust-toolchain.toml rustc (stable), with the host's own sysroot, so std, core, alloc and proc_macro come from it; Soong's `--sysroot=/dev/null` + prebuilt std modules are not mirrored.
//     Soong's -Z flags (stack-protector, remap-cwd-prefix, dylib-lto, link-native-libraries, tls-model, debug-info-for-profiling) need nightly; they harden or tune code, never change what compiles, so they are dropped.
//   - Device crates build for the host triple (rustc's default target), as cc device modules do: target_os is linux, not android. cc device compiles define __ANDROID__ (androidDefine); rustc's target_os is no define, so it stays linux.
//     So a device crate takes the linux_glibc `target: {…}` blocks (rustOSSide), keeping device codegen flags (rustDeviceFlags) -- unlike cc, which keeps android blocks.
//     cc device code is written against the Android userspace API, which this build provides, so android blocks stay right for it.
//     A Rust crate's blocks are a function of rustc's target_os: cargo_embargo generates `target: { android }` from `cfg(target_os = "android")`,
//     so an android block names code the crate itself compiles out on linux (liblog_rust's default_log_impl -> android-only log_ffi; liblogger's android_logger; rustix).
//   - Exception, providers.toml `rust_os = "android"` (g.rustAndroid): a device crate whose Android behaviour this runtime provides (system properties, liblog) takes the android blocks
//     and gets `--cfg atl_android_userspace` (rustAndroidCfg), which a patch to its source adds to exactly the `target_os = "android"` gates it needs; its host variant is untouched.
//   - Lints: `--cap-lints allow` for every crate, as cc's -w: a newer rustc warns differently, errors are what this build measures (Soong caps only external/ and prebuilts/).
//   - Source providers (rust_protobuf, rust_bindgen, aidl_interface rust backends): rlibs whose crate root a build step generates, rustsrc.go; named in another crate's srcs, their source variant (Kind Gen) generates only.
//   - Not supported, reported as problems: dylib / cdylib / staticlib crate variants (rust_ffi in shared_libs), provider types without a rustSourceProviders row (rust_aconfig_library).

// rustOSSide: the side whose OS blocks a Rust module takes on side (flatten): rustc compiles for linux_glibc, so Host, but Device for a device variant of a rust_os = "android" row.
func (g *Graph) rustOSSide(m *Module, side Side) Side {
	if side == Device && g.rustAndroid[m.Name] {
		return Device
	}
	return Host
}

// rustAndroidCfg: the cfg a rust_os = "android" crate gets; never a builtin, so rustc sets it nowhere else, and a patched gate reads `any(target_os = "android", atl_android_userspace)`.
const rustAndroidCfg = "atl_android_userspace"

// checkRustAndroid: every rust_os = "android" row names a Rust module with a device variant; the field means nothing elsewhere, so a row on a cc module, a host-only crate or a renamed module would be silently dead.
// A row naming no module at all is reported with the other providers.toml rows.
func (g *Graph) checkRustAndroid() {
	for _, name := range sortedKeys(g.rustAndroid) {
		mods := g.mods[Device][name]
		if len(mods) == 0 {
			continue
		}
		for _, m := range mods {
			t := g.baseType(m.Type)
			switch {
			case !isRustType(t):
				g.problem("providers.toml: %s: rust_os on a %s, not a Rust module", name, m.Type)
			case g.noDeviceVariant(m):
				g.problem("providers.toml: %s: rust_os on a %s, which has no device variant", name, m.Type)
			}
		}
	}
}

// rustDefaultEdition:rust/config/global.go DefaultEdition, the edition of a crate without `edition`.
const rustDefaultEdition = "2021"

// rustStdlibs: rust/config/global.go Stdlibs (+ the sysroot crates a module may name in rustlibs): built into the sysroot, so a reference to one is dropped.
var rustStdlibs = map[string]bool{"libstd": true, "libcore": true, "liballoc": true, "libproc_macro": true, "libtest": true}

// rustGlobalFlags: rust/config/global.go GlobalRustFlags that a stable rustc takes (codegen choices of every crate), without debuginfo (cc builds without -g either).
var rustGlobalFlags = []string{"-C", "opt-level=3", "-C", "relocation-model=pic", "-C", "overflow-checks=on", "-C", "force-unwind-tables=yes", "-C", "symbol-mangling-version=v0", "--cfg", "soong"}

// rustDeviceFlags: rust/config/global.go deviceGlobalRustFlags that a stable rustc takes.
var rustDeviceFlags = []string{"-C", "panic=abort"}

// rustCrate is what a Rust node compiles.
type rustCrate struct {
	crateType, crateName, root string
	// generated files srcs names (`:genrule`, `:provider`), copied into outDir, which env!("OUT_DIR") names (rust/builder.go: the cp rule + rustEnvVars)
	srcDeps []string
	outDir  string
	// source variants of the providers srcs names (rust.go sourceDepTag)
	providers []*Node
	// a source provider's generated sources (rustsrc.go), root first
	source rustSource
	flags  []string
	// direct crate dependencies by the name the source uses (aliases applied)
	externs []rustExtern
	broken  string // first reason it cannot be compiled; its step fails with it
}

type rustExtern struct {
	name string
	dep  *Node
}

// isRustType: a module type built by rustNode.
func isRustType(t string) bool { return strings.HasPrefix(t, "rust_") && t != "rust_defaults" }

// rustKind: kindOf for Rust module types; ok=false for variants that are not built (dylib, cdylib, source providers without a row).
func rustKind(t string, want Kind) (Kind, bool) {
	if rustSourceProviders[t] != nil {
		// source_provider.go NewSourceProviderModule: a library, only its rlib built here; Gen = its source variant (generation only), what another crate's srcs names
		if want == Gen {
			return Gen, true
		}
		return Rlib, true
	}
	switch t {
	case "rust_library", "rust_library_rlib", "rust_library_host", "rust_library_host_rlib", "rust_ffi_static", "rust_ffi_host_static":
		return Rlib, true
	case "rust_ffi", "rust_ffi_host":
		// a shared variant (cdylib) is not built
		return Rlib, want != Shared
	case "rust_proc_macro":
		return ProcMacro, true
	case "rust_binary", "rust_binary_host":
		return Binary, true
	}
	return 0, false
}

// rustNode fills a Rust node: its crate, crate dependencies (rustlibs, proc_macros) and cc dependencies (static_libs, whole_static_libs, shared_libs: linked by the final link, rust.go depsToPaths).
func (g *Graph) rustNode(n *Node) {
	p, m := n.props, n.mod
	r := &rustCrate{outDir: filepath.Join(g.out, "obj", n.side.String(), n.name, "out")}
	n.rust = r
	fail := func(format string, args ...any) {
		msg := fmt.Sprintf(format, args...)
		g.problem("%s: %s", m.Name, msg)
		if r.broken == "" {
			r.broken = msg
		}
	}
	sp := rustSourceProviders[g.baseType(m.Type)]
	ccDeps := func(refs []string, want Kind) []*Node {
		var out []*Node
		for _, ref := range refs {
			if d := g.node(ref, n.side, want, m.Name); d != nil {
				out = append(out, d)
			}
		}
		return out
	}
	n.deps.whole = ccDeps(p.strs("whole_static_libs"), Static)
	n.deps.static = ccDeps(p.strs("static_libs"), Static)
	n.deps.shared = ccDeps(p.strs("shared_libs"), Shared)
	if n.kind == Gen {
		r.source = sp.gen(g, n, fail)
		return
	}
	r.crateType = map[Kind]string{Rlib: "rlib", ProcMacro: "proc-macro", Binary: "bin"}[n.kind]
	r.crateName = p.str("crate_name")
	if r.crateName == "" {
		if n.kind != Binary {
			fail("crate_name is required")
		}
		// binary.go: a binary's crate is named after its stem
		r.crateName = strings.ReplaceAll(g.rustStem(n), "-", "_")
	}
	// crate root: a source provider's first output (library.go checkedCrateRootPath), else crate_root, else the one plain source of srcs, else the first output of its first generated source
	// (compiler.go srcPathFromModuleSrcs)
	if cr := p.str("crate_root"); cr != "" && sp == nil {
		r.root = filepath.Join(m.Dir, cr)
	}
	var plain []string
	var srcs []string
	if sp == nil {
		srcs = p.strs("srcs")
	}
	for _, s := range srcs {
		if !isRef(s) {
			plain = append(plain, s)
			continue
		}
		var paths []string
		dm := g.lookup(s, n.side)
		switch {
		case dm != nil && rustSourceProviders[g.baseType(dm.Type)] != nil:
			// rust.go depsToPaths: every output of the provider's source variant
			d := g.node(s, n.side, Gen, m.Name)
			if d == nil || d.rust == nil {
				fail("source %s unresolved", s)
				continue
			}
			if d.rust.broken != "" {
				fail("source %s: %s", s, d.rust.broken)
				continue
			}
			r.providers = append(r.providers, d)
			paths = d.rust.source.outs
		case dm != nil && !isGenType(g.baseType(dm.Type)) && dm.Type != "filegroup":
			fail("source %s is a %s (unsupported)", s, dm.Type)
			continue
		default:
			files, ok := g.resolveSrc(m.Dir, n.side, s, m.Name)
			if !ok {
				fail("source %s unresolved", s)
			}
			for _, f := range files {
				if f.gen != nil {
					g.useGen(n, f.gen)
				}
				paths = append(paths, f.path)
			}
		}
		for _, path := range paths {
			// builder.go: the copies share OUT_DIR, one file name each
			if slices.ContainsFunc(r.srcDeps, func(s string) bool { return filepath.Base(s) == filepath.Base(path) }) {
				fail("multiple source providers generate the same filename output: %s", filepath.Base(path))
				continue
			}
			r.srcDeps = append(r.srcDeps, path)
		}
	}
	if r.root == "" && sp == nil {
		switch {
		case len(plain) > 1:
			fail("srcs can only contain one path for a rust file and source providers prefixed by \":\"")
		case len(plain) == 1:
			r.root = filepath.Join(m.Dir, plain[0])
		case len(r.srcDeps) > 0:
			r.root = r.srcDeps[0]
		default:
			fail("srcs must not be empty")
		}
	}
	aliases := map[string]string{}
	for _, a := range p.strs("aliases") {
		if dep, alias, ok := strings.Cut(a, ":"); ok {
			aliases[dep] = alias
		} else {
			fail("invalid aliases entry %q missing ':'", a)
		}
	}
	crate := func(ref string, side Side, want Kind) {
		if rustStdlibs[strings.TrimPrefix(ref, ":")] {
			return
		}
		// a cc module named as a crate would otherwise be built as a shared library for nothing
		if dm := g.lookup(ref, side); dm != nil && !isRustType(g.baseType(dm.Type)) {
			fail("crate %s is a %s, not a Rust module", ref, dm.Type)
			return
		}
		d := g.node(ref, side, want, m.Name)
		switch {
		case d == nil:
			fail("crate %s unavailable", ref)
		case d.rust == nil || d.kind != want:
			fail("crate %s is a %s, not a %s", ref, d.mod.Type, want)
		default:
			name := d.rust.crateName
			if a, ok := aliases[name]; ok {
				name = a
			}
			n.deps.crates = append(n.deps.crates, d)
			r.externs = append(r.externs, rustExtern{name, d})
		}
	}
	var spCrates, spMacros []string
	if sp != nil {
		spCrates, spMacros = sp.deps(n)
	}
	for _, ref := range slices.Concat(p.strs("rustlibs"), p.strs("rlibs"), spCrates) {
		crate(ref, n.side, Rlib)
	}
	// proc macros run in the compiler: host crates whatever the side
	for _, ref := range slices.Concat(p.strs("proc_macros"), spMacros) {
		crate(ref, Host, ProcMacro)
	}
	n.deps.crates = uniqueNodes(n.deps.crates)
	if sp != nil {
		r.source = sp.gen(g, n, fail)
		if len(r.source.outs) > 0 {
			r.root = r.source.outs[0]
		}
	}
	// library.go: include_dirs (deprecated) and export_include_dirs both reach the cc modules linking the library
	for _, d := range slices.Concat(p.strs("include_dirs"), p.strs("export_include_dirs")) {
		n.exports = append(n.exports, "-I"+g.abs(filepath.Join(m.Dir, d)))
	}

	r.flags = append(r.flags, rustGlobalFlags...)
	if n.side == Device {
		r.flags = append(r.flags, rustDeviceFlags...)
	}
	if m.lints != nil {
		// a workspace crate (aidlcrates.go): its lints, not capped; emitRust compiles it with clippy-driver
		r.flags = append(r.flags, m.lints...)
	} else {
		r.flags = append(r.flags, "--cap-lints", "allow")
	}
	r.flags = append(r.flags, "--crate-type="+r.crateType, "--crate-name="+r.crateName)
	edition := p.str("edition")
	if edition == "" {
		edition = rustDefaultEdition
	}
	r.flags = append(r.flags, "--edition="+edition)
	// compiler.go cfgFlags / featureFlags
	for _, c := range p.strs("cfgs") {
		r.flags = append(r.flags, "--cfg", c)
	}
	if g.rustOSSide(m, n.side) == Device {
		r.flags = append(r.flags, "--cfg", rustAndroidCfg)
	}
	for _, f := range p.strs("features") {
		r.flags = append(r.flags, "--cfg", `feature="`+f+`"`)
	}
	switch n.kind {
	case Rlib:
		// library.go: one crate name may come from several modules (syn 1 and 2): the module name keeps their symbols apart
		r.flags = append(r.flags, "-C", "metadata="+m.Name)
	case ProcMacro:
		r.flags = append(r.flags, "--extern", "proc_macro", "-C", "metadata="+m.Name)
	}
	for _, f := range p.strs("flags") {
		if strings.HasPrefix(f, "-Z") {
			fail("flag %s needs a nightly rustc (unsupported)", f)
			continue
		}
		r.flags = append(r.flags, strings.Fields(f)...)
	}
}

// RustFeatureGate: which module dirs may use unstable features, read from the pinned Soong (readRustFeatureGate) so an AOSP upgrade brings its own list.
// rust/builder.go passes `-Zallow-features=""` unless android.IsThirdPartyPath(ModuleDir) or the same condition's HasPrefix(ModuleDir, "prebuilts"): first-party crates are stable-only, third-party ones may use what their rustc offers.
// The stable toolchain's counterpart is a per-crate RUSTC_BOOTSTRAP (bootstrap), set on the rustc edge only, never in the build's environment.
type RustFeatureGate struct {
	prefixes   []string         // android/paths.go IsThirdPartyPath's thirdPartyDirPrefixes
	exceptions []*regexp.Regexp // android/paths.go thirdPartyDirPrefixExceptions: first-party dirs under those prefixes (hardware/interfaces/)
	extra      []string         // the builder.go condition's own HasPrefix prefixes
}

// allows: the gate's condition for a module dir (relative to the tree, Soong's ModuleDir).
func (r RustFeatureGate) allows(dir string) bool {
	if slices.ContainsFunc(r.extra, func(p string) bool { return strings.HasPrefix(dir, p) }) {
		return true
	}
	if !slices.ContainsFunc(r.prefixes, func(p string) bool { return strings.HasPrefix(dir, p) }) {
		return false
	}
	return !slices.ContainsFunc(r.exceptions, func(e *regexp.Regexp) bool { return e.MatchString(dir) })
}

// bootstrap: the crate's RUSTC_BOOTSTRAP: 1 lets a stable rustc take #![feature] (proc-macro2's proc_macro_span cfg); -1 makes it stable whatever the caller's environment holds, as -Zallow-features="" does.
func (r RustFeatureGate) bootstrap(dir string) string {
	if r.allows(dir) {
		return "1"
	}
	return "-1"
}

// readRustFeatureGate: RustFeatureGate from build/soong/rust/builder.go's `-Zallow-features` condition and android/paths.go IsThirdPartyPath; any other shape fails, so a Soong change is read, never guessed.
func readRustFeatureGate(tree string) (RustFeatureGate, error) {
	var r RustFeatureGate
	parse := func(rel string) (*ast.File, error) {
		return parser.ParseFile(token.NewFileSet(), filepath.Join(tree, rel), nil, 0)
	}
	const builderGo, pathsGo = "build/soong/rust/builder.go", "build/soong/android/paths.go"
	builder, err := parse(builderGo)
	if err != nil {
		return r, err
	}
	// the `if !(…) { … "-Zallow-features=\"\"" … }` statement: its condition's calls are IsThirdPartyPath and HasPrefix only
	var cond ast.Expr
	ast.Inspect(builder, func(x ast.Node) bool {
		if s, ok := x.(*ast.IfStmt); ok && cond == nil {
			ast.Inspect(s.Body, func(y ast.Node) bool {
				if l, ok := y.(*ast.BasicLit); ok && strings.Contains(goString(l), "-Zallow-features") {
					cond = s.Cond
				}
				return cond == nil
			})
		}
		return cond == nil
	})
	u, ok := cond.(*ast.UnaryExpr)
	if !ok || u.Op != token.NOT {
		return r, fmt.Errorf("%s: no `if !(…)` around -Zallow-features", builderGo)
	}
	thirdParty := false
	ast.Inspect(u.X, func(x ast.Node) bool {
		c, ok := x.(*ast.CallExpr)
		if !ok {
			return err == nil
		}
		sel, _ := c.Fun.(*ast.SelectorExpr)
		switch {
		case sel != nil && sel.Sel.Name == "IsThirdPartyPath":
			thirdParty = true
		case sel != nil && sel.Sel.Name == "HasPrefix" && len(c.Args) == 2 && goString(c.Args[1]) != "":
			r.extra = append(r.extra, goString(c.Args[1]))
		default:
			err = fmt.Errorf("%s: unknown call in the -Zallow-features condition", builderGo)
		}
		return false
	})
	if err != nil {
		return r, err
	}
	if !thirdParty {
		return r, fmt.Errorf("%s: the -Zallow-features condition no longer calls IsThirdPartyPath", builderGo)
	}
	paths, err := parse(pathsGo)
	if err != nil {
		return r, err
	}
	for _, decl := range paths.Decls {
		if f, ok := decl.(*ast.FuncDecl); ok && f.Name.Name == "IsThirdPartyPath" {
			ast.Inspect(f.Body, func(x ast.Node) bool {
				if a, ok := x.(*ast.AssignStmt); ok && len(a.Lhs) == 1 && len(a.Rhs) == 1 {
					if id, ok := a.Lhs[0].(*ast.Ident); ok && id.Name == "thirdPartyDirPrefixes" {
						r.prefixes, _ = stringList(a.Rhs[0])
					}
				}
				return true
			})
		}
	}
	if len(r.prefixes) == 0 {
		return r, fmt.Errorf("%s: IsThirdPartyPath's thirdPartyDirPrefixes not found", pathsGo)
	}
	list, ok := valueSpecs(paths)["thirdPartyDirPrefixExceptions"].(*ast.CompositeLit)
	if !ok {
		return r, fmt.Errorf("%s: thirdPartyDirPrefixExceptions not found", pathsGo)
	}
	for _, e := range list.Elts {
		c, ok := e.(*ast.CallExpr)
		var sel *ast.SelectorExpr
		if ok {
			sel, _ = c.Fun.(*ast.SelectorExpr)
		}
		if sel == nil || sel.Sel.Name != "MustCompile" || len(c.Args) != 1 || goString(c.Args[0]) == "" {
			return r, fmt.Errorf("%s: thirdPartyDirPrefixExceptions entry is not regexp.MustCompile(\"…\")", pathsGo)
		}
		re, err := regexp.Compile(goString(c.Args[0]))
		if err != nil {
			return r, fmt.Errorf("%s: %v", pathsGo, err)
		}
		r.exceptions = append(r.exceptions, re)
	}
	return r, nil
}

// rustStem: a binary's output name (binary.go getStem).
func (g *Graph) rustStem(n *Node) string {
	if s := n.props.str("stem"); s != "" {
		return s
	}
	return n.mod.Name
}

// rustEnv: builder.go rustEnvVars.
func (g *Graph) rustEnv(n *Node, rustVersion string) []string {
	p, r := n.props, n.rust
	env := []string{"OUT_DIR=out"}
	if len(r.srcDeps) > 0 {
		env[0] = "OUT_DIR=" + r.outDir
	}
	env = append(env, "ANDROID_RUST_VERSION="+rustVersion, "RUSTC_BOOTSTRAP="+g.soong.RustFeatures.bootstrap(n.mod.Dir))
	if p.boolean("cargo_env_compat", false) {
		if n.kind == Binary {
			env = append(env, "CARGO_BIN_NAME="+g.rustStem(n))
		}
		env = append(env, "CARGO_CRATE_NAME="+r.crateName, "CARGO_PKG_NAME="+r.crateName)
		if v := p.str("cargo_pkg_version"); v != "" {
			env = append(env, "CARGO_PKG_VERSION="+v)
			if parts := strings.Split(v, "."); len(parts) == 3 {
				env = append(env, "CARGO_PKG_VERSION_MAJOR="+parts[0], "CARGO_PKG_VERSION_MINOR="+parts[1], "CARGO_PKG_VERSION_PATCH="+parts[2])
			}
		}
	}
	return env
}

// rustOut: the file a Rust node produces (library.go: lib<crate>.rlib; proc_macro.go: lib<crate>.so; a binary its stem); a source variant none, its generated sources are on its edges.
func (g *Graph) rustOut(n *Node) string {
	base := filepath.Join(g.out, n.side.String(), n.name)
	switch n.kind {
	case Gen:
		return ""
	case Rlib:
		return filepath.Join(base, "lib"+n.rust.crateName+".rlib")
	case ProcMacro:
		return filepath.Join(base, "lib"+n.rust.crateName+".so")
	}
	return filepath.Join(base, g.rustStem(n))
}

// crateClosure: the crates n's crates need, transitively, each once; proc macros' dependencies are their own (compiled into the macro).
func crateClosure(roots []*Node) []*Node {
	seen := map[*Node]bool{}
	var out []*Node
	var walk func(*Node)
	walk = func(d *Node) {
		if seen[d] {
			return
		}
		seen[d] = true
		out = append(out, d)
		if d.kind != ProcMacro {
			for _, c := range d.deps.crates {
				walk(c)
			}
		}
	}
	for _, r := range roots {
		walk(r)
	}
	return out
}

// rustLibFlags: --extern for the direct crates, -L dependency= for every crate the closure holds (rustc finds the transitive ones there; builder.go makeLibFlags).
// linked: the closure's rlibs, whose code (inlined generics included) the output carries. The rustc rule appends them to rustc's dep-info depfile, as a cc depfile lists headers,
// so `ninja -t deps` holds what each Rust output is made of (xtask's licence gate follows it). Proc macros run at compile time and ship nothing: implicit inputs only.
func rustLibFlags(externs []rustExtern) (flags, inputs, linked []string) {
	var roots []*Node
	for _, e := range externs {
		flags = append(flags, "--extern", e.name+"="+e.dep.out)
		roots = append(roots, e.dep)
	}
	for _, d := range crateClosure(roots) {
		inputs = append(inputs, d.out)
		if d.kind != ProcMacro {
			linked = append(linked, d.out)
		}
	}
	for _, dir := range crateDirs(roots) {
		flags = append(flags, "-L", "dependency="+dir)
	}
	return flags, inputs, linked
}

// crateDirs: the directories of the crates roots need (crateClosure), each once, in closure order: rustc's -L dependency= search path for them.
func crateDirs(roots []*Node) []string {
	var dirs []string
	for _, d := range crateClosure(roots) {
		dirs = appendNew(dirs, filepath.Dir(d.out))
	}
	return dirs
}

// emitRust writes the steps of one Rust node: OUT_DIR copies of its generated sources, then rustc; a broken crate's step fails with the reason.
func (g *Graph) emitRust(b *strings.Builder, n *Node, tools Tools) {
	r := n.rust
	if n.kind == Gen {
		return
	}
	if r.broken != "" {
		fmt.Fprintf(b, "build %s: fail\n  msg = %s\n", esc(n.out), shellQuote([]string{fmt.Sprintf("error: rust %s: %s", n.name, r.broken)}))
		return
	}
	var implicit []string
	for _, s := range r.srcDeps {
		cp := filepath.Join(r.outDir, filepath.Base(s))
		fmt.Fprintf(b, "build %s: cp %s\n", esc(cp), esc(g.abs(s)))
		implicit = append(implicit, cp)
	}
	// a provider's root (rustc's input) names its other outputs as modules (builder.go: srcProviderFiles are implicits)
	if len(r.source.outs) > 1 {
		implicit = append(implicit, r.source.outs[1:]...)
	}
	libs, inputs, linked := rustLibFlags(r.externs)
	implicit = append(implicit, inputs...)
	flags := append([]string{}, r.flags...)
	if n.kind == Binary {
		link, linkInputs := g.rustLinkArgs(n, tools.CXX)
		flags = append(flags, link...)
		implicit = append(implicit, linkInputs...)
	}
	var waits []string
	genClosure(n, map[*Node]bool{}, &waits)
	fmt.Fprintf(b, "build %s: rustc %s | %s || %s\n  env = %s\n  flags = %s\n  libs = %s\n  crates = %s\n", esc(n.out), esc(g.abs(r.root)), join(implicit), join(waits),
		shellQuote(g.rustEnv(n, tools.RustVersion)), shellQuote(flags), shellQuote(libs), shellQuote(linked))
	if n.mod.lints != nil {
		// clippy-driver is rustc plus clippy's lints (cargo clippy runs it the same way), from the same toolchain dir
		fmt.Fprintf(b, "  rustc = %s\n", esc(filepath.Join(filepath.Dir(tools.Rustc), "clippy-driver")))
	}
}

// rustLinkArgs: a Rust binary's link: the cc link driver (Soong's rustc_linker runs clang++), the cc libraries its crates need (rustCcLibs) as link args, and their dirs as -L (rustLinkDirs).
func (g *Graph) rustLinkArgs(n *Node, linker string) (flags, inputs []string) {
	flags = []string{"-C", "linker=" + linker}
	libs, inputs := g.rustCcLibs(n)
	for _, l := range libs {
		flags = append(flags, "-C", "link-arg="+l)
	}
	for _, d := range rustLinkDirs(libs) {
		flags = append(flags, "-L", d)
	}
	return flags, inputs
}

// rustLinkDirs: the dirs of the library files among rustCcLibs' arguments, each once: rustc's -L for the crates' `#[link(name = ...)]` (android_log_sys's `log` -> liblog.so), as Soong passes each cc dependency's dir (rust/builder.go linkDirs).
func rustLinkDirs(libs []string) []string {
	var dirs []string
	for _, l := range libs {
		if filepath.IsAbs(l) {
			dirs = appendNew(dirs, filepath.Dir(l))
		}
	}
	return dirs
}

// rustCcLibs: the cc libraries (whole archives, the static closure, shared libraries) a final link of Rust node n needs, as linker arguments of the cc link driver; none when neither n nor a crate it links names a cc library.
// A crate deep in the closure counts (libbinder_rs -> libbinder_ndk_sys's shared libbinder_ndk), as linkLibs takes every closure member's shared libraries.
// Also what graph.json gives a cargo package linking rlib n (graph.go artifact.Link).
func (g *Graph) rustCcLibs(n *Node) (libs, inputs []string) {
	var closure []*Node
	staticClosure(n, map[*Node]bool{}, &closure)
	if !slices.ContainsFunc(append([]*Node{n}, closure...), func(d *Node) bool { return d.rust == nil || len(d.deps.shared) > 0 }) {
		return nil, nil
	}
	return g.linkLibs(n)
}

// rustStaticlib: the generated staticlib of a final cc link (cc/builder.go GenerateRustStaticlib): its path, the edges building it, and whether it links whole
// (an rlib in a shared library's whole_static_libs: its exported functions stay exported, cc.go WholeRustStaticlib). "" when the link has no Rust.
// The roots are the rlibs cc modules of the static closure name; their crates come along through -L.
func (g *Graph) rustStaticlib(b *strings.Builder, n *Node) (lib string, whole bool) {
	var closure []*Node
	staticClosure(n, map[*Node]bool{}, &closure)
	var roots []rustExtern
	seen := map[*Node]bool{}
	for _, c := range append([]*Node{n}, closure...) {
		if c.rust != nil {
			continue
		}
		for _, d := range slices.Concat(c.deps.whole, c.deps.static) {
			if d.rust != nil && !seen[d] {
				seen[d] = true
				roots = append(roots, rustExtern{d.rust.crateName, d})
				whole = whole || n.kind == Shared && c == n && slices.Contains(n.deps.whole, d)
			}
		}
	}
	if len(roots) == 0 {
		return "", false
	}
	dir := filepath.Join(g.out, n.side.String(), n.name, "generated_rust_staticlib")
	lib = filepath.Join(dir, "librustlibs.a")
	src := filepath.Join(dir, "librustlibs.rs")
	lines := []string{"// generated by bp2ninja from the Rust libraries " + n.name + " links, do not edit", "#![no_std]"}
	for _, r := range roots {
		lines = append(lines, "extern crate "+r.name+";")
	}
	g.genFile(src, strings.Join(lines, "\n")+"\n")
	libs, inputs, linked := rustLibFlags(roots)
	flags := append([]string{}, rustGlobalFlags...)
	if n.side == Device {
		flags = append(flags, rustDeviceFlags...)
	}
	flags = append(flags, "--cap-lints", "allow", "--crate-type=staticlib", "--crate-name=generated_rust_staticlib", "--edition="+rustDefaultEdition)
	fmt.Fprintf(b, "build %s: rustc %s | %s\n  env = OUT_DIR=out\n  flags = %s\n  libs = %s\n  crates = %s\n", esc(lib), esc(src), join(inputs), shellQuote(flags), shellQuote(libs), shellQuote(linked))
	return lib, whole
}
