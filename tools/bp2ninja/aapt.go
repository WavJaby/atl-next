// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
)

// Resource file helpers of the android modules (app.go): aapt2.go's AAPT2_SHARD_SIZE and pathToAapt2Path, android_resources.go's
// androidResourceGlob. framework-res is an android_app like any other: its apk and `{.aapt.srcjar}` come from app.go.

// aapt2ShardSize: aapt2.go AAPT2_SHARD_SIZE.
const aapt2ShardSize = 100

// androidResourceIgnore: android_resources.go androidResourceIgnoreFilenames, as globs of a base name.
var androidResourceIgnore = []string{".svn", ".git", ".ds_store", "*.scc", ".*", "CVS", "thumbs.db", "picasa.ini", "*~"}

var flagsPathRe = regexp.MustCompile(`flag\(!?([a-zA-Z_-]+\.)*[a-zA-Z0-9_-]+\)`)

// aapt2Path is pathToAapt2Path below the compile output root: values-*/<f>.xml -> values-*_<f>.arsc.flat, flag(x)/<dir>/<f> ->
// <dir>_<f>.(x).<ext>.flat, else <dir>_<f>.flat, all under the source's directory above <dir>.
func aapt2Path(res string) string {
	ext := filepath.Ext(res)
	name := strings.TrimSuffix(filepath.Base(res), ext)
	sub, last := filepath.Split(filepath.Dir(res))
	if strings.HasPrefix(last, "values") {
		ext = ".arsc"
	}
	if flagsPathRe.MatchString(filepath.Base(sub)) {
		var flag string
		sub, flag = filepath.Split(filepath.Dir(sub))
		flag = strings.TrimPrefix(flag, "flag")
		return filepath.Join(sub, fmt.Sprintf("%s_%s.%s%s.flat", last, name, flag, ext))
	}
	return filepath.Join(sub, fmt.Sprintf("%s_%s%s.flat", last, name, ext))
}

func ignoredResource(base string) bool {
	for _, p := range androidResourceIgnore {
		if ok, _ := filepath.Match(p, strings.ToLower(base)); ok {
			return true
		}
	}
	return false
}

// resourceGlob: androidResourceGlob, tree-relative and sorted.
func (g *Graph) resourceGlob(dir string) []string {
	var out []string
	for _, f := range globTree(g.abs(dir), "**/*", dir) {
		skip := false
		for _, part := range strings.Split(strings.TrimPrefix(f, dir+"/"), "/") {
			if ignoredResource(part) {
				skip = true
			}
		}
		if !skip {
			out = append(out, f)
		}
	}
	slices.Sort(out)
	return out
}

// globDirExists: whether dir is an existing directory of the tree.
func globDirExists(g *Graph, dir string) (bool, error) {
	if st, err := statPath(g.abs(dir)); err != nil || !st.IsDir() {
		return false, fmt.Errorf("%s: not a directory", dir)
	}
	return true, nil
}
