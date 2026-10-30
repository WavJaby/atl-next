// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"sort"
	"strconv"
	"strings"
)

// aidl_interface backends: Soong creates `<iface>[-V<n>]-<lang>` cc libraries in the interface's load hook, no Android.bp defines them.
// Spec: system/tools/aidl/build/{aidl_interface.go, aidl_interface_backends.go, aidl_gen_rule.go}. lookup falls back to aidlBackend, which
// synthesizes the Module; its sources go through the extension table (gensrc.go genAidl) like any cc module's, with per-interface flags.
//   - Names (addLibrary): the next version (latest frozen + 1, "1" with no versions) and every frozen version are `<iface>-V<v>-<lang>`; an
//     unstable interface has no versions and one unversioned `<iface>-<lang>`. A stable interface has no unversioned backend.
//   - Sources: a frozen version = aidl_api/<iface>/<v>/**/*.aidl under the module dir (root = that dir); the next version = the module's `srcs` (root
//     local_include_dir).
//   - Libraries (addCppLibrary): shared libbinder + libutils (cpp; libcutils too with gen_trace, default on), libbinder_ndk (ndk), backend
//     additional_shared_libraries, then the imports' backends of the same lang (addLanguageLibraries); all but the additional ones are exported
//     (export_shared_lib_headers); generated headers exported. Only the platform variant is built: ndk gets -DBINDER_STABILITY_SUPPORT (target.platform).
//   - Imports (getImportWithVersion): `-V<n>` as written; an unstable import is bare; the importing interface's next version takes the import's
//     next version; a frozen version takes the import's latest. Version imports come from versions_with_info, else the module's imports.
//   - aidl flags (generateBuildActionsForSingleAidl, flagsForAidlGenRule): see aidlSpec.flags. Import types are found with -I of every imported
//     version's source roots (transitively) instead of Soong's `aidl --preprocess` + -p chain: same types, no extra edges; revisit if a clash of
//     two versions of one package ever shows up.
//   - RELEASE_AIDL_USE_UNFROZEN is false in the pinned release chain (flag_values: ap2a, ap3a false, no later config sets it), so an unfrozen
//     version of a frozen interface gets --previous_api_dir / --previous_hash.
//   - `headers` (aidl_library modules) are -I dirs after include_dirs (aidl_api.go getDeps).
//   - Not built, one problem each: java, rust, cpp-analyzer, ndk_platform backends; a backend with enabled: false; a
//     host backend of an interface without host_supported; the next version of a `frozen: true` interface (Soong rejects its users); a
//     version the interface does not have. Soong's -Wall -Werror cflags, ldflags, tidy, sdk_version/stl are dropped (this build uses -w -Wno-error).

var (
	aidlLangSuffixes = []string{"-cpp-analyzer", "-ndk_platform", "-cpp", "-ndk", "-java", "-rust"}
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
	closure  map[string][]aidlRef
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
		i = &aidlIface{mod: m, side: side, props: p, withInfo: map[string][]string{}, srcs: map[string][]srcFile{}, imports: map[string][]aidlRef{}, closure: map[string][]aidlRef{},
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

// aidlOwned: ref names a backend of an aidl_interface the tree has, so lookup said why it failed and the name is no missing module.
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

// importClosure: every interface version `version` of i imports, transitively (each once).
func (g *Graph) importClosure(i *aidlIface, version string) []aidlRef {
	if c, ok := i.closure[version]; ok {
		return c
	}
	seen := map[string]bool{}
	var out []aidlRef
	var walk func(i *aidlIface, version string)
	walk = func(i *aidlIface, version string) {
		for _, r := range g.resolveImports(i, version) {
			if k := r.iface.mod.Name + "-V" + r.version; !seen[k] {
				seen[k] = true
				out = append(out, r)
				walk(r.iface, r.version)
			}
		}
	}
	walk(i, version)
	i.closure[version] = out
	return out
}

// aidlSources: aidlInterface.srcsForVersion + getPaths. The next version's srcs are the module's (paths, globs, filegroups; root =
// local_include_dir), a frozen version's every .aidl below aidl_api/<iface>/<version>.
func (g *Graph) aidlSources(i *aidlIface, version string) []srcFile {
	if s, ok := i.srcs[version]; ok {
		return s
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
	lang       string // cpp | ndk
	name       string // interface name (aidl_api/<name>)
	dir        string // its module dir
	version    string // "" = unversioned
	unstable   bool
	warnings   []string // -W flags of the unfrozen version
	trace, log bool
	stability  string
	minSdk     string
	roots      []string // own source roots (-N)
	includes   []string // tree-relative import roots and include_dirs (-I)
	prevVer    string   // previous frozen version when this one is unfrozen, "" = none
	srcs       []srcFile
	// the missing previous .hash is one problem, not one per source
	prevReported bool
}

// aidlBackend: lookup's fallback for `<iface>[-V<n>]-<lang>`. nil without a problem: ref names no backend of an aidl_interface (a plain missing
// module); nil with one: the backend does not exist or is not supported.
func (g *Graph) aidlBackend(ref string, side Side) *Module {
	k := ref + "/" + side.String()
	if m, ok := g.aidlMods[k]; ok {
		return m
	}
	m := g.synthAidl(ref, side)
	if g.aidlMods == nil {
		g.aidlMods = map[string]*Module{}
	}
	g.aidlMods[k] = m
	return m
}

func strVals(list []string) []Val {
	out := make([]Val, len(list))
	for i, s := range list {
		out[i] = s
	}
	return out
}

func (g *Graph) synthAidl(ref string, side Side) *Module {
	name, ver, lang, ok := parseAidlRef(ref)
	if !ok {
		return nil
	}
	i := g.aidlIfaceOf(name, side)
	if i == nil {
		return nil
	}
	if lang != "cpp" && lang != "ndk" {
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
	case ver == next && i.frozen:
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
	if v := be.str("min_sdk_version"); v != "" {
		spec.minSdk = v
	} else if v := p.str("min_sdk_version"); v != "" {
		spec.minSdk = v
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
	for _, r := range g.importClosure(i, ver) {
		for _, f := range g.aidlSources(r.iface, r.version) {
			spec.includes = appendMissing(spec.includes, []string{f.root})
		}
	}
	spec.includes = appendMissing(spec.includes, p.strs("include_dirs"))
	for _, h := range p.strs("headers") {
		spec.includes = appendMissing(spec.includes, g.aidlLibraryDirs(h, side, ref, map[string]bool{}))
	}

	// addCppLibrary: dependencies and exports
	export, shared := []string{"libbinder_ndk"}, be.strs("additional_shared_libraries")
	if lang == "cpp" {
		export = []string{"libbinder", "libutils"}
		if spec.trace {
			shared = append(shared, "libcutils")
		}
	}
	var imports []string
	for _, r := range g.resolveImports(i, ver) {
		imports = append(imports, r.iface.versionedName(r.version)+"-"+lang)
	}
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
// aidl's own import roots as aidl_gen_rule.go: -I include dirs and imported sources, -N own roots.
func (s *aidlSpec) flags(g *Graph, f srcFile) (flags, implicit []string) {
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
	if s.trace {
		flags = append(flags, "-t")
	}
	if s.stability != "" {
		flags = append(flags, "--stability", s.stability)
	}
	flags = append(flags, "--min_sdk_version", s.minSdk)
	if s.prevVer != "" && !hasHash {
		prev := filepath.Join(s.dir, "aidl_api", s.name, s.prevVer)
		if h, ok := g.readHash(filepath.Join(prev, ".hash")); ok {
			flags = append(flags, "--previous_api_dir="+g.abs(prev), "--previous_hash", h)
			implicit = append(implicit, g.abs(filepath.Join(prev, ".hash")))
		} else if !s.prevReported {
			s.prevReported = true
			g.problem("%s: no .hash of the previous version in %s", s.name, prev)
		}
	}
	if s.log {
		flags = append(flags, "--log")
	}
	for _, r := range s.includes {
		flags = append(flags, "-I"+g.abs(r))
	}
	for _, r := range s.roots {
		flags = append(flags, "-N"+g.abs(r))
	}
	return flags, implicit
}
