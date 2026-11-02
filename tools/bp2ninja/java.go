// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
)

// Java modules, following build/soong/java (spec): java.go, base.go (compile, collectDeps, collectJavacFlags), builder.go (rules),
// dex.go (d8 flags), sdk.go (decodeSdkDep), system_modules.go, java_resources.go. One jNode per (module, side) = Soong's
// android_common (Device) or linux_glibc_common (Host) variant.
//   - Turbine is not run: Soong's TURBINE_ENABLED=false path, which every host module takes anyway. A module's header jar is its
//     implementation jar without META-INF/services (convertImplementationJarToHeaderJar); dependents compile against it.
//   - Header jars are one combined jar per module, where Soong lists the module's and its static libraries' header jars: same
//     classes, one file.
//   - d8 runs in debug mode unless dxflags ask for --release: D8's own default, and what Soong's eng builds pass explicitly.
//   - Every installable / compile_dex device jar is stored uncompressed and aligned (Soong: boot jars, APEX jars and preopted jars are,
//     which is what this build's runtime jars are); `uncompress_dex: false` opts out.
//   - A genrule named in libs / static_libs whose command cannot run is dropped from the jars with a problem, not failed downstream
//     (notices-for-stubs-jar: NOTICE files for API stubs jars, needs gen_notice).
//   - Generated sources: javagen.go (.aidl, .logtags, .proto), kotlin.go (.kt), jarjar.go (jarjar_rules, inherited rename rules), aapt.go
//     (an android_app's R sources); javac sharding needs Turbine, so javac runs once.
//   - Not supported, reported as problems: errorprone, jacoco, lint, hiddenapi, dexpreopt, R8 (optimize), services, kapt.

// JavaTools are the host programs and directories the Java rules run, given by the caller (all absolute). Soong's equivalents:
// ANDROID_JAVA_HOME, the soong_zip / merge_zips / zip2zip / zipalign host tools, metalava.jar. aconfig and the release config are the Graph's (aconfig.go).
type JavaTools struct {
	JDK, BuildTools, Metalava string
}

type jKey struct {
	name string
	side Side
}

// jNode is one Java-side module variant and what Soong's providers hand its dependents (JavaInfo, SystemModulesProvider, …).
type jNode struct {
	jKey
	mod   *Module
	typ   string // base module type
	props *Obj
	dir   string // build directory of this variant

	header  string   // HeaderJars (one jar)
	impl    []string // TransitiveStaticLibsImplementationJars
	res     []string // TransitiveStaticLibsResourceJars
	implRes string   // ImplementationAndResourcesJars
	dex     string   // dex jar: device and (installable or compile_dex)
	srcjar  string   // droidstubs / java_aconfig_library sources
	// java_system_modules: --system directory, the files that stand for it, header jars of its libs
	sysDir  string
	sysDeps []string
	sysJars []string
	// java_plugin
	processor string
	// aidl.export_include_dirs (tree-relative): dependents' aidl -I (JavaInfo.AidlIncludeDirs)
	aidlExport []string
	// jarjar rename rules it hands dependents, original class -> new name ("" = renamed by a later jarjar_prefix); jarjar.go
	renames map[string]string
	// exported_plugins
	expPluginJars, expProcessors []string
	// droiddoc_exported_dir
	expDir     string
	expDirDeps []string
	// java_binary_host: the runnable wrapper and its installed jar
	tool, toolJar string
}

// javaGraph holds the Java side of a Graph.
type javaGraph struct {
	tools JavaTools
	cfg   JavaConfig
	nodes map[jKey]*jNode
	order []*jNode
	b     strings.Builder
	outs  []string
	// java_sdk_library components synthesised so far (stubs.go), by name + side
	synth map[string]*Module
	// metalavaTool's install edges are written
	metalavaInstalled bool
	// the SDK's preprocessed aidl files by name (sdkaidl.go), "" = not buildable
	sdkAidl map[string]string
}

// javaTypes: module types built by java nodes (after soong_config_module_type unwrapping).
var javaTypes = map[string]bool{
	"java_library": true, "java_library_static": true, "java_library_host": true, "java_plugin": true, "java_binary_host": true,
	"java_import": true, "java_import_host": true, "java_system_modules": true, "droidstubs": true, "java_aconfig_library": true,
	"droiddoc_exported_dir": true, "java_sdk_library": true, "java_api_library": true, syspropType: true,
}

func (g *Graph) isJava(t string) bool { return javaTypes[g.baseType(t)] }

// lookupJava finds a module by reference for the Java side. Prebuilts and imports lose to a source module of the same name but,
// unlike for cc, stand when they are the only definition (java_import of a prebuilt jar is a real dependency). Components a
// java_sdk_library creates (sdklib.go) are found under their own names.
func (g *Graph) lookupJava(ref string, side Side) *Module {
	name, _ := splitTag(ref)
	if m := g.lookupWith(name, side, isJavaPrebuilt); m != nil {
		return m
	}
	if m := g.lookupWith(name, side, func(t string) bool { return t == "fdo_profile" || strings.HasPrefix(t, "prebuilt_") }); m != nil {
		return m
	}
	return g.sdkLibComponent(strings.TrimPrefix(name, ":"), side)
}

// initJava reads what Java nodes need: the tools and Soong's Java flags; the release config and aconfig must be set up already.
func (g *Graph) initJava(t JavaTools) error {
	if t.BuildTools == "" || g.acfg == nil {
		return fmt.Errorf("java: -jdk, -build-tools, -aconfig and -release are required together")
	}
	cfg, err := readJavaConfig(g.tree, t.JDK)
	if err != nil {
		return err
	}
	g.java = &javaGraph{tools: t, cfg: cfg, nodes: map[jKey]*jNode{}}
	return nil
}

// ninja is the Java part of the ninja file: rules, edges, and its default targets.
func (j *javaGraph) ninja(g *Graph) string {
	s := "\n" + j.header(g) + j.b.String()
	if len(j.outs) > 0 {
		s += "\ndefault " + join(j.outs) + "\n"
	}
	return s
}

func isJavaPrebuilt(t string) bool {
	return strings.Contains(t, "prebuilt") || strings.HasSuffix(t, "_import") || t == "fdo_profile"
}

// splitTag: `:name{.tag}` -> `:name`, `.tag` (Soong's output tag syntax).
func splitTag(ref string) (name, tag string) {
	if i := strings.IndexByte(ref, '{'); i >= 0 && strings.HasSuffix(ref, "}") {
		return ref[:i], ref[i+1 : len(ref)-1]
	}
	return ref, ""
}

// jnode builds (once) the Java node a reference names; nil with a problem recorded when it cannot.
func (g *Graph) jnode(ref string, side Side, from string) *jNode {
	if g.omit[strings.TrimPrefix(ref, ":")] {
		return nil
	}
	m := g.lookupJava(ref, side)
	if m == nil {
		g.problem("%s: %s module %s not found", from, side, ref)
		g.notFound(ref)
		return nil
	}
	t := g.baseType(m.Type)
	if !javaTypes[t] && !isGenType(t) {
		g.problem("%s: %s is a %s (unsupported for java)", from, m.Name, m.Type)
		return nil
	}
	j := g.java
	k := jKey{m.Name, side}
	if n, ok := j.nodes[k]; ok {
		return n
	}
	if isGenType(t) {
		// a genrule's outputs as jars in libs / static_libs (base.go collectDeps: SourceFilesInfoProvider)
		gn := g.genNode(ref, side, from)
		if gn == nil {
			return nil
		}
		if gn.rule.broken != "" {
			// a jar no generator can build is left out of the classpath / static merge, not failed downstream
			g.problem("%s: genrule %s cannot run (%s); dropped from %s's jars", from, m.Name, gn.rule.broken, from)
			return nil
		}
		n := &jNode{jKey: k, mod: m, typ: t, dir: filepath.Join(g.out, side.String(), m.Name)}
		j.nodes[k] = n
		n.impl = gn.rule.outs
		n.header = gn.rule.outs[0]
		if len(gn.rule.outs) > 1 {
			n.header = filepath.Join(n.dir, "combined", m.Name+".jar")
			j.combine(n.header, gn.rule.outs, "")
		}
		n.implRes = n.header
		return n
	}
	n := &jNode{jKey: k, mod: m, typ: t, dir: filepath.Join(g.out, side.String(), m.Name)}
	j.nodes[k] = n
	n.props = g.flattenJava(m, side)
	switch t {
	case "java_system_modules":
		g.sysModules(n)
	case "java_import", "java_import_host":
		g.javaImport(n)
	case "droidstubs":
		g.droidstubs(n)
	case "java_api_library":
		g.apiLibrary(n)
	case "droiddoc_exported_dir":
		g.exportedDir(n)
	case "java_aconfig_library":
		g.aconfigJavaLib(n)
	case syspropType:
		g.syspropJava(n)
	case "java_sdk_library":
		// the library itself is its .impl; its stubs are separate modules (sdklib.go)
		g.javaLib(n)
	default:
		g.javaLib(n)
	}
	j.order = append(j.order, n)
	return n
}

// flattenJava applies a side's target blocks onto the defaults-merged properties. Java variants are arch-common: no arch,
// multilib or arch-suffixed target blocks (Soong's android_common / linux_glibc_common).
func (g *Graph) flattenJava(m *Module, side Side) *Obj {
	p := g.resolveDefaults(m, side, map[string]bool{})
	base := newObj()
	for _, k := range p.keys {
		switch k {
		case "target", "arch", "multilib", "defaults":
		default:
			base.set(k, p.m[k])
		}
	}
	if t := p.obj("target"); t != nil {
		for _, b := range osBlocks(side, g.cfg.PointerBits) {
			if o := t.obj(b); o != nil {
				base = merge(base, o)
			}
		}
	}
	return base
}

// javaVersion is Soong's javaVersion: the -source/-target value and whether it uses java modules (>= 9).
type javaVersion int

func (v javaVersion) String() string {
	switch {
	case v <= 8:
		return "1.8"
	case v == 9:
		return "1.9"
	}
	return strconv.Itoa(int(v))
}

func (v javaVersion) modules() bool { return v >= 9 }

// parseJavaVersion is normalizeJavaVersion: 1.6/1.7 are bumped to 1.8; 0 = unsupported.
func parseJavaVersion(s string) javaVersion {
	switch s {
	case "1.6", "6", "1.7", "7", "1.8", "8":
		return 8
	case "1.9", "9":
		return 9
	case "11", "17", "21":
		v, _ := strconv.Atoi(s)
		return javaVersion(v)
	}
	return 0
}

// sdkSpec is Soong's SdkSpec reduced to what the build uses: kind and API level ("" level = current / future).
type sdkSpec struct {
	kind  string // private, none, core_platform, public, core, system, test, test_frameworks_core, module, system_server
	level string // number, or "" for current
}

// parseSdk is SdkSpecFromWithConfig.
func parseSdk(s string) sdkSpec {
	switch s {
	case "":
		return sdkSpec{kind: "private"}
	case "none", "core_platform":
		return sdkSpec{kind: s}
	}
	kind, ver := "public", s
	if i := strings.LastIndexByte(s, '_'); i > 0 {
		kind, ver = s[:i], s[i+1:]
	}
	if ver == "current" {
		ver = ""
	}
	return sdkSpec{kind: kind, level: ver}
}

// stable is SdkSpec.Stable: an unspecified sdk_version (private) is not.
func (s sdkSpec) stable() bool {
	switch s.kind {
	case "none", "core", "public", "system", "module", "system_server":
		return true
	}
	return false
}

// defaultJavaVersion is getJavaVersion without java_version: device by API level (sdk.go defaultJavaLanguageVersion), host by
// RELEASE_TARGET_JAVA_21.
func (g *Graph) defaultJavaVersion(side Side, s sdkSpec) javaVersion {
	if side == Device && s.level != "" {
		if lvl, err := strconv.Atoi(s.level); err == nil {
			switch {
			case lvl <= 29:
				return 8
			case lvl <= 31:
				return 9
			case lvl <= 33:
				return 11
			}
		}
	}
	if g.release.Bool("RELEASE_TARGET_JAVA_21") {
		return 21
	}
	return 17
}

// sdkDep is decodeSdkDep's result for module-based SDKs: what the module compiles against besides its own libs.
type sdkDep struct {
	boot         []string // bootclasspath modules
	java9        []string // java9Classpath modules
	classpath    []string // sdkLib modules
	sysModules   string
	prebuiltJars []string // useFiles: tree-relative jars (numbered SDKs)
	// aidl: the SDK's preprocessed aidl file (-p) -- tree-relative for a numbered SDK; for a module SDK the framework.aidl Soong's sdk
	// singleton extracts from the stubs (sdkFrameworkAidl), which is not built here
	aidl, aidlGen string
}

// decodeSdk is decodeSdkDep (sdk.go) for this build: SDK modules from source (no unbundled prebuilts), numbered levels from prebuilts/sdk.
func (g *Graph) decodeSdk(n *jNode, s sdkSpec) (sdkDep, error) {
	p := n.props
	if s.level != "" {
		// prebuilts/sdk/<level>/<SdkKind.String()>: the kinds named with `_` in sdk_version are `-` there
		kind := s.kind
		switch kind {
		case "module":
			kind = "module-lib"
		case "system_server":
			kind = "system-server"
		}
		dir := filepath.Join("prebuilts/sdk", s.level, kind)
		// "There's no aidl for other SDKs yet": always the public one
		d := sdkDep{prebuiltJars: []string{filepath.Join(dir, "android.jar"), "prebuilts/sdk/tools/core-lambda-stubs.jar"},
			aidl: filepath.Join("prebuilts/sdk", s.level, "public", "framework.aidl")}
		if g.defaultJavaVersion(Device, s).modules() {
			// prebuilt_apis' sdk_<kind>_<level>_system_modules (prebuiltapis.go), kind by systemModuleKind
			d.sysModules = "sdk_" + prebuiltSystemModuleKind(s) + "_" + s.level + "_system_modules"
		}
		return d, nil
	}
	toModule := func(lib, sysKind string) sdkDep {
		aidl := "framework.aidl"
		if s.kind == "module" {
			aidl = "framework_non_updatable.aidl"
		}
		return sdkDep{boot: []string{lib, "core-lambda-stubs"}, java9: []string{lib}, sysModules: "core-" + sysKind + "-stubs-system-modules", aidlGen: aidl}
	}
	// legacy_core_platform_api_usage.go corePlatformSystemModules / corePlatformBootclasspathLibraries: the legacy core platform API for the
	// modules on its list, the stable one for every other
	corePlatform := sdkDep{sysModules: "stable-core-platform-api-stubs-system-modules", boot: []string{"stable.core.platform.api.stubs", "core-lambda-stubs"}}
	if g.java != nil && g.java.cfg.LegacyCorePlatform[n.name] {
		corePlatform = sdkDep{sysModules: "legacy-core-platform-api-stubs-system-modules", boot: []string{"legacy.core.platform.api.stubs", "core-lambda-stubs"}}
	}
	switch s.kind {
	case "private":
		corePlatform.classpath = []string{"ext", "framework"}
		return corePlatform, nil
	case "none":
		sm := p.str("system_modules")
		switch sm {
		case "":
			return sdkDep{}, fmt.Errorf(`system_modules is required to be set to a non-empty value when sdk_version is "none"`)
		case "none":
			return sdkDep{}, nil
		}
		return sdkDep{sysModules: sm, boot: []string{sm}}, nil
	case "core_platform":
		return corePlatform, nil
	case "public":
		return toModule("android_stubs_current", "public"), nil
	case "system":
		return toModule("android_system_stubs_current", "public"), nil
	case "test":
		return toModule("android_test_stubs_current", "public"), nil
	// system modules kind (systemModuleKind): SdkKind.String, system-server using module-lib's
	case "module":
		return toModule("android_module_lib_stubs_current", "module-lib"), nil
	case "system_server":
		return toModule("android_system_server_stubs_current", "module-lib"), nil
	case "core":
		return sdkDep{boot: []string{"core.current.stubs", "core-lambda-stubs"}, sysModules: "core-public-stubs-system-modules"}, nil
	}
	return sdkDep{}, fmt.Errorf("sdk_version %q is unsupported", p.str("sdk_version"))
}

// javaDeps is collectDeps' result.
type javaDeps struct {
	boot, classpath, java9, dexClasspath []string
	processorPath, processors            []string
	sys                                  *jNode
	staticImpl, staticRes                []string
	srcJars                              []string
	// aidl: libs' and static_libs' exported include dirs (tree-relative), the SDK's preprocessed file (absolute; "" = none), and the
	// generated one a module SDK would need instead (not built)
	aidlIncludes            []string
	aidlPreprocess, aidlGen string
}

// collectJavaDeps resolves n's sdk, libs, static_libs and plugins in Soong's dependency order (deps(): sdk deps, libs, static_libs,
// plugins), which is the classpath order.
func (g *Graph) collectJavaDeps(n *jNode, s sdkSpec) (javaDeps, []*jNode) {
	var d javaDeps
	p := n.props
	dep := func(ref string, side Side) *jNode { return g.jnode(ref, side, n.name) }
	var statics []*jNode
	// java_import adds no SDK dependencies (Import.DepsMutator: libs and static_libs only)
	if n.side == Device && n.typ != "java_import" {
		sd, err := g.decodeSdk(n, s)
		if err != nil {
			g.problem("%s: %v", n.name, err)
		}
		for _, r := range sd.boot {
			if b := dep(r, Device); b != nil {
				if b.typ == "java_system_modules" {
					d.boot = append(d.boot, b.sysJars...)
				} else {
					d.boot = append(d.boot, b.header)
				}
			}
		}
		for _, r := range sd.java9 {
			if b := dep(r, Device); b != nil {
				d.java9 = append(d.java9, b.header)
			}
		}
		for _, r := range sd.classpath {
			if b := dep(r, Device); b != nil {
				d.classpath = append(d.classpath, b.header)
				d.dexClasspath = append(d.dexClasspath, b.header)
			}
		}
		for _, j := range sd.prebuiltJars {
			d.classpath = append(d.classpath, g.abs(j))
			d.dexClasspath = append(d.dexClasspath, g.abs(j))
		}
		if sd.sysModules != "" {
			d.sys = dep(sd.sysModules, Device)
		}
		if sd.aidl != "" {
			d.aidlPreprocess = g.abs(sd.aidl)
		}
		d.aidlGen = sd.aidlGen
	}
	addPlugins := func(jars, classes []string) {
		d.processorPath = append(d.processorPath, jars...)
		d.processors = append(d.processors, classes...)
	}
	for _, r := range p.strs("libs") {
		if l := dep(r, n.side); l != nil {
			if l.header == "" {
				g.problem("%s: libs %s provides no jar", n.name, r)
				continue
			}
			d.classpath = append(d.classpath, l.header)
			d.dexClasspath = append(d.dexClasspath, l.header)
			d.aidlIncludes = append(d.aidlIncludes, l.aidlExport...)
			addPlugins(l.expPluginJars, l.expProcessors)
		}
	}
	for _, r := range p.strs("static_libs") {
		if l := dep(r, n.side); l != nil {
			if l.header == "" {
				g.problem("%s: static_libs %s provides no jar", n.name, r)
				continue
			}
			d.classpath = append(d.classpath, l.header)
			d.staticImpl = append(d.staticImpl, l.impl...)
			d.staticRes = append(d.staticRes, l.res...)
			d.aidlIncludes = append(d.aidlIncludes, l.aidlExport...)
			addPlugins(l.expPluginJars, l.expProcessors)
			statics = append(statics, l)
		}
	}
	for _, r := range p.strs("plugins") {
		if pl := dep(r, Host); pl != nil {
			if pl.typ != "java_plugin" {
				g.problem("%s: plugins: %q is not a java_plugin module", n.name, r)
				continue
			}
			var classes []string
			if pl.processor != "" {
				classes = []string{pl.processor}
			}
			addPlugins([]string{pl.implRes}, classes)
		}
	}
	d.classpath = dedupKeep(d.classpath)
	d.boot = dedupKeep(d.boot)
	d.processors = dedupKeep(d.processors)
	return d, statics
}

func dedupKeep(in []string) []string {
	seen := map[string]bool{}
	var out []string
	for _, s := range in {
		if !seen[s] {
			seen[s] = true
			out = append(out, s)
		}
	}
	return out
}

// jSrcs: a module's sources by kind (base.go compile: srcFiles after genSources). Paths of generated inputs are absolute, others tree-relative.
type jSrcs struct {
	java, kt, srcjars, waits []string
	// sources a generator turns into java / srcjars (javagen.go), in source order per kind
	gen map[string][]srcFile
}

// javaSrcs expands srcs into sources by kind. Extensions no step consumes (package.html, …) are dropped as Soong drops them: compile filters
// srcFiles by extension (.java, .kt, .srcjar after genSources).
func (g *Graph) javaSrcs(n *jNode, srcs []string) jSrcs {
	exclude := g.excludeSet(n.mod.Dir, n.side, n.props.strs("exclude_srcs"), n.name)
	s := jSrcs{gen: map[string][]srcFile{}}
	// a generated-source input named twice (two filegroups listing one .aidl) is generated once: Soong's module source paths are unique
	// (FirstUniquePaths), and two srcjars declaring one class fail Turbine
	seenGen := map[string]bool{}
	add := func(f srcFile) {
		switch ext := filepath.Ext(f.path); ext {
		case ".java":
			s.java = append(s.java, f.path)
		case ".kt":
			s.kt = append(s.kt, f.path)
		case ".srcjar":
			s.srcjars = append(s.srcjars, f.path)
		default:
			if javaGen(ext) != nil && !seenGen[f.path] {
				seenGen[f.path] = true
				s.gen[ext] = append(s.gen[ext], f)
			}
		}
	}
	for _, src := range srcs {
		if isRef(src) {
			if js, ok := g.javaSrcRef(n, src); ok {
				for _, f := range js {
					add(srcFile{path: f})
				}
				continue
			}
		}
		files, _ := g.resolveSrc(n.mod.Dir, n.side, src, n.name)
		for _, f := range files {
			if exclude[f.path] {
				continue
			}
			if f.gen != nil {
				s.waits = append(s.waits, f.gen.rule.deps...)
			}
			add(f)
		}
	}
	// base.go: uniqueJavaFiles then uniqueKtFiles, one set across both
	s.java, s.kt, s.srcjars, s.waits = dedupKeep(s.java), dedupKeep(s.kt), dedupKeep(s.srcjars), dedupKeep(s.waits)
	return s
}

// javaSrcRef resolves a srcs reference to a Java module producing sources (droidstubs, a java_sdk_library's stubs source tag).
// ok=false: not such a module; the caller resolves it as a filegroup / genrule.
func (g *Graph) javaSrcRef(n *jNode, ref string) ([]string, bool) {
	name, tag := splitTag(ref)
	m := g.lookupJava(name, n.side)
	if m == nil {
		return nil, false
	}
	switch g.baseType(m.Type) {
	case "droidstubs":
		if tag != "" && tag != ".everything" {
			g.problem("%s: %s: output tag %s is unsupported", n.name, ref, tag)
		}
		if d := g.jnode(name, n.side, n.name); d != nil && d.srcjar != "" {
			return []string{d.srcjar}, true
		}
		return nil, true
	case "java_sdk_library":
		comp := sdkLibTagComponent(strings.TrimPrefix(name, ":"), tag)
		if comp == "" {
			g.problem("%s: %s: java_sdk_library output tag %q is unsupported", n.name, ref, tag)
			return nil, true
		}
		if d := g.jnode(comp, n.side, n.name); d != nil && d.srcjar != "" {
			return []string{d.srcjar}, true
		}
		return nil, true
	case "aconfig_declarations_group":
		if tag != ".srcjars" {
			g.problem("%s: %s: aconfig_declarations_group output tag %q is unsupported (only .srcjars)", n.name, ref, tag)
			return nil, true
		}
		return g.aconfigGroupSrcjars(m, n.side, n.name), true
	}
	return nil, false
}

// aconfigGroupSrcjars: an aconfig_declarations_group's `.srcjars` (aconfig_declarations_group.go): the srcjars of its nested groups, then
// of its java_aconfig_libraries, in property order.
func (g *Graph) aconfigGroupSrcjars(m *Module, side Side, from string) []string {
	var out []string
	for _, r := range m.Props.strs("aconfig_declarations_groups") {
		gm := g.lookupJava(r, side)
		switch {
		case gm != nil && g.baseType(gm.Type) == "aconfig_declarations_group":
			out = append(out, g.aconfigGroupSrcjars(gm, side, from)...)
		case gm != nil && gm.Type == "aconfig_declarations":
			// provides no CodegenInfo: the group's VisitDirectDeps skips it
		default:
			g.problem("%s: %s is not an aconfig_declarations_group", from, r)
		}
	}
	for _, r := range m.Props.strs("java_aconfig_libraries") {
		if d := g.jnode(r, side, from); d != nil && d.srcjar != "" {
			out = append(out, d.srcjar)
		}
	}
	return out
}

// javaLib builds java_library / java_library_static / java_library_host / java_plugin / java_binary_host / java_aconfig_library's
// library half (base.go compile). extraSrcJars: generated sources of the module type (aconfig).
func (g *Graph) javaLib(n *jNode, extraSrcJars ...string) {
	p := n.props
	j := g.java
	if n.typ == "java_sdk_library" {
		// collectDeps: depending on the library itself is an error; users name <lib>.impl or a stubs component (stubs.go)
		g.problem("%s: cannot depend directly on java_sdk_library %s", n.name, n.mod.Name)
		return
	}
	// javac_shard_size shards javac only with Turbine (base.go enableSharding), which is off
	for _, k := range []string{"services"} {
		if p.get(k) != nil {
			g.problem("%s: %s is unsupported", n.name, k)
		}
	}
	if o := p.obj("optimize"); o != nil && o.boolean("enabled", false) {
		g.problem("%s: optimize (R8) is unsupported", n.name)
	}
	s := parseSdk(p.str("sdk_version"))
	ver := g.defaultJavaVersion(n.side, s)
	if v := p.str("java_version"); v != "" {
		if ver = parseJavaVersion(v); ver == 0 {
			g.problem("%s: java_version %s is unsupported", n.name, v)
			ver = 21
		}
	}
	srcs := p.strs("srcs")
	if ver.modules() {
		srcs = append(srcs, p.obj("openjdk9").strs("srcs")...)
	}
	g.implicitJavaLibs(n, srcs)
	d, _ := g.collectJavaDeps(n, s)
	if n.typ == "java_plugin" {
		n.processor = p.str("processor_class")
	}
	for _, dir := range p.obj("aidl").strs("export_include_dirs") {
		n.aidlExport = append(n.aidlExport, filepath.Join(n.mod.Dir, dir))
	}
	for _, r := range p.strs("exported_plugins") {
		if pl := g.jnode(r, Host, n.name); pl != nil {
			n.expPluginJars = append(n.expPluginJars, pl.implRes)
			if pl.processor != "" {
				n.expProcessors = append(n.expProcessors, pl.processor)
			}
		}
	}
	src := g.javaSrcs(n, srcs)
	java, srcjars, waits := src.java, src.srcjars, src.waits
	// genSources: logtags java in place (here: after the module's own), then proto and aidl srcjars after its own .srcjar sources
	for _, ext := range genOrder {
		if files := src.gen[ext]; len(files) > 0 {
			if ext == ".aidl" && d.aidlGen != "" {
				d.aidlPreprocess = g.sdkFrameworkAidl(d.aidlGen, n.name)
			}
			gj, gs := javaGen(ext)(g, n, files, d)
			java = append(java, gj...)
			srcjars = append(srcjars, gs...)
		}
	}
	srcjars = append(srcjars, extraSrcJars...)
	stem := p.str("stem")
	if stem == "" {
		stem = n.name
	}
	jar := stem + ".jar"
	out := func(sub string) string { return filepath.Join(n.dir, sub, jar) }

	// javac flags (collectJavacFlags)
	flags := append([]string{}, p.strs("javacflags")...)
	flags = append(flags, "-Xlint:-dep-ann")
	if ver.modules() {
		flags = append(flags, p.obj("openjdk9").strs("javacflags")...)
		if pm := p.str("patch_module"); pm != "" {
			// "." = the tree root javac runs in, then the build directory (Soong: SoongOutDir)
			paths := []string{".", g.out}
			if len(d.classpath) > 0 {
				paths = append(paths, strings.Join(d.classpath, ":"))
			}
			flags = append(flags, "--patch-module="+pm+"="+strings.Join(paths, ":"))
		}
	}
	var localImpl []string
	// compile: jarjar rename rules inherited from dependencies (jarjar.go), applied to each compiled jar (repackageFlagsIfNecessary)
	repackage := g.repackageRules(n, srcs, s)
	if p.boolean("headers_only", false) {
		// base.go compile: headers_only compiles header classes with Turbine (Soong rejects it with Turbine off) and nothing else
		g.turbineHeaders(n, java, src.kt, srcjars, waits, flags, ver, d, jar, repackage)
		return
	}
	if len(src.kt) > 0 {
		// kotlinc first over every .java and .kt source (it reads the java ones only to resolve types), then javac against its header jar
		if len(d.processorPath) > 0 {
			g.problem("%s: annotation processors on Kotlin sources need kapt, which is unsupported", n.name)
		}
		kt, kh := filepath.Join(n.dir, "kotlin", jar), filepath.Join(n.dir, "kotlin_headers", jar)
		g.kotlinc(n, kt, kh, java, src.kt, srcjars, waits, ver, d)
		localImpl = append(localImpl, g.repackage(n, kt, repackage, "kotlinc", jar))
		d.classpath = append([]string{kh}, d.classpath...)
	}
	if len(java)+len(srcjars) > 0 {
		classes := out("javac")
		g.javac(n, classes, java, srcjars, waits, flags, ver, d)
		localImpl = append(localImpl, g.repackage(n, classes, repackage, "javac", jar))
	}

	// resources (java_resource_dirs, java_resources)
	resArgs, resDeps, resWaits := g.javaResources(n)
	var localRes []string
	if len(resArgs) > 0 {
		r := out("res")
		j.edge("jar", []string{r}, nil, resDeps, resWaits, [][2]string{{"args", strings.Join(resArgs, " ")}})
		localRes = append(localRes, r)
	}
	if p.boolean("include_srcs", false) {
		// the module's java sources as resources (srcJarArgs: srcs after generation, without srcjars)
		var files []srcFile
		for _, s := range srcs {
			if isRef(s) {
				if _, ok := g.javaSrcRef(n, s); ok {
					g.problem("%s: include_srcs of generated stubs %s is unsupported", n.name, s)
					continue
				}
			}
			fs, _ := g.resolveSrc(n.mod.Dir, n.side, s, n.name)
			for _, f := range fs {
				if filepath.Ext(f.path) == ".java" {
					files = append(files, f)
				}
			}
		}
		a, deps, w := g.fileJarArgs(files)
		r := filepath.Join(n.dir, n.name+".srcjar")
		j.edge("jar", []string{r}, nil, deps, w, [][2]string{{"args", strings.Join(a, " ")}})
		localRes = append(localRes, r)
	}
	n.res = dedupKeep(append(append([]string{}, localRes...), d.staticRes...))

	// combined implementation jar
	manifest := ""
	if mf := p.str("manifest"); mf != "" {
		manifest = g.abs(filepath.Join(n.mod.Dir, mf))
	}
	if mc := p.str("main_class"); mc != "" && n.typ == "java_binary_host" {
		// java.go Binary: GenerateMainClassManifest's manifest.txt overrides the manifest
		if manifest != "" {
			g.problem("%s: main_class cannot be used when manifest is set", n.name)
		}
		manifest = filepath.Join(n.dir, "manifest.txt")
		g.genFile(manifest, "Main-Class: "+mc+"\n")
	}
	implJars := dedupKeep(append(append([]string{}, localImpl...), d.staticImpl...))
	var impl string
	switch {
	case len(implJars) == 0 && manifest == "":
	case len(implJars) == 1 && manifest == "":
		impl = implJars[0]
	default:
		impl = out("combined")
		j.combine(impl, implJars, manifest)
	}
	if rules := g.jarjarRulesFile(n); rules != "" && impl != "" {
		// jarjarIfNecessary: the implementation jar in jarjar_shards shards, the resources in one
		shards := 1
		if v := p.str("jarjar_shards"); v != "" {
			if k, err := strconv.Atoi(v); err == nil && k > 0 {
				shards = k
			} else {
				g.problem("%s: jarjar_shards must be an integer represented as a string", n.name)
			}
		}
		jj := out("jarjar")
		g.jarjar(n, jj, impl, rules, shards)
		impl = jj
		implJars = []string{jj}
		if len(n.res) > 0 {
			rc := n.res[0]
			if len(n.res) > 1 {
				rc = out("res-combined")
				j.combine(rc, n.res, "")
			}
			rj := filepath.Join(n.dir, "jarjar", "resource", jar)
			g.jarjar(n, rj, rc, rules, 1)
			n.res = []string{rj}
		}
	}
	var validations []string
	if pkgs := p.strs("permitted_packages"); len(pkgs) > 0 && impl != "" {
		// base.go: a validation of the implementation jar, run whenever something uses it
		stamp := filepath.Join(n.dir, "package-check.stamp")
		j.edge("pkgcheck", []string{stamp}, []string{impl}, nil, nil, [][2]string{{"packages", shellQuote(pkgs)}})
		validations = append(validations, stamp)
	}
	n.impl = implJars
	if impl != "" {
		n.header = out("javac-header")
		j.edgeV("header", []string{n.header}, []string{impl}, nil, nil, validations, nil)
	}

	// implementation + resources
	n.implRes = impl
	if len(n.res) > 0 {
		n.implRes = out("withres")
		j.combine(n.implRes, append(append([]string{}, n.res...), implJars...), manifest)
	}

	compileDex := p.boolean("compile_dex", false) || p.boolean("installable", false)
	if n.side == Device && compileDex && impl != "" {
		g.javaDex(n, d, s, jar)
	}
	if n.typ == "java_binary_host" {
		g.javaBinary(n, stem)
	}
}

// output is what building a Java goal means: its dex jar, else its jar with resources, else its stubs / system modules.
func (n *jNode) output() []string {
	switch {
	case n.dex != "":
		return []string{n.dex}
	case n.implRes != "":
		return []string{n.implRes}
	case n.srcjar != "":
		return []string{n.srcjar}
	}
	return n.sysDeps
}

// javac is transformJavaToClasses (builder.go): java >= 9 compiles against system modules (--system, none on device when there are
// none) with java9Classpath first on the classpath; older targets against the bootclasspath (explicitly empty on device).
func (g *Graph) javac(n *jNode, out string, java, srcjars, waits, flags []string, ver javaVersion, d javaDeps) {
	boot, implicit := "", []string{}
	cp := d.classpath
	if ver.modules() {
		switch {
		case d.sys != nil:
			boot = "--system=" + esc(d.sys.sysDir)
			implicit = append(implicit, d.sys.sysDeps...)
		case n.side == Device:
			boot = "--system=none"
		}
		cp = append(append([]string{}, d.java9...), cp...)
	} else {
		implicit = append(implicit, d.boot...)
		switch {
		case len(d.boot) == 0 && n.side == Device:
			boot = `-bootclasspath ""`
		case len(d.boot) > 0:
			boot = "-bootclasspath " + esc(strings.Join(d.boot, ":"))
		}
	}
	implicit = append(append(append(implicit, cp...), d.processorPath...), srcjars...)
	processor := "-proc:none"
	if len(d.processors) > 0 {
		processor = "-processor " + strings.Join(d.processors, ",")
	}
	dir := filepath.Dir(out)
	vars := [][2]string{{"flags", ninjaRaw(flags)}, {"bootcp", boot}, {"processor", processor}, {"srcjars", join(srcjars)},
		{"ver", ver.String()}, {"outdir", esc(filepath.Join(dir, "classes"))}, {"annodir", esc(filepath.Join(dir, "anno"))},
		{"srcjardir", esc(filepath.Join(dir, "srcjars"))}}
	if len(cp) > 0 {
		vars = append(vars, [2]string{"cp", "-classpath " + esc(strings.Join(cp, ":"))})
	}
	if len(d.processorPath) > 0 {
		vars = append(vars, [2]string{"processorpath", "-processorpath " + esc(strings.Join(d.processorPath, ":"))})
	}
	// javac runs from the tree root with tree files named tree-relative, as Soong's: annotation processors record these paths in
	// their outputs (unsupportedappusage .uau, compat config XML)
	vars = append(vars, [2]string{"srcs", join(java)})
	g.java.edge("javac", []string{out}, absAll(g, java), implicit, waits, vars)
}

// javaDex is dexer.compileDex with d8 plus the dex-withres merge (base.go), dex.go dexCommonFlags / d8Flags.
func (g *Graph) javaDex(n *jNode, d javaDeps, s sdkSpec, jar string) {
	p := n.props
	j := g.java
	var flags []string
	release := false
	for _, f := range p.strs("dxflags") {
		switch f {
		case "--core-library", "--dex", "--multi-dex", "--debug":
		case "--release":
			release = true
		default:
			flags = append(flags, f)
		}
	}
	if release {
		flags = append(flags, "--release")
	} else {
		flags = append(flags, "--debug")
	}
	minSdk := p.str("min_sdk_version")
	if minSdk == "" {
		minSdk = s.level
	}
	platform, final := g.release.PlatformSdk()
	if _, err := strconv.Atoi(minSdk); err != nil {
		// current / none / core_platform / a codename: ApiLevel.EffectiveVersion
		minSdk = platform
	}
	platformBuild := !s.stable()
	if minSdk == futureApiLevel || !final && minSdk == platform {
		minSdk = g.release.Flag("RELEASE_PLATFORM_SDK_VERSION")
		platformBuild = true
	}
	// dex container format (DEX v41) from API 36 (W), while the platform is 36 or Baklava
	r := g.release
	lvl, _ := strconv.Atoi(minSdk)
	plat, _ := strconv.Atoi(r.Flag("RELEASE_PLATFORM_SDK_VERSION"))
	if lvl >= 36 && !p.boolean("no_dex_container", false) && r.Bool("RELEASE_USE_DEX_V41") &&
		(plat >= 36 || r.Flag("RELEASE_PLATFORM_VERSION_CODENAME") == "Baklava") {
		flags = append([]string{"-JDcom.android.tools.r8.dexContainerExperiment"}, flags...)
	}
	flags = append(flags, "--min-api", minSdk)
	if platformBuild && !slices.Contains(flags, "--android-platform-build") {
		flags = append(flags, "--android-platform-build")
	}
	var libs []string
	for _, l := range append(append([]string{}, d.boot...), d.dexClasspath...) {
		flags = append(flags, "--lib", l)
		libs = append(libs, l)
	}
	uncompress := p.boolean("uncompress_dex", true)
	zipFlags := "--ignore_missing_files"
	if uncompress {
		zipFlags += " -L 0"
	}
	dex := filepath.Join(n.dir, "dex", jar)
	d8 := g.jtool("d8", n.name)
	j.edge("d8", []string{dex}, []string{n.implRes}, append(libs, d8), nil, [][2]string{
		{"flags", ninjaRaw(flags)}, {"outdir", esc(filepath.Join(n.dir, "dex"))}, {"zipflags", zipFlags}, {"d8", esc(d8)},
	})
	if uncompress {
		a := filepath.Join(n.dir, "aligned", jar)
		j.edge("zipalign", []string{a}, []string{dex}, nil, nil, nil)
		dex = a
	}
	if len(n.res) > 0 {
		dr := filepath.Join(n.dir, "dex-withres", jar)
		j.combine(dr, append([]string{dex}, n.res...), "")
		dex = dr
		if uncompress {
			a := filepath.Join(n.dir, "dex-withres-aligned", jar)
			j.edge("zipalign", []string{a}, []string{dr}, nil, nil, nil)
			dex = a
		}
	}
	n.dex = dex
}

// javaBinary installs a java_binary_host as Soong lays it out (HostJavaToolPath / HostBinToolPath): <out>/host/framework/<stem>.jar
// and, with `wrapper`, <out>/host/bin/<stem> running it; the wrapper finds its jar at ../framework.
func (g *Graph) javaBinary(n *jNode, stem string) {
	j := g.java
	jarOut := filepath.Join(g.out, "host", "framework", stem+".jar")
	j.edge("cp", []string{jarOut}, []string{n.implRes}, nil, nil, nil)
	n.tool, n.toolJar = jarOut, jarOut
	if n.typ != "java_binary_host" {
		return
	}
	// java.go Binary: the wrapper, else build/soong/scripts/jar-wrapper.sh (host), installed as bin/<module name>; it runs ../framework/<name>.jar
	w := filepath.Join("build/soong/scripts", "jar-wrapper.sh")
	if p := n.props.str("wrapper"); p != "" {
		w = filepath.Join(n.mod.Dir, p)
	}
	// installed executable whatever its mode in the tree (prebuilts/r8/d8 is 0644 in git)
	bin := filepath.Join(g.out, "host", "bin", n.name)
	j.edge("installexec", []string{bin}, []string{g.abs(w)}, []string{jarOut}, nil, nil)
	n.tool = bin
}

// metalavaTool: metalava as a runnable tool for genrules, the -metalava jar (J1: Soong's build of it until metalava is built here) installed
// as host/framework/metalava.jar with jar-wrapper.sh as host/bin/metalava, as javaBinary lays out a java_binary_host.
func (g *Graph) metalavaTool() string {
	j := g.java
	bin := filepath.Join(g.out, "host", "bin", "metalava")
	if !j.metalavaInstalled {
		j.metalavaInstalled = true
		jar := filepath.Join(g.out, "host", "framework", "metalava.jar")
		j.edge("cp", []string{jar}, []string{j.tools.Metalava}, nil, nil, nil)
		j.edge("installexec", []string{bin}, []string{g.abs("build/soong/scripts/jar-wrapper.sh")}, []string{jar}, nil, nil)
	}
	return bin
}

// jtoolJar: the jar of a host Java tool, for rules that run `java -jar` themselves (jarjar).
func (g *Graph) jtoolJar(name, from string) string {
	g.jtool(name, from)
	if t := g.java.nodes[jKey{name, Host}]; t != nil && t.toolJar != "" {
		return t.toolJar
	}
	return filepath.Join(g.out, "missing-tool", name+".jar")
}

// jtool: the runnable path of a host Java tool module (java_binary_host / java_library_host with a Main-Class manifest).
func (g *Graph) jtool(name, from string) string {
	t := g.jnode(name, Host, from+" (tool "+name+")")
	if t == nil {
		return filepath.Join(g.out, "missing-tool", name)
	}
	if t.tool == "" {
		g.javaBinary(t, name)
	}
	return t.tool
}

// javaImport is java_import / java_import_host: the jars (and static_libs) combined into one; header = implementation.
func (g *Graph) javaImport(n *jNode) {
	p := n.props
	var jars []string
	for _, f := range g.globs(n.mod.Dir, p.strs("jars")) {
		jars = append(jars, g.abs(f))
	}
	_, statics := g.collectJavaDeps(n, sdkSpec{kind: "none"})
	for _, s := range statics {
		jars = append(jars, s.impl...)
	}
	switch len(jars) {
	case 0:
		g.problem("%s: java_import without jars", n.name)
		return
	case 1:
		n.impl = jars
		n.header = jars[0]
	default:
		c := filepath.Join(n.dir, "combined", n.name+".jar")
		g.java.combine(c, jars, "")
		n.impl = []string{c}
		n.header = c
	}
	n.implRes = n.header
}

// sysModules is java_system_modules: its libs' header jars through jmod + jlink into a --system directory (system_modules.go).
func (g *Graph) sysModules(n *jNode) {
	for _, r := range n.props.strs("libs") {
		if l := g.jnode(r, n.side, n.name); l != nil && l.header != "" {
			n.sysJars = append(n.sysJars, l.header)
		}
	}
	n.sysDir = filepath.Join(n.dir, "system")
	n.sysDeps = []string{filepath.Join(n.sysDir, "lib", "modules"), filepath.Join(n.sysDir, "lib", "jrt-fs.jar"), filepath.Join(n.sysDir, "release")}
	g.java.edge("sysmodules", n.sysDeps, n.sysJars, nil, nil, [][2]string{
		{"classpath", esc(strings.Join(n.sysJars, ":"))}, {"outdir", esc(n.sysDir)}, {"workdir", esc(filepath.Join(n.dir, "modules"))},
	})
	n.header = ""
}

// exportedDir is droiddoc_exported_dir: a source directory metalava reads (path, default the module dir).
func (g *Graph) exportedDir(n *jNode) {
	path := n.props.str("path")
	n.expDir = g.abs(filepath.Join(n.mod.Dir, path))
	for _, f := range globTree(n.expDir, "**/*", "") {
		n.expDirDeps = append(n.expDirDeps, filepath.Join(n.expDir, f))
	}
}

// ninjaRaw: flags as Soong passes them, joined unquoted into the shell command (javacflags may hold "--add-exports x=y"), `$` escaped
// for ninja.
func ninjaRaw(flags []string) string { return strings.ReplaceAll(strings.Join(flags, " "), "$", "$$") }

func absAll(g *Graph, paths []string) []string {
	out := make([]string, len(paths))
	for i, p := range paths {
		out[i] = g.abs(p)
	}
	return out
}

// combine is TransformJarsToJar (merge_zips; module-info.class stripped, a manifest when given).
func (j *javaGraph) combine(out string, jars []string, manifest string) {
	args := []string{}
	var implicit []string
	if manifest != "" {
		args = append(args, "-m", esc(manifest))
		implicit = append(implicit, manifest)
	}
	args = append(args, "-stripFile", "module-info.class", "-stripFile", "'META-INF/versions/*/module-info.class'")
	j.edge("combine", []string{out}, jars, implicit, nil, [][2]string{{"args", strings.Join(args, " ")}})
}

// edge writes one build statement: inputs, implicit inputs, order-only inputs (generated headers, …), variables.
func (j *javaGraph) edge(rule string, outs, ins, implicit, orderOnly []string, vars [][2]string) {
	j.edgeV(rule, outs, ins, implicit, orderOnly, nil, vars)
}

// edgeV is edge with ninja validations (`|@`: built with the outputs, never blocking their users).
func (j *javaGraph) edgeV(rule string, outs, ins, implicit, orderOnly, validations []string, vars [][2]string) {
	fmt.Fprintf(&j.b, "build %s: %s %s", join(outs), rule, join(ins))
	if len(implicit) > 0 {
		fmt.Fprintf(&j.b, " | %s", join(implicit))
	}
	if len(orderOnly) > 0 {
		fmt.Fprintf(&j.b, " || %s", join(orderOnly))
	}
	if len(validations) > 0 {
		fmt.Fprintf(&j.b, " |@ %s", join(validations))
	}
	j.b.WriteString("\n")
	for _, v := range vars {
		fmt.Fprintf(&j.b, "  %s = %s\n", v[0], v[1])
	}
}

// javaResources is ResourceDirsToJarArgs + ResourceFilesToJarArgs (java_resources.go): soong_zip args `-C dir -f file`, the files
// (rebuild triggers) and generated inputs to wait for.
func (g *Graph) javaResources(n *jNode) (args, deps, waits []string) {
	p := n.props
	excludeDirs := map[string]bool{}
	for _, d := range g.globDirs(n.mod.Dir, p.strs("exclude_java_resource_dirs")) {
		excludeDirs[d] = true
	}
	excludeFiles := map[string]bool{}
	for _, f := range g.globs(n.mod.Dir, p.strs("exclude_java_resources")) {
		excludeFiles[f] = true
	}
	for _, rd := range g.globDirs(n.mod.Dir, p.strs("java_resource_dirs")) {
		if excludeDirs[rd] {
			continue
		}
		files := globTree(g.abs(rd), "**/*", rd)
		var kept []string
		for _, f := range files {
			if excludeFiles[f] || isResourceExcluded(f) || underAny(f, excludeDirs) {
				continue
			}
			kept = append(kept, f)
		}
		if len(kept) == 0 {
			continue
		}
		slices.Sort(kept)
		args = append(args, "-C", esc(g.abs(rd)))
		for _, f := range kept {
			args = append(args, "-f", esc(matchEscape(g.abs(f))))
			deps = append(deps, g.abs(f))
		}
	}
	var files []srcFile
	for _, r := range p.strs("java_resources") {
		fs, _ := g.resolveSrc(n.mod.Dir, n.side, r, n.name)
		for _, f := range fs {
			if !excludeFiles[f.path] && !isResourceExcluded(f.path) {
				files = append(files, f)
			}
		}
	}
	a, d, w := g.fileJarArgs(files)
	return append(args, a...), append(deps, d...), append(waits, w...)
}

// fileJarArgs is resourcePathsToJarArgs: each file below its root (Path.Rel: the module dir or a filegroup's path), a new -C
// whenever the root changes.
func (g *Graph) fileJarArgs(files []srcFile) (args, deps, waits []string) {
	lastDir := ""
	for i, f := range files {
		if f.gen != nil {
			waits = append(waits, f.gen.rule.deps...)
		}
		path := g.abs(f.path)
		dir := g.abs(f.root)
		if !strings.HasPrefix(path, dir+"/") {
			dir = filepath.Dir(path)
		}
		if i == 0 || dir != lastDir {
			args = append(args, "-C", esc(dir))
		}
		lastDir = dir
		args = append(args, "-f", esc(matchEscape(path)))
		deps = append(deps, path)
	}
	return args, deps, dedupKeep(waits)
}

func underAny(f string, dirs map[string]bool) bool {
	for d := range dirs {
		if strings.HasPrefix(f, d+"/") {
			return true
		}
	}
	return false
}

// isResourceExcluded is java_resources.go resourceExcludes: **/*.java, **/package.html, **/overview.html, **/.*.swp, **/.DS_Store, **/*~.
func isResourceExcluded(path string) bool {
	b := filepath.Base(path)
	return strings.HasSuffix(b, ".java") || b == "package.html" || b == "overview.html" ||
		strings.HasPrefix(b, ".") && strings.HasSuffix(b, ".swp") || b == ".DS_Store" || strings.HasSuffix(b, "~")
}

// matchEscape is pathtools.MatchEscape: soong_zip -f takes a glob.
func matchEscape(s string) string {
	return strings.NewReplacer(`\`, `\\`, `*`, `\*`, `?`, `\?`, `[`, `\[`).Replace(s)
}

// globDirs resolves directory paths / globs below dir (tree-relative results).
func (g *Graph) globDirs(dir string, pats []string) []string {
	var out []string
	for _, p := range pats {
		if !strings.ContainsAny(p, "*?[") {
			out = append(out, filepath.Join(dir, p))
			continue
		}
		seen := map[string]bool{}
		for _, f := range globTree(filepath.Join(g.tree, dir), p+"/**/*", dir) {
			rel := strings.TrimPrefix(f, dir+"/")
			// the directory part matched by the pattern: as many components as the pattern has
			n := strings.Count(p, "/") + 1
			parts := strings.SplitN(rel, "/", n+1)
			if len(parts) > n {
				d := filepath.Join(dir, strings.Join(parts[:n], "/"))
				if !seen[d] {
					seen[d] = true
					out = append(out, d)
				}
			}
		}
		slices.Sort(out)
	}
	return out
}

// javaHeader: the rules and tool variables of every Java edge. Command shapes follow Soong's rules of the same names
// (java/builder.go javac, jar, combineJar, convertImplementationJarToHeaderJar, jarjar, packageCheck, zipalign; dex.go d8;
// system_modules.go jarsTosystemModules). Soong's javac wrapper and incremental-state tools (soong_javac_wrapper, find_input_delta)
// do not change outputs and are not used; srcjars are unpacked with unzip (zipsync's job).
func (j *javaGraph) header(g *Graph) string {
	c := j.cfg
	tools := j.tools
	bt := func(t string) string { return esc(filepath.Join(tools.BuildTools, t)) }
	javacVm := "-J-Xmx" + c.JavacHeapSize + " " + strings.Join(c.JavacVmFlags, " ")
	// only srcjars holding .java files are unpacked: unzip fails on an empty zip (aidl parcelable declarations generate no source) and when no
	// entry matches
	unpack := `(for j in $srcjars; do if unzip -Z1 $$j 2>/dev/null | grep -q '\.java$$'; then unzip -qo $$j '*.java' -d $srcjardir || exit 1; fi; done) && find $srcjardir -name '*.java' | sort > $srcjardir/list`
	var b strings.Builder
	fmt.Fprintf(&b, "jdk = %s\nsoongzip = %s\nmergezips = %s\nzip2zip = %s\nzipalign = %s\n", esc(tools.JDK), bt("soong_zip"),
		bt("merge_zips"), bt("zip2zip"), bt("zipalign"))
	fmt.Fprintf(&b, `rule javac
  command = rm -rf $outdir $annodir $srcjardir $out.tmp && mkdir -p $outdir $annodir $srcjardir && %s && (if [ -s $srcjardir/list ] || [ -s $out.rsp ]; then cd $root && $jdk/bin/javac %s %s $processorpath $processor $flags $bootcp $cp -source $ver -target $ver -d $outdir -s $annodir @$out.rsp @$srcjardir/list; fi) && $soongzip -jar -o $out -C $outdir -D $outdir && rm -rf $outdir $srcjardir
  rspfile = $out.rsp
  rspfile_content = $srcs
  description = javac $out
rule jar
  command = cd $root && $soongzip -jar -o $out @$out.rsp
  rspfile = $out.rsp
  rspfile_content = $args
  description = jar $out
rule combine
  command = $mergezips --ignore-duplicates -j $args $out $in
  description = merge $out
rule header
  command = $zip2zip -i $in -o $out -x 'META-INF/services/**/*'
  description = header $out
rule d8
  command = rm -rf $outdir && mkdir -p $outdir && PATH=$jdk/bin:$$PATH $d8 %s $flags --output $outdir --no-dex-input-jar $in && $soongzip $zipflags -o $outdir/classes.dex.jar -C $outdir -f "$outdir/classes*.dex" && $mergezips -D -stripFile "**/*.class" $out $outdir/classes.dex.jar $in && rm -f $outdir/classes*.dex $outdir/classes.dex.jar
  description = d8 $out
rule zipalign
  command = if ! $zipalign -c -p 4 $in > /dev/null; then $zipalign -f -p 4 $in $out; else cp -f $in $out; fi
  description = align $out
rule jarjar
  command = rm -f $out && $jdk/bin/java %s -DremoveAndroidCompatAnnotations=true -jar $jarjar process $rules $in $out $total $index &&[ -e $out ] || (echo "Missing output file"; exit 1)
  description = jarjar $out
rule pkgcheck
  command = rm -f $out && $root/build/soong/scripts/package-check.sh $in $packages && touch $out
  description = package check $in
rule sysmodules
  command = rm -rf $outdir $workdir && mkdir -p $workdir/jmod && $root/build/soong/scripts/jars-to-module-info-java.sh java.base $in > $workdir/module-info.java && $jdk/bin/javac --system=none --patch-module=java.base=$classpath $workdir/module-info.java && $soongzip -jar -o $workdir/classes.jar -C $workdir -f $workdir/module-info.class && $mergezips -j $workdir/module.jar $workdir/classes.jar $in && $jdk/bin/jmod create --module-version %s --target-platform LINUX-OTHER --class-path $workdir/module.jar $workdir/jmod/java.base.jmod && $jdk/bin/jlink --module-path $workdir/jmod --add-modules java.base --output $outdir --disable-plugin system-modules && rm -rf $workdir && cp $jdk/lib/jrt-fs.jar $outdir/lib/
  description = system modules $outdir
rule installexec
  command = cp -f $in $out && chmod +x $out
  description = install $out
`, unpack, javacVm, strings.Join(c.CommonJdkFlags, " "), strings.Join(c.D8Flags, " "), strings.Join(c.JavaVmFlags, " "), c.JlinkVersion)
	b.WriteString(j.stubsRules(g, unpack))
	b.WriteString(aconfigJavaRule)
	b.WriteString(javaGenRules)
	b.WriteString(syspropJavaRule)
	b.WriteString(sdkAidlRules)
	b.WriteString(j.turbineRule())
	b.WriteString(j.kotlincRule(g))
	return b.String()
}
