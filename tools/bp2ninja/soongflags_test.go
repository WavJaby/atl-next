// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// flagsGlobal: an excerpt of the pinned android-16.0.0_r1 cc/config/global.go, each construct as upstream writes it (var block, init's GOOS append, StaticVariable joins, the CommonGlobalCflags / DeviceGlobalCflags / NoOverrideGlobalCflags VariableFuncs with their env branches).
const flagsGlobal = `package config

var (
	pctx = android.NewPackageContext("android/soong/cc/config")

	commonGlobalCflags = []string{
		"-O2",
		"-Wall",
		"-DANDROID",
		"-D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__",
		"-fcolor-diagnostics",
		"-fno-exceptions",
		"-gz=zstd",
	}
	commonGlobalConlyflags = []string{}
	commonGlobalAsflags = []string{"-D__ASSEMBLY__", "-fdebug-default-version=4"}
	deviceGlobalCflags = []string{"-ffunction-sections", "-Wa,--noexecstack", "-Werror=return-type"}
	deviceGlobalCppflags = []string{"-fvisibility-inlines-hidden"}
	hostGlobalCflags = []string{}
	hostGlobalCppflags = []string{}
	commonGlobalCppflags = []string{"-Wimplicit-fallthrough", "-D_LIBCPP_ENABLE_THREAD_SAFETY_ANNOTATIONS"}
	noOverrideGlobalCflags = []string{"-Werror=bool-operation"}
	noOverride64GlobalCflags = []string{}
	extraExternalCflags = []string{"-Wno-enum-compare"}
	noOverrideExternalGlobalCflags = []string{"-fcommon", "-Wno-unused"}
	llvmNextExtraCommonGlobalCflags = []string{"-Wno-everything"}
	IllegalFlags = []string{"-w"}
)

func init() {
	if runtime.GOOS == "linux" {
		commonGlobalCflags = append(commonGlobalCflags, "-fdebug-prefix-map=/proc/self/cwd=")
	}
	pctx.StaticVariable("CommonGlobalConlyflags", strings.Join(commonGlobalConlyflags, " "))
	pctx.StaticVariable("CommonGlobalAsflags", strings.Join(commonGlobalAsflags, " "))
	pctx.StaticVariable("DeviceGlobalCppflags", strings.Join(deviceGlobalCppflags, " "))
	pctx.StaticVariable("HostGlobalCppflags", strings.Join(hostGlobalCppflags, " "))
	pctx.VariableFunc("CommonGlobalCflags", func(ctx android.PackageVarContext) string {
		flags := slices.Clone(commonGlobalCflags)
		if ctx.Config().IsEnvTrue("AUTO_ZERO_INITIALIZE") {
			flags = append(flags, "-ftrivial-auto-var-init=zero")
		} else if ctx.Config().IsEnvTrue("AUTO_PATTERN_INITIALIZE") {
			flags = append(flags, "-ftrivial-auto-var-init=pattern")
		} else {
			// Default to zero initialization.
			flags = append(flags, "-ftrivial-auto-var-init=zero")
		}
		switch ctx.Config().Getenv("CLANG_DEFAULT_DEBUG_LEVEL") {
		case "debug_level_0":
			flags = append(flags, "-g0")
		default:
			flags = append(flags, "-g")
		}
		return strings.Join(flags, " ")
	})
	pctx.VariableFunc("DeviceGlobalCflags", func(ctx android.PackageVarContext) string {
		return strings.Join(deviceGlobalCflags, " ")
	})
	pctx.VariableFunc("NoOverrideGlobalCflags", func(ctx android.PackageVarContext) string {
		flags := noOverrideGlobalCflags
		if ctx.Config().IsEnvTrue("LLVM_NEXT") {
			flags = append(noOverrideGlobalCflags, llvmNextExtraCommonGlobalCflags...)
			IllegalFlags = []string{}
		}
		return strings.Join(flags, " ")
	})
	pctx.StaticVariable("NoOverride64GlobalCflags", strings.Join(noOverride64GlobalCflags, " "))
	pctx.StaticVariable("HostGlobalCflags", strings.Join(hostGlobalCflags, " "))
	pctx.StaticVariable("NoOverrideExternalGlobalCflags", strings.Join(noOverrideExternalGlobalCflags, " "))
	pctx.StaticVariable("CommonGlobalCppflags", strings.Join(commonGlobalCppflags, " "))
	pctx.StaticVariable("ExternalCflags", strings.Join(extraExternalCflags, " "))
	pctx.SourcePathVariable("RSClangBase", "prebuilts/clang/host")
}
`

// flagsX86_64: x86_64_device.go's shape (variant / feature maps, the page-size VariableFunc, the variant registration loop, the factory and its methods); toolchain.go's and bionic.go's base types.
const flagsX86_64 = `package config

var (
	x86_64Cflags = []string{"-Werror=implicit-function-declaration"}
	x86_64Cppflags = []string{}
	x86_64ArchVariantCflags = map[string][]string{
		"": []string{"-march=x86-64"},
		"haswell": []string{"-march=core-avx2"},
	}
	x86_64ArchFeatureCflags = map[string][]string{
		"ssse3":  []string{"-mssse3"},
		"popcnt": []string{"-mpopcnt"},
	}
)

func init() {
	pctx.StaticVariable("X86_64ToolchainCflags", "-m64")
	pctx.VariableFunc("X86_64Cflags", func(ctx android.PackageVarContext) string {
		flags := x86_64Cflags
		if ctx.Config().NoBionicPageSizeMacro() {
			flags = append(flags, "-D__BIONIC_NO_PAGE_SIZE_MACRO")
		} else {
			flags = append(flags, "-D__BIONIC_DEPRECATED_PAGE_SIZE_MACRO")
		}
		return strings.Join(flags, " ")
	})
	pctx.StaticVariable("X86_64Cppflags", strings.Join(x86_64Cppflags, " "))
	for variant, cflags := range x86_64ArchVariantCflags {
		pctx.StaticVariable("X86_64"+variant+"VariantCflags", strings.Join(cflags, " "))
	}
}

type toolchainX86_64 struct {
	toolchainBionic
	toolchain64Bit
	toolchainCflags string
}

func (t *toolchainX86_64) ToolchainCflags() string { return t.toolchainCflags }
func (t *toolchainX86_64) Cflags() string { return "${config.X86_64Cflags}" }
func (t *toolchainX86_64) Cppflags() string { return "${config.X86_64Cppflags}" }

func x86_64ToolchainFactory(arch android.Arch) Toolchain {
	if _, ok := x86_64ArchVariantCflags[arch.ArchVariant]; !ok {
		panic(fmt.Sprintf("Unknown x86_64 architecture version: %q", arch.ArchVariant))
	}
	toolchainCflags := []string{
		"${config.X86_64ToolchainCflags}",
		"${config.X86_64" + arch.ArchVariant + "VariantCflags}",
	}
	for _, feature := range arch.ArchFeatures {
		toolchainCflags = append(toolchainCflags, x86_64ArchFeatureCflags[feature]...)
	}
	return &toolchainX86_64{
		toolchainCflags: strings.Join(toolchainCflags, " "),
	}
}

func init() {
	registerToolchainFactory(android.Android, android.X86_64, x86_64ToolchainFactory)
}
`

const flagsBase = `package config

type toolchainBase struct {
}

func (toolchainBase) InstructionSetFlags(s string) (string, error) {
	if s != "" {
		return "", fmt.Errorf("instruction_set: %s is not a supported instruction set", s)
	}
	return "", nil
}

func (toolchainBase) ToolchainCflags() string {
	return ""
}

func (toolchainBase) Asflags() string {
	return ""
}

type toolchain64Bit struct {
}

type toolchainBionic struct {
	toolchainBase
}
`

// flagsPaths: android/paths.go IsThirdPartyPath with two of its exceptions.
const flagsPaths = `package android

var thirdPartyDirPrefixExceptions = []*regexp.Regexp{
	regexp.MustCompile("^vendor/[^/]*google[^/]*/"),
	regexp.MustCompile("^hardware/interfaces/"),
}

func IsThirdPartyPath(path string) bool {
	thirdPartyDirPrefixes := []string{"external/", "vendor/", "hardware/"}
	return HasAnyPrefix(path, thirdPartyDirPrefixes)
}
`

func flagsTree(t *testing.T, global string) string {
	tree := t.TempDir()
	for p, text := range map[string]string{
		"build/soong/cc/config/global.go":        global,
		"build/soong/cc/config/x86_64_device.go": flagsX86_64,
		"build/soong/cc/config/toolchain.go":     flagsBase,
		"build/soong/android/arch.go":            "package android\n\nvar (\n\tX86_64 = newArch(\"x86_64\", \"lib64\")\n)\n",
		"build/soong/android/paths.go":           flagsPaths,
	} {
		full := filepath.Join(tree, p)
		if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(full, []byte(text), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	return tree
}

// Expected by hand from Soong's semantics on the fixture, this build's environment (no env var, no product: else / default branches, NoBionicPageSizeMacro false, GOOS linux), and keepFlag:
// device Common = InstructionSetFlags("") "" + X86_64Cflags' -D (its -Werror dropped) + CommonGlobalCflags (its -W, -fcolor-diagnostics, -gz, -g, -fdebug-prefix-map dropped; auto-var-init's default zero) + DeviceGlobalCflags' kept two;
// Tail = -m64 + the variant's -march + each feature's flag in feature order.
func TestReadGlobalFlags(t *testing.T) {
	tree := flagsTree(t, flagsGlobal)
	g, err := readGlobalFlags(tree, Config{Arch: "x86_64", PointerBits: 64}, DeviceArch{Features: []string{"ssse3", "popcnt"}})
	if err != nil {
		t.Fatal(err)
	}
	common := []string{"-O2", "-DANDROID", "-D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__", "-fno-exceptions", "-ftrivial-auto-var-init=zero"}
	for _, c := range []struct {
		name      string
		got, want []string
	}{
		{"device head", g.Head[Device], slices.Concat([]string{"-D__BIONIC_DEPRECATED_PAGE_SIZE_MACRO"}, common, []string{"-ffunction-sections", "-Wa,--noexecstack"})},
		{"host head", g.Head[Host], common},
		{"device tail", g.Tail[Device], []string{"-m64", "-march=x86-64", "-mssse3", "-mpopcnt"}},
		{"host tail", g.Tail[Host], nil},
		{"device cpp", g.Cpp[Device], []string{"-D_LIBCPP_ENABLE_THREAD_SAFETY_ANNOTATIONS", "-fvisibility-inlines-hidden"}},
		{"host cpp", g.Cpp[Host], []string{"-D_LIBCPP_ENABLE_THREAD_SAFETY_ANNOTATIONS"}},
		{"device as", g.As[Device], []string{"-D__ASSEMBLY__"}},
		{"external", g.External, nil},
		{"no override", g.NoOverride, nil},
		{"no override external", g.NoOverrideExt, []string{"-fcommon"}},
		{"skipped", g.Skipped, []string{"-Wall", "-Werror=bool-operation", "-Werror=implicit-function-declaration", "-Werror=return-type", "-Wimplicit-fallthrough",
			"-Wno-enum-compare", "-Wno-unused", "-fcolor-diagnostics", "-fdebug-default-version=4", "-fdebug-prefix-map=/proc/self/cwd=", "-g", "-gz=zstd"}},
	} {
		if !slices.Equal(c.got, c.want) {
			t.Errorf("%s %q, want %q", c.name, c.got, c.want)
		}
	}
	// IsThirdPartyPath: a prefix, unless an exception matches
	for dir, want := range map[string]bool{"external/zlib": true, "hardware/interfaces/x": false, "hardware/ril": true, "frameworks/base": false} {
		if g.ThirdParty.is(dir) != want {
			t.Errorf("third party %s: %v", dir, !want)
		}
	}
	if f := g.common(Device, "external/zlib"); len(f) != len(g.Head[Device])+len(g.Tail[Device]) || f[len(f)-1] != "-mpopcnt" {
		t.Errorf("third-party common %q", f)
	}

	// the haswell variant: its -march, its features
	if h, err := readGlobalFlags(tree, Config{Arch: "x86_64", PointerBits: 64, ArchVariant: "haswell"}, DeviceArch{Variant: "haswell", Features: []string{"ssse3"}}); err != nil || !slices.Equal(h.Tail[Device], []string{"-m64", "-march=core-avx2", "-mssse3"}) {
		t.Errorf("haswell: %q, %v", h.Tail[Device], err)
	}
	// where Soong's factory panics, or the evaluated code asks what this build has no value for, or Soong drops the weak define: an error, never a guess
	for _, c := range []struct {
		name, global, variant, want string
	}{
		{"unknown variant", flagsGlobal, "nehalem", "panics"},
		{"unknown config method", strings.Replace(flagsGlobal, `IsEnvTrue("AUTO_ZERO_INITIALIZE")`, `ReleaseFoo()`, 1), "", "Config().ReleaseFoo: no known value"},
		{"no weak define", strings.Replace(flagsGlobal, `"-D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__",`, "", 1), "", "lacks -D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__"},
		{"unregistered variable", strings.Replace(flagsGlobal, `pctx.StaticVariable("ExternalCflags"`, `pctx.StaticVariable("OtherCflags"`, 1), "", "${config.ExternalCflags} is registered by no"},
	} {
		_, err := readGlobalFlags(flagsTree(t, c.global), Config{Arch: "x86_64", PointerBits: 64, ArchVariant: c.variant}, DeviceArch{Variant: c.variant})
		if err == nil || !strings.Contains(err.Error(), c.want) {
			t.Errorf("%s: err %v, want %q", c.name, err, c.want)
		}
	}
}

// Soong's command-line order (cc/builder.go): global flags, then the language's global list, the module's flags, includes, no-override last; asm without the module's cflags or the no-override flags.
func TestGlobalFlagsOrder(t *testing.T) {
	g := graphFor(t, map[string]string{"external/z/Android.bp": `cc_library { name: "libz", srcs: ["a.c", "b.cpp", "c.S"], cflags: ["-fno-function-sections"], cppflags: ["-fexceptions"] }`})
	g.soong.Flags = GlobalFlags{
		Head:          [2][]string{Device: {"-ffunction-sections", "-fno-exceptions"}},
		Tail:          [2][]string{Device: {"-m64"}},
		External:      []string{"-DEXT"},
		Cpp:           [2][]string{Device: {"-fvisibility-inlines-hidden"}},
		As:            [2][]string{Device: {"-D__ASSEMBLY__"}},
		NoOverrideExt: []string{"-fcommon"},
		ThirdParty:    ThirdParty{Prefixes: []string{"external/"}},
	}
	n := g.node("libz", Device, Shared, "t")
	if n == nil {
		t.Fatal(g.problems)
	}
	f := g.compileFlags(n)
	at := slices.Index[[]string]
	if !(at(f.c, "-ffunction-sections") < at(f.c, "-DEXT") && at(f.c, "-DEXT") < at(f.c, "-m64") && at(f.c, "-m64") < at(f.c, "-fno-function-sections")) || f.c[len(f.c)-1] != "-fcommon" {
		t.Errorf("c %q", f.c)
	}
	if !(at(f.cxx, "-fno-exceptions") < at(f.cxx, "-fvisibility-inlines-hidden") && at(f.cxx, "-fvisibility-inlines-hidden") < at(f.cxx, "-fexceptions")) || at(f.c, "-fvisibility-inlines-hidden") >= 0 {
		t.Errorf("cxx %q", f.cxx)
	}
	if at(f.asm, "-D__ASSEMBLY__") < 0 || at(f.asm, "-fno-function-sections") >= 0 || at(f.asm, "-fcommon") >= 0 {
		t.Errorf("asm %q", f.asm)
	}
}
