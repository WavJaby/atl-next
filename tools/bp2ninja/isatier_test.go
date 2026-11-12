// SPDX-License-Identifier: MIT

package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// libzFiles: zlib's shape on a riscv64 cross host. The device side gets the RVV defines from target.android_riscv64; adler32.c runs the tier source behind riscv_cpu_enable_rvv.
var libzFiles = map[string]string{
	"p/Android.bp":     `cc_library { name: "libz", srcs: ["adler32.c", "adler32_simd.c"], target: { android_riscv64: { cflags: ["-DADLER32_SIMD_RVV"] } } }`,
	"p/adler32.c":      "if (buf != Z_NULL && len >= 32 && riscv_cpu_enable_rvv) adler32_rvv();\n",
	"p/adler32_simd.c": "#include <riscv_vector.h>\n",
}

var libzRow = isaTier{Src: "adler32_simd.c", March: "rv64gcv_zba_zbb", Gate: "adler32.c:riscv_cpu_enable_rvv"}

// The rows are checked against the tree at the pin: the module exists, the src is one of its sources, the gate file names the symbol. Each break fails the converter run, and the good row passes.
// Expected results are the test's own tree's facts, not computed by checkISATiers.
func TestISATierRowChecks(t *testing.T) {
	cases := []struct {
		name  string
		tiers map[string]isaTier
		want  string // substring of the error; "" = passes
	}{
		{"row passes", map[string]isaTier{"libz": libzRow}, ""},
		{"module absent", map[string]isaTier{"libnope": libzRow}, "libnope names no module of the tree"},
		{"src not a source", map[string]isaTier{"libz": {Src: "missing.c", March: libzRow.March, Gate: libzRow.Gate}}, "missing.c is not one of its sources"},
		{"gate symbol absent", map[string]isaTier{"libz": {Src: libzRow.Src, March: libzRow.March, Gate: "adler32.c:riscv_cpu_enable_rvb"}}, "gate file adler32.c does not name riscv_cpu_enable_rvb"},
		{"gate file absent", map[string]isaTier{"libz": {Src: libzRow.Src, March: libzRow.March, Gate: "cpu_features.c:riscv_cpu_enable_rvv"}}, "gate file cpu_features.c"},
		{"gate malformed", map[string]isaTier{"libz": {Src: libzRow.Src, March: libzRow.March, Gate: "adler32.c"}}, "is not <file>:<symbol>"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			g := graphForCfg(t, riscvOnX86, libzFiles)
			g.tiers = c.tiers
			err := g.checkISATiers()
			switch {
			case c.want == "" && err != nil:
				t.Errorf("unexpected: %v", err)
			case c.want != "" && (err == nil || !strings.Contains(err.Error(), c.want)):
				t.Errorf("error %v, want it to contain %q", err, c.want)
			}
		})
	}
}

// ninjaFlags: the flags of the cc edge that builds obj, from the emitted ninja text.
func ninjaFlags(t *testing.T, ninja, obj string) []string {
	t.Helper()
	lines := strings.Split(ninja, "\n")
	for i, l := range lines {
		if !strings.HasPrefix(l, "build "+obj+": cc ") {
			continue
		}
		for _, f := range lines[i:] {
			if rest, ok := strings.CutPrefix(f, "  flags = "); ok {
				return strings.Fields(rest)
			}
		}
	}
	t.Fatalf("no cc edge for %s", obj)
	return nil
}

// withoutMarch: a flags list minus its -march= word.
func withoutMarch(flags []string) []string {
	return slices.DeleteFunc(slices.Clone(flags), func(f string) bool { return strings.HasPrefix(f, "-march=") })
}

// marchOf: the one -march= word of a flags list (the compile has exactly one).
func marchOf(t *testing.T, flags []string) string {
	t.Helper()
	var got []string
	for _, f := range flags {
		if strings.HasPrefix(f, "-march=") {
			got = append(got, f)
		}
	}
	if len(got) != 1 {
		t.Fatalf("-march words %q, want one", got)
	}
	return got[0]
}

// On a cross host the tier file's compile carries the row's isa plus V, on both product sides, and keeps Soong's flags; the module's other sources keep the row's isa. Without the row, every compile carries the row's isa alone.
// Break: a tier on every source, on the build side, or without the -march rewrite fails here; so does a row-less build that loses the ABI define (the zve32x error comes from the probe's compile, not from this test).
func TestISATierArgv(t *testing.T) {
	g := graphForCfg(t, riscvOnX86, libzFiles)
	g.soong.Flags = mustFlags(t, riscvOnX86, DeviceArch{})
	g.tiers = map[string]isaTier{"libz": libzRow}
	g.isaProbes = map[string]isaTierProbe{}
	for _, side := range []Side{Device, Host} {
		if g.node("libz", side, Shared, "goal") == nil {
			t.Fatalf("libz %v: %v", side, g.problems)
		}
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	if len(g.problems) > 0 {
		t.Fatal(g.problems)
	}
	for _, side := range []string{"device", "host"} {
		simd := ninjaFlags(t, ninja, filepath.Join(g.out, "obj", side, "libz", "shared", "p", "adler32_simd.c.o"))
		if got := marchOf(t, simd); got != "-march=rv64gcv_zba_zbb" {
			t.Errorf("%s adler32_simd.c: %s, want the tier", side, got)
		}
		// Soong's device toolchain sets -mno-implicit-float; the host side has no Soong toolchain here
		if side == "device" && !slices.Contains(simd, "-mno-implicit-float") {
			t.Errorf("device adler32_simd.c: -mno-implicit-float dropped: %q", simd)
		}
		rest := ninjaFlags(t, ninja, filepath.Join(g.out, "obj", side, "libz", "shared", "p", "adler32.c.o"))
		if got := marchOf(t, rest); got != "-march=rv64gc_zba_zbb" {
			t.Errorf("%s adler32.c: %s, want the row's isa", side, got)
		}
	}
	if device := ninjaFlags(t, ninja, filepath.Join(g.out, "obj", "device", "libz", "shared", "p", "adler32_simd.c.o")); !slices.Contains(device, "-DADLER32_SIMD_RVV") {
		t.Errorf("device adler32_simd.c lost its android define: %q", device)
	}
	if len(g.isaProbes) != 2 {
		t.Errorf("probes %d, want one per product side", len(g.isaProbes))
	}
	// the tier changes only the -march word of its edge
	for key, p := range g.isaProbes {
		if !slices.Equal(withoutMarch(p.Base), withoutMarch(p.Tier)) {
			t.Errorf("%s: tier argv differs from the row's beyond -march:\n%q\n%q", key, p.Base, p.Tier)
		}
	}

	plain := graphForCfg(t, riscvOnX86, libzFiles)
	plain.soong.Flags = mustFlags(t, riscvOnX86, DeviceArch{})
	plain.tiers = nil
	if plain.node("libz", Device, Shared, "goal") == nil {
		t.Fatal(plain.problems)
	}
	base := ninjaFlags(t, plain.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"}), filepath.Join(plain.out, "obj", "device", "libz", "shared", "p", "adler32_simd.c.o"))
	if got := marchOf(t, base); got != "-march=rv64gc_zba_zbb" {
		t.Errorf("row-less adler32_simd.c: %s, want the row's isa", got)
	}
	if !slices.Contains(base, "-DADLER32_SIMD_RVV") {
		t.Errorf("row-less adler32_simd.c without the RVV define: the zve32x failure would not reproduce: %q", base)
	}
}

// The probe file lists each tier compile once per module and side, sorted: base argv at the row's isa, tier argv with the extension.
func TestISATierProbeFile(t *testing.T) {
	g := graphForCfg(t, riscvOnX86, libzFiles)
	g.soong.Flags = mustFlags(t, riscvOnX86, DeviceArch{})
	g.tiers = map[string]isaTier{"libz": libzRow}
	g.isaProbes = map[string]isaTierProbe{}
	if g.node("libz", Device, Shared, "goal") == nil || g.node("libz", Device, Static, "goal") == nil {
		t.Fatal(g.problems)
	}
	g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	if err := g.writeISATierProbes(g.out); err != nil {
		t.Fatal(err)
	}
	text, err := os.ReadFile(filepath.Join(g.out, isaTierProbesFile))
	if err != nil {
		t.Fatal(err)
	}
	var probes []isaTierProbe
	if err := json.Unmarshal(text, &probes); err != nil {
		t.Fatal(err)
	}
	if len(probes) != 1 || probes[0].Module != "libz" || probes[0].Side != "device" || probes[0].Src != filepath.Join(g.tree, "p/adler32_simd.c") {
		t.Fatalf("probes %+v", probes)
	}
	if marchOf(t, probes[0].Base) != "-march=rv64gc_zba_zbb" || marchOf(t, probes[0].Tier) != "-march=rv64gcv_zba_zbb" {
		t.Errorf("probe argv: base %q, tier %q", probes[0].Base, probes[0].Tier)
	}
	want := []string{"obj/device/libz/shared/p/adler32_simd.c.o", "obj/device/libz/static/p/adler32_simd.c.o"}
	if !slices.Equal(probes[0].Objects, want) {
		t.Errorf("objects %q, want %q", probes[0].Objects, want)
	}
}

// marchArgv replaces the one -march= word and refuses an argv without exactly one.
func TestMarchArgv(t *testing.T) {
	got, err := marchArgv([]string{"-fPIC", "-march=rv64gc_zba_zbb", "-w"}, "rv64gcv_zba_zbb")
	if err != nil || !slices.Equal(got, []string{"-fPIC", "-march=rv64gcv_zba_zbb", "-w"}) {
		t.Errorf("got %q, %v", got, err)
	}
	for _, argv := range [][]string{{"-w"}, {"-march=rv64gc", "-march=rv64gc_zba"}} {
		if _, err := marchArgv(argv, "rv64gcv"); err == nil {
			t.Errorf("%q: no error", argv)
		}
	}
}
