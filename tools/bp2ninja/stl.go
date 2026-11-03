// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"os/exec"
	"path/filepath"
	"strings"
)

// C++ standard library selection, after build/soong/cc/stl.go (begin, flags, deps), for this toolchain.
//   - Selection: a module's stl property (through defaults, as every property), else libc++ ("libc++_static" for a static library, as
//     Soong's unset case). "none" selects no library. Header-only modules compile nothing here, so Soong's header default is not read.
//   - Flags: Soong's non-Bionic branch on both sides, since the C++ compiler is the distro clang, not a Bionic toolchain. A C++ compile gets
//     -nostdinc++ and, unless "none", the libc++ headers as -isystem; every link gets -nostdlib++ and, unless "none", -lc++ (Soong's
//     SharedLibs libc++). "none" modules bring their own runtime headers (cronet's cc_defaults use stl "none" and build a bundled libc++ through
//     include_dirs); the provider's headers would mix with theirs (a `string.h` include_next lands in the provider's wrapper).
//   - Deviations: the ndk_* selections of sdk_version modules are the non-SDK ones (the NDK's STL and headers are not built here, and one C++
//     runtime is compiled against). libc++_static links the shared -lc++ (no static archive model); its compile flags are libc++'s, as in Soong.
//     Device modules take the non-Bionic branch: Soong's Bionic branch adds nothing because its toolchain's default libc++ is the provider here.

// selectSTL: Soong's SelectedStl (cc/stl.go begin) for a non-Windows module whose stl property is set (set) to prop; static: a static library.
func selectSTL(prop string, set, static bool) (string, error) {
	s := ""
	if set {
		s = prop
	}
	if s == "none" {
		return "", nil
	}
	switch s {
	case "c++_shared":
		s = "libc++"
	case "c++_static":
		s = "libc++_static"
	}
	switch s {
	case "libc++", "libc++_static":
		return s, nil
	case "", "system":
		if static {
			return "libc++_static", nil
		}
		return "libc++", nil
	}
	return "", fmt.Errorf("stl: %q is not a supported STL", s)
}

// stlOf: n's selected C++ standard library; a bad stl value is a problem and selects none.
func (g *Graph) stlOf(n *Node) string {
	sel, err := selectSTL(n.props.str("stl"), n.props.has("stl"), n.kind == Static)
	if err != nil {
		g.problem("%s: %v", n.name, err)
		return ""
	}
	return sel
}

// stlCompileFlags: the C++ compile flags of selection sel (cc/stl.go flags, non-Bionic). The provider's headers are an -isystem
// of their own: they follow the module's -I dirs, as Soong's dependency exports do.
func (g *Graph) stlCompileFlags(sel string) []string {
	if sel == "" {
		return []string{"-nostdinc++"}
	}
	if g.libcxx == "" {
		g.problem("C++ standard library %s: no libc++ headers derived (libcxxProvider)", sel)
		return []string{"-nostdinc++"}
	}
	return []string{"-nostdinc++", "-isystem", g.libcxx}
}

// stlLinkFlags: the link flags of selection sel (cc/stl.go flags, non-Bionic): -nostdlib++ always, -lc++ for libc++.
func stlLinkFlags(sel string) []string {
	if sel == "" {
		return []string{"-nostdlib++"}
	}
	return []string{"-nostdlib++", "-lc++"}
}

// libcxxProvider: the libc++ headers the compiler's own -stdlib=libc++ searches, the first directory named c++/v1 in its search list.
// Derived from the compiler the build uses, so the headers are the ones its libc++ is installed with. The probe is a query (no output
// kept, no build), the only place -stdlib=libc++ remains.
func libcxxProvider(cxx string) (string, error) {
	out, err := exec.Command(cxx, "-stdlib=libc++", "-x", "c++", "-E", "-v", "/dev/null").CombinedOutput()
	if err != nil {
		return "", fmt.Errorf("probing libc++ headers with %s: %v: %s", cxx, err, out)
	}
	if dir, ok := parseLibcxxSearch(string(out)); ok {
		return dir, nil
	}
	return "", fmt.Errorf("%s -stdlib=libc++ searches no c++/v1 directory", cxx)
}

// parseLibcxxSearch: the first directory of the `#include <...>` search list in clang's -v output that is named c++/v1.
func parseLibcxxSearch(text string) (string, bool) {
	in := false
	for _, line := range strings.Split(text, "\n") {
		switch {
		case strings.HasPrefix(line, "#include <...> search starts here:"):
			in = true
		case strings.HasPrefix(line, "End of search list."):
			return "", false
		case in:
			dir := filepath.Clean(strings.TrimSpace(line))
			if filepath.Base(dir) == "v1" && filepath.Base(filepath.Dir(dir)) == "c++" {
				return dir, true
			}
		}
	}
	return "", false
}
