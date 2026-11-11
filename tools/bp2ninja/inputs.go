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

// What the conversion depends on, for build.ninja's regeneration edge (regen.go): ninja reruns the converter exactly when one changed. Two classes, by how the converter looked.
// A file read -> the file (its content): `recorder.inputs`, the regeneration edge's depfile.
// A question about a directory's entries -> a probe holding the question and the answer the conversion got (globs.go): a listing (readDir, walkTree) -> the entries' names, types and link texts; a glob -> its matches;
// a path stat'ed, resolved, or read but missing -> its type or target, or missing. A probe's anchors are the directories whose entries decide its answer.
// Probes mirror Soong's glob results (soong_build writes `.globs`, soong_ui re-runs a glob whose deps changed and touches `.glob_results` only when its matches differ: build/soong/ui/build/soong.go checkGlobs, the regen's implicit `.glob_results` at :247;
// an existence check is a one-path glob: android/paths.go existsWithDependencies). Here ninja runs the check: each shard's check edge (regen.go manifest) depends on its anchors and rewrites its result, restat, only on a different answer.
// So a save by rename (sed -i, most editors: the directory's mtime moves, its entries do not) reruns one shard's check, not the conversion.
// A directory of the build dir (-out) never counts: its entries are build outputs, the conversion's own among them (build.ninja.d, compat/), which change after every conversion -> regeneration forever (measured).
// A file there that the conversion reads does (xtask's argument file and JSON inputs).
// Files the converter writes (compat/, generated files) and reads back only to write if changed are not inputs: those calls use os directly.
// rec: the run's recorder; a test starts a run with a new one.
var rec = newRecorder()

type recorder struct {
	inputs map[string]bool
	// probes: by kind + path; the first answer stands (the conversion used it).
	probes map[[2]string]probe
	// listings: readDir's answers by the path as asked: a directory is listed once per run, so the conversion sees the recorded answer (globs re-walked the same subtrees per pattern: ~half the converter's wall time).
	listings map[string]listing
	// trees: treeFiles' answers by base (ninja.go), made of the listings above
	trees map[string][]string
}

type listing struct {
	entries []fs.DirEntry
	err     error
}

func newRecorder() *recorder {
	return &recorder{map[string]bool{}, map[[2]string]probe{}, map[string]listing{}, map[string][]string{}}
}

// probe: one question about directory entries and the answer the conversion got (Soong pathtools.GlobResult: Pattern, Matches, Deps).
type probe struct {
	Kind    string   `json:"k"`
	Path    string   `json:"p"`
	Answer  string   `json:"a"`
	Anchors []string `json:"d"` // absolute; may be missing: a depfile names the nearest existing ancestor (a missing depfile entry would make ninja rerun on every run)
}

// buildDir: -out, absolute; set before the conversion reads anything (main).
var buildDir string

func record(path string) {
	if abs, err := filepath.Abs(path); err == nil {
		rec.inputs[abs] = true
	}
}

func inBuildDir(abs string) bool {
	return buildDir != "" && (abs == buildDir || strings.HasPrefix(abs, buildDir+"/"))
}

// recordProbe keeps a question with its answer; anchors in the build dir are dropped, and a probe with none left is not kept.
func recordProbe(kind, path, answer string, anchors ...string) {
	abs, err := filepath.Abs(path)
	if err != nil {
		return
	}
	var keep []string
	for _, a := range anchors {
		if a, err := filepath.Abs(a); err == nil && !inBuildDir(a) {
			keep = append(keep, a)
		}
	}
	key := [2]string{kind, abs}
	if _, seen := rec.probes[key]; seen || len(keep) == 0 {
		return
	}
	rec.probes[key] = probe{kind, abs, answer, keep}
}

// recordUnread: a read that failed is a stat of the path: it may appear.
func recordUnread(path string) {
	recordProbe(probeStat, path, statAnswer(os.Stat(path)), filepath.Dir(path))
}

// readFile is os.ReadFile, recorded.
func readFile(path string) ([]byte, error) {
	b, err := os.ReadFile(path)
	if err == nil {
		record(path)
	} else {
		recordUnread(path)
	}
	return b, err
}

// openFile is os.Open for reading, recorded.
func openFile(path string) (*os.File, error) {
	f, err := os.Open(path)
	if err == nil {
		record(path)
	} else {
		recordUnread(path)
	}
	return f, err
}

// statPath is os.Stat: the answer is the type, which changes with the parent's entries (a content edit does not reach the output).
func statPath(path string) (fs.FileInfo, error) {
	info, err := os.Stat(path)
	recordProbe(probeStat, path, statAnswer(info, err), filepath.Dir(path))
	return info, err
}

// lstatPath is os.Lstat, as statPath.
func lstatPath(path string) (fs.FileInfo, error) {
	info, err := os.Lstat(path)
	recordProbe(probeLstat, path, statAnswer(info, err), filepath.Dir(path))
	return info, err
}

// readLink is os.Readlink: a retargeted link is a new entry of its parent.
func readLink(path string) (string, error) {
	target, err := os.Readlink(path)
	recordProbe(probeReadlink, path, stringAnswer(target, err), filepath.Dir(path))
	return target, err
}

// evalSymlinks is filepath.EvalSymlinks; anchored at the path's parent and the resolved path's.
func evalSymlinks(path string) (string, error) {
	real, err := filepath.EvalSymlinks(path)
	anchors := []string{filepath.Dir(path)}
	if err == nil {
		anchors = append(anchors, filepath.Dir(real))
	}
	recordProbe(probeRealpath, path, stringAnswer(real, err), anchors...)
	return real, err
}

// readDir is os.ReadDir, recorded as a listing of every entry.
func readDir(dir string) ([]fs.DirEntry, error) {
	entries, err := listDir(dir)
	recordListing(probeList, dir, entries, err)
	return entries, err
}

// listDir is os.ReadDir, listed once per run (recorder.listings) and not recorded: its caller records the question it asked (recordListing).
func listDir(dir string) ([]fs.DirEntry, error) {
	if l, ok := rec.listings[dir]; ok {
		return l.entries, l.err
	}
	entries, err := os.ReadDir(dir)
	rec.listings[dir] = listing{entries, err}
	return entries, err
}

// recordListing records a listing as a probe of kind (probeList, probeBpList); the answer is formatted only for a question not yet recorded.
func recordListing(kind, dir string, entries []fs.DirEntry, err error) {
	if abs, aerr := filepath.Abs(dir); aerr == nil {
		if _, seen := rec.probes[[2]string{kind, abs}]; seen {
			return
		}
	}
	recordProbe(kind, dir, listingAnswer(kind, dir, entries, err), dir)
}

// glob is filepath.Glob over one directory level: the directory holding the pattern's wildcard part decides the matches.
func glob(pattern string) ([]string, error) {
	matches, err := filepath.Glob(pattern)
	recordProbe(probeGlob, pattern, globAnswer(matches, err), filepath.Dir(pattern))
	return matches, err
}

// parseGoFile is go/parser.ParseFile reading the file itself (Soong's sources), recorded.
func parseGoFile(path string) (*ast.File, error) {
	f, err := parser.ParseFile(token.NewFileSet(), path, nil, 0)
	if err == nil {
		record(path)
	} else {
		recordUnread(path)
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
	return depfileEscaper.Replace(path)
}

var depfileEscaper = strings.NewReplacer(" ", `\ `, "#", `\#`, "$", "$$")

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

// writeDepfile writes paths as a depfile of target, one per line, sorted. A path the depfile cannot spell stands as its nearest ancestor that it can: coarser for those (a change below it is missed), never a split path.
func writeDepfile(path, target string, paths map[string]bool) error {
	set := map[string]bool{}
	for p := range paths {
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
