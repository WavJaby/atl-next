// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"strings"
	"testing"
)

// Expected values follow build/soong/java aapt2.go pathToAapt2Path's own doc comment and Soong's ninja for framework-res
// (aapt2/frameworks/base/core/res/res/anim-ldrtl_activity_close_enter.xml.flat, values_strings.arsc.flat; compile flags
// "--pseudo-localize --feature-flags @<declarations>/intermediate.txt …"; link "--enable-compact-entries --manifest …").

func TestAapt2Path(t *testing.T) {
	for in, want := range map[string]string{
		"r/res/anim-ldrtl/activity_close_enter.xml":   "r/res/anim-ldrtl_activity_close_enter.xml.flat",
		"r/res/values/strings.xml":                    "r/res/values_strings.arsc.flat",
		"r/res/values-en-rCA/strings.xml":             "r/res/values-en-rCA_strings.arsc.flat",
		"r/res/flag(a.b.c)/values/strings.xml":        "r/res/values_strings.(a.b.c).arsc.flat",
		"r/res/flag(!a.b.c)/drawable/icon.png":        "r/res/drawable_icon.(!a.b.c).png.flat",
		"r/res/raw/remote_views_color_resources.arsc": "r/res/raw_remote_views_color_resources.arsc.flat",
	} {
		if got := aapt2Path(in); got != want {
			t.Errorf("%s: %s, want %s", in, got, want)
		}
	}
}

// framework-res is an android_app like any other, with the special cases Soong gives it: no code is compiled (its srcs are resource inputs), the
// version name is the platform's, the apk holds only the resource package. Its `{.aapt.srcjar}` is the R.java of use_resource_processor: false.
func TestFrameworkResApp(t *testing.T) {
	g := javaTestGraph(t, appFixture(map[string]string{
		"app/Android.bp": `
android_app { name: "framework-res", use_resource_processor: false, sdk_version: "core_platform", srcs: [":arsc"], aaptflags: ["--private-symbols", "x.y"],
    flags_packages: ["decl"], resource_zips: [":zips"] }
filegroup { name: "arsc", srcs: ["x.arsc"] }
filegroup { name: "zips", srcs: ["r.zip"] }
aconfig_declarations { name: "decl", package: "com.x.flags", container: "system", srcs: ["f.aconfig"] }
`,
		"app/AndroidManifest.xml": "", "app/res/values/strings.xml": "", "app/res/.hidden": "", "app/res/drawable/a.png": "", "app/x.arsc": "", "app/r.zip": "",
		"app/f.aconfig": "", "frameworks/base/core/res/Android.bp": "", // this test defines framework-res itself
	}), map[string]string{"RELEASE_ACONFIG_FLAG_DEFAULT_PERMISSION": "READ_WRITE", "RELEASE_ACONFIG_REQUIRE_ALL_READ_ONLY": "",
		"RELEASE_PLATFORM_VERSION": "16"})
	g.release.ValueSets = nil
	files, ok := g.resolveSrc("app", Device, ":framework-res{.aapt.srcjar}", "test")
	d := filepath.Join(g.out, "device", "framework-res")
	if !ok || len(files) != 1 || files[0].path != filepath.Join(d, "gen", "android", "R.srcjar") {
		t.Fatalf("files %+v ok %v problems %v", files, ok, g.problems)
	}
	ninja := g.java.b.String()
	text := filepath.Join(g.out, "aconfig", "decl", "intermediate.txt")
	// androidResourceGlob drops .hidden; one compile shard; feature flags from flags_packages
	flats := []string{filepath.Join(d, "aapt2", "app", "res", "drawable_a.png.flat"), filepath.Join(d, "aapt2", "app", "res", "values_strings.arsc.flat")}
	cz := ninjaEdgeVars(ninja, flats[0])
	if cz == nil || cz["cflags"] != "--pseudo-localize --feature-flags @"+text || cz["outdir"] != filepath.Join(d, "aapt2", "app", "res") ||
		!strings.Contains(cz[""], flats[0]+" "+flats[1]+": aapt2compile "+g.abs("app/res/drawable/a.png")+" "+g.abs("app/res/values/strings.xml")) {
		t.Errorf("compile %v", cz)
	}
	// manifest_fixer: the source has code (a srcs entry of any kind), so no --has-no-code; ManifestFixer's order of flags
	fixed := filepath.Join(d, "manifest_fixer", "AndroidManifest.xml")
	fix := ninjaEdgeVars(ninja, fixed)
	if want := "--extract-native-libs=true --targetSdkVersion 36 --minSdkVersion 36 --replaceMaxSdkVersionPlaceholder 36 --raise-min-sdk-version"; fix == nil || fix["args"] != want {
		t.Errorf("manifest_fixer %v, want args %q", fix, want)
	}
	// link: aaptflags, --enable-compact-entries, the fixed manifest, sdk levels (core_platform: the platform SDK), version code / name (framework-res: the
	// platform version), --java; the resource zip's flata is an overlay
	link := ninjaEdgeVars(ninja, filepath.Join(d, "package-res.apk"))
	wantFlags := "--private-symbols x.y --enable-compact-entries --manifest " + fixed + " --min-sdk-version 36 --target-sdk-version 36 --version-code 36 --version-name 16" +
		" --no-static-lib-packages --java " + filepath.Join(d, "gen", "aapt2", "R")
	if link == nil || link["flags"] != wantFlags {
		t.Fatalf("link %v, want flags %q", link, wantFlags)
	}
	flata := filepath.Join(d, "reszip.0.flata")
	if want := "@" + filepath.Join(d, "aapt2", "res.list") + " -R @" + filepath.Join(d, "aapt2", "overlay.list") + " --feature-flags @" + text; link["inflags"] != want {
		t.Errorf("link inputs %q, want %q", link["inflags"], want)
	}
	// aapt2Link: the compiled files of the dirs are resources (no static libraries, one dir), the zips' flata overlays
	if got := g.genFiles[filepath.Join(d, "aapt2", "res.list")]; got != flats[0]+" "+flats[1] {
		t.Errorf("res.list %q", got)
	}
	if got := g.genFiles[filepath.Join(d, "aapt2", "overlay.list")]; got != flata {
		t.Errorf("overlay.list %q, want %q", got, flata)
	}
	// the apk: merge_zips of the resource package alone (no code: no dex), aligned as signapk would
	n := g.jnode("framework-res", Device, "test")
	if n == nil || n.apk != filepath.Join(d, "framework-res.apk") || n.dex != "" {
		t.Fatalf("node %+v problems %v", n, g.problems)
	}
	if su := ninjaEdgeVars(ninja, filepath.Join(d, "framework-res-unsigned.apk")); su == nil || !strings.HasSuffix(su[""], ": mergezips "+filepath.Join(d, "package-res.apk")+" ") &&
		!strings.HasSuffix(su[""], ": mergezips "+filepath.Join(d, "package-res.apk")) {
		t.Errorf("unsigned apk %v", su)
	}
}
