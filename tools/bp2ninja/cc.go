// SPDX-License-Identifier: MIT

package main

import (
	_ "embed"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"
)

// Side is which source set a module is built from, and for which C library. device = the module's android properties built as a Linux glibc binary (runtime libraries of an app process); host = its host properties (build tools such as aidl that run on the build machine); native bridge = android properties + target.native_bridge, built against bionic for a guest arch.
// Comparable: per-side state is a map keyed by Side, iterated in Graph.sides order (never map order: emit must be deterministic).
type Side struct {
	kind sideKind
	arch string // native bridge only: the guest arch (arches.toml name)
}

type sideKind int

const (
	deviceSide sideKind = iota
	hostSide
	nativeBridgeSide
)

var (
	Device = Side{kind: deviceSide}
	Host   = Side{kind: hostSide}
)

// NativeBridge: the native bridge side for guest arch.
func NativeBridge(arch string) Side { return Side{kind: nativeBridgeSide, arch: arch} }

func (s Side) String() string {
	switch s.kind {
	case deviceSide:
		return "device"
	case hostSide:
		return "host"
	}
	return "nb/" + s.arch
}

// android: the side evaluates the module's android properties (device and native bridge).
func (s Side) android() bool { return s.kind != hostSide }

// glibc: the side links against the build machine's glibc (device and host); the native bridge side links bionic.
func (s Side) glibc() bool { return s.kind != nativeBridgeSide }

// evalOS: the Soong OS the side's modules are evaluated for (select(os(), …), target blocks).
func (s Side) evalOS() string {
	if s.android() {
		return "android"
	}
	return "linux_glibc"
}

// nbGoalPrefix: a goal naming a module's native bridge variant.
const nbGoalPrefix = "nb:"

// sides: the sides a run evaluates, in emit order: device, host, then the native bridge guest if -native-bridge names one.
func (c Config) sides() []Side {
	s := []Side{Device, Host}
	if c.NativeBridge != "" {
		s = append(s, NativeBridge(c.NativeBridge))
	}
	return s
}

// archOf: the Soong arch and pointer width a side's modules are built for: the host arch, or the native bridge guest's.
func (c Config) archOf(s Side) (string, int) {
	if s.kind == nativeBridgeSide {
		return s.arch, c.NativeBridgeBits
	}
	return c.Arch, c.PointerBits
}

// osBlocks: the OS-only `target: {…}` blocks a side applies, in Soong's order (android/arch.go setOSProperties, the os mutator: before any arch block).
// not_windows is a host class block; android64/android32 follow the device's pointer width.
func osBlocks(s Side, pointerBits int) []string {
	if s.android() {
		return []string{"linux", "bionic", "android", fmt.Sprintf("android%d", pointerBits)}
	}
	return []string{"host", "linux", "host_linux", "glibc", "linux_glibc", "not_windows"}
}

// osArchBlocks: the OS+arch `target: {…}` blocks, in Soong's order (android/arch.go getArchProperties, after `arch` and `multilib`).
// The native bridge side applies target.native_bridge last (getArchProperties: after the OS+arch blocks, os == Android && NativeBridgeEnabled).
func osArchBlocks(s Side, arch string) []string {
	if s.kind == nativeBridgeSide {
		return []string{"linux_" + arch, "bionic_" + arch, "android_" + arch, "native_bridge"}
	}
	if s == Device {
		return []string{"linux_" + arch, "bionic_" + arch, "android_" + arch}
	}
	return []string{"linux_" + arch, "linux_glibc_" + arch, "glibc_" + arch}
}

// Kind is the artifact a node produces.
type Kind int

const (
	Static Kind = iota
	Shared
	Headers
	Binary
	Gen       // genrule / cc_genrule: outputs only, no cc artifact
	Rlib      // Rust library crate (rust.go)
	ProcMacro // Rust proc-macro crate, host only (rust.go)
)

func (k Kind) String() string {
	return [...]string{"static", "shared", "headers", "binary", "gen", "rlib", "proc-macro"}[k]
}

type key struct {
	name string
	side Side
	kind Kind
}

// Node is one module built one way.
type Node struct {
	key
	mod   *Module
	props *Obj     // after defaults, target/arch/multilib and kind blocks
	srcs  []string // compiled sources: plain ones and the outputs of generators
	// crates: a Rust node's rustlibs and proc_macros (rust.go); a cc node's rlibs are in whole / static
	// runtime: runtime_libs, which no step waits for (stepDeps): a library loads them at run time, and they may name it back (libart -> libjavacore -> libart)
	deps struct{ whole, static, shared, headers, crates, runtime []*Node }
	// libraries a source generator added to deps (protobuf runtime): their exports travel on, as export_*_lib_headers would
	reexport []*Node
	// exported include flags, transitive through export_*_lib_headers
	exports []string
	out     string // archive, shared object or binary
	objs    []string
	gen     []string // files generated for it (generator headers, genrule output): its compiles wait for them
	// source generators (gensrc.go): their edges, include flags and cflags for n's compiles
	edges     []genEdge
	genCflags []string
	// genrules: usedGens are those n consumes; genIncs their include flags and the generators'; genExports
	// those of its export_generated_headers and of generators with their export property set, handed to dependents
	usedGens   map[*Node]bool
	genIncs    []string
	genExports []string
	// aidl import dirs of the aidl_library modules aidl.libs names (ccAidlLibs)
	aidlIncs []string
	rule     *genRule // Kind Gen only
	// cc_object modules srcs name (`:bcm_object`)
	objSrcs []*Node
	// built outside ninja by xtask (provided.go): out and exports only, never in g.order
	provided bool
	// provided nodes: every artifact a dependent links, in order (out is the first)
	libs []string
	// Rust nodes only (rust.go)
	rust *rustCrate
	// final cc links: the generated staticlib holding the link's Rust (rust.go rustStaticlib), "" = none; whole: linked whole
	rustlib      string
	rustlibWhole bool
	// shared libraries and binaries: the version_script file (absolute), "" = none; versionScript resolves it
	versionScript string
}

// genClosure: generated files n and its dependencies produce; n's compiles wait for them (headers).
// Not global: the aidl tool's own compiles must not wait for aidl output.
func genClosure(n *Node, seen map[*Node]bool, out *[]string) {
	if seen[n] {
		return
	}
	seen[n] = true
	*out = append(*out, n.gen...)
	for _, d := range append(append(append(append([]*Node{}, n.deps.headers...), n.deps.static...), n.deps.shared...), n.deps.whole...) {
		genClosure(d, seen, out)
	}
}

type Graph struct {
	tree, out string
	cfg       Config
	soong     SoongConfig
	// the libc++ headers of the stl provider: -isystem of every libc++ C++ compile (stl.go libcxxProvider)
	libcxx string
	omit   map[string]bool
	// dependencies dropped by omit: "<side>/<module> <omitted module>"
	dropped map[string]bool
	// providers.toml rows built outside the tree (provided.go)
	provided map[string]Provided
	// bionic sides' provided modules: the LLVM runtimes archives built for the guest (xtask runtimes) under Soong's module names (libc++_static_noexcept, libclang_rt.builtins, ...)
	nbProvided map[string]Provided
	// providers.toml rows replacing only some sources of a module, and the "module\x00file" ones dropped (provided.go)
	partial     map[string]Partial
	droppedSrcs map[string]bool
	// providers.toml `rust_os = "android"` rows: device Rust modules compiled as Android userspace (rust.go rustOSSide)
	rustAndroid map[string]bool
	// providers.toml `defines` rows: module -> macros its device cc compiles get (provided.go checkDefines)
	defines map[string][]string
	// the C++ output nodes of xsd_config modules, by the module's key (xsd.go xsdCppNode)
	xsdCpp map[key]*Node
	// providers.toml `module:` rows: module -> the tree module built in its place (node), whose output name must be the module's (outName)
	stand map[string]string
	// module types defined as another type plus hooks -> that type: soong_config_module_type names (their module_type), the ART plugin's art_* types (artplugin.go)
	baseTypes map[string]string
	// soong_config_module_type definitions by name (soongconfig.go)
	soongConfigTypes map[string]*soongConfigType
	mods             map[Side]map[string][]*Module
	nodes            map[key]*Node
	order            []*Node
	problems         []string
	// references no module of the tree answers to (see notFound)
	missing map[string]bool
	// tree-relative directories modules name by path (include_dirs) that do not exist
	missingPaths map[string]bool
	// files emit generates (genrule scripts, Rust staticlib roots) by path, written by writeGenFiles after emit
	genFiles map[string]string
	// host binaries source generators run (gensrc.go hostTool), nil entry = module missing
	tools map[string]*Node
	// aidl_interface / hidl_interface modules by name/side (nil = none or unusable), for the modules synth makes from them
	aidlIfaces map[string]*aidlIface
	// the same interfaces by module dir, and the interface owning each .aidl path (nil = none), for aidl crates (aidlcrates.go aidlOwner)
	aidlDirs   map[string][]*Module
	aidlOwners map[string]*aidlIface
	hidlIfaces map[string]*hidlIface
	// aidl_interface versions' preprocessed.aidl by side/name/version ("" = none) and their edges (aidliface.go aidlPreprocessed)
	aidlPre  map[string]string
	aidlPreB strings.Builder
	// the combined_apis module, read once (combinedapis.go)
	combined     *combinedApis
	combinedRead bool
	// modules synthesized by ref/side (synth.go), nil = none
	synthMods map[string]*Module
	// build/release config (release.go) and aconfig codegen (aconfig.go); nil without -release
	release *Release
	acfg    *aconfigGraph
	// Java modules (java.go)
	java *javaGraph
}

func (g *Graph) problem(format string, args ...any) {
	g.problems = append(g.problems, fmt.Sprintf(format, args...))
}

// notFound records a reference lookup that found no module: a module, defaults or filegroup the tree
// lacks, which the caller may fetch. Stored the way the AOSP index takes it (`name`, `//dir:name`: no
// leading `:`, no `#tag`), so a project can be looked up without re-parsing prose.
func (g *Graph) notFound(ref string) {
	ref = strings.TrimPrefix(ref, ":")
	if i := strings.IndexByte(ref, '#'); i >= 0 {
		ref = ref[:i]
	}
	g.missing = addKey(g.missing, ref)
}

func addKey(m map[string]bool, k string) map[string]bool {
	if m == nil {
		m = map[string]bool{}
	}
	m[k] = true
	return m
}

func sortedKeys[V any](m map[string]V) []string {
	out := make([]string, 0, len(m))
	for r := range m {
		out = append(out, r)
	}
	sort.Strings(out)
	return out
}

// noDeviceVariant: Soong registers the module host-only (`*_host` types, rust_proc_macro) or it opts out with `device_supported: false`.
func (g *Graph) noDeviceVariant(m *Module) bool {
	t := g.baseType(m.Type)
	return strings.Contains(t, "_host") || t == "rust_proc_macro" || !m.Props.boolean("device_supported", true)
}

// ccGoalSide: a C/C++/Rust goal builds its device variant; a host-only one (aapt2's cc_binary_host) its host one -- a device variant would be a build tool linked as if shipped, its closure (libprotobuf-cpp-full) under the licence gate.
func (g *Graph) ccGoalSide(goal string) Side {
	if m := g.lookup(goal, Device); m != nil && g.noDeviceVariant(m) {
		return Host
	}
	return Device
}

// lookup finds a module by Soong reference: `name`, `//dir:name`, `name#tag`; prebuilts, imports
// and NDK/LLNDK stubs lose to a source definition of the same name.
func (g *Graph) lookup(ref string, side Side) *Module { return g.lookupWith(ref, side, isPrebuiltType) }

// isPrebuiltType: module types lookup skips. fdo_profile: PGO profile data sharing a library's name, never its code.
func isPrebuiltType(t string) bool {
	return strings.Contains(t, "prebuilt") || strings.Contains(t, "_import") || strings.HasPrefix(t, "ndk_") ||
		strings.HasPrefix(t, "llndk") || strings.Contains(t, "stubs") || t == "fdo_profile"
}

// lookupWith is lookup with the module types to skip given; among several candidates the first by directory wins.
func (g *Graph) lookupWith(ref string, side Side, skip func(string) bool) *Module {
	ref = strings.TrimPrefix(ref, ":")
	if i := strings.IndexByte(ref, '#'); i >= 0 {
		ref = ref[:i]
	}
	dir := ""
	if strings.HasPrefix(ref, "//") {
		if d, n, ok := strings.Cut(ref[2:], ":"); ok {
			dir, ref = d, n
		}
	}
	var cands []*Module
	for _, m := range g.mods[side][ref] {
		if dir != "" && m.Dir != dir && !strings.HasPrefix(m.Dir, dir+"/") {
			continue
		}
		if skip(m.Type) {
			continue
		}
		cands = append(cands, m)
	}
	if len(cands) == 0 {
		// made by a module type's load hook, defined in no Android.bp (synth.go)
		if dir == "" {
			return g.synth(ref, side)
		}
		return nil
	}
	sort.Slice(cands, func(i, j int) bool { return cands[i].Dir < cands[j].Dir })
	// a hidl_interface is named like the cc library its load hook creates
	if cands[0].Type == "hidl_interface" {
		return g.hidlLibrary(cands[0], side)
	}
	return cands[0]
}

// resolveDefaults merges a module's defaults (recursively) under its own properties.
func (g *Graph) resolveDefaults(m *Module, side Side, seen map[string]bool) *Obj {
	acc := newObj()
	// defaults.go applyDefaults prepends each defaults module in turn (proptools.PrependProperties): lists end up last-listed defaults
	// first, then earlier ones, then the module's own; a scalar keeps the module's own value, else the first-listed defaults'. So merge
	// (later lists after, later scalars winning) the defaults in reverse, then the module.
	defs := slices.Clone(m.Props.strs("defaults"))
	slices.Reverse(defs)
	for _, d := range defs {
		if seen[d] {
			continue
		}
		seen[d] = true
		// a defaults module never loses to a prebuilt; its (soong_config) type may contain "stubs"
		dm := g.lookupWith(d, side, func(t string) bool { return isPrebuiltType(t) && !strings.HasSuffix(t, "_defaults") })
		if dm == nil {
			g.problem("%s: defaults %s not found", m.Name, d)
			g.notFound(d)
			continue
		}
		acc = merge(acc, g.resolveDefaults(dm, side, seen))
	}
	return merge(acc, g.soongConfigProps(m))
}

// flatten applies the blocks of one side, arch and kind onto the base properties, in Soong's order: soong_config_variables (a load hook), OS blocks (os mutator), `arch` with its variant and feature sub-blocks, `multilib`, OS+arch blocks (arch mutator), `static`/`shared` (link mutator).
// The order decides flag order: libz's `arch: { x86_64 }` -DCPU_NO_SIMD must precede its `target: { android_x86_64 }` -UCPU_NO_SIMD.
func (g *Graph) flatten(m *Module, side Side, kind Kind) *Obj {
	p := g.resolveDefaults(m, side, map[string]bool{})
	base := newObj()
	// `static` / `shared` stay in base: arch and target blocks may hold them too (Soong's StaticProperties / SharedProperties are arch-variant; ART's codegen hook writes target.android.static), each merged into the module's own
	for _, k := range p.keys {
		switch k {
		case "target", "arch", "multilib", "defaults", "product_variables":
		default:
			base.set(k, p.m[k])
		}
	}
	target := func(blocks []string) {
		if t := p.obj("target"); t != nil {
			for _, b := range blocks {
				if o := t.obj(b); o != nil {
					base = merge(base, o)
				}
			}
		}
	}
	osSide := side
	if isRustType(g.baseType(m.Type)) {
		osSide = g.rustOSSide(m, side)
	}
	arch, bits := g.cfg.archOf(side)
	target(osBlocks(osSide, bits))
	if a := p.obj("arch"); a != nil {
		if o := a.obj(arch); o != nil {
			base = merge(base, o)
			// variant and feature sub-blocks: only the device toolchain has a variant and features
			if side == Device {
				for _, b := range g.soong.Device.blocks() {
					if s := o.obj(b); s != nil {
						base = merge(base, s)
					}
				}
			}
		}
	}
	if ml := p.obj("multilib"); ml != nil {
		if o := ml.obj(fmt.Sprintf("lib%d", bits)); o != nil {
			base = merge(base, o)
		}
	}
	target(osArchBlocks(osSide, arch))
	var part *Obj
	if kind == Static || kind == Shared {
		part = base.obj(kind.String())
	}
	base = removeKey(removeKey(base, "static"), "shared")
	if part != nil {
		base = merge(base, part)
	}
	return g.applyProductVariables(m, base, p.obj("product_variables"))
}

// kindOf is the artifact a module produces when used with `want` (Static/Shared as the dependent asks).
func kindOf(t string, want Kind) (Kind, bool) {
	if isRustType(t) {
		return rustKind(t, want)
	}
	switch {
	case strings.HasSuffix(t, "_headers"):
		return Headers, true
	case strings.HasPrefix(t, "cc_binary"):
		return Binary, true
	case t == "cc_library_static" || t == "cc_library_host_static":
		return Static, true
	case t == "cc_library_shared" || t == "cc_library_host_shared":
		return Shared, true
	case t == "cc_library" || t == "cc_object" || t == ccAconfigType:
		if want == Static || want == Shared {
			return want, true
		}
		return Shared, true
	}
	return 0, false
}

// cxxCompat: bionic headers AOSP code relies on and glibc lacks or differs in, put first on the C++ include path of device-side modules (never C: stdatomic.h's C branch needs bionic's bits/), linked unchanged into <out>/compat/cxx.
//   - stdatomic.h: in C++ bionic maps it onto <atomic>; clang's is incompatible with <atomic> before C++23 (liblog, libbinder, libutils' LightRefBase.h include it from C++).
//
// Kernel uapi headers only bionic carries (linux/ashmem.h, ...) are not here: the libc provider (atl-bionic) forwards them.
var cxxCompat = []compatHeader{
	{"stdatomic.h", "bionic/libc/include/stdatomic.h"},
}

// deviceCompat: bionic headers on the include path of every device-side C and C++ compile, linked unchanged into <out>/compat/device.
// Bionic's sys/cdefs.h includes it into every translation unit that includes a libc header; glibc's does not, and AOSP headers use its macros without including it (icu's ndk_headers, nativewindow's hardware_buffer_aidl.h, libsync's sync.h).
//   - android/versioning.h: __INTRODUCED_IN & co.; with weakAvailability they are availability(android, …) attributes, which clang ignores on a linux-gnu target. Force-included (deviceForced).
//     Function-like macros, no API: not a header atl-bionic generates.
var deviceCompat = []compatHeader{
	{"android/versioning.h", "bionic/libc/include/android/versioning.h"},
}

// libcCompat: headers of the libc provider (atl-bionic, generated from bionic's) every device-side compile needs whether or not the module depends on libc, linked from the provider's include dirs into <out>/compat/device.
//   - android/api-level.h: the level names (`__ANDROID_API_U__`, …), android_get_device_api_level; bionic's sys/cdefs.h brings it into every unit, and libc++'s locale_base_api/android.h includes it under __ANDROID__. atl-bionic generates it include-free, so it is forced (deviceForced).
var libcCompat = []string{"android/api-level.h"}

// libcModule: the module whose provider supplies libcCompat (system_shared_libs brings it to nearly every device module, not to all).
const libcModule = "libc"

// deviceForced: the compat/device headers every device-side C and C++ compile force-includes, in order; not assembly, nothing there uses them.
// Only an include-free header may be forced: one that includes a libc header runs glibc's features.h before the source's own `#define _GNU_SOURCE` / `#undef _GNU_SOURCE`, which then has no effect (measured with bionic's api-level.h forced: libbase posix_strerror_r.cpp got GNU strerror_r; sqlite, zstd, libusbhost, bcc lost GNU declarations).
var deviceForced = []string{"android/versioning.h", "android/api-level.h"}

// libcxxShim: headers libc++ includes under __ANDROID__ that glibc lacks, written into <out>/compat/libcxx, searched last (-idirafter) by every device-side C++ compile, so a header the system has wins.
//   - xlocale.h: libc++'s locale_base_api/android.h includes it for locale_t, which bionic's declares; glibc declares locale_t in <locale.h> (its own xlocale.h went in 2.26), so the shim includes that. The libc++ objects stay byte-identical with it (xtask check step libcxx).
//
//go:embed compat/xlocale.h
var xlocaleShim string

var libcxxShim = map[string]string{"xlocale.h": xlocaleShim}

// libcHeaderLinks: libcCompat name -> the file in the libc provider's include dirs (provided.go Include), the first dir holding it; fails when the provider is missing or lacks one, since every device compile force-includes them.
func libcHeaderLinks(provided map[string]Provided) (map[string]string, error) {
	p, ok := provided[libcModule]
	if !ok {
		return nil, fmt.Errorf("no provider for %s (providers.toml): it supplies %v to every device compile", libcModule, libcCompat)
	}
	out := map[string]string{}
	for _, name := range libcCompat {
		for _, dir := range p.Include {
			f := filepath.Join(dir, name)
			if _, err := os.Stat(f); err == nil {
				out[name] = f
				break
			}
		}
		if out[name] == "" {
			return nil, fmt.Errorf("the %s provider's include dirs %v hold no %s", libcModule, p.Include, name)
		}
	}
	return out, nil
}

// writeCompat fills <out>/compat: cxx and device link their tree headers, device also the libc provider's (libcCompat), libcxx holds the shims.
// An entry is written only when its target or text changed, so an unchanged header keeps its mtime for ninja; an entry no longer wanted is removed.
func writeCompat(tree, out string, provided map[string]Provided) error {
	libc, err := libcHeaderLinks(provided)
	if err != nil {
		return err
	}
	links := map[string]string{}
	for dir, set := range map[string][]compatHeader{"cxx": cxxCompat, "device": deviceCompat} {
		for _, h := range set {
			links[filepath.Join(dir, h.name)] = filepath.Join(tree, h.path)
		}
	}
	for name, f := range libc {
		links[filepath.Join("device", name)] = f
	}
	root := filepath.Join(out, "compat")
	wanted := map[string]bool{}
	for rel, target := range links {
		wanted[rel] = true
		if err := placeLink(filepath.Join(root, rel), target); err != nil {
			return err
		}
	}
	for name, text := range libcxxShim {
		rel := filepath.Join("libcxx", name)
		wanted[rel] = true
		if err := placeFile(filepath.Join(root, rel), text); err != nil {
			return err
		}
	}
	return pruneCompat(root, wanted)
}

// placeLink leaves a symlink already pointing at target alone, else makes path one.
func placeLink(path, target string) error {
	if got, err := os.Readlink(path); err == nil && got == target {
		return nil
	}
	if err := removeEntry(path); err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	return os.Symlink(target, path)
}

// placeFile leaves a regular file already holding text alone, else writes it; a symlink there is replaced, never written through.
func placeFile(path, text string) error {
	if fi, err := os.Lstat(path); err == nil && fi.Mode().IsRegular() {
		if b, err := os.ReadFile(path); err == nil && string(b) == text {
			return nil
		}
	}
	if err := removeEntry(path); err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	return os.WriteFile(path, []byte(text), 0o644)
}

// removeEntry removes the file or symlink at path; a missing one is no error.
func removeEntry(path string) error {
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return err
	}
	return nil
}

// pruneCompat removes the files and symlinks under root that wanted does not name, then each directory no wanted entry lies under.
// WalkDir does not follow symlinks, so a link into the tree is removed, never its target.
func pruneCompat(root string, wanted map[string]bool) error {
	keepDir := map[string]bool{}
	for rel := range wanted {
		for d := filepath.Dir(rel); d != "."; d = filepath.Dir(d) {
			keepDir[d] = true
		}
	}
	var dirs []string
	err := filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if errors.Is(err, fs.ErrNotExist) {
			return nil // first run: no compat dir yet
		}
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(root, path)
		if err != nil || rel == "." {
			return err
		}
		switch {
		case d.IsDir():
			dirs = append(dirs, rel)
		case !wanted[rel]:
			return os.Remove(path)
		}
		return nil
	})
	if err != nil {
		return err
	}
	// deepest first: a directory is removed after what lies under it
	for i := len(dirs) - 1; i >= 0; i-- {
		if !keepDir[dirs[i]] {
			if err := os.Remove(filepath.Join(root, dirs[i])); err != nil {
				return err
			}
		}
	}
	return nil
}

// compatHeader: include name -> tree-relative file.
type compatHeader struct{ name, path string }

func compatPath(set []compatHeader, name string) string {
	for _, h := range set {
		if h.name == name {
			return h.path
		}
	}
	panic("compat header " + name + " not in its set")
}

// stlModules: Soong's STL libraries (cc/stl.go). Every link takes -lc++ with its module's stl selection (stl.go linkStlFlags), so a module naming one directly (libcxx's shared_libs) needs nothing more.
var stlModules = map[string]bool{"libc++": true, "libc++_static": true}

// node builds (once) the node for a reference, depth first.
func (g *Graph) node(ref string, side Side, want Kind, from string) *Node {
	name := strings.TrimPrefix(ref, ":")
	// providers.toml replaces bionic and platform pieces for glibc sides only; a bionic side builds them from the tree, and takes only its own provided modules (the runtimes build: -nb-provided)
	if !side.glibc() {
		if p, ok := g.nbProvided[name]; ok {
			return g.providedNode(name, side, p)
		}
		return g.treeNode(ref, side, want, from)
	}
	// providers.toml `provider = "none"`: the dependency is dropped (bionic itself, platform-only APIs); recorded for the caller's none gate (graph.go)
	if g.omit[name] {
		if from != "goal" {
			g.dropped = addKey(g.dropped, side.String()+"/"+from+" "+name)
		}
		return nil
	}
	if stlModules[name] {
		return nil
	}
	if p, ok := g.provided[name]; ok {
		return g.providedNode(name, side, p)
	}
	// providers.toml `module:`: the tree module that installs under this name stands in for it; anything else under the name would be a different library
	if alt, ok := g.stand[name]; ok {
		n := g.node(alt, side, want, from)
		if n != nil && g.outName(n) != name {
			g.problem("providers.toml: %s: module:%s installs as %s, not %s (its stem / suffix)", name, alt, g.outName(n), name)
			return nil
		}
		return n
	}
	return g.treeNode(ref, side, want, from)
}

// treeNode: node for a reference the tree's own module answers.
func (g *Graph) treeNode(ref string, side Side, want Kind, from string) *Node {
	m := g.lookup(ref, side)
	if m == nil {
		// made by a module type the tree has: lookup recorded why there is none, and the name is nothing to fetch
		if !g.synthOwned(ref) {
			g.problem("%s: %s module %s not found", from, side, ref)
			g.notFound(ref)
		}
		return nil
	}
	kind, ok := kindOf(g.baseType(m.Type), want)
	if !ok {
		g.problem("%s: %s is a %s (unsupported)", from, m.Name, m.Type)
		return nil
	}
	k := key{m.Name, side, kind}
	if n, ok := g.nodes[k]; ok {
		return n
	}
	n := &Node{key: k, mod: m}
	g.nodes[k] = n
	n.props = g.flatten(m, side, kind)
	if isRustType(g.baseType(m.Type)) {
		g.rustNode(n)
		g.order = append(g.order, n)
		return n
	}
	aconfig := g.baseType(m.Type) == ccAconfigType
	if aconfig {
		g.ccAconfigDeps(n)
	}
	var files []srcFile
	for _, s := range n.props.strs("srcs") {
		files = append(files, g.srcFiles(n, s)...)
	}
	if m.aidl != nil {
		files = append(files, m.aidl.srcs...)
	}
	files = append(files, g.ccAidlLibs(n)...)
	// a provided shared object links shared, and first, whichever list names it (static_libs, whole_static_libs included); a provided archive stays where it is named
	var provided []*Node
	if p := g.partialProvider(n); p != nil {
		provided = append(provided, p)
	}
	dep := func(refs []string, want Kind) []*Node {
		var out []*Node
		for _, r := range refs {
			d := g.node(r, side, want, m.Name)
			switch {
			case d == nil:
			case d.provided && d.kind == Shared && want != Headers:
				provided = append(provided, d)
			default:
				out = append(out, d)
			}
		}
		return out
	}
	// cc_object modules named in srcs (srcFiles) are linked whole, as Soong links their object
	n.deps.whole = append(dep(n.props.strs("whole_static_libs"), Static), n.objSrcs...)
	n.deps.static = dep(n.props.strs("static_libs"), Static)
	n.deps.shared = dep(n.props.strs("shared_libs"), Shared)
	// Soong's implicit system_shared_libs (cc/linker.go): the toolchain's default when unset, `[]` declines it; android sides only, host tools link the host's C library
	if side.android() {
		system := g.soong.SystemSharedLibs
		if n.props.has("system_shared_libs") {
			system = n.props.strs("system_shared_libs")
		}
		n.deps.shared = append(n.deps.shared, dep(system, Shared)...)
	}
	n.deps.shared = uniqueNodes(append(provided, n.deps.shared...))
	// Soong's runtime_libs (cc/linker.go linkerDeps, minus exclude_runtime_libs; cc.go: shared variant, runtimeDepTag = installed with the module, never linked): built, and found at run time through linkLibs' rpath
	for _, r := range n.props.strs("runtime_libs") {
		if slices.Contains(n.props.strs("exclude_runtime_libs"), r) {
			continue
		}
		if d := g.node(r, side, Shared, m.Name); d != nil {
			n.deps.runtime = append(n.deps.runtime, d)
		}
	}
	n.deps.headers = dep(n.props.strs("header_libs"), Headers)
	exported := map[string]bool{}
	for _, r := range n.props.strs("export_generated_headers") {
		exported[r] = true
	}
	for _, r := range n.props.strs("generated_headers") {
		if gn := g.ccGenNode(r, side, m.Name); gn != nil {
			g.useGen(n, gn)
			if exported[r] {
				n.genExports = append(n.genExports, gn.rule.incs...)
			}
		}
	}
	skip := map[string]bool{}
	for _, r := range n.props.strs("exclude_generated_sources") {
		skip[r] = true
	}
	for _, r := range n.props.strs("generated_sources") {
		if skip[r] {
			continue
		}
		if gn := g.ccGenNode(r, side, m.Name); gn != nil {
			g.useGen(n, gn)
			for _, o := range gn.rule.outs {
				// headers are only waited on
				if !isHeaderSrc(o) {
					files = append(files, srcFile{path: o, root: g.genDir(gn, ""), gen: gn})
				}
			}
		}
	}
	g.genSources(n, g.dropPartial(n, dedupSrcs(files, g.expand(n, n.props.strs("exclude_srcs")))))
	if aconfig {
		g.addGen(n, g.ccAconfigGen(n), true)
	}
	n.versionScript = g.versionScript(n)
	n.exports = g.exportsOf(n)
	g.order = append(g.order, n)
	return n
}

// versionScript: the file a shared library's or binary's link takes as -Wl,--version-script (cc/linker.go linkerFlags: version_script, after target / arch blocks; a path or a `:genrule` output), "" = none.
// Not on archives: Soong reads it in the linker, which a static library has none of. Vendor / product variants, the only other source of one, are never built here.
func (g *Graph) versionScript(n *Node) string {
	v := n.props.str("version_script")
	if v == "" || n.kind != Shared && n.kind != Binary {
		return ""
	}
	files, ok := g.resolveSrc(n.mod.Dir, n.side, v, n.mod.Name)
	if !ok {
		return ""
	}
	if len(files) != 1 {
		g.problem("%s: version_script %s names %d files, want 1", n.mod.Name, v, len(files))
		return ""
	}
	return g.abs(files[0].path)
}

func uniqueNodes(list []*Node) []*Node {
	seen := map[*Node]bool{}
	var out []*Node
	for _, n := range list {
		if !seen[n] {
			seen[n] = true
			out = append(out, n)
		}
	}
	return out
}

// srcFiles expands one `srcs` entry of n; the genrules it names are consumed (n's compiles wait for them).
func (g *Graph) srcFiles(n *Node, s string) []srcFile {
	if isRef(s) {
		if m := g.lookup(s, n.side); m != nil && g.baseType(m.Type) == "cc_object" {
			if o := g.node(s, n.side, Static, n.mod.Name); o != nil {
				n.objSrcs = append(n.objSrcs, o)
			}
			return nil
		}
	}
	files, _ := g.resolveSrc(n.mod.Dir, n.side, s, n.mod.Name)
	var out []srcFile
	for _, f := range files {
		if f.gen != nil {
			g.useGen(n, f.gen)
			// headers a genrule produces are only waited on
			if isHeaderSrc(f.path) {
				continue
			}
		}
		out = append(out, f)
	}
	return out
}

// dedupSrcs drops excluded (exclude_srcs) and repeated sources; defaults and target blocks may list one file twice, Soong builds it once.
func dedupSrcs(files []srcFile, exclude []string) []srcFile {
	skip := map[string]bool{}
	for _, e := range exclude {
		skip[e] = true
	}
	var out []srcFile
	for _, f := range files {
		if !skip[f.path] {
			skip[f.path] = true
			out = append(out, f)
		}
	}
	return out
}

// srcFile is one file a `srcs`-style entry names: tree-relative, or absolute under the build dir when a
// genrule (gen) or a cc_object (obj: its object file, objectOut) produces it. root is the import root aidl and proto sources need.
type srcFile struct {
	path, root string
	gen        *Node
	obj        *Node
}

// isRef: a srcs entry naming a module (`:name`, `//dir:name`) rather than a path.
func isRef(s string) bool { return strings.HasPrefix(s, ":") || strings.HasPrefix(s, "//") }

// objectOut: a cc_object node's own output, Soong's PathForModuleOut(<name>, + ".o" unless the name ends in it) (cc/object.go link), which `:name` in a genrule's srcs names;
// its static archive (n.out) is what cc modules link whole.
func (g *Graph) objectOut(n *Node) string {
	name := n.name
	if !strings.HasSuffix(name, ".o") {
		name += ".o"
	}
	return filepath.Join(g.out, n.side.String(), n.name, name)
}

// resolveSrc expands one entry of srcs / tool_files written in dir: a path or glob, `:name` or
// `//dir:name` naming a filegroup (its srcs, recursively), a genrule (its outputs) or a cc_object (its object file). ok=false: a
// reference no known module of those types answers to (problem recorded).
func (g *Graph) resolveSrc(dir string, side Side, s, from string) (files []srcFile, ok bool) {
	if !isRef(s) {
		for _, f := range g.globs(dir, []string{s}) {
			files = append(files, srcFile{path: f, root: dir})
		}
		return files, true
	}
	// a Java module's outputs (base.go setOutputFiles): {.jar} its implementation and resources jar, {.hjar} its header jar,
	// {.generated_srcjars} its generated sources (a java_aconfig_library's srcjar)
	if name, tag := splitTag(s); (tag == ".jar" || tag == ".hjar" || tag == ".generated_srcjars") && g.java != nil {
		if m := g.lookupJava(name, side); m != nil && g.isJava(m.Type) {
			jn := g.jnode(name, side, from)
			if jn == nil {
				return nil, false
			}
			out := map[string]string{".jar": jn.implRes, ".hjar": jn.header, ".generated_srcjars": jn.srcjar}[tag]
			if out == "" {
				g.problem("%s: %s has no %s output", from, name, tag)
				return nil, false
			}
			return []srcFile{{path: out, root: filepath.Dir(out)}}, true
		}
	}
	if name, tag := splitTag(s); g.java != nil {
		if files, handled := g.appOutputs(name, tag, side, from); handled {
			return files, files != nil
		}
	}
	m := g.lookup(s, side)
	switch {
	case m != nil && g.baseType(m.Type) == "cc_object":
		o := g.node(s, side, Static, from)
		if o == nil {
			return nil, false
		}
		return []srcFile{{path: g.objectOut(o), obj: o}}, true
	case m != nil && g.baseType(m.Type) == ccPreprocessType:
		pn := g.preprocessNode(s, side, from)
		if pn == nil {
			return nil, false
		}
		return []srcFile{{path: pn.rule.outs[0], root: g.genDir(pn, ""), gen: pn}}, true
	case m != nil && isGenType(g.baseType(m.Type)):
		gn := g.genNode(s, side, from)
		// providers.toml none: dropped
		if gn == nil {
			return nil, true
		}
		for _, o := range gn.rule.outs {
			files = append(files, srcFile{path: o, root: g.genDir(gn, ""), gen: gn})
		}
		return files, true
	case m != nil && m.Type == "filegroup":
		root := filepath.Join(m.Dir, m.Props.str("path"))
		ok = true
		exclude := g.excludeSet(m.Dir, side, m.Props.strs("exclude_srcs"), from)
		// filegroup.go: srcs (less exclude_srcs), then device_first_srcs and device_common_srcs, whose dependencies are device variants
		type entry struct {
			src  string
			side Side
			excl bool
		}
		var entries []entry
		for _, e := range m.Props.strs("srcs") {
			entries = append(entries, entry{e, side, true})
		}
		for _, e := range append(m.Props.strs("device_first_srcs"), m.Props.strs("device_common_srcs")...) {
			entries = append(entries, entry{e, Device, false})
		}
		for _, en := range entries {
			e := en.src
			sub, subOK := g.resolveSrc(m.Dir, en.side, e, from)
			ok = ok && subOK
			for _, f := range sub {
				if f.gen == nil && en.excl && exclude[f.path] {
					continue
				}
				// a nested reference keeps the root of the group that owns the file
				if f.gen == nil && !isRef(e) {
					f.root = root
				}
				files = append(files, f)
			}
		}
		return files, ok
	}
	g.problem("%s: source %s is not a known filegroup", from, s)
	// a module of another type under that name is defined, not missing
	if m == nil {
		g.notFound(s)
	}
	return nil, false
}

func (g *Graph) expand(n *Node, pats []string) []string { return g.globs(n.mod.Dir, pats) }

// excludeSet: the tree paths exclude_srcs names (PathsForModuleSrcExcludes): paths and globs below dir, and the files of module references.
func (g *Graph) excludeSet(dir string, side Side, excludes []string, from string) map[string]bool {
	out := map[string]bool{}
	for _, e := range excludes {
		if isRef(e) {
			files, _ := g.resolveSrc(dir, side, e, from)
			for _, f := range files {
				out[f.path] = true
			}
			continue
		}
		for _, f := range g.globs(dir, []string{e}) {
			out[f] = true
		}
	}
	return out
}

// globs resolves paths and globs (`**` included) relative to dir; results are tree-relative.
func (g *Graph) globs(dir string, pats []string) []string {
	var out []string
	for _, p := range pats {
		if !strings.ContainsAny(p, "*?[") {
			out = append(out, filepath.Join(dir, p))
			continue
		}
		out = append(out, globTree(filepath.Join(g.tree, dir), p, dir)...)
	}
	return out
}

func removeKey(o *Obj, k string) *Obj {
	out := newObj()
	for _, key := range o.keys {
		if key != k {
			out.set(key, o.m[key])
		}
	}
	return out
}

// baseType: the module type behind a `soong_config_module_type` wrapper (libandroid_runtime is a
// `cc_library_shared_for_libandroid_runtime`) or an ART plugin type (libdexfile_support is an `art_cc_library_static`), else the type itself.
func (g *Graph) baseType(t string) string {
	for i := 0; i < 8; i++ {
		b, ok := g.baseTypes[t]
		if !ok {
			return t
		}
		t = b
	}
	return t
}

// exportsOf: the include flags a node hands to its dependents.
func (g *Graph) exportsOf(n *Node) []string {
	out := append([]string{}, n.genExports...)
	for _, d := range n.props.strs("export_include_dirs") {
		out = append(out, "-I"+g.abs(filepath.Join(n.mod.Dir, d)))
	}
	for _, d := range n.props.strs("export_system_include_dirs") {
		out = append(out, "-isystem", g.abs(filepath.Join(n.mod.Dir, d)))
	}
	reexport := map[string]bool{}
	for _, l := range []string{"export_header_lib_headers", "export_static_lib_headers", "export_shared_lib_headers"} {
		for _, r := range n.props.strs(l) {
			// `libdexfile#impl` re-exports what shared_libs `libdexfile#impl` names: the module, whichever variant tag (lookup drops it too)
			if i := strings.IndexByte(r, '#'); i >= 0 {
				r = r[:i]
			}
			reexport[r] = true
		}
	}
	for _, d := range append(append(append(append([]*Node{}, n.deps.headers...), n.deps.static...), n.deps.shared...), n.deps.whole...) {
		if reexport[d.name] || d.kind == Headers && reexport[d.mod.Name] || slices.Contains(n.reexport, d) {
			out = append(out, d.exports...)
		}
	}
	// whole_static_libs' exports travel with the merged archive
	for _, d := range n.deps.whole {
		out = append(out, d.exports...)
	}
	return dedup(out)
}

func dedup(in []string) []string {
	seen := map[string]bool{}
	var out []string
	for i := 0; i < len(in); i++ {
		s := in[i]
		if s == "-isystem" && i+1 < len(in) {
			pair := s + " " + in[i+1]
			if !seen[pair] {
				seen[pair] = true
				out = append(out, s, in[i+1])
			}
			i++
			continue
		}
		if !seen[s] {
			seen[s] = true
			out = append(out, s)
		}
	}
	return out
}

// abs: a tree-relative path under the tree, an absolute one (generated file) as is.
func (g *Graph) abs(rel string) string {
	if filepath.IsAbs(rel) {
		return rel
	}
	return filepath.Join(g.tree, rel)
}

func (g *Graph) genDir(n *Node, sub string) string {
	return filepath.Join(g.out, "gen", n.side.String(), n.name, sub)
}

// staticClosure: archives a final link needs for n's static (and whole static) dependencies; through a Rust node also its library crates, whose cc libraries the final link takes (rust.go).
func staticClosure(n *Node, seen map[*Node]bool, out *[]*Node) {
	deps := slices.Concat(n.deps.whole, n.deps.static)
	if n.rust != nil {
		for _, c := range n.deps.crates {
			if c.kind == Rlib {
				deps = append(deps, c)
			}
		}
	}
	for _, d := range deps {
		if seen[d] {
			continue
		}
		seen[d] = true
		*out = append(*out, d)
		staticClosure(d, seen, out)
	}
}
