// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

// The fixture copies the shapes of build/soong/java/config/config.go (var lists, pctx.StaticVariable with a literal, a joined
// literal list and an append of a list variable); expected values are the fixture's literals.
const javaConfigFixture = `package config

var (
	javaVmFlagsList = []string{
		"-XX:A",
		"-XX:B",
	}
	dexerJavaVmFlagsList = []string{"-JXX:C"}
)

func init() {
	pctx.StaticVariable("JavacHeapSize", "4096M")
	pctx.StaticVariable("D8Flags", strings.Join(append([]string{
		"-JXmx4096M",
	}, dexerJavaVmFlagsList...), " "))
	pctx.StaticVariable("CommonJdkFlags", strings.Join([]string{
		` + "`-Xmaxerrs 9999999`" + `,
		` + "`-sourcepath \"\"`" + `,
	}, " "))
}
`

func TestGoConstants(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.go")
	if err := os.WriteFile(path, []byte(javaConfigFixture), 0o644); err != nil {
		t.Fatal(err)
	}
	lists, strs := map[string][]string{}, map[string]string{}
	if err := goConstants(path, lists, strs); err != nil {
		t.Fatal(err)
	}
	for name, want := range map[string][]string{
		"javaVmFlagsList": {"-XX:A", "-XX:B"},
		"D8Flags":         {"-JXmx4096M", "-JXX:C"},
		"CommonJdkFlags":  {"-Xmaxerrs 9999999", `-sourcepath ""`},
	} {
		if !reflect.DeepEqual(lists[name], want) {
			t.Errorf("%s = %q, want %q", name, lists[name], want)
		}
	}
	if strs["JavacHeapSize"] != "4096M" {
		t.Errorf("JavacHeapSize = %q", strs["JavacHeapSize"])
	}
}
