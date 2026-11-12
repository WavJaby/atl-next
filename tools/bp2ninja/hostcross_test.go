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

// riscvOnX86: the riscv64 host built on an x86_64 build machine (xtask native: arches::build_machine's row and pointer_bits).
var riscvOnX86 = Config{Arch: "riscv64", PointerBits: 64, HostCross: riscvCross, BuildArch: "x86_64", BuildBits: 64}

// arm64 has no (linux_glibc, arm64) toolchain in Soong, as a riscv64 host has none (TestToolchainYasm): its host side takes the cross flags ahead of the common ones and no x86 toolchain flag.
// The build side is the build machine's linux_glibc toolchain without cross flags: what the x86_64 host side of a plain x86_64 build gets.
// Break: a host side that takes the x86 toolchain (-m64, -march=x86-64, --gcc-toolchain) or loses HostLink, or a build side with the cross target, fails here.
func TestHostCrossHostSide(t *testing.T) {
	arm64 := DeviceArch{Variant: "armv8-a"}
	plain := mustFlags(t, Config{Arch: "arm64", PointerBits: 64, ArchVariant: arm64.Variant}, arm64)
	cross := mustFlags(t, Config{Arch: "arm64", PointerBits: 64, ArchVariant: arm64.Variant, HostCross: riscvCross, BuildArch: "x86_64", BuildBits: 64}, arm64)
	x86 := mustFlags(t, Config{Arch: "x86_64", PointerBits: 64}, DeviceArch{})
	if cross.NoToolchain[Build] != "" || !slices.Equal(cross.Head[Build], x86.Head[Host]) || !slices.Equal(cross.Tail[Build], x86.Tail[Host]) || !slices.Equal(cross.Cpp[Build], x86.Cpp[Host]) {
		t.Errorf("build head %q tail %q, want the x86_64 host side's %q %q", cross.Head[Build], cross.Tail[Build], x86.Head[Host], x86.Tail[Host])
	}
	if _, ok := plain.Head[Build]; ok {
		t.Error("a plain build has a build side")
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

// A cross host builds every cc goal's host variant: the device variant needs the guest's bionic (plan B1). The build machine's own host keeps the device side.
func TestCrossGoalSide(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `cc_binary_host { name: "tool" }
cc_library { name: "lib" }
cc_library { name: "optout", device_supported: false }`})
	if got := g.ccGoalSide("lib"); got != Device {
		t.Fatalf("build machine's host: side %v, want device", got)
	}
	g.cfg.HostCross = riscvCross
	for goal, want := range map[string]Side{"tool": Host, "lib": Host, "optout": Host} {
		if got := g.ccGoalSide(goal); got != want {
			t.Errorf("%s: side %v, want %v", goal, got, want)
		}
	}
}

// The tool rule (Soong's BuildOSTarget for a tools dependency): a genrule's tool, its link closure and a generator tool are build-side nodes on a cross host, the consumer and its own libraries stay host-side; on the build machine's own host they are one host node.
// Break: a tool edge left on Host puts an ATL-host binary where the build executes it (Exec format error); checkMachines then fails, which the hand-made crossing edge at the end shows.
func TestCrossToolEdges(t *testing.T) {
	files := map[string]string{"p/Android.bp": `
cc_binary_host { name: "tool", srcs: ["tool.cc"], static_libs: ["libt"] }
cc_library_host_static { name: "libt", srcs: ["t.cc"] }
genrule { name: "gen", tools: ["tool"], cmd: "$(location tool) > $(out)", out: ["g.h"] }
cc_library { name: "lib", srcs: ["a.cc"], generated_headers: ["gen"], static_libs: ["libu"], host_supported: true }
cc_library_static { name: "libu", srcs: ["u.cc"], host_supported: true }
`, "p/tool.cc": "", "p/t.cc": "", "p/a.cc": "", "p/u.cc": ""}
	for _, c := range []struct {
		name       string
		cfg        Config
		tool, prod Side
	}{
		{"x86_64 on x86_64", Config{Arch: "x86_64", PointerBits: 64}, Host, Host},
		{"riscv64 on x86_64", riscvOnX86, Build, Host},
	} {
		g := graphForCfg(t, c.cfg, files)
		lib := g.node("lib", Host, Shared, "test")
		if lib == nil {
			t.Fatalf("%s: lib not built: %v", c.name, g.problems)
		}
		gen := g.nodes[key{"gen", c.prod, Gen}]
		if gen == nil || len(gen.rule.tools) != 1 {
			t.Fatalf("%s: genrule gen on %v with tools %v", c.name, c.prod, gen)
		}
		tool := gen.rule.tools[0]
		if tool.side != c.tool || len(tool.deps.static) != 1 || tool.deps.static[0].side != c.tool {
			t.Errorf("%s: tool on %v (static %v), want %v", c.name, tool.side, tool.deps.static, c.tool)
		}
		if lib.deps.static[0].side != c.prod {
			t.Errorf("%s: libu on %v, want %v", c.name, lib.deps.static[0].side, c.prod)
		}
		if gt := g.buildTool("tool", lib); gt != tool {
			t.Errorf("%s: generator tool %v is not the genrule's %v", c.name, gt, tool)
		}
		if err := g.checkMachines(); err != nil {
			t.Errorf("%s: %v", c.name, err)
		}
		if c.tool == Build {
			tool.deps.static = append(tool.deps.static, lib.deps.static[0])
			if err := g.checkMachines(); err == nil || !strings.Contains(err.Error(), "build/tool -> host/libu (link): on the host machine, want build") {
				t.Errorf("%s: a build tool linking a host library: %v", c.name, err)
			}
		}
	}
}

// A provided module resolves per machine: a cross host's build tool links the build machine's copy (-build-provided), its products the host's (-provided); on the build machine both are the one -provided set.
func TestProvidedPerMachine(t *testing.T) {
	files := map[string]string{"p/Android.bp": `
cc_binary_host { name: "tool", srcs: ["tool.cc"], shared_libs: ["libprov"] }
genrule { name: "gen", tools: ["tool"], cmd: "$(location tool) > $(out)", out: ["g.h"] }
cc_library { name: "lib", srcs: ["a.cc"], generated_headers: ["gen"], shared_libs: ["libprov"], host_supported: true }
`, "p/tool.cc": "", "p/a.cc": ""}
	const hostLib, buildLib = "/host/libprov.so", "/build/libprov.so"
	for _, c := range []struct {
		name             string
		cfg              Config
		build            map[string]Provided
		toolLib, prodLib string
	}{
		{"x86_64 on x86_64", Config{Arch: "x86_64", PointerBits: 64}, nil, hostLib, hostLib},
		{"riscv64 on x86_64", riscvOnX86, map[string]Provided{"libprov": {Lib: buildLib}}, buildLib, hostLib},
	} {
		g := graphForCfg(t, c.cfg, files)
		g.provided = map[string]Provided{"libprov": {Lib: hostLib}}
		g.buildProvided = c.build
		lib := g.node("lib", Host, Shared, "test")
		if lib == nil {
			t.Fatalf("%s: lib not built: %v", c.name, g.problems)
		}
		tool := g.nodes[key{"gen", lib.side, Gen}].rule.tools[0]
		for _, n := range []struct {
			n    *Node
			want string
		}{{tool, c.toolLib}, {lib, c.prodLib}} {
			if len(n.n.deps.shared) != 1 || n.n.deps.shared[0].out != n.want || n.n.deps.shared[0].side != n.n.side {
				t.Errorf("%s: %s links %v, want %s on its own side", c.name, n.n.dir(), n.n.deps.shared, n.want)
			}
		}
		if err := g.checkMachines(); err != nil {
			t.Errorf("%s: %v", c.name, err)
		}
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

// compat/ is read by device-side compiles only: a cross host's plan of host objects needs no compat/, so writeCompat (which wants the libc provider for api-level.h) is skipped.
// Break: a device object in the plan brings the libc requirement back -- writeCompat fails with "no provider for libc".
func TestCompatNeedsDeviceObjs(t *testing.T) {
	g := &Graph{order: []*Node{{key: key{name: "lib", side: Host, kind: Shared}, objs: []string{"lib.o"}}}}
	if g.hasDeviceObjs() {
		t.Fatal("host objects only: device objects reported")
	}
	g.order = append(g.order, &Node{key: key{name: "lib", side: Device, kind: Shared}, objs: []string{"lib-dev.o"}})
	if !g.hasDeviceObjs() {
		t.Fatal("device object not reported")
	}
	err := writeCompat(t.TempDir(), t.TempDir(), map[string]Provided{})
	if err == nil || !strings.Contains(err.Error(), "no provider for libc") {
		t.Errorf("writeCompat without a libc provider: %v", err)
	}
}
