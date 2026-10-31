// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"slices"
	"testing"
)

// A farm: real dir f/a, absolute link f/p -> outside x/ (which links back to f and to itself: cycles), relative link f/r -> a.
func TestWalkFollowsAbsoluteLinksOnce(t *testing.T) {
	dir := t.TempDir()
	farm, outside := filepath.Join(dir, "f"), filepath.Join(dir, "x")
	for _, d := range []string{filepath.Join(farm, "a"), filepath.Join(outside, "sub")} {
		if err := os.MkdirAll(d, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	for _, f := range []string{filepath.Join(farm, "a", "Android.bp"), filepath.Join(outside, "sub", "Android.bp")} {
		if err := os.WriteFile(f, nil, 0o644); err != nil {
			t.Fatal(err)
		}
	}
	links := [][2]string{{outside, filepath.Join(farm, "p")}, {"a", filepath.Join(farm, "r")}, {farm, filepath.Join(outside, "back")}, {outside, filepath.Join(outside, "sub", "loop")}}
	for _, l := range links {
		if err := os.Symlink(l[0], l[1]); err != nil {
			t.Fatal(err)
		}
	}
	var got []string
	err := walkTree(farm, func(path string, d os.DirEntry, err error) error {
		if err == nil && !d.IsDir() && d.Type()&os.ModeSymlink == 0 {
			rel, _ := filepath.Rel(farm, path)
			got = append(got, rel)
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	// p followed under its link path, r (relative) not, back -> farm (the root) not again
	want := []string{"a/Android.bp", "p/sub/Android.bp"}
	slices.Sort(got)
	if !slices.Equal(got, want) {
		t.Errorf("walked %q, want %q", got, want)
	}
	// a glob below an absolute link root: the root is a link, so back -> f is entered once
	if g := globTree(filepath.Join(farm, "p"), "**/*.bp", "p"); !slices.Equal(g, []string{"p/back/a/Android.bp", "p/sub/Android.bp"}) {
		t.Errorf("globTree through the link: %q", g)
	}
}
