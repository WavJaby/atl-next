// SPDX-License-Identifier: MIT

package main

import (
	"io/fs"
	"os"
	"path/filepath"
)

// walkTree is filepath.WalkDir (lexical order, fn before a directory's entries, SkipDir) that also enters a symlinked directory whose link text is absolute, reporting it as a directory under its link path.
// Absolute = leaves the tree: xtask's AOSP link farm (src-wt/<task>/aosp-tree) is real directories plus absolute links into src/aosp and the task copies; AOSP's own links and the manifest's linkfiles are relative and stay unfollowed.
// Each link target, and a real root, is entered once by real path, so a link cycle ends.
// Its own recursion, not WalkDir's: each directory fn lets it enter is listed (listDir) and recorded as a probe of kind (inputs.go): probeList when fn reads every entry, probeBpList when it reads only the blueprint walk's (discover); a skipped directory is only an entry of its listed parent. The root is lstat'ed through the recorder: a missing root that appears changes the answer.
// A listing error is handed to fn as WalkDir's second call does; SkipDir from a non-directory skips the rest of its parent; SkipAll is not used here.
func walkTree(root, kind string, fn fs.WalkDirFunc) error {
	info, err := lstatPath(root)
	if err != nil {
		return fn(root, nil, err)
	}
	entered := map[string]bool{}
	if info.IsDir() {
		if real, err := filepath.EvalSymlinks(root); err == nil {
			entered[real] = true
		}
	}
	if err := walkEntry(root, kind, fs.FileInfoToDirEntry(info), fn, entered); err != filepath.SkipDir {
		return err
	}
	return nil
}

// walkEntry visits path; SkipDir returned for a directory is consumed here, from a file it reaches the caller.
func walkEntry(path, kind string, d fs.DirEntry, fn fs.WalkDirFunc, entered map[string]bool) error {
	if d.Type()&fs.ModeSymlink != 0 {
		target, lerr := os.Readlink(path)
		info, serr := os.Stat(path)
		if lerr != nil || serr != nil || !filepath.IsAbs(target) || !info.IsDir() {
			return fn(path, d, nil)
		}
		real, rerr := filepath.EvalSymlinks(path)
		if rerr != nil || entered[real] {
			return nil
		}
		entered[real] = true
		d = fs.FileInfoToDirEntry(info)
	}
	if err := fn(path, d, nil); err != nil || !d.IsDir() {
		if err == filepath.SkipDir && d.IsDir() {
			return nil
		}
		return err
	}
	entries, err := listDir(path)
	recordListing(kind, path, entries, err)
	if err != nil {
		if err := fn(path, d, err); err != nil && err != filepath.SkipDir {
			return err
		}
		return nil
	}
	for _, e := range entries {
		if err := walkEntry(filepath.Join(path, e.Name()), kind, e, fn, entered); err != nil {
			if err == filepath.SkipDir {
				return nil
			}
			return err
		}
	}
	return nil
}
