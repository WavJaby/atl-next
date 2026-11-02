// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
)

// prebuilt_apis: build/soong/java/prebuilt_apis.go (the spec) creates, in a load hook, a java_import per <api_dir>/<scope>/<module>.jar below
// the module's directory, named <prebuilt_apis name>_<scope>_<api_dir>_<module> (prebuiltApiModuleName); lookup falls back to synth, which
// calls synthPrebuiltApi.
//   - The import: jars [that file], sdk_version imports_sdk_version (default current), installable false, compile_dex imports_compile_dex.
//   - Not made: the system modules of core-for-system-modules, the API text genrules / filegroups (<module>.api.<scope>.latest, …: only
//     API checks and --migrate-nullness read them, J4), extension SDK modules.

const prebuiltApisType = "prebuilt_apis"

// prebuiltApiScopes: globScopeDir's scopes.
var prebuiltApiScopes = []string{"public", "system", "test", "core", "module-lib", "system-server"}

// prebuiltSystemModuleKind is sdk.go systemModuleKind for a numbered SDK, as prebuilt_apis names scopes: levels up to 31 (no module-lib
// core-for-system-modules then), core, system and test use public's; system_server uses module-lib's.
func prebuiltSystemModuleKind(s sdkSpec) string {
	if lvl, err := strconv.Atoi(strings.SplitN(s.level, ".", 2)[0]); err == nil && lvl <= 31 {
		return "public"
	}
	switch s.kind {
	case "core", "system", "test", "test_frameworks_core":
		return "public"
	case "system_server", "module":
		return "module-lib"
	}
	return s.kind
}

// synthPrebuiltApi: the java_import a prebuilt_apis module makes under the name ref (synth.go), or the java_system_modules of a
// core-for-system-modules jar (createSystemModules: <name>_<scope>_<api_dir>_system_modules); nil for other names.
func (g *Graph) synthPrebuiltApi(ref string, side Side) *Module {
	if base, ok := strings.CutSuffix(trimRef(ref), "_system_modules"); ok && side == Device {
		lib := base + "_core-for-system-modules"
		if m, _ := g.prebuiltApiJar(lib); m != nil {
			props := newObj()
			props.set("libs", strVals([]string{lib}))
			return &Module{Type: "java_system_modules", Name: trimRef(ref), Dir: m.Dir, Props: props}
		}
		return nil
	}
	m, path := g.prebuiltApiJar(trimRef(ref))
	if m == nil || side != Device {
		return nil
	}
	p := m.Props
	props := newObj()
	props.set("jars", strVals([]string{strings.TrimPrefix(path, m.Dir+"/")}))
	sdk := p.str("imports_sdk_version")
	if sdk == "" {
		sdk = "current"
	}
	props.set("sdk_version", sdk)
	props.set("installable", false)
	props.set("compile_dex", p.boolean("imports_compile_dex", false))
	return &Module{Type: "java_import", Name: trimRef(ref), Dir: m.Dir, Props: props}
}

// prebuiltApiJar: the prebuilt_apis module and tree-relative jar a name stands for, nil when it names none.
func (g *Graph) prebuiltApiJar(name string) (*Module, string) {
	for _, ms := range g.mods[Device] {
		for _, m := range ms {
			if m.Type != prebuiltApisType {
				continue
			}
			rest, ok := strings.CutPrefix(name, m.Name+"_")
			if !ok {
				continue
			}
			// <scope>_<api_dir>_<module>: scopes and api dirs hold no `_`
			parts := strings.SplitN(rest, "_", 3)
			if len(parts) != 3 || !slices.Contains(prebuiltApiScopes, parts[0]) || !slices.Contains(m.Props.strs("api_dirs"), parts[1]) {
				continue
			}
			path := filepath.Join(m.Dir, parts[1], parts[0], parts[2]+".jar")
			if _, err := os.Stat(g.abs(path)); err == nil {
				return m, path
			}
		}
	}
	return nil, ""
}

func (g *Graph) prebuiltApiOwned(ref string) bool {
	name := trimRef(ref)
	if base, ok := strings.CutSuffix(name, "_system_modules"); ok {
		name = base + "_core-for-system-modules"
	}
	m, _ := g.prebuiltApiJar(name)
	return m != nil
}
