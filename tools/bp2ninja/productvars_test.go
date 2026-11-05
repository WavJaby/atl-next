// SPDX-License-Identifier: MIT

package main

import (
	"slices"
	"strings"
	"testing"
)

// A set variable's block is appended after the arch blocks with its value filled in; an unset one (debuggable) is skipped, as in Soong's VariableMutator.
func TestProductVariables(t *testing.T) {
	const sdk = "36" // the release fixture's value; the filled flag must carry exactly it
	g := graphFor(t, map[string]string{"Android.bp": `
cc_library {
    name: "libpv",
    cflags: ["-DBASE"],
    arch: { x86_64: { cflags: ["-DARCH"] } },
    product_variables: {
        platform_sdk_version: { cflags: ["-DAPI=%d"], asflags: ["-DAPI=%d"] },
        debuggable: { cflags: ["-DDEBUGGABLE"] },
    },
}
cc_library {
    name: "libbad",
    product_variables: { platform_sdk_version: { cflags: ["-DA=%d-%d"] } },
}
`})
	g.release = &Release{Name: "test", flags: map[string]string{"RELEASE_PLATFORM_SDK_VERSION": sdk}}
	p := g.flatten(g.mods[Device]["libpv"][0], Device, Shared)
	if got, want := p.strs("cflags"), []string{"-DBASE", "-DARCH", "-DAPI=" + sdk}; !slices.Equal(got, want) {
		t.Errorf("cflags = %q, want %q", got, want)
	}
	if got, want := p.strs("asflags"), []string{"-DAPI=" + sdk}; !slices.Equal(got, want) {
		t.Errorf("asflags = %q, want %q", got, want)
	}
	if p.has("product_variables") {
		t.Error("product_variables left in the flattened properties")
	}
	if len(g.problems) != 0 {
		t.Fatalf("problems: %q", g.problems)
	}
	g.flatten(g.mods[Device]["libbad"][0], Device, Shared)
	if len(g.problems) != 1 || !strings.Contains(g.problems[0], "single '%'") {
		t.Errorf("two %%: problems = %q", g.problems)
	}
	// without a release no variable is set
	g.release, g.problems = nil, nil
	if got := g.flatten(g.mods[Device]["libpv"][0], Device, Shared).strs("cflags"); !slices.Equal(got, []string{"-DBASE", "-DARCH"}) {
		t.Errorf("no release: cflags = %q", got)
	}
}
