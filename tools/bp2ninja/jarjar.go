// SPDX-License-Identifier: MIT

package main

import (
	"maps"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
)

// jarjar: build/soong/java base.go jarjarIfNecessary / repackageFlagsIfNecessary / collectJarJarRules, builder.go TransformJarJarWithShards.
//   - jarjar_rules (a path or a module reference) renames the combined implementation jar, in jarjar_shards runs merged by combineJar, and
//     the combined resources in one.
//   - Rename rules travel with dependencies (JarJarProviderData): a java_aconfig_library of an exportable declarations package adds
//     <pkg>.{Flags,FeatureFlags,FeatureFlagsImpl,CustomFeatureFlags,FakeFeatureFlagsImpl} -> "" (rename later); jarjar_rename adds its
//     classes the same way; a module with jarjar_prefix turns every "" into <prefix>.<class>. Every module with resolved rules rewrites its
//     own compiled jars (kotlinc, javac) through them, as repackaged-jarjar/<step>/<jar>.
//   - Inherited from (collectDirectDepsProviders): libs, static_libs, plugins and the SDK's classpath libraries, unless the dependency is a
//     java_sdk_library or the SDK is a numbered prebuilt one; an aconfig_declarations_group or java_aconfig_library named in srcs. Not from
//     the bootclasspath, java9 classpath, filegroups and genrules. java_library's StubsLinkType stays Unknown (java.go: only set when already
//     known), so is_stubs_module does not exclude a library here.
//   - Merge: a rule already set to a name is kept unless that name has the overridable prefix com.android.internal.hidden_from_bootclasspath.

const overridableJarJarPrefix = "com.android.internal.hidden_from_bootclasspath"

// jarjarRulesFile: jarjar_rules as an absolute path ("" = none); a module reference is resolved like a source.
func (g *Graph) jarjarRulesFile(n *jNode) string {
	r := n.props.str("jarjar_rules")
	if r == "" {
		return ""
	}
	files, _ := g.resolveSrc(n.mod.Dir, n.side, r, n.name)
	if len(files) != 1 {
		g.problem("%s: jarjar_rules %s is not one file", n.name, r)
		return ""
	}
	return g.abs(files[0].path)
}

// jarjar is TransformJarJarWithShards: one run (`1 0`), or shards runs (`<total> <index>`) into <out minus .jar>.-<i>.jar merged without
// strip flags.
func (g *Graph) jarjar(n *jNode, out, in, rules string, shards int) {
	j := g.java
	tool := g.jtoolJar("jarjar", n.name)
	run := func(o, total, index string) {
		j.edge("jarjar", []string{o}, []string{in}, []string{rules, tool}, nil, [][2]string{{"rules", esc(rules)}, {"jarjar", esc(tool)},
			{"total", total}, {"index", index}})
	}
	if shards == 1 {
		run(out, "1", "0")
		return
	}
	var parts []string
	for i := 0; i < shards; i++ {
		o := strings.TrimSuffix(out, ".jar") + ".-" + strconv.Itoa(i) + ".jar"
		run(o, strconv.Itoa(shards), strconv.Itoa(i))
		parts = append(parts, o)
	}
	j.edge("combine", []string{out}, parts, nil, nil, [][2]string{{"args", ""}})
}

// repackageRules: collectJarJarRules for n, stored on n for its dependents; returns the rules file of the resolved renames ("" = none).
func (g *Graph) repackageRules(n *jNode, srcs []string, s sdkSpec) string {
	p := n.props
	rules := map[string]string{}
	merge := func(theirs map[string]string) {
		for orig, renamed := range theirs {
			pre, ok := rules[orig]
			switch {
			case !ok || pre == "":
				rules[orig] = renamed
			case renamed != "" && pre != renamed:
				if strings.HasPrefix(pre, overridableJarJarPrefix) {
					rules[orig] = renamed
				} else if !strings.HasPrefix(renamed, overridableJarJarPrefix) {
					g.problem("%s: conflicting jarjar rules inherited for class %s (%s and %s)", n.name, orig, renamed, pre)
				}
			}
		}
	}
	// a numbered prebuilt SDK excludes every dependency
	if n.side != Device || s.level == "" {
		var refs []string
		if n.side == Device && s.kind == "private" {
			refs = append(refs, "ext", "framework")
		}
		refs = append(refs, p.strs("libs")...)
		refs = append(refs, p.strs("static_libs")...)
		for _, r := range refs {
			if m := g.lookupJava(r, n.side); m != nil && g.baseType(m.Type) == "java_sdk_library" {
				continue
			}
			if d := g.java.nodes[jKey{g.javaName(r, n.side), n.side}]; d != nil {
				merge(d.renames)
			}
		}
		for _, r := range p.strs("plugins") {
			if d := g.java.nodes[jKey{g.javaName(r, Host), Host}]; d != nil {
				merge(d.renames)
			}
		}
		for _, r := range srcs {
			if isRef(r) {
				merge(g.codegenRenames(r, n.side, n.name))
			}
		}
	}
	for orig, renamed := range n.renames {
		// the module's own (java_aconfig_library), set before compile
		if pre, ok := rules[orig]; ok && renamed != "" && pre != renamed {
			g.problem("%s: conflicting jarjar rules inherited for class %s (%s and %s)", n.name, orig, renamed, pre)
			continue
		}
		rules[orig] = renamed
	}
	for _, c := range p.strs("jarjar_rename") {
		rules[c] = ""
	}
	if len(rules) == 0 {
		return ""
	}
	if prefix := p.str("jarjar_prefix"); prefix != "" {
		if strings.HasPrefix(prefix, ".") || strings.HasSuffix(prefix, ".") {
			g.problem("%s: jarjar_prefix can not start or end with '.'", n.name)
		} else {
			for orig, renamed := range rules {
				if renamed == "" {
					rules[orig] = prefix + "." + orig
				}
			}
		}
	}
	n.renames = rules
	var text strings.Builder
	for _, orig := range slices.Sorted(maps.Keys(rules)) {
		if rules[orig] != "" {
			text.WriteString("rule " + orig + " " + rules[orig] + "\n")
		}
	}
	if text.Len() == 0 {
		return ""
	}
	file := filepath.Join(n.dir, "repackaged-jarjar", "repackaging.txt")
	g.genFile(file, text.String())
	return file
}

// javaName: the module name a Java reference resolves to (nodes are keyed by it), "" when none.
func (g *Graph) javaName(ref string, side Side) string {
	if m := g.lookupJava(ref, side); m != nil {
		return m.Name
	}
	return ""
}

// codegenRenames: the rename rules a srcs reference carries: an aconfig_declarations_group (its java_aconfig_libraries' and nested
// groups') or a java_aconfig_library; nothing for other modules.
func (g *Graph) codegenRenames(ref string, side Side, from string) map[string]string {
	name, _ := splitTag(ref)
	m := g.lookupJava(name, side)
	if m == nil {
		m = g.lookup(name, side)
	}
	if m == nil {
		return nil
	}
	switch g.baseType(m.Type) {
	case "java_aconfig_library":
		if d := g.jnode(name, side, from); d != nil {
			return d.renames
		}
	case "aconfig_declarations_group":
		out := map[string]string{}
		p := m.Props
		for _, r := range slices.Concat(p.strs("aconfig_declarations_groups"), p.strs("java_aconfig_libraries")) {
			for k, v := range g.codegenRenames(":"+r, side, from) {
				if pre, ok := out[k]; !ok || pre == "" {
					out[k] = v
				}
			}
		}
		return out
	}
	return nil
}

// repackage: jar through the rules file into repackaged-jarjar/<step>/<jar> (repackageFlagsIfNecessary); jar itself without rules.
func (g *Graph) repackage(n *jNode, jar, rules, step, name string) string {
	if rules == "" {
		return jar
	}
	out := filepath.Join(n.dir, "repackaged-jarjar", step, name)
	g.jarjar(n, out, jar, rules, 1)
	return out
}
