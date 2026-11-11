// SPDX-License-Identifier: MIT

package main

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"
)

// build.ninja regenerates itself: ninja is the one engine deciding what is out of date, the conversion included.
// The caller (xtask native) writes the converter's flags and goals into an argument file. `bp2ninja -bootstrap <file>` writes a build.ninja holding only the edge that runs `bp2ninja -args <file>`;
// ninja runs it (output and deps record missing) and every later time an input changed: its depfile lists every input the conversion read (inputs.go), the argument file and the converter binary among them,
// so a flag, goal or converter change reruns it as a tree change does. Directory entries reach it through the probe check edges' results (inputs.go), not as directories. Bootstrapping through ninja, not by converting directly, leaves the deps log holding the record from the first run on.
// The command line never changes, so `generator = 1` (no rerun on a command change, not cleaned) loses nothing.

// argsFlag: `-args <file>`: the converter's arguments are the file's lines; bootstrapFlag: `-bootstrap <file>`: only write the build.ninja that runs `-args <file>`.
const (
	argsFlag      = "-args"
	bootstrapFlag = "-bootstrap"
)

// regenSetup: how build.ninja's regeneration edge reruns this conversion.
type regenSetup struct {
	exe       string // the converter binary
	args      string // the argument file, absolute
	bootstrap bool
	// outputs of edges in build.ninja the conversion reads (the loadLibrary scan, bootclasspath.go): implicit inputs, so ninja brings them up to date before deciding on a regeneration
	implicit []string
}

// expandArgs: argv as given, or with `-args <file>` / `-bootstrap <file>` the file's lines and the setup of the regeneration edge.
func expandArgs(argv []string) ([]string, *regenSetup, error) {
	if len(argv) == 0 || argv[0] != argsFlag && argv[0] != bootstrapFlag {
		return argv, nil, nil
	}
	if len(argv) != 2 {
		return nil, nil, fmt.Errorf("%s <file> takes no other arguments", argv[0])
	}
	file, err := filepath.Abs(argv[1])
	if err != nil {
		return nil, nil, err
	}
	text, err := readFile(file)
	if err != nil {
		return nil, nil, err
	}
	exe, err := os.Executable()
	if err != nil {
		return nil, nil, err
	}
	record(exe)
	// one argument per line, each line ended by "\n": an empty line is an empty argument (`-omit` with nothing omitted)
	var out []string
	if len(text) > 0 {
		out = strings.Split(strings.TrimSuffix(string(text), "\n"), "\n")
	}
	return out, &regenSetup{exe: exe, args: file, bootstrap: argv[0] == bootstrapFlag}, nil
}

// writeBootstrap writes <out>/build.ninja holding only the regeneration edge: no probes exist yet, so no check edges.
func (r *regenSetup) writeBootstrap(out string) error {
	if err := os.MkdirAll(out, 0o755); err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(out, regenOutputs[0]), []byte(r.regenEdge(nil)), 0o644)
}

// manifest: the check edges of the directory probes (inputs.go) and the regeneration edge depending on their results.
// A missing result is created here, before build.ninja is written: the check edge, on its first run, then finds it older than build.ninja (a result created by the check would be newer -> a second conversion on every first build).
func (r *regenSetup) manifest(out string) (string, error) {
	var b strings.Builder
	b.WriteString("rule bp2ninja_globs\n  command = " + esc(r.exe) + " " + checkGlobsFlag + " $in $out\n" +
		"  description = bp2ninja: directory entries changed, asking $in again\n" +
		"  depfile = $out.d\n  deps = gcc\n  restat = 1\n")
	var results []string
	for n := 0; n < globShards; n++ {
		result := globFile(n, "result")
		if err := os.MkdirAll(filepath.Join(out, globDir), 0o755); err != nil {
			return "", err
		}
		f, err := os.OpenFile(filepath.Join(out, result), os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o644)
		if err == nil {
			f.Close()
		} else if !errors.Is(err, fs.ErrExist) {
			return "", err
		}
		b.WriteString("build " + esc(result) + ": bp2ninja_globs " + esc(globFile(n, "json")) + "\n")
		results = append(results, result)
	}
	b.WriteString(r.regenEdge(results))
	return b.String(), nil
}

// regenOutputs: the outputs of the regeneration edge, relative to the out dir: ninja finds the manifest's edge by the name it loaded (`build.ninja` under -C), not by an absolute path.
var regenOutputs = []string{"build.ninja", "problems.txt", "graph.json", "missing.txt", "missing-paths.txt"}

// regenDepfile: the edge's depfile, relative to the out dir.
const regenDepfile = "build.ninja.d"

// regenEdge: the regeneration rule and edge; globResults are implicit inputs beside r.implicit.
func (r *regenSetup) regenEdge(globResults []string) string {
	return "rule bp2ninja\n  command = " + esc(r.exe) + " " + argsFlag + " " + esc(r.args) + "\n" +
		"  description = bp2ninja: an input of build.ninja changed, converting again\n" +
		"  depfile = " + regenDepfile + "\n  deps = gcc\n  generator = 1\n" +
		"build " + strings.Join(regenOutputs, " ") + ": bp2ninja" + implicitDeps(append(slices.Clone(r.implicit), globResults...)) + "\n"
}

func implicitDeps(paths []string) string {
	s := ""
	for _, p := range paths {
		s += " " + esc(p)
	}
	if s != "" {
		s = " |" + s
	}
	return s
}

// writeInputs writes into out what the conversion read so far: the files as the edge's depfile, the directory probes as the check edges' lists.
func (r *regenSetup) writeInputs(out string) error {
	if err := writeProbes(out); err != nil {
		return err
	}
	return writeDepfile(filepath.Join(out, regenDepfile), regenOutputs[0], inputs)
}
