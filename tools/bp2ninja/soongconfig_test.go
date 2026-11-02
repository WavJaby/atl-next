// SPDX-License-Identifier: MIT

package main

import (
	"slices"
	"testing"
)

// android/soongconfig/modules.go PropertiesToApply per kind; config.go soongConfig.Bool. Each variable contributes one flag, so the
// expected list names which branch each took: b set "on" (Bool true), off set "false", s = "two", v = "x" (%s substituted), u unset.
func TestSoongConfigProps(t *testing.T) {
	g := graphFor(t, map[string]string{"p/Android.bp": `
soong_config_module_type {
    name: "my_cc",
    module_type: "cc_library",
    config_namespace: "ns",
    bool_variables: ["b", "off", "u"],
    value_variables: ["v"],
    variables: ["s"],
    properties: ["cflags"],
}
my_cc {
    name: "libc1",
    cflags: ["-DBASE"],
    soong_config_variables: {
        b: { cflags: ["-DB_ON"], conditions_default: { cflags: ["-DB_DEF"] } },
        off: { cflags: ["-DOFF_ON"], conditions_default: { cflags: ["-DOFF_DEF"] } },
        s: { one: { cflags: ["-DS_ONE"] }, two: { cflags: ["-DS_TWO"] }, conditions_default: { cflags: ["-DS_DEF"] } },
        v: { cflags: ["-DV=%s"], conditions_default: { cflags: ["-DV_DEF"] } },
        u: { cflags: ["-DU_ON"], conditions_default: { cflags: ["-DU_DEF"] } },
    },
}
`})
	m := g.lookup("my_cc", Device)
	g.soongConfigTypes = map[string]*soongConfigType{"my_cc": readSoongConfigType(m)}
	g.release = &Release{configVars: map[string]string{"ns/b": "on", "ns/off": "false", "ns/s": "two", "ns/v": "x"}}
	want := []string{"-DBASE", "-DB_ON", "-DOFF_DEF", "-DS_TWO", "-DV=x", "-DU_DEF"}
	if got := g.flatten(g.lookup("libc1", Device), Device, Shared).strs("cflags"); !slices.Equal(got, want) {
		t.Errorf("cflags %q, want %q", got, want)
	}
}
