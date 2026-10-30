// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
)

// hidl_interface: Soong's load hook (system/tools/hidl/build/{hidl_interface.go hidlInterfaceMutator, fqName.go, hidl_package_root.go, properties.go})
// creates modules around the interface `<pkg>@<maj>.<min>` (the module's own name), none of them in an Android.bp. lookup falls back to synth (synth.go),
// which calls synthHidl; a hidl_interface found by name is replaced by its cc library.
//   - `<fq>_genc++` / `<fq>_genc++_headers`: genrules running `hidl-gen -L c++-sources|c++-headers` once for the package. Outputs below the gen dir:
//     sources `<pkg path>/<ver>/<X>All.cpp` per I<X>.hal and `<name>.cpp` per other .hal; headers I<X>, Bs<X>, BnHw<X>, BpHw<X>, IHw<X> per interface
//     and `<name>.h`, `hw<name>.h` per other .hal. The gen dir is the include dir. Core packages (android.hidl.base@, android.hidl.manager@) get
//     these two as well: libhidlbase's Android.bp builds them.
//   - `<fq>`: cc_library with defaults hidl-module-defaults, those two as generated_sources / generated_headers (exported), shared libhidlbase, liblog,
//     libutils, libcutils plus the `interfaces` (core packages are in libhidlbase, so left out); libhidlbase, libutils and the interfaces are exported.
//     Core packages have no library.
//   - `<fq>_hal`: filegroup of srcs.
//   - hidl-gen options: `-r<root>:<path>` of the interface's root and of every `interfaces` entry (hidl_package_root: path default its own dir), `-F` for
//     the root's require_frozen; the root's current.txt (use_current: must exist) is an input. hidl-gen runs from the tree root with `-p .`.
//   - Not built, one problem each: the java, java-shallow, java-constants, lint and inheritance-hierarchy modules.
//   - Dropped: Soong's `rm -rf ${genDir}` (the gen dir holds the rule's script) and `-d ${out}.d` (the genrule rule has no depfile: edits to an imported
//     package do not rebuild), Soong's -Wall -Werror flags of hidl-module-defaults (this build uses -w), min_sdk_version, double_loadable, apex_available.

var (
	hidlFqRe = regexp.MustCompile(`^([a-zA-Z_][a-zA-Z_0-9]*(?:\.[a-zA-Z_][a-zA-Z_0-9]*)*)@([0-9]+)\.([0-9]+)$`)
	// fqName.go javaName & co: sanitizedString is `<pkg>-V<maj>.<min>`
	hidlJavaRe = regexp.MustCompile(`^(.+)-V([0-9]+\.[0-9]+)-java(?:-shallow|_gen_java|-constants(?:_gen_java)?)?$`)
	// properties.go coreDependencyPackageNames: packages in libhidlbase
	hidlCorePackages = []string{"android.hidl.base@", "android.hidl.manager@"}
)

// hidl_interface backends by the suffix of their names (fqName.go).
const (
	hidlLib     = ""
	hidlSources = "_genc++"
	hidlHeaders = "_genc++_headers"
	hidlHal     = "_hal"
	// not built: java, lint, inheritance-hierarchy modules
	hidlOther = "other"
)

// hidlFq is a parsed `<pkg>@<maj>.<min>` (fqName.go).
type hidlFq struct{ pkg, major, minor string }

func parseHidlFq(s string) (hidlFq, bool) {
	m := hidlFqRe.FindStringSubmatch(s)
	if m == nil {
		return hidlFq{}, false
	}
	return hidlFq{m[1], m[2], m[3]}, true
}

func (f hidlFq) String() string { return f.pkg + "@" + f.major + "." + f.minor }

// dir: fqName.dir, the output directory below the gen dir.
func (f hidlFq) dir() string {
	return filepath.Join(strings.ReplaceAll(f.pkg, ".", "/"), f.major+"."+f.minor) + "/"
}

// inPackage: fqName.inPackage, `root` is a leading run of package components.
func (f hidlFq) inPackage(root string) bool {
	return f.pkg == root || strings.HasPrefix(f.pkg, root+".")
}

func (f hidlFq) core() bool {
	return slices.ContainsFunc(hidlCorePackages, func(p string) bool { return strings.HasPrefix(f.String(), p) })
}

// parseHidlRef splits a name a hidl_interface makes into the interface it belongs to and which module that is (hidl*).
func parseHidlRef(ref string) (fq, kind string, ok bool) {
	ref = strings.TrimPrefix(ref, ":")
	if m := hidlJavaRe.FindStringSubmatch(ref); m != nil {
		return m[1] + "@" + m[2], hidlOther, true
	}
	for _, s := range []string{"-hidl-lint", "-inheritance-hierarchy"} {
		if base, found := strings.CutSuffix(ref, s); found {
			return base, hidlOther, hidlFqRe.MatchString(base)
		}
	}
	for _, s := range []string{hidlHeaders, hidlSources, hidlHal} {
		if base, found := strings.CutSuffix(ref, s); found {
			return base, s, hidlFqRe.MatchString(base)
		}
	}
	return ref, hidlLib, hidlFqRe.MatchString(ref)
}

// hidlOwned: ref names a module of a hidl_interface the tree has (also one with a malformed name, which lookup reported).
func (g *Graph) hidlOwned(ref string) bool {
	has := func(name string) bool {
		return g.rawModule(name, Device, "hidl_interface") != nil || g.rawModule(name, Host, "hidl_interface") != nil
	}
	fq, _, ok := parseHidlRef(ref)
	return ok && has(fq) || has(strings.TrimPrefix(ref, ":"))
}

// hidlIface is a hidl_interface module as one side sees it.
type hidlIface struct {
	mod  *Module
	fq   hidlFq
	srcs []string // tree-relative .hal files
	// base names of the sources: `Foo` of IFoo.hal, `types` of types.hal (processSources)
	ifaces, types []string
	deps          []string // `interfaces`
	rootOpt       string   // -r<root>:<path>
	frozen        bool     // the root's require_frozen
	current       string   // tree-relative current.txt of the root, "" = none
	opts          []string // hidl-gen options (hidlOptions)
}

// hidlIfaceOf: the usable hidl_interface module m, nil with a problem when its load hook would fail.
func (g *Graph) hidlIfaceOf(m *Module, side Side) *hidlIface {
	k := m.Name + "/" + side.String()
	if h, ok := g.hidlIfaces[k]; ok {
		return h
	}
	h := g.newHidlIface(m, side)
	if g.hidlIfaces == nil {
		g.hidlIfaces = map[string]*hidlIface{}
	}
	g.hidlIfaces[k] = h
	return h
}

func (g *Graph) newHidlIface(m *Module, side Side) *hidlIface {
	fq, ok := parseHidlFq(m.Name)
	if !ok {
		g.problem("%s: poorly formed hal interface name, must match %s", m.Name, hidlFqRe)
		return nil
	}
	p := m.Props
	root := p.str("root")
	if !fq.inPackage(root) {
		g.problem("%s: root %q must be a prefix of the name", m.Name, root)
		return nil
	}
	rm := g.rawModule(root, side, "hidl_package_root")
	if rm == nil {
		g.problem("%s: no hidl_package_root %s", m.Name, root)
		g.notFound(root)
		return nil
	}
	h := &hidlIface{mod: m, fq: fq, deps: p.strs("interfaces")}
	for _, s := range p.strs("srcs") {
		name, isHal := strings.CutSuffix(s, ".hal")
		if !isHal {
			g.problem("%s: srcs: source must be a .hal file: %s", m.Name, s)
			return nil
		}
		h.srcs = append(h.srcs, filepath.Join(m.Dir, s))
		if base, isIface := strings.CutPrefix(name, "I"); isIface {
			h.ifaces = append(h.ifaces, base)
		} else {
			h.types = append(h.types, name)
		}
	}
	if len(h.srcs) == 0 {
		g.problem("%s: no sources provided", m.Name)
		return nil
	}
	for _, d := range h.deps {
		if _, ok := parseHidlFq(d); !ok {
			g.problem("%s: interfaces: poorly formed hal interface name %s", m.Name, d)
			return nil
		}
	}
	// hidl_package_root: path defaults to the module's directory; current.txt is there, required with use_current
	rp := rm.Props
	path := rp.str("path")
	if path == "" {
		path = rm.Dir
	}
	h.rootOpt = "-r" + root + ":" + path
	h.frozen = rp.boolean("require_frozen", false)
	useCurrent := rp.boolean("use_current", false)
	if useCurrent && path != rm.Dir {
		g.problem("%s: hidl_package_root %s: cannot use unrelated path %s with use_current", m.Name, root, path)
		return nil
	}
	cur := filepath.Join(rm.Dir, "current.txt")
	if _, err := os.Stat(g.abs(cur)); err == nil {
		h.current = cur
	} else if useCurrent {
		g.problem("%s: hidl_package_root %s: use_current but %s does not exist", m.Name, root, cur)
		return nil
	}
	return h
}

// hidlOptions: the options hidl-gen gets beside -L (hidlGenRule.GenerateAndroidBuildActions): the roots of the interface and of its `interfaces`, then
// -F. An `interfaces` entry the tree lacks is a missing module (the project to fetch) and a problem, once.
func (g *Graph) hidlOptions(h *hidlIface, side Side) []string {
	if h.opts != nil {
		return h.opts
	}
	opts := []string{h.rootOpt}
	for _, d := range h.deps {
		dm := g.rawModule(d, side, "hidl_interface")
		if dm == nil {
			g.problem("%s: hidl_interface %s not found", h.mod.Name, d)
			g.notFound(d)
			continue
		}
		if dh := g.hidlIfaceOf(dm, side); dh != nil {
			opts = appendMissing(opts, []string{dh.rootOpt})
		}
	}
	if h.frozen {
		opts = append(opts, "-F")
	}
	h.opts = opts
	return opts
}

// outputs: what hidl-gen writes below the gen dir for the sources or the headers (the Outputs of the two hidlGenRule modules).
func (h *hidlIface) outputs(headers bool) []string {
	dir := h.fq.dir()
	wrap := func(prefix string, names []string, suffix string) (out []string) {
		for _, n := range names {
			out = append(out, dir+prefix+n+suffix)
		}
		return out
	}
	if !headers {
		return append(wrap("", h.ifaces, "All.cpp"), wrap("", h.types, ".cpp")...)
	}
	var out []string
	for _, p := range []string{"I", "Bs", "BnHw", "BpHw", "IHw"} {
		out = append(out, wrap(p, h.ifaces, ".h")...)
	}
	return append(append(out, wrap("", h.types, ".h")...), wrap("hw", h.types, ".h")...)
}

// hidlGen is the hidl-gen run of a `_genc++[_headers]` module.
type hidlGen struct {
	h       *hidlIface
	lang    string // hidl-gen -L
	headers bool
}

// synthHidl: the module a hidl_interface makes under the name ref (synth.go).
func (g *Graph) synthHidl(ref string, side Side) *Module {
	fq, kind, ok := parseHidlRef(ref)
	if !ok {
		return nil
	}
	m := g.rawModule(fq, side, "hidl_interface")
	if m == nil {
		return nil
	}
	h := g.hidlIfaceOf(m, side)
	if h == nil {
		return nil
	}
	switch kind {
	case hidlSources:
		return &Module{Type: "genrule", Name: ref, Dir: m.Dir, Props: newObj(), hidl: &hidlGen{h: h, lang: "c++-sources"}}
	case hidlHeaders:
		return &Module{Type: "genrule", Name: ref, Dir: m.Dir, Props: newObj(), hidl: &hidlGen{h: h, lang: "c++-headers", headers: true}}
	case hidlHal:
		props := newObj()
		props.set("srcs", strVals(m.Props.strs("srcs")))
		return &Module{Type: "filegroup", Name: ref, Dir: m.Dir, Props: props}
	case hidlOther:
		g.problem("%s: only the C++ modules of hidl_interface %s are built", ref, fq)
		return nil
	}
	// hidlLib: the interface's own name, lookup finds the hidl_interface and calls hidlLibrary
	return nil
}

// hidlLibrary: the cc_library of the hidl_interface m (hidlInterfaceMutator's cc.LibraryFactory call); lookup returns it for m's own name.
func (g *Graph) hidlLibrary(m *Module, side Side) *Module {
	return g.synthMemo(m.Name, side, func() *Module { return g.newHidlLibrary(m, side) })
}

func (g *Graph) newHidlLibrary(m *Module, side Side) *Module {
	h := g.hidlIfaceOf(m, side)
	if h == nil {
		return nil
	}
	if h.fq.core() {
		g.problem("%s: a core HIDL package has no library: libhidlbase contains it", m.Name)
		return nil
	}
	var cppDeps []string
	for _, d := range h.deps {
		// parsed by newHidlIface
		if fq, _ := parseHidlFq(d); !fq.core() {
			cppDeps = append(cppDeps, fq.String())
		}
	}
	name := h.fq.String()
	props := newObj()
	props.set("defaults", strVals([]string{"hidl-module-defaults"}))
	props.set("generated_sources", strVals([]string{name + hidlSources}))
	props.set("generated_headers", strVals([]string{name + hidlHeaders}))
	props.set("shared_libs", strVals(slices.Concat(cppDeps, []string{"libhidlbase", "liblog", "libutils", "libcutils"})))
	props.set("export_shared_lib_headers", strVals(slices.Concat(cppDeps, []string{"libhidlbase", "libutils"})))
	props.set("export_generated_headers", strVals([]string{name + hidlHeaders}))
	return &Module{Type: "cc_library", Name: name, Dir: h.mod.Dir, Props: props}
}

// hidlRule: the genrule of a `_genc++[_headers]` module. The command runs from the tree root, where hidl-gen's `-p .` and the `-r` paths are meant to.
func (g *Graph) hidlRule(n *Node) *genRule {
	spec := n.mod.hidl
	h := spec.h
	r, fail := g.newGenRule(n)
	genDir := g.genDir(n, "")
	r.hasTools = true
	r.labels = []genLabel{g.genTool(n, r, "hidl-gen", fail)}
	for _, s := range h.srcs {
		if _, err := os.Stat(g.abs(s)); err != nil {
			fail("input %s does not exist", s)
		}
		r.inputs = append(r.inputs, s)
	}
	if h.current != "" {
		r.implicit = append(r.implicit, h.current)
	}
	cmd := []string{"$(location hidl-gen)", "-R", "-p", ".", "-o", "$(genDir)", "-L", spec.lang}
	for _, o := range g.hidlOptions(h, n.side) {
		cmd = append(cmd, shellEscape(o))
	}
	r.cmd = strings.Join(append(cmd, shellEscape(h.fq.String())), " ")
	for _, o := range h.outputs(spec.headers) {
		r.outs = append(r.outs, filepath.Join(genDir, o))
	}
	r.incs = []string{"-I" + genDir}
	r.consumerDeps(genDir)
	return r
}
