// SPDX-License-Identifier: MIT

package main

import (
	"crypto/sha256"
	"encoding/hex"
	"go/ast"
	"go/parser"
	"go/token"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
)

// artFixture: a plugin with the shapes art/build/art.go and codegen.go use (registration in init, a defaults factory wrapping another, hooks as identifiers, a codegen closure, installCodegenCustomizer, an install hook), not their code.
const artFixture = `package art

func init() {
	android.AddNeverAllowRules(android.NeverAllow().ModuleType("art_cc_library_static"))
	android.RegisterModuleType("art_cc_library_static", artStaticLibrary)
	android.RegisterModuleType("art_cc_binary", artBinary)
	android.RegisterModuleType("art_cc_test", artTest)
	android.RegisterModuleType("art_global_defaults", artGlobalDefaultsFactory)
}

var (
	staticLibrary          = moduleType{true, true, false}
	staticAndSharedLibrary = moduleType{true, true, true}
	binary                 = moduleType{false, false, false}
)

func artGlobalDefaultsFactory() android.Module {
	module := artDefaultsFactory()
	android.AddLoadHook(module, addImplicitFlags)
	android.AddLoadHook(module, globalDefaults)
	return module
}

func artDefaultsFactory() android.Module {
	c := &codegenProperties{}
	module := cc.DefaultsFactory(c)
	android.AddLoadHook(module, func(ctx android.LoadHookContext) { codegen(ctx, c, staticAndSharedLibrary) })
	return module
}

func artStaticLibrary() android.Module {
	module := cc.LibraryStaticFactory()
	installCodegenCustomizer(module, staticLibrary)
	android.AddLoadHook(module, addImplicitFlags)
	return module
}

func artBinary() android.Module {
	module := cc.BinaryFactory()
	android.AddLoadHook(module, addImplicitFlags)
	android.AddLoadHook(module, customLinker)
	android.AddInstallHook(module, addTestcasesFile)
	return module
}

func artTest() android.Module {
	module := cc.NewTest(android.HostAndDeviceSupported).Init()
	installCodegenCustomizer(module, binary)
	android.AddLoadHook(module, prefer32Bit)
	return module
}
`

func artTypes(t *testing.T, src string) (map[string]artType, error) {
	t.Helper()
	p, err := parseArtPackage(map[string][]byte{"art.go": []byte(src)})
	if err != nil {
		t.Fatal(err)
	}
	return p.moduleTypes()
}

// Soong runs load hooks in the order AddLoadHook adds them, a wrapped factory's first (art_global_defaults: the defaults factory's codegen, then its own two).
func TestArtModuleTypes(t *testing.T) {
	types, err := artTypes(t, artFixture)
	if err != nil {
		t.Fatal(err)
	}
	want := map[string]artType{
		"art_cc_library_static": {"cc_library_static", []artHook{{"codegen", artModuleType{true, true, false}}, {name: "addImplicitFlags"}}},
		"art_cc_binary":         {"cc_binary", []artHook{{name: "addImplicitFlags"}, {name: "customLinker"}}},
		"art_cc_test":           {"cc_test", []artHook{{"codegen", artModuleType{}}, {name: "prefer32Bit"}}},
		"art_global_defaults":   {"cc_defaults", []artHook{{"codegen", artModuleType{true, true, true}}, {name: "addImplicitFlags"}, {name: "globalDefaults"}}},
	}
	if !reflect.DeepEqual(types, want) {
		t.Errorf("types %+v\nwant %+v", types, want)
	}
	// a hook, constructor or statement the converter has no reading for fails instead of being dropped
	for _, c := range []struct{ name, from, to, want string }{
		{"new hook", "android.AddLoadHook(module, customLinker)", "android.AddLoadHook(module, newHook)", "load hook newHook has no translation"},
		{"new constructor", "cc.BinaryFactory()", "cc.ObjectFactory()", "unknown constructor ObjectFactory"},
		{"new statement", "android.AddInstallHook(module, addTestcasesFile)", "if module != nil { return module }", "cannot read: if module != nil"},
	} {
		if _, err := artTypes(t, strings.Replace(artFixture, c.from, c.to, 1)); err == nil || !strings.Contains(err.Error(), c.want) {
			t.Errorf("%s: err %v, want %q", c.name, err, c.want)
		}
	}
}

func TestArtMirrored(t *testing.T) {
	src := "package art\n\nfunc hook() { a() }\n\ntype props struct{ Cflags []string }\n"
	p, err := parseArtPackage(map[string][]byte{"art.go": []byte(src)})
	if err != nil {
		t.Fatal(err)
	}
	// the declarations' own text, from `func` / the type name to the closing brace
	digest := func(s string) string {
		sum := sha256.Sum256([]byte(s))
		return hex.EncodeToString(sum[:])[:12]
	}
	want := map[string]string{"hook": digest("func hook() { a() }"), "props": digest("props struct{ Cflags []string }")}
	if err := p.checkMirrored(want); err != nil {
		t.Fatal(err)
	}
	want["hook"] = digest("func hook() { b() }")
	want["gone"] = "000000000000"
	err = p.checkMirrored(want)
	if err == nil || !strings.Contains(err.Error(), "gone (gone), hook (now "+digest("func hook() { a() }")+")") {
		t.Errorf("err %v", err)
	}
}

// codegen.go defaultDeviceCodegenArches: a 64-bit device arch brings the 32-bit one it runs, sorted.
func TestArtDeviceCodegenArches(t *testing.T) {
	for arch, want := range map[string][]string{"x86_64": {"x86", "x86_64"}, "arm64": {"arm", "arm64"}, "riscv64": {"riscv64"}} {
		if got := defaultDeviceCodegenArches([]string{arch}); !slices.Equal(got, want) {
			t.Errorf("%s: %q, want %q", arch, got, want)
		}
	}
}

// android/config.go PrebuiltOS's nested switch on runtime.GOOS / runtime.GOARCH.
func TestArtPrebuiltOS(t *testing.T) {
	src := "package android\n\nfunc (c *config) PrebuiltOS() string {\n\tswitch runtime.GOOS {\n\tcase \"linux\":\n\t\tswitch runtime.GOARCH {\n\t\tcase \"amd64\":\n\t\t\treturn \"linux-x86\"\n" +
		"\t\tcase \"arm64\":\n\t\t\treturn \"linux-arm64\"\n\t\t}\n\tcase \"darwin\":\n\t\treturn \"darwin-x86\"\n\t}\n\treturn \"\"\n}\n"
	f, err := parser.ParseFile(token.NewFileSet(), "config.go", src, 0)
	if err != nil {
		t.Fatal(err)
	}
	fd := f.Decls[0].(*ast.FuncDecl)
	for _, c := range []struct{ goos, goarch, want string }{{"linux", "amd64", "linux-x86"}, {"linux", "arm64", "linux-arm64"}, {"linux", "riscv64", ""}} {
		if got := prebuiltOS(fd, c.goos, c.goarch); got != c.want {
			t.Errorf("%s/%s: %q, want %q", c.goos, c.goarch, got, c.want)
		}
	}
}

func TestArtReadBarrierDefault(t *testing.T) {
	for _, c := range []struct {
		line string
		ok   bool
	}{
		// soong_config.mk:150 at android-16.0.0_r1
		{"$(call add_json_bool, ArtUseReadBarrier,                 $(call invert_bool,$(filter false,$(PRODUCT_ART_USE_READ_BARRIER))))", true},
		// a derivation that is false for an unset variable
		{"$(call add_json_bool, ArtUseReadBarrier,                 $(filter true,$(PRODUCT_ART_USE_READ_BARRIER)))", false},
	} {
		tree := t.TempDir()
		path := filepath.Join(tree, "build/make/core/soong_config.mk")
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(c.line+"\n"), 0o644); err != nil {
			t.Fatal(err)
		}
		if err := readBarrierDefault(tree); (err == nil) != c.ok {
			t.Errorf("%q: err %v", c.line, err)
		}
	}
}

// The hooks on modules: art_global_defaults' own flags travel to a plain cc module through defaults; an art_cc_library_static gets its codegen blocks and the implicit flags.
// Order (Soong): defaults' properties before the module's, top level before target blocks; within a module its own lines, then each hook's appends in hook order; codegen arch by arch, srcs once after them.
func TestArtHooks(t *testing.T) {
	g := graphFor(t, map[string]string{"art/Android.bp": `
art_global_defaults {
    name: "art_defaults",
    cflags: ["-DOWN"],
    codegen: {
        arm: { cflags: ["-DCG_arm"] },
        x86: { cflags: ["-DCG_x86"] },
        x86_64: { cflags: ["-DCG_x86_64"] },
    },
}
cc_library_headers { name: "hdrs", defaults: ["art_defaults"], host_supported: true }
art_cc_library_static {
    name: "libsupp",
    defaults: ["art_defaults"],
    host_supported: true,
    srcs: ["a.cc"],
    cflags: ["-DLIB"],
    codegen: {
        x86: { srcs: ["x86.cc", "common.cc"] },
        x86_64: {
            srcs: ["x86_64.cc", "common.cc"],
            static_libs: ["libcg"],
            static: { whole_static_libs: ["libwhole"] },
            shared: { shared_libs: ["libshared_only"] },
        },
        arm64: { srcs: ["arm64.cc"] },
    },
}
`})
	types, err := artTypes(t, artFixture)
	if err != nil {
		t.Fatal(err)
	}
	a := &artPlugin{types: types, codegenArches: [2][]string{Device: {"x86", "x86_64"}, Host: {"arm", "arm64", "riscv64", "x86", "x86_64"}},
		baseAddress: [2]string{"0xD", "0xH"}, clangPath: "prebuilts/clang/host/linux-x86/clang-rX"}
	g.loadArt(a)
	// art.go globalFlags with none of its variables set, no sanitizer and ArtUseReadBarrier true
	global := []string{"-O3", "-DART_DEFAULT_GC_TYPE_IS_CMC", "-DART_USE_READ_BARRIER=1", "-DART_READ_BARRIER_TYPE_IS_BAKER=1", "-DART_USE_GENERATIONAL_GC=1", "-DART_USE_TLAB=1",
		"-DART_STACK_OVERFLOW_GAP_arm=8192", "-DART_STACK_OVERFLOW_GAP_arm64=8192", "-DART_STACK_OVERFLOW_GAP_riscv64=8192", "-DART_STACK_OVERFLOW_GAP_x86=8192", "-DART_STACK_OVERFLOW_GAP_x86_64=8192",
		"-DART_PAGE_SIZE_AGNOSTIC=1", "-DUSE_D8_DESUGAR=1"}
	// deviceFlags / hostFlags: frame limit, the base address config.go returns, the deltas' defaults; the host's clang path
	side := func(base string) []string {
		return []string{"-Wframe-larger-than=1744", "-DART_FRAME_SIZE_LIMIT=1744", "-DART_BASE_ADDRESS=" + base, "-DART_BASE_ADDRESS_MIN_DELTA=(-0x1000000)", "-DART_BASE_ADDRESS_MAX_DELTA=0x1000000"}
	}
	implicit := []string{"-DART_TARGET", "-DART_TARGET_ANDROID"}
	clang := `-DART_CLANG_PATH="prebuilts/clang/host/linux-x86/clang-rX"`
	// defaults' target block: codegen of the side's arches, then addImplicitFlags (device only), then globalDefaults' side flags
	defDevice := slices.Concat([]string{"-DCG_x86", "-DCG_x86_64"}, implicit, side("0xD"))
	defHost := slices.Concat([]string{"-DCG_arm", "-DCG_x86", "-DCG_x86_64"}, side("0xH"), []string{clang})
	for _, c := range []struct {
		name string
		side Side
		kind Kind
		want []string
	}{
		{"hdrs", Device, Headers, slices.Concat([]string{"-DOWN"}, global, defDevice)},
		{"hdrs", Host, Headers, slices.Concat([]string{"-DOWN"}, global, defHost)},
		{"libsupp", Device, Static, slices.Concat([]string{"-DOWN"}, global, []string{"-DLIB"}, defDevice, implicit)},
		{"libsupp", Host, Static, slices.Concat([]string{"-DOWN"}, global, []string{"-DLIB"}, defHost)},
	} {
		p := g.flatten(g.lookup(c.name, c.side), c.side, c.kind)
		if got := p.strs("cflags"); !slices.Equal(got, c.want) {
			t.Errorf("%s %s: cflags %q\nwant %q", c.name, c.side, got, c.want)
		}
		if p.has("codegen") {
			t.Errorf("%s %s: codegen left in the properties", c.name, c.side)
		}
	}
	dev := g.flatten(g.lookup("libsupp", Device), Device, Static)
	// x86 then x86_64, common.cc once; arm64 is not a device codegen arch
	if got, want := dev.strs("srcs"), []string{"a.cc", "x86.cc", "common.cc", "x86_64.cc"}; !slices.Equal(got, want) {
		t.Errorf("device srcs %q, want %q", got, want)
	}
	// a static library's codegen takes its static part, never its shared one
	if got := dev.strs("static_libs"); !slices.Equal(got, []string{"libcg"}) {
		t.Errorf("static_libs %q", got)
	}
	if got := dev.strs("whole_static_libs"); !slices.Equal(got, []string{"libwhole"}) {
		t.Errorf("whole_static_libs %q", got)
	}
	if dev.has("shared_libs") {
		t.Errorf("shared_libs %q from a static library's codegen", dev.strs("shared_libs"))
	}
	if got, want := g.flatten(g.lookup("libsupp", Host), Host, Static).strs("srcs"), []string{"a.cc", "arm64.cc", "x86.cc", "common.cc", "x86_64.cc"}; !slices.Equal(got, want) {
		t.Errorf("host srcs %q, want %q", got, want)
	}
	// asflags: globalFlags' read-barrier defines only
	if got := g.flatten(g.lookup("hdrs", Device), Device, Headers).strs("asflags"); !slices.Equal(got, []string{"-DART_USE_READ_BARRIER=1", "-DART_READ_BARRIER_TYPE_IS_BAKER=1"}) {
		t.Errorf("asflags %q", got)
	}
	if g.baseType("art_cc_library_static") != "cc_library_static" || g.baseType("art_global_defaults") != "cc_defaults" {
		t.Errorf("base types %v", g.baseTypes)
	}
}
