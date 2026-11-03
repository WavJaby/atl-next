// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
)

// A build must never write into the source tree: Python run by a rule writes __pycache__ beside an imported module unless
// PYTHONDONTWRITEBYTECODE is set. The genrule rule's command is run as ninja would (variables substituted) on a script importing a
// sibling module; the control run drops the variable and must write __pycache__, so the probe is shown to respond.

func ruleCommand(t *testing.T, rules, name string) string {
	t.Helper()
	m := regexp.MustCompile(`(?m)^rule ` + name + `\n  command = (.*)$`).FindStringSubmatch(rules)
	if m == nil {
		t.Fatalf("no rule %s", name)
	}
	return m[1]
}

func TestNoPycacheInTree(t *testing.T) {
	if _, err := exec.LookPath("python3"); err != nil {
		t.Skip("python3 not installed")
	}
	if !strings.Contains(ruleCommand(t, javaGenRules, "logtags"), "PYTHONDONTWRITEBYTECODE=1 python3 ") {
		t.Error("logtags rule runs python3 without PYTHONDONTWRITEBYTECODE=1")
	}
	cmd := ruleCommand(t, header, "genrule")
	run := func(command string) bool {
		tree := t.TempDir()
		for name, body := range map[string]string{"tool.py": "import helper\n", "helper.py": "X = 1\n"} {
			if err := os.WriteFile(filepath.Join(tree, name), []byte(body), 0o644); err != nil {
				t.Fatal(err)
			}
		}
		script := filepath.Join(t.TempDir(), "gen.sh")
		if err := os.WriteFile(script, []byte("python3 tool.py\n"), 0o644); err != nil {
			t.Fatal(err)
		}
		c := strings.NewReplacer("$root", tree, "$script", script).Replace(command)
		sh := exec.Command("/bin/sh", "-c", c)
		sh.Env = append(os.Environ(), "PYTHONDONTWRITEBYTECODE=")
		if out, err := sh.CombinedOutput(); err != nil {
			t.Fatalf("%s: %v\n%s", c, err, out)
		}
		_, err := os.Stat(filepath.Join(tree, "__pycache__"))
		return err == nil
	}
	if run(cmd) {
		t.Error("genrule rule left __pycache__ in the tree")
	}
	if !run(strings.Replace(cmd, "PYTHONDONTWRITEBYTECODE=1 ", "", 1)) {
		t.Error("control: python wrote no __pycache__ without the variable; the probe does not respond")
	}
}
