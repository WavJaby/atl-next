// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
)

// android_app resources, the `{.aapt.srcjar}` output only (R.java and Manifest.java as `aapt2 link --java` writes them), following
// build/soong/java aar.go aapt2Flags / buildActions and aapt2.go (the spec). Built as one generated step (a genrule node), its script running:
//   - aapt2 compile per resource dir in shards of AAPT2_SHARD_SIZE sources (resource_dirs, default res; androidResourceGlob's exclusions),
//     with --pseudo-localize and --feature-flags @<text> of every flags_packages aconfig_declarations (sorted, unique);
//   - per resource_zips entry: unzip, then `aapt2 compile --dir` into reszip.<i>.flata (aapt2CompileZip);
//   - aapt2 link: aaptflags, --enable-compact-entries, --manifest, -A asset dirs (asset_dirs, default assets), --min-sdk-version /
//     --target-sdk-version (the SDK's effective level), --version-code (platform SDK), --version-name (framework-res: the platform version
//     name, RELEASE_PLATFORM_VERSION; else the platform SDK), --no-static-lib-packages, --java, then @res.list (pathToAapt2Path order),
//     -R each resource zip's flata, --feature-flags; soong_zip -jar of the --java dir is the srcjar.
//   - manifest_fixer is not run: the condition is that it edits only uses-sdk / extractNativeLibs / uses-library / placeholder versions
//     (android_manifest.go ManifestFixer args), which reach the apk, not R.java / Manifest.java; framework-res's R.srcjar measured
//     byte-identical to Soong's. Re-check if ManifestFixer gains an argument that adds permissions or resources.
//   - Dropped: the asset-dir hash
//     comment, product characteristics (-c / --product: the apk's configs), overlays and RROs (this product has none), the apk, R.txt,
//     proguard options and every app-level step after link.

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
	r.labels = []genLabel{aapt2}
	tool := "$(location aapt2)"
	soongZip := shellEscape(filepath.Join(g.java.tools.BuildTools, "soong_zip"))
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
	link = append(link, "--manifest", shellEscape(manifest))
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
	level, _ := g.release.PlatformSdk()
	if v := parseSdk(p.str("sdk_version")).level; v != "" {
		level = v
	}
	if v := p.str("min_sdk_version"); v != "" {
		level = v
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
	r.cmd = strings.Join(cmds, " && ")
	r.implicit = append(r.implicit, filepath.Join(g.java.tools.BuildTools, "soong_zip"))
	r.outs = []string{out}
	r.consumerDeps(genDir)
	return r
}

// globDirExists: whether dir is an existing directory of the tree.
func globDirExists(g *Graph, dir string) (bool, error) {
	files := globTree(g.abs(dir), "*", dir)
	if len(files) == 0 {
		return false, fmt.Errorf("%s: empty or missing", dir)
	}
	return true, nil
}
