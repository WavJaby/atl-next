// SPDX-License-Identifier: MIT

package main

import (
	"io/fs"
	"os"
	"path/filepath"
)

// walkTree is filepath.WalkDir that also enters a symlinked directory whose link text is absolute, reporting it as a directory under its link path.
// Absolute = leaves the tree: xtask's AOSP link farm (src-wt/<task>/aosp-tree) is real directories plus absolute links into src/aosp and the task copies; AOSP's own links and the manifest's linkfiles are relative and stay unfollowed, as before.
// Each link target, and a real root, is entered once by real path, so a link cycle ends.
func walkTree(root string, fn fs.WalkDirFunc) error {
	entered := map[string]bool{}
	if info, err := os.Lstat(root); err == nil && info.IsDir() {
		if real, err := filepath.EvalSymlinks(root); err == nil {
			entered[real] = true
		}
	}
	var visit fs.WalkDirFunc
	visit = func(path string, d fs.DirEntry, err error) error {
		if err != nil || d.Type()&fs.ModeSymlink == 0 {
			return fn(path, d, err)
		}
		target, lerr := os.Readlink(path)
		info, serr := os.Stat(path)
		if lerr != nil || serr != nil || !filepath.IsAbs(target) || !info.IsDir() {
			return fn(path, d, err)
		}
		real, rerr := filepath.EvalSymlinks(path)
		if rerr != nil || entered[real] {
			return nil
		}
		entered[real] = true
		// a trailing separator makes WalkDir's Lstat resolve the link
		return filepath.WalkDir(path+string(filepath.Separator), func(p string, d fs.DirEntry, err error) error {
			return visit(filepath.Clean(p), d, err)
		})
	}
	return filepath.WalkDir(root, visit)
}
