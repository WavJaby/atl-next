// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
)

// build.version.extensions.* as AOSP's derive_sdk computes them at boot (packages/modules/SdkExtensions, pin in sources.toml): SetSdkLevels (derive_sdk.cpp l.186-265)
// reads extensions_db.textpb and each installed APEX's sdkinfo.pb, then sets one property per dessert. ATL has no /apex tree at boot, so the converter derives the same
// values at build time and run() appends them to build.prop (buildprop.go), as derive_classpath's boot classpath is derived here too (bootclasspath.go). This is a
// second copy of upstream's ~60-line algorithm in Go, accepted for that reason.
//   - Installed APEX: one with at least one jar on the boot classpath (Graph.bootJars; com.android.art included). An APEX without one (com.android.extservices) is
//     missing: derive_sdk finds no sdkinfo.pb for it, so its module has no version.
//   - Module version of an installed APEX: RELEASE_PLATFORM_SDK_EXTENSION_VERSION. Every module APEX ships current_sdkinfo generated from that flag
//     (packages/modules/common/tools/Android.bp, cur_sdkinfo_src).
//   - Read from the tree, never copied: the APEX -> module map, the dessert sets and the db. A shape these parsers do not know fails the converter with
//     "changed shape: update sdkext.go", as bootJarsFinal does for product_config.mk.
//   - Output order: SetSdkLevels' order, r, s, t, u, v, b, then ad_services. Lines are name=value, without a `ro.` prefix (derive_sdk.cpp kSystemPropertiesPrefix).

const (
	deriveSdkCpp  = "packages/modules/SdkExtensions/derive_sdk/derive_sdk.cpp"
	extensionsDB  = "packages/modules/SdkExtensions/gen_sdk/extensions_db.textpb"
	sdkPropPrefix = "build.version.extensions."
	adServices    = "AD_SERVICES"
	// derive_sdk.cpp l.251-253: an AD_SERVICES module at this version or above is the ad_services value itself; below it, GetSdkLevel over AD_SERVICES alone.
	// The literal is upstream's, and the tree does not say what it is based on.
	adServicesDirect = 7
	// Build.VERSION_CODES.TIRAMISU: derive_sdk's IsAtLeastT gate, for the t dessert and for ad_services.
	sdkTiramisu = 33
)

// sdkDessert is one dessert of SetSdkLevels. set is the letter of derive_sdk.cpp's k<set>Modules; minSDK is the Build.VERSION_CODES level at which
// SetSdkLevels sets it (IsAtLeastS..B: S 31, TIRAMISU 33, UPSIDE_DOWN_CAKE 34, VANILLA_ICE_CREAM 35, BAKLAVA 36). 0 for r, which is set unconditionally.
type sdkDessert struct {
	suffix string
	set    byte
	minSDK int
}

// sdkDesserts in SetSdkLevels order. Each set adds to the relevant modules whether or not its level is set (relevant_modules accumulates).
var sdkDesserts = []sdkDessert{{"r", 'R', 0}, {"s", 'S', 31}, {"t", 'T', sdkTiramisu}, {"u", 'U', 34}, {"v", 'V', 35}, {"b", 'B', 36}}

// Shape of the two upstream files, and of the textpb subset the db uses.
var (
	reApexMap  = regexp.MustCompile(`(?s)kApexNameToModule\s*=\s*\{(.*?)\};`)
	reApexPair = regexp.MustCompile(`\{"([^"]+)",\s*SdkModule::(\w+)\}`)
	reModSet   = regexp.MustCompile(`k([A-Z])Modules\s*=\s*\{([^}]*)\}`)
	reModule   = regexp.MustCompile(`SdkModule::(\w+)`)
	reTextTok  = regexp.MustCompile(`#[^\n]*|[{}:]|[^\s{}:#]+`)
)

// extVersion is one `versions` block of extensions_db.textpb: a db version and the module versions it requires.
type extVersion struct {
	version int
	reqs    []extRequirement
}

type extRequirement struct {
	module  string
	version int
}

// sdkExtensions returns the build.version.extensions.* rows (name, value) of the tree's derive_sdk.cpp and extensions_db.textpb, given the installed APEX names
// (installedApexes). r gives the SDK level (which desserts are set) and the module version (RELEASE_PLATFORM_SDK_EXTENSION_VERSION).
func sdkExtensions(tree string, r *Release, apexes []string) ([][2]string, error) {
	src, err := readFile(filepath.Join(tree, deriveSdkCpp))
	if err != nil {
		return nil, err
	}
	apexModule, sets, err := parseDeriveSdk(string(src))
	if err != nil {
		return nil, err
	}
	text, err := readFile(filepath.Join(tree, extensionsDB))
	if err != nil {
		return nil, err
	}
	db, err := parseExtensionDB(string(text))
	if err != nil {
		return nil, err
	}
	sdk, err := releaseInt(r, "RELEASE_PLATFORM_SDK_VERSION")
	if err != nil {
		return nil, err
	}
	moduleVersion, err := releaseInt(r, "RELEASE_PLATFORM_SDK_EXTENSION_VERSION")
	if err != nil {
		return nil, err
	}
	// an installed APEX's sdkinfo.pb carries the release's version for its module
	versions := map[string]int{}
	for _, a := range apexes {
		if m, ok := apexModule[a]; ok {
			versions[m] = moduleVersion
		}
	}
	relevant := map[string]bool{}
	var rows [][2]string
	for _, d := range sdkDesserts {
		for _, m := range sets[d.set] {
			relevant[m] = true
		}
		if sdk >= d.minSDK {
			rows = append(rows, [2]string{sdkPropPrefix + d.suffix, strconv.Itoa(sdkLevel(db, relevant, versions))})
		}
	}
	// derive_sdk.cpp l.240-249: a requirement on a module that no dessert sets fails SetSdkLevels
	for _, v := range db {
		for _, req := range v.reqs {
			if !relevant[req.module] {
				return nil, fmt.Errorf("%s: version %d requires %s, which no dessert sets", filepath.Base(extensionsDB), v.version, req.module)
			}
		}
	}
	if sdk >= sdkTiramisu {
		level := versions[adServices]
		if level < adServicesDirect {
			level = sdkLevel(db, map[string]bool{adServices: true}, versions)
		}
		rows = append(rows, [2]string{sdkPropPrefix + "ad_services", strconv.Itoa(level)})
	}
	return rows, nil
}

// installedApexes: the APEX names of the boot classpath jars, each once, in their order. Platform jars have no APEX.
func installedApexes(jars []bootJar) []string {
	seen := map[string]bool{}
	var out []string
	for _, b := range jars {
		if b.apex == platformApex || seen[b.apex] {
			continue
		}
		seen[b.apex] = true
		out = append(out, b.apex)
	}
	return out
}

// sdkLevel is derive_sdk.cpp GetSdkLevel: the largest db version whose requirements on the relevant modules are all met.
func sdkLevel(db []extVersion, relevant map[string]bool, versions map[string]int) int {
	level := 0
	for _, v := range db {
		if v.version > level && requirementsMet(v, relevant, versions) {
			level = v.version
		}
	}
	return level
}

// requirementsMet is VersionRequirementsMet: a relevant module without a version is not met.
func requirementsMet(v extVersion, relevant map[string]bool, versions map[string]int) bool {
	for _, req := range v.reqs {
		if !relevant[req.module] {
			continue
		}
		have, ok := versions[req.module]
		if !ok || have < req.version {
			return false
		}
	}
	return true
}

// parseDeriveSdk reads derive_sdk.cpp's APEX name -> SdkModule name map and the dessert sets (set letter -> SdkModule names), in the declaration shapes it has.
func parseDeriveSdk(src string) (map[string]string, map[byte][]string, error) {
	m := reApexMap.FindStringSubmatch(src)
	if m == nil {
		return nil, nil, shapeErr(deriveSdkCpp, "kApexNameToModule not found")
	}
	apex := map[string]string{}
	for _, p := range reApexPair.FindAllStringSubmatch(m[1], -1) {
		apex[p[1]] = p[2]
	}
	if len(apex) == 0 {
		return nil, nil, shapeErr(deriveSdkCpp, "kApexNameToModule has no pairs")
	}
	sets := map[byte][]string{}
	for _, s := range reModSet.FindAllStringSubmatch(src, -1) {
		var mods []string
		for _, n := range reModule.FindAllStringSubmatch(s[2], -1) {
			mods = append(mods, n[1])
		}
		sets[s[1][0]] = mods
	}
	for _, d := range sdkDesserts {
		if _, ok := sets[d.set]; !ok {
			return nil, nil, shapeErr(deriveSdkCpp, fmt.Sprintf("k%cModules not found", d.set))
		}
	}
	return apex, sets, nil
}

// releaseInt reads an integer build flag of r.
func releaseInt(r *Release, name string) (int, error) {
	v := strings.TrimSpace(r.Flag(name))
	n, err := strconv.Atoi(v)
	if err != nil {
		return 0, fmt.Errorf("release %s: %s %q is not a number", r.Name, name, v)
	}
	return n, nil
}

// textpbField is a field of a protobuf text-format message: a scalar (value), or a nested message (msg; isMsg, which an empty one is too).
type textpbField struct {
	name, value string
	isMsg       bool
	msg         []textpbField
}

// parseTextpb reads the text-format subset extensions_db.textpb uses: `name: scalar`, `name { ... }` and `name: { ... }`, with `#` comments.
func parseTextpb(text string) ([]textpbField, error) {
	var toks []string
	for _, t := range reTextTok.FindAllString(text, -1) {
		if !strings.HasPrefix(t, "#") {
			toks = append(toks, t)
		}
	}
	i := 0
	return textpbFields(toks, &i, false)
}

// textpbFields parses fields from toks at *i; nested: they end at a `}` that is consumed.
func textpbFields(toks []string, i *int, nested bool) ([]textpbField, error) {
	var out []textpbField
	for *i < len(toks) {
		t := toks[*i]
		*i++
		if t == "}" {
			if !nested {
				return nil, shapeErr(extensionsDB, "unbalanced }")
			}
			return out, nil
		}
		if t == "{" || t == ":" {
			return nil, shapeErr(extensionsDB, fmt.Sprintf("unexpected %q", t))
		}
		if *i < len(toks) && toks[*i] == ":" {
			*i++
		}
		if *i >= len(toks) {
			return nil, shapeErr(extensionsDB, fmt.Sprintf("field %s has no value", t))
		}
		switch v := toks[*i]; v {
		case "{":
			*i++
			msg, err := textpbFields(toks, i, true)
			if err != nil {
				return nil, err
			}
			out = append(out, textpbField{name: t, isMsg: true, msg: msg})
		case "}", ":":
			return nil, shapeErr(extensionsDB, fmt.Sprintf("field %s has no value", t))
		default:
			*i++
			out = append(out, textpbField{name: t, value: v})
		}
	}
	if nested {
		return nil, shapeErr(extensionsDB, "unclosed {")
	}
	return out, nil
}

// parseExtensionDB reads the `versions` blocks of extensions_db.textpb: each db version and the module versions it requires.
func parseExtensionDB(text string) ([]extVersion, error) {
	fields, err := parseTextpb(text)
	if err != nil {
		return nil, err
	}
	var db []extVersion
	for _, f := range fields {
		if f.name != "versions" || !f.isMsg {
			return nil, shapeErr(extensionsDB, "top-level field "+f.name)
		}
		v, err := parseVersion(f.msg)
		if err != nil {
			return nil, err
		}
		db = append(db, v)
	}
	if len(db) == 0 {
		return nil, shapeErr(extensionsDB, "no versions block")
	}
	return db, nil
}

func parseVersion(fields []textpbField) (extVersion, error) {
	var v extVersion
	have := false
	for _, f := range fields {
		switch {
		case f.name == "version" && !f.isMsg && !have:
			n, err := strconv.Atoi(f.value)
			if err != nil {
				return v, shapeErr(extensionsDB, "version "+f.value)
			}
			v.version, have = n, true
		case f.name == "requirements" && f.isMsg:
			req, err := parseRequirement(f.msg)
			if err != nil {
				return v, err
			}
			v.reqs = append(v.reqs, req)
		default:
			return v, shapeErr(extensionsDB, "versions block field "+f.name)
		}
	}
	if !have {
		return v, shapeErr(extensionsDB, "versions block without version")
	}
	return v, nil
}

func parseRequirement(fields []textpbField) (extRequirement, error) {
	var req extRequirement
	haveModule, haveVersion := false, false
	for _, f := range fields {
		switch {
		case f.name == "module" && !f.isMsg && !haveModule:
			req.module, haveModule = f.value, true
		case f.name == "version" && f.isMsg && !haveVersion:
			if len(f.msg) != 1 || f.msg[0].name != "version" || f.msg[0].isMsg {
				return req, shapeErr(extensionsDB, "requirement version is not `version: N`")
			}
			n, err := strconv.Atoi(f.msg[0].value)
			if err != nil {
				return req, shapeErr(extensionsDB, "requirement version "+f.msg[0].value)
			}
			req.version, haveVersion = n, true
		default:
			return req, shapeErr(extensionsDB, "requirement field "+f.name)
		}
	}
	if !haveModule || !haveVersion {
		return req, shapeErr(extensionsDB, "requirement without module or version")
	}
	return req, nil
}

// shapeErr: the tree's file changed in a way these parsers do not read; the converter fails rather than guess.
func shapeErr(path, what string) error {
	return fmt.Errorf("%s changed shape: update sdkext.go (%s)", filepath.Base(path), what)
}
