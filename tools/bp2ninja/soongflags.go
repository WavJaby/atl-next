// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"slices"
	"strings"

	"android/soong/android"
	ccconfig "android/soong/cc/config"
	rustconfig "android/soong/rust/config"

	"github.com/google/blueprint"
)

// GlobalFlags: the compile flags Soong gives every C/C++/asm compile before and after the module's own (cc/compiler.go compilerFlags, cc/builder.go), per side, evaluated by the pinned build/soong/cc/config package itself (soongExpand).
// Soong's command lines (builder.go): C = Common + Conly + module + NoOverride; C++ = Common + Cpp + module + NoOverride; asm = Common + As + module. Common is [instruction set, tc.Cflags, CommonGlobalCflags, <Hod>GlobalCflags, ExternalCflags if third-party, tc.ToolchainCflags].
// The device side takes the bionic toolchain for (Android, arch), the host side the linux_glibc one for (Linux, arch) (-fstack-protector-strong, -D_FORTIFY_SOURCE=3, -fno-omit-frame-pointer, -m64, ...).
// Every flag passes toolchainLocation and keepFlag; what they drop is in Skipped.
type GlobalFlags struct {
	Head, Tail     map[Side][]string // Common up to <Hod>GlobalCflags, and tc.ToolchainCflags: ExternalCflags goes between them
	External       []string          // ExternalCflags, for third-party module dirs (thirdParty)
	Conly, Cpp, As map[Side][]string
	NoOverride     []string // NoOverrideGlobalCflags + NoOverride64GlobalCflags (64-bit arches)
	NoOverrideExt  []string // NoOverrideExternalGlobalCflags, third-party only
	Skipped        []string // dropped by toolchainLocation (with its argument) or keepFlag, unique, sorted
	// per Side: the toolchain's YasmFlags, the format flags of every .asm compile (cc/compiler.go Global.YasmFlags: `-f elf64 -m amd64` for x86_64); toolchainBase's "" on arches without .asm sources
	Yasm map[Side][]string
	// per Side: why Soong has no toolchain there ("" = it has one): linux_glibc exists for x86/x86_64 only, so an arm64 or riscv64 host's host side has none, and an .asm source there is a problem, not a run failure
	NoToolchain map[Side]string
	// per bionic Side (native bridge): the toolchain's ClangTriple, which AddTargetFlags (cc/compiler.go) suffixes with the API level as `-target`; glibc sides compile for the distro clang's default triple
	Triple map[Side]string
	// per bionic Side: rust/config's RustTriple, rustc's --target there (rust/compiler.go); its std is rustup's for that target (rust-toolchain.toml toolchain). Glibc sides compile for rustc's default host triple
	RustTriple map[Side]string
	// per Side: the side's machine target on its links (HostCross.linkFlags on a cross host's product sides); empty where the side links for the build machine or a bionic triple
	Link map[Side][]string
}

// hod: the side's Host-or-Device prefix of Soong's global flag variables (<Hod>GlobalCflags).
func hod(s Side) string {
	if s.android() {
		return "Device"
	}
	return "Host"
}

// keepFlag: a Soong global flag the converter passes. Dropped are the flags that never change the object's code or data, only diagnostics or DWARF:
//   - -W* warnings and warning-errors (the converter compiles with -w -Wno-error: distro clang warns differently from AOSP's); -Wa, -Wl, -Wp are pass-throughs and kept
//   - -fcolor-diagnostics, -fmessage-length=*: diagnostic layout (ANSI codes would also reach the triage's log parsing)
//   - -g*, -fdebug-*: debug info (clang's -g never changes generated code); with it every compile bypasses the compiler cache (debugInfo) and every out dir path lands in the objects
//
// Everything else is kept, codegen and preprocessor alike (-ftrivial-auto-var-init=zero, -fno-strict-aliasing, -fno-exceptions, -D_FORTIFY_SOURCE=2, -march, ...).
func keepFlag(f string) bool {
	switch {
	case strings.HasPrefix(f, "-W"):
		return strings.HasPrefix(f, "-Wa,") || strings.HasPrefix(f, "-Wl,") || strings.HasPrefix(f, "-Wp,")
	case f == "-fcolor-diagnostics", strings.HasPrefix(f, "-fmessage-length="):
		return false
	case strings.HasPrefix(f, "-g"), strings.HasPrefix(f, "-fdebug-"):
		return false
	}
	return true
}

// toolchainLocation: how many argv words a Soong toolchain flag naming its prebuilt toolchain spans (0 = not one): --gcc-toolchain=<dir> and --sysroot <dir> (also --sysroot=<dir>, --gcc-toolchain <dir>).
// linux_glibc points clang at prebuilts/gcc/linux-x86/host/x86_64-linux-glibc2.17-4.8 (GCC 4.8 runtime, glibc 2.17 sysroot), the companion of Soong's prebuilt clang (not fetched: the flags naming it are dropped below).
// The converter's toolchain is the distro clang with the build machine's glibc, libc++ and GCC runtime, compile and link alike; 2.17 headers would compile against one C library and link another (and the distro libc++, built on the host's).
// A toolchain choice, like the compiler itself, not a flag AOSP code relies on; dropped with its argument and listed in Skipped.
func toolchainLocation(f string) int {
	switch {
	case f == "--sysroot", f == "--gcc-toolchain":
		return 2
	case strings.HasPrefix(f, "--sysroot="), strings.HasPrefix(f, "--gcc-toolchain="):
		return 1
	}
	return 0
}

// HostCross: the target of a host whose arch is not the build machine's (xtask native: the host row's clang, sysroot, isa and rust). Zero = the build machine's own, no flags.
// The distro clang compiles and links for any target it knows given --target and --sysroot (the sysroot's libraries, crt objects and GCC runtime dir are found under it). -march pins the ISA baseline: no V or Zbs reaches an object, except an isa-tiers.toml row's file (isatier.go; readelf -A Tag_RISCV_arch).
// Rust follows the same row: rustc --target and the isa's extensions as target features (rustFlags), so no product crate takes the build machine's default target or Soong's riscv64 Rust features.
// No -fuse-ld here: Soong's lldflags give every host link -fuse-ld=lld already (readLldflags).
type HostCross struct {
	Target       string // clang --target
	Sysroot      string // the sysroot directory (a fetched sysroot_debs row)
	ISA          string // -march
	Libcxx       string // the sysroot's libc++ headers (xtask cross::libcxx_dir): the one c++/v1 holding __config
	RustTarget   string // rustc --target of the host's product crates (arches.toml rust); its std is a rustup_target row
	RustFeatures string // rustc -C target-feature: the isa's extensions past the base (arches::rustc_target_features); empty for the base alone
}

// set: the host is cross. main.go requires Target, Sysroot, ISA, Libcxx and RustTarget together; RustFeatures may be empty.
func (h HostCross) set() bool { return h != (HostCross{}) }

// rustFlags: the rustc flags of a cross host's product crates, in place of rustc's default target (Soong's RustTriple: --target, then the target's features).
func (h HostCross) rustFlags() []string {
	if !h.set() {
		return nil
	}
	flags := []string{"--target", h.RustTarget}
	if h.RustFeatures != "" {
		flags = append(flags, "-C", "target-feature="+h.RustFeatures)
	}
	return flags
}

// compileFlags: the target flags of every host compile, ahead of the Soong toolchain's (instruction set first, compiler.go).
func (h HostCross) compileFlags() []string {
	if !h.set() {
		return nil
	}
	return []string{"--target=" + h.Target, "--sysroot=" + h.Sysroot, "-march=" + h.ISA}
}

// linkFlags: the target and sysroot of every host link. clang adds the sysroot's library and GCC runtime dirs itself (checked on riscv64: a link with no -L).
func (h HostCross) linkFlags() []string {
	if !h.set() {
		return nil
	}
	return []string{"--target=" + h.Target, "--sysroot=" + h.Sysroot}
}

// UpstreamToolchain: what a build outside the converter (xtask upstream.rs: CMake) needs to compile and link like a host module: one derivation, read through -upstream-toolchain.
type UpstreamToolchain struct {
	Libcxx  string   `json:"libcxx"`
	Compile []string `json:"compile"`
	Link    []string `json:"link"`
}

// upstreamToolchainOf: the converter's libc++ and host target flags. Link adds -fuse-ld=lld, which the converter's host links get from Soong's lldflags (readLldflags) and which a cross link needs: the distro's ld is the build machine's.
func upstreamToolchainOf(cxx string, h HostCross) (UpstreamToolchain, error) {
	dir, err := libcxxProvider(cxx, h)
	if err != nil {
		return UpstreamToolchain{}, err
	}
	return UpstreamToolchain{
		Libcxx: dir,
		// non-nil: the build machine's empty list must read as [] (json: nil -> null)
		Compile: append([]string{}, h.compileFlags()...),
		Link:    append(h.linkFlags(), "-fuse-ld=lld"),
	}, nil
}

// common: the side's Global.CommonFlags for a module in dir.
func (f *GlobalFlags) common(side Side, dir string) []string {
	out := slices.Clone(f.Head[side])
	if thirdParty(dir) {
		out = append(out, f.External...)
	}
	return append(out, f.Tail[side]...)
}

// noOverride: the flags after the module's own on C and C++ compiles.
func (f *GlobalFlags) noOverride(dir string) []string {
	out := slices.Clone(f.NoOverride)
	if thirdParty(dir) {
		out = append(out, f.NoOverrideExt...)
	}
	return out
}

// thirdParty: Soong's android.IsThirdPartyPath for a module dir (its prefixes end in "/").
func thirdParty(dir string) bool { return android.IsThirdPartyPath(dir + "/") }

func newGlobalFlags() GlobalFlags {
	return GlobalFlags{Head: map[Side][]string{}, Tail: map[Side][]string{}, Conly: map[Side][]string{}, Cpp: map[Side][]string{}, As: map[Side][]string{},
		Yasm: map[Side][]string{}, NoToolchain: map[Side]string{}, Triple: map[Side]string{}, RustTriple: map[Side]string{}, Link: map[Side][]string{}}
}

// soongCcConfig, soongRustConfig: the Soong toolchain packages read as data. The converter compiles the pinned build/soong (xtask converter.rs go.work), so their package vars, inits, VariableFuncs and toolchain factories run as in Soong.
const (
	soongCcConfig   = "android/soong/cc/config"
	soongRustConfig = "android/soong/rust/config"
)

// soongConfig: the android.Config those packages' VariableFuncs read. NullConfig is this build's: no Soong environment variable set (IsEnvTrue / IsEnvFalse false, Getenv ""), every product variable unset (no product: NoBionicPageSizeMacro false).
// Its soong out dir only has to lie outside the tree-relative source paths: SourcePathVariable refuses a path under it.
var soongConfig = android.NullConfig("out", "out/soong")

// soongExpand: str with every ninja variable reference expanded as in Soong's build.ninja, `${config.X}` naming variable X of package pkg.
func soongExpand(pkg, str string) (string, error) {
	return blueprint.EvalNinjaString(str, soongConfig, pkg)
}

// soongCall: f's result, or its panic as an error: Soong's toolchain lookups panic on an unregistered (os, arch) or an unknown arch variant.
func soongCall[T any](f func() T) (v T, err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("Soong panics: %v", r)
		}
	}()
	return f(), nil
}

// soongArchType: Soong's android.ArchType named arch (android/arch.go archTypeList).
func soongArchType(arch string) (android.ArchType, error) {
	for _, a := range android.ArchTypeList() {
		if a.Name == arch {
			return a, nil
		}
	}
	return android.ArchType{}, fmt.Errorf("Soong has no arch %q", arch)
}

// soongOS: the android.OsType the side's toolchain factory is registered for (cc/config registerToolchainFactory); the host side is linux_glibc.
func soongOS(s Side) android.OsType {
	if s.android() {
		return android.Android
	}
	return android.Linux
}

// soongArches: android.Arch per side (android/arch.go decodeArch): the device its arches.toml variant and features, the native bridge guest its row's variant, the host Soong's baseline.
// Known gap: Soong also sets NB features from androidArchFeatureMap[arch][variant] (arch.go:1874-1878; x86_64 "" = ssse3, sse4, sse4_1, sse4_2, popcnt), so this side misses their -m flags; fixing it changes x86 NB output.
func soongArches(cfg Config, dev DeviceArch) (map[Side]android.Arch, error) {
	out := map[Side]android.Arch{}
	for _, side := range cfg.sides() {
		name, _ := cfg.archOf(side)
		t, err := soongArchType(name)
		if err != nil {
			return nil, err
		}
		a := android.Arch{ArchType: t}
		switch {
		case side == Device:
			a.ArchVariant, a.ArchFeatures = dev.Variant, dev.Features
		case side.kind == nativeBridgeSide:
			a.ArchVariant = cfg.NativeBridgeArchVariant
		}
		out[side] = a
	}
	return out, nil
}

// readGlobalFlags: GlobalFlags for the device arch cfg (its arches.toml variant and Soong's feature list for it, DeviceArch).
func readGlobalFlags(cfg Config, dev DeviceArch) (GlobalFlags, error) {
	g := newGlobalFlags()
	skipped := map[string]bool{}
	get := func(expr string) ([]string, error) {
		s, err := soongExpand(soongCcConfig, expr)
		if err != nil {
			return nil, err
		}
		var out []string
		fields := strings.Fields(s)
		for i := 0; i < len(fields); i++ {
			f := fields[i]
			if n := toolchainLocation(f); n > 0 {
				skipped[strings.Join(fields[i:min(i+n, len(fields))], " ")] = true
				i += n - 1
			} else if keepFlag(f) {
				out = append(out, f)
			} else {
				skipped[f] = true
			}
		}
		return out, nil
	}
	arches, err := soongArches(cfg, dev)
	if err != nil {
		return g, err
	}
	type part struct {
		dst  map[Side][]string
		vars []string // ${config.X}, in order
	}
	for _, side := range cfg.sides() {
		for _, p := range []part{
			{g.Head, []string{"CommonGlobalCflags", hod(side) + "GlobalCflags"}},
			{g.Conly, []string{"CommonGlobalConlyflags"}},
			{g.Cpp, []string{"CommonGlobalCppflags", hod(side) + "GlobalCppflags"}},
			{g.As, []string{"CommonGlobalAsflags"}},
		} {
			for _, v := range p.vars {
				l, err := get("${config." + v + "}")
				if err != nil {
					return g, err
				}
				p.dst[side] = append(p.dst[side], l...)
			}
		}
	}
	// each side's toolchain around the global lists (compiler.go: instruction set first, tc.Cflags before CommonGlobalCflags, tc.ToolchainCflags last; tc.Cppflags after the global C++ lists, tc.Asflags before CommonGlobalAsflags)
	for _, side := range cfg.sides() {
		os, arch := soongOS(side), arches[side]
		get, all := get, get
		// a cross host's product side: the host row's isa (HostCross.ISA) is its only ISA authority; the toolchain's -march/-mcpu (riscv64_device.go rv64gcv_zba_zbb_zbs) is dropped into Skipped, never overridden by order
		if cfg.machine(side) == "host" {
			get = func(expr string) ([]string, error) {
				l, err := all(expr)
				return slices.DeleteFunc(l, func(f string) bool {
					drop := strings.HasPrefix(f, "-march=") || strings.HasPrefix(f, "-mcpu=")
					if drop {
						skipped[f] = true
					}
					return drop
				}), err
			}
		}
		tc, err := soongCall(func() ccconfig.Toolchain { return ccconfig.FindToolchain(os, arch) })
		if err != nil {
			// a host's arch variant is always "", so its only failure is an unregistered (os, arch)
			if side.android() {
				return g, fmt.Errorf("cc toolchain (%s, %s): %w", os, arch.ArchType, err)
			}
			g.NoToolchain[side] = "Soong registers no toolchain for (" + os.String() + ", " + arch.ArchType.String() + ")"
			continue
		}
		isaExpr, err := tc.InstructionSetFlags("")
		if err != nil {
			return g, err
		}
		isa, err := get(isaExpr)
		if err != nil {
			return g, err
		}
		cflags, err := get(tc.Cflags())
		if err != nil {
			return g, err
		}
		g.Head[side] = slices.Concat(isa, cflags, g.Head[side])
		if g.Tail[side], err = get(tc.ToolchainCflags()); err != nil {
			return g, err
		}
		cpp, err := get(tc.Cppflags())
		if err != nil {
			return g, err
		}
		g.Cpp[side] = append(g.Cpp[side], cpp...)
		as, err := get(tc.Asflags())
		if err != nil {
			return g, err
		}
		g.As[side] = append(as, g.As[side]...)
		if !side.glibc() {
			t, err := get(tc.ClangTriple())
			if err != nil {
				return g, err
			}
			if len(t) != 1 {
				return g, fmt.Errorf("ClangTriple for (%s, %s) is %q, not one word", os, arch.ArchType, t)
			}
			g.Triple[side] = t[0]
		}
		// assembler arguments, not clang flags: keepFlag does not apply
		yasm, err := soongExpand(soongCcConfig, tc.YasmFlags())
		if err != nil {
			return g, err
		}
		g.Yasm[side] = strings.Fields(yasm)
	}
	// the cross host's target flags lead every product side's compiles (device and host source sets: both run on the ATL host), ahead of the toolchain's (if it has one); its link flags go on their links (Link)
	for _, side := range cfg.sides() {
		if cfg.machine(side) == "host" {
			g.Head[side] = slices.Concat(cfg.HostCross.compileFlags(), g.Head[side])
			g.Link[side] = cfg.HostCross.linkFlags()
		}
	}
	if err := readRustTriples(cfg, arches, g.RustTriple); err != nil {
		return g, err
	}
	if g.External, err = get("${config.ExternalCflags}"); err != nil {
		return g, err
	}
	if g.NoOverride, err = get("${config.NoOverrideGlobalCflags}"); err != nil {
		return g, err
	}
	if cfg.PointerBits == 64 {
		l, err := get("${config.NoOverride64GlobalCflags}")
		if err != nil {
			return g, err
		}
		g.NoOverride = append(g.NoOverride, l...)
	}
	if g.NoOverrideExt, err = get("${config.NoOverrideExternalGlobalCflags}"); err != nil {
		return g, err
	}
	if !slices.Contains(g.Head[Device], weakAvailability) {
		return g, fmt.Errorf("${config.CommonGlobalCflags} lacks %s (deviceCompat assumes it)", weakAvailability)
	}
	g.Skipped = sortedKeys(skipped)
	return g, nil
}

// readRustTriples: rust/config's RustTriple per bionic side, from the toolchain rust/config registers for (Android, arch) with the same android.Arch cc's gets.
func readRustTriples(cfg Config, arches map[Side]android.Arch, dst map[Side]string) error {
	for _, side := range cfg.sides() {
		if side.glibc() {
			continue
		}
		os, arch := soongOS(side), arches[side]
		tc, err := soongCall(func() rustconfig.Toolchain { return rustconfig.FindToolchain(os, arch) })
		if err != nil {
			return fmt.Errorf("rust toolchain (%s, %s): %w", os, arch.ArchType, err)
		}
		t, err := soongExpand(soongRustConfig, tc.RustTriple())
		if err != nil {
			return err
		}
		if t == "" || strings.ContainsAny(t, " \t") {
			return fmt.Errorf("RustTriple for (%s, %s) is %q, not one word", os, arch.ArchType, t)
		}
		dst[side] = t
	}
	return nil
}
