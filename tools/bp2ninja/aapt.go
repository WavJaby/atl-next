// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
)

// android_app resources: the `{.aapt.srcjar}` output (R.java and Manifest.java as `aapt2 link --java` writes them) and the app's resource-only
// apk (<name>.apk, framework-res's install), following build/soong/java aar.go aapt2Flags / buildActions, aapt2.go, android_manifest.go and
// app_builder.go (the spec). Built as one generated step (a genrule node), its script running:
//   - manifest_fixer (android_manifest.go ManifestFixer) on the manifest: --extract-native-libs=true (no embedded JNI libs, min SDK >= 23),
//     --targetSdkVersion / --minSdkVersion / --replaceMaxSdkVersionPlaceholder at the SDK's effective level, --raise-min-sdk-version,
//     --has-no-code without srcs and static_libs. Not modelled: uses-library (class loader contexts), logging_parent, default manifest
//     version, embedded dex / JNI: framework-res, the one app built, sets none.
//   - aapt2 compile per resource dir in shards of AAPT2_SHARD_SIZE sources (resource_dirs, default res; androidResourceGlob's exclusions),
//     with --pseudo-localize and --feature-flags @<text> of every flags_packages aconfig_declarations (sorted, unique);
//   - per resource_zips entry: unzip, then `aapt2 compile --dir` into reszip.<i>.flata (aapt2CompileZip);
//   - aapt2 link: aaptflags, --enable-compact-entries, --manifest (the fixed one), -A asset dirs (asset_dirs, default assets),
//     --min-sdk-version / --target-sdk-version (the SDK's effective level), --version-code (platform SDK), --version-name (framework-res: the
//     platform version name, RELEASE_PLATFORM_VERSION; else the platform SDK), --no-static-lib-packages, --java, then @res.list
//     (pathToAapt2Path order), -R each resource zip's flata, --feature-flags; soong_zip -jar of the --java dir is the srcjar.
//   - the apk (CreateAndSignAppPackage without a dex or JNI jar): merge_zips of package-res.apk into <name>-unsigned.apk, then zipalign -p 4
//     in place of signapk, which aligns as it signs. Not signed: signapk needs its host jar and conscrypt's host JNI library built, and
//     nothing reads framework-res's signature on this runtime yet (AssetManager reads the resource table).
//   - Dropped: the asset-dir hash comment, product characteristics (-c / --product: the apk's configs), overlays and RROs (this product has
//     none), R.txt, proguard options, dexpreopt and the signature.

// aapt2ShardSize: aapt2.go AAPT2_SHARD_SIZE.
const aapt2ShardSize = 100

// androidResourceIgnore: android_resources.go androidResourceIgnoreFilenames, as globs of a base name.
var androidResourceIgnore = []string{".svn", ".git", ".ds_store", "*.scc", ".*", "CVS", "thumbs.db", "picasa.ini", "*~"}

var flagsPathRe = regexp.MustCompile(`flag\(!?([a-zA-Z_-]+\.)*[a-zA-Z0-9_-]+\)`)

// aapt2Path is pathToAapt2Path below the compile output root: values-*/<f>.xml -> values-*_<f>.arsc.flat, flag(x)/<dir>/<f> ->
// <dir>_<f>.(x).<ext>.flat, else <dir>_<f>.flat, all under the source's directory above <dir>.
func aapt2Path(res string) string {
	ext := filepath.Ext(res)
	name := strings.TrimSuffix(filepath.Base(res), ext)
	sub, last := filepath.Split(filepath.Dir(res))
	if strings.HasPrefix(last, "values") {
		ext = ".arsc"
	}
	if flagsPathRe.MatchString(filepath.Base(sub)) {
		var flag string
		sub, flag = filepath.Split(filepath.Dir(sub))
		flag = strings.TrimPrefix(flag, "flag")
		return filepath.Join(sub, fmt.Sprintf("%s_%s.%s%s.flat", last, name, flag, ext))
	}
	return filepath.Join(sub, fmt.Sprintf("%s_%s%s.flat", last, name, ext))
}

func ignoredResource(base string) bool {
	for _, p := range androidResourceIgnore {
		if ok, _ := filepath.Match(p, strings.ToLower(base)); ok {
			return true
		}
	}
	return false
}

// resourceGlob: androidResourceGlob, tree-relative and sorted.
func (g *Graph) resourceGlob(dir string) []string {
	var out []string
	for _, f := range globTree(g.abs(dir), "**/*", dir) {
		skip := false
		for _, part := range strings.Split(strings.TrimPrefix(f, dir+"/"), "/") {
			if ignoredResource(part) {
				skip = true
			}
		}
		if !skip {
			out = append(out, f)
		}
	}
	slices.Sort(out)
	return out
}

// aaptSrcjar: the genrule node building an android_app's R srcjar (the `.aapt.srcjar` tag), nil with a problem when it cannot.
func (g *Graph) aaptSrcjar(m *Module, side Side, from string) *Node {
	// its own node (gen/<side>/<name>.aapt): no tag braces in paths the script quotes
	k := key{m.Name + ".aapt", side, Gen}
	if n, ok := g.nodes[k]; ok {
		return n
	}
	if g.java == nil || g.release == nil {
		g.problem("%s: %s{.aapt.srcjar} needs the Java tools and the release config", from, m.Name)
		return nil
	}
	n := &Node{key: k, mod: m}
	g.nodes[k] = n
	n.props = g.flattenJava(m, side)
	n.rule = g.aaptRule(n)
	g.order = append(g.order, n)
	return n
}

func (g *Graph) aaptRule(n *Node) *genRule {
	p, m := n.props, n.mod
	r, fail := g.newGenRule(n)
	genDir := g.genDir(n, "")
	r.hasTools = true
	aapt2 := g.genTool(n, r, "aapt2", fail)
	fixer := g.genTool(n, r, "manifest_fixer", fail)
	r.labels = []genLabel{aapt2, fixer}
	tool := "$(location aapt2)"
	bt := func(t string) string { return filepath.Join(g.java.tools.BuildTools, t) }
	soongZip := shellEscape(bt("soong_zip"))
	var cmds []string
	add := func(format string, args ...any) { cmds = append(cmds, fmt.Sprintf(format, args...)) }

	// feature flags: the aconfig text of each flags_packages declarations (aapt2Compile / link: sorted unique paths)
	var ff []string
	for _, d := range p.strs("flags_packages") {
		if t := g.aconfigText(d, n.mod.Name); t != "" {
			ff = append(ff, t)
			r.implicit = append(r.implicit, t)
		}
	}
	slices.Sort(ff)
	ff = slices.Compact(ff)
	var ffFlags []string
	for _, f := range ff {
		ffFlags = append(ffFlags, "--feature-flags", "@"+shellEscape(f))
	}
	cflags := strings.Join(append([]string{"--pseudo-localize"}, ffFlags...), " ")

	add("rm -rf %s && mkdir -p %s", shellEscape(genDir+"/aapt2"), shellEscape(genDir+"/aapt2"))
	resDirs := p.strs("resource_dirs")
	if len(resDirs) == 0 {
		resDirs = []string{"res"}
	}
	var flats []string
	for _, rd := range resDirs {
		dir := filepath.Join(m.Dir, rd)
		files := g.resourceGlob(dir)
		r.inputs = append(r.inputs, files...)
		outDir := filepath.Join(genDir, "aapt2", dir)
		add("mkdir -p %s", shellEscape(outDir))
		for _, shard := range shards(files, aapt2ShardSize) {
			add("%s compile -o %s %s %s", tool, shellEscape(outDir), cflags, strings.Join(mapStrings(shard, shellEscape), " "))
		}
		for _, f := range files {
			flats = append(flats, filepath.Join(genDir, "aapt2", aapt2Path(f)))
		}
	}
	g.genFile(filepath.Join(genDir, "res.list"), strings.Join(flats, " "))
	r.implicit = append(r.implicit, filepath.Join(genDir, "res.list"))

	var zips []string
	for i, z := range p.strs("resource_zips") {
		files, ok := g.resolveSrc(m.Dir, n.side, z, m.Name)
		if !ok {
			fail("resource_zips %s unresolved", z)
		}
		for _, f := range files {
			r.inputs = append(r.inputs, f.path)
			if f.gen != nil {
				r.implicit = append(r.implicit, f.gen.rule.deps...)
			}
			zd := filepath.Join(genDir, "aapt2", "reszip."+strconv.Itoa(i))
			flata := filepath.Join(genDir, "reszip."+strconv.Itoa(i)+".flata")
			add("rm -rf %s && mkdir -p %s && unzip -qo %s -d %s && %s compile -o %s %s --dir %s", shellEscape(zd), shellEscape(zd), shellEscape(f.path),
				shellEscape(zd), tool, shellEscape(flata), cflags, shellEscape(zd))
			zips = append(zips, flata)
		}
	}

	// link flags (aapt2Flags, then buildActions)
	link := append([]string{}, p.strs("aaptflags")...)
	link = append(link, "--enable-compact-entries")
	manifest := filepath.Join(m.Dir, "AndroidManifest.xml")
	if mf := p.str("manifest"); mf != "" {
		manifest = filepath.Join(m.Dir, mf)
	}
	r.inputs = append(r.inputs, manifest)
	level, _ := g.release.PlatformSdk()
	if v := parseSdk(p.str("sdk_version")).level; v != "" {
		level = v
	}
	if v := p.str("min_sdk_version"); v != "" {
		level = v
	}
	fixArgs := []string{"--extract-native-libs=true", "--targetSdkVersion", level, "--minSdkVersion", level, "--replaceMaxSdkVersionPlaceholder", level, "--raise-min-sdk-version"}
	if len(p.strs("srcs")) == 0 && len(p.strs("static_libs")) == 0 {
		fixArgs = append(fixArgs, "--has-no-code")
	}
	fixed := filepath.Join(genDir, "manifest_fixer", "AndroidManifest.xml")
	add("mkdir -p %s && $(location manifest_fixer) %s %s %s", shellEscape(filepath.Dir(fixed)), strings.Join(fixArgs, " "), shellEscape(manifest), shellEscape(fixed))
	link = append(link, "--manifest", shellEscape(fixed))
	assetDirs := p.strs("asset_dirs")
	if len(assetDirs) == 0 {
		if _, err := globDirExists(g, filepath.Join(m.Dir, "assets")); err == nil {
			assetDirs = []string{"assets"}
		}
	}
	for _, a := range assetDirs {
		dir := filepath.Join(m.Dir, a)
		link = append(link, "-A", shellEscape(dir))
		r.implicit = append(r.implicit, g.resourceGlob(dir)...)
	}
	link = append(link, "--min-sdk-version", level, "--target-sdk-version", level)
	if !slices.ContainsFunc(p.strs("aaptflags"), func(f string) bool { return strings.HasPrefix(f, "--version-code") }) {
		link = append(link, "--version-code", g.release.Flag("RELEASE_PLATFORM_SDK_VERSION"))
	}
	if !slices.ContainsFunc(p.strs("aaptflags"), func(f string) bool { return strings.HasPrefix(f, "--version-name") }) {
		name := g.release.Flag("RELEASE_PLATFORM_SDK_VERSION")
		if m.Name == "framework-res" {
			name = g.release.Flag("RELEASE_PLATFORM_VERSION")
		}
		link = append(link, "--version-name", shellEscape(name))
	}
	link = append(link, "--no-static-lib-packages")
	java := filepath.Join(genDir, "aapt2", "java")
	link = append(link, "--java", shellEscape(java))
	in := []string{"@" + shellEscape(filepath.Join(genDir, "res.list"))}
	for _, z := range zips {
		in = append(in, "-R", shellEscape(z))
	}
	in = append(in, ffFlags...)
	apk := filepath.Join(genDir, "aapt2", "package-res.apk")
	out := filepath.Join(genDir, "R.srcjar")
	add("rm -rf %s && %s link -o %s %s %s", shellEscape(java), tool, shellEscape(apk), strings.Join(link, " "), strings.Join(in, " "))
	add("%s -write_if_changed -jar -o %s -C %s -D %s", soongZip, shellEscape(out), shellEscape(java), shellEscape(java))
	unsigned := filepath.Join(genDir, m.Name+"-unsigned.apk")
	installed := filepath.Join(genDir, m.Name+".apk")
	add("%s %s %s && %s -f -p 4 %s %s", shellEscape(bt("merge_zips")), shellEscape(unsigned), shellEscape(apk), shellEscape(bt("zipalign")),
		shellEscape(unsigned), shellEscape(installed))
	r.cmd = strings.Join(cmds, " && ")
	r.implicit = append(r.implicit, bt("soong_zip"), bt("merge_zips"), bt("zipalign"))
	r.outs = []string{out, installed}
	r.consumerDeps(genDir)
	return r
}

// globDirExists: whether dir is an existing directory of the tree.
func globDirExists(g *Graph, dir string) (bool, error) {
	if st, err := os.Stat(g.abs(dir)); err != nil || !st.IsDir() {
		return false, fmt.Errorf("%s: not a directory", dir)
	}
	return true, nil
}
