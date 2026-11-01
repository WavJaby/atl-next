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
	pctx.StaticVariable("X86_64YasmFlags", "-f elf64 -m amd64")
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
func (t *toolchainX86_64) YasmFlags() string { return "${config.X86_64YasmFlags}" }

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

func (toolchainBase) YasmFlags() string {
	return ""
}

type toolchainNoCrt struct{}

type toolchain64Bit struct {
}

type toolchainBionic struct {
	toolchainBase
}
`

// flagsLinuxHost: x86_linux_host.go's shape (the flag lists, the GCC version consts and their Split init, the prebuilt GCC root as a SourcePathVariable, toolchainLinux / toolchainGlibc embedded in the glibc type, whose Cflags joins both embedded ones' through t.<field>.Cflags(), the singleton var its factory returns); upstream registers no glibc factory for arm64.
const flagsLinuxHost = `package config

var (
	linuxCflags = []string{
		"-Wa,--noexecstack",
		"-fPIC",
		"-fno-omit-frame-pointer",
		"-U_FORTIFY_SOURCE",
		"-D_FORTIFY_SOURCE=3",
		"-fstack-protector",
		"--gcc-toolchain=${LinuxGccRoot}",
		"-fstack-protector-strong",
	}
	linuxGlibcCflags = []string{
		"--sysroot ${LinuxGccRoot}/sysroot",
	}
	linuxX8664Cflags = []string{
		"-m64",
	}
)

const (
	linuxGccVersion   = "4.8.3"
	linuxGlibcVersion = "2.17"
)

func init() {
	pctx.StaticVariable("LinuxGccVersion", linuxGccVersion)
	pctx.StaticVariable("LinuxGlibcVersion", linuxGlibcVersion)
	if p := strings.Split(linuxGccVersion, "."); len(p) > 2 {
		pctx.StaticVariable("ShortLinuxGccVersion", strings.Join(p[:2], "."))
	} else {
		pctx.StaticVariable("ShortLinuxGccVersion", linuxGccVersion)
	}
	pctx.SourcePathVariable("LinuxGccRoot",
		"prebuilts/gcc/linux-x86/host/x86_64-linux-glibc${LinuxGlibcVersion}-${ShortLinuxGccVersion}")
	pctx.StaticVariable("LinuxCflags", strings.Join(linuxCflags, " "))
	pctx.StaticVariable("LinuxGlibcCflags", strings.Join(linuxGlibcCflags, " "))
	pctx.StaticVariable("LinuxX8664Cflags", strings.Join(linuxX8664Cflags, " "))
	pctx.StaticVariable("LinuxX8664YasmFlags", "-f elf64 -m amd64")
}

type toolchainLinux struct {
	toolchainBase
	cFlags, ldFlags string
}

type toolchainLinuxX8664 struct {
	toolchain64Bit
	toolchainLinux
}

func (t *toolchainLinuxX8664) Cflags() string {
	return "${config.LinuxCflags} ${config.LinuxX8664Cflags}"
}

func (t *toolchainLinuxX8664) Cppflags() string {
	return ""
}

func (t *toolchainLinuxX8664) YasmFlags() string {
	return "${config.LinuxX8664YasmFlags}"
}

type toolchainGlibc struct {
	toolchainNoCrt
}

func (toolchainGlibc) Cflags() string {
	return "${config.LinuxGlibcCflags}"
}

type toolchainLinuxGlibcX8664 struct {
	toolchainLinuxX8664
	toolchainGlibc
}

func (t *toolchainLinuxGlibcX8664) Cflags() string {
	return t.toolchainLinuxX8664.Cflags() + " " + t.toolchainGlibc.Cflags()
}

var toolchainLinuxGlibcX8664Singleton Toolchain = &toolchainLinuxGlibcX8664{}

func linuxGlibcX8664ToolchainFactory(arch android.Arch) Toolchain {
	return toolchainLinuxGlibcX8664Singleton
}

func init() {
	registerToolchainFactory(android.Linux, android.X86_64, linuxGlibcX8664ToolchainFactory)
}
`

// flagsArm64: an arm64 device toolchain with no flags of its own: YasmFlags is toolchainBase's "".
const flagsArm64 = `package config

type toolchainArm64 struct {
	toolchainBionic
	toolchain64Bit
}

func (t *toolchainArm64) Cflags() string { return "" }
func (t *toolchainArm64) Cppflags() string { return "" }

func arm64ToolchainFactory(arch android.Arch) Toolchain {
	return &toolchainArm64{}
}

func init() {
	registerToolchainFactory(android.Android, android.Arm64, arm64ToolchainFactory)
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
		"build/soong/cc/config/global.go":         global,
		"build/soong/cc/config/x86_64_device.go":  flagsX86_64,
		"build/soong/cc/config/arm64_device.go":   flagsArm64,
		"build/soong/cc/config/x86_linux_host.go": flagsLinuxHost,
		"build/soong/cc/config/toolchain.go":      flagsBase,
		"build/soong/android/arch.go":             "package android\n\nvar (\n\tArm64 = newArch(\"arm64\", \"lib64\")\n\tX86_64 = newArch(\"x86_64\", \"lib64\")\n)\n",
		"build/soong/android/paths.go":            flagsPaths,
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
// host Common = the glibc toolchain's Cflags (toolchainLinuxX8664's LinuxCflags + LinuxX8664Cflags, then toolchainGlibc's) minus --gcc-toolchain and --sysroot with its path (toolchainLocation), then the same globals; LinuxGccRoot's version part 4.8 = linuxGccVersion's first two fields.
func TestReadGlobalFlags(t *testing.T) {
	tree := flagsTree(t, flagsGlobal)
	g, err := readGlobalFlags(tree, Config{Arch: "x86_64", PointerBits: 64}, DeviceArch{Features: []string{"ssse3", "popcnt"}})
	if err != nil {
		t.Fatal(err)
	}
	common := []string{"-O2", "-DANDROID", "-D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__", "-fno-exceptions", "-ftrivial-auto-var-init=zero"}
	hostTC := []string{"-Wa,--noexecstack", "-fPIC", "-fno-omit-frame-pointer", "-U_FORTIFY_SOURCE", "-D_FORTIFY_SOURCE=3", "-fstack-protector", "-fstack-protector-strong", "-m64"}
	gccRoot := filepath.Join(tree, "prebuilts/gcc/linux-x86/host/x86_64-linux-glibc2.17-4.8")
	for _, c := range []struct {
		name      string
		got, want []string
	}{
		{"device head", g.Head[Device], slices.Concat([]string{"-D__BIONIC_DEPRECATED_PAGE_SIZE_MACRO"}, common, []string{"-ffunction-sections", "-Wa,--noexecstack"})},
		{"host head", g.Head[Host], slices.Concat(hostTC, common)},
		{"device tail", g.Tail[Device], []string{"-m64", "-march=x86-64", "-mssse3", "-mpopcnt"}},
		{"host tail", g.Tail[Host], nil},
		{"device cpp", g.Cpp[Device], []string{"-D_LIBCPP_ENABLE_THREAD_SAFETY_ANNOTATIONS", "-fvisibility-inlines-hidden"}},
		{"host cpp", g.Cpp[Host], []string{"-D_LIBCPP_ENABLE_THREAD_SAFETY_ANNOTATIONS"}},
		{"device as", g.As[Device], []string{"-D__ASSEMBLY__"}},
		{"host as", g.As[Host], []string{"-D__ASSEMBLY__"}},
		{"external", g.External, nil},
		{"no override", g.NoOverride, nil},
		{"no override external", g.NoOverrideExt, []string{"-fcommon"}},
		{"skipped", g.Skipped, []string{"--gcc-toolchain=" + gccRoot, "--sysroot " + gccRoot + "/sysroot", "-Wall", "-Werror=bool-operation", "-Werror=implicit-function-declaration", "-Werror=return-type", "-Wimplicit-fallthrough",
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
		{"other package's variable", strings.Replace(flagsGlobal, `"-fno-exceptions",`, `"-fno-exceptions", "-I${android.Foo}",`, 1), "", "keeps a reference this evaluator cannot resolve"},
	} {
		_, err := readGlobalFlags(flagsTree(t, c.global), Config{Arch: "x86_64", PointerBits: 64, ArchVariant: c.variant}, DeviceArch{Variant: c.variant})
		if err == nil || !strings.Contains(err.Error(), c.want) {
			t.Errorf("%s: err %v, want %q", c.name, err, c.want)
		}
	}
}

// Expected yasm flags: the pctx literals (X86_64YasmFlags, LinuxX8664YasmFlags), promoted from the embedded toolchainLinuxX8664 through the host singleton; arm64: toolchainBase's "" on the device side, and no (Linux, Arm64) factory.
func TestToolchainYasm(t *testing.T) {
	tree := flagsTree(t, flagsGlobal)
	g, err := readGlobalFlags(tree, Config{Arch: "x86_64", PointerBits: 64}, DeviceArch{})
	if err != nil {
		t.Fatal(err)
	}
	want := []string{"-f", "elf64", "-m", "amd64"}
	if !slices.Equal(g.Yasm[Device], want) || !slices.Equal(g.Yasm[Host], want) || g.NoToolchain != [2]string{} {
		t.Errorf("x86_64: %q, none %q", g.Yasm, g.NoToolchain)
	}
	g, err = readGlobalFlags(tree, Config{Arch: "arm64", PointerBits: 64}, DeviceArch{})
	if err != nil {
		t.Fatal(err)
	}
	if len(g.Yasm[Device]) != 0 || g.NoToolchain[Device] != "" || !strings.Contains(g.NoToolchain[Host], "(Linux, Arm64)") {
		t.Errorf("arm64: %q, none %q", g.Yasm, g.NoToolchain)
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
