// SPDX-License-Identifier: MIT

package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"hash/fnv"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"
)

// Directory probes (inputs.go) as Soong's glob results: answers formatted from one call each, the shard lists the converter writes, and the check edge's second ask (regen.go manifest).

// The probe kinds: each answer is formatted by one function from the same call the conversion made, so the check (askProbe) and the conversion cannot disagree on format.
const (
	probeList     = "list"
	probeStat     = "stat"
	probeLstat    = "lstat"
	probeReadlink = "readlink"
	probeRealpath = "realpath"
	probeGlob     = "glob"
)

func errAnswer(err error) string {
	if errors.Is(err, fs.ErrNotExist) {
		return "missing"
	}
	return "error: " + err.Error()
}

// listAnswer: name, type and, for a link, its text, per entry: a retargeted link changes the answer, a rewritten file does not.
func listAnswer(dir string, entries []fs.DirEntry, err error) string {
	if err != nil {
		return errAnswer(err)
	}
	var b strings.Builder
	for _, e := range entries {
		b.WriteString(e.Name() + "\t" + e.Type().String())
		if e.Type()&fs.ModeSymlink != 0 {
			target, _ := os.Readlink(filepath.Join(dir, e.Name()))
			b.WriteString("\t" + target)
		}
		b.WriteString("\n")
	}
	return b.String()
}

func statAnswer(info fs.FileInfo, err error) string {
	if err != nil {
		return errAnswer(err)
	}
	return info.Mode().Type().String()
}

func stringAnswer(s string, err error) string {
	if err != nil {
		return errAnswer(err)
	}
	return "= " + s
}

func globAnswer(matches []string, err error) string {
	if err != nil {
		return errAnswer(err)
	}
	return strings.Join(matches, "\n")
}

// askProbe asks a recorded question again.
func askProbe(p probe) (string, error) {
	switch p.Kind {
	case probeList:
		entries, err := os.ReadDir(p.Path)
		return listAnswer(p.Path, entries, err), nil
	case probeStat:
		return statAnswer(os.Stat(p.Path)), nil
	case probeLstat:
		return statAnswer(os.Lstat(p.Path)), nil
	case probeReadlink:
		return stringAnswer(os.Readlink(p.Path)), nil
	case probeRealpath:
		return stringAnswer(filepath.EvalSymlinks(p.Path)), nil
	case probeGlob:
		return globAnswer(filepath.Glob(p.Path)), nil
	}
	return "", fmt.Errorf("unknown probe kind %q", p.Kind)
}

// globShards: probes are split by anchor into this many check edges, so a changed directory re-asks one shard's questions, not all.
// Basis: ~97k probes on a full conversion (slot-1, 2026-11-12: 97478 directories in the old regen depfile); 64 shards -> ~1.5k re-asked questions per changed directory, a few ms beside the converter binary's start (Soong's old bpglob made a file per glob; per question here would be ~97k edges in build.ninja).
const globShards = 64

func shardOf(p probe) int {
	h := fnv.New32a()
	h.Write([]byte(p.Anchors[0]))
	return int(h.Sum32() % globShards)
}

// globDir: the shards' directory under the out dir; xtask keepset skips the check edges' records by it (native.rs GLOB_DIR).
const globDir = "globs"

// globFile: shard n's probe list ("json") or check result ("result"), relative to the out dir.
func globFile(n int, ext string) string {
	return filepath.Join(globDir, fmt.Sprintf("%d.%s", n, ext))
}

// writeProbes writes each shard's probe list into out, only when it changed: its check edge reruns on a newer list.
func writeProbes(out string) error {
	shards := make([][]probe, globShards)
	for _, p := range probes {
		n := shardOf(p)
		shards[n] = append(shards[n], p)
	}
	for n, list := range shards {
		slices.SortFunc(list, func(a, b probe) int { return strings.Compare(a.Kind+"\x00"+a.Path, b.Kind+"\x00"+b.Path) })
		if list == nil {
			list = []probe{}
		}
		b, err := json.Marshal(list)
		if err != nil {
			return err
		}
		if err := writeIfChanged(filepath.Join(out, globFile(n, "json")), b); err != nil {
			return err
		}
	}
	return nil
}

// writeIfChanged: an output read back only to keep its mtime when the bytes are the same.
func writeIfChanged(path string, b []byte) error {
	if old, err := os.ReadFile(path); err == nil && string(old) == string(b) {
		return nil
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	return os.WriteFile(path, b, 0o644)
}

// existingAncestor: path or its nearest existing ancestor.
func existingAncestor(path string) string {
	for {
		if _, err := os.Stat(path); err == nil || path == "/" {
			return path
		}
		path = filepath.Dir(path)
	}
}

// checkGlobsFlag: `-check-globs <shard list> <result>`: the check edge's command.
const checkGlobsFlag = "-check-globs"

// checkGlobs asks a shard's questions again and writes its depfile (`<result>.d`, the anchors); it writes result only when an answer differs, naming them (soong_ui's "One culprit glob"), or when result is missing.
func checkGlobs(list, result string) error {
	b, err := os.ReadFile(list)
	if err != nil {
		return err
	}
	var ps []probe
	if err := json.Unmarshal(b, &ps); err != nil {
		return fmt.Errorf("%s: %w", list, err)
	}
	anchors := map[string]bool{}
	var changed []string
	for _, p := range ps {
		for _, a := range p.Anchors {
			anchors[existingAncestor(a)] = true
		}
		got, err := askProbe(p)
		if err != nil {
			return fmt.Errorf("%s: %w", list, err)
		}
		if got != p.Answer {
			changed = append(changed, p.Kind+" "+p.Path)
		}
	}
	if err := writeDepfile(result+".d", result, anchors); err != nil {
		return err
	}
	if _, err := os.Stat(result); len(changed) == 0 && err == nil {
		return nil
	}
	if len(changed) > 0 {
		fmt.Printf("bp2ninja: %d directory answers changed, build.ninja is out of date; first: %s\n", len(changed), changed[0])
	}
	return os.WriteFile(result, []byte(strings.Join(changed, "\n")+"\n"), 0o644)
}
