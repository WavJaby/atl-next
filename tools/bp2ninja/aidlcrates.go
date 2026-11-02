// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"sort"
	"strings"
)

// AIDL crates of workspace packages (-aidl-crates; Cargo.toml `[package.metadata.atl.aidl]`, xtask packages.rs). Each becomes an unstable aidl_interface at the tree root whose `<name>-rust`
// backend synthAidl + rustAidl build like any tree interface. Its sources are not a srcs list but the import closure of `types` over `include_dirs`, what aidl itself would read:
//   - include_dirs: tree-relative import roots, as Soong's framework `aidl.export_include_dirs` (-I); a compiled file's root is the dir it was found in (-N).
//   - types: qualified AIDL names; each, and every interface / structured parcelable / enum / union it references (imports, same-package names, qualified names), is compiled.
//   - parcelables (optional): the crate giving Rust types to every unstructured `parcelable X;` declared under include_dirs without its own rust_type (all of them, not the closure's: one
//     scan, no second walk to keep in step), through a generated preprocessed file `parcelable X rust_type "<crate>::<x path>";` (-p; aidl skips imports it defines), and to the Java-only
//     builtins (aidlJavaOnly) through the host aidl patch's --rust_java_type (every aidl crate also gets its --rust_unimplemented_hook: our services implement a subset). The crate is a rust_library the converter builds from the package's cargo dir (Soong's additional_rustlibs):
//     root <dir>/src/lib.rs, its rustlibs, plus a generated `parcelables.rs` in OUT_DIR (rust_aidl_parcelables, rustsrc.go) declaring each type: `pub use crate::codecs::<path>::<Name>::<Name>;`
//     where <dir>/src/codecs/<java path>/<Name>.rs exists (a hand-written codec), else `crate::opaque!(<Name>[<T>], "<qualified>");`, in modules mirroring the Java packages; a nested class
//     `Outer.Inner` is the type `Outer_Inner` (aidlDecl.rustName).
//     The parcelables crate is ours: compiled by clippy-driver with its package's lints (AidlParcelables.Lints), never --cap-lints allow; `aconfig` names aconfig_declarations whose
//     flag dump (aconfig dump-cache, aconfig.go aconfigText: `<package>.<flag>:<permission>=<bool>` lines) lands in its OUT_DIR, so a codec reads a flag as aconfig evaluated it.
// A name a tree module already has, an include dir or type that does not exist, a type that is not compiled code, or a codec file naming no declared type is an error: nothing would
// build what the package asked for.

// AidlCrate is one -aidl-crates entry.
type AidlCrate struct {
	IncludeDirs []string         `json:"include_dirs"`
	Types       []string         `json:"types"`
	Parcelables *AidlParcelables `json:"parcelables,omitempty"`
}

// AidlParcelables is the rust_library giving unstructured parcelables their Rust types (Cargo.toml `[package.metadata.atl] rust_library` of the package `aidl.parcelables` names).
type AidlParcelables struct {
	Module   string   `json:"module"`  // its module name, lib<crate>
	Crate    string   `json:"crate"`   // crate_name
	Dir      string   `json:"dir"`     // the package's cargo dir, absolute
	Edition  string   `json:"edition"` // the package's edition
	Rustlibs []string `json:"rustlibs"`
	// aconfig_declarations whose flag dumps go into its OUT_DIR
	Aconfig []string `json:"aconfig"`
	// rustc / clippy lint flags (the workspace's `[workspace.lints]`, xtask packages.rs); at least -D warnings
	Lints []string `json:"lints"`
}

// aidlJavaOnly: the builtins aidl accepts for Java only (aidl_language.cpp LanguageSpecificCheckValid), by the Java class the Java backend spells each as (aidl_to_java.cpp); the Rust
// backend takes them as `--rust_java_type <builtin>=<crate>::<class path>` (patches/aosp/system/tools/aidl).
var aidlJavaOnly = [][2]string{{"Map", "java.util.Map"}, {"CharSequence", "java.lang.CharSequence"}, {"FileDescriptor", "java.io.FileDescriptor"}, {"List", "java.util.List"}}

// aidlCrateSpec is what an aidl crate's interface compiles (Module.aidlCrate).
type aidlCrateSpec struct {
	srcs         []srcFile
	includes     []string
	preprocessed string   // absolute, "" without parcelables
	javaTypes    []string // --rust_java_type values
	rustlibs     []string // additional_rustlibs: the parcelables crate
}

// aidlParcelablesType: the source provider generating a parcelables crate's parcelables.rs (rustsrc.go row); its module carries the text (Module.genText).
const aidlParcelablesType = "rust_aidl_parcelables"

// AIDL source scanning: comments stripped, then declarations by keyword. Enough for the closure; aidl itself parses every file it compiles.
var (
	aidlCommentRe  = regexp.MustCompile(`(?s)/\*.*?\*/|//[^\n]*`)
	aidlPackageRe  = regexp.MustCompile(`\bpackage\s+([\w.]+)\s*;`)
	aidlImportRe   = regexp.MustCompile(`\bimport\s+([\w.]+)\s*;`)
	aidlUnstructRe = regexp.MustCompile(`\bparcelable\s+([\w.]+)\s*(<[^>]*>)?\s*([^;{]*);`)
	aidlCodeRe     = regexp.MustCompile(`\b(interface|enum|union)\b|\bparcelable\s+[\w.]+\s*(<[^>]*>)?\s*\{`)
	aidlNameRe     = regexp.MustCompile(`\b[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*\b`)
)

// aidlScanned is one .aidl file as the closure sees it.
type aidlScanned struct {
	path, root, pkg string // tree-relative path, its include dir, its package
	code            bool   // declares an interface, structured parcelable, enum or union: compiled
	imports, names  []string
	unstructured    []aidlDecl
}

// aidlDecl is one unstructured parcelable declaration.
type aidlDecl struct {
	name     string // qualified
	pkg      string // its package; name = pkg + "." + class, class may be nested (`Outer.Inner`)
	params   string // "<T>" or ""
	rustType bool   // carries its own rust_type
}

// rustName: the declaration's Rust path below the crate: package segments as modules, the class flattened (`Outer.Inner` -> `Outer_Inner`): a module named after the outer class
// would share the type namespace with the outer class's own type. A class named like what libbinder_rs's impl_(de)serialize_for_unstructured_parcelable macros name unqualified
// (rustMacroNames) gets a `_` suffix: the item would shadow it where the macros expand (android.os.StatusCode, VoiceInteractor.PickOptionRequest.Option).
func (d aidlDecl) rustName() (mods []string, typ string) {
	if d.pkg != "" {
		mods = strings.Split(d.pkg, ".")
	}
	typ = strings.ReplaceAll(strings.TrimPrefix(d.name, d.pkg+"."), ".", "_")
	if slices.Contains(rustMacroNames, typ) {
		typ += "_"
	}
	return mods, typ
}

// rustMacroNames: names libbinder_rs's unstructured-parcelable impl macros (rust/src/parcel/parcelable.rs) use without a path.
var rustMacroNames = []string{"Option", "Some", "None", "Ok", "Err", "Result", "StatusCode", "Default", "Self"}

// rustPath: <crate>::<package modules>::<flattened class>.
func (d aidlDecl) rustPath(crate string) string {
	mods, typ := d.rustName()
	segs := []string{crate}
	for _, m := range mods {
		segs = append(segs, rustIdent(m))
	}
	return strings.Join(append(segs, rustIdent(typ)), "::")
}

func scanAidl(text string) (pkg string, code bool, imports, names []string, decls []aidlDecl) {
	text = aidlCommentRe.ReplaceAllString(text, " ")
	if m := aidlPackageRe.FindStringSubmatch(text); m != nil {
		pkg = m[1]
	}
	for _, m := range aidlImportRe.FindAllStringSubmatch(text, -1) {
		imports = append(imports, m[1])
	}
	code = aidlCodeRe.MatchString(text)
	if !code {
		for _, m := range aidlUnstructRe.FindAllStringSubmatch(text, -1) {
			decls = append(decls, aidlDecl{name: pkg + "." + m[1], pkg: pkg, params: strings.Join(strings.Fields(m[2]), ""), rustType: strings.Contains(m[3], "rust_type")})
		}
		return
	}
	names = aidlNameRe.FindAllString(text, -1)
	return
}

// aidlIndex: every .aidl under the include dirs, first dir wins for a qualified name (aidl's -I order).
type aidlIndex struct {
	files map[string]*aidlScanned // qualified name -> file (compiled: pkg.<stem>; unstructured: each declaration)
	decls []aidlDecl              // unstructured declarations, first per name, in dir then path order
}

func (g *Graph) indexAidl(dirs []string) (*aidlIndex, error) {
	x := &aidlIndex{files: map[string]*aidlScanned{}}
	for _, d := range dirs {
		if st, err := os.Stat(g.abs(d)); err != nil || !st.IsDir() {
			return nil, fmt.Errorf("include dir %s: not a directory in the tree", d)
		}
		paths := globTree(g.abs(d), "**/*.aidl", d)
		sort.Strings(paths)
		for _, p := range paths {
			b, err := os.ReadFile(g.abs(p))
			if err != nil {
				return nil, err
			}
			f := &aidlScanned{path: p, root: d}
			f.pkg, f.code, f.imports, f.names, f.unstructured = scanAidl(string(b))
			if f.code {
				q := f.pkg + "." + strings.TrimSuffix(filepath.Base(p), ".aidl")
				if _, dup := x.files[q]; !dup {
					x.files[q] = f
				}
				continue
			}
			for _, dcl := range f.unstructured {
				if _, dup := x.files[dcl.name]; !dup {
					x.files[dcl.name] = f
					x.decls = append(x.decls, dcl)
				}
			}
		}
	}
	return x, nil
}

// lookup: the file defining q, or the file of its outer type (nested `a.b.C.D` -> a.b.C).
func (x *aidlIndex) lookup(q string) *aidlScanned {
	for {
		if f, ok := x.files[q]; ok {
			return f
		}
		i := strings.LastIndexByte(q, '.')
		if i < 0 {
			return nil
		}
		q = q[:i]
	}
}

// closure: the compiled files `types` reach, in visit order.
func (x *aidlIndex) closure(types []string) ([]*aidlScanned, error) {
	var out, queue []*aidlScanned
	seen := map[*aidlScanned]bool{}
	for _, t := range types {
		f := x.files[t]
		if f == nil || !f.code {
			return nil, fmt.Errorf("type %s: no interface, structured parcelable, enum or union of that name under the include dirs", t)
		}
		queue = append(queue, f)
	}
	for len(queue) > 0 {
		f := queue[0]
		queue = queue[1:]
		if seen[f] {
			continue
		}
		seen[f] = true
		out = append(out, f)
		for _, n := range slices.Concat(f.imports, f.names) {
			r := x.lookup(n)
			if r == nil && !strings.Contains(n, ".") {
				r = x.lookup(f.pkg + "." + n)
			}
			if r != nil && r.code && !seen[r] {
				queue = append(queue, r)
			}
		}
	}
	return out, nil
}

// rustIdent: a Java package or class name segment as a Rust path segment (raw where it is a keyword).
func rustIdent(s string) string {
	switch s {
	case "as", "break", "const", "continue", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
		"return", "static", "struct", "trait", "true", "type", "unsafe", "use", "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
		"override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen":
		return "r#" + s
	}
	return s
}

func (g *Graph) addAidlCrates(crates map[string]AidlCrate) error {
	for _, name := range sortedKeys(crates) {
		c := crates[name]
		if len(c.IncludeDirs) == 0 || len(c.Types) == 0 {
			return fmt.Errorf("aidl crate %s: needs include_dirs and types", name)
		}
		if len(g.mods[Device][name]) > 0 {
			return fmt.Errorf("aidl crate %s: the tree already has a module of that name", name)
		}
		x, err := g.indexAidl(c.IncludeDirs)
		if err != nil {
			return fmt.Errorf("aidl crate %s: %w", name, err)
		}
		files, err := x.closure(c.Types)
		if err != nil {
			return fmt.Errorf("aidl crate %s: %w", name, err)
		}
		spec := &aidlCrateSpec{includes: c.IncludeDirs}
		for _, f := range files {
			spec.srcs = append(spec.srcs, srcFile{path: f.path, root: f.root})
		}
		if p := c.Parcelables; p != nil {
			if err := g.addParcelables(name, p, x, spec); err != nil {
				return fmt.Errorf("aidl crate %s: %w", name, err)
			}
		}
		props := newObj()
		props.set("unstable", true)
		g.mods[Device][name] = append(g.mods[Device][name], &Module{Type: "aidl_interface", Name: name, Props: props, aidlCrate: spec})
	}
	return nil
}

// addParcelables: the preprocessed file and Java-only flags of one crate's interface, and the parcelables rust_library with its generated parcelables.rs.
func (g *Graph) addParcelables(name string, p *AidlParcelables, x *aidlIndex, spec *aidlCrateSpec) error {
	if p.Module == "" || p.Crate == "" || p.Dir == "" {
		return fmt.Errorf("parcelables: module, crate and dir are required")
	}
	root := filepath.Join(p.Dir, "src", "lib.rs")
	if _, err := os.Stat(root); err != nil {
		return fmt.Errorf("parcelables %s: %w", p.Module, err)
	}
	// hand-written codecs: src/codecs/<package path>/<flattened class>.rs (mod.rs files are module glue), keyed by that Rust path below the crate
	codecs := map[string]bool{}
	codecDir := filepath.Join(p.Dir, "src", "codecs")
	for _, f := range globTree(codecDir, "**/*.rs", "") {
		rel := strings.TrimSuffix(strings.TrimPrefix(f, "/"), ".rs")
		if filepath.Base(rel) != "mod" {
			codecs[strings.ReplaceAll(rel, "/", "::")] = true
		}
	}
	var pre strings.Builder
	fmt.Fprintf(&pre, "// generated by bp2ninja from the unstructured parcelables under the include dirs of %s, do not edit\n", name)
	tree := &parcelMod{nested: map[string]*parcelMod{}}
	for _, d := range x.decls {
		if d.rustType {
			continue
		}
		fmt.Fprintf(&pre, "parcelable %s%s rust_type \"%s\";\n", d.name, d.params, d.rustPath(p.Crate))
		tree.add(d, codecs)
	}
	for _, j := range aidlJavaOnly {
		d := aidlDecl{name: j[1], pkg: j[1][:strings.LastIndexByte(j[1], '.')]}
		spec.javaTypes = append(spec.javaTypes, j[0]+"="+d.rustPath(p.Crate))
		tree.add(d, codecs)
	}
	for _, q := range sortedKeys(codecs) {
		if !tree.has(q) {
			return fmt.Errorf("parcelables %s: codec src/codecs/%s.rs names no unstructured parcelable or Java-only builtin", p.Module, strings.ReplaceAll(q, "::", "/"))
		}
	}
	spec.preprocessed = filepath.Join(g.out, "aidl", "device", name, "parcelables.aidl")
	g.genFile(spec.preprocessed, pre.String())
	spec.rustlibs = []string{p.Module}
	if len(g.mods[Device][p.Module]) > 0 {
		return fmt.Errorf("parcelables %s: the tree already has a module of that name", p.Module)
	}
	var rs strings.Builder
	fmt.Fprintf(&rs, "// generated by bp2ninja from the unstructured parcelables of %s and %s, do not edit\n", name, codecDir)
	tree.emit(&rs, "")
	gen := p.Module + "-types"
	g.mods[Device][gen] = []*Module{{Type: aidlParcelablesType, Name: gen, Props: newObj(), genText: rs.String(), aconfig: p.Aconfig}}
	if !slices.Contains(p.Lints, "warnings") {
		return fmt.Errorf("parcelables %s: lints %v lack -D warnings", p.Module, p.Lints)
	}
	props := newObj()
	props.set("crate_name", p.Crate)
	props.set("edition", p.Edition)
	props.set("srcs", strVals([]string{root, ":" + gen}))
	props.set("rustlibs", strVals(p.Rustlibs))
	g.mods[Device][p.Module] = []*Module{{Type: "rust_library", Name: p.Module, Props: props, lints: p.Lints}}
	return nil
}

// parcelMod: one Java package of parcelables.rs; types and sub-packages in sorted order when emitted.
type parcelMod struct {
	items  map[string]string // Rust item by flattened class name
	nested map[string]*parcelMod
}

func (m *parcelMod) add(d aidlDecl, codecs map[string]bool) {
	mods, typ := d.rustName()
	for _, s := range mods {
		c, ok := m.nested[s]
		if !ok {
			c = &parcelMod{nested: map[string]*parcelMod{}}
			m.nested[s] = c
		}
		m = c
	}
	if m.items == nil {
		m.items = map[string]string{}
	}
	t := rustIdent(typ)
	if codecs[strings.Join(append(mods, typ), "::")] {
		m.items[typ] = fmt.Sprintf("pub use %s::%s;", d.rustPath("crate::codecs"), t)
	} else {
		m.items[typ] = fmt.Sprintf("crate::opaque!(%s%s, %q);", t, d.params, d.name)
	}
}

// has: a type at Rust path q (`a::b::C`) below the crate.
func (m *parcelMod) has(q string) bool {
	segs := strings.Split(q, "::")
	for _, s := range segs[:len(segs)-1] {
		if m = m.nested[s]; m == nil {
			return false
		}
	}
	_, ok := m.items[segs[len(segs)-1]]
	return ok
}

func (m *parcelMod) emit(b *strings.Builder, indent string) {
	for _, t := range sortedKeys(m.items) {
		fmt.Fprintf(b, "%s%s\n", indent, m.items[t])
	}
	for _, s := range sortedKeys(m.nested) {
		fmt.Fprintf(b, "%spub mod %s {\n", indent, rustIdent(s))
		m.nested[s].emit(b, indent+"  ")
		fmt.Fprintf(b, "%s}\n", indent)
	}
}
