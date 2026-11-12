// SPDX-License-Identifier: MIT

package main

import (
	"slices"
	"testing"
)

// riscvRust: the riscv64 host's arches.toml clang, sysroot, isa and rust triple, with the features of its isa past the base (arches::rustc_target_features).
var riscvRust = HostCross{Target: "riscv64-linux-gnu", Sysroot: "/s", ISA: "rv64gc_zba_zbb", Libcxx: "/s/c++", RustTarget: "riscv64gc-unknown-linux-gnu", RustFeatures: "+zba,+zbb"}

// TestCrossRustFlags: a cross host's product crates take the row's target and features last; the build machine's crates keep rustc's default target; an x86_64 build takes no target.
// Break: Soong's riscv64 Rust features (+V,+Zba,+Zbb,+Zbs) in place of the row's, or a build-side crate given the cross target, fails here.
func TestCrossRustFlags(t *testing.T) {
	cross := &Graph{cfg: Config{Arch: "riscv64", PointerBits: 64, HostCross: riscvRust, BuildArch: "x86_64", BuildBits: 64}}
	want := []string{"--target", "riscv64gc-unknown-linux-gnu", "-C", "target-feature=+zba,+zbb"}
	for _, s := range []Side{Device, Host} {
		got := cross.rustSideFlags(s)
		if !slices.Equal(got[len(got)-len(want):], want) {
			t.Errorf("%v: rust flags %q, want %q last", s, got, want)
		}
	}
	if got := cross.rustSideFlags(Build); slices.Contains(got, "--target") || slices.Contains(got, "target-feature=+zba,+zbb") {
		t.Errorf("build side takes the host target: %q", got)
	}
	plain := &Graph{cfg: Config{Arch: "x86_64", PointerBits: 64}}
	if got := plain.rustSideFlags(Host); slices.Contains(got, "--target") {
		t.Errorf("x86_64 host takes a target: %q", got)
	}
}

// TestCrossRustLinkArgs: a cross host's Rust binary links with its side's cc link flags and the lld flags; an x86_64 build's binary links with the driver alone.
func TestCrossRustLinkArgs(t *testing.T) {
	cross := &Graph{cfg: Config{Arch: "riscv64", PointerBits: 64, HostCross: riscvRust, BuildArch: "x86_64", BuildBits: 64}}
	cross.soong.Flags.Link = map[Side][]string{Device: riscvRust.linkFlags(), Host: riscvRust.linkFlags()}
	cross.soong.Lldflags = []string{"-fuse-ld=lld"}
	n := &Node{key: key{side: Host, kind: Binary}, rust: &rustCrate{}}
	got, _ := cross.rustLinkArgs(n, "clang++")
	want := []string{"-C", "linker=clang++", "-C", "link-arg=--target=riscv64-linux-gnu", "-C", "link-arg=--sysroot=/s", "-C", "link-arg=-fuse-ld=lld"}
	if !slices.Equal(got, want) {
		t.Errorf("cross link args %q, want %q", got, want)
	}
	plain := &Graph{cfg: Config{Arch: "x86_64", PointerBits: 64}}
	plain.soong.Lldflags = []string{"-fuse-ld=lld"}
	if got, _ := plain.rustLinkArgs(n, "clang++"); !slices.Equal(got, []string{"-C", "linker=clang++"}) {
		t.Errorf("x86_64 link args %q, want the driver alone", got)
	}
}
