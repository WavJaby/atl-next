// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// sdkTree: the synthetic tree (testdata/sdkext) in the layout sdkExtensions reads. Its db and derive_sdk.cpp are ours, not AOSP's.
const sdkTree = "testdata/sdkext"

var (
	sdkAllApexes = []string{"com.android.art", "com.android.adservices", "com.android.extservices", "com.android.mediaprovider"}
	sdkNoExt     = []string{"com.android.art", "com.android.adservices", "com.android.mediaprovider"}
	sdkNoAd      = []string{"com.android.art", "com.android.extservices", "com.android.mediaprovider"}
)

func sdkRelease(sdk, moduleVersion string) *Release {
	return &Release{Name: "test", flags: map[string]string{
		"RELEASE_PLATFORM_SDK_VERSION":           sdk,
		"RELEASE_PLATFORM_SDK_EXTENSION_VERSION": moduleVersion,
	}}
}

// sdkRows formats rows as `name=value` words, without the property prefix.
func sdkRows(rows [][2]string) string {
	var out []string
	for _, r := range rows {
		out = append(out, strings.TrimPrefix(r[0], sdkPropPrefix)+"="+r[1])
	}
	return strings.Join(out, " ")
}

// The wants are derived by hand from testdata/sdkext's db, not by running sdkExtensions:
//   - all installed, module 17, SDK 36: versions 1-4 meet r's requirements (MEDIA_PROVIDER, EXT_SERVICES present), so r = 4; s adds ART and t adds
//     AD_SERVICES, all met at v4: 4; ad_services is the module's 17 (at least 7).
//   - EXT_SERVICES missing: v3 and v4 require it, so r = 2; every later dessert keeps EXT_SERVICES in its set, so all are 2.
//   - module 3, all installed: v4 needs MEDIA_PROVIDER 4 > 3, so r = s = t = 3. ad_services: 3 < 7, so GetSdkLevel over AD_SERVICES alone: v1 has no
//     AD_SERVICES requirement, v2 and v3 meet 2 and 3, v4 needs 4: 3.
//   - AD_SERVICES missing: v1 holds, v2-v4 require it: t = 1, and ad_services = GetSdkLevel over AD_SERVICES alone = 1.
//   - SDK 33 sets r, s, t and ad_services (IsAtLeastT); SDK 30 sets r only.
func TestSdkExtensions(t *testing.T) {
	cases := []struct {
		name   string
		apexes []string
		sdk    string
		module string
		want   string
	}{
		{"all installed", sdkAllApexes, "36", "17", "r=4 s=4 t=4 u=4 v=4 b=4 ad_services=17"},
		{"extservices missing", sdkNoExt, "36", "17", "r=2 s=2 t=2 u=2 v=2 b=2 ad_services=17"},
		{"module version 3", sdkAllApexes, "36", "3", "r=3 s=3 t=3 u=3 v=3 b=3 ad_services=3"},
		{"adservices missing", sdkNoAd, "36", "17", "r=4 s=4 t=1 u=1 v=1 b=1 ad_services=1"},
		{"sdk 33 gates u v b", sdkAllApexes, "33", "17", "r=4 s=4 t=4 ad_services=17"},
		{"sdk 30 r only", sdkAllApexes, "30", "17", "r=4"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			rows, err := sdkExtensions(sdkTree, sdkRelease(c.sdk, c.module), c.apexes)
			if err != nil {
				t.Fatal(err)
			}
			if got := sdkRows(rows); got != c.want {
				t.Errorf("rows = %q, want %q", got, c.want)
			}
		})
	}
}

// sdkCopyTree writes a tree with the fixture's db and its derive_sdk.cpp, with one replacement of old by repl in the latter.
func sdkCopyTree(t *testing.T, old, repl string) string {
	t.Helper()
	cpp, err := os.ReadFile(filepath.Join(sdkTree, deriveSdkCpp))
	if err != nil {
		t.Fatal(err)
	}
	src := strings.Replace(string(cpp), old, repl, 1)
	if src == string(cpp) {
		t.Fatalf("fixture has no %q", old)
	}
	db, err := os.ReadFile(filepath.Join(sdkTree, extensionsDB))
	if err != nil {
		t.Fatal(err)
	}
	dir := t.TempDir()
	for path, body := range map[string]string{deriveSdkCpp: src, extensionsDB: string(db)} {
		p := filepath.Join(dir, path)
		if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(p, []byte(body), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	return dir
}

func TestSdkExtensionsShape(t *testing.T) {
	// the APEX map renamed: no kApexNameToModule to read
	tree := sdkCopyTree(t, "kApexNameToModule = {", "kApexMap = {")
	if _, err := sdkExtensions(tree, sdkRelease("36", "17"), sdkAllApexes); err == nil || !strings.Contains(err.Error(), "changed shape: update sdkext.go") {
		t.Errorf("renamed APEX map: err = %v, want the shape error", err)
	}
	// T set emptied: the db's AD_SERVICES requirement is met by no dessert's set (derive_sdk.cpp l.240-249)
	tree = sdkCopyTree(t, "kTModules = {SdkModule::AD_SERVICES}", "kTModules = {}")
	if _, err := sdkExtensions(tree, sdkRelease("36", "17"), sdkAllApexes); err == nil || !strings.Contains(err.Error(), "requires AD_SERVICES, which no dessert sets") {
		t.Errorf("unmapped requirement: err = %v", err)
	}
}

func TestParseExtensionDB(t *testing.T) {
	// `name: {` and a trailing comment parse like `name {`
	db, err := parseExtensionDB("# c\nversions: {\n  version: 5 # trailing\n  requirements { module: MEDIA version { version: 5 } }\n}\n")
	if err != nil {
		t.Fatal(err)
	}
	if len(db) != 1 || db[0].version != 5 || len(db[0].reqs) != 1 || db[0].reqs[0] != (extRequirement{"MEDIA", 5}) {
		t.Errorf("db = %+v", db)
	}
	// fields outside the subset, a requirement without its version, a block without a version, an unclosed block, no block: all refused
	for _, bad := range []string{
		"versions { version: 1 bogus: 2 }",
		"versions { version: 1 requirements { module: ART } }",
		"versions { requirements { module: ART version { version: 1 } } }",
		"versions { version: 1 ",
		"",
	} {
		if _, err := parseExtensionDB(bad); err == nil {
			t.Errorf("parseExtensionDB(%q): no error", bad)
		}
	}
}

func TestInstalledApexes(t *testing.T) {
	got := installedApexes([]bootJar{{platformApex, "framework"}, {artApex, "core-oj"}, {artApex, "okhttp"}, {"com.android.sdkext", "framework-sdkextensions"}})
	if want := "com.android.art,com.android.sdkext"; strings.Join(got, ",") != want {
		t.Errorf("installedApexes = %v, want %s", got, want)
	}
}
