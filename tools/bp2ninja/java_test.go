// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

// Expected values are derived from Soong's java package (build/soong/java at the pinned tag), never from the code under test:
// sdk.go decodeSdkDep / defaultJavaLanguageVersion, android/sdk_version.go SdkSpecFromWithConfig / Stable, java.go
// normalizeJavaVersion / javaVersion.String, java_resources.go resourceExcludes, base.go collectJavacFlags, dex.go dexCommonFlags,
// and from Soong's own generated ninja for core-oj (javacFlags "-Xlint:-dep-ann --patch-module=java.base=.:<out>", d8Flags
// "--android-platform-build --debug --min-api 31 --lib …").

func TestParseSdk(t *testing.T) {
	for in, want := range map[string]sdkSpec{
		"":                 {kind: "private"},
		"none":             {kind: "none"},
		"core_platform":    {kind: "core_platform"},
		"current":          {kind: "public"},
		"30":               {kind: "public", level: "30"},
		"system_current":   {kind: "system"},
		"module_current":   {kind: "module"},
		"core_current":     {kind: "core"},
		"system_server_33": {kind: "system_server", level: "33"},
		"test_current":     {kind: "test"},
	} {
		if got := parseSdk(in); got != want {
			t.Errorf("parseSdk(%q) = %+v, want %+v", in, got, want)
		}
	}
}

func TestSdkStable(t *testing.T) {
	// SdkSpec.Stable: none, core, public, system, module, system_server; not core_platform, test, private
	for kind, want := range map[string]bool{"none": true, "core": true, "public": true, "system": true, "module": true,
		"system_server": true, "core_platform": false, "test": false, "private": false, "test_frameworks_core": false} {
		if got := (sdkSpec{kind: kind}).stable(); got != want {
			t.Errorf("stable(%s) = %v, want %v", kind, got, want)
		}
	}
}

func TestJavaVersion(t *testing.T) {
	for in, want := range map[string]string{"1.6": "1.8", "7": "1.8", "1.8": "1.8", "9": "1.9", "11": "11", "17": "17", "21": "21"} {
		if got := parseJavaVersion(in).String(); got != want {
			t.Errorf("parseJavaVersion(%q) = %s, want %s", in, got, want)
		}
	}
	if parseJavaVersion("10") != 0 {
		t.Errorf("10 must be unsupported")
	}
	if parseJavaVersion("1.8").modules() || !parseJavaVersion("9").modules() {
		t.Errorf("usesJavaModules: >= 9")
	}
}

func TestDefaultJavaVersion(t *testing.T) {
	g := &Graph{java: &javaGraph{release: &Release{flags: map[string]string{"RELEASE_TARGET_JAVA_21": "true"}}}}
	// defaultJavaLanguageVersion: <=29 -> 8, <=31 -> 9, <=33 -> 11, newer / current -> 21 with RELEASE_TARGET_JAVA_21
	for s, want := range map[string]string{"29": "1.8", "31": "1.9", "33": "11", "34": "21", "current": "21", "none": "21"} {
		if got := g.defaultJavaVersion(Device, parseSdk(s)).String(); got != want {
			t.Errorf("sdk %s: %s, want %s", s, got, want)
		}
	}
	g.java.release.flags["RELEASE_TARGET_JAVA_21"] = ""
	if got := g.defaultJavaVersion(Host, sdkSpec{}).String(); got != "17" {
		t.Errorf("host without RELEASE_TARGET_JAVA_21: %s, want 17", got)
	}
}

func TestDecodeSdk(t *testing.T) {
	g := &Graph{}
	n := func(props *Obj) *jNode { return &jNode{props: props} }
	cases := []struct {
		sdk, sys string
		want     sdkDep
	}{
		// none + system_modules X: X is the system modules and the bootclasspath
		{"none", "core-all-system-modules", sdkDep{sysModules: "core-all-system-modules", boot: []string{"core-all-system-modules"}}},
		{"none", "none", sdkDep{}},
		{"core_platform", "", sdkDep{sysModules: "stable-core-platform-api-stubs-system-modules", boot: []string{"stable.core.platform.api.stubs", "core-lambda-stubs"}}},
		{"core_current", "", sdkDep{boot: []string{"core.current.stubs", "core-lambda-stubs"}, sysModules: "core-public-stubs-system-modules"}},
		{"current", "", sdkDep{boot: []string{"android_stubs_current", "core-lambda-stubs"}, java9: []string{"android_stubs_current"}, sysModules: "core-public-stubs-system-modules"}},
		{"system_current", "", sdkDep{boot: []string{"android_system_stubs_current", "core-lambda-stubs"}, java9: []string{"android_system_stubs_current"}, sysModules: "core-public-stubs-system-modules"}},
		{"module_current", "", sdkDep{boot: []string{"android_module_lib_stubs_current", "core-lambda-stubs"}, java9: []string{"android_module_lib_stubs_current"}, sysModules: "core-module_lib-stubs-system-modules"}},
		{"", "", sdkDep{sysModules: "legacy-core-platform-api-stubs-system-modules", boot: []string{"legacy.core.platform.api.stubs", "core-lambda-stubs"}, classpath: []string{"ext", "framework"}}},
		{"30", "", sdkDep{prebuiltJars: []string{"prebuilts/sdk/30/public/android.jar", "prebuilts/sdk/tools/core-lambda-stubs.jar"}}},
	}
	for _, c := range cases {
		p := objOf("sdk_version", c.sdk)
		if c.sys != "" {
			p.set("system_modules", c.sys)
		}
		got, err := g.decodeSdk(n(p), parseSdk(c.sdk))
		if err != nil || !reflect.DeepEqual(got, c.want) {
			t.Errorf("sdk %q sys %q: %+v %v, want %+v", c.sdk, c.sys, got, err, c.want)
		}
	}
	if _, err := g.decodeSdk(n(objOf("sdk_version", "none")), parseSdk("none")); err == nil {
		t.Errorf(`sdk_version "none" without system_modules must be an error`)
	}
}

func TestResourceExcludes(t *testing.T) {
	for path, want := range map[string]bool{"a/B.java": true, "a/package.html": true, "overview.html": true, "a/.x.swp": true,
		"a/.DS_Store": true, "a/b~": true, "a/b.properties": false, "a/x.html": false} {
		if got := isResourceExcluded(path); got != want {
			t.Errorf("%s: %v, want %v", path, got, want)
		}
	}
	if got := matchEscape(`a*b?c[d\e`); got != `a\*b\?c\[d\\e` {
		t.Errorf("matchEscape: %s", got)
	}
}

func TestSplitTag(t *testing.T) {
	if n, tag := splitTag(":i18n.module.intra.core.api{.public.stubs.source}"); n != ":i18n.module.intra.core.api" || tag != ".public.stubs.source" {
		t.Errorf("got %q %q", n, tag)
	}
	if n, tag := splitTag("x"); n != "x" || tag != "" {
		t.Errorf("got %q %q", n, tag)
	}
}

// javaTestGraph evaluates blueprint files in a temporary tree into a Graph with Java nodes enabled, tools at fixed fake paths.
func javaTestGraph(t *testing.T, files map[string]string, flags map[string]string) *Graph {
	t.Helper()
	tree, out := t.TempDir(), t.TempDir()
	for path, body := range files {
		p := filepath.Join(tree, path)
		if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(p, []byte(body), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	parsed, err := discover(tree)
	if err != nil {
		t.Fatal(err)
	}
	cfg := Config{Arch: "x86_64", PointerBits: 64}
	g := &Graph{tree: tree, out: out, cfg: cfg, nodes: map[key]*Node{}, baseTypes: map[string]string{}}
	for side, osName := range []string{"android", "linux_glibc"} {
		g.mods[side], _ = (&evaluator{cfg: cfg, os: osName}).modules(parsed)
	}
	fl := map[string]string{"RELEASE_TARGET_JAVA_21": "true", "RELEASE_PLATFORM_VERSION_CODENAME": "REL", "RELEASE_PLATFORM_SDK_VERSION": "36",
		"RELEASE_USE_DEX_V41": ""}
	for k, v := range flags {
		fl[k] = v
	}
	g.java = &javaGraph{tools: JavaTools{JDK: "/jdk", BuildTools: "/bt", Metalava: "/m.jar", Aconfig: "/aconfig"},
		cfg: JavaConfig{JavacHeapSize: "1M"}, release: &Release{Name: "r", flags: fl}, nodes: map[jKey]*jNode{}}
	return g
}

// ninjaEdgeVars returns the variables of the build statement whose outputs start with out (the statement line itself under "").
func ninjaEdgeVars(ninja, out string) map[string]string {
	lines := strings.Split(ninja, "\n")
	for i, l := range lines {
		if !strings.HasPrefix(l, "build "+out) {
			continue
		}
		vars := map[string]string{"": l}
		for _, v := range lines[i+1:] {
			if !strings.HasPrefix(v, "  ") {
				break
			}
			k, val, _ := strings.Cut(strings.TrimSpace(v), " = ")
			vars[k] = val
		}
		return vars
	}
	return nil
}

func TestJavaLibraryEdges(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"lib/Android.bp": `
filegroup { name: "res", path: "r", srcs: ["r/x/a.properties"] }
java_library {
    name: "core",
    srcs: ["src/**/*.java"],
    java_resources: [":res"],
    static_libs: ["dep"],
    sdk_version: "none",
    system_modules: "none",
    patch_module: "java.base",
    installable: true,
    min_sdk_version: "31",
    dxflags: ["--core-library", "--android-platform-build"],
}
java_library { name: "dep", srcs: ["d/D.java"], sdk_version: "none", system_modules: "none" }
`,
		"lib/src/p/A.java":        "",
		"lib/d/D.java":            "",
		"lib/r/x/a.properties":    "",
		"prebuilts/r8/Android.bp": `java_binary_host { name: "d8", wrapper: "d8", srcs: ["W.java"] }`,
		"prebuilts/r8/W.java":     "",
		"prebuilts/r8/d8":         "",
	}, nil)
	n := g.jnode("core", Device, "test")
	if n == nil {
		t.Fatalf("no node: %v", g.problems)
	}
	ninja := g.java.b.String()
	dir := filepath.Join(g.out, "device", "core")
	javac := ninjaEdgeVars(ninja, filepath.Join(dir, "javac", "core.jar"))
	if javac == nil {
		t.Fatalf("no javac edge:\n%s", ninja)
	}
	// collectJavacFlags: -Xlint:-dep-ann, then --patch-module=<module>=.:<out>:<classpath> (classpath = the static lib's header)
	depHeader := filepath.Join(g.out, "device", "dep", "javac-header", "dep.jar")
	if want := "-Xlint:-dep-ann --patch-module=java.base=.:" + g.out + ":" + depHeader; javac["flags"] != want {
		t.Errorf("javac flags %q, want %q", javac["flags"], want)
	}
	// system_modules "none" on device: --system=none; java 21 (RELEASE_TARGET_JAVA_21)
	if javac["bootcp"] != "--system=none" || javac["ver"] != "21" || javac["processor"] != "-proc:none" {
		t.Errorf("javac vars %v", javac)
	}
	if javac["srcs"] != "lib/src/p/A.java" {
		t.Errorf("javac sources %q: tree-relative, run from the tree root", javac["srcs"])
	}
	// resources: -C <filegroup path> -f <file> (Path.Rel below the filegroup's path)
	res := ninjaEdgeVars(ninja, filepath.Join(dir, "res", "core.jar"))
	if want := "-C " + esc(filepath.Join(g.tree, "lib/r")) + " -f " + esc(filepath.Join(g.tree, "lib/r/x/a.properties")); res == nil || res["args"] != want {
		t.Errorf("res args %v, want %q", res, want)
	}
	// combined = local javac jar + the static lib's implementation; withres = resources first, then implementation jars
	comb := ninjaEdgeVars(ninja, filepath.Join(dir, "combined", "core.jar"))
	if comb == nil || !strings.Contains(comb[""], filepath.Join(dir, "javac", "core.jar")+" "+filepath.Join(g.out, "device", "dep", "javac", "dep.jar")) {
		t.Errorf("combined %v", comb)
	}
	withres := ninjaEdgeVars(ninja, filepath.Join(dir, "withres", "core.jar"))
	if withres == nil || !strings.Contains(withres[""], filepath.Join(dir, "res", "core.jar")+" "+filepath.Join(dir, "javac", "core.jar")) {
		t.Errorf("withres %v", withres)
	}
	// dexCommonFlags: dxflags without --core-library, --debug, --min-api <min_sdk_version>; sdk none is stable, so no extra
	// --android-platform-build beyond the one dxflags give; static libs are not --lib
	d8 := ninjaEdgeVars(ninja, filepath.Join(dir, "dex", "core.jar"))
	if d8 == nil || d8["flags"] != "--android-platform-build --debug --min-api 31" || d8["zipflags"] != "--ignore_missing_files -L 0" {
		t.Errorf("d8 %v", d8)
	}
	if want := filepath.Join(dir, "dex-withres-aligned", "core.jar"); n.dex != want {
		t.Errorf("dex %s, want %s", n.dex, want)
	}
}

func TestDexMinApi(t *testing.T) {
	// ApiLevel.EffectiveVersion: an unnumbered level is the platform SDK once final (REL); not final -> 10000 -> the platform version
	// with --android-platform-build. core_platform is unstable: --android-platform-build always.
	for _, c := range []struct {
		sdk, codename, want string
	}{
		{"none", "REL", "--debug --min-api 36"},
		{"core_platform", "REL", "--debug --min-api 36 --android-platform-build"},
		{"none", "Baklava", "--debug --min-api 36 --android-platform-build"},
	} {
		g := javaTestGraph(t, map[string]string{
			"a/Android.bp": `java_library { name: "a", srcs: ["A.java"], sdk_version: "` + c.sdk + `", system_modules: "none", compile_dex: true, uncompress_dex: false }`,
			"a/A.java":     "",
		}, map[string]string{"RELEASE_PLATFORM_VERSION_CODENAME": c.codename})
		g.jnode("a", Device, "test")
		d8 := ninjaEdgeVars(g.java.b.String(), filepath.Join(g.out, "device", "a", "dex", "a.jar"))
		if d8 == nil || d8["flags"] != c.want || d8["zipflags"] != "--ignore_missing_files" {
			t.Errorf("%s %s: %v, want flags %q", c.sdk, c.codename, d8, c.want)
		}
	}
}

func TestSystemModules(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"s/Android.bp": `
java_system_modules { name: "sm", libs: ["l"] }
java_library { name: "l", srcs: ["L.java"], sdk_version: "none", system_modules: "none" }
java_library { name: "u", srcs: ["U.java"], sdk_version: "none", system_modules: "sm", java_version: "1.8" }
`,
		"s/L.java": "", "s/U.java": "",
	}, nil)
	u := g.jnode("u", Device, "test")
	if u == nil {
		t.Fatalf("%v", g.problems)
	}
	ninja := g.java.b.String()
	smDir := filepath.Join(g.out, "device", "sm", "system")
	// system_modules.go outputs: system/lib/modules, system/lib/jrt-fs.jar, system/release
	if !strings.Contains(ninja, "build "+esc(filepath.Join(smDir, "lib", "modules"))+" "+esc(filepath.Join(smDir, "lib", "jrt-fs.jar"))+" "+esc(filepath.Join(smDir, "release"))+": sysmodules") {
		t.Errorf("system modules edge missing:\n%s", ninja)
	}
	// java 8 does not use modules: the system modules' libs form -bootclasspath (transformJavaToClasses)
	javac := ninjaEdgeVars(ninja, filepath.Join(g.out, "device", "u", "javac", "u.jar"))
	lHeader := filepath.Join(g.out, "device", "l", "javac-header", "l.jar")
	if javac == nil || javac["bootcp"] != "-bootclasspath "+esc(lHeader) || javac["ver"] != "1.8" {
		t.Errorf("javac %v", javac)
	}
}
