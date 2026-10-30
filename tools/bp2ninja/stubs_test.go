// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

// Expected values from build/soong/java/sdk_library.go (initApiScope's droidstubsArgs, getGeneratedApiScopes, the component names),
// sdk_library_internal.go (createDroidstubs / stubsLibraryProps / createTopLevelStubsLibrary), droidstubs.go and Soong's own metalava
// command for i18n.module.intra.core.api.stubs.source (sbox textproto of the reference build).

func TestDroidstubsArgs(t *testing.T) {
	priv := "android.annotation.SystemApi(client=android.annotation.SystemApi.Client.PRIVILEGED_APPS)"
	mod := "android.annotation.SystemApi(client=android.annotation.SystemApi.Client.MODULE_LIBRARIES)"
	ss := "android.annotation.SystemApi(client=android.annotation.SystemApi.Client.SYSTEM_SERVER)"
	q := func(s string) string { return "'" + s + "'" }
	for _, c := range []struct {
		s    *apiScope
		want []string
	}{
		{scopePublic, nil},
		{scopeSystem, []string{"--show-annotation", q(priv)}},
		// module-lib extends system: shows its own annotation, keeps system's for stub purposes
		{scopeModuleLib, []string{"--show-annotation", q(mod), "--show-for-stub-purposes-annotation", q(priv)}},
		{scopeTest, []string{"--show-annotation", "android.annotation.TestApi", "--show-for-stub-purposes-annotation", q(priv)}},
		{scopeSystemServer, []string{"--show-annotation", q(ss), "--hide-annotation", "android.annotation.Hide", "--hide", "InternalClasses"}},
	} {
		if got := c.s.droidstubsArgs(); !reflect.DeepEqual(got, c.want) {
			t.Errorf("%s: %q, want %q", c.s.name, got, c.want)
		}
	}
}

func TestSdkLibScopes(t *testing.T) {
	names := func(l []*apiScope) []string {
		var out []string
		for _, s := range l {
			out = append(out, s.name)
		}
		return out
	}
	// legacy: public always, system + test when building against standard libraries
	if got := names(sdkLibScopes(objOf("sdk_version", "current"))); !reflect.DeepEqual(got, []string{"public", "system", "test"}) {
		t.Errorf("legacy std libs: %v", got)
	}
	if got := names(sdkLibScopes(objOf("sdk_version", "none"))); !reflect.DeepEqual(got, []string{"public"}) {
		t.Errorf("legacy core: %v", got)
	}
	// any explicit enabled: the others take their non-legacy default (public only)
	p := objOf("sdk_version", "current")
	p.set("module_lib", objOf("enabled", true))
	if got := names(sdkLibScopes(p)); !reflect.DeepEqual(got, []string{"public", "module-lib"}) {
		t.Errorf("explicit: %v", got)
	}
}

func TestSdkLibComponents(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"i/Android.bp": `
java_sdk_library {
    name: "lib",
    srcs: ["src/**/*.java"],
    api_dir: "api/intra",
    stub_only_libs: ["stub-annotations"],
    annotations_enabled: true,
    droiddoc_options: ["--hide-annotation libcore.api.Hide "],
    public: { api_surface: "intra-core" },
    sdk_version: "none",
    system_modules: "sm",
    build_from_text_stub: false,
}
`,
	}, nil)
	ds := g.lookupJava("lib.stubs.source", Device)
	if ds == nil || ds.Type != "droidstubs" {
		t.Fatalf("lib.stubs.source: %+v", ds)
	}
	p := ds.Props
	// createDroidstubs: libs = libs + static_libs + stub_only_libs; args = droiddoc_options, then --hide of the legacy-allowed
	// warnings (matches the reference build's i18n intra-core command); public scope keeps javadoc
	if !reflect.DeepEqual(p.strs("libs"), []string{"stub-annotations"}) || p.str("api_surface") != "intra-core" || p.str("system_modules") != "sm" {
		t.Errorf("droidstubs props %+v", p.m)
	}
	wantArgs := "--hide-annotation libcore.api.Hide  --hide HiddenSuperclass --hide BroadcastBehavior --hide DeprecationMismatch --hide MissingPermission --hide SdkConstant --hide Todo"
	if p.str("args") != wantArgs || !p.boolean("output_javadoc_comments", false) {
		t.Errorf("args %q", p.str("args"))
	}
	if p.str("previous_api") != ":lib.api.combined.public.latest" || p.obj("check_api").obj("current").str("api_file") != "api/intra/current.txt" {
		t.Errorf("previous_api %q, check_api %+v", p.str("previous_api"), p.obj("check_api"))
	}
	// from-source stubs: srcs = the droidstubs, Java 1.8, sdk none (core library), stub-annotations added with annotations_enabled
	fs := g.lookupJava("lib.stubs.from-source", Device)
	if fs == nil || !reflect.DeepEqual(fs.Props.strs("srcs"), []string{":lib.stubs.source"}) || fs.Props.str("java_version") != "1.8" ||
		fs.Props.str("sdk_version") != "none" || !reflect.DeepEqual(fs.Props.strs("libs"), []string{"stub-annotations", "stub-annotations"}) {
		t.Errorf("from-source %+v", fs)
	}
	// top-level: static_libs the from-source library (build_from_text_stub false)
	top := g.lookupJava("lib.stubs", Device)
	if top == nil || !reflect.DeepEqual(top.Props.strs("static_libs"), []string{"lib.stubs.from-source"}) {
		t.Errorf("top %+v", top)
	}
	if g.lookupJava("lib.stubs.system", Device) != nil {
		t.Errorf("system scope is not generated for a core library")
	}
	if c := sdkLibTagComponent("lib", ".public.stubs.source"); c != "lib.stubs.source" {
		t.Errorf("tag component %q", c)
	}
}

func TestDroidstubsSandbox(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"d/Android.bp": `
droiddoc_exported_dir { name: "ann", path: "ann" }
droidstubs { name: "ds", srcs: ["A.java"], merge_inclusion_annotations_dirs: ["ann"], sdk_version: "none", system_modules: "none" }
`,
		"d/A.java":                                  "",
		"d/ann/A.annotated.java":                    "",
		"build/soong/java/metalava/Android.bp":      `filegroup { name: "metalava-config-files", srcs: ["*-config.xml"] }`,
		"build/soong/java/metalava/main-config.xml": "",
	}, nil)
	n := g.jnode("ds", Device, "test")
	if n == nil {
		t.Fatalf("%v", g.problems)
	}
	v := ninjaEdgeVars(g.java.b.String(), n.srcjar)
	// sbox layout: tree files tree-relative; the merged annotations dir is read from inside the sandbox
	if v["sources"] != "d/A.java" || v["extra"] != "d/ann/A.annotated.java" || !strings.Contains(v["post"], "--merge-inclusion-annotations d/ann") {
		t.Errorf("metalava vars %v", v)
	}
	if !strings.Contains(v["post"], "--config-file "+esc(filepath.Join(g.tree, "build/soong/java/metalava/main-config.xml"))) ||
		!strings.Contains(v["post"], "--exclude-documentation-from-stubs") {
		t.Errorf("post %q", v["post"])
	}
}

func TestGensrcsCommands(t *testing.T) {
	// paths.go genPathWithExtAndTrimExt: tree path with the extension replaced; trim_extension cut, then output_extension appended
	ins := []srcFile{{path: "lib/x/A.txt", root: "lib"}, {path: "/o/gen/device/g/sub/B.txt", root: "/o/gen/device/g", gen: &Node{}}}
	outs, cmd := gensrcsCommands("/o/gen/device/m", "sed 's/a/b/' $(in) > $(out)", "java", "", ins, func(string, ...any) {})
	if want := []string{"/o/gen/device/m/gensrcs/lib/x/A.java", "/o/gen/device/m/gensrcs/sub/B.java"}; !reflect.DeepEqual(outs, want) {
		t.Errorf("outs %v, want %v", outs, want)
	}
	// `bash -c` + proptools.ShellEscape of each command: single-quoted, each ' as '"'"'
	want := `bash -c 'sed '"'"'s/a/b/'"'"' lib/x/A.txt > /o/gen/device/m/gensrcs/lib/x/A.java' && ` +
		`bash -c 'sed '"'"'s/a/b/'"'"' /o/gen/device/g/sub/B.txt > /o/gen/device/m/gensrcs/sub/B.java'`
	if cmd != want {
		t.Errorf("cmd %q, want %q", cmd, want)
	}
	outs, _ = gensrcsCommands("/g", "x", "java", ".in.txt", []srcFile{{path: "a/b.in.txt", root: "a"}}, func(string, ...any) {})
	if !reflect.DeepEqual(outs, []string{"/g/gensrcs/a/b.java"}) {
		t.Errorf("trim: %v", outs)
	}
	outs, _ = gensrcsCommands("/g", "x", "", ".tmpl", []srcFile{{path: "a/b.java.tmpl", root: "a"}}, func(string, ...any) {})
	if !reflect.DeepEqual(outs, []string{"/g/gensrcs/a/b.java"}) {
		t.Errorf("trim only: %v", outs)
	}
}
