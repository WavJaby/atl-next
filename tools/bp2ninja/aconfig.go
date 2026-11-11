// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
)

// aconfig flags (build/soong/aconfig: aconfig_declarations.go, aconfig_value_set.go, codegen/{java,cc}_aconfig_library.go, the spec).
//   - Values: the release's RELEASE_ACONFIG_VALUE_SETS (Release.ValueSets) in order; each aconfig_value_set takes the aconfig_values
//     modules of the blueprint files its srcs name (and its `values`); a declarations module gets the files of those for its package.
//   - Only the current release config is built (no RELEASE_ACONFIG_EXTRA_RELEASE_CONFIGS).
//   - One declarations cache per aconfig_declarations module, shared by every codegen that names it (Java, cc): Soong's
//     AconfigDeclarationsProviderData.IntermediateCacheOutputPath. aconfig_declarations has no variants, so it is looked up on the device side.
//   - The aconfig tool is build/make/tools/aconfig's rust_binary_host, built in this graph as Soong builds it: its codegen depends on
//     the tree (the finalized flags it embeds turn SDK-finalized flags into `SDK_INT >= <level>` checks), so a binary of another tree
//     generates other code.
//   - Not supported: rust_aconfig_library (rust.go reports it).

// aconfigGraph: the aconfig tool (built once the first declarations need it) and the declarations caches built so far; values come
// from Graph.release.
type aconfigGraph struct {
	tool  string
	decls map[string]*aconfigDecl
	b     strings.Builder
	// valueSets, values: aconfigValues' indexes (valueSet, valueFiles)
	valueSets map[*Module]map[string][]*Module
	values    map[*Module][]string
}

func newAconfigGraph(tool string) *aconfigGraph {
	return &aconfigGraph{tool: tool, decls: map[string]*aconfigDecl{}, valueSets: map[*Module]map[string][]*Module{}, values: map[*Module][]string{}}
}

// aconfigDecl is what an aconfig_declarations module hands its users (AconfigDeclarationsProviderData).
type aconfigDecl struct {
	pkg, cache string
	exportable bool
	// intermediate.txt once some module reads it (aconfigText)
	text string
}

// aconfigModes: the codegen modes Soong accepts (codegen/java_aconfig_library.go aconfigSupportedModes, used by cc and rust too).
var aconfigModes = []string{"production", "test", "exported", "force-read-only"}

const aconfigRules = `rule aconfig_cache
  command = $aconfig create-cache --package $package $container $declarations $values $permission $readwrite --cache $out.tmp && (if cmp -s $out.tmp $out; then rm $out.tmp; else mv $out.tmp $out; fi)
  restat = 1
  description = aconfig $out
rule aconfig_cc
  command = rm -rf $gendir && mkdir -p $gendir && $aconfig create-cpp-lib --mode $mode --cache $in --out $gendir
  description = aconfig cc $out
rule aconfig_text
  command = $aconfig dump-cache --dedup --format='{fully_qualified_name}:{permission}={state:bool}' --cache $in --out $out.tmp && (if cmp -s $out.tmp $out; then rm $out.tmp; else mv $out.tmp $out; fi)
  restat = 1
  description = aconfig text $out
`

// aconfigJavaRule: written with the Java rules, whose $soongzip it runs.
const aconfigJavaRule = `rule aconfig_java
  command = rm -rf $out.tmp && mkdir -p $out.tmp && $aconfig create-java-lib --mode $mode --cache $in --out $out.tmp --allow-instrumentation $debug --new-exported $newexported --check-api-level $checkapi && $soongzip -write_if_changed -jar -o $out -C $out.tmp -D $out.tmp && rm -rf $out.tmp
  restat = 1
  description = aconfig java $out
`

// ninja: the aconfig variable, rules and cache edges; empty when no declarations were built.
func (a *aconfigGraph) ninja() string {
	if a == nil || a.tool == "" {
		return ""
	}
	return fmt.Sprintf("\naconfig = %s\n%s%s", esc(a.tool), aconfigRules, a.b.String())
}

// aconfigValues: the value files for one package, in value set order (aconfig_value_set.go AvailablePackages).
func (g *Graph) aconfigValues(pkg string) []string {
	var out []string
	for _, set := range g.release.ValueSets {
		vs := g.lookup(set, Device)
		if vs == nil {
			g.problem("aconfig value set %s not found", set)
			g.notFound(set)
			continue
		}
		for _, m := range g.acfg.valueSet(g, vs)[pkg] {
			out = append(out, g.acfg.valueFiles(g, m)...)
		}
	}
	return out
}

// valueSet: an aconfig_value_set's aconfig_values modules by package, each list sorted by dir then name; indexed once per set (per package it re-globbed the set and scanned every device module).
func (a *aconfigGraph) valueSet(g *Graph, vs *Module) map[string][]*Module {
	if idx, ok := a.valueSets[vs]; ok {
		return idx
	}
	mods := vs.Props.strs("values")
	dirs := map[string]bool{}
	for _, bp := range g.globs(vs.Dir, vs.Props.strs("srcs")) {
		dirs[filepath.Dir(bp)] = true
	}
	var found []*Module
	for _, list := range g.mods[Device] {
		for _, m := range list {
			if m.Type == "aconfig_values" && (dirs[m.Dir] || slices.Contains(mods, m.Name)) {
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
	idx := map[string][]*Module{}
	for _, m := range found {
		pkg := m.Props.str("package")
		idx[pkg] = append(idx[pkg], m)
	}
	a.valueSets[vs] = idx
	return idx
}

// valueFiles: an aconfig_values module's files, sorted; globbed once.
func (a *aconfigGraph) valueFiles(g *Graph, m *Module) []string {
	if files, ok := a.values[m]; ok {
		return files
	}
	files := g.globs(m.Dir, m.Props.strs("srcs"))
	slices.Sort(files)
	a.values[m] = files
	return files
}

// aconfigDeclarations builds (once) the aconfig_declarations module ref names: create-cache over its declarations with the release's values.
// nil with a problem recorded without a release config or such a module.
func (g *Graph) aconfigDeclarations(ref, from string) *aconfigDecl {
	a := g.acfg
	if a == nil {
		g.problem("%s: aconfig_declarations %s needs -release", from, ref)
		return nil
	}
	m := g.lookup(ref, Device)
	if m == nil {
		g.problem("%s: aconfig_declarations %q not found", from, ref)
		g.notFound(ref)
		return nil
	}
	if m.Type != "aconfig_declarations" {
		g.problem("%s: %s is a %s, not aconfig_declarations", from, m.Name, m.Type)
		return nil
	}
	if d, ok := a.decls[m.Name]; ok {
		return d
	}
	if a.tool == "" {
		a.tool = g.jhost("aconfig", from)
	}
	p := g.resolveDefaults(m, Device, map[string]bool{})
	r := g.release
	pkg := p.str("package")
	decls := g.globs(m.Dir, p.strs("srcs"))
	values := g.aconfigValues(pkg)
	var declArgs, valueArgs []string
	for _, d := range decls {
		declArgs = append(declArgs, "--declarations", esc(g.abs(d)))
	}
	for _, v := range values {
		valueArgs = append(valueArgs, "--values", esc(g.abs(v)))
	}
	vars := [][2]string{{"package", pkg}, {"declarations", strings.Join(declArgs, " ")}, {"values", strings.Join(valueArgs, " ")}}
	if perm := r.Flag("RELEASE_ACONFIG_FLAG_DEFAULT_PERMISSION"); perm != "" {
		vars = append(vars, [2]string{"permission", "--default-permission " + perm})
	}
	// RELEASE_ACONFIG_REQUIRE_ALL_READ_ONLY is the negation of aconfig's allow-read-write
	vars = append(vars, [2]string{"readwrite", "--allow-read-write " + strconv.FormatBool(!r.Bool("RELEASE_ACONFIG_REQUIRE_ALL_READ_ONLY"))})
	if c := p.str("container"); c != "" {
		vars = append(vars, [2]string{"container", "--container " + c})
	}
	cache := filepath.Join(g.out, "aconfig", m.Name, "intermediate.pb")
	fmt.Fprintf(&a.b, "build %s: aconfig_cache %s | %s\n", esc(cache), join(absAll(g, append(append([]string{}, decls...), values...))), esc(a.tool))
	for _, v := range vars {
		fmt.Fprintf(&a.b, "  %s = %s\n", v[0], v[1])
	}
	d := &aconfigDecl{pkg: pkg, cache: cache, exportable: p.boolean("exportable", false)}
	a.decls[m.Name] = d
	return d
}

// aconfigMode: the module's codegen mode (default production); ok=false with a problem for one Soong rejects.
func (g *Graph) aconfigMode(name string, p *Obj) (string, bool) {
	mode := p.str("mode")
	if mode == "" {
		mode = "production"
	}
	if !slices.Contains(aconfigModes, mode) {
		g.problem("%s: aconfig mode %q is not a supported mode", name, mode)
		return "", false
	}
	return mode, true
}

// aconfigText: the declarations' flags as text (aconfig_declarations.go aconfigTextRule, intermediate.txt), aapt2's --feature-flags input;
// "" when the declarations are unusable.
func (g *Graph) aconfigText(ref, from string) string {
	d := g.aconfigDeclarations(ref, from)
	if d == nil {
		return ""
	}
	if d.text == "" {
		d.text = strings.TrimSuffix(d.cache, ".pb") + ".txt"
		fmt.Fprintf(&g.acfg.b, "build %s: aconfig_text %s | %s\n", esc(d.text), esc(d.cache), esc(g.acfg.tool))
	}
	return d.text
}

// aconfigJavaLib is java_aconfig_library: the generated flag classes compiled as a java library (GeneratedJavaLibraryModule).
func (g *Graph) aconfigJavaLib(n *jNode) {
	p := n.props
	r := g.release
	decl := g.aconfigDeclarations(p.str("aconfig_declarations"), n.name)
	if decl == nil {
		return
	}
	mode, ok := g.aconfigMode(n.name, p)
	if !ok {
		return
	}
	srcjar := filepath.Join(n.dir, "gen", n.name+".srcjar")
	g.java.edge("aconfig_java", []string{srcjar}, []string{decl.cache}, []string{g.acfg.tool}, nil, [][2]string{
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
	n.srcjar = srcjar
	if decl.exportable {
		// "possibly needing jarjar repackaging" (jarjar.go): renamed by a later jarjar_prefix
		n.renames = map[string]string{}
		for _, c := range []string{"Flags", "FeatureFlags", "FeatureFlagsImpl", "CustomFeatureFlags", "FakeFeatureFlagsImpl"} {
			n.renames[decl.pkg+"."+c] = ""
		}
	}
	g.javaLib(n, srcjar)
}

// cc_aconfig_library (codegen/cc_aconfig_library.go, a cc.GeneratedCcLibraryModuleFactory library): one generated source, built like a cc_library.
//   - aconfig create-cpp-lib --mode <mode> --cache <declarations cache> --out <gendir> writes <gendir>/<pkg>.cc and <gendir>/include/<pkg>.h,
//     pkg = the package with `.` -> `_` (aconfig codegen_cpp.rs); gendir is emptied first, so it belongs to this module alone.
//   - include/ is on the module's include path and re-exported (GeneratorSources IncludeDirs + ReexportedDirs).
//   - Unless mode is force-read-only: shared_libs libaconfig_storage_read_api_cc, libbase, liblog (GeneratorDeps), not re-exported.
const ccAconfigType = "cc_aconfig_library"

var ccAconfigLibs = []string{"libaconfig_storage_read_api_cc", "libbase", "liblog"}

// ccAconfigDeps: the shared libraries the generated code needs, added to n's properties before its dependencies resolve.
func (g *Graph) ccAconfigDeps(n *Node) {
	if n.props.str("mode") == "force-read-only" {
		return
	}
	n.props.set("shared_libs", strVals(append(n.props.strs("shared_libs"), ccAconfigLibs...)))
}

// ccAconfigGen: the generator of a cc_aconfig_library node; rule "" when the declarations or the mode are unusable (problem recorded).
func (g *Graph) ccAconfigGen(n *Node) genOut {
	decl := g.aconfigDeclarations(n.props.str("aconfig_declarations"), n.mod.Name)
	if decl == nil {
		return genOut{}
	}
	mode, ok := g.aconfigMode(n.mod.Name, n.props)
	if !ok {
		return genOut{}
	}
	dir := g.genDir(n, "aconfig")
	base := strings.ReplaceAll(decl.pkg, ".", "_")
	cc, hdr := filepath.Join(dir, base+".cc"), filepath.Join(dir, "include", base+".h")
	inc := "-I" + filepath.Join(dir, "include")
	return genOut{
		edge: genEdge{rule: "aconfig_cc", outs: []string{cc, hdr}, in: decl.cache, implicit: []string{g.acfg.tool}, vars: [][2]string{
			{"gendir", esc(dir)}, {"mode", mode},
		}},
		srcs: []string{cc}, waits: []string{hdr}, incs: []string{inc}, exports: []string{inc},
	}
}
