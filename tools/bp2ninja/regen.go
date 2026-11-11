// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// build.ninja regenerates itself: ninja is the one engine deciding what is out of date, the conversion included.
// The caller (xtask native) writes the converter's flags and goals into an argument file. `bp2ninja -bootstrap <file>` writes a build.ninja holding only the edge that runs `bp2ninja -args <file>`;
// ninja runs it (output and deps record missing) and every later time an input changed: its depfile lists every input the conversion read (inputs.go), the argument file and the converter binary among them,
// so a flag, goal or converter change reruns it as a tree change does. Bootstrapping through ninja, not by converting directly, leaves the deps log holding the record from the first run on.
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

// writeBootstrap writes <out>/build.ninja holding only the regeneration edge.
func (r *regenSetup) writeBootstrap(out string) error {
	if err := os.MkdirAll(out, 0o755); err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(out, regenOutputs[0]), []byte(r.ninja()), 0o644)
}

// regenOutputs: the outputs of the regeneration edge, relative to the out dir: ninja finds the manifest's edge by the name it loaded (`build.ninja` under -C), not by an absolute path.
var regenOutputs = []string{"build.ninja", "problems.txt", "graph.json", "missing.txt", "missing-paths.txt"}

// regenDepfile: the edge's depfile, relative to the out dir.
const regenDepfile = "build.ninja.d"

// ninja: the regeneration rule and edge.
func (r *regenSetup) ninja() string {
	return "rule bp2ninja\n  command = " + esc(r.exe) + " " + argsFlag + " " + esc(r.args) + "\n" +
		"  description = bp2ninja: an input of build.ninja changed, converting again\n" +
		"  depfile = " + regenDepfile + "\n  deps = gcc\n  generator = 1\n" +
		"build " + strings.Join(regenOutputs, " ") + ": bp2ninja" + implicitDeps(r.implicit) + "\n"
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

// writeDepfile writes the edge's depfile into out: every input recorded so far.
func (r *regenSetup) writeDepfile(out string) error {
	return writeDepfile(filepath.Join(out, regenDepfile), regenOutputs[0])
}
