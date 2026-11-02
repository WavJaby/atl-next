// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"sort"
	"strconv"
	"strings"
)

// aidl_interface backends: Soong creates `<iface>[-V<n>]-<lang>` cc libraries (cpp, ndk) and Rust libraries (rust) in the interface's load hook, no Android.bp defines them.
//   - rust (addRustLibrary): a source-provider rlib (rustsrc.go rustAidl, type aidlRustType) generating its own sources with the same aidl flags as the cc backends but no -t / --log; crate_name = the interface
//     name, crate root = the versioned name (fixRustName), rustlibs = backend.rust.additional_rustlibs + the provider's.
// Spec: system/tools/aidl/build/{aidl_interface.go, aidl_interface_backends.go, aidl_gen_rule.go}. lookup falls back to synth (synth.go), which
// calls synthAidl to make the Module; its sources go through the extension table (gensrc.go genAidl) like any cc module's, with per-interface flags.
//   - Names (addLibrary): the next version (latest frozen + 1, "1" with no versions) and every frozen version are `<iface>-V<v>-<lang>`; an
//     unstable interface has no versions and one unversioned `<iface>-<lang>`. A stable interface has no unversioned backend.
//   - Sources: a frozen version = aidl_api/<iface>/<v>/**/*.aidl under the module dir (root = that dir); the next version = the module's `srcs` (root
//     local_include_dir).
//   - Libraries (addCppLibrary): shared libbinder + libutils (cpp; libcutils too with gen_trace, default on), libbinder_ndk (ndk), backend
//     additional_shared_libraries, then the imports' backends of the same lang (addLanguageLibraries); all but the additional ones are exported
//     (export_shared_lib_headers); generated headers exported. Only the platform variant is built: ndk gets -DBINDER_STABILITY_SUPPORT (target.platform).
//   - Imports (getImportWithVersion): `-V<n>` as written; an unstable import is bare; the importing interface's next version takes the import's
//     next version; a frozen version takes the import's latest. Version imports come from versions_with_info, else the module's imports.
//   - aidl flags (generateBuildActionsForSingleAidl, flagsForAidlGenRule): see aidlSpec.flags. Import types come from each direct import's
//     preprocessed.aidl (-p, aidlPreprocessed = buildPreprocessed, chained through the imports' own), as Soong's: -I of the import sources would
//     see two versions of one interface where a closure holds both (android.hardware.radio.*).
//   - RELEASE_AIDL_USE_UNFROZEN is false in the pinned release chain (flag_values: ap2a, ap3a false, no later config sets it), so an unfrozen
//     version of a frozen interface gets --previous_api_dir / --previous_hash.
//   - `headers` (aidl_library modules) are -I dirs after include_dirs (aidl_api.go getDeps).
//   - Not built, one problem each: java, cpp-analyzer, ndk_platform backends; a backend with enabled: false; a
//     host backend of an interface without host_supported; the next version of a `frozen: true` interface (Soong rejects its users); a
//     version the interface does not have. Soong's -Wall -Werror cflags, ldflags, tidy, sdk_version/stl are dropped (this build uses -w -Wno-error).

var (
	aidlLangSuffixes = []string{"-cpp-analyzer", "-ndk_platform", "-cpp", "-ndk", "-java-source", "-java", "-rust"}
	aidlVersionRe    = regexp.MustCompile(`-V(\d+)$`)
)

// parseModuleWithVersion: aidl_interface.go's function of the same name (`foo-V2` -> foo, 2; no suffix -> name, "").
func parseModuleWithVersion(name string) (string, string) {
	if m := aidlVersionRe.FindStringSubmatchIndex(name); m != nil {
		return name[:m[0]], name[m[2]:m[3]]
	}
	return name, ""
}

// parseAidlRef splits `<iface>[-V<n>]-<lang>`; ok=false when no backend suffix is there.
func parseAidlRef(ref string) (iface, version, lang string, ok bool) {
	for _, s := range aidlLangSuffixes {
		if base, found := strings.CutSuffix(ref, s); found {
			iface, version = parseModuleWithVersion(base)
			return iface, version, s[1:], iface != ""
		}
	}
	return "", "", "", false
}

// aidlIface is an aidl_interface module as one side sees it.
type aidlIface struct {
	mod      *Module
	side     Side
	props    *Obj
	versions []string
	// versions_with_info: version -> its imports
	withInfo map[string][]string
	unstable bool
	frozen   bool
	srcs     map[string][]srcFile
	imports  map[string][]aidlRef
}

// aidlRef is one version of one interface.
type aidlRef struct {
	iface   *aidlIface
	version string
}

// aidlIfaceOf: the aidl_interface module of that name, nil when the tree has none.
func (g *Graph) aidlIfaceOf(name string, side Side) *aidlIface {
	k := name + "/" + side.String()
	if i, ok := g.aidlIfaces[k]; ok {
		return i
	}
	var i *aidlIface
	if m := g.lookup(name, side); m != nil && m.Type == "aidl_interface" {
		p := g.flatten(m, side, Gen)
		i = &aidlIface{mod: m, side: side, props: p, withInfo: map[string][]string{}, srcs: map[string][]srcFile{}, imports: map[string][]aidlRef{},
			unstable: p.boolean("unstable", false), frozen: p.boolean("frozen", false)}
		i.versions = p.strs("versions")
		if l, ok := p.get("versions_with_info").([]Val); ok {
			i.versions = nil
			for _, v := range l {
				if o, ok := v.(*Obj); ok {
					i.versions = append(i.versions, o.str("version"))
					i.withInfo[o.str("version")] = o.strs("imports")
				}
			}
		}
	}
	if g.aidlIfaces == nil {
		g.aidlIfaces = map[string]*aidlIface{}
	}
	g.aidlIfaces[k] = i
	return i
}

// aidlOwned: ref names a backend of an aidl_interface the tree has.
func (g *Graph) aidlOwned(ref string) bool {
	iface, _, _, ok := parseAidlRef(strings.TrimPrefix(ref, ":"))
	return ok && (g.aidlIfaceOf(iface, Device) != nil || g.aidlIfaceOf(iface, Host) != nil)
}

// nextVersion: aidlInterface.nextVersion.
func (i *aidlIface) nextVersion() string {
	if i.unstable {
		return ""
	}
	if len(i.versions) == 0 {
		return "1"
	}
	n, _ := strconv.Atoi(i.versions[len(i.versions)-1])
	return strconv.Itoa(n + 1)
}

func (i *aidlIface) latestVersion() string {
	if len(i.versions) == 0 {
		return "0"
	}
	return i.versions[len(i.versions)-1]
}

// versionedName: aidlInterface.versionedName.
func (i *aidlIface) versionedName(version string) string {
	if version == "" {
		return i.mod.Name
	}
	return i.mod.Name + "-V" + version
}

// importsOf: aidlInterface.getImportsForVersion.
func (i *aidlIface) importsOf(version string) []string {
	if imp, ok := i.withInfo[version]; ok {
		return imp
	}
	return i.props.strs("imports")
}

// importWithVersion: aidlInterface.getImportWithVersion, the backend base name (`foo-V2`, or bare `foo`) an import of version `version` means.
func (i *aidlIface) importWithVersion(version, anImport string, other *aidlIface) string {
	if aidlVersionRe.MatchString(anImport) {
		return anImport
	}
	if other.unstable {
		return anImport
	}
	if version == i.nextVersion() || len(other.versions) == 0 {
		return other.versionedName(other.nextVersion())
	}
	return other.versionedName(other.latestVersion())
}

// resolveImports: the interfaces `version` imports, with the version each means. An import the tree lacks is a missing interface (the
// project to fetch) and a problem, once.
func (g *Graph) resolveImports(i *aidlIface, version string) []aidlRef {
	if r, ok := i.imports[version]; ok {
		return r
	}
	var out []aidlRef
	defer func() { i.imports[version] = out }()
	for _, imp := range i.importsOf(version) {
		name, _ := parseModuleWithVersion(imp)
		other := g.aidlIfaceOf(name, i.side)
		if other == nil {
			g.problem("%s: aidl_interface import %s not found", i.versionedName(version), name)
			g.notFound(name)
			continue
		}
		_, v := parseModuleWithVersion(i.importWithVersion(version, imp, other))
		out = append(out, aidlRef{other, v})
	}
	return out
}

// aidlSources: aidlInterface.srcsForVersion + getPaths. The next version's srcs are the module's (paths, globs, filegroups; root =
// local_include_dir), a frozen version's every .aidl below aidl_api/<iface>/<version>.
func (g *Graph) aidlSources(i *aidlIface, version string) []srcFile {
	if s, ok := i.srcs[version]; ok {
		return s
	}
	if c := i.mod.aidlCrate; c != nil {
		// an aidl crate: the closure aidlcrates.go computed, each file under its include dir
		i.srcs[version] = c.srcs
		return c.srcs
	}
	dir := i.mod.Dir
	var out []srcFile
	if version == i.nextVersion() {
		root := filepath.Join(dir, i.props.str("local_include_dir"))
		for _, s := range i.props.strs("srcs") {
			if isRef(s) {
				files, _ := g.resolveSrc(dir, i.side, s, i.mod.Name)
				out = append(out, files...)
				continue
			}
			for _, f := range g.globs(dir, []string{s}) {
				out = append(out, srcFile{path: f, root: root})
			}
		}
	} else {
		root := filepath.Join(dir, "aidl_api", i.mod.Name, version)
		for _, f := range globTree(filepath.Join(g.tree, root), "**/*.aidl", root) {
			out = append(out, srcFile{path: f, root: root})
		}
		sort.Slice(out, func(a, b int) bool { return out[a].path < out[b].path })
	}
	out = slices.DeleteFunc(out, func(f srcFile) bool { return filepath.Ext(f.path) != ".aidl" })
	i.srcs[version] = out
	return out
}

// aidlSpec is how one backend's sources are generated: the part of the aidl command line that is per interface and version.
type aidlSpec struct {
	lang                     string // cpp | ndk | rust
	name                     string // interface name (aidl_api/<name>)
	dir                      string // its module dir
	version                  string // "" = unversioned
	unstable                 bool
	warnings                 []string // -W flags of the unfrozen version
	trace, log, mockall, rpc bool
	// the imports' backends of the same lang (addLanguageLibraries / rust SourceProviderDeps)
	imports []string
	// rust: the imports' crate names, whose `mangled` modules the crate root re-exports (aidl_rust_source_provider.go importFlags)
	mangled   []string
	stability string
	minSdk    string
	roots     []string // own source roots (-N)
	includes  []string // tree-relative include_dirs and headers' dirs (-I)
	// the direct imports' preprocessed.aidl files (-p, aidlPreprocessed), absolute
	preprocessed []string
	prevVer      string // previous frozen version when this one is unfrozen, "" = none
	srcs         []srcFile
	// the missing previous .hash is one problem, not one per source
	prevReported bool
	// rust: --rust_java_type values (an aidl crate with parcelables, aidlcrates.go)
	javaTypes []string
	// rust, every aidl crate (framework AIDL our Rust services serve, each a subset, to Java callers): --rust_unimplemented_hook and --rust_java_nullability
	rustServed bool
}

func strVals(list []string) []Val {
	out := make([]Val, len(list))
	for i, s := range list {
		out[i] = s
	}
	return out
}

// synthAidl: the backend `<iface>[-V<n>]-<lang>` (synth.go). nil without a problem: ref names no backend of an aidl_interface; nil with one: the
// backend does not exist or is not supported.
func (g *Graph) synthAidl(ref string, side Side) *Module {
	name, ver, lang, ok := parseAidlRef(ref)
	if !ok {
		return nil
	}
	i := g.aidlIfaceOf(name, side)
	if i == nil {
		return nil
	}
	// `-java-source`: the aidl_gen_rule of the java backend, which `-java` compiles (addJavaLibrary)
	source := lang == "java-source"
	if source {
		lang = "java"
	}
	if lang != "cpp" && lang != "ndk" && lang != "rust" && lang != "java" {
		g.problem("%s: aidl_interface backend %s is unsupported", ref, lang)
		return nil
	}
	p := i.props
	be := p.obj("backend").obj(lang)
	if !be.boolean("enabled", true) {
		g.problem("%s: backend.%s.enabled is false in %s", ref, lang, name)
		return nil
	}
	if side == Host && !p.boolean("host_supported", false) {
		g.problem("%s: aidl_interface %s has no host variant (host_supported unset)", ref, name)
		return nil
	}
	next := i.nextVersion()
	switch {
	case ver == "" && next != "":
		g.problem("%s: %s is not unstable: its backends are %s-V<version>-%s only", ref, name, name, lang)
		return nil
	case ver != next && !slices.Contains(i.versions, ver):
		g.problem("%s: %s has no version %q (frozen %v, next %q)", ref, name, ver, i.versions, next)
		return nil
	case ver == next && i.frozen && !source:
		// checkAidlGeneratedModules: a cc / java / rust module depending on it fails; a `-java-source` rule reached through a filegroup (the
		// framework's AIDL sources) carries no AidlVersionInfo past the filegroup, so Soong builds it
		g.problem("%s: version %s of %s is unfrozen but the interface is `frozen: true` (Soong rejects its users)", ref, ver, name)
		return nil
	}
	srcs := g.aidlSources(i, ver)
	if len(srcs) == 0 {
		g.problem("%s: no .aidl sources for version %q of %s", ref, ver, name)
		return nil
	}
	spec := &aidlSpec{lang: lang, name: name, dir: i.mod.Dir, version: ver, unstable: i.unstable, srcs: srcs,
		trace: be.boolean("gen_trace", true), log: be.boolean("gen_log", false), stability: p.str("stability"), minSdk: "current"}
	if lang == "rust" {
		// addRustLibrary's aidl_gen_rule sets neither GenTrace (b/236880829) nor GenLog; GenMockall from backend.rust.gen_mockall
		spec.trace, spec.log, spec.mockall = false, false, be.boolean("gen_mockall", false)
	}
	if v := be.str("min_sdk_version"); v != "" {
		spec.minSdk = v
	} else if v := p.str("min_sdk_version"); v != "" {
		spec.minSdk = v
	}
	if lang == "java" {
		// genTrace: on by default only with platform_apis; no gen_log; gen_rpc; platform_apis -> --min_sdk_version platform_apis
		platform := be.boolean("platform_apis", false)
		spec.trace, spec.log, spec.rpc = be.boolean("gen_trace", platform), false, be.boolean("gen_rpc", false)
		// addJavaLibrary: without a min_sdk_version, the API level of the sdk_version (system_current by default: current)
		if be.str("min_sdk_version") == "" && p.str("min_sdk_version") == "" {
			if l := parseSdk(be.str("sdk_version")).level; l != "" {
				spec.minSdk = l
			}
		}
		if platform {
			spec.minSdk = "platform_apis"
		}
	}
	// flagsForAidlGenRule: only the unfrozen version, and only while the interface is not marked frozen
	if ver == next && !i.frozen {
		spec.warnings = append([]string{"-Weverything", "-Wno-missing-permission-annotation"}, p.strs("flags")...)
	}
	// useUnfrozen is false: previous frozen version = this - 1, only for a version without a .hash of its own (checked per source)
	if n, err := strconv.Atoi(ver); err == nil && n > 1 && !i.unstable {
		spec.prevVer = strconv.Itoa(n - 1)
	}
	for _, f := range srcs {
		spec.roots = appendMissing(spec.roots, []string{f.root})
	}
	// getDeps: the direct imports' preprocessed types (-p), the interface's include_dirs and its headers' dirs (-I)
	for _, r := range g.resolveImports(i, ver) {
		if pre := g.aidlPreprocessed(r.iface, r.version); pre != "" {
			spec.preprocessed = append(spec.preprocessed, pre)
		}
	}
	spec.includes = appendMissing(spec.includes, p.strs("include_dirs"))
	if c := i.mod.aidlCrate; c != nil {
		spec.includes = appendMissing(spec.includes, c.includes)
		if c.preprocessed != "" {
			spec.preprocessed = append(spec.preprocessed, c.preprocessed)
		}
		spec.javaTypes = c.javaTypes
		spec.rustServed = true
	}
	for _, h := range p.strs("headers") {
		spec.includes = appendMissing(spec.includes, g.aidlLibraryDirs(h, side, ref, map[string]bool{}))
	}

	for _, r := range g.resolveImports(i, ver) {
		// the imports' backends of the same lang; for java-source too, the libraries -java takes
		spec.imports = append(spec.imports, r.iface.versionedName(r.version)+"-"+lang)
		spec.mangled = append(spec.mangled, fixRustName(r.iface.mod.Name))
	}
	if source {
		// aidlGenRule for java: one .java per source below its root (genrule.go aidlJavaRule)
		return &Module{Type: "genrule", Name: ref, Dir: i.mod.Dir, Props: newObj(), aidl: spec}
	}
	if lang == "java" {
		// addJavaLibrary: an installable java_library compiling the `-java-source` rule's output; sdk_version system_current unless platform_apis;
		// static_libs = additional_libs, then the imports' java libraries (aidl_interface.go addLanguageLibraries)
		props := newObj()
		props.set("srcs", strVals([]string{":" + strings.TrimSuffix(strings.TrimPrefix(ref, ":"), "-java") + "-java-source"}))
		sdk := be.str("sdk_version")
		if sdk == "" && !be.boolean("platform_apis", false) {
			sdk = "system_current"
		}
		if sdk != "" {
			props.set("sdk_version", sdk)
		}
		if v := be.str("min_sdk_version"); v != "" {
			props.set("min_sdk_version", v)
		} else if v := p.str("min_sdk_version"); v != "" {
			props.set("min_sdk_version", v)
		}
		props.set("static_libs", strVals(slices.Concat(be.strs("additional_libs"), spec.imports)))
		props.set("installable", true)
		return &Module{Type: "java_library", Name: ref, Dir: i.mod.Dir, Props: props}
	}
	if lang == "rust" {
		// addRustLibrary: crate named after the interface, root after its version; rustlibs = additional_rustlibs, the rest is the provider's (rustAidl.deps)
		props := newObj()
		props.set("crate_name", fixRustName(name))
		props.set("source_stem", fixRustName(i.versionedName(ver)))
		rustlibs := be.strs("additional_rustlibs")
		if c := i.mod.aidlCrate; c != nil {
			rustlibs = append(rustlibs, c.rustlibs...)
		}
		props.set("rustlibs", strVals(rustlibs))
		return &Module{Type: aidlRustType, Name: ref, Dir: i.mod.Dir, Props: props, aidl: spec}
	}

	// addCppLibrary: dependencies and exports
	export, shared := []string{"libbinder_ndk"}, be.strs("additional_shared_libraries")
	if lang == "cpp" {
		export = []string{"libbinder", "libutils"}
		if spec.trace {
			shared = append(shared, "libcutils")
		}
	}
	imports := spec.imports
	cflags := be.strs("cflags")
	if lang == "ndk" {
		cflags = append(cflags, "-DBINDER_STABILITY_SUPPORT")
	}
	props := newObj()
	props.set("shared_libs", strVals(slices.Concat(export, shared, imports)))
	props.set("export_shared_lib_headers", strVals(slices.Concat(export, imports)))
	props.set("cflags", strVals(cflags))
	a := newObj()
	a.set("export_aidl_headers", true)
	props.set("aidl", a)
	return &Module{Type: "cc_library", Name: ref, Dir: i.mod.Dir, Props: props, aidl: spec}
}

// fixRustName: aidl_utils.go's function of the same name, `-` and `.` -> `_`.
func fixRustName(name string) string { return strings.NewReplacer("-", "_", ".", "_").Replace(name) }

// aidlLibraryDirs: the import dirs of an aidl_library (build/soong/aidl_library): <module dir>/strip_import_prefix, then those of its aidl_library
// deps. Its srcs / hdrs files are found through these dirs and the depfile, so they are not listed as inputs.
func (g *Graph) aidlLibraryDirs(ref string, side Side, from string, seen map[string]bool) []string {
	m := g.lookup(ref, side)
	switch {
	case m == nil:
		g.problem("%s: aidl_library %s not found", from, ref)
		g.notFound(ref)
		return nil
	case m.Type != "aidl_library":
		g.problem("%s: %s is a %s, not an aidl_library", from, ref, m.Type)
		return nil
	case seen[m.Name]:
		return nil
	}
	seen[m.Name] = true
	p := g.flatten(m, side, Gen)
	out := []string{filepath.Join(m.Dir, p.str("strip_import_prefix"))}
	for _, d := range p.strs("deps") {
		// deps may also name aidl files: only libraries contribute dirs
		if filepath.Ext(d) != ".aidl" {
			out = appendMissing(out, g.aidlLibraryDirs(d, side, from, seen))
		}
	}
	return out
}

// readHash:`tail -1 <file>` of a .hash file, ok=false when there is none.
func (g *Graph) readHash(rel string) (string, bool) {
	b, err := os.ReadFile(g.abs(rel))
	if err != nil {
		return "", false
	}
	lines := strings.Split(strings.TrimRight(string(b), "\n"), "\n")
	return strings.TrimSpace(lines[len(lines)-1]), true
}

// flags: the aidl command line (besides --lang, -h, -o) for source f and the files it also depends on (.hash). Order of
// generateBuildActionsForSingleAidl: warnings, --structured --version --hash, -t, --stability, --min_sdk_version, previous version, --log; then
// aidl's own import roots as aidl_gen_rule.go: -I include dirs and imported sources, -N own roots. path renders a tree path on the command
// line: g.abs for edges run from the build dir, as is for those run from the tree root.
func (s *aidlSpec) flags(g *Graph, f srcFile, path func(string) string) (flags, implicit []string) {
	flags = append(flags, s.warnings...)
	hasHash := false
	if !s.unstable {
		version := s.version
		if version == "" {
			version = "1" // an interface without versions: aidl_gen_rule.go's default
		}
		hash := "notfrozen"
		if h, ok := g.readHash(filepath.Join(f.root, ".hash")); ok {
			hash, hasHash = h, true
			implicit = append(implicit, g.abs(filepath.Join(f.root, ".hash")))
		}
		flags = append(flags, "--structured", "--version", version, "--hash", hash)
	}
	if s.rpc {
		flags = append(flags, "--rpc")
	}
	if s.trace {
		flags = append(flags, "-t")
	}
	if s.mockall {
		flags = append(flags, "--mockall")
	}
	if s.stability != "" {
		flags = append(flags, "--stability", s.stability)
	}
	flags = append(flags, "--min_sdk_version", s.minSdk)
	for _, p := range s.preprocessed {
		flags = append(flags, "-p"+p)
		implicit = append(implicit, p)
	}
	if s.prevVer != "" && !hasHash {
		prev := filepath.Join(s.dir, "aidl_api", s.name, s.prevVer)
		if h, ok := g.readHash(filepath.Join(prev, ".hash")); ok {
			flags = append(flags, "--previous_api_dir="+path(prev), "--previous_hash", h)
			implicit = append(implicit, g.abs(filepath.Join(prev, ".hash")))
		} else if !s.prevReported {
			s.prevReported = true
			g.problem("%s: no .hash of the previous version in %s", s.name, prev)
		}
	}
	if s.log {
		flags = append(flags, "--log")
	}
	for _, j := range s.javaTypes {
		flags = append(flags, "--rust_java_type="+j)
	}
	if s.rustServed {
		flags = append(flags, "--rust_unimplemented_hook", "--rust_java_nullability")
	}
	for _, r := range s.includes {
		flags = append(flags, "-I"+path(r))
	}
	for _, r := range s.roots {
		flags = append(flags, "-N"+path(r))
	}
	return flags, implicit
}

// aidlPreprocessed is aidl_interface.go buildPreprocessed: the types of one interface version, `aidl --preprocess <out> [--structured]
// [--stability s] -p<imports' preprocessed> -I<own source roots, include_dirs, headers' dirs, include_dirs> <srcs>`, into
// <out>/aidl/<iface>/<version>/preprocessed.aidl; an importer's aidl runs read it with -p (getDeps). "" when the version has no sources.
func (g *Graph) aidlPreprocessed(i *aidlIface, ver string) string {
	k := i.side.String() + "/" + i.mod.Name + "/" + ver
	if out, ok := g.aidlPre[k]; ok {
		return out
	}
	if g.aidlPre == nil {
		g.aidlPre = map[string]string{}
	}
	g.aidlPre[k] = ""
	srcs := g.aidlSources(i, ver)
	if len(srcs) == 0 {
		return ""
	}
	dirVer := ver
	if dirVer == "" {
		dirVer = "unversioned"
	}
	out := filepath.Join(g.out, "aidl", i.side.String(), i.mod.Name, dirVer, "preprocessed.aidl")
	var flags, implicit []string
	if !i.unstable {
		flags = append(flags, "--structured")
	}
	if s := i.props.str("stability"); s != "" {
		flags = append(flags, "--stability", s)
	}
	for _, r := range g.resolveImports(i, ver) {
		if p := g.aidlPreprocessed(r.iface, r.version); p != "" {
			flags = append(flags, "-p"+p)
			implicit = append(implicit, p)
		}
	}
	var imports []string
	for _, f := range srcs {
		imports = appendMissing(imports, []string{f.root})
	}
	// getDeps for the interface itself: its include_dirs, then its headers' (aidl_library) dirs; buildPreprocessed adds include_dirs again
	imports = append(imports, i.props.strs("include_dirs")...)
	for _, h := range i.props.strs("headers") {
		imports = append(imports, g.aidlLibraryDirs(h, i.side, i.mod.Name, map[string]bool{})...)
	}
	imports = append(imports, i.props.strs("include_dirs")...)
	for _, d := range imports {
		flags = append(flags, "-I"+g.abs(d))
	}
	tool := filepath.Join(g.out, "missing-tool", "aidl")
	if t := g.hostTool("aidl", &Node{mod: i.mod}); t != nil {
		tool = g.nodeOut(t)
	}
	var ins []string
	for _, f := range srcs {
		ins = append(ins, g.abs(f.path))
	}
	fmt.Fprintf(&g.aidlPreB, "build %s: aidl_preprocess %s | %s\n  aidl = %s\n  flags = %s\n", esc(out), join(ins),
		join(append([]string{tool}, implicit...)), esc(tool), shellQuote(flags))
	g.aidlPre[k] = out
	return out
}

// aidlPreNinja: the preprocess rule and edges (written before the cc edges that read them).
func (g *Graph) aidlPreNinja() string {
	if g.aidlPreB.Len() == 0 {
		return ""
	}
	return "rule aidl_preprocess\n  command = $aidl --preprocess $out $flags $in\n  description = aidl preprocess $out\n" + g.aidlPreB.String()
}

// aidlJavaRule: the genrule of a `-java-source` module (aidl_gen_rule.go aidlJavaRule): `aidl --lang=java <flags> -o <genDir> <src>` per source,
// writing <genDir>/<path below the source's root>.java. Soong's depfile (-d) is dropped as for hidl-gen: the genrule rule has none.
func (g *Graph) aidlJavaRule(n *Node) *genRule {
	s := n.mod.aidl
	r, fail := g.newGenRule(n)
	genDir := g.genDir(n, "")
	r.hasTools = true
	r.labels = []genLabel{g.genTool(n, r, "aidl", fail)}
	var cmds []string
	for _, f := range s.srcs {
		// tree-relative from the genrule's cwd (the tree root), as Soong's: aidl writes the source path into the generated code
		flags, implicit := s.flags(g, f, func(p string) string { return p })
		r.inputs = append(r.inputs, f.path)
		r.implicit = appendMissing(r.implicit, implicit)
		cmd := []string{"$(location aidl)", "--lang=java"}
		for _, fl := range flags {
			cmd = append(cmd, shellEscape(fl))
		}
		// the source in the same form as its -N root: aidl matches a source to its import root by path prefix
		cmds = append(cmds, strings.Join(append(cmd, "-o", "$(genDir)", shellEscape(f.path)), " "))
		r.outs = append(r.outs, filepath.Join(genDir, strings.TrimSuffix(strings.TrimPrefix(f.path, f.root+"/"), ".aidl")+".java"))
	}
	r.cmd = strings.Join(cmds, " && ")
	r.consumerDeps(genDir)
	return r
}
