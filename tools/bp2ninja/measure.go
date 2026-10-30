// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"sort"
	"strings"
)

// measure walks the reference closure of goals without building anything, to size what a goal needs before the converter
// supports it: module types reached, and the kinds of sources they list. Every property known to name modules is followed,
// whatever the module type, plus the SDK a Java module's sdk_version / system_modules imply (decodeSdk) and the components a
// java_sdk_library creates. Not followed: install-only references (required, data), hidden-API annotation libraries, and a
// java_api_library's API contributions (it reads their API text files; Soong builds the droidstubs behind them only to validate).
// Output: one `type <type> <count>` line per module type, `src <ext> <count>` per source extension, `missing <ref>` per reference
// no module answers to.

// refProps: properties holding module references, top level or in a nested block (`openjdk9.srcs`, `aidl.libs`, …).
var refProps = []string{"srcs", "api_srcs", "java_resources", "libs", "static_libs", "stub_only_libs", "stub_only_static_libs",
	"plugins", "exported_plugins", "kotlin_plugins", "aconfig_declarations", "merge_annotations_dirs",
	"merge_inclusion_annotations_dirs", "tools", "tool_files", "header_libs", "shared_libs", "whole_static_libs",
	"generated_headers", "generated_sources", "srcs_lib", "impl_only_libs", "impl_only_static_libs"}

// refBlocks: nested blocks whose refProps are followed too.
var refBlocks = []string{"openjdk9", "aidl", "proto", "public", "system", "module_lib", "test", "system_server"}

type measurement struct {
	types   map[string]int
	exts    map[string]int
	missing map[string]bool
	seen    map[string]bool
}

func (g *Graph) measure(goals []string) string {
	ms := &measurement{types: map[string]int{}, exts: map[string]int{}, missing: map[string]bool{}, seen: map[string]bool{}}
	for _, goal := range goals {
		g.measureRef(ms, goal, Device)
	}
	var b strings.Builder
	for _, k := range sortedCount(ms.types) {
		fmt.Fprintf(&b, "type %s %d\n", k, ms.types[k])
	}
	for _, k := range sortedCount(ms.exts) {
		fmt.Fprintf(&b, "src %s %d\n", k, ms.exts[k])
	}
	for _, k := range sortedKeys(ms.missing) {
		fmt.Fprintf(&b, "missing %s\n", k)
	}
	return b.String()
}

// generatedJavaLib: "aidl_interface (java)" / "hidl_interface (java)" when name is a Java library such an interface module creates,
// "" otherwise. The interface's own dependencies are not followed (lower bound).
func (g *Graph) generatedJavaLib(name string, side Side) string {
	base, ok := strings.CutSuffix(name, "-java")
	if !ok {
		base, ok = strings.CutSuffix(name, "-java-constants")
	}
	if !ok {
		return ""
	}
	cands := []string{base}
	if i := strings.LastIndex(base, "-V"); i > 0 {
		cands = append(cands, base[:i])
	}
	for _, c := range cands {
		for _, m := range g.mods[side][c] {
			switch t := g.baseType(m.Type); t {
			case "aidl_interface", "hidl_interface":
				return t + " (java)"
			}
		}
		// hidl: <pkg>-V<x.y>-java names the interface <pkg>@<x.y>
		if i := strings.LastIndex(base, "-V"); i > 0 {
			for _, m := range g.mods[side][base[:i]+"@"+base[i+2:]] {
				if g.baseType(m.Type) == "hidl_interface" {
					return "hidl_interface (java)"
				}
			}
		}
	}
	return ""
}

func sortedCount(m map[string]int) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Slice(keys, func(i, j int) bool {
		if m[keys[i]] != m[keys[j]] {
			return m[keys[i]] > m[keys[j]]
		}
		return keys[i] < keys[j]
	})
	return keys
}

func (g *Graph) measureRef(ms *measurement, ref string, side Side) {
	name, tag := splitTag(ref)
	name = strings.TrimPrefix(name, ":")
	if i := strings.IndexByte(name, '#'); i >= 0 {
		name = name[:i]
	}
	if g.omit[name] || name == "none" || name == "" {
		return
	}
	m := g.lookupJava(name, side)
	if m == nil {
		// libraries an aidl_interface / hidl_interface creates per backend and version (<iface>[-V<n>]-java[-constants])
		if t := g.generatedJavaLib(name, side); t != "" {
			ms.types[t]++
			return
		}
		ms.missing[name] = true
		return
	}
	key := m.Name + "\x00" + side.String()
	if ms.seen[key] {
		return
	}
	ms.seen[key] = true
	t := g.baseType(m.Type)
	ms.types[t]++
	p := g.flattenJava(m, side)
	// plugins and tools run on the host
	follow := func(props *Obj) {
		for _, k := range refProps {
			s := side
			if k == "plugins" || k == "exported_plugins" || k == "kotlin_plugins" || k == "tools" {
				s = Host
			}
			pathProp := k == "srcs" || k == "api_srcs" || k == "java_resources" || k == "tool_files"
			for _, v := range props.strs(k) {
				// path properties name modules as `:name` / `//dir:name`, the others hold module names only
				if !pathProp || isRef(v) {
					g.measureRef(ms, v, s)
					continue
				}
				if k == "srcs" || k == "api_srcs" {
					for _, f := range g.globs(m.Dir, []string{v}) {
						ms.exts[filepath.Ext(f)]++
					}
				}
			}
		}
	}
	follow(p)
	for _, blk := range refBlocks {
		follow(p.obj(blk))
	}
	if sm := p.str("system_modules"); sm != "" {
		g.measureRef(ms, sm, side)
	}
	if javaTypes[t] && side == Device && t != "java_system_modules" && t != "aconfig_declarations" && t != "droiddoc_exported_dir" {
		n := &jNode{jKey: jKey{m.Name, side}, mod: m, props: p}
		if sd, err := g.decodeSdk(n, parseSdk(p.str("sdk_version"))); err == nil {
			for _, r := range append(append(append(sd.boot, sd.java9...), sd.classpath...), sd.sysModules) {
				if r != "" {
					g.measureRef(ms, r, side)
				}
			}
		}
	}
	if t == "java_sdk_library" && tag != "" {
		if c := sdkLibTagComponent(m.Name, tag); c != "" {
			g.measureRef(ms, c, side)
		}
	}
	if t == "java_aconfig_library" {
		g.measureRef(ms, p.str("aconfig_declarations"), side)
	}
}
