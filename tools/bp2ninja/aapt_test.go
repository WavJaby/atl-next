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

func TestAaptSrcjar(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"app/Android.bp": `
android_app { name: "fw-res", sdk_version: "core_platform", aaptflags: ["--private-symbols", "x.y"], flags_packages: ["decl"] }
aconfig_declarations { name: "decl", package: "com.x.flags", container: "system", srcs: ["f.aconfig"] }
cc_binary_host { name: "aapt2", srcs: ["a.cc"] }
python_binary_host { name: "manifest_fixer", main: "manifest_fixer.py", srcs: ["manifest_fixer.py"] }
`,
		"app/AndroidManifest.xml": "", "app/res/values/strings.xml": "", "app/res/.hidden": "", "app/res/drawable/a.png": "", "app/a.cc": "",
		"app/f.aconfig": "", "app/manifest_fixer.py": "",
	}, map[string]string{"RELEASE_ACONFIG_FLAG_DEFAULT_PERMISSION": "READ_WRITE", "RELEASE_ACONFIG_REQUIRE_ALL_READ_ONLY": "",
		"RELEASE_PLATFORM_VERSION": "16"})
	g.release.ValueSets = nil
	files, ok := g.resolveSrc("app", Device, ":fw-res{.aapt.srcjar}", "test")
	if !ok || len(files) != 1 || files[0].gen == nil {
		t.Fatalf("files %+v ok %v problems %v", files, ok, g.problems)
	}
	n := files[0].gen
	gd := g.genDir(n, "")
	if files[0].path != filepath.Join(gd, "R.srcjar") {
		t.Errorf("srcjar %s", files[0].path)
	}
	cmd := n.rule.cmd
	text := filepath.Join(g.out, "aconfig", "decl", "intermediate.txt")
	// androidResourceGlob drops .hidden; one compile shard; feature flags from flags_packages
	if want := "$(location aapt2) compile -o " + gd + "/aapt2/app/res --pseudo-localize --feature-flags @" + text + " app/res/drawable/a.png app/res/values/strings.xml"; !strings.Contains(cmd, want) {
		t.Errorf("cmd lacks %q:\n%s", want, cmd)
	}
	// manifest_fixer: Soong's args for framework-res (its ninja: "--extract-native-libs=true --targetSdkVersion  36 --minSdkVersion  36
	// --replaceMaxSdkVersionPlaceholder  36 --raise-min-sdk-version"); this fixture has no srcs, so --has-no-code too
	fixed := gd + "/manifest_fixer/AndroidManifest.xml"
	if want := "$(location manifest_fixer) --extract-native-libs=true --targetSdkVersion 36 --minSdkVersion 36 --replaceMaxSdkVersionPlaceholder 36 --raise-min-sdk-version --has-no-code app/AndroidManifest.xml " + fixed; !strings.Contains(cmd, want) {
		t.Errorf("cmd lacks %q:\n%s", want, cmd)
	}
	if len(g.problems) > 0 {
		t.Errorf("problems %v", g.problems)
	}
	// apk: merge_zips into <name>-unsigned.apk (combineApk), then aligned as signapk would
	if want := "/bt/merge_zips " + gd + "/fw-res-unsigned.apk " + gd + "/aapt2/package-res.apk && /bt/zipalign -f -p 4 " + gd + "/fw-res-unsigned.apk " + gd + "/fw-res.apk"; !strings.Contains(cmd, want) {
		t.Errorf("cmd lacks %q:\n%s", want, cmd)
	}
	// link: aaptflags, --enable-compact-entries, the fixed manifest, sdk levels (core_platform: the platform SDK), version code / name, --java
	if want := "link -o " + gd + "/aapt2/package-res.apk --private-symbols x.y --enable-compact-entries --manifest " + fixed + " --min-sdk-version 36 --target-sdk-version 36 --version-code 36 --version-name 36 --no-static-lib-packages --java " + gd + "/aapt2/java @" + gd + "/res.list --feature-flags @" + text; !strings.Contains(cmd, want) {
		t.Errorf("cmd lacks %q:\n%s", want, cmd)
	}
	if got := g.genFiles[filepath.Join(gd, "res.list")]; got != gd+"/aapt2/app/res/drawable_a.png.flat "+gd+"/aapt2/app/res/values_strings.arsc.flat" {
		t.Errorf("res.list %q", got)
	}
}
