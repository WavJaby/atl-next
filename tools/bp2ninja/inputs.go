// SPDX-License-Identifier: MIT

package main

import (
	"go/ast"
	"go/parser"
	"go/token"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
)

// Every path whose state the conversion depends on, for the depfile of build.ninja's regeneration edge (regen.go): ninja reruns the converter exactly when one changed.
// What counts, by how the converter looked: a file read -> the file (its content); a directory listed (walk, ReadDir, glob) -> the directory (an entry added or removed changes its mtime);
// a path only stat'ed or resolved, or read but missing -> its parent directory (existence and type change with the parent's entries, a content edit does not reach the output);
// a missing directory -> its nearest existing ancestor (a missing depfile entry would make ninja rerun on every run).
// A directory of the build dir (-out) never counts: its entries are build outputs, the conversion's own among them (build.ninja.d, compat/), which change after every conversion -> regeneration forever (measured).
// A file there that the conversion reads does (xtask's argument file and JSON inputs).
// Files the converter writes (compat/, generated files) and reads back only to write if changed are not inputs: those calls use os directly.
var inputs = map[string]bool{}

// buildDir: -out, absolute; set before the conversion reads anything (main).
var buildDir string

func record(path string) {
	if abs, err := filepath.Abs(path); err == nil {
		inputs[abs] = true
	}
}

// recordDir: a directory listed, or holding a path looked at, unless it is the build dir's.
func recordDir(abs string) {
	if buildDir != "" && (abs == buildDir || strings.HasPrefix(abs, buildDir+"/")) {
		return
	}
	inputs[abs] = true
}

// recordParent: path was stat'ed or resolved, or is missing.
func recordParent(path string) { recordExisting(filepath.Dir(path)) }

// recordExisting: directory path or, missing, its nearest existing ancestor.
func recordExisting(path string) {
	abs, err := filepath.Abs(path)
	if err != nil {
		return
	}
	for {
		if _, err := os.Stat(abs); err == nil || abs == "/" {
			recordDir(abs)
			return
		}
		abs = filepath.Dir(abs)
	}
}

// readFile is os.ReadFile, recorded.
func readFile(path string) ([]byte, error) {
	b, err := os.ReadFile(path)
	if err == nil {
		record(path)
	} else {
		recordParent(path)
	}
	return b, err
}

// openFile is os.Open for reading, recorded.
func openFile(path string) (*os.File, error) {
	f, err := os.Open(path)
	if err == nil {
		record(path)
	} else {
		recordParent(path)
	}
	return f, err
}

// statPath is os.Stat, its answer depending on the parent's entries.
func statPath(path string) (fs.FileInfo, error) {
	recordParent(path)
	return os.Stat(path)
}

// lstatPath is os.Lstat, its answer depending on the parent's entries.
func lstatPath(path string) (fs.FileInfo, error) {
	recordParent(path)
	return os.Lstat(path)
}

// readLink is os.Readlink: a retargeted link is a new entry of its parent.
func readLink(path string) (string, error) {
	recordParent(path)
	return os.Readlink(path)
}

// evalSymlinks is filepath.EvalSymlinks; the resolved path's parent is recorded too.
func evalSymlinks(path string) (string, error) {
	recordParent(path)
	real, err := filepath.EvalSymlinks(path)
	if err == nil {
		recordParent(real)
	}
	return real, err
}

// readDir is os.ReadDir, recorded as a listing.
func readDir(dir string) ([]fs.DirEntry, error) {
	recordExisting(dir)
	return os.ReadDir(dir)
}

// glob is filepath.Glob over one directory level: the directory holding the pattern's wildcard part is the listing.
func glob(pattern string) ([]string, error) {
	recordExisting(filepath.Dir(pattern))
	return filepath.Glob(pattern)
}

// parseGoFile is go/parser.ParseFile reading the file itself (Soong's sources), recorded.
func parseGoFile(path string) (*ast.File, error) {
	f, err := parser.ParseFile(token.NewFileSet(), path, nil, 0)
	if err == nil {
		record(path)
	} else {
		recordParent(path)
	}
	return f, err
}

// command is exec.Command whose output depends on the program: its resolved binary is recorded (a package upgrade replaces it).
func command(name string, args ...string) *exec.Cmd {
	if p, err := exec.LookPath(name); err == nil {
		if real, err := filepath.EvalSymlinks(p); err == nil {
			record(real)
		}
	}
	return exec.Command(name, args...)
}

// depfileEscape: a path as ninja's depfile parser reads it (depfile_parser.in.cc): `\` before space and `#`, `$$` for `$`.
func depfileEscape(path string) string {
	return strings.NewReplacer(" ", `\ `, "#", `\#`, "$", "$$").Replace(path)
}

// depfileSpellable: ninja's depfile lexer takes a path only of [A-Za-z0-9+,/_:.~(){}%=@[]!-], bytes >= 0x80 and the escaped space, `#`, `$`; any other byte splits it (measured: blueprint's testdata dir `escapes/**/b` read as `.../escapes` and a missing `/b`, regenerating forever).
func depfileSpellable(path string) bool {
	for i := 0; i < len(path); i++ {
		c := path[i]
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c >= 0x80 || strings.IndexByte("+,/_:.~(){}%=@[]!- #$", c) >= 0) {
			return false
		}
	}
	return true
}

// writeDepfile writes the recorded inputs as a depfile of target, one path per line, sorted. A path the depfile cannot spell stands as its nearest ancestor that it can: coarser for those (a change below it is missed), never a split path.
func writeDepfile(path, target string) error {
	set := map[string]bool{}
	for p := range inputs {
		for !depfileSpellable(p) {
			p = filepath.Dir(p)
		}
		set[p] = true
	}
	list := make([]string, 0, len(set))
	for p := range set {
		list = append(list, p)
	}
	sort.Strings(list)
	var b strings.Builder
	b.WriteString(depfileEscape(target) + ":")
	for _, p := range list {
		b.WriteString(" \\\n  " + depfileEscape(p))
	}
	b.WriteString("\n")
	return os.WriteFile(path, []byte(b.String()), 0o644)
}
