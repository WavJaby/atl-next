// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"sort"
	"strings"
)

// java_system_features_srcs: build/soong/systemfeatures/system_features.go (the spec), a source generator built here as the genrule it amounts to:
// `systemfeatures-gen-tool <full_class_name> --feature=<K>:<V>… --readonly=<RELEASE_USE_SYSTEM_FEATURE_BUILD_FLAGS> --metadata-only=<metadata_only>`
// into <class name>.java, one --feature per RELEASE_SYSTEM_FEATURE_<K> build flag (sorted "<K>:<V>", bool flags "true" / "" as BuildFlags holds
// them). Soong's `rm -rf` of its gen dir is dropped: the gen dir holds the rule's script.

const sysFeaturesType = "java_system_features_srcs"

// sysFeaturesProps: the genrule properties of a java_system_features_srcs module.
func (g *Graph) sysFeaturesProps(n *Node) *Obj {
	p := n.props
	class := p.str("full_class_name")
	parts := strings.Split(class, ".")
	out := parts[len(parts)-1] + ".java"
	var features []string
	readonly := "false"
	if g.release != nil {
		for _, k := range g.release.Names("RELEASE_SYSTEM_FEATURE_") {
			features = append(features, fmt.Sprintf("--feature=%s:%s", strings.TrimPrefix(k, "RELEASE_SYSTEM_FEATURE_"), g.release.Flag(k)))
		}
		readonly = fmt.Sprint(g.release.Bool("RELEASE_USE_SYSTEM_FEATURE_BUILD_FLAGS"))
	} else {
		g.problem("%s: java_system_features_srcs needs the release config (-release)", n.mod.Name)
	}
	sort.Strings(features)
	cmd := []string{"$(location systemfeatures-gen-tool)", shellEscape(class)}
	for _, f := range features {
		cmd = append(cmd, shellEscape(f))
	}
	cmd = append(cmd, "--readonly="+readonly, fmt.Sprintf("--metadata-only=%v", p.boolean("metadata_only", false)), ">", "$(out)")
	props := newObj()
	props.set("tools", strVals([]string{"systemfeatures-gen-tool"}))
	props.set("cmd", strings.Join(cmd, " "))
	props.set("out", strVals([]string{out}))
	return props
}
