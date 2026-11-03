// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"strings"
	"testing"
)

// android_library_import: its one aar is unzipped (aar.go unzipAAR: outputs <name>.jar, proguard.txt, AndroidManifest.xml in the aar dir,
// assets.zip beside it, R.txt), the classes jar is a static library's implementation of a dependent.
func TestAarImportClassesJar(t *testing.T) {
	g := javaTestGraph(t, appFixture(map[string]string{
		"lib/Android.bp": `
android_library_import { name: "annot", aars: ["a.aar"], sdk_version: "34" }
java_library { name: "user", srcs: ["U.java"], static_libs: ["annot"], sdk_version: "none", system_modules: "none" }
`,
		"lib/a.aar":  "",
		"lib/U.java": "",
	}), nil)
	if n := g.jnode("user", Device, "test"); n == nil {
		t.Fatalf("no node: %v", g.problems)
	}
	ninja := g.java.b.String()
	dir := filepath.Join(g.out, "device", "annot")
	jar := filepath.Join(dir, "aar", "annot.jar")
	aar := ninjaEdgeVars(ninja, jar)
	if aar == nil {
		t.Fatalf("no unzipAAR edge:\n%s", ninja)
	}
	outs := strings.Join([]string{jar, filepath.Join(dir, "aar", "proguard.txt"), filepath.Join(dir, "aar", "AndroidManifest.xml"),
		filepath.Join(dir, "assets.zip"), filepath.Join(dir, "aar", "R.txt")}, " ")
	if want := "build " + outs + ": unzipaar " + filepath.Join(g.tree, "lib", "a.aar"); !strings.HasPrefix(aar[""], want) {
		t.Errorf("unzipAAR edge %q, want prefix %q", aar[""], want)
	}
	// Soong extracts into the aar dir itself (extractedAARDir) and merges classes.jar and libs/*.jar into <name>.jar there
	if aar["outdir"] != esc(filepath.Join(dir, "aar")) || aar["classes"] != esc(jar) || aar["assets"] != esc(filepath.Join(dir, "assets.zip")) {
		t.Errorf("unzipAAR vars %v", aar)
	}
	if n := g.jnode("annot", Device, "test"); n == nil || n.header != jar {
		t.Errorf("import header %v, want %s (problems %v)", n, jar, g.problems)
	}
	comb := ninjaEdgeVars(ninja, filepath.Join(g.out, "device", "user", "combined", "user.jar"))
	if comb == nil || !strings.Contains(comb[""], jar) {
		t.Errorf("user's combined jar %v does not take the aar's classes jar %s", comb, jar)
	}
}

// AARImport resources (aar.go GenerateAndroidBuildActions): res/ of the aar compiled with --zip-prefix res, linked `--static-lib --merge-only
// --auto-add-overlay --no-static-lib-packages --manifest <manifest>` plus -I of the SDK's android.jar (a numbered SDK links its jar), the compiled
// flata the one overlay, and an R.jar made from the aar's own R.txt with non-final fields.
func TestAarImportResources(t *testing.T) {
	g := javaTestGraph(t, appFixture(map[string]string{
		"lib/Android.bp": `android_library_import { name: "annot", aars: ["a.aar"], sdk_version: "34" }`,
		"lib/a.aar":      "",
	}), nil)
	n := g.jnode("annot", Device, "test")
	if n == nil || n.aapt == nil {
		t.Fatalf("no node: %v", g.problems)
	}
	ninja := g.java.b.String()
	dir := filepath.Join(g.out, "device", "annot")
	flata := filepath.Join(dir, "flat-res", "gen_res.flata")
	cz := ninjaEdgeVars(ninja, flata)
	// aapt2CompileZip: the aar's res/ below zipdir, --pseudo-localize
	if cz == nil || cz["cflags"] != "--pseudo-localize" || cz["pattern"] != "'res/*'" || cz["resdir"] != esc(filepath.Join(dir, "aapt2", "reszip", "gen_res.flata", "res")) {
		t.Errorf("compile zip %v", cz)
	}
	manifest := filepath.Join(dir, "aar", "AndroidManifest.xml")
	// sdk.go decodeSdkDep useFiles: jars = android.jar and core-lambda-stubs.jar, both shared libraries of aapt2 link
	androidJar := g.abs("prebuilts/sdk/34/public/android.jar") + " -I " + g.abs("prebuilts/sdk/tools/core-lambda-stubs.jar")
	link := ninjaEdgeVars(ninja, filepath.Join(dir, "package-res.apk"))
	wantFlags := "--static-lib --merge-only --auto-add-overlay --no-static-lib-packages --manifest " + manifest + " -I " + androidJar
	if link == nil || link["flags"] != wantFlags {
		t.Fatalf("link %v, want flags %q", link, wantFlags)
	}
	if link["inflags"] != "-R @"+filepath.Join(dir, "aapt2", "overlay.list") || g.genFiles[filepath.Join(dir, "aapt2", "overlay.list")] != flata {
		t.Errorf("link inputs %q, overlay list %q", link["inflags"], g.genFiles[filepath.Join(dir, "aapt2", "overlay.list")])
	}
	if want := filepath.Join(dir, "R.txt"); link["rtxt"] != want {
		t.Errorf("link R.txt %q, want %q", link["rtxt"], want)
	}
	rj := ninjaEdgeVars(ninja, filepath.Join(dir, "busybox", "R.jar"))
	if rj == nil || rj["rtxt"] != filepath.Join(dir, "aar", "R.txt") || rj["manifest"] != manifest || rj["args"] != "--finalFields=false" {
		t.Errorf("R.jar %v", rj)
	}
}
