// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"sort"
	"strings"
)

// Goal atl-sepolicy: the device's SELinux policy and the context files servicemanager and the seapp evaluator read, built as Soong builds them
// (system/sepolicy/build/soong: policy.go se_policy_conf / se_policy_cil / se_policy_binary, selinux_contexts.go buildGeneralContexts, selinux.go,
// flags.go), then the service access table (-sepolicy-av) queried from the binary. Outputs under <out>/gen/sepolicy ([sepolicyOuts]).
//   - Policy: one se_policy_conf over flagging_macros + plat public + plat private + plat vendor (system/sepolicy/vendor: the HAL default domains
//     ATL runs, hal_keymint_default ...) + the -sepolicy-system-ext-private dir (ATL's rows) in policyConfOrder; checkpolicy -C -M -c 30,
//     technical_debt.cil appended (plat_sepolicy.cil's additional_cil_files), secilc -m -M true -G -c 30 -> binary with neverallows checked.
//     A device compiles plat_sepolicy.cil, the plat-filtered system_ext_sepolicy.cil and vendor_sepolicy.cil (plat_vendor over the versioned
//     public policy) and init's secilc joins them with the mapping files: the same union of rules, so filter_out and the versioning are not reproduced.
//   - m4 defines: a user build of a full-treble, non-recovery, non-asan, non-coverage product with BOARD_SEPOLICY_M4DEFS unset; dexpreopt on
//     (Soong's default); debugfs restrictions off (PRODUCT_SET_DEBUGFS_RESTRICTIONS unset); target_board_api_level = RELEASE_BOARD_API_LEVEL
//     (VendorApiLevel); target_flag_<F> for each flag the tree's se_flags modules export to all_selinux_flags that the release declares.
//   - Contexts: plat_service_contexts (service_contexts_files .plat_private), system_ext_service_contexts (the ATL dir's), plat_seapp_contexts
//     (seapp_contexts_files .plat_private): m4 --fatal-warnings -s over flagging_macros + the files, each followed by a newline file. Not reproduced: checkfc /
//     checkseapp validation (host tools ATL does not build); the table tool refuses a context whose type the policy lacks, and libselinux sorts
//     seapp rows itself at load.

const (
	sepolicyGoal = "atl-sepolicy"
	sepolicyGen  = "gen/sepolicy"
	// policy.go PolicyVers, MlsSens, MlsCats
	sepolicyVers = "30"
	sepolicySens = "1"
	sepolicyCats = "1024"
)

// sepolicyOuts: the goal's files below <out>/gen/sepolicy, by role.
var sepolicyOuts = struct{ Conf, Cil, Policy, PlatService, SysExtService, Seapp, Table string }{
	"policy.conf", "policy.cil", "sepolicy", "plat_service_contexts", "system_ext_service_contexts", "plat_seapp_contexts", "service_access",
}

// sepolicyTools: the host programs of the goal's edges (deps.toml rows checkpolicy, secilc; m4 is -m4) and the table tool.
type sepolicyTools struct{ M4, Checkpolicy, Secilc, Av, SysExtDir string }

// policyConfOrder: policy.go's order of policy.conf parts; "attributes|*.te" is one rank.
var policyConfOrder = []string{"flagging_macros", "security_classes", "initial_sids", "access_vectors", "global_macros", "neverallow_macros", "mls_macros",
	"mls_decl", "mls", "policy_capabilities", "te_macros", "ioctl_defines", "ioctl_macros", "attributes|*.te", "roles_decl", "roles", "users",
	"initial_sid_contexts", "fs_use", "genfs_contexts", "port_contexts"}

// policyConfRank: policy.go findPolicyConfOrder.
func policyConfRank(base string) int {
	for i, p := range policyConfOrder {
		if p == "attributes|*.te" && (base == "attributes" || strings.HasSuffix(base, ".te")) || p == base {
			return i
		}
	}
	return len(policyConfOrder)
}

// sortPolicyConf: policy.go transformPolicyToConf's stable sort of srcs.
func sortPolicyConf(srcs []string) {
	sort.SliceStable(srcs, func(i, j int) bool {
		return policyConfRank(filepath.Base(srcs[i])) < policyConfRank(filepath.Base(srcs[j]))
	})
}

// seBuildDirs: build_files.go GenerateAndroidBuildActions' tag -> directory, for the tags this goal reads; .system_ext_private is the ATL dir.
func (g *Graph) seBuildDirs(t sepolicyTools) map[string]string {
	d := map[string]string{
		".reqd_mask":    "system/sepolicy/reqd_mask",
		".plat_public":  "system/sepolicy/public",
		".plat_private": "system/sepolicy/private",
		".plat_vendor":  "system/sepolicy/vendor",
	}
	if t.SysExtDir != "" {
		d[".system_ext_private"] = t.SysExtDir
	}
	return d
}

// seBuildFiles: `:<module>{<tag>}` of a se_build_files module, as build_files.go findSrcsInDirs globs it: each src pattern in order, its matches sorted.
// A tag without a directory (no ATL dir given) is empty, as Soong's for an unset *_SEPOLICY_DIRS.
func (g *Graph) seBuildFiles(module, tag string, t sepolicyTools) ([]string, error) {
	m := g.rawModule(module, Device, "se_build_files")
	if m == nil {
		return nil, fmt.Errorf("se_build_files %s not in the tree", module)
	}
	dir, ok := g.seBuildDirs(t)[tag]
	if !ok {
		return nil, nil
	}
	var out []string
	for _, p := range m.Props.strs("srcs") {
		matches := globTree(g.abs(dir), p, dir)
		sort.Strings(matches)
		out = append(out, matches...)
	}
	return out, nil
}

// seFlagDefines: flagsToM4Macros over the build flags of all_selinux_flags (flags.go flagsCollectorModule): every flag a se_flags module exports
// there, sorted, that the release declares, as `-D target_flag_<F>=<value>`.
func (g *Graph) seFlagDefines() []string {
	var flags []string
	for _, list := range g.mods[Device] {
		for _, m := range list {
			if m.Type == "se_flags" && contains(m.Props.strs("export_to"), "all_selinux_flags") {
				flags = append(flags, m.Props.strs("flags")...)
			}
		}
	}
	sort.Strings(flags)
	var out []string
	for i, f := range flags {
		if i > 0 && flags[i-1] == f {
			continue
		}
		if v, ok := g.release.flags[f]; ok {
			out = append(out, "-D target_flag_"+f+"="+v)
		}
	}
	return out
}

func contains(list []string, s string) bool {
	for _, x := range list {
		if x == s {
			return true
		}
	}
	return false
}

// sepolicyNinja: the goal's rule and edges, default targets. Problems (no release, a missing module or tool) are recorded and leave the goal out.
func (g *Graph) sepolicyNinja(t sepolicyTools) string {
	fail := func(f string, a ...any) string {
		g.problem("goal %s: "+f, append([]any{sepolicyGoal}, a...)...)
		return ""
	}
	if g.release == nil {
		return fail("needs the release config (-release)")
	}
	if t.Av == "" || t.Checkpolicy == "" || t.Secilc == "" {
		return fail("needs -checkpolicy, -secilc and -sepolicy-av")
	}
	level := strings.TrimSpace(g.release.Flag("RELEASE_BOARD_API_LEVEL"))
	if level == "" {
		return fail("release %s: RELEASE_BOARD_API_LEVEL is empty", g.release.Name)
	}
	flagDefs := g.seFlagDefines()
	macros, ok := g.resolveSrc("system/sepolicy/flagging", Device, ":sepolicy_flagging_macros", sepolicyGoal)
	if !ok || len(macros) != 1 {
		return fail("filegroup sepolicy_flagging_macros unresolved")
	}
	srcs := []string{macros[0].path}
	for _, tag := range []string{".plat_public", ".plat_private", ".plat_vendor", ".system_ext_private"} {
		files, err := g.seBuildFiles("se_build_files", tag, t)
		if err != nil {
			return fail("%v", err)
		}
		srcs = append(srcs, files...)
	}
	sortPolicyConf(srcs)
	debt, err := g.seBuildFiles("sepolicy_technical_debt", ".plat_private", t)
	if err != nil || len(debt) != 1 {
		return fail("sepolicy_technical_debt{.plat_private} unresolved (%v)", err)
	}
	out := filepath.Join(g.out, sepolicyGen)
	at := func(f string) string { return filepath.Join(out, f) }
	abs := func(l []string) []string { return mapStrings(l, g.abs) }
	q := func(l []string) string { return strings.Join(mapStrings(l, shellEscape), " ") }

	var b strings.Builder
	// run from the tree root with tree-relative inputs, as Soong runs from the build top: m4 -s writes each input's path into its #line markers, so absolute paths would make the
	// output depend on where the tree is
	fmt.Fprintf(&b, "\nrule sepolicy\n  command = cd %s && $cmd\n  description = sepolicy $desc\n", esc(shellEscape(g.tree)))
	edge := func(outs, ins []string, desc, cmd string) {
		fmt.Fprintf(&b, "build %s: sepolicy %s\n  desc = %s\n  cmd = %s\n", join(outs), join(ins), esc(desc), esc(cmd))
	}
	// policy.go transformPolicyToConf
	defs := []string{"-D mls_num_sens=" + sepolicySens, "-D mls_num_cats=" + sepolicyCats, "-D target_arch=" + g.cfg.Arch, "-D target_with_asan=false",
		"-D target_with_dexpreopt=true", "-D target_with_native_coverage=false", "-D target_build_variant=user", "-D target_full_treble=true",
		"-D target_compatible_property=true", "-D target_treble_sysprop_neverallow=true", "-D target_enforce_sysprop_owner=true",
		"-D target_exclude_build_test=false", "-D target_requires_insecure_execmem_for_swiftshader=false", "-D target_enforce_debugfs_restriction=false",
		"-D target_recovery=false", "-D target_board_api_level=" + level}
	defs = append(defs, flagDefs...)
	conf := at(sepolicyOuts.Conf)
	edge([]string{conf}, abs(srcs), sepolicyOuts.Conf, fmt.Sprintf("%s --fatal-warnings %s -s %s > %s", shellEscape(t.M4), strings.Join(defs, " "), q(srcs), shellEscape(conf)))
	// policy.go compileConfToCil (+ additional_cil_files), then policyBinary's secilc
	cil := at(sepolicyOuts.Cil)
	edge([]string{cil}, append([]string{conf}, abs(debt)...), sepolicyOuts.Cil, fmt.Sprintf("%s -C -M -c %s -o %s %s && cat %s >> %s", shellEscape(t.Checkpolicy),
		sepolicyVers, shellEscape(cil), shellEscape(conf), q(debt), shellEscape(cil)))
	policy := at(sepolicyOuts.Policy)
	edge([]string{policy}, []string{cil}, sepolicyOuts.Policy, fmt.Sprintf("%s -m -M true -G -c %s %s -o %s -f /dev/null", shellEscape(t.Secilc), sepolicyVers,
		shellEscape(cil), shellEscape(policy)))
	// selinux_contexts.go buildGeneralContexts
	newline := at("newline")
	g.genFile(newline, "\n")
	ctxDefs := append([]string{"-D target_board_api_level=" + level}, flagDefs...)
	contexts := func(name, module, tag string) (string, bool) {
		files, err := g.seBuildFiles(module, tag, t)
		if err != nil {
			fail("%v", err)
			return "", false
		}
		o := at(name)
		if len(files) == 0 {
			// no input files (no ATL dir) -> an empty file; m4 without inputs would read stdin
			g.genFile(o, "")
			return o, true
		}
		// contexts_flags_defaults' srcs (flagging_macros) come first, as a default's list is prepended
		files = append([]string{macros[0].path}, files...)
		var ins []string
		for _, f := range files {
			ins = append(ins, f, newline)
		}
		edge([]string{o}, abs(files), name, fmt.Sprintf("%s --fatal-warnings -s %s %s > %s", shellEscape(t.M4), strings.Join(ctxDefs, " "), q(ins), shellEscape(o)))
		return o, true
	}
	plat, ok1 := contexts(sepolicyOuts.PlatService, "service_contexts_files", ".plat_private")
	sysext, ok2 := contexts(sepolicyOuts.SysExtService, "service_contexts_files", ".system_ext_private")
	seapp, ok3 := contexts(sepolicyOuts.Seapp, "seapp_contexts_files", ".plat_private")
	if !ok1 || !ok2 || !ok3 {
		return ""
	}
	table := at(sepolicyOuts.Table)
	edge([]string{table}, []string{policy, plat, sysext, seapp, t.Av}, sepolicyOuts.Table, fmt.Sprintf("%s --policy %s --service-contexts %s --service-contexts %s --seapp-contexts %s --out %s",
		shellEscape(t.Av), shellEscape(policy), shellEscape(plat), shellEscape(sysext), shellEscape(seapp), shellEscape(table)))
	fmt.Fprintf(&b, "default %s\n", join([]string{policy, plat, sysext, seapp, table}))
	return b.String()
}
