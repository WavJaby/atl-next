// SPDX-License-Identifier: MIT

package main

import (
	"bufio"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
)

// globTree matches pattern (Soong glob: `*`, `?`, `**/`) below base; results are prefixed with dir.
func globTree(base, pattern, dir string) []string {
	var re strings.Builder
	re.WriteString("^")
	for i := 0; i < len(pattern); i++ {
		switch c := pattern[i]; {
		case strings.HasPrefix(pattern[i:], "**/"):
			re.WriteString("(?:.*/)?")
			i += 2
		case c == '*':
			re.WriteString("[^/]*")
		case c == '?':
			re.WriteString("[^/]")
		default:
			re.WriteString(regexp.QuoteMeta(string(c)))
		}
	}
	re.WriteString("$")
	rx := regexp.MustCompile(re.String())
	var out []string
	for _, rel := range treeFiles(base) {
		if rx.MatchString(rel) {
			out = append(out, filepath.Join(dir, rel))
		}
	}
	return out
}

// treeFiles: the non-directories below base, relative, in walk order; walked once per run (recorder.trees): many globs share a base (every `**/*.aconfig` or Java srcs glob of one big module directory).
func treeFiles(base string) []string {
	if files, ok := rec.trees[base]; ok {
		return files
	}
	files := []string{}
	walkTree(base, probeList, func(path string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() {
			return nil
		}
		rel, _ := filepath.Rel(base, path)
		files = append(files, rel)
		return nil
	})
	rec.trees[base] = files
	return files
}

// Tools are the host programs the generated rules call. RustVersion: Rustc's version, Soong's ANDROID_RUST_VERSION, which crates read (rustversion).
// LibClang: the directory of CC's libclang.so, which bindgen loads at run time (LIBCLANG_PATH): the cc side's clang parses the headers bindgen reads.
// Launcher: compiler cache prefixed to the cc rule only (empty = none), except compiles with debug info (debugInfo). Not yasm (sccache does not know it) nor rustc: sccache 0.10 refuses rustc's `-o`, keys Rust on the cwd (= the out dir, so no hit across out dirs) and restores the dep-info of the out dir that filled the entry.
// BionicStaticRust: -bionic-static-rust (rust.go rustStaticlib). LicenceGate: -licence-gate (licence.go).
type Tools struct{ CC, CXX, AR, Bison, Flex, M4, Yasm, Rustc, RustVersion, LibClang, LLVM, Launcher, RtInclude, BionicStaticRust, LicenceGate string }

// ninjaFile: build.ninja written through a buffer into <path>.tmp; commit renames it into place, discard (deferred) removes an uncommitted one. A write error is kept by the buffer and returned by commit.
type ninjaFile struct {
	*bufio.Writer
	f    *os.File
	path string
	done bool
}

func newNinjaFile(path string) (*ninjaFile, error) {
	f, err := os.Create(path + ".tmp")
	if err != nil {
		return nil, err
	}
	return &ninjaFile{bufio.NewWriterSize(f, 1<<20), f, path, false}, nil
}

func (n *ninjaFile) commit() error {
	err := n.Flush()
	if cerr := n.f.Close(); err == nil {
		err = cerr
	}
	if err != nil {
		return err
	}
	if err := os.Rename(n.f.Name(), n.path); err != nil {
		return err
	}
	n.done = true
	return nil
}

func (n *ninjaFile) discard() {
	if !n.done {
		n.f.Close()
		os.Remove(n.f.Name())
	}
}

// ninjaEscaper: built once; a Replacer per call was 76 % of the converter's allocations (its tables are built on first use).
var ninjaEscaper = strings.NewReplacer("$", "$$", " ", "$ ", ":", "$:", "\n", "$\n")

func esc(s string) string {
	return ninjaEscaper.Replace(s)
}

func join(list []string) string {
	out := make([]string, len(list))
	for i, s := range list {
		out[i] = esc(s)
	}
	return strings.Join(out, " ")
}

// shellQuote for flag values passed through a ninja variable into /bin/sh.
func shellQuote(list []string) string {
	out := make([]string, len(list))
	for i, s := range list {
		if strings.ContainsAny(s, " \"'$\\()<>&;|*?") {
			s = "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
		}
		out[i] = strings.ReplaceAll(s, "$", "$$")
	}
	return strings.Join(out, " ")
}

// header: the rules. Every rule that can run Python (genrule: script tools and shebang scripts; javagen logtags) sets PYTHONDONTWRITEBYTECODE=1:
// Python otherwise writes __pycache__ beside the scripts, into the shared src/ checkouts, which the drift test then refuses.
// link writes $out.trace, lld's --trace list of the objects and archive members it loaded: the linked set xtask's licence gate checks (like a depfile, a side file, not a ninja output).
// link has Soong's `restat` (cc/builder.go ld: an output the linker left untouched stops there); the gate stays exact: lld writes $out anew on every link, and every trace is in the gate edge's depfile, rewritten by `>` on every link.
// Soong's rsp also holds the lib flags; ours keeps $libs on the command line (archives and -l flags, short). ar, no_addrsig, lex: Soong's commands (cc/builder.go ar, noAddrSig; cc/gen.go lex: M4= pins flex's m4 against an ambient $M4).
// Every depfile rule is `deps = gcc`, as Soong's are: ninja folds the depfile into .ninja_deps after the step. Without it ninja re-parses the depfile at load and refuses one naming an
// undeclared output -- protoc's --dependency_out lists every file it wrote, e.g. protoc-gen-rust's mod.rs, which several edges write into one dir and so none can declare.
// cc runs from the tree root, as Soong's $relPwd does: a relative flag names a tree file (-fprofile-list=external/cronet/exclude_coverage.list), and `ninja -C out` would run it from the out dir.
// Its depfile then names that file tree-relative, which ninja resolves from the out dir: missing there, so the edge and everything downstream would rebuild on every run; rootDeps prefixes the root.
const header = `# Generated by bp2ninja; do not edit.
rule cc
  command = cd "$root" && $launcher $cc -MD -MF $out.d $flags -c $in -o $out && ` + rootDeps + `
  depfile = $out.d
  deps = gcc
  description = cc $in
rule ar
  command = rm -f $out && $ar crsD $out @$out.rsp
  rspfile = $out.rsp
  rspfile_content = $in
  description = ar $out
rule link
  command = $cxx $mode -o $out $crtbegin @$out.rsp $libs $crtend -Wl,--trace >$out.trace
  rspfile = $out.rsp
  rspfile_content = $in
  restat = 1
  description = link $out
rule toc
  command = CLANG_BIN=$clangbin $root/` + tocScript + ` --elf -i $in -o $out -d $out.d
  depfile = $out.d
  deps = gcc
  restat = 1
  description = toc $in
rule partial_ld
  command = $cxx -fuse-ld=lld -nostdlib -no-pie -Wl,-r $in -o $out $mode
  description = ld -r $out
rule no_addrsig
  command = rm -f $out && $objcopy --remove-section=.llvm_addrsig $in $out
  description = no addrsig $out
rule prefix_symbols
  command = $objcopy --prefix-symbols=$prefix $in $out
  description = prefix symbols $out
rule aidl
  command = $aidl --lang=$lang $flags --ninja -d $dep -h $hdir -o $sdir $in
  depfile = $dep
  deps = gcc
  description = aidl $in
rule aidl_rust
  command = $aidl --lang=rust $flags --ninja -d $dep -o $outdir $in
  depfile = $dep
  deps = gcc
  description = aidl rust $in
rule protoc
  command = $aprotoc --cpp_out=$params:$outdir --dependency_out=$dep -I $base $flags $in
  depfile = $dep
  deps = gcc
  description = protoc $in
rule protoc_rust
  command = cd "$root" && $aprotoc --rust_out=:$outdir --dependency_out=$dep -I $base $flags --plugin=protoc-gen-rust=$plugin $incs $in
  depfile = $dep
  deps = gcc
  description = protoc rust $in
rule sysprop
  command = $sysprop_cpp --header-dir=$hdir --public-header-dir=$pdir --source-dir=$sdir --include-name=$incname $in
  description = sysprop $in
rule yacc
  command = $bison -d --defines=$hdr -o $src $in
  description = bison $in
rule lex
  command = M4=$m4 $flex -o$out $in
  description = flex $in
rule yasm
  command = $yasm $flags -o $out $in && $yasm $flags -M $in >$out.d
  depfile = $out.d
  deps = gcc
  description = yasm $in
rule genrule
  command = cd "$root" && PYTHONDONTWRITEBYTECODE=1 /bin/bash $script
  description = genrule $name
  restat = 1
rule rustc
  command = $env $rustc $flags --emit link -o $out --emit dep-info=$out.d.raw $in $libs && line=$$(grep ^$out: $out.d.raw) && printf '%s ' "$$line" $crates > $out.d
  depfile = $out.d
  deps = gcc
  description = rustc $in
rule bindgen
  command = CLANG_PATH=$clang LIBCLANG_PATH=$libclang RUSTFMT=$rustfmt $bindgen $flags $$(cat /dev/null $flagfiles) $in -o $out -- -MD -MF $out.d $cflags
  depfile = $out.d
  deps = gcc
  description = bindgen $in
rule cp
  command = cp $in $out
  description = cp $out
rule fail
  command = echo $msg >&2 && exit 1
  description = fail $out
`

// rootDeps: makes the tree-relative entries of a depfile written from "$root" absolute, as ninja (run with -C out) resolves relative ones from the out dir.
// Per whitespace-separated token, since clang packs several short paths on one line; a token starting with `/` or `\` (absolute, line continuation) stays. Paths with escaped spaces are not handled (none in the tree).
const rootDeps = `sed -i -E "s#(^|[[:space:]])([^/[:space:]\\][^[:space:]]*)#\1$root/\2#g" $out.d`

// tocScript: Soong's table-of-contents extractor (cc/builder.go tocPath): the soname and dynamic symbol table via $CLANG_BIN/llvm-readelf, the file
// replaced only when it differs, so the `restat` toc edge stops dependents' relinks while those stay equal.
const tocScript = "build/soong/scripts/toc.sh"

// commitIfChanged: a command that wrote $out.tmp keeps the old $out (and its mtime) when every byte is equal (cmp), else moves the new one in. In
// a `restat = 1` rule ninja then skips the dependents of an unchanged output (Soong: java/builder.go javac and turbine, java/sdk.go
// commitChangeForRestat). Exact: any differing byte replaces the file.
const commitIfChanged = `(if cmp -s $out.tmp $out; then rm $out.tmp; else mv $out.tmp $out; fi)`

// nodeOut: the file a node builds ("" for kinds without one: generated sources, objects). Fixed by the node's module, side, kind and
// properties, so Java edges may name a host tool before emit (javagen.go jhost).
func (g *Graph) nodeOut(n *Node) string {
	if n.rust != nil {
		return g.rustOut(n)
	}
	base := filepath.Join(g.out, n.side.String(), n.name)
	switch n.kind {
	case Static:
		return filepath.Join(base, n.name+".a")
	case Shared:
		return filepath.Join(base, g.outName(n)+".so")
	case Binary:
		return filepath.Join(base, g.outName(n))
	}
	return ""
}

// emit writes the ninja file for every node.
func (g *Graph) emit(tools Tools) string {
	var b strings.Builder
	// rustfmt: bindgen.go's ${config.RustBin}/rustfmt, the toolchain's own beside rustc
	fmt.Fprintf(&b, "%sar = %s\ncxx = %s\nbison = %s\nflex = %s\nm4 = %s\nyasm = %s\nrustc = %s\nroot = %s\nclang = %s\nlibclang = %s\nrustfmt = %s\nlauncher = %s\nobjcopy = %s\nclangbin = %s\n\n", header, esc(tools.AR), esc(tools.CXX), esc(tools.Bison),
		esc(tools.Flex), esc(tools.M4), esc(tools.Yasm), esc(tools.Rustc), esc(g.tree), esc(tools.CC), esc(tools.LibClang), esc(filepath.Join(filepath.Dir(tools.Rustc), "rustfmt")), esc(tools.Launcher), esc(filepath.Join(tools.LLVM, "bin", "llvm-objcopy")),
		esc(filepath.Join(tools.LLVM, "bin")))
	// before the cc edges: ninja needs a rule defined before its first use
	b.WriteString(g.acfg.ninja())
	b.WriteString(g.aidlPreNinja())
	// paths first: links need their dependencies' outputs
	for _, n := range g.order {
		if out := g.nodeOut(n); out != "" {
			n.out = out
			// its link edge below also writes the toc; set before any dependent's link names it (linkDep)
			if n.kind == Shared && n.rust == nil && !n.provided {
				n.toc = out + ".toc"
			}
		}
	}
	// genrules: tools' paths are known now (a Rust source variant's steps are its edges)
	for _, n := range g.order {
		if n.kind == Gen && n.rust == nil {
			g.emitGen(&b, n)
		}
	}
	// a cc_library used as static and as shared is two nodes of one module: its generator outputs are named by module, so both write the same edges
	emitted := map[string]bool{}
	for _, n := range g.order {
		for _, e := range n.edges {
			if !emitted[e.outs[0]] {
				emitted[e.outs[0]] = true
				g.emitEdge(&b, e)
			}
		}
	}
	for _, n := range g.order {
		if n.rust != nil {
			g.emitRust(&b, n, tools)
			continue
		}
		if n.kind == Headers || n.kind == Gen {
			continue
		}
		flags := g.compileFlags(n)
		var waits []string
		genClosure(n, map[*Node]bool{}, &waits)
		for _, s := range n.srcs {
			src := s
			if !filepath.IsAbs(src) {
				src = g.abs(s)
			}
			rel := strings.TrimPrefix(strings.TrimPrefix(src, g.tree+"/"), g.out+"/")
			obj := filepath.Join(g.out, "obj", n.side.String(), n.name, n.kind.String(), rel+".o")
			lang := flags.cxx
			cc := tools.CXX
			switch filepath.Ext(s) {
			case ".c":
				lang, cc = flags.c, tools.CC
			case ".S", ".s":
				lang, cc = flags.asm, tools.CC
			case ".asm":
				if why := g.soong.Flags.NoToolchain[n.side]; why != "" {
					g.problem("%s: source %s: %s", n.mod.Name, s, why)
				} else {
					fmt.Fprintf(&b, "build %s: yasm %s || %s\n  flags = %s\n", esc(obj), esc(src), join(waits), shellQuote(flags.yasm))
					n.objs = append(n.objs, obj)
				}
				continue
			}
			fmt.Fprintf(&b, "build %s: cc %s || %s\n  cc = %s\n  flags = %s\n", esc(obj), esc(src), join(waits), esc(cc), shellQuote(lang))
			if debugInfo(lang) {
				b.WriteString("  launcher =\n")
			}
			n.objs = append(n.objs, obj)
		}
		switch n.kind {
		case Static:
			if g.baseType(n.mod.Type) == "cc_object" {
				fmt.Fprintf(&b, "build %s: ar %s\n", esc(n.out), join(g.objectInputs(n)))
				g.emitObject(&b, n)
			} else {
				fmt.Fprintf(&b, "build %s: ar %s\n", esc(n.out), join(n.objs))
			}
		case Shared, Binary:
			n.rustlib, n.rustlibWhole = g.rustStaticlib(&b, n)
			libs, inputs := g.linkLibs(n)
			mode := g.linkFlags(n)
			if n.versionScript != "" {
				inputs = append(inputs, n.versionScript)
			}
			begin, end := g.crtOuts(n.crtBegin), g.crtOuts(n.crtEnd)
			inputs = append(inputs, begin...)
			inputs = append(inputs, end...)
			// prefix_symbols: the link writes the unprefixed output, objcopy the module's (bionic.go)
			linkOut := n.out
			prefix := n.props.str("prefix_symbols")
			if prefix != "" && n.kind == Binary {
				linkOut = filepath.Join(filepath.Dir(n.out), "unprefixed", filepath.Base(n.out))
			}
			fmt.Fprintf(&b, "build %s: link %s | %s\n  mode = %s\n  libs = %s\n", esc(linkOut), join(n.objs), join(inputs), shellQuote(mode), shellQuote(libs))
			if strings.HasPrefix(linkOut, filepath.Join(g.out, "device")+"/") {
				g.deviceLinks = append(g.deviceLinks, linkOut)
			}
			if len(begin)+len(end) > 0 {
				fmt.Fprintf(&b, "  crtbegin = %s\n  crtend = %s\n", join(begin), join(end))
			}
			if n.toc != "" {
				// cc/builder.go TransformSharedObjectToToc: upstream toc.sh as is, --elf (Linux and Android), the tool its dependency
				fmt.Fprintf(&b, "build %s: toc %s | %s\n", esc(n.toc), esc(n.out), esc(g.abs(tocScript)))
			}
			if linkOut != n.out {
				fmt.Fprintf(&b, "build %s: prefix_symbols %s\n  prefix = %s\n", esc(n.out), esc(linkOut), shellQuote([]string{prefix}))
			}
		}
	}
	var defaults []string
	for _, n := range g.order {
		if n.out != "" {
			defaults = append(defaults, n.out)
		}
	}
	// none with only Java goals; ninja rejects an empty default statement
	if len(defaults) > 0 {
		fmt.Fprintf(&b, "\ndefault %s\n", join(defaults))
	}
	return b.String()
}

// debugInfo: the compile may emit DWARF (any -g* but -g0 / -gno-*; a non-debug -g* such as -gz only costs its cache hit), so it runs without the launcher.
// sccache 0.10's preprocessor mode keys on neither the cwd nor source paths, and DWARF records both: a hit would carry the out dir (or AOSP tree) of the compile that filled the entry -- measured on libaidl-common / libhidl-gen (module cflags -g, 77 of 6512 compiles).
func debugInfo(flags []string) bool {
	return slices.ContainsFunc(flags, func(f string) bool {
		return strings.HasPrefix(f, "-g") && f != "-g0" && !strings.HasPrefix(f, "-gno-")
	})
}

type langFlags struct{ c, cxx, asm, yasm []string }

// compileFlags: Soong's command lines (cc/builder.go) -- its global and toolchain flags (GlobalFlags, evaluated from its source), the module's flags, include paths in Soong's order (local, global, own exports, dependencies' exports, Soong's global includes), then the no-override flags.
// The global flags precede the module's, so a module's own -fexceptions, -fno-function-sections or -mno-* wins, as in Soong.
// yasm is the whole argv of a .asm compile; the others go to the cc rule.
// -w -Wno-error: distro clang warns differently from AOSP's clang; errors are what this build measures (keepFlag drops Soong's warning flags for the same reason).
// libc++: AOSP's C++ assumes it (libstdc++ 14 rejects libbase's non-const set comparator).
// bionicTargetFlags: what Soong's device compile adds for a bionic side (cc/compiler.go): AddTargetFlags' `-target <ClangTriple><FutureApiLevelInt>` (clang then defines __ANDROID__ and __ANDROID_API__ itself), -nostdlibinc (bionic's headers come from libc's exports), and -D__ANDROID_NATIVE_BRIDGE__ for a native bridge target.
func (g *Graph) bionicTargetFlags(s Side) []string {
	level := strings.TrimPrefix(g.soong.APILevel, apiLevelPrefix)
	out := []string{"-target", g.soong.Flags.Triple[s] + level, "-nostdlibinc", "-D__ANDROID_NATIVE_BRIDGE__"}
	if g.rtInclude != "" {
		out = append(out, "-isystem", g.rtInclude)
	}
	return out
}

func (g *Graph) compileFlags(n *Node) langFlags {
	p := n.props
	f := &g.soong.Flags
	global := append([]string{"-fPIC", "-w"}, f.common(n.side, n.mod.Dir)...)
	if !n.side.glibc() {
		global = append(global, g.bionicTargetFlags(n.side)...)
	}
	// .S/.s: global common + asflags, none of the module's cflags (cc/builder.go transformSourceToObj)
	asm := slices.Concat(global, f.As[n.side], []string{"-Wno-error"}, moduleFlags(p, "asflags"))
	common := slices.Concat(moduleFlags(p, "cflags"), n.genCflags, []string{"-Wno-error"})
	var inc []string
	for _, d := range p.strs("local_include_dirs") {
		inc = append(inc, "-I"+g.abs(filepath.Join(n.mod.Dir, d)))
	}
	// a Rust bindgen module (rustsrc.go) takes only local_include_dirs of its own: its include_dirs / export_include_dirs are its crate's exports, its dir no include path (bindgen.go)
	cc := n.rust == nil
	if cc {
		for _, d := range p.strs("include_dirs") {
			// a tree path, not a module reference: an absent one is usually an unfetched project (tflite -> external/gemmlowp)
			if _, err := statPath(g.abs(d)); err != nil {
				g.problem("%s: include dir %s does not exist", n.mod.Name, d)
				g.missingPaths = addKey(g.missingPaths, filepath.Clean(d))
			}
			inc = append(inc, "-I"+g.abs(d))
		}
		inc = append(inc, "-I"+g.abs(n.mod.Dir))
	}
	// .asm: the toolchain's format flags, the module's asflags and its own include dirs only (cc/compiler.go Local.YasmFlags): no defines, no dependencies' exports
	yasm := slices.Concat(f.Yasm[n.side], moduleFlags(p, "asflags"), inc)
	if cc {
		inc = append(inc, n.exports...)
	}
	inc = append(inc, n.genIncs...)
	for _, d := range append(append(append(append([]*Node{}, n.deps.headers...), n.deps.static...), n.deps.shared...), n.deps.whole...) {
		inc = append(inc, d.exports...)
	}
	for _, d := range g.soong.GlobalIncludes {
		inc = append(inc, "-I"+g.abs(d))
	}
	inc = dedup(inc)
	cstd, cppstd := g.stdVersions(p)
	// Soong's -std heads the language's local flags, after the module's cflags (cc/builder.go: localCFlags, then localConlyFlags / localCppFlags), so it wins over a -std in cflags (camera_metadata's -std=c11 builds as gnu23, which declares dprintf)
	cxx := slices.Concat(g.sideStlCompileFlags(n), global, f.Cpp[n.side], common, []string{"-std=" + cppstd}, moduleFlags(p, "cppflags"))
	c := slices.Concat(global, f.Conly[n.side], common, []string{"-std=" + cstd}, moduleFlags(p, "conlyflags"))
	// android properties compiled against glibc: bionic's macros and declarations supplied (LFS64, compat dirs, forced api-level headers); the native bridge side compiles against bionic itself
	if n.side == Device {
		dev := []string{androidDefine, lfs64Define, defaultSourceDefine, g.soong.APILevel, "-I" + filepath.Join(g.out, "compat", "device")}
		for _, d := range g.defines[n.mod.Name] {
			dev = append(dev, "-D"+d)
		}
		for _, h := range deviceForced {
			dev = append(dev, "-include", h)
		}
		cxx = append(append(cxx, dev...), "-I"+filepath.Join(g.out, "compat", "cxx"), "-idirafter", filepath.Join(g.out, "compat", "libcxx"))
		c = append(c, dev...)
	}
	if !p.boolean("rtti", false) {
		cxx = append(cxx, "-fno-rtti")
	}
	tail := f.noOverride(n.mod.Dir)
	return langFlags{slices.Concat(c, inc, tail), slices.Concat(cxx, inc, tail), append(asm, inc...), yasm}
}

// probeCxxFlags: the C++ argv compileFlags gives a device module in dir that has nothing of its own (no properties, sources or dependencies): what every device C++ compile there shares -- Soong's global and toolchain flags, the default -std, the device defines and forced headers, the compat dirs, the no-override flags.
// xtask check step libcxx compiles its probe with exactly this (-device-cxx-flags), so it measures the configuration the build uses.
func (g *Graph) probeCxxFlags(dir string) []string {
	n := &Node{key: key{name: "probe", side: Device, kind: Static}, mod: &Module{Name: "probe", Dir: dir}}
	return g.compileFlags(n).cxx
}

// stdVersions: the C and C++ -std values Soong gives a module (cc/compiler.go parseCStd, parseCppStd, maybeReplaceGnuToC): c_std / cpp_std, "" = the default, "experimental" = the experimental version; gnu_extensions: false turns gnu into c.
func (g *Graph) stdVersions(p *Obj) (string, string) {
	pick := func(v, def, exp string) string {
		switch v {
		case "":
			return def
		case "experimental":
			return exp
		}
		return v
	}
	cstd := pick(p.str("c_std"), g.soong.CStd, g.soong.ExperimentalCStd)
	cppstd := pick(p.str("cpp_std"), g.soong.CppStd, g.soong.ExperimentalCppStd)
	if !p.boolean("gnu_extensions", true) {
		r := strings.NewReplacer("gnu", "c")
		cstd, cppstd = r.Replace(cstd), r.Replace(cppstd)
	}
	return cstd, cppstd
}

// moduleFlags: a module's flag property as the argv Soong's command line gives clang.
// Soong escapes each entry with proptools.NinjaAndShellEscapeList (cc/compiler.go), whose ShellEscape leaves an entry of shell-safe characters and spaces unquoted, so the shell splits it into words (`-include AndroidFixup.h` is two arguments; cc/check.go validates exactly that form); an entry with any other shell-unsafe character is single-quoted whole and stays one argument.
// shellQuote then re-quotes our argv for the ninja file.
func moduleFlags(p *Obj, prop string) []string {
	var out []string
	for _, f := range p.strs(prop) {
		if strings.IndexFunc(f, func(r rune) bool { return r != ' ' && soongShellUnsafe(r) }) == -1 {
			out = append(out, strings.Fields(f)...)
		} else {
			out = append(out, f)
		}
	}
	return out
}

// soongShellUnsafe: blueprint proptools.shellUnsafeChar; safe = letters, digits and `_ + - = . , /`.
func soongShellUnsafe(r rune) bool {
	switch {
	case 'A' <= r && r <= 'Z', 'a' <= r && r <= 'z', '0' <= r && r <= '9':
		return false
	}
	return !strings.ContainsRune("_+-=.,/", r)
}

// outName: a shared library's or binary's file name, Soong's stem + suffix (cc/library.go getLibNameHelper, cc/binary.go getStem: stem, else the module name, then suffix; both after target / multilib blocks, so dalvikvm's lib64 suffix gives dalvikvm64).
// The soname follows it: libartpalette_fake builds as libartpalette-system.so, the name libartpalette dlopens.
func (g *Graph) outName(n *Node) string {
	stem := n.props.str("stem")
	if stem == "" {
		stem = n.mod.Name
	}
	return stem + n.props.str("suffix")
}

// linkFlags: a shared library's or binary's link flags ahead of its inputs, in Soong's order: the kind's (cc/library.go sharedLinkerFlags, cc/binary.go linkerFlags) and the toolchain's global ones (Flags.Global.LdFlags: DeviceLink on the device side, Lldflags, --no-undefined from CommonLinkerFlags), then the module's own (Flags.Local.LdFlags, cc/linker.go linkerFlags: its ldflags, after target / arch blocks, then version_script, which it makes a dependency of the link too).
// Not taken: linkerFlags' device-side --pack-dyn-relocs=android+relr. The loader is glibc's ld.so, not bionic's linker (measured, glibc 2.41 + lld 19): `android` packing moves relocations to DT_ANDROID_RELA, which glibc ignores -> pointers left unrelocated, segfault; `relr` -> "DT_RELR without GLIBC_ABI_DT_RELR dependency", a version need lld adds only for -z pack-relative-relocs. Unpacked relocations are what Soong's own pack_relocations: false gives (--pack-dyn-relocs=none): bigger, same behaviour.
func (g *Graph) linkFlags(n *Node) []string {
	var out []string
	out = append(out, g.soong.Flags.Link[n.side]...)
	if n.kind == Shared {
		out = append(out, "-shared", "-Wl,-soname,"+g.outName(n)+".so")
	}
	if n.side.android() {
		out = append(out, g.soong.DeviceLink.Ldflags...)
	}
	// a bionic side links for its triple without the driver's C library, crt objects or runtimes (Soong's device link: -nostdlib, crt and runtimes as dependencies), and takes the packing Soong gives a device link: the loader is bionic's linker, which reads both
	if !n.side.glibc() {
		out = append(out, g.bionicTarget(n.side)...)
		out = append(out, "-nostdlib", "-Wl,--pack-dyn-relocs=android+relr")
		if n.kind == Binary {
			out = append(out, g.bionicBinaryFlags(n)...)
		}
	}
	out = append(out, g.soong.Lldflags...)
	out = append(out, "-Wl,--no-undefined")
	out = append(out, moduleFlags(n.props, "ldflags")...)
	if n.versionScript != "" {
		out = append(out, "-Wl,--version-script,"+n.versionScript)
	}
	return out
}

// linkDep: what a link naming shared library d depends on: its table of contents where it has one (cc.go depsToPaths: depFile = TableOfContents),
// so a relink of d that keeps its dynamic symbols and soname stops at the restat toc edge instead of relinking every dependent; else d itself.
func (d *Node) linkDep() string {
	if d.toc != "" {
		return d.toc
	}
	return d.out
}

// linkLibs: provided shared objects, whole archives, the generated Rust staticlib, the static closure (provided archives included), shared libraries (also those static dependencies need), and the glibc system libraries bionic folds into libc.
// Provided shared objects come first, so they lead the output's DT_NEEDED and the process's lookup order: their strong definitions beat weak fallbacks elsewhere (libbase's and libcutils' __system_property_*); xtask's provided gate checks the result. Soong links system_shared_libs last (LateSharedLibs), where bionic's libc has no fallbacks to beat.
// An archive has no place in DT_NEEDED or the lookup order: its members are copied into the output, so it links in the group like any static library.
// Bionic sides link as Soong does (cc.go depsToPaths: orderStaticModuleDeps(directStaticDeps, directSharedDeps)): only the module's own static and shared dependencies, whose static_libs and shared_libs it lists itself
// (bionic's linker names libbase's liblog_for_runtime_apex; libbase's libc++_static would clash with the linker's libc++_static_noexcept). Rust crates still bring their cc libraries (Soong bundles them into the rlib).
func (g *Graph) linkLibs(n *Node) (libs, inputs []string) {
	closure := linkClosure(n)
	// a whole dependency's own whole_static_libs are objects of its archive in Soong (library.go), so they are whole-linked with it, recursively
	whole := map[*Node]bool{}
	var wholeOrder []*Node
	var addWhole func(list []*Node)
	addWhole = func(list []*Node) {
		for _, d := range list {
			if !whole[d] {
				whole[d] = true
				wholeOrder = append(wholeOrder, d)
				addWhole(d.deps.whole)
			}
		}
	}
	addWhole(n.deps.whole)
	sharedOrder := linkedShared(n, closure)
	// rlibs link through the generated staticlib (rust.go rustStaticlib)
	var wholeLibs []string
	for _, d := range wholeOrder {
		if d.rust == nil {
			wholeLibs = append(wholeLibs, d.out)
		}
	}
	if n.rustlibWhole {
		wholeLibs = append(wholeLibs, n.rustlib)
	}
	if len(wholeLibs) > 0 {
		libs = append(libs, "-Wl,--whole-archive")
		libs = append(libs, wholeLibs...)
		inputs = append(inputs, wholeLibs...)
		libs = append(libs, "-Wl,--no-whole-archive")
	}
	libs = append(libs, "-Wl,--start-group")
	if n.rustlib != "" && !n.rustlibWhole {
		libs = append(libs, n.rustlib)
		inputs = append(inputs, n.rustlib)
	}
	for _, d := range closure {
		if !whole[d] && d.kind == Static {
			libs = append(libs, d.out)
			inputs = append(inputs, d.out)
		}
	}
	libs = append(libs, "-Wl,--end-group")
	// Soong gives only host binaries an rpath (cc/linker.go linkerFlags: `ctx.Host() && !ctx.Windows() && !staticLib` -> RpathFlags, $ORIGIN-relative); device and native-bridge ones get none, the linker's search path (here LD_LIBRARY_PATH over the stage) finds their libraries.
	// A build-tree rpath on an android side would let a library missing from the stage load silently from the tree; xtask's runpath gate fails on one.
	// The converter links with lld, which needs no search path for a needed library's own needs; an outside ld.bfd link (Rust packages) gets the stage's dirs as -rpath-link (xtask packages.rs).
	addRpath := func(list []string, dir string) []string {
		if n.side.android() {
			return list
		}
		return appendNew(list, "-Wl,-rpath,"+dir)
	}
	var provided, providedRpath []string
	// several provided modules may share an artifact (one crate providing two rows: atl-bionic is libc, libasync_safe, libdl_android), linked once
	seenLib := map[string]bool{}
	for _, d := range sharedOrder {
		if d.provided && !seenLib[d.out] {
			seenLib[d.out] = true
			provided = append(provided, d.out)
			providedRpath = addRpath(providedRpath, filepath.Dir(d.out))
			inputs = append(inputs, d.out)
		}
	}
	libs = append(provided, libs...)
	for _, d := range sharedOrder {
		if d.kind == Shared && !d.provided {
			// host: absolute build-tree rpath, build tools (aidl) run from here
			libs = addRpath(append(libs, d.out), filepath.Dir(d.out))
			inputs = append(inputs, d.linkDep())
		}
	}
	// runtime_libs of n and of what it links whole or static (cc.go): where Soong installs them beside the module, the build tree has their own dirs; glibc's dlopen searches the caller's DT_RUNPATH, so libartpalette finds libartpalette-system
	runtime := slices.Clone(n.deps.runtime)
	for _, d := range closure {
		runtime = append(runtime, d.deps.runtime...)
	}
	for _, d := range runtime {
		if !d.provided {
			libs = addRpath(libs, filepath.Dir(d.out))
			continue
		}
		providedRpath = addRpath(providedRpath, filepath.Dir(d.out))
	}
	// provided dirs search after the tree's: a system provider's dir (/usr/lib/<triple>) holds distro builds
	// of libraries the tree also builds (libz, libselinux, libpng, ...); searched first, it shadows every
	// one of them at load time (libselinux: distro lacks AOSP's LIBSELINUX_R version node -> load fails)
	libs = append(libs, providedRpath...)
	// glibc sides: the C++ runtime the module's stl selects (stl.go; a bionic side links it as a dependency, bionicStlDeps), glibc's split of bionic's libc, and bionic's resolver API (b64_ntop -> __b64_ntop: libnetd_client) as glibc's libresolv, needed only where used
	if n.side.glibc() {
		libs = append(libs, stlLinkFlags(g.stlOf(n))...)
		libs = append(libs, systemLinkFlags...)
	}
	return libs, inputs
}

// linkClosure: the static archives n's final link takes (linkLibs): staticClosure on glibc sides, directStatic on bionic ones.
func linkClosure(n *Node) []*Node {
	var closure []*Node
	if n.side.glibc() {
		staticClosure(n, map[*Node]bool{}, &closure)
	} else {
		directStatic(n, &closure)
	}
	return closure
}

// linkedShared: the shared libraries n's link names, in linkLibs' DT_NEEDED order: its own shared dependencies, then on glibc sides those of its static closure; each once.
func linkedShared(n *Node, closure []*Node) []*Node {
	var out []*Node
	seen := map[*Node]bool{}
	add := func(list []*Node) {
		for _, d := range list {
			if !seen[d] {
				seen[d] = true
				out = append(out, d)
			}
		}
	}
	add(n.deps.shared)
	if n.side.glibc() {
		for _, d := range closure {
			add(d.deps.shared)
		}
	}
	return out
}

// systemLinkFlags: the glibc libraries every glibc-side link ends with (linkLibs, devicePlatform).
var systemLinkFlags = []string{"-lpthread", "-ldl", "-lrt", "-lm", "-Wl,--push-state,--as-needed", "-lresolv", "-Wl,--pop-state"}

// appendNew: list + s, unless list holds s already.
func appendNew(list []string, s string) []string {
	if slices.Contains(list, s) {
		return list
	}
	return append(list, s)
}
