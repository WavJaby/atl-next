// SPDX-License-Identifier: MIT

package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
)

// isa-tiers.toml rows (xtask isa_tiers.rs), route B: one source of a module is compiled with the host row's isa plus one extension, because upstream runs it only behind a run-time CPU check (zlib adler32_simd.c: riscv_cpu_enable_rvv from AT_HWCAP).
// Every other source of the module keeps the row's isa, so an extension reaches only the listed file. The tier applies on a cross host's product sides (device and host source sets: both run on the ATL host), as the row's isa does.
// No per-block rule: Android riscv64 target blocks apply on the cross host like on any other, and a V-needing file that is not a row fails its compile.
const isaTierProbesFile = "isa-tier-probes.json"

// isaTier: one row as xtask writes it (-isa-tiers, keyed by module).
type isaTier struct {
	Src   string `json:"src"`   // the module's source, relative to its directory, as Android.bp lists it
	March string `json:"march"` // -march of that source: the row's isa with the extension (xtask isa_tiers::march)
	Gate  string `json:"gate"`  // <file>:<symbol>: <file> in the module's directory names <symbol>, the run-time check the source runs behind
}

// isaTierProbe: one tier source's compile argv, for xtask's probe (gate a): Base (the row's isa) must fail naming the extension, Tier must compile.
type isaTierProbe struct {
	Module string   `json:"module"`
	Side   string   `json:"side"`
	Src    string   `json:"src"` // absolute
	Base   []string `json:"base"`
	Tier   []string `json:"tier"`
}

// checkISATiers: each row's module is in the tree, its src is one of the module's sources in some variant at the pin, and its gate file names the symbol.
// A wrong row fails this run, not a build step later; a row whose module the goals do not build is still checked here.
func (g *Graph) checkISATiers() error {
	var bad []string
	for _, name := range sortedKeys(keysOf(g.tiers)) {
		t := g.tiers[name]
		var mods []*Module
		for _, side := range []Side{Device, Host} {
			mods = append(mods, g.mods[side][name]...)
		}
		if len(mods) == 0 {
			bad = append(bad, name+" names no module of the tree")
			continue
		}
		listed := false
		for _, side := range []Side{Device, Host} {
			for _, m := range g.mods[side][name] {
				listed = listed || listsSrc(g.flatten(m, side, Shared), t.Src)
			}
		}
		if !listed {
			bad = append(bad, fmt.Sprintf("%s: %s is not one of its sources", name, t.Src))
		}
		file, symbol, ok := strings.Cut(t.Gate, ":")
		if !ok || file == "" || symbol == "" {
			bad = append(bad, fmt.Sprintf("%s: gate %q is not <file>:<symbol>", name, t.Gate))
			continue
		}
		text, err := readFile(g.abs(filepath.Join(mods[0].Dir, file)))
		if err != nil {
			bad = append(bad, fmt.Sprintf("%s: gate file %s: %v", name, file, err))
		} else if !regexp.MustCompile(`\b` + regexp.QuoteMeta(symbol) + `\b`).Match(text) {
			bad = append(bad, fmt.Sprintf("%s: gate file %s does not name %s", name, file, symbol))
		}
	}
	if len(bad) > 0 {
		return fmt.Errorf("isa-tiers.toml: %s", strings.Join(bad, "; "))
	}
	return nil
}

// isaTierArgv: n's compile argv for source s, with the tier's -march when s is its module's tier source on a cross host's product side; the other argv is returned unchanged.
// The base and tier argv are recorded for the probe (writeISATierProbes).
func (g *Graph) isaTierArgv(n *Node, s string, argv []string) []string {
	t, ok := g.tiers[n.mod.Name]
	if !ok || g.cfg.machine(n.side) != "host" || filepath.Clean(s) != filepath.Join(n.mod.Dir, t.Src) {
		return argv
	}
	tier, err := marchArgv(argv, t.March)
	if err != nil {
		g.problem("isa-tiers.toml: %s %s: %v", n.mod.Name, s, err)
		return argv
	}
	key := n.side.String() + "\x00" + n.mod.Name + "\x00" + s
	g.isaProbes[key] = isaTierProbe{Module: n.mod.Name, Side: n.side.String(), Src: g.abs(s), Base: argv, Tier: tier}
	return tier
}

// marchArgv: argv with its one -march= replaced by march; a compile without exactly one is refused (the host row's -march is the only one a cross product side has).
func marchArgv(argv []string, march string) ([]string, error) {
	out := slices.Clone(argv)
	found := 0
	for i, f := range out {
		if strings.HasPrefix(f, "-march=") {
			out[i] = "-march=" + march
			found++
		}
	}
	if found != 1 {
		return nil, fmt.Errorf("compile argv has %d -march= (want the host row's one)", found)
	}
	return out, nil
}

// writeISATierProbes: the tier compiles, sorted, to <out>/isa-tier-probes.json, which xtask native probes before ninja. Written whenever -isa-tiers is given, "[]" when no row's module is in the goals.
func (g *Graph) writeISATierProbes(out string) error {
	if g.tiers == nil {
		return nil
	}
	probes := make([]isaTierProbe, 0, len(g.isaProbes))
	for _, k := range sortedKeys(keysOf(g.isaProbes)) {
		probes = append(probes, g.isaProbes[k])
	}
	text, err := json.MarshalIndent(probes, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(out, isaTierProbesFile), append(text, '\n'), 0o644)
}
