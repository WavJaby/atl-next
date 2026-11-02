// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

// Expected values follow build/release's release_config semantics: every config inherits root first, then its inherits chain
// parents first; a later config's flag value wins over an earlier one and over the declaration; bool flags read "true" / "" as
// Soong's GetBuildFlag reports them. The fixture mirrors bp2a -> bp1a -> ap4a -> ap3a with two of those links.

func TestReadRelease(t *testing.T) {
	tree := t.TempDir()
	write := func(path, body string) {
		p := filepath.Join(tree, "build/release", path)
		if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(p, []byte(body), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	write("release_configs/root.textproto", "name: \"root\"\naconfig_value_sets: \"vs-root\"\n")
	write("release_configs/a.textproto", "name: \"a\"\naconfig_value_sets: \"vs-a\"\n")
	write("release_configs/b.textproto", "name: \"b\"\ninherits: \"a\"\naconfig_value_sets: \"vs-b\"\n")
	write("flag_declarations/F_PERM.textproto", "name: \"F_PERM\"\nvalue: {\n  string_value: \"READ_WRITE\"\n}\n")
	write("flag_declarations/F_BOOL.textproto", "name: \"F_BOOL\"\nvalue: {\n  bool_value: false\n}\n")
	write("flag_declarations/F_KEEP.textproto", "name: \"F_KEEP\"\nvalue: {\n  bool_value: true\n}\n")
	write("flag_values/a/F_PERM.textproto", "name: \"F_PERM\"\nvalue {\n  string_value: \"READ_ONLY\"\n}\n")
	write("flag_values/a/F_BOOL.textproto", "name: \"F_BOOL\"\nvalue {\n  bool_value: true\n}\n")
	write("flag_values/b/F_BOOL.textproto", "name: \"F_BOOL\"\nvalue {\n  bool_value: false\n}\n")
	// android_soong_config_vars.mk: top-level release-flag lines only; inside an if block or computed by make, not read
	mk := filepath.Join(tree, configVarsMk)
	if err := os.MkdirAll(filepath.Dir(mk), 0o755); err != nil {
		t.Fatal(err)
	}
	body := "$(call soong_config_set,ANDROID,perm,$(F_PERM))\n" +
		"$(call soong_config_set_bool,ANDROID,keep,$(RELEASE_KEEP))\n" +
		"$(call soong_config_set_bool,ANDROID,off,$(RELEASE_OFF))\n" +
		"$(call soong_config_set,ns,mode,$(RELEASE_MODE))\n" +
		"ifeq (true,$(RELEASE_KEEP))\n$(call soong_config_set,ANDROID,inner,$(RELEASE_MODE))\nendif\n" +
		"$(call soong_config_set_bool,ANDROID,made,$(if $(RELEASE_KEEP),true,false))\n"
	if err := os.WriteFile(mk, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
	write("flag_declarations/RELEASE_KEEP.textproto", "name: \"RELEASE_KEEP\"\nvalue: {\n  bool_value: true\n}\n")
	write("flag_declarations/RELEASE_OFF.textproto", "name: \"RELEASE_OFF\"\nvalue: {\n  bool_value: false\n}\n")
	write("flag_declarations/RELEASE_MODE.textproto", "name: \"RELEASE_MODE\"\nvalue: {\n  string_value: \"fast\"\n}\n")
	r, err := readRelease(tree, "b")
	if err != nil {
		t.Fatal(err)
	}
	if want := []string{"root", "a", "b"}; !reflect.DeepEqual(r.Chain, want) {
		t.Errorf("chain %v, want %v", r.Chain, want)
	}
	if want := []string{"vs-root", "vs-a", "vs-b"}; !reflect.DeepEqual(r.ValueSets, want) {
		t.Errorf("value sets %v, want %v", r.ValueSets, want)
	}
	if r.Flag("F_PERM") != "READ_ONLY" || r.Bool("F_BOOL") || r.Flag("F_BOOL") != "" || !r.Bool("F_KEEP") {
		t.Errorf("flags %v", r.flags)
	}
	// select(release_flag()): bool flags as true / false, strings as written, undeclared unset
	for name, want := range map[string][2]string{"F_BOOL": {"false", "ok"}, "F_KEEP": {"true", "ok"}, "F_PERM": {"READ_ONLY", "ok"}, "NOPE": {"", ""}} {
		v, ok := r.Condition(name)
		if v != want[0] || ok != (want[1] == "ok") {
			t.Errorf("Condition(%s) = %q %v, want %v", name, v, ok, want)
		}
	}
	// soong config variables: only RELEASE_* flags named directly, outside if blocks; soong_config_set_bool false for a false flag
	if want := map[string]string{"ANDROID/keep": "true", "ANDROID/off": "false", "ns/mode": "fast"}; !reflect.DeepEqual(r.configVars, want) {
		t.Errorf("config vars %v, want %v", r.configVars, want)
	}
	defer func() {
		if recover() == nil {
			t.Errorf("an undeclared flag must panic")
		}
	}()
	r.Flag("F_UNDECLARED")
}

func TestPlatformSdk(t *testing.T) {
	r := &Release{flags: map[string]string{"RELEASE_PLATFORM_VERSION_CODENAME": "REL", "RELEASE_PLATFORM_SDK_VERSION": "36"}}
	if lvl, final := r.PlatformSdk(); lvl != "36" || !final {
		t.Errorf("REL: %s %v", lvl, final)
	}
	r.flags["RELEASE_PLATFORM_VERSION_CODENAME"] = "Baklava"
	// android/api_levels.go FutureApiLevel
	if lvl, final := r.PlatformSdk(); lvl != "10000" || final {
		t.Errorf("preview: %s %v", lvl, final)
	}
}
