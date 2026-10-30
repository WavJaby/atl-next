// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"regexp"
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
	r := &Release{Name: name, flags: map[string]string{}}
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
	return r, nil
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
