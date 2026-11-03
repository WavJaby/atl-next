// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

// Expected values are derived from Soong's java package at the pinned tag, never from the code under test: aar.go (aapt.buildActions,
// aaptLibs, resourceProcessorBusyBoxGenerateBinaryR, AndroidLibrary, AARImport), app.go (aaptBuildActions, dexBuildActions), app_builder.go
// (CreateAndSignAppPackage, TransformJniLibsToJar), android_manifest.go (ManifestFixer, manifestMerger), aapt2.go (aapt2Link) and
// blueprint's depset.go.

// appFixture is the tools every android module needs plus extra: aapt2, manifest_fixer, manifest-merger, d8, ResourceProcessorBusyBox's jar
// and framework-res.
func appFixture(extra map[string]string) map[string]string {
	files := map[string]string{
		"tools/Android.bp": `cc_binary_host { name: "aapt2", srcs: ["a.cc"] }
python_binary_host { name: "manifest_fixer", main: "manifest_fixer.py", srcs: ["manifest_fixer.py"] }`,
		"tools/a.cc": "", "tools/manifest_fixer.py": "",
		"prebuilts/manifest-merger/Android.bp": `java_binary_host { name: "manifest-merger", wrapper: "mm", srcs: ["M.java"] }`,
		"prebuilts/manifest-merger/M.java":     "", "prebuilts/manifest-merger/mm": "",
		"prebuilts/r8/Android.bp": `java_binary_host { name: "d8", wrapper: "d8", srcs: ["W.java"] }`,
		"prebuilts/r8/W.java":     "", "prebuilts/r8/d8": "",
		busyboxPath:                                    "",
		"frameworks/base/core/res/Android.bp":          `android_app { name: "framework-res", sdk_version: "core_platform" }`,
		"frameworks/base/core/res/AndroidManifest.xml": "", "frameworks/base/core/res/res/values/strings.xml": "",
	}
	for k, v := range extra {
		files[k] = v
	}
	return files
}

var appFlags = map[string]string{"RELEASE_PLATFORM_VERSION": "16"}

// depset.go: TOPOLOGICAL lists every set's elements before those of the sets it names, siblings in the order given, first copy of an element
// kept; a set with neither elements nor inputs is the zero set.
func TestDepSetTopological(t *testing.T) {
	d := newDepSet([]int{4}, nil)
	b := newDepSet([]int{2}, []*depSet[int]{d})
	c := newDepSet([]int{3}, []*depSet[int]{d})
	a := newDepSet([]int{1}, []*depSet[int]{b, c})
	if got, want := a.toList(), []int{1, 2, 3, 4}; !reflect.DeepEqual(got, want) {
		t.Errorf("toList = %v, want %v", got, want)
	}
	// a direct slice keeps its order; an element named twice is listed once, at its first (topologically earliest) place
	x := newDepSet([]int{1, 2}, []*depSet[int]{newDepSet([]int{2, 3}, nil)})
	if got, want := x.toList(), []int{1, 2, 3}; !reflect.DeepEqual(got, want) {
		t.Errorf("toList with a duplicate = %v, want %v", got, want)
	}
	if newDepSet[int](nil, nil) != nil || newDepSet[int](nil, nil).toList() != nil {
		t.Errorf("an empty set must be the zero set")
	}
}

func TestFrameworkResAndEffectiveSdk(t *testing.T) {
	// sdk.go decodeSdkDep: frameworkResModule is "framework-res" for private and the module-backed kinds, never for none, core_platform, core or
	// a numbered SDK
	for in, want := range map[string]bool{"": true, "current": true, "system_current": true, "test_current": true, "module_current": true,
		"system_server_current": true, "none": false, "core_platform": false, "core_current": false, "34": false, "system_30": false} {
		if got := frameworkResFor(parseSdk(in)); got != want {
			t.Errorf("frameworkResFor(%q) = %v, want %v", in, got, want)
		}
	}
	g := &Graph{release: &Release{flags: map[string]string{"RELEASE_PLATFORM_VERSION_CODENAME": "REL", "RELEASE_PLATFORM_SDK_VERSION": "36"}}}
	// ApiLevel.EffectiveVersionString: a number stays, current / private / none become the platform SDK once final
	for in, want := range map[string]string{"34": "34", "": "36", "current": "36"} {
		if got := g.effectiveSdk(in); got != want {
			t.Errorf("final: effectiveSdk(%q) = %q, want %q", in, got, want)
		}
	}
	// not final: DefaultAppTargetSdk is the codename, and FinalOrFutureInt of a preview level is 10000
	g.release.flags["RELEASE_PLATFORM_VERSION_CODENAME"] = "Baklava"
	if got := g.effectiveSdk(""); got != "Baklava" {
		t.Errorf("not final: effectiveSdk(\"\") = %q, want Baklava", got)
	}
	if got := g.effectiveSdkInt("current"); got != 10000 {
		t.Errorf("not final: effectiveSdkInt(current) = %d, want 10000", got)
	}
	if got := g.effectiveSdkInt("30"); got != 30 {
		t.Errorf("effectiveSdkInt(30) = %d", got)
	}
}

const appTree = `
android_library_import { name: "annot", aars: ["a.aar"], sdk_version: "34" }
android_library { name: "lib", srcs: ["L.java"], static_libs: ["annot"], manifest: "LibManifest.xml", sdk_version: "34" }
android_app { name: "app", srcs: ["A.java"], static_libs: ["lib"], sdk_version: "34" }
`

func appGraph(t *testing.T, bp string, extra map[string]string) *Graph {
	files := map[string]string{
		"p/Android.bp": bp, "p/a.aar": "", "p/L.java": "", "p/A.java": "", "p/AndroidManifest.xml": "", "p/LibManifest.xml": "",
		"p/res/values/strings.xml": "",
	}
	for k, v := range extra {
		files[k] = v
	}
	return javaTestGraph(t, appFixture(files), appFlags)
}

// An android_library with busybox (aar.go buildActions, isLibrary && useResourceProcessorBusyBox): the manifest_fixer args of a library,
// the manifest merged over its static libraries' for the R.jar only, `--static-lib ... --merge-only` after the SDK's -I and the dependencies'
// packages as imports (-I), its own resources overlaid because it has static libraries, and its R.jar (non-final fields) ahead of the
// dependencies' on javac's classpath.
func TestAndroidLibraryResources(t *testing.T) {
	g := appGraph(t, appTree, nil)
	n := g.jnode("lib", Device, "test")
	if n == nil || n.aapt == nil {
		t.Fatalf("no node: %v", g.problems)
	}
	ninja := g.java.b.String()
	l, a := filepath.Join(g.out, "device", "lib"), filepath.Join(g.out, "device", "annot")
	fixed := filepath.Join(l, "manifest_fixer", "AndroidManifest.xml")
	fix := ninjaEdgeVars(ninja, fixed)
	if want := "--library --targetSdkVersion 34 --minSdkVersion 34 --replaceMaxSdkVersionPlaceholder 36 --raise-min-sdk-version"; fix == nil || fix["args"] != want {
		t.Errorf("manifest_fixer %v, want args %q", fix, want)
	}
	if want := "python3 " + g.abs("tools/manifest_fixer.py"); fix["tool"] != want {
		t.Errorf("manifest_fixer tool %q, want %q", fix["tool"], want)
	}
	// manifestMerger: the aar's manifest is a static library manifest; a library passes no --remove-tools-declarations
	aarManifest := filepath.Join(a, "aar", "AndroidManifest.xml")
	merged := filepath.Join(l, "manifest_merger", "AndroidManifest.xml")
	mm := ninjaEdgeVars(ninja, merged)
	if mm == nil || mm["args"] != "" || mm["libs"] != "--libs "+aarManifest || !strings.Contains(mm[""], ": manifestmerger "+fixed+" | "+filepath.Join(g.out, "host", "bin", "manifest-merger")+" "+aarManifest) {
		t.Errorf("manifest merger %v", mm)
	}
	androidJar := g.abs("prebuilts/sdk/34/public/android.jar") + " -I " + g.abs("prebuilts/sdk/tools/core-lambda-stubs.jar")
	link := ninjaEdgeVars(ninja, filepath.Join(l, "package-res.apk"))
	wantFlags := "--enable-compact-entries --manifest " + fixed + " --min-sdk-version 34 --target-sdk-version 34 --version-code 36 --version-name 36" +
		" --auto-add-overlay -I " + androidJar + " --static-lib --no-static-lib-packages --merge-only -I " + filepath.Join(a, "package-res.apk")
	if link == nil || link["flags"] != wantFlags {
		t.Fatalf("link %v, want flags %q", link, wantFlags)
	}
	// the library has static libraries, so its compiled resources are overlays (and no static package is: those are imports)
	list := filepath.Join(l, "aapt2", "overlay.list")
	if want := filepath.Join(l, "aapt2", "p", "res", "values_strings.arsc.flat"); link["inflags"] != "-R @"+list || g.genFiles[list] != want {
		t.Errorf("link inputs %q, overlay list %q, want %q", link["inflags"], g.genFiles[list], want)
	}
	rj := ninjaEdgeVars(ninja, filepath.Join(l, "busybox", "R.jar"))
	if rj == nil || rj["rtxt"] != filepath.Join(l, "R.txt") || rj["manifest"] != merged || rj["args"] != "--finalFields=false" {
		t.Errorf("R.jar %v", rj)
	}
	// transitiveAaptRJars: the module's R.jar first, then its dependencies'
	javac := ninjaEdgeVars(ninja, filepath.Join(l, "javac", "lib.jar"))
	if want := "-classpath " + esc(filepath.Join(l, "busybox", "R.jar")+":"+filepath.Join(a, "busybox", "R.jar")+":"); javac == nil || !strings.HasPrefix(javac["cp"], want) {
		t.Errorf("javac classpath %v, want prefix %q", javac, want)
	}
	// the library's implementation does not hold its R classes (extraClasspathJars only)
	if comb := ninjaEdgeVars(ninja, filepath.Join(l, "combined", "lib.jar")); comb == nil || strings.Contains(comb[""], "R.jar") {
		t.Errorf("combined %v holds an R.jar", comb)
	} else if strings.Contains(comb["args"], "-m ") {
		// aar.go / app.go: `a.properties.Manifest = nil`, the Android manifest is not the jar's
		t.Errorf("combined jar takes the Android manifest as its jar manifest: %q", comb["args"])
	}
}

// An android_app (app.go, aar.go buildActions): the merged manifest is what aapt2 links and the R.jar reads; every transitive static library's
// package is an overlay ahead of the app's own compiled resources, reversed from the depset's order; the busybox R.jar names each
// library's R.txt and manifest and is combined into the classes after the javac jar; a dependency's assets are merged after the link;
// the apk is the dex jar and package-res.apk merged, then aligned.
func TestAndroidAppResourcesAndPackage(t *testing.T) {
	g := appGraph(t, appTree, map[string]string{"p/res/values/lib.xml": ""})
	n := g.jnode("app", Device, "test")
	if n == nil || n.aapt == nil {
		t.Fatalf("no node: %v", g.problems)
	}
	ninja := g.java.b.String()
	ap, l, a := filepath.Join(g.out, "device", "app"), filepath.Join(g.out, "device", "lib"), filepath.Join(g.out, "device", "annot")
	fixed := filepath.Join(ap, "manifest_fixer", "AndroidManifest.xml")
	// ManifestFixer: an app extracts native libraries (minSdkVersion >= 23, no embedded libs)
	fix := ninjaEdgeVars(ninja, fixed)
	if want := "--extract-native-libs=true --targetSdkVersion 34 --minSdkVersion 34 --replaceMaxSdkVersionPlaceholder 36 --raise-min-sdk-version"; fix == nil || fix["args"] != want {
		t.Errorf("manifest_fixer %v, want args %q", fix, want)
	}
	// manifestMerger: the static libraries' manifests in ManifestsDepSet order (lib's, then the aar's), tools declarations removed
	libFixed, aarManifest := filepath.Join(l, "manifest_fixer", "AndroidManifest.xml"), filepath.Join(a, "aar", "AndroidManifest.xml")
	merged := filepath.Join(ap, "manifest_merger", "AndroidManifest.xml")
	mm := ninjaEdgeVars(ninja, merged)
	if mm == nil || mm["args"] != "--remove-tools-declarations" || mm["libs"] != "--libs "+libFixed+" --libs "+aarManifest {
		t.Errorf("manifest merger %v", mm)
	}
	androidJar := g.abs("prebuilts/sdk/34/public/android.jar") + " -I " + g.abs("prebuilts/sdk/tools/core-lambda-stubs.jar")
	link := ninjaEdgeVars(ninja, filepath.Join(ap, "aapt2", "package-res.apk"))
	wantFlags := "--enable-compact-entries --manifest " + merged + " --min-sdk-version 34 --target-sdk-version 34 --version-code 36 --version-name 36" +
		" --auto-add-overlay -I " + androidJar + " --no-static-lib-packages"
	if link == nil || link["flags"] != wantFlags {
		t.Fatalf("link %v, want flags %q", link, wantFlags)
	}
	// overlays: the transitive static packages reversed (aar before lib), then the app's own compiled resources; no res.list (every file is an overlay)
	list := filepath.Join(ap, "aapt2", "overlay.list")
	wantList := filepath.Join(a, "package-res.apk") + " " + filepath.Join(l, "package-res.apk") + " " + filepath.Join(ap, "aapt2", "p", "res", "values_lib.arsc.flat") + " " +
		filepath.Join(ap, "aapt2", "p", "res", "values_strings.arsc.flat")
	if link["inflags"] != "-R @"+list || g.genFiles[list] != wantList {
		t.Errorf("link inputs %q, overlay list %q, want %q", link["inflags"], g.genFiles[list], wantList)
	}
	// aapt2Link: assets of the dependencies are merged after the link into the module's package-res.apk
	if mz := ninjaEdgeVars(ninja, filepath.Join(ap, "package-res.apk")); mz == nil ||
		!strings.Contains(mz[""], ": mergezips "+filepath.Join(ap, "aapt2", "package-res.apk")+" "+filepath.Join(a, "assets.zip")) {
		t.Errorf("assets merge %v", mz)
	}
	// resourceProcessorBusyBoxGenerateBinaryR (an app): --library=<R.txt>,<manifest> per transitive static library in depset order, the merged manifest
	rj := ninjaEdgeVars(ninja, filepath.Join(ap, "busybox", "R.jar"))
	wantArgs := "--library=" + filepath.Join(l, "R.txt") + "," + libFixed + " --library=" + filepath.Join(a, "aar", "R.txt") + "," + aarManifest
	if rj == nil || rj["args"] != wantArgs || rj["manifest"] != merged || rj["rtxt"] != filepath.Join(ap, "R.txt") {
		t.Errorf("R.jar %v, want args %q", rj, wantArgs)
	}
	// dexBuildActions: the R.jar is on the classpath and in the classes, after javac's jar
	javac := ninjaEdgeVars(ninja, filepath.Join(ap, "javac", "app.jar"))
	if javac == nil || !strings.HasPrefix(javac["cp"], "-classpath "+esc(filepath.Join(ap, "busybox", "R.jar")+":")) {
		t.Errorf("javac %v", javac)
	}
	comb := ninjaEdgeVars(ninja, filepath.Join(ap, "combined", "app.jar"))
	if comb == nil || !strings.Contains(comb[""], filepath.Join(ap, "javac", "app.jar")+" "+filepath.Join(ap, "busybox", "R.jar")+" ") {
		t.Errorf("combined %v", comb)
	}
	// CreateAndSignAppPackage: dex jar, resource package; unsigned, then aligned in place of signapk
	unsigned := ninjaEdgeVars(ninja, filepath.Join(ap, "app-unsigned.apk"))
	if unsigned == nil || !strings.Contains(unsigned[""], ": mergezips "+filepath.Join(ap, "aligned", "app.jar")+" "+filepath.Join(ap, "package-res.apk")) {
		t.Errorf("unsigned apk %v", unsigned)
	}
	if al := ninjaEdgeVars(ninja, filepath.Join(ap, "app.apk")); al == nil || !strings.Contains(al[""], ": zipalign "+filepath.Join(ap, "app-unsigned.apk")) || n.apk != filepath.Join(ap, "app.apk") {
		t.Errorf("aligned apk %v", al)
	}
}

// A module that sets use_resource_processor: false (CtsUtilTestCases) compiles aapt2's R.java: `--java <dir>` with a soong_zip of it into
// gen/android/R.srcjar (aapt2Link, the `android` subdir PathForModuleGen keeps), the srcjar among javac's sources, no R.jar.
func TestAndroidLibraryWithoutBusybox(t *testing.T) {
	g := appGraph(t, `android_library { name: "plain", srcs: ["L.java"], use_resource_processor: false, sdk_version: "34" }`, nil)
	if n := g.jnode("plain", Device, "test"); n == nil || n.aapt == nil {
		t.Fatalf("no node: %v", g.problems)
	}
	ninja := g.java.b.String()
	d := filepath.Join(g.out, "device", "plain")
	srcjar := filepath.Join(d, "gen", "android", "R.srcjar")
	link := ninjaEdgeVars(ninja, filepath.Join(d, "package-res.apk"))
	javaDir := filepath.Join(d, "gen", "aapt2", "R")
	if link == nil || !strings.HasSuffix(link["flags"], " --java "+javaDir) || link["preamble"] != "rm -rf "+javaDir+" &&" ||
		!strings.Contains(link["postamble"], "-jar -o "+srcjar+" -C "+javaDir) || !strings.Contains(link[""], srcjar) {
		t.Errorf("link %v", link)
	}
	if javac := ninjaEdgeVars(ninja, filepath.Join(d, "javac", "plain.jar")); javac == nil || javac["srcjars"] != esc(srcjar) {
		t.Errorf("javac %v, want srcjars %s", javac, srcjar)
	}
	if ninjaEdgeVars(ninja, filepath.Join(d, "busybox", "R.jar")) != nil {
		t.Errorf("an R.jar is generated without ResourceProcessorBusyBox")
	}
}

// aaptLibs: an SDK with framework libraries links framework-res's package (frameworkResTag, a shared library: -I); platform_apis and the module
// SDK tell manifest_fixer about non-SDK API use; uses_libs become --uses-library, an optional one only when it exists (presentOptionalUsesLibs).
func TestAndroidAppFlags(t *testing.T) {
	g := appGraph(t, `
android_app { name: "cur", srcs: ["A.java"], sdk_version: "current", uses_libs: ["req.lib"], optional_uses_libs: ["opt.lib", "absent.lib"] }
android_app { name: "plat", srcs: ["A.java"], platform_apis: true, min_sdk_version: "30" }
java_library { name: "opt.lib", srcs: ["L.java"], sdk_version: "none", system_modules: "none" }
`, nil)
	if n := g.jnode("cur", Device, "test"); n == nil || n.aapt == nil {
		t.Fatalf("no node: %v", g.problems)
	}
	ninja := g.java.b.String()
	cur := filepath.Join(g.out, "device", "cur")
	link := ninjaEdgeVars(ninja, filepath.Join(cur, "package-res.apk"))
	fwRes := filepath.Join(g.out, "gen", "device", "framework-res.aapt", "framework-res.apk")
	if link == nil || !strings.Contains(link["flags"], " -I "+fwRes) {
		t.Errorf("link %v lacks -I %s", link, fwRes)
	}
	fix := ninjaEdgeVars(ninja, filepath.Join(cur, "manifest_fixer", "AndroidManifest.xml"))
	if want := "--extract-native-libs=true --uses-library req.lib --optional-uses-library opt.lib --targetSdkVersion 36 --minSdkVersion 36"; fix == nil || !strings.HasPrefix(fix["args"], want) {
		t.Errorf("manifest_fixer %v, want prefix %q", fix, want)
	}
	if n := g.jnode("plat", Device, "test"); n == nil {
		t.Fatalf("no node: %v", g.problems)
	}
	plat := ninjaEdgeVars(g.java.b.String(), filepath.Join(g.out, "device", "plat", "manifest_fixer", "AndroidManifest.xml"))
	if want := "--extract-native-libs=true --uses-non-sdk-api --targetSdkVersion 36 --minSdkVersion 30 "; plat == nil || !strings.HasPrefix(plat["args"], want) {
		t.Errorf("manifest_fixer %v, want prefix %q", plat, want)
	}
}

// TransformJniLibsToJar: files junked (-j) and re-added under lib/<abi>/; stored when the apk embeds native libraries (min_sdk_version >= 23
// and use_embedded_native_libs).
func TestJniZipEdge(t *testing.T) {
	g := appGraph(t, `android_app { name: "j", srcs: ["A.java"], sdk_version: "34", min_sdk_version: "30", use_embedded_native_libs: true }
android_app { name: "k", srcs: ["A.java"], sdk_version: "34" }`, nil)
	g.cfg.ABI = "x86_64"
	for name, want := range map[string]string{"j": "-j -L 0 -P lib/x86_64 -f /o/liba.so -P lib/x86_64 -f /o/libb.so", "k": "-j -P lib/x86_64 -f /o/liba.so -P lib/x86_64 -f /o/libb.so"} {
		n := g.jnode(name, Device, "test")
		if n == nil {
			t.Fatalf("no node %s: %v", name, g.problems)
		}
		zip := g.jniZipEdge(n, []string{"/o/liba.so", "/o/libb.so"})
		e := ninjaEdgeVars(g.java.b.String(), zip)
		if zip != filepath.Join(n.dir, "jnilibs.zip") || e == nil || e["args"] != want {
			t.Errorf("%s: jni zip %q %v, want args %q", name, zip, e, want)
		}
	}
	if g.jniZipEdge(g.java.nodes[jKey{"k", Device}], nil) != "" {
		t.Errorf("no libraries, no zip")
	}
}

// An app named in srcs / java_resources (app.go setOutputFiles): `:name` is the apk and its splits, {.apk} the apk alone, {.export-package.apk}
// the resource package; package_splits link `--split <path>:<name>` (a comma in the name becomes `_` in the file name) and each split is
// packaged on its own. A prebuilt_apex is its src, from the arch block of the device arch (prebuilt.go).
func TestAppOutputsAndSplits(t *testing.T) {
	g := appGraph(t, `
android_test_helper_app { name: "h", srcs: ["A.java"], sdk_version: "34", package_splits: ["anydpi", "a,b"] }
prebuilt_apex { name: "ap", src: "generic.apex", arch: { x86_64: { src: "x86/ap.apex" }, arm: { src: "arm/ap.apex" } } }
`, nil)
	d := filepath.Join(g.out, "device", "h")
	paths := func(ref string) []string {
		files, ok := g.resolveSrc("p", Device, ref, "test")
		if !ok {
			t.Fatalf("%s unresolved: %v", ref, g.problems)
		}
		var out []string
		for _, f := range files {
			out = append(out, f.path)
		}
		return out
	}
	if got, want := paths(":h"), []string{filepath.Join(d, "h.apk"), filepath.Join(d, "h_anydpi.apk"), filepath.Join(d, "h_a_b.apk")}; !reflect.DeepEqual(got, want) {
		t.Errorf(":h = %v, want %v", got, want)
	}
	if got, want := paths(":h{.apk}"), []string{filepath.Join(d, "h.apk")}; !reflect.DeepEqual(got, want) {
		t.Errorf(":h{.apk} = %v, want %v", got, want)
	}
	if got, want := paths(":h{.export-package.apk}"), []string{filepath.Join(d, "package-res.apk")}; !reflect.DeepEqual(got, want) {
		t.Errorf(":h{.export-package.apk} = %v, want %v", got, want)
	}
	if got, want := paths(":ap"), []string{"p/x86/ap.apex"}; !reflect.DeepEqual(got, want) {
		t.Errorf(":ap = %v, want %v", got, want)
	}
	ninja := g.java.b.String()
	link := ninjaEdgeVars(ninja, filepath.Join(d, "package-res.apk"))
	if link == nil || !strings.HasSuffix(link["flags"], " --split "+filepath.Join(d, "package_anydpi.apk")+":anydpi --split "+filepath.Join(d, "package_a_b.apk")+":a,b") ||
		!strings.Contains(link[""], filepath.Join(d, "package_a_b.apk")) {
		t.Errorf("link %v", link)
	}
	if su := ninjaEdgeVars(ninja, filepath.Join(d, "h_a_b-unsigned.apk")); su == nil || !strings.Contains(su[""], ": mergezips "+filepath.Join(d, "package_a_b.apk")) {
		t.Errorf("split apk %v", su)
	}
	if al := ninjaEdgeVars(ninja, filepath.Join(d, "h_a_b.apk")); al == nil || !strings.Contains(al[""], ": zipalign "+filepath.Join(d, "h_a_b-unsigned.apk")) {
		t.Errorf("split aligned apk %v", al)
	}
}
