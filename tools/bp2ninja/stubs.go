// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"slices"
	"strings"
)

// API stubs: droidstubs (build/soong/java/droidstubs.go, the spec) and the modules a java_sdk_library creates (sdk_library.go
// CreateInternalModules, sdk_library_internal.go).
//   - droidstubs runs metalava for the "everything" stubs only: --api/--removed-api/--stubs, annotation and inclusion-annotation
//     flags, `args`. Not run: API checks (check_api, api_lint, --check-compatibility), API levels, exportable stubs, doc stubs; they
//     validate or produce files no build edge here consumes.
//   - java_sdk_library components are synthesised under their Soong names when looked up: <lib>.stubs.source[.<scope>]
//     (droidstubs), <lib>.stubs[.<scope>].from-source and <lib>.stubs[.<scope>] (java_library). A library that builds its stubs
//     from API text (build_from_text_stub, default true; java_api_library) is reported, not built.

// apiScope is sdk_library.go's apiScope, the fields this build reads.
type apiScope struct {
	name, prop, suffix, filePrefix, sdkVersion, annotation string
	extends                                                *apiScope
	extraArgs                                              []string
	unstable, legacyDefault                                bool
}

var (
	scopePublic = &apiScope{name: "public", prop: "public", sdkVersion: "current", legacyDefault: true}
	scopeSystem = &apiScope{name: "system", prop: "system", suffix: ".system", filePrefix: "system-", sdkVersion: "system_current",
		annotation: "android.annotation.SystemApi(client=android.annotation.SystemApi.Client.PRIVILEGED_APPS)", extends: scopePublic}
	scopeTest = &apiScope{name: "test", prop: "test", suffix: ".test", filePrefix: "test-", sdkVersion: "test_current",
		annotation: "android.annotation.TestApi", extends: scopeSystem, unstable: true}
	scopeModuleLib = &apiScope{name: "module-lib", prop: "module_lib", suffix: ".module_lib", filePrefix: "module-lib-",
		sdkVersion: "module_current", annotation: "android.annotation.SystemApi(client=android.annotation.SystemApi.Client.MODULE_LIBRARIES)",
		extends: scopeSystem}
	scopeSystemServer = &apiScope{name: "system-server", prop: "system_server", suffix: ".system_server", filePrefix: "system-server-",
		sdkVersion: "system_server_current", annotation: "android.annotation.SystemApi(client=android.annotation.SystemApi.Client.SYSTEM_SERVER)",
		extends: scopePublic, extraArgs: []string{"--hide-annotation", "android.annotation.Hide", "--hide", "InternalClasses"}}
	allScopes = []*apiScope{scopePublic, scopeSystem, scopeTest, scopeModuleLib, scopeSystemServer}
)

// droidstubsArgs is initApiScope's scope-specific droidstubs arguments, shell-escaped.
func (s *apiScope) droidstubsArgs() []string {
	var args []string
	if s.annotation != "" {
		args = append(args, "--show-annotation", s.annotation)
	}
	for x := s; x != nil; x = x.extends {
		args = append(args, x.extraArgs...)
		if x != s && x.annotation != "" {
			args = append(args, "--show-for-stub-purposes-annotation", x.annotation)
		}
	}
	if len(args) == 0 {
		return nil
	}
	return mapStrings(args, shellEscape)
}

// sdkLibScopes is getGeneratedApiScopes: explicitly enabled scopes switch every scope to its non-legacy default (public only).
// generate_system_and_test_apis (legacy system/test default) holds when the library builds against standard libraries.
func sdkLibScopes(p *Obj) []*apiScope {
	explicit := false
	for _, s := range allScopes {
		if _, ok := p.obj(s.prop).get("enabled").(bool); ok {
			explicit = true
		}
	}
	stdLibs := true
	if sv := p.str("sdk_version"); sv == "none" || sv == "core_platform" {
		stdLibs = false
	}
	var out []*apiScope
	for _, s := range allScopes {
		def := s == scopePublic
		if !explicit {
			def = s.legacyDefault || (s == scopeSystem || s == scopeTest) && stdLibs
		}
		if p.obj(s.prop).boolean("enabled", def) {
			out = append(out, s)
		}
	}
	return out
}

// sdkLibTagComponent: the component an output tag of a java_sdk_library names (`{.public.stubs.source}` …), "" when unsupported.
func sdkLibTagComponent(lib, tag string) string {
	for _, s := range allScopes {
		if tag == "."+s.name+".stubs.source" {
			return lib + ".stubs.source" + s.suffix
		}
	}
	return ""
}

// sdkLibComponent synthesises the module a java_sdk_library creates under name, nil when name is no component.
func (g *Graph) sdkLibComponent(name string, side Side) *Module {
	if base, ok := strings.CutSuffix(name, ".impl"); ok {
		// createImplLibrary: the library itself as a java_library, impl_only_libs / impl_only_static_libs added
		if lib := g.lookupWith(base, side, isJavaPrebuilt); lib != nil && g.baseType(lib.Type) == "java_sdk_library" {
			props := newObj()
			for _, k := range lib.Props.keys {
				props.set(k, lib.Props.m[k])
			}
			props.set("name", name)
			p := g.flattenJava(lib, side)
			for from, to := range map[string]string{"impl_only_libs": "libs", "impl_only_static_libs": "static_libs"} {
				if extra := p.strs(from); len(extra) > 0 {
					l, _ := props.get(to).([]Val)
					for _, e := range extra {
						l = append(l, e)
					}
					props.set(to, l)
				}
			}
			return &Module{Type: "java_library", Name: name, Dir: lib.Dir, Props: props}
		}
		return nil
	}
	for _, s := range allScopes {
		for _, c := range []struct{ suffix, kind string }{
			{".stubs.source" + s.suffix, "droidstubs"},
			{".stubs" + s.suffix + ".from-source", "from-source"},
			{".stubs" + s.suffix + ".from-text", "from-text"},
			{".stubs" + s.suffix, "top"},
		} {
			base, ok := strings.CutSuffix(name, c.suffix)
			if !ok || base == "" {
				continue
			}
			lib := g.lookupWith(base, side, isJavaPrebuilt)
			if lib == nil || g.baseType(lib.Type) != "java_sdk_library" {
				continue
			}
			key := name + "\x00" + side.String()
			if m, ok := g.java.synth[key]; ok {
				return m
			}
			p := g.flattenJava(lib, side)
			if !containsScope(sdkLibScopes(p), s) {
				continue
			}
			m := g.sdkLibModule(lib, p, s, c.kind, name)
			g.java.synth = addModule(g.java.synth, key, m)
			return m
		}
	}
	return nil
}

func containsScope(list []*apiScope, s *apiScope) bool {
	for _, x := range list {
		if x == s {
			return true
		}
	}
	return false
}

func addModule(m map[string]*Module, k string, v *Module) map[string]*Module {
	if m == nil {
		m = map[string]*Module{}
	}
	m[k] = v
	return m
}

// sdkLibModule builds one component's properties as createDroidstubs / stubsLibraryProps / topLevelStubsLibraryProps do.
func (g *Graph) sdkLibModule(lib *Module, p *Obj, s *apiScope, kind, name string) *Module {
	props := newObj()
	set := func(k string, v Val) {
		if v != nil {
			props.set(k, v)
		}
	}
	list := func(xs ...[]string) Val {
		var out []Val
		for _, x := range xs {
			for _, e := range x {
				out = append(out, e)
			}
		}
		if out == nil {
			return nil
		}
		return out
	}
	str := func(k string) Val {
		if v := p.str(k); v != "" {
			return v
		}
		return nil
	}
	set("name", name)
	scopeProps := p.obj(s.prop)
	stem := p.str("dist_stem")
	if stem == "" {
		stem = lib.Name
	}
	switch kind {
	case "droidstubs":
		set("srcs", list(p.strs("srcs"), p.strs("api_srcs")))
		set("sdk_version", str("sdk_version"))
		set("system_modules", str("system_modules"))
		set("java_version", str("java_version"))
		set("libs", list(p.strs("libs"), p.strs("static_libs"), p.strs("stub_only_libs"), scopeProps.strs("libs")))
		surface := scopeProps.str("api_surface")
		if surface == "" {
			surface = s.name
		}
		set("api_surface", surface)
		if p.boolean("annotations_enabled", false) {
			set("annotations_enabled", true)
		}
		set("merge_annotations_dirs", list(p.strs("merge_annotations_dirs")))
		set("merge_inclusion_annotations_dirs", list(p.strs("merge_inclusion_annotations_dirs")))
		var args []string
		if pk := p.strs("api_packages"); len(pk) > 0 {
			args = append(args, "--stub-packages "+strings.Join(pk, ":"))
		}
		args = append(args, p.strs("droiddoc_options")...)
		disabled := []string{"HiddenSuperclass"}
		if p.obj("api_lint").boolean("legacy_errors_allowed", true) {
			disabled = append(disabled, "BroadcastBehavior", "DeprecationMismatch", "MissingPermission", "SdkConstant", "Todo")
		}
		for _, w := range disabled {
			args = append(args, "--hide "+w)
		}
		args = append(args, s.droidstubsArgs()...)
		set("args", strings.Join(args, " "))
		if s == scopePublic {
			set("output_javadoc_comments", true)
		}
		if !s.unstable {
			set("previous_api", ":"+stem+".api.combined."+s.name+".latest")
		}
		// check_api.current: what the droidstubs' .api.contribution names (createApiContribution)
		current := newObj()
		current.set("api_file", filepath.Join(sdkLibApiDir(p), s.filePrefix+"current.txt"))
		checkAPI := newObj()
		checkAPI.set("current", current)
		set("check_api", checkAPI)
		return &Module{Type: "droidstubs", Name: name, Dir: lib.Dir, Props: props}
	case "from-source":
		stubsLibProps(set, list, str, p, s)
		set("srcs", list([]string{":" + lib.Name + ".stubs.source" + s.suffix}))
	case "from-text":
		var contribs []string
		for x := s; x != nil; x = x.extends {
			contribs = append(contribs, lib.Name+".stubs.source"+x.suffix+".api.contribution")
		}
		if extra, ok := apiLibraryAdditionalContribution[lib.Name]; ok && s == scopePublic {
			contribs = append(contribs, extra)
		}
		set("api_contributions", list(contribs))
		set("libs", list([]string{"stub-annotations"}, p.strs("libs"), p.strs("static_libs"), p.strs("stub_only_libs"), scopeProps.strs("libs")))
		set("static_libs", list(p.strs("stub_only_static_libs")))
		set("system_modules", str("system_modules"))
		set("stubs_type", "everything")
		set("sdk_version", str("sdk_version"))
		if !s.unstable {
			set("previous_api", ":"+stem+".api.combined."+s.name+".latest")
		}
		return &Module{Type: "java_api_library", Name: name, Dir: lib.Dir, Props: props}
	case "top":
		set("sdk_version", sdkVersionForStubs(p, s))
		set("system_modules", str("system_modules"))
		if p.boolean("compile_dex", false) {
			set("compile_dex", true)
		}
		static := lib.Name + ".stubs" + s.suffix + ".from-source"
		if p.boolean("build_from_text_stub", true) && g.release != nil {
			// java_api_library: from-text stubs (BuildFromTextStub is on in this configuration)
			static = lib.Name + ".stubs" + s.suffix + ".from-text"
		}
		set("static_libs", list([]string{static}))
	}
	return &Module{Type: "java_library", Name: name, Dir: lib.Dir, Props: props}
}

// apiLibraryAdditionalContribution is sdk_library.go apiLibraryAdditionalProperties: public from-text stubs that need another
// library's API file too.
var apiLibraryAdditionalContribution = map[string]string{
	"legacy.i18n.module.platform.api": "i18n.module.public.api.stubs.source.api.contribution",
	"stable.i18n.module.platform.api": "i18n.module.public.api.stubs.source.api.contribution",
	"conscrypt.module.platform.api":   "conscrypt.module.public.api.stubs.source.api.contribution",
}

// sdkLibApiDir is getApiDir: api_dir, default "api".
func sdkLibApiDir(p *Obj) string {
	if d := p.str("api_dir"); d != "" {
		return d
	}
	return "api"
}

// apiScopeOrder is java.go scopeOrderMap: API surfaces narrowest first, then the core library surfaces.
var apiScopeOrder = map[string]int{"public": 0, "system": 1, "test": 2, "module-lib": 3, "system-server": 4,
	"core": 6, "core-platform": 7, "intra-core": 8, "core-platform-plus-public": 9, "core-platform-legacy": 10}

// apiContribution resolves a java_api_contribution: an explicit module, or the one every droidstubs creates as
// <droidstubs>.api.contribution (createApiContribution: its check_api.current.api_file and api_surface).
func (g *Graph) apiContribution(ref string, side Side, from string) (file, surface string, ok bool) {
	if m := g.lookupWith(ref, side, isJavaPrebuilt); m != nil && g.baseType(m.Type) == "java_api_contribution" {
		return filepath.Join(m.Dir, m.Props.str("api_file")), m.Props.str("api_surface"), true
	}
	stubs, isContrib := strings.CutSuffix(ref, ".api.contribution")
	if m := g.lookupJava(stubs, side); isContrib && m != nil && g.baseType(m.Type) == "droidstubs" {
		p := g.flattenJava(m, side)
		f := p.obj("check_api").obj("current").str("api_file")
		if f == "" {
			g.problem("%s: %s: droidstubs %s has no check_api.current.api_file", from, ref, stubs)
			return "", "", false
		}
		return filepath.Join(m.Dir, f), p.str("api_surface"), true
	}
	g.problem("%s: api contribution %s not found", from, ref)
	g.notFound(ref)
	return "", "", false
}

// apiLibrary is java_api_library (java.go ApiLibrary): metalava turns API text files into stubs (metalavaStubCmd), javac compiles
// them for Java 1.8 against the system modules' and bootclasspath jars, merge_zips adds static_libs. Header = that jar.
// Not run: validation against the droidstubs' current API, revert annotations (stubs_type other than everything is reported).
func (g *Graph) apiLibrary(n *jNode) {
	p := n.props
	j := g.java
	if st := p.str("stubs_type"); st != "everything" {
		g.problem("%s: stubs_type %q is unsupported", n.name, st)
	}
	type contrib struct{ file, surface string }
	var cs []contrib
	for _, r := range p.strs("api_contributions") {
		if f, s, ok := g.apiContribution(r, n.side, n.name); ok {
			cs = append(cs, contrib{f, s})
		}
	}
	slices.SortStableFunc(cs, func(a, b contrib) int { return apiScopeOrder[a.surface] - apiScopeOrder[b.surface] })
	s := parseSdk(p.str("sdk_version"))
	var sysJars, cp, boot []string
	var statics []string
	if n.side == Device {
		sd, err := g.decodeSdk(n, s)
		if err != nil {
			g.problem("%s: %v", n.name, err)
		}
		if sd.sysModules != "" {
			if sm := g.jnode(sd.sysModules, n.side, n.name); sm != nil {
				sysJars = sm.sysJars
			}
		}
		for _, r := range sd.classpath {
			if l := g.jnode(r, n.side, n.name); l != nil {
				cp = append(cp, l.header)
			}
		}
		for _, r := range sd.boot {
			if l := g.jnode(r, n.side, n.name); l != nil {
				if l.typ == "java_system_modules" {
					boot = append(boot, l.sysJars...)
				} else {
					boot = append(boot, l.header)
				}
			}
		}
	}
	for _, r := range p.strs("libs") {
		if l := g.jnode(r, n.side, n.name); l != nil && l.header != "" {
			cp = append(cp, l.header)
		}
	}
	for _, r := range p.strs("static_libs") {
		if l := g.jnode(r, n.side, n.name); l != nil && l.header != "" {
			statics = append(statics, l.header)
		}
	}
	md := filepath.Join(n.dir, "metalava")
	stubsDir := filepath.Join(md, "stubsDir")
	var files []string
	for _, c := range cs {
		files = append(files, g.abs(c.file))
	}
	args := []string{"--source-files", join(files), "--color", "--quiet", "--include-annotations",
		"--force-convert-to-warning-nullability-annotations", "+*:-android.*:+android.icu.*:-dalvik.*", "--repeat-errors-max", "10",
		"--hide", "UnresolvedImport", "--hide", "InvalidNullabilityOverride", "--hide", "ChangedDefault"}
	var implicit []string
	configFiles, _ := g.resolveSrc("build/soong/java/metalava", n.side, ":metalava-config-files", n.name)
	for _, f := range configFiles {
		args = append(args, "--config-file", esc(g.abs(f.path)))
		implicit = append(implicit, g.abs(f.path))
	}
	if sfc := p.str("api_surface"); sfc != "" {
		args = append(args, "--api-surface", sfc)
	}
	all := append(append(append([]string{}, sysJars...), cp...), boot...)
	if len(all) == 0 {
		args = append(args, "--api-class-resolution", "api")
	} else {
		args = append(args, "--api-class-resolution", "api:classpath", "--classpath", esc(strings.Join(all, ":")))
	}
	args = append(args, "--stubs", esc(stubsDir))
	if prev := p.str("previous_api"); prev != "" {
		if pf, ok := g.resolveSrc(n.mod.Dir, n.side, prev, n.name); ok {
			for _, f := range pf {
				args = append(args, "--migrate-nullness", esc(g.abs(f.path)))
				implicit = append(implicit, g.abs(f.path))
			}
		}
	}
	srcjar := filepath.Join(md, n.name+"-stubs.srcjar")
	implicit = append(append(implicit, all...), j.tools.Metalava)
	j.edge("metalava_text", []string{srcjar}, files, implicit, nil, [][2]string{
		{"args", strings.Join(args, " ")}, {"stubsdir", esc(stubsDir)}, {"home", esc(filepath.Join(md, "home"))},
	})
	// javac (getStubsJavaVersion = 1.8): system modules' and bootclasspath jars form the bootclasspath
	stubsJar := filepath.Join(md, "stubs.jar")
	g.javac(n, stubsJar, nil, []string{srcjar}, nil, p.strs("javacflags"), 8,
		javaDeps{classpath: cp, boot: append(append([]string{}, sysJars...), boot...)})
	n.header = filepath.Join(n.dir, n.name, n.name+".jar")
	j.edge("mergezips", []string{n.header}, append([]string{stubsJar}, statics...), nil, nil, nil)
	n.impl = []string{n.header}
	n.implRes = n.header
}

// stubsLibProps is stubsLibraryProps: the from-source stubs library compiles the droidstubs output for Java 1.8.
func stubsLibProps(set func(string, Val), list func(...[]string) Val, str func(string) Val, p *Obj, s *apiScope) {
	set("sdk_version", sdkVersionForStubs(p, s))
	set("system_modules", str("system_modules"))
	set("patch_module", str("patch_module"))
	libs := append(append([]string{}, p.strs("stub_only_libs")...), p.obj(s.prop).strs("libs")...)
	if p.boolean("annotations_enabled", false) {
		libs = append(libs, "stub-annotations")
	}
	set("libs", list(libs))
	set("static_libs", list(p.strs("stub_only_static_libs")))
	if o := p.obj("openjdk9"); o != nil {
		set("openjdk9", o)
	}
	set("java_version", "1.8")
}

// sdkVersionForStubs is sdkVersionForStubsLibrary: the scope's SDK when the library builds against standard libraries, else none.
func sdkVersionForStubs(p *Obj, s *apiScope) string {
	if sv := p.str("sdk_version"); sv == "none" || sv == "core_platform" {
		return "none"
	}
	return s.sdkVersion
}

// droidstubs: metalava over the module's sources against its SDK and libs (droidstubs.go everythingStubCmd); n.srcjar = the stubs.
func (g *Graph) droidstubs(n *jNode) {
	p := n.props
	j := g.java
	if !p.boolean("generate_stubs", true) || p.boolean("create_doc_stubs", false) {
		g.problem("%s: droidstubs without stubs (generate_stubs false / create_doc_stubs) is unsupported", n.name)
		return
	}
	if len(p.strs("arg_files")) > 0 || strings.Contains(p.str("args"), "$(") {
		g.problem("%s: droidstubs arg_files / $(location) in args are unsupported", n.name)
	}
	s := parseSdk(p.str("sdk_version"))
	ver := g.defaultJavaVersion(n.side, s)
	if v := p.str("java_version"); v != "" {
		ver = parseJavaVersion(v)
	}
	d, _ := g.collectJavaDeps(n, s)
	java, srcjars, waits := g.javaSrcs(n, p.strs("srcs"))
	cp := append(append([]string{}, d.boot...), d.classpath...)
	var implicit []string
	implicit = append(append(implicit, cp...), srcjars...)
	ev := filepath.Join(n.dir, "everything")
	stubsDir := filepath.Join(ev, "stubsDir")
	var args []string
	args = append(args, "--java-source", ver.String())
	// @rsp and @srcjar list come here (rule)
	tail := []string{}
	if len(cp) > 0 {
		tail = append(tail, "--classpath", esc(strings.Join(cp, ":")))
	}
	tail = append(tail, j.cfg.MetalavaFlags...)
	configFiles, cfOK := g.resolveSrc("build/soong/java/metalava", n.side, ":metalava-config-files", n.name)
	if !cfOK {
		g.problem("%s: metalava config files unresolved", n.name)
	}
	for _, f := range configFiles {
		tail = append(tail, "--config-file", esc(g.abs(f.path)))
		implicit = append(implicit, g.abs(f.path))
	}
	if sfc := p.str("api_surface"); sfc != "" {
		tail = append(tail, "--api-surface", sfc)
	}
	tail = append(tail, "--api", esc(filepath.Join(ev, n.name+"_api.txt")), "--removed-api", esc(filepath.Join(ev, n.name+"_removed.txt")),
		"--stubs", esc(stubsDir))
	if !p.boolean("output_javadoc_comments", false) {
		tail = append(tail, "--exclude-documentation-from-stubs")
	}
	if p.boolean("annotations_enabled", false) {
		tail = append(tail, j.cfg.MetalavaAnno...)
		if prev := p.str("previous_api"); prev != "" {
			files, ok := g.resolveSrc(n.mod.Dir, n.side, prev, n.name)
			if ok {
				for _, f := range files {
					tail = append(tail, "--migrate-nullness", esc(g.abs(f.path)))
					implicit = append(implicit, g.abs(f.path))
				}
			}
		}
		tail = append(tail, "--extract-annotations", esc(filepath.Join(ev, n.name+"_annotations.zip")))
		for _, r := range p.strs("merge_annotations_dirs") {
			if e := g.jnode(r, n.side, n.name); e != nil {
				tail = append(tail, "--merge-qualifier-annotations", esc(e.expDir))
				implicit = append(implicit, e.expDirDeps...)
			}
		}
		tail = append(tail, j.cfg.MetalavaAnnoW...)
	}
	// merged annotation directories are read inside the sandbox (sandboxed inputs, like the sources)
	var extra []string
	for _, r := range p.strs("merge_inclusion_annotations_dirs") {
		if e := g.jnode(r, n.side, n.name); e != nil {
			tail = append(tail, "--merge-inclusion-annotations", esc(g.sboxRel(e.expDir)))
			implicit = append(implicit, e.expDirDeps...)
			for _, f := range e.expDirDeps {
				extra = append(extra, g.sboxRel(f))
			}
		}
	}
	if a := p.str("args"); a != "" {
		tail = append(tail, strings.ReplaceAll(a, "$", "$$"))
	}
	if j.tools.Metalava == "" {
		g.problem("%s: no metalava.jar given (-metalava)", n.name)
	} else {
		implicit = append(implicit, j.tools.Metalava)
	}
	n.srcjar = filepath.Join(ev, n.name+"-stubs.srcjar")
	rel := make([]string, len(java))
	for i, f := range java {
		rel[i] = esc(g.sboxRel(g.abs(f)))
	}
	j.edge("metalava", []string{n.srcjar}, absAll(g, java), implicit, waits, [][2]string{
		{"pre", strings.Join(args, " ")}, {"post", strings.Join(tail, " ")}, {"stubsdir", esc(stubsDir)},
		{"home", esc(filepath.Join(ev, "home"))}, {"srcjardir", esc(filepath.Join(ev, "srcjars"))}, {"srcjars", join(srcjars)},
		{"sbox", esc(filepath.Join(ev, "sbox"))}, {"sources", strings.Join(rel, " ")}, {"extra", strings.Join(mapStrings(extra, esc), " ")},
	})
}

// sboxRel: an input's path inside a metalava sandbox, Soong's sbox layout: tree files tree-relative, build outputs below out/.
func (g *Graph) sboxRel(abs string) string {
	if r, ok := strings.CutPrefix(abs, g.out+"/"); ok {
		return filepath.Join("out", r)
	}
	if r, ok := strings.CutPrefix(abs, g.tree+"/"); ok {
		return r
	}
	return abs
}

// stubsRules: the metalava rules (droidstubs.go metalavaCmd + the soong_zip of the stubs directory, java.go metalavaStubCmd).
// droidstubs runs in a sandbox holding only its declared inputs, hard-linked, as Soong's sbox (SandboxInputs) does: metalava infers
// source roots from its sources and would otherwise also read every undeclared class below them in the tree (java.lang.constant
// next to libcore's API files turns hidden references into errors).
func (j *javaGraph) stubsRules(g *Graph, unpack string) string {
	vm := strings.ReplaceAll(strings.Join(j.cfg.JavacVmFlags, " ")+" "+j.cfg.MetalavaAddOpen, "-J", "")
	return fmt.Sprintf(`metalava = %s
outroot = %s
rule metalava
  command = rm -rf $stubsdir $home $srcjardir $sbox && mkdir -p $stubsdir $home $srcjardir $sbox && %s && for r in $$(cat $out.rsp) $extra; do case $$r in out/*) s=$outroot/$${r#out/};; *) s=$root/$$r;; esac; mkdir -p $sbox/$$(dirname $$r) && { ln -f $$s $sbox/$$r 2>/dev/null || cp $$s $sbox/$$r; } || exit 1; done && cd $sbox && ANDROID_PREFS_ROOT=$home $jdk/bin/java %s -jar $metalava $pre @$out.rsp @$srcjardir/list $post && $soongzip -write_if_changed -jar -o $out -C $stubsdir -D $stubsdir && rm -rf $srcjardir $sbox
  rspfile = $out.rsp
  rspfile_content = $sources
  restat = 1
  description = metalava $out
rule metalava_text
  command = rm -rf $stubsdir $home && mkdir -p $stubsdir $home && ANDROID_PREFS_ROOT=$home $jdk/bin/java %s -jar $metalava $args && $soongzip -write_if_changed -jar -o $out -C $stubsdir -D $stubsdir
  restat = 1
  description = metalava text $out
rule mergezips
  command = $mergezips $out $in
  description = merge $out
`, esc(j.tools.Metalava), esc(g.out), unpack, vm, vm)
}
