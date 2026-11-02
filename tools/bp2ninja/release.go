// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
)

// Release is one AOSP release config (build/release, `TARGET_RELEASE`): its build flag values and its aconfig value sets.
//   - Inheritance (release_config tool): every config inherits `root` first, then its `inherits` chain, parents before children; a later
//     config's flag value overrides an earlier one, the declaration's value is the default.
//   - Only the textproto forms build/release uses are read: `name: "X"`, `inherits: "Y"`, `aconfig_value_sets: "Z"`, `bool_value: b`,
//     `string_value: "s"`. A flag declared nowhere is an error at lookup, not a silent default.
type Release struct {
	Name      string
	Chain     []string // root first, the config itself last
	ValueSets []string // aconfig_value_set module names, in chain order
	flags     map[string]string
	bools     map[string]bool // flags declared with a bool_value
	// soong config variables "<namespace>/<variable>" set from a release flag (readConfigVars)
	configVars map[string]string
}

var (
	reName     = regexp.MustCompile(`(?m)^name:\s*"([^"]*)"`)
	reInherits = regexp.MustCompile(`(?m)^inherits:\s*"([^"]*)"`)
	reValueSet = regexp.MustCompile(`(?m)^aconfig_value_sets:\s*"([^"]*)"`)
	reBool     = regexp.MustCompile(`bool_value:\s*(true|false)`)
	reString   = regexp.MustCompile(`string_value:\s*"([^"]*)"`)
)

// releaseRoot is the config every release config inherits first (release_config's implicit root).
const releaseRoot = "root"

func readRelease(tree, name string) (*Release, error) {
	dir := filepath.Join(tree, "build/release")
	r := &Release{Name: name, flags: map[string]string{}, bools: map[string]bool{}}
	seen := map[string]bool{}
	var walk func(cfg string) error
	walk = func(cfg string) error {
		if seen[cfg] {
			return nil
		}
		seen[cfg] = true
		text, err := os.ReadFile(filepath.Join(dir, "release_configs", cfg+".textproto"))
		if err != nil {
			return err
		}
		for _, m := range reInherits.FindAllStringSubmatch(string(text), -1) {
			if err := walk(m[1]); err != nil {
				return err
			}
		}
		r.Chain = append(r.Chain, cfg)
		for _, m := range reValueSet.FindAllStringSubmatch(string(text), -1) {
			r.ValueSets = append(r.ValueSets, m[1])
		}
		return nil
	}
	if err := walk(releaseRoot); err != nil {
		return nil, err
	}
	if err := walk(name); err != nil {
		return nil, err
	}
	decls, _ := filepath.Glob(filepath.Join(dir, "flag_declarations", "*.textproto"))
	for _, f := range decls {
		if n, v, ok := flagValue(f); ok {
			r.flags[n] = v
			if b, err := os.ReadFile(f); err == nil && reBool.Match(b) {
				r.bools[n] = true
			}
		}
	}
	for _, cfg := range r.Chain {
		vals, _ := filepath.Glob(filepath.Join(dir, "flag_values", cfg, "*.textproto"))
		for _, f := range vals {
			if n, v, ok := flagValue(f); ok {
				r.flags[n] = v
			}
		}
	}
	var err error
	r.configVars, err = readConfigVars(filepath.Join(tree, configVarsMk), r)
	return r, err
}

// configVarsMk: where the product config maps build flags to Soong config variables.
const configVarsMk = "build/make/core/android_soong_config_vars.mk"

var reConfigSet = regexp.MustCompile(`^\$\(call (soong_config_set|soong_config_set_bool|add_soong_config_var_value),\s*(\w+)\s*,\s*(\w+)\s*,\s*\$\((RELEASE_\w+)\)\s*\)\s*$`)

// readConfigVars: the soong config variables android_soong_config_vars.mk sets straight from a release flag, outside any if block
// (`$(call soong_config_set|soong_config_set_bool|add_soong_config_var_value,<ns>,<var>,$(RELEASE_X))`). soong_config_set_bool makes an
// unset or false flag "false"; the others leave an empty value unset. Variables computed by make expressions or product variables are not
// read: their selects take the default branch.
func readConfigVars(path string, r *Release) (map[string]string, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	out := map[string]string{}
	depth := 0
	for _, line := range strings.Split(string(b), "\n") {
		t := strings.TrimSpace(line)
		switch {
		case strings.HasPrefix(t, "ifdef ") || strings.HasPrefix(t, "ifndef ") || strings.HasPrefix(t, "ifeq ") || strings.HasPrefix(t, "ifneq "):
			depth++
			continue
		case t == "endif":
			depth--
			continue
		}
		m := reConfigSet.FindStringSubmatch(line)
		if m == nil || depth > 0 {
			continue
		}
		v, declared := r.flags[m[4]]
		switch {
		case m[1] == "soong_config_set_bool" && v != "true":
			v = "false"
		case !declared || v == "":
			continue
		}
		out[m[2]+"/"+m[3]] = v
	}
	return out, nil
}

// Condition answers select(release_flag(name)): a bool flag "true" / "false", a string flag its value; ok=false for an undeclared flag.
func (r *Release) Condition(name string) (string, bool) {
	v, ok := r.flags[name]
	if !ok {
		return "", false
	}
	if r.bools[name] && v != "true" {
		v = "false"
	}
	return v, true
}

// Names: the declared flags whose names start with prefix, sorted.
func (r *Release) Names(prefix string) []string {
	var out []string
	for n := range r.flags {
		if strings.HasPrefix(n, prefix) {
			out = append(out, n)
		}
	}
	sort.Strings(out)
	return out
}

// ConfigVar answers select(soong_config_variable(ns, name)) for the variables readConfigVars knows.
func (r *Release) ConfigVar(ns, name string) (string, bool) {
	v, ok := r.configVars[ns+"/"+name]
	return v, ok
}

// flagValue reads one flag declaration or value file: bool flags are "true" / "" as Soong's GetBuildFlag reports them.
func flagValue(path string) (name, value string, ok bool) {
	b, err := os.ReadFile(path)
	if err != nil {
		return "", "", false
	}
	text := string(b)
	m := reName.FindStringSubmatch(text)
	if m == nil {
		return "", "", false
	}
	if v := reBool.FindStringSubmatch(text); v != nil {
		if v[1] == "true" {
			return m[1], "true", true
		}
		return m[1], "", true
	}
	if v := reString.FindStringSubmatch(text); v != nil {
		return m[1], v[1], true
	}
	return m[1], "", true
}

// Flag is a build flag's value; a flag no declaration names is a converter bug, so it panics.
func (r *Release) Flag(name string) string {
	v, ok := r.flags[name]
	if !ok {
		panic(fmt.Sprintf("release %s: build flag %s is not declared", r.Name, name))
	}
	return v
}

func (r *Release) Bool(name string) bool { return r.Flag(name) == "true" }

// PlatformSdk: the API level `current` / `none` resolve to (ApiLevel.EffectiveVersion): the platform SDK version once the platform is
// final (codename REL), else Soong's future level 10000 with --android-platform-build.
func (r *Release) PlatformSdk() (level string, final bool) {
	if strings.TrimSpace(r.Flag("RELEASE_PLATFORM_VERSION_CODENAME")) == "REL" {
		return r.Flag("RELEASE_PLATFORM_SDK_VERSION"), true
	}
	return futureApiLevel, false
}

// futureApiLevel is Soong's FutureApiLevel (android/api_levels.go): an unfinalised API, compiled as the platform.
const futureApiLevel = "10000"
