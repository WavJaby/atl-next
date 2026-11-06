// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"slices"
	"strings"
)

// Soong's implicit link dependencies and link steps of a bionic side (the native bridge guest), which a glibc side's toolchain driver supplies itself:
//   - crt objects (cc/library.go linkerDeps, cc/binary.go linkerDeps; names cc/config/bionic.go): shared library crtbegin_so + crtend_so (+ crt_pad_segment unless no_crt_pad_segment),
//     static binary crtbegin_static + crtend_android, dynamic binary crtbegin_dynamic + crtend_android; none with nocrt. Linked before the inputs and after the libraries, as Soong's CrtBegin / CrtEnd.
//   - libclang_rt.builtins (cc/linker.go baseLinker.linkerDeps, Bionic): an unexported static dependency of every library and binary unless no_libcrt; cc_object has its own linkerDeps without it.
//   - cc_object objs (cc/object.go): combined with the module's own objects by ld -r (partialLd: clang++ -fuse-ld=lld -nostdlib -no-pie -Wl,-r), then .llvm_addrsig removed (noAddrSig); prefix_symbols applied by objcopy.
//   - static_executable (cc/binary.go linkerDeps / linkerFlags): system_shared_libs linked statically; -static unless ldflags has -shared (the dynamic linker: a shared object linked statically), -nostdlib -Bstatic -Wl,--gc-sections.
//   - prefix_symbols on a binary (cc/binary.go link, builder.go transformBinaryPrefixSymbols): the link output passes through llvm-objcopy --prefix-symbols.
// Kept to bionic sides: a glibc side's driver links its own crt and runtime, and its cc_object objs stay unmirrored there.

// builtinsModule: config.BuiltinsRuntimeLibrary() (cc/config/toolchain.go).
const builtinsModule = "libclang_rt.builtins"

// staticBinary: a bionic binary with static_executable (cc/binary.go static).
func (n *Node) staticBinary() bool {
	return n.kind == Binary && !n.side.glibc() && n.props.boolean("static_executable", false)
}

// bionicLinkDeps adds n's implicit bionic dependencies; dep resolves refs on n's side.
func (g *Graph) bionicLinkDeps(n *Node, dep func([]string, Kind) []*Node) {
	if g.baseType(n.mod.Type) == "cc_object" {
		n.objDeps = dep(n.props.strs("objs"), Static)
		return
	}
	if n.kind != Static && n.kind != Shared && n.kind != Binary {
		return
	}
	if !n.props.boolean("no_libcrt", false) {
		// a module naming it in static_libs links it once
		n.deps.static = uniqueNodes(append(n.deps.static, dep([]string{builtinsModule}, Static)...))
	}
	if n.props.boolean("nocrt", false) {
		return
	}
	var begin, end []string
	switch {
	case n.kind == Shared:
		begin, end = []string{"crtbegin_so"}, []string{"crtend_so"}
		if !n.props.boolean("no_crt_pad_segment", false) {
			end = append(end, "crt_pad_segment")
		}
	case n.staticBinary():
		begin, end = []string{"crtbegin_static"}, []string{"crtend_android"}
	case n.kind == Binary:
		begin, end = []string{"crtbegin_dynamic"}, []string{"crtend_android"}
	}
	n.crtBegin, n.crtEnd = dep(begin, Static), dep(end, Static)
}

// objectInputs: the object files a cc_object combines: its own, then those of the cc_objects its objs names.
func (g *Graph) objectInputs(n *Node) []string {
	in := slices.Clone(n.objs)
	for _, d := range n.objDeps {
		in = append(in, g.objectOut(d))
	}
	return in
}

// crtOuts: the crt objects' files, in order.
func (g *Graph) crtOuts(list []*Node) []string {
	var out []string
	for _, d := range list {
		out = append(out, g.objectOut(d))
	}
	return out
}

// bionicTarget: the -target a bionic side's link or partial link takes (cc/config: the toolchain's ClangTriple with the platform API level).
func (g *Graph) bionicTarget(s Side) []string {
	return []string{"-target", g.soong.Flags.Triple[s] + strings.TrimPrefix(g.soong.APILevel, apiLevelPrefix)}
}

// emitObject writes a cc_object's object file (cc/object.go link): one input is copied (or prefixed), several are combined by ld -r and stripped of .llvm_addrsig, then prefixed.
func (g *Graph) emitObject(b *strings.Builder, n *Node) {
	in := g.objectInputs(n)
	out := g.objectOut(n)
	prefix := n.props.str("prefix_symbols")
	switch {
	case len(in) == 1 && prefix != "":
		fmt.Fprintf(b, "build %s: prefix_symbols %s\n  prefix = %s\n", esc(out), esc(in[0]), shellQuote([]string{prefix}))
	case len(in) == 1:
		fmt.Fprintf(b, "build %s: cp %s\n", esc(out), esc(in[0]))
	case n.side.glibc():
		fmt.Fprintf(b, "build %s: fail\n  msg = %s\n", esc(out), shellQuote([]string{fmt.Sprintf("error: %s: cc_object output of %d objects (ld -r) is unsupported", n.mod.Name, len(in))}))
	default:
		dir, base := filepath.Dir(out), filepath.Base(out)
		addrsig := filepath.Join(dir, "addrsig", base)
		fmt.Fprintf(b, "build %s: partial_ld %s\n  mode = %s\n", esc(addrsig), join(in), shellQuote(g.bionicTarget(n.side)))
		stripped := out
		if prefix != "" {
			stripped = filepath.Join(dir, "unprefixed", base)
		}
		fmt.Fprintf(b, "build %s: no_addrsig %s\n", esc(stripped), esc(addrsig))
		if prefix != "" {
			fmt.Fprintf(b, "build %s: prefix_symbols %s\n  prefix = %s\n", esc(out), esc(stripped), shellQuote([]string{prefix}))
		}
	}
}

// bionicBinaryFlags: cc/binary.go linkerFlags for a bionic binary, after the toolchain's.
func (g *Graph) bionicBinaryFlags(n *Node) []string {
	if n.staticBinary() {
		var out []string
		if !slices.Contains(moduleFlags(n.props, "ldflags"), "-shared") {
			out = append(out, "-static")
		}
		return append(out, "-nostdlib", "-Bstatic", "-Wl,--gc-sections")
	}
	// Android: /system/bin/linker, linker64 for a 64-bit toolchain (flags.DynamicLinker, passed by builder.go as -Wl,-dynamic-linker)
	linker := "/system/bin/linker"
	if _, bits := g.cfg.archOf(n.side); bits == 64 {
		linker += "64"
	}
	return []string{"-Wl,-dynamic-linker," + linker, "-pie", "-nostdlib", "-Bdynamic", "-Wl,--gc-sections", "-Wl,-z,nocopyreloc"}
}
