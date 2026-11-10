// SPDX-License-Identifier: MIT

package main

import (
	"slices"
	"strings"
	"testing"
)

// riscvCross: the riscv64 host's arches.toml clang and isa, and a stand-in for its deps.toml sysroot directory.
var riscvCross = HostCross{Target: "riscv64-linux-gnu", Sysroot: "/cache/sysroot/riscv64-sysroot/root", ISA: "rv64gc_zba_zbb"}

func TestHostCrossFlags(t *testing.T) {
	if got, want := riscvCross.compileFlags(), []string{"--target=riscv64-linux-gnu", "--sysroot=/cache/sysroot/riscv64-sysroot/root", "-march=rv64gc_zba_zbb"}; !slices.Equal(got, want) {
		t.Errorf("compile flags %q, want %q", got, want)
	}
	if got, want := riscvCross.linkFlags(), []string{"--target=riscv64-linux-gnu", "--sysroot=/cache/sysroot/riscv64-sysroot/root"}; !slices.Equal(got, want) {
		t.Errorf("link flags %q, want %q", got, want)
	}
	if none := (HostCross{}); none.set() || none.compileFlags() != nil || none.linkFlags() != nil {
		t.Error("the build machine's own host has target flags")
	}
}

// arm64 is the fixture arch with no (Linux, Arm64) toolchain, as a riscv64 host has none (TestToolchainYasm): its host side takes the cross flags ahead of the common ones and no x86 toolchain flag.
// Break: a host side that takes the x86 toolchain (-m64, -march=x86-64, --gcc-toolchain) or loses HostLink fails here.
func TestHostCrossHostSide(t *testing.T) {
	tree := flagsTree(t, flagsGlobal)
	plain, err := readGlobalFlags(tree, Config{Arch: "arm64", PointerBits: 64}, DeviceArch{})
	if err != nil {
		t.Fatal(err)
	}
	cross, err := readGlobalFlags(tree, Config{Arch: "arm64", PointerBits: 64, HostCross: riscvCross}, DeviceArch{})
	if err != nil {
		t.Fatal(err)
	}
	if plain.NoToolchain[Host] == "" || cross.NoToolchain[Host] == "" {
		t.Fatalf("arm64 host has a Soong toolchain: %q", plain.NoToolchain[Host])
	}
	if want := slices.Concat(riscvCross.compileFlags(), plain.Head[Host]); !slices.Equal(cross.Head[Host], want) {
		t.Errorf("host head %q, want %q", cross.Head[Host], want)
	}
	for _, f := range cross.Head[Host] {
		if f == "-m64" || strings.HasPrefix(f, "-march=x86") || strings.Contains(f, "gcc-toolchain") {
			t.Errorf("x86 flag %q on the cross host", f)
		}
	}
	if !slices.Equal(cross.HostLink, riscvCross.linkFlags()) || plain.HostLink != nil {
		t.Errorf("host link %q, plain %q", cross.HostLink, plain.HostLink)
	}
	if !slices.Equal(cross.Head[Device], plain.Head[Device]) || !slices.Equal(cross.Tail[Device], plain.Tail[Device]) {
		t.Error("the device side changed with the host's cross target")
	}
}

// A cross host's libc++ headers must lie under its sysroot: the distro's own include dir is the build machine's libc++. The sysroot prefix ends at a separator, so root-other is not under root.
func TestLibcxxInSysroot(t *testing.T) {
	for _, c := range []struct {
		dir   string
		cross HostCross
		ok    bool
	}{
		{"/usr/lib/llvm-19/include/c++/v1", riscvCross, false},
		{riscvCross.Sysroot + "/usr/include/c++/v1", riscvCross, true},
		{riscvCross.Sysroot + "-other/usr/include/c++/v1", riscvCross, false},
		{"/usr/lib/llvm-19/include/c++/v1", HostCross{}, true},
	} {
		if err := libcxxInSysroot(c.dir, c.cross); (err == nil) != c.ok {
			t.Errorf("%s under %q: err %v, want ok %v", c.dir, c.cross.Sysroot, err, c.ok)
		}
	}
}
