// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"slices"
)

// combined_apis: frameworks/base/api's Soong plugin (api.go createInternalModules, the spec) creates merged API libraries from its
// bootclasspath list in a load hook; no Android.bp defines them. lookup falls back to synth, which calls synthCombinedApis. The module lists
// core_libraries_modules and non_updatable_modules are read from the plugin's api.go (goConstants), next to the Android.bp of the
// combined_apis module.
//   - all-modules-public-stubs: <bootclasspath>.stubs.
//   - all-updatable-modules-system-stubs: <bootclasspath minus non-updatable>.stubs.system; all-modules-system-stubs: <non-updatable>.stubs.system,
//     then all-updatable-modules-system-stubs.
//   - framework-updatable-stubs-module_libs_api: <bootclasspath minus core libraries minus non-updatable>.stubs.module_lib.
//   - Each a java_library of those static_libs, sdk_version module_current, is_stubs_module.
//   - all-non-updatable-modules-test-stubs: <non-updatable>.stubs.test (createMergedTestStubsForNonUpdatableModules; android_test_stubs_current
//     is built on it).
//   - Not made: the merged API text files, the exportable / system-server variants, annotation and source filegroups.

const combinedApisType = "combined_apis"

// combinedApis: the bootclasspath of the tree's combined_apis module and the plugin's module lists, nil when the tree has none.
type combinedApis struct {
	dir                               string
	bootclasspath, core, nonUpdatable []string
}

func (g *Graph) combinedApisOf() *combinedApis {
	if g.combined != nil || g.combinedRead {
		return g.combined
	}
	g.combinedRead = true
	var m *Module
	for _, ms := range g.mods[Device] {
		for _, c := range ms {
			if c.Type == combinedApisType {
				m = c
			}
		}
	}
	if m == nil {
		return nil
	}
	lists, strs := map[string][]string{}, map[string]string{}
	if err := goConstants(filepath.Join(g.tree, m.Dir, "api.go"), lists, strs); err != nil {
		g.problem("%s: reading the combined_apis plugin: %v", m.Name, err)
		return nil
	}
	c := &combinedApis{dir: m.Dir, bootclasspath: m.Props.strs("bootclasspath"), core: lists["core_libraries_modules"], nonUpdatable: lists["non_updatable_modules"]}
	if len(c.core) == 0 || len(c.nonUpdatable) == 0 {
		g.problem("%s: api.go: core_libraries_modules / non_updatable_modules not found", m.Name)
		return nil
	}
	g.combined = c
	return c
}

// combinedApisNames: the modules synthCombinedApis makes.
var combinedApisNames = []string{"all-modules-public-stubs", "all-updatable-modules-system-stubs", "all-modules-system-stubs",
	"framework-updatable-stubs-module_libs_api", "all-non-updatable-modules-test-stubs"}

func (g *Graph) combinedApisOwned(ref string) bool {
	return slices.Contains(combinedApisNames, trimRef(ref)) && g.combinedApisOf() != nil
}

// synthCombinedApis: the java_library the combined_apis hook makes under the name ref (synth.go), nil for other names.
func (g *Graph) synthCombinedApis(ref string, side Side) *Module {
	name := trimRef(ref)
	if side != Device || !slices.Contains(combinedApisNames, name) {
		return nil
	}
	c := g.combinedApisOf()
	if c == nil {
		return nil
	}
	without := func(list, drop []string) []string {
		var out []string
		for _, m := range list {
			if !slices.Contains(drop, m) {
				out = append(out, m)
			}
		}
		return out
	}
	suffix := func(list []string, s string) []string {
		out := make([]string, len(list))
		for i, m := range list {
			out[i] = m + s
		}
		return out
	}
	var libs []string
	switch name {
	case "all-modules-public-stubs":
		libs = suffix(c.bootclasspath, ".stubs")
	case "all-updatable-modules-system-stubs":
		libs = suffix(without(c.bootclasspath, c.nonUpdatable), ".stubs.system")
	case "all-modules-system-stubs":
		libs = append(suffix(c.nonUpdatable, ".stubs.system"), "all-updatable-modules-system-stubs")
	case "framework-updatable-stubs-module_libs_api":
		libs = suffix(without(without(c.bootclasspath, c.core), c.nonUpdatable), ".stubs.module_lib")
	case "all-non-updatable-modules-test-stubs":
		libs = suffix(c.nonUpdatable, ".stubs.test")
	}
	props := newObj()
	props.set("static_libs", strVals(libs))
	props.set("sdk_version", "module_current")
	props.set("is_stubs_module", true)
	return &Module{Type: "java_library", Name: name, Dir: c.dir, Props: props}
}

func trimRef(ref string) string {
	if len(ref) > 0 && ref[0] == ':' {
		return ref[1:]
	}
	return ref
}
