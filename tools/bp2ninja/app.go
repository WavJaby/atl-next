// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
)

// android_library, android_app, android_test, android_test_helper_app and the resources of android_library_import, following build/soong/java
// aar.go (aapt.buildActions, aaptLibs, AndroidLibrary, AARImport), app.go (AndroidApp.generateAndroidBuildActions, aaptBuildActions,
// dexBuildActions), app_builder.go (CreateAndSignAppPackage, TransformJniLibsToJar), android_manifest.go (ManifestFixer, manifestMerger) and aapt2.go
// (the spec). One resNode per module is what Soong's resourcesNode is; the depsets carry the transitive static libraries in Soong's order (depset.go).
//   - Resources: aapt2 compile per resource dir in shards, `aapt2 link` once per module. A library links `--static-lib --merge-only`; with
//     ResourceProcessorBusyBox (use_resource_processor, default) its dependencies are imports (`-I`) and an R.jar per module is generated from the
//     R.txt; an app links every transitive static library's package as an overlay and its busybox R.jar carries the final ids of every package.
//     Without busybox the link writes R.java (`--java`) into an R.srcjar compiled with the module.
//   - Manifests: manifest_fixer on the module's, manifest-merger over the transitive static libraries' (not merged for a library: the app does).
//   - An app's classes are dexed by javaDex (d8; the framework-res apk has none); the apk is merge_zips of the dex jar, the resource package and
//     the JNI zip, then zipalign -p 4 where Soong signs. NOT SIGNED: nothing here verifies a signature (the package manager is atld's, which
//     reads the manifest), and signapk needs its host JNI library built.
//   - Not modelled: product characteristics / RRO / overlays, the asset-dir hash comment, uses-library class loader contexts beyond uses_libs
//     and optional_uses_libs, extra_packages, proguard, resource shrinking, split packages, the tradefed test config, test data installs.
//     `instrumentation_for` and the properties listed in appUnsupported are reported as problems.

// aaptOut is what Soong's aapt struct hands the module's dependents and its apk.
type aaptOut struct {
	pkg      string // exportPackage: package-res.apk
	manifest string // manifestPath: the manifest_fixer output, or for an app the merged one
	merged   string // mergedManifestFile
	rTxt     string // aapt2's --output-text-symbols
	rJar     string // ResourceProcessorBusyBox's classes jar ("" without it)
	srcjar   string // R.java srcjar ("" with busybox)
	assets   string // assets.zip ("" without -A)
	busybox  bool
	// ResourcesNodeDepSet, ManifestsDepSet
	nodes     *depSet[*resNode]
	manifests *depSet[string]
	// transitiveAaptRJars: this module's R.jar first, then its direct dependencies' and theirs
	rJars []string
	// FlagsPackages.AconfigTextFiles
	aconfig []string
	// package_splits: the split resource packages
	splits []aaptSplit
}

// aaptSplit is one package_splits entry: the suffix of its apk's name and its resource package.
type aaptSplit struct{ suffix, path string }

// resNode is Soong's resourcesNode.
type resNode struct {
	pkg, manifest, rTxt, rJar, assets string
	busybox                           bool
}

// appUnsupported are app properties the converter does not model (reported when set).
var appUnsupported = []string{"generate_product_characteristics_rro", "privapp_allowlist", "use_embedded_dex", "embed_notices",
	"package_name", "instrumentation_for", "instrumentation_target_package"}

// androidTypes: module types built by android nodes (java.go jnode).
var androidTypes = map[string]bool{"android_library": true, "android_app": true, "android_test": true, "android_test_helper_app": true}

// isApp: the types that package an apk.
func isApp(t string) bool {
	return t == "android_app" || t == "android_test" || t == "android_test_helper_app"
}

// frameworkResFor: sdkDep.frameworkResModule is "framework-res" for the SDK kinds backed by modules that have framework libraries (sdk.go
// decodeSdkDep: private, public, system, test, module, system_server), not for none / core_platform / core nor a numbered SDK (its android.jar is
// linked with -I instead, sdkDep.jars).
func frameworkResFor(s sdkSpec) bool {
	if s.level != "" {
		return false
	}
	switch s.kind {
	case "private", "public", "system", "test", "test_frameworks_core", "module", "system_server":
		return true
	}
	return false
}

// effectiveSdk is ApiLevel.EffectiveVersionString: a number as it is, anything else (current, private, none) what DefaultAppTargetSdk is, the
// platform SDK once final, else the codename.
func (g *Graph) effectiveSdk(level string) string {
	if _, err := strconv.Atoi(level); err == nil {
		return level
	}
	if v, final := g.release.PlatformSdk(); final {
		return v
	}
	return strings.TrimSpace(g.release.Flag("RELEASE_PLATFORM_VERSION_CODENAME"))
}

// effectiveSdkInt is ApiLevel.EffectiveVersion().FinalOrFutureInt(): the number, the platform SDK once final, else the future level.
func (g *Graph) effectiveSdkInt(level string) int {
	v, _ := strconv.Atoi(g.effectiveSdk(level))
	if v == 0 {
		v, _ = strconv.Atoi(futureApiLevel)
	}
	return v
}

// aaptOpts is aaptBuildActionOptions plus the aapt fields the module type sets before buildActions.
type aaptOpts struct {
	library        bool
	s              sdkSpec
	extraLink      []string
	hasNoCode      bool
	usesNonSdkApis bool
	embedNative    bool
}

// aaptDeps is aaptLibs' result.
type aaptDeps struct {
	static, shared *depSet[*resNode]
	manifests      *depSet[string]
	sharedLibs     []string // -I inputs: an SDK's android.jar, the libs' packages, framework-res
	staticLibs     []*jNode // direct static_libs that are android libraries, in order
	aconfig        []string
}

// aaptLibs is aaptLibs (aar.go): the direct dependencies' resources in Soong's visit order (libs, static_libs, then framework-res), static ones
// reversed into the depset because "the highest priority dependency has to be listed last".
func (g *Graph) aaptLibs(n *jNode, s sdkSpec) aaptDeps {
	var d aaptDeps
	p := n.props
	if s.level != "" {
		if sd, err := g.decodeSdk(n, s); err == nil {
			for _, j := range sd.prebuiltJars {
				d.sharedLibs = append(d.sharedLibs, g.abs(j))
			}
		}
	}
	var staticSets, sharedSets []*depSet[*resNode]
	var manifestSets []*depSet[string]
	for _, r := range p.strs("libs") {
		if l := g.jnode(r, n.side, n.name); l != nil && l.aapt != nil {
			sharedSets = append(sharedSets, l.aapt.nodes)
			d.sharedLibs = append(d.sharedLibs, l.aapt.pkg)
		}
	}
	for _, r := range p.strs("static_libs") {
		if l := g.jnode(r, n.side, n.name); l != nil && l.aapt != nil {
			staticSets = append(staticSets, l.aapt.nodes)
			manifestSets = append(manifestSets, l.aapt.manifests)
			d.staticLibs = append(d.staticLibs, l)
			d.aconfig = append(d.aconfig, l.aapt.aconfig...)
		}
	}
	if frameworkResFor(s) {
		if pkg := g.frameworkResPackage(n.name); pkg != "" {
			d.sharedLibs = append(d.sharedLibs, pkg)
		}
	}
	slices.Reverse(staticSets)
	slices.Reverse(sharedSets)
	d.static = newDepSet(nil, staticSets)
	d.shared = newDepSet(nil, sharedSets)
	d.manifests = newDepSet(nil, manifestSets)
	return d
}

// frameworkResPackage is the framework-res dependency (frameworkResTag): its resource package, "" with a problem when it cannot be built.
func (g *Graph) frameworkResPackage(from string) string {
	n := g.jnode("framework-res", Device, from)
	if n == nil || n.aapt == nil {
		return ""
	}
	return n.aapt.pkg
}

// aconfigPaths is getAconfigFilePaths: the static libraries' flag packages, then this module's flags_packages, each once.
func (g *Graph) aconfigPaths(n *jNode, inherited []string) []string {
	paths := slices.Clone(inherited)
	for _, d := range n.props.strs("flags_packages") {
		if t := g.aconfigText(d, n.name); t != "" {
			paths = append(paths, t)
		}
	}
	return dedupKeep(paths)
}

// hostCmd is the command words and files of a host tool module run as a script or program (genTool's resolution): "" with a problem when it
// cannot run.
func (g *Graph) hostCmd(from, name string) (cmd string, deps []string) {
	n := &Node{key: key{name: from, side: Device, kind: Gen}}
	r := &genRule{}
	l := g.genTool(n, r, name, func(format string, args ...any) { g.problem("%s: "+format, append([]any{from}, args...)...) })
	if l.err != "" {
		return "", nil
	}
	deps = slices.Clone(r.implicit)
	for _, t := range r.tools {
		deps = append(deps, g.nodeOut(t))
	}
	words := (&genExpander{r: r}).words(l)
	for i := range deps {
		deps[i] = g.abs(deps[i])
	}
	if l.interp != "" {
		// the script by absolute path: the edge does not run from the tree root
		words = []string{l.interp + " " + shellEscape(g.abs(l.files[0]))}
	}
	return strings.Join(words, " "), deps
}

// manifestFixer is ManifestFixer: the module's manifest through manifest_fixer.py, the SDK levels and flags as Soong passes them.
func (g *Graph) manifestFixer(n *jNode, manifest string, o aaptOpts) string {
	p := n.props
	var args []string
	minSdk := p.str("min_sdk_version")
	if minSdk == "" {
		minSdk = o.s.level
	}
	if o.library {
		args = append(args, "--library")
	} else if g.effectiveSdkInt(minSdk) >= 23 {
		args = append(args, fmt.Sprintf("--extract-native-libs=%v", !o.embedNative))
	} else if o.embedNative {
		g.problem("%s: module attempted to store uncompressed native libraries, but minSdkVersion=%s doesn't support it", n.name, g.effectiveSdk(minSdk))
	}
	if o.usesNonSdkApis {
		args = append(args, "--uses-non-sdk-api")
	}
	// ClassLoaderContexts.UsesLibs() of the top context: uses_libs, then optional_uses_libs that exist in the tree
	for _, l := range p.strs("uses_libs") {
		args = append(args, "--uses-library", l)
	}
	for _, l := range p.strs("optional_uses_libs") {
		if g.lookupJava(l, Device) != nil {
			args = append(args, "--optional-uses-library", l)
		}
	}
	if o.hasNoCode {
		args = append(args, "--has-no-code")
	}
	if lp := p.str("logging_parent"); lp != "" {
		args = append(args, "--logging-parent", lp)
	}
	target := p.str("target_sdk_version")
	if target == "" {
		target = o.s.level
	}
	// SdkSpec.EffectiveVersionString of a level that is none: private and none kinds are not numbers
	args = append(args, "--targetSdkVersion", g.effectiveSdk(target), "--minSdkVersion", g.effectiveSdk(minSdk),
		"--replaceMaxSdkVersionPlaceholder", strconv.Itoa(g.effectiveSdkInt(p.str("replace_max_sdk_version_placeholder"))), "--raise-min-sdk-version")
	tool, deps := g.hostCmd(n.name, "manifest_fixer")
	out := filepath.Join(n.dir, "manifest_fixer", "AndroidManifest.xml")
	g.java.edge("manifestfixer", []string{out}, []string{g.abs(manifest)}, deps, nil, [][2]string{{"tool", tool}, {"args", ninjaRaw(args)}})
	return out
}

// manifestMerger is manifestMerger: the main manifest over the static libraries' (--libs each), tools declarations removed for an app.
func (g *Graph) manifestMerger(n *jNode, main string, libs []string, library bool) string {
	var args []string
	if !library {
		args = append(args, "--remove-tools-declarations")
	}
	if id := n.props.obj("manifest_values").str("applicationId"); id != "" {
		args = append(args, "--property PACKAGE="+id)
	}
	var libFlags []string
	for _, l := range libs {
		libFlags = append(libFlags, "--libs "+esc(l))
	}
	tool := g.jtool("manifest-merger", n.name)
	out := filepath.Join(n.dir, "manifest_merger", "AndroidManifest.xml")
	g.java.edge("manifestmerger", []string{out}, []string{main}, append([]string{tool}, libs...), nil,
		[][2]string{{"tool", esc(tool)}, {"args", ninjaRaw(args)}, {"libs", strings.Join(libFlags, " ")}})
	return out
}

// aaptBuild is aapt.buildActions for n: manifest fix and merge, resource compile and link, R classes. The edges are written; the result is
// n.aapt.
func (g *Graph) aaptBuild(n *jNode, o aaptOpts) *aaptOut {
	p := n.props
	j := g.java
	deps := g.aaptLibs(n, o.s)
	a := &aaptOut{busybox: g.useBusybox(n)}
	n.aapt = a

	// the manifest
	manifest := filepath.Join(n.mod.Dir, "AndroidManifest.xml")
	if mf := p.str("manifest"); mf != "" {
		manifest = filepath.Join(n.mod.Dir, mf)
	}
	fixed := g.manifestFixer(n, manifest, o)
	manifestPath := fixed
	var additional []string
	for _, f := range p.strs("additional_manifests") {
		additional = append(additional, g.abs(filepath.Join(n.mod.Dir, f)))
	}
	transitive := slices.Concat([]string{fixed}, additional, deps.manifests.toList())
	if len(transitive) > 1 && !p.boolean("dont_merge_manifests", false) {
		a.merged = g.manifestMerger(n, transitive[0], transitive[1:], o.library)
		if !o.library {
			manifestPath = a.merged
		}
	} else {
		a.merged = fixed
	}
	a.manifest = manifestPath

	// aapt2Flags
	aaptflags := p.strs("aaptflags")
	level := g.effectiveSdk(p.str("min_sdk_version"))
	if p.str("min_sdk_version") == "" {
		level = g.effectiveSdk(o.s.level)
	}
	var link, compile []string
	var linkDeps, aconfigOrder []string
	link = append(link, aaptflags...)
	link = append(link, "--enable-compact-entries")
	assetDirs := p.strs("asset_dirs")
	if len(assetDirs) == 0 {
		if ok, _ := globDirExists(g, filepath.Join(n.mod.Dir, "assets")); ok {
			assetDirs = []string{"assets"}
		}
	}
	if len(p.strs("assets")) > 0 {
		g.problem("%s: assets is unsupported (individual asset files need a temporary asset dir)", n.name)
	}
	link = append(link, "--manifest", shellEscape(manifestPath))
	linkDeps = append(linkDeps, manifestPath)
	var assetArgs []string
	for _, ad := range assetDirs {
		dir := filepath.Join(n.mod.Dir, ad)
		assetArgs = append(assetArgs, "-A", shellEscape(g.abs(dir)))
		for _, f := range g.resourceGlob(dir) {
			linkDeps = append(linkDeps, g.abs(f))
		}
	}
	link = append(link, assetArgs...)
	link = append(link, "--min-sdk-version", level, "--target-sdk-version", level)
	if !slices.ContainsFunc(aaptflags, func(f string) bool { return strings.HasPrefix(f, "--version-code") }) {
		link = append(link, "--version-code", g.release.Flag("RELEASE_PLATFORM_SDK_VERSION"))
	}
	if !slices.ContainsFunc(aaptflags, func(f string) bool { return strings.HasPrefix(f, "--version-name") }) {
		name := g.release.Flag("RELEASE_PLATFORM_SDK_VERSION")
		if n.name == "framework-res" {
			name = g.release.Flag("RELEASE_PLATFORM_VERSION")
		}
		link = append(link, "--version-name", shellEscape(name))
	}
	// FilterListByPrefix: --legacy and --png-compression-level are compile flags
	var linkOnly []string
	for _, f := range link {
		if strings.HasPrefix(f, "--legacy") || strings.HasPrefix(f, "--png-compression-level") {
			compile = append(compile, f)
		} else {
			linkOnly = append(linkOnly, f)
		}
	}
	link = linkOnly
	compile = append(compile, "--pseudo-localize")

	// aconfig: the static libraries', then this module's flags_packages; compile takes them sorted, link as listed
	aconfigOrder = g.aconfigPaths(n, deps.aconfig)
	a.aconfig = aconfigOrder
	sortedAconfig := slices.Clone(aconfigOrder)
	slices.Sort(sortedAconfig)
	var compileFF, linkFF []string
	for _, f := range sortedAconfig {
		compileFF = append(compileFF, "--feature-flags", "@"+shellEscape(f))
	}
	for _, f := range aconfigOrder {
		linkFF = append(linkFF, "--feature-flags", "@"+shellEscape(f))
	}

	staticNodes := deps.static.toList()
	sharedNodes := deps.shared.toList()
	// libFlags: --auto-add-overlay when there are static library resources, then -I of each shared package
	if len(staticNodes) > 0 {
		link = append(link, "--auto-add-overlay")
	}
	for _, l := range deps.sharedLibs {
		link = append(link, "-I", shellEscape(l))
		linkDeps = append(linkDeps, l)
	}
	link = append(link, o.extraLink...)
	if o.library {
		link = append(link, "--static-lib")
	}
	link = append(link, "--no-static-lib-packages")
	if o.library {
		link = append(link, "--merge-only")
	}
	for _, nd := range staticNodes {
		linkDeps = append(linkDeps, nd.pkg)
	}

	// compile the resources
	resDirs := p.strs("resource_dirs")
	if len(resDirs) == 0 {
		if ok, _ := globDirExists(g, filepath.Join(n.mod.Dir, "res")); ok {
			resDirs = []string{"res"}
		}
	}
	aapt2 := g.jhost("aapt2", n.name)
	aapt2Deps := []string{aapt2}
	var compiled [][]string
	for _, rd := range resDirs {
		dir := filepath.Join(n.mod.Dir, rd)
		files := g.resourceGlob(dir)
		var outs []string
		for i, shard := range shards(files, aapt2ShardSize) {
			var shardOuts []string
			for _, f := range shard {
				shardOuts = append(shardOuts, filepath.Join(n.dir, "aapt2", aapt2Path(f)))
			}
			outs = append(outs, shardOuts...)
			j.edge("aapt2compile", shardOuts, absAll(g, shard), slices.Concat(aapt2Deps, aconfigOrder), nil, [][2]string{
				{"aapt2", esc(aapt2)}, {"outdir", esc(filepath.Join(n.dir, "aapt2", dir))}, {"cflags", ninjaRaw(slices.Concat(compile, compileFF))},
				{"tmpdir", esc(filepath.Join(n.dir, "aapt2-tmp", dir, strconv.Itoa(i)))},
			})
		}
		slices.Sort(outs)
		compiled = append(compiled, outs)
	}
	for i, z := range p.strs("resource_zips") {
		files, ok := g.resolveSrc(n.mod.Dir, n.side, z, n.name)
		if !ok {
			g.problem("%s: resource_zips %s unresolved", n.name, z)
		}
		for _, f := range files {
			flata := filepath.Join(n.dir, fmt.Sprintf("reszip.%d.flata", i))
			g.aapt2CompileZip(n, flata, g.abs(f.path), "", compile, aapt2)
			compiled = append(compiled, []string{flata})
		}
	}

	// which compiled files are resources and which overlays (buildActions)
	transitiveStaticLibs := make([]string, 0, len(staticNodes))
	for _, nd := range staticNodes {
		transitiveStaticLibs = append(transitiveStaticLibs, nd.pkg)
	}
	slices.Reverse(transitiveStaticLibs)
	var compiledRes, compiledOverlay []string
	if !(o.library && a.busybox) {
		compiledOverlay = append(compiledOverlay, transitiveStaticLibs...)
	} else {
		// a library with busybox takes its static libraries' packages as imports, after --merge-only
		for _, nd := range staticNodes {
			link = append(link, "-I", shellEscape(nd.pkg))
		}
	}
	switch {
	case len(transitiveStaticLibs) > 0:
		for _, c := range compiled {
			compiledOverlay = append(compiledOverlay, c...)
		}
	case o.library:
		for _, c := range compiled {
			compiledRes = append(compiledRes, c...)
		}
	case len(compiled) > 0:
		compiledRes = append(compiledRes, compiled[0]...)
		for _, c := range compiled[1:] {
			compiledOverlay = append(compiledOverlay, c...)
		}
	}

	// R.srcjar without busybox: the srcjar's `android` subdir keeps the package filter (PathForModuleGen)
	a.rTxt = filepath.Join(n.dir, "R.txt")
	proguard := filepath.Join(n.dir, "gen", "proguard.options")
	if !a.busybox {
		a.srcjar = filepath.Join(n.dir, "gen", "android", "R.srcjar")
	}
	var assetPkgs []string
	if !o.library {
		for _, nd := range staticNodes {
			if nd.assets != "" {
				assetPkgs = append(assetPkgs, nd.assets)
			}
		}
		slices.Reverse(assetPkgs)
	}
	a.pkg = filepath.Join(n.dir, "package-res.apk")
	// package_splits: one `--split <path>:<name>` each, the split packages implicit outputs of the link (a name's commas become `_`)
	var splitPaths []string
	for _, s := range p.strs("package_splits") {
		sp := filepath.Join(n.dir, "package_"+strings.ReplaceAll(s, ",", "_")+".apk")
		link = append(link, "--split", shellEscape(sp)+":"+s)
		splitPaths = append(splitPaths, sp)
		a.splits = append(a.splits, aaptSplit{suffix: strings.ReplaceAll(s, ",", "_"), path: sp})
	}
	g.aapt2Link(n, a, link, linkFF, linkDeps, compiledRes, compiledOverlay, assetPkgs, splitPaths, proguard, aapt2, aapt2Deps)
	if len(assetArgs) > 0 {
		a.assets = filepath.Join(n.dir, "assets.zip")
		j.edge("extractassets", []string{a.assets}, []string{a.pkg}, nil, nil, nil)
	}

	// R classes
	var transitiveRJars []string
	if o.library && a.busybox {
		for _, nd := range sharedNodes {
			if nd.busybox {
				transitiveRJars = append(transitiveRJars, nd.rJar)
			}
		}
		for _, nd := range staticNodes {
			if nd.busybox {
				transitiveRJars = append(transitiveRJars, nd.rJar)
			}
		}
	}
	if a.busybox {
		a.rJar = filepath.Join(n.dir, "busybox", "R.jar")
		g.busyboxR(n, a, a.rTxt, a.merged, a.rJar, staticNodes, o.library, aaptflags)
		transitiveRJars = append(transitiveRJars, a.rJar)
	}
	slices.Reverse(transitiveRJars)
	a.rJars = transitiveRJars

	own := &resNode{pkg: a.pkg, manifest: a.manifest, rTxt: a.rTxt, rJar: a.rJar, assets: a.assets, busybox: a.busybox}
	a.nodes = newDepSet([]*resNode{own}, []*depSet[*resNode]{deps.static})
	a.manifests = newDepSet(append([]string{a.manifest}, additional...), []*depSet[string]{deps.manifests})
	// "apps manifests are handled by aapt, don't let Module see them" (aar.go, app.go): `manifest` is no jar manifest here
	p.set("manifest", nil)
	return a
}

// aarImport is AARImport.GenerateAndroidBuildActions (aar.go): the aar unzipped (unzipAAR: classes and libs jars merged into <name>.jar, the
// manifest, R.txt, proguard.txt and an assets zip beside), its res/ compiled and linked as a static library, its R.jar from the aar's own R.txt.
// Returns the classes jar.
func (g *Graph) aarImport(n *jNode) (string, bool) {
	p := n.props
	j := g.java
	aars := p.strs("aars")
	if len(aars) != 1 {
		g.problem("%s: android_library_import needs exactly one aar, has %d", n.name, len(aars))
		return "", false
	}
	if p.boolean("jetifier", false) {
		g.problem("%s: jetifier is unsupported", n.name)
	}
	aar := g.abs(filepath.Join(n.mod.Dir, aars[0]))
	dir := filepath.Join(n.dir, "aar")
	classes := filepath.Join(dir, n.name+".jar")
	manifest := filepath.Join(dir, "AndroidManifest.xml")
	if mf := p.str("manifest"); mf != "" {
		manifest = g.abs(filepath.Join(n.mod.Dir, mf))
	}
	extractedRTxt := filepath.Join(dir, "R.txt")
	assets := filepath.Join(n.dir, "assets.zip")
	j.edge("unzipaar", []string{classes, filepath.Join(dir, "proguard.txt"), filepath.Join(dir, "AndroidManifest.xml"), assets, extractedRTxt}, []string{aar},
		[]string{g.abs(filepath.Join(g.java.tools.BuildTools, "zip2zip"))}, nil,
		[][2]string{{"outdir", esc(dir)}, {"assets", esc(assets)}, {"classes", esc(classes)}})

	s := parseSdk(p.str("sdk_version"))
	deps := g.aaptLibs(n, s)
	aapt2 := g.jhost("aapt2", n.name)
	flata := filepath.Join(n.dir, "flat-res", "gen_res.flata")
	g.aapt2CompileZip(n, flata, aar, "res", []string{"--pseudo-localize"}, aapt2)

	a := &aaptOut{busybox: true, assets: assets, manifest: manifest, merged: manifest, rTxt: filepath.Join(n.dir, "R.txt"), pkg: filepath.Join(n.dir, "package-res.apk")}
	n.aapt = a
	staticNodes := deps.static.toList()
	link := []string{"--static-lib", "--merge-only", "--auto-add-overlay", "--no-static-lib-packages", "--manifest", shellEscape(manifest)}
	linkDeps := []string{manifest}
	if len(staticNodes) > 0 {
		link = append(link, "--auto-add-overlay")
	}
	for _, l := range deps.sharedLibs {
		link = append(link, "-I", shellEscape(l))
		linkDeps = append(linkDeps, l)
	}
	var assetPkgs []string
	for _, nd := range staticNodes {
		link = append(link, "-I", shellEscape(nd.pkg))
		linkDeps = append(linkDeps, nd.pkg)
		if nd.assets != "" {
			assetPkgs = append(assetPkgs, nd.assets)
		}
	}
	slices.Reverse(assetPkgs)
	g.aapt2Link(n, a, link, nil, linkDeps, nil, []string{flata}, assetPkgs, nil, filepath.Join(n.dir, "gen", "proguard.options"), aapt2, []string{aapt2})

	a.rJar = filepath.Join(n.dir, "busybox", "R.jar")
	g.busyboxR(n, a, extractedRTxt, manifest, a.rJar, nil, true, nil)
	a.rJars = []string{a.rJar}
	own := &resNode{pkg: a.pkg, manifest: manifest, rTxt: extractedRTxt, rJar: a.rJar, assets: assets, busybox: true}
	a.nodes = newDepSet([]*resNode{own}, []*depSet[*resNode]{deps.static})
	a.manifests = newDepSet([]string{manifest}, []*depSet[string]{deps.manifests})
	return classes, true
}

// useBusybox is useResourceProcessorBusyBox: on unless use_resource_processor is false or the module links a shared library.
func (g *Graph) useBusybox(n *jNode) bool {
	return n.props.boolean("use_resource_processor", true) && !slices.Contains(n.props.strs("aaptflags"), "--shared-lib")
}

// aapt2CompileZip is aapt2CompileZip: the resources of a zip (below zipPrefix) compiled into one flata. zipsync's job is done by unzip,
// which extracts the prefix's entries below zipdir/<prefix>; a zip without any gives an empty directory, as zipsync's does.
func (g *Graph) aapt2CompileZip(n *jNode, flata, zip, zipPrefix string, cflags []string, aapt2 string) {
	zipDir := filepath.Join(n.dir, "aapt2", "reszip", filepath.Base(flata))
	pattern, dir := "'*'", zipDir
	if zipPrefix != "" {
		pattern, dir = shellEscape(zipPrefix+"/*"), filepath.Join(zipDir, zipPrefix)
	}
	g.java.edge("aapt2compilezip", []string{flata}, []string{zip}, []string{aapt2}, nil, [][2]string{
		{"aapt2", esc(aapt2)}, {"cflags", ninjaRaw(cflags)}, {"zipdir", esc(zipDir)}, {"pattern", pattern}, {"resdir", esc(dir)},
	})
}

// aapt2Link is aapt2Link (aapt2.go): the compiled resources as a list file, the overlays as -R, assets merged after the link when dependencies
// bring some, and with a.srcjar `--java` plus a soong_zip of its output.
func (g *Graph) aapt2Link(n *jNode, a *aaptOut, flags, featureFlags, deps, compiledRes, compiledOverlay, assetPkgs, splits []string, proguard, aapt2 string, aapt2Deps []string) {
	j := g.java
	var in []string
	if len(compiledRes) > 0 {
		list := filepath.Join(n.dir, "aapt2", "res.list")
		g.genFile(list, strings.Join(compiledRes, " "))
		deps = append(deps, compiledRes...)
		deps = append(deps, list)
		in = append(in, "@"+shellEscape(list))
	}
	if len(compiledOverlay) > 0 {
		list := filepath.Join(n.dir, "aapt2", "overlay.list")
		g.genFile(list, strings.Join(compiledOverlay, " "))
		deps = append(deps, compiledOverlay...)
		deps = append(deps, list)
		in = append(in, "-R", "@"+shellEscape(list))
	}
	linkOut := a.pkg
	if len(assetPkgs) > 0 {
		linkOut = filepath.Join(n.dir, "aapt2", "package-res.apk")
		j.edge("mergezips", []string{a.pkg}, append([]string{linkOut}, assetPkgs...), nil, nil, nil)
	}
	for _, f := range featureFlags {
		if f != "--feature-flags" {
			deps = append(deps, strings.TrimPrefix(f, "@"))
		}
	}
	in = append(in, featureFlags...)
	outs := append([]string{linkOut}, splits...)
	outs = append(outs, proguard, a.rTxt)
	vars := [][2]string{{"aapt2", esc(aapt2)}, {"pkg", esc(linkOut)}, {"flags", ninjaRaw(flags)}, {"proguard", esc(proguard)}, {"rtxt", esc(a.rTxt)},
		{"inflags", ninjaRaw(in)}}
	if a.srcjar != "" {
		javaDir := filepath.Join(n.dir, "gen", "aapt2", "R")
		outs = append(outs, a.srcjar)
		vars = append(vars, [2]string{"preamble", "rm -rf " + esc(javaDir) + " && "},
			[2]string{"postamble", " && $soongzip -write_if_changed -jar -o " + esc(a.srcjar) + " -C " + esc(javaDir) + " -D " + esc(javaDir) + " && rm -rf " + esc(javaDir)})
		vars[2][1] += " --java " + esc(javaDir)
	}
	j.edge("aapt2link", outs, nil, slices.Concat(aapt2Deps, deps), nil, vars)
}

// busyboxR is resourceProcessorBusyBoxGenerateBinaryR: R classes from R.txt and the manifest; an app passes every transitive static library's
// R.txt and manifest to regenerate theirs with the final ids, a library only its own with non-final fields.
func (g *Graph) busyboxR(n *jNode, a *aaptOut, rTxt, manifest, rJar string, statics []*resNode, library bool, aaptflags []string) {
	var args, deps []string
	if library {
		args = append(args, "--finalFields=false")
	} else {
		for _, s := range statics {
			args = append(args, "--library="+s.rTxt+","+s.manifest)
			deps = append(deps, s.rTxt, s.manifest)
		}
	}
	for i, f := range aaptflags {
		if rest, ok := strings.CutPrefix(f, "--custom-package"); ok {
			pkg := strings.TrimSpace(rest)
			if pkg == "" && i+1 < len(aaptflags) {
				pkg = aaptflags[i+1]
			}
			args = append(args, "--packageForR "+pkg)
		}
	}
	deps = append(deps, rTxt, manifest)
	jar := g.busyboxJar(n.name)
	g.java.edge("busybox", []string{rJar}, nil, append([]string{jar}, deps...), nil, [][2]string{
		{"rtxt", esc(rTxt)}, {"manifest", esc(manifest)}, {"args", ninjaRaw(args)}, {"rpbb", esc(jar)},
	})
}

// busyboxPath: ResourceProcessorBusyBox (java/config/config.go ResourceProcessorBusyBox), a prebuilt jar of the tree.
const busyboxPath = "prebuilts/bazel/common/android_tools/android_tools/all_android_tools_deploy.jar"

func (g *Graph) busyboxJar(from string) string {
	p := g.abs(busyboxPath)
	if _, err := statPath(p); err != nil {
		g.problem("%s: %s does not exist", from, busyboxPath)
		g.missingPaths = addKey(g.missingPaths, filepath.Dir(busyboxPath))
	}
	return p
}

// androidLibrary is AndroidLibrary (aar.go): the resources, then the library compiled with the R jars on its classpath (busybox) or the R
// srcjar among its sources.
func (g *Graph) androidLibrary(n *jNode) {
	s := parseSdk(n.props.str("sdk_version"))
	a := g.aaptBuild(n, aaptOpts{library: true, s: s})
	if a.busybox {
		g.javaLibWith(n, javaExtra{classpath: a.rJars})
	} else {
		g.javaLibWith(n, javaExtra{srcjars: []string{a.srcjar}})
	}
}

// androidApp is AndroidApp.generateAndroidBuildActions and AndroidTest / AndroidTestHelperApp: the resources, the code compiled against the R
// jar (which is also combined into the classes), dexed, and the apk packaged.
func (g *Graph) androidApp(n *jNode) {
	p := n.props
	for _, k := range appUnsupported {
		if p.get(k) != nil {
			g.problem("%s: %s is unsupported", n.name, k)
		}
	}
	s := parseSdk(p.str("sdk_version"))
	min := p.str("min_sdk_version")
	if min == "" {
		min = s.level
	}
	embed := g.effectiveSdkInt(min) >= 23 && p.boolean("use_embedded_native_libs", false)
	// aaptBuildActions: platform_apis and the module SDK use non-SDK APIs; hasCode is srcs or static_libs
	platformApis := p.boolean("platform_apis", false) || s.kind == "module"
	a := g.aaptBuild(n, aaptOpts{s: s, hasNoCode: !g.hasCode(n), usesNonSdkApis: platformApis, embedNative: embed})
	switch {
	case n.name == "framework-res":
		// dexBuildActions: framework-res has no code to compile (its srcs are resource inputs), so no classes and no dex
	case a.busybox:
		g.javaLibWith(n, javaExtra{classpath: []string{a.rJar}, combined: []string{a.rJar}})
	default:
		g.javaLibWith(n, javaExtra{srcjars: []string{a.srcjar}})
	}
	g.appPackage(n)
}

// hasCode is Module.hasCode (base.go): any file in srcs (of any kind: framework-res's are resource inputs) less exclude_srcs, or a static
// library.
func (g *Graph) hasCode(n *jNode) bool {
	if len(n.props.strs("static_libs")) > 0 {
		return true
	}
	exclude := g.excludeSet(n.mod.Dir, n.side, n.props.strs("exclude_srcs"), n.name)
	for _, s := range n.props.strs("srcs") {
		if isRef(s) {
			if _, ok := g.javaSrcRef(n, s); ok {
				return true
			}
		}
		files, _ := g.resolveSrc(n.mod.Dir, n.side, s, n.name)
		for _, f := range files {
			if !exclude[f.path] {
				return true
			}
		}
	}
	return false
}

// appPackage is CreateAndSignAppPackage (without the signature) and jniBuildActions: merge_zips of the dex jar, the resource package and the
// JNI zip into <name>-unsigned.apk, aligned into <name>.apk.
func (g *Graph) appPackage(n *jNode) {
	j := g.java
	name := n.name
	if stem := n.props.str("stem"); stem != "" {
		name = stem
	}
	var inputs []string
	if n.dex != "" {
		inputs = append(inputs, n.dex)
	}
	inputs = append(inputs, n.aapt.pkg)
	if jni := g.jniZip(n); jni != "" {
		inputs = append(inputs, jni)
	}
	unsigned := filepath.Join(n.dir, name+"-unsigned.apk")
	j.edge("mergezips", []string{unsigned}, inputs, nil, nil, nil)
	n.apk = filepath.Join(n.dir, name+".apk")
	j.edge("zipalign", []string{n.apk}, []string{unsigned}, nil, nil, nil)
	// a split is packaged alone (no dex, no JNI): <name>_<suffix>.apk, the module's extraOutputFiles
	for _, s := range n.aapt.splits {
		su := filepath.Join(n.dir, name+"_"+s.suffix+"-unsigned.apk")
		j.edge("mergezips", []string{su}, []string{s.path}, nil, nil, nil)
		apk := filepath.Join(n.dir, name+"_"+s.suffix+".apk")
		j.edge("zipalign", []string{apk}, []string{su}, nil, nil, nil)
		n.extraApks = append(n.extraApks, apk)
	}
}

// jniZip is TransformJniLibsToJar: the jni_libs shared libraries stored under lib/<abi>/ in one zip ("" without any).
func (g *Graph) jniZip(n *jNode) string {
	libs := n.props.strs("jni_libs")
	if len(libs) == 0 {
		return ""
	}
	if g.cfg.ABI == "" {
		g.problem("%s: jni_libs needs the device ABI (-abi)", n.name)
		return ""
	}
	var outs []string
	for _, l := range libs {
		if ln := g.node(l, Device, Shared, n.name); ln != nil {
			outs = append(outs, g.nodeOut(ln))
		}
	}
	return g.jniZipEdge(n, outs)
}

// jniZipEdge writes TransformJniLibsToJar's edge over the libraries' files: junked paths (-j), re-added under lib/<abi>/ with -P, stored
// uncompressed (-L 0) when the apk embeds native libraries (min_sdk_version >= 23 and use_embedded_native_libs).
func (g *Graph) jniZipEdge(n *jNode, libs []string) string {
	if len(libs) == 0 {
		return ""
	}
	args := []string{"-j"}
	if g.effectiveSdkInt(n.props.str("min_sdk_version")) >= 23 && n.props.boolean("use_embedded_native_libs", false) {
		args = append(args, "-L", "0")
	}
	for _, l := range libs {
		args = append(args, "-P", "lib/"+g.cfg.ABI, "-f", esc(l))
	}
	zip := filepath.Join(n.dir, "jnilibs.zip")
	g.java.edge("jnizip", []string{zip}, nil, libs, nil, [][2]string{{"args", strings.Join(args, " ")}})
	return zip
}

// appOutputs is what `:name{tag}` in srcs / java_resources names for an android app (app.go setOutputFiles: "" the apk and its splits, .apk the
// apk, .export-package.apk the resource package, .manifest.xml the manifest, .aapt.jar the R.jar, .aapt.srcjar the R srcjar) and for a prebuilt_apex
// (prebuilt.go: "" the apex file; its `src`, picked by the arch block of the device arch, stands for the copy Soong makes). handled=false: not one of
// those modules; files nil with handled: it has no such output (problem recorded).
func (g *Graph) appOutputs(name, tag string, side Side, from string) (files []srcFile, handled bool) {
	// lookupJava skips prebuilt_* modules (a source module of the name wins): a prebuilt_apex is found by any type
	m := g.lookupWith(name, side, func(string) bool { return false })
	if m == nil {
		return nil, false
	}
	t := g.baseType(m.Type)
	if t == "prebuilt_apex" && tag == "" {
		src := m.Props.obj("arch").obj(g.cfg.Arch).str("src")
		if src == "" {
			src = m.Props.str("src")
		}
		if src == "" {
			g.problem("%s: %s has no src for %s", from, m.Name, g.cfg.Arch)
			return nil, true
		}
		path := filepath.Join(m.Dir, src)
		return []srcFile{{path: path, root: m.Dir}}, true
	}
	if !isApp(t) || side != Device {
		return nil, false
	}
	jn := g.jnode(name, side, from)
	if jn == nil || jn.aapt == nil {
		return nil, true
	}
	var out []string
	switch tag {
	case "":
		out = append([]string{jn.apk}, jn.extraApks...)
	case ".apk":
		out = []string{jn.apk}
	case ".export-package.apk":
		out = []string{jn.aapt.pkg}
	case ".manifest.xml":
		out = []string{jn.aapt.manifest}
	case ".aapt.jar":
		out = []string{jn.aapt.rJar}
	case ".aapt.srcjar":
		out = []string{jn.aapt.srcjar}
	default:
		return nil, false
	}
	for _, o := range out {
		if o == "" {
			g.problem("%s: %s has no %s output", from, name, tag)
			return nil, true
		}
		files = append(files, srcFile{path: o, root: filepath.Dir(o)})
	}
	return files, true
}

// appRules are the rules the android modules' edges use; command shapes follow the Soong rules of the same names (aapt2.go aapt2Compile,
// aapt2CompileZip, aapt2Link, mergeAssets, extractAssets; android_manifest.go manifestFixer, manifestMerger; aar.go resourceProcessorBusyBox,
// unzipAAR; app_builder.go combineApk, TransformJniLibsToJar). mergeAssets and combineApk are stubs.go's `mergezips` (`merge_zips $out $in`).
// aapt2link restats: its R srcjar is written only if changed (javagen.go srcjar says why that needs restat).
// aapt2compile compiles into $tmpdir and moves each .flat over its output only when its bytes differ, then restats (Soong's aapt2Compile
// does not; exact as commitIfChanged): a rebuilt aapt2 that compiles the same bytes stops at the .flat files instead of relinking every app.
const appRules = `rule aapt2compile
  command = rm -rf $tmpdir && mkdir -p $tmpdir $outdir && $aapt2 compile -o $tmpdir $cflags $in && for o in $out; do t=$tmpdir/$${o##*/}; if cmp -s $$t $$o; then rm $$t; else mv $$t $$o || exit 1; fi; done && rm -rf $tmpdir
  restat = 1
  description = aapt2 compile $outdir
rule aapt2compilezip
  command = rm -rf $zipdir && mkdir -p $zipdir && (unzip -qo $in $pattern -d $zipdir || [ $$? -eq 11 ]) && mkdir -p $resdir && $aapt2 compile -o $out $cflags --dir $resdir
  description = aapt2 compile zip $out
rule aapt2link
  command = $preamble$aapt2 link -o $pkg $flags --proguard $proguard --output-text-symbols $rtxt $inflags$postamble
  restat = 1
  description = aapt2 link $pkg
rule manifestfixer
  command = PYTHONDONTWRITEBYTECODE=1 $tool $args $in $out
  description = fix manifest $out
rule manifestmerger
  command = $tool $args --main $in $libs --out $out
  description = merge manifest $out
rule busybox
  command = $jdk/bin/java -cp $rpbb com.google.devtools.build.android.ResourceProcessorBusyBox --tool=GENERATE_BINARY_R -- @$out.args && if cmp -s $out.tmp $out ; then rm $out.tmp ; else mv $out.tmp $out; fi
  rspfile = $out.args
  rspfile_content = --primaryRTxt $rtxt --primaryManifest $manifest --classJarOutput $out.tmp $args
  restat = 1
  description = ResourceProcessorBusyBox $out
rule extractassets
  command = $zip2zip -i $in -o $out "assets/**/*"
  description = extract assets $out
rule unzipaar
  command = rm -rf $outdir && mkdir -p $outdir && unzip -qoDD -d $outdir $in && rm -rf $outdir/res && touch $out && $zip2zip -i $in -o $assets 'assets/**/*' && $mergezips $classes $$(ls $outdir/classes.jar 2> /dev/null) $$(ls $outdir/libs/*.jar 2> /dev/null)
  description = unzip aar $in
rule jnizip
  command = $soongzip -o $out $args
  description = zip jni libs $out
`
