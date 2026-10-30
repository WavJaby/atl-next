// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"slices"
	"strconv"
)

// aconfig flags (build/soong/aconfig: aconfig_declarations.go, aconfig_value_set.go, codegen/java_aconfig_library.go, the spec).
//   - Values: the release's RELEASE_ACONFIG_VALUE_SETS (Release.ValueSets) in order; each aconfig_value_set takes the aconfig_values
//     modules of the blueprint files its srcs name (and its `values`); a declarations module gets the files of those for its package.
//   - Only the current release config is built (no RELEASE_ACONFIG_EXTRA_RELEASE_CONFIGS).

// aconfigDecl is what an aconfig_declarations module hands its users (AconfigDeclarationsProviderData).
type aconfigDecl struct {
	pkg, cache string
	exportable bool
}

const aconfigRules = `rule aconfig_cache
  command = $aconfig create-cache --package $package $container $declarations $values $permission $readwrite --cache $out.tmp && (if cmp -s $out.tmp $out; then rm $out.tmp; else mv $out.tmp $out; fi)
  restat = 1
  description = aconfig $out
rule aconfig_java
  command = rm -rf $out.tmp && mkdir -p $out.tmp && $aconfig create-java-lib --mode $mode --cache $in --out $out.tmp --allow-instrumentation $debug --new-exported $newexported --check-api-level $checkapi && $soongzip -write_if_changed -jar -o $out -C $out.tmp -D $out.tmp && rm -rf $out.tmp
  restat = 1
  description = aconfig java $out
`

// aconfigValues: the value files for one package, in value set order (aconfig_value_set.go AvailablePackages).
func (g *Graph) aconfigValues(pkg string) []string {
	var out []string
	for _, set := range g.java.release.ValueSets {
		vs := g.lookup(set, Device)
		if vs == nil {
			g.problem("aconfig value set %s not found", set)
			g.notFound(set)
			continue
		}
		var mods []string
		mods = append(mods, vs.Props.strs("values")...)
		dirs := map[string]bool{}
		for _, bp := range g.globs(vs.Dir, vs.Props.strs("srcs")) {
			dirs[filepath.Dir(bp)] = true
		}
		var found []*Module
		for _, list := range g.mods[Device] {
			for _, m := range list {
				if m.Type == "aconfig_values" && (dirs[m.Dir] || slices.Contains(mods, m.Name)) && m.Props.str("package") == pkg {
					found = append(found, m)
				}
			}
		}
		slices.SortFunc(found, func(a, b *Module) int {
			if a.Dir != b.Dir {
				if a.Dir < b.Dir {
					return -1
				}
				return 1
			}
			if a.Name < b.Name {
				return -1
			}
			return 1
		})
		for _, m := range found {
			files := g.globs(m.Dir, m.Props.strs("srcs"))
			slices.Sort(files)
			out = append(out, files...)
		}
	}
	return out
}

// aconfigDeclarations is aconfig_declarations: create-cache over the declarations with the release's values.
func (g *Graph) aconfigDeclarations(n *jNode) {
	p := n.props
	r := g.java.release
	pkg := p.str("package")
	decls := g.globs(n.mod.Dir, p.strs("srcs"))
	values := g.aconfigValues(pkg)
	var declArgs, valueArgs []string
	for _, d := range decls {
		declArgs = append(declArgs, "--declarations", esc(g.abs(d)))
	}
	for _, v := range values {
		valueArgs = append(valueArgs, "--values", esc(g.abs(v)))
	}
	vars := [][2]string{{"package", pkg}, {"declarations", joinWords(declArgs)}, {"values", joinWords(valueArgs)}}
	if perm := r.Flag("RELEASE_ACONFIG_FLAG_DEFAULT_PERMISSION"); perm != "" {
		vars = append(vars, [2]string{"permission", "--default-permission " + perm})
	}
	// RELEASE_ACONFIG_REQUIRE_ALL_READ_ONLY is the negation of aconfig's allow-read-write
	vars = append(vars, [2]string{"readwrite", "--allow-read-write " + strconv.FormatBool(!r.Bool("RELEASE_ACONFIG_REQUIRE_ALL_READ_ONLY"))})
	if c := p.str("container"); c != "" {
		vars = append(vars, [2]string{"container", "--container " + c})
	}
	cache := filepath.Join(n.dir, "intermediate.pb")
	g.java.edge("aconfig_cache", []string{cache}, absAll(g, append(append([]string{}, decls...), values...)), []string{g.java.tools.Aconfig}, nil, vars)
	n.aconfig = &aconfigDecl{pkg: pkg, cache: cache, exportable: p.boolean("exportable", false)}
}

// aconfigJavaLib is java_aconfig_library: the generated flag classes compiled as a java library (GeneratedJavaLibraryModule).
func (g *Graph) aconfigJavaLib(n *jNode) {
	p := n.props
	r := g.java.release
	decl := g.jnode(p.str("aconfig_declarations"), Device, n.name)
	if decl == nil || decl.aconfig == nil {
		g.problem("%s: aconfig_declarations %q unresolved", n.name, p.str("aconfig_declarations"))
		return
	}
	mode := p.str("mode")
	if mode == "" {
		mode = "production"
	}
	srcjar := filepath.Join(n.dir, "gen", n.name+".srcjar")
	g.java.edge("aconfig_java", []string{srcjar}, []string{decl.aconfig.cache}, []string{g.java.tools.Aconfig}, nil, [][2]string{
		{"mode", mode}, {"debug", strconv.FormatBool(r.Bool("RELEASE_READ_FROM_NEW_STORAGE"))},
		{"newexported", strconv.FormatBool(r.Flag("RELEASE_ACONFIG_NEW_EXPORTED") == "true")},
		{"checkapi", strconv.FormatBool(r.Bool("RELEASE_ACONFIG_CHECK_API_LEVEL"))},
	})
	// DepsMutator: libraries the generated code needs, unless the library is built with sdk_version "none"
	if p.str("sdk_version") != "none" {
		libs := []Val{}
		for _, l := range p.strs("libs") {
			libs = append(libs, l)
		}
		for _, l := range []string{"aconfig-annotations-lib", "unsupportedappusage", "aconfig_storage_stub"} {
			libs = append(libs, l)
		}
		n.props.set("libs", libs)
	}
	g.javaLib(n, srcjar)
}

func joinWords(w []string) string {
	out := ""
	for i, s := range w {
		if i > 0 {
			out += " "
		}
		out += s
	}
	return out
}
