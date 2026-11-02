// SPDX-License-Identifier: MIT

package main

import (
	"strings"
	"testing"
)

const aidlCratesTree = `
filegroup {
    name: "fg_aidl",
    srcs: ["aidl/a/IFoo.aidl"],
    path: "aidl",
}
`

// An aidl crate becomes an aidl_interface whose -rust backend synthAidl builds from the filegroup's files under its path; an unknown filegroup, a
// missing path, a name clash and an empty list are refused.
func TestAidlCrates(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": aidlCratesTree, "p/aidl/a/IFoo.aidl": "package a; interface IFoo {}"})
	if err := g.addAidlCrates(map[string][]string{"pkg-aidl": {":fg_aidl"}}); err != nil {
		t.Fatal(err)
	}
	m := g.synth("pkg-aidl-rust", Device)
	if m == nil || m.aidl == nil || m.aidl.lang != "rust" {
		t.Fatalf("pkg-aidl-rust: %+v, problems %v", m, g.problems)
	}
	if len(m.aidl.srcs) != 1 || m.aidl.srcs[0].path != "p/aidl/a/IFoo.aidl" || m.aidl.srcs[0].root != "p/aidl" {
		t.Errorf("srcs %+v, want p/aidl/a/IFoo.aidl under root p/aidl (the filegroup path)", m.aidl.srcs)
	}
	for _, c := range []struct {
		crates map[string][]string
		want   string
	}{
		{map[string][]string{"x": {":no_such_fg"}}, ":no_such_fg names no module"},
		{map[string][]string{"x": {"p/aidl/a/Missing.aidl"}}, "Missing.aidl"},
		{map[string][]string{"fg_aidl": {":fg_aidl"}}, "already has a module"},
		{map[string][]string{"x": {}}, "no srcs"},
	} {
		err := g.addAidlCrates(c.crates)
		if err == nil || !strings.Contains(err.Error(), c.want) {
			t.Errorf("%v: error %v, want %q", c.crates, err, c.want)
		}
	}
}
