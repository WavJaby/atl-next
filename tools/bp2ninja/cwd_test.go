// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
)

// cc runs from the tree root (cc/builder.go $relPwd): `ninja -C out` changes into the out dir, so a relative flag such as -fprofile-list=external/... names no file unless the compile cds first.
func TestCompilesRunFromTreeRoot(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
cc_library_static { name: "libp", srcs: ["s.cc"], cflags: ["-fprofile-list=external/x/exclude.list"] }
`})
	if g.node("libp", Device, Static, "goal") == nil {
		t.Fatal(g.problems)
	}
	ninja := g.emit(Tools{CC: "cc", CXX: "c++", AR: "ar"})
	if err := os.WriteFile(filepath.Join(g.out, "build.ninja"), []byte(ninja), 0o644); err != nil {
		t.Fatal(err)
	}
	objs := regexp.MustCompile(`(?m)^build (\S+\.o): cc `).FindAllStringSubmatch(ninja, -1)
	if len(objs) != 1 {
		t.Fatalf("%d cc edges, want 1", len(objs))
	}
	out, err := exec.Command("ninja", "-C", g.out, "-t", "commands", "-s", objs[0][1]).Output()
	if err != nil {
		t.Fatal(err)
	}
	if want := `cd "` + g.tree + `" && `; !strings.HasPrefix(string(out), want) {
		t.Errorf("compile command %q, want the tree root first: %q", out, want)
	}
}
