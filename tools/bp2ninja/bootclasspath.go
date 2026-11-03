// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
)

// The boot classpath as a device of this release sees it at runtime: goal `platform-bootclasspath` builds every jar on it and writes
// <out>/bootclasspath.txt, their dex jars one per line in BOOTCLASSPATH order (what the app process puts on -Xbootclasspath).
//   - Which jars: Soong takes them from product variables (platform_bootclasspath.go configuredJars <- dexpreopt BootJars / ApexBootJars <-
//     soong_config.mk <- PRODUCT_BOOT_JARS / PRODUCT_APEX_BOOT_JARS), not from Blueprint. Those are set by default_art_config.mk (every
//     product with ART inherits it: runtime_libart.mk, module_common.mk, sdk.mk) over envsetup.mk's ART_APEX_JARS, under the release
//     config's flags; product_config.mk then qualifies platform jars, moves com.android.i18n:core-icu4j to the end of PRODUCT_BOOT_JARS
//     and sorts PRODUCT_APEX_BOOT_JARS (bootJarsFinal). No PRODUCT_BOOT_JARS_EXTRA: this product sets none.
//   - APEX_BOOT_JARS_EXCLUDED (art_config.mk, RELEASE_APEX_BOOT_JARS_PREBUILT_EXCLUDED_LIST) is not applied: it removes jars replaced by
//     Google prebuilt APEXes, and every jar here is built from source (as for products with PRODUCT_BUILD_IGNORE_APEX_CONTRIBUTION_CONTENTS).
//   - Order: derive_classpath (packages/modules/SdkExtensions/derive_classpath.cpp, which exports BOOTCLASSPATH at boot) reads the ART
//     APEX's fragment first, then /system's (platform_bootclasspath: PRODUCT_BOOT_JARS minus the ART jars), then /apex/*'s sorted by path,
//     each fragment's jars in ApexBootJars order. The sorted ApexBootJars list is already in that order.
//   - Blueprint's part: each APEX jar must be in the `contents` of a bootclasspath_fragment that platform-bootclasspath
//     (frameworks/base/boot/Android.bp) lists for its APEX (Soong checkApexModules). Fragments behind soong config variables count
//     whatever the variable's value: membership is checked, not selected, here.
//   - Hidden-API flags are not encoded into the dex (Soong's hiddenapi step): without them ART treats every member as SDK API.

const (
	bootclasspathGoal  = "platform-bootclasspath"
	bootclasspathFile  = "bootclasspath.txt"
	artBootJarsMk      = "build/make/core/envsetup.mk"
	productBootJarsMk  = "build/make/target/product/default_art_config.mk"
	productConfigMk    = "build/make/core/product_config.mk"
	artApex            = "com.android.art"
	icu4jBootJar       = "com.android.i18n:core-icu4j"
	platformApex       = "platform"
	bootclasspathBpDir = "frameworks/base/boot"
)

// makeEnv: variables make reads from the environment that the boot jar lists depend on, as this build has them. EMMA_INSTRUMENT_FRAMEWORK
// (envsetup.mk: adds jacocoagent to ART_APEX_JARS) is set only by coverage builds.
var makeEnv = map[string]string{"EMMA_INSTRUMENT_FRAMEWORK": ""}

// bootJar is one `<apex>:<jar>` entry; apex "platform" for /system jars.
type bootJar struct{ apex, jar string }

func (b bootJar) String() string { return b.apex + ":" + b.jar }

func parseBootJar(s string) bootJar {
	if a, j, ok := strings.Cut(s, ":"); ok {
		return bootJar{a, j}
	}
	return bootJar{platformApex, s}
}

// bootJars returns the boot classpath jars in runtime order (see the file comment).
func (g *Graph) bootJars() ([]bootJar, error) {
	if g.release == nil {
		return nil, fmt.Errorf("%s needs -release", bootclasspathGoal)
	}
	vars := map[string][]string{"ART_APEX_JARS": nil, "PRODUCT_BOOT_JARS": nil, "PRODUCT_APEX_BOOT_JARS": nil}
	for _, f := range []string{artBootJarsMk, productBootJarsMk} {
		if err := readMakeLists(filepath.Join(g.tree, f), vars, g.release); err != nil {
			return nil, err
		}
	}
	boot, apex, err := bootJarsFinal(g.tree, vars)
	if err != nil {
		return nil, err
	}
	if err := g.checkFragments(apex); err != nil {
		return nil, err
	}
	var art, platform []bootJar
	for _, b := range boot {
		if b.apex == artApex {
			art = append(art, b)
		} else {
			platform = append(platform, b)
		}
	}
	return slices.Concat(art, platform, apex), nil
}

// bootclasspath adds every boot jar's device dex jar to the Java outputs and writes their list (bootclasspathFile).
func (g *Graph) bootclasspath() error {
	jars, err := g.bootJars()
	if err != nil {
		return err
	}
	var dex []string
	for _, b := range jars {
		// a java_sdk_library's runtime jar is its .impl library (platform_bootclasspath.go DepsMutator: implLibraryModuleName)
		name := b.jar
		if m := g.lookupJava(name, Device); m != nil && g.baseType(m.Type) == "java_sdk_library" {
			name += ".impl"
		}
		n := g.jnode(name, Device, bootclasspathGoal)
		if n == nil || n.dex == "" {
			// the list is written whole or not at all: a partial boot classpath must not be read as the real one
			g.problem("%s: boot jar %s has no device dex jar", bootclasspathGoal, b)
			continue
		}
		g.java.outs = append(g.java.outs, n.dex)
		dex = append(dex, n.dex)
	}
	file := filepath.Join(g.out, bootclasspathFile)
	if len(dex) < len(jars) {
		if err := os.Remove(file); err != nil && !os.IsNotExist(err) {
			return err
		}
		return nil
	}
	g.genFile(file, strings.Join(dex, "\n")+"\n")
	return nil
}

// bootJarsFinal applies product_config.mk's boot jar lines (qualify-platform-jars, the core-icu4j move, the APEX sort); its file must
// still carry the core-icu4j move, else this mirror is stale.
func bootJarsFinal(tree string, vars map[string][]string) (boot, apex []bootJar, err error) {
	b, err := os.ReadFile(filepath.Join(tree, productConfigMk))
	if err != nil {
		return nil, nil, err
	}
	if !strings.Contains(string(b), "$(filter-out "+icu4jBootJar+",$(PRODUCT_APEX_BOOT_JARS))") {
		return nil, nil, fmt.Errorf("%s no longer moves %s out of PRODUCT_APEX_BOOT_JARS: update bootJarsFinal", productConfigMk, icu4jBootJar)
	}
	if len(vars["ART_APEX_JARS"]) == 0 || len(vars["PRODUCT_BOOT_JARS"]) == 0 {
		return nil, nil, fmt.Errorf("%s / %s: empty ART_APEX_JARS or PRODUCT_BOOT_JARS", artBootJarsMk, productBootJarsMk)
	}
	for _, s := range vars["PRODUCT_BOOT_JARS"] {
		boot = append(boot, parseBootJar(s))
	}
	var apexStr []string
	for _, s := range vars["PRODUCT_APEX_BOOT_JARS"] {
		if s != icu4jBootJar {
			apexStr = append(apexStr, s)
		}
	}
	boot = append(boot, parseBootJar(icu4jBootJar))
	slices.Sort(apexStr)
	apexStr = slices.Compact(apexStr)
	for _, s := range apexStr {
		apex = append(apex, parseBootJar(s))
	}
	return boot, apex, nil
}

// checkFragments: every APEX jar is in the contents of a fragment platform-bootclasspath lists for that APEX.
func (g *Graph) checkFragments(apex []bootJar) error {
	pbcp := g.lookup(bootclasspathGoal, Device)
	if pbcp == nil || pbcp.Dir != bootclasspathBpDir {
		return fmt.Errorf("%s: no module %s", bootclasspathBpDir, bootclasspathGoal)
	}
	frags := map[string][]string{} // apex -> fragment modules
	for _, it := range anyConfig(pbcp.Props, "fragments") {
		if o, ok := it.(*Obj); ok {
			frags[o.str("apex")] = append(frags[o.str("apex")], o.str("module"))
		}
	}
	for _, b := range apex {
		found := false
		for _, f := range frags[b.apex] {
			if m := g.lookup(f, Device); m != nil && slices.Contains(anyConfig(m.Props, "contents"), Val(b.jar)) {
				found = true
			}
		}
		if !found {
			return fmt.Errorf("boot jar %s: in no bootclasspath_fragment %s lists for %s (fragments %v)", b, bootclasspathGoal, b.apex, frags[b.apex])
		}
	}
	return nil
}

// anyConfig: list property key of p plus its value in every soong_config_variables branch (conditions_default included).
func anyConfig(p *Obj, key string) []Val {
	out, _ := p.get(key).([]Val)
	out = slices.Clone(out)
	vs := p.obj("soong_config_variables")
	if vs == nil {
		return out
	}
	for _, k := range vs.keys {
		v := vs.obj(k)
		l, _ := v.get(key).([]Val)
		out = append(out, l...)
		if d := v.obj("conditions_default"); d != nil {
			l, _ := d.get(key).([]Val)
			out = append(out, l...)
		}
	}
	return out
}

var (
	reMkAssign    = regexp.MustCompile(`^([A-Z_][A-Z0-9_]*)\s*(:=|\+=|=)\s*(.*)$`)
	reMkCond      = regexp.MustCompile(`^(ifeq|ifneq)\s*\((.*)\)$`)
	reMkRef       = regexp.MustCompile(`^\$\(([A-Za-z_][A-Za-z0-9_]*)\)$`)
	reMkConfigSet = regexp.MustCompile(`\$\(call soong_config_set,[^)]*\)`)
)

// readMakeLists reads the list variables already in vars (keys present, empty or not) from a product makefile: `:=` / `+=` / `=`
// assignments with backslash continuations, `$(VAR)` items, `ifeq|ifneq (a,b)` / `else` / `endif` whose operands are literals, list
// variables, release flags or makeEnv. A condition it cannot evaluate is fine unless its body assigns a list variable (error); `define`
// blocks are skipped. Returns an error for `$(error …)` reached on a taken branch.
func readMakeLists(path string, vars map[string][]string, rel *Release) error {
	b, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	// join continuations, drop comments
	var lines []string
	cur := ""
	for _, l := range strings.Split(string(b), "\n") {
		if i := strings.Index(l, "#"); i >= 0 {
			l = l[:i]
		}
		if strings.HasSuffix(strings.TrimRight(l, " \t"), "\\") {
			cur += strings.TrimSuffix(strings.TrimRight(l, " \t"), "\\") + " "
			continue
		}
		lines = append(lines, strings.TrimSpace(cur+l))
		cur = ""
	}
	value := func(s string) (string, bool) {
		s = strings.TrimSpace(s)
		m := reMkRef.FindStringSubmatch(s)
		if m == nil {
			if strings.Contains(s, "$") {
				return "", false
			}
			return s, true
		}
		if l, ok := vars[m[1]]; ok {
			return strings.Join(l, " "), true
		}
		if v, ok := makeEnv[m[1]]; ok {
			return v, true
		}
		if strings.HasPrefix(m[1], "RELEASE_") {
			v, ok := rel.flags[m[1]]
			return v, ok
		}
		return "", false
	}
	// stack of branch states: 1 taken, 0 not taken, -1 unknown; done = an earlier branch of this if was taken
	type branch struct{ state, done int }
	var stack []branch
	active := func() int {
		s := 1
		for _, b := range stack {
			switch {
			case b.state == 0:
				return 0
			case b.state < 0:
				s = -1
			}
		}
		return s
	}
	cond := func(t string) int {
		m := reMkCond.FindStringSubmatch(t)
		if m == nil {
			return -1
		}
		// the comma outside any $(…)
		depth, comma := 0, -1
		for i, r := range m[2] {
			switch {
			case r == '(':
				depth++
			case r == ')':
				depth--
			case r == ',' && depth == 0 && comma < 0:
				comma = i
			}
		}
		if comma < 0 {
			return -1
		}
		a, okA := value(m[2][:comma])
		c, okC := value(m[2][comma+1:])
		if !okA || !okC {
			return -1
		}
		if (a == c) == (m[1] == "ifeq") {
			return 1
		}
		return 0
	}
	inDefine := false
	for _, t := range lines {
		word, rest, _ := strings.Cut(t, " ")
		switch {
		case inDefine:
			inDefine = word != "endef"
			continue
		case word == "define":
			inDefine = true
			continue
		case word == "ifeq" || word == "ifneq" || word == "ifdef" || word == "ifndef":
			s := cond(t)
			stack = append(stack, branch{s, max(s, 0)})
			continue
		case word == "else":
			if len(stack) == 0 {
				return fmt.Errorf("%s: else without if", path)
			}
			top := &stack[len(stack)-1]
			switch {
			case top.done == 1:
				top.state = 0
			case top.state < 0:
				top.state = -1
			case rest != "":
				top.state = cond(strings.TrimSpace(rest))
				top.done = max(top.state, 0)
			default:
				top.state, top.done = 1, 1
			}
			continue
		case word == "endif":
			if len(stack) == 0 {
				return fmt.Errorf("%s: endif without if", path)
			}
			stack = stack[:len(stack)-1]
			continue
		}
		if strings.HasPrefix(t, "$(error ") && active() == 1 {
			return fmt.Errorf("%s: %s", path, t)
		}
		m := reMkAssign.FindStringSubmatch(t)
		if m == nil {
			continue
		}
		if _, tracked := vars[m[1]]; !tracked {
			continue
		}
		switch active() {
		case 0:
			continue
		case -1:
			return fmt.Errorf("%s: %s is assigned under a condition this reader cannot evaluate", path, m[1])
		}
		var items []string
		// a soong_config_set call continued into a list (default_art_config.mk ranging) expands to nothing
		for _, it := range strings.Fields(reMkConfigSet.ReplaceAllString(m[3], "")) {
			v, ok := value(it)
			if !ok {
				return fmt.Errorf("%s: %s: cannot expand %s", path, m[1], it)
			}
			items = append(items, strings.Fields(v)...)
		}
		if m[2] == "+=" {
			vars[m[1]] = append(vars[m[1]], items...)
		} else {
			vars[m[1]] = items
		}
	}
	return nil
}
