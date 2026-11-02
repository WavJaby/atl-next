// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"strings"
)

// The SDK's preprocessed aidl, build/soong/java/sdk.go createSdkFrameworkAidl / createNonUpdatableFrameworkAidl (the spec): the Parcelables
// of SDK stubs header jars, each `sdkparcelables <jar> <out>/aidl/<jar name>.aidl --guarantee_stable`, concatenated through `sort -u`.
// framework.aidl takes android_stubs_current, android_test_stubs_current and android_system_stubs_current; framework_non_updatable.aidl
// android_module_lib_stubs_current. A java module building .aidl sources against a module SDK reads it with -p (decodeSdkDep's aidl).

var sdkAidlStubs = map[string][]string{
	"framework.aidl":               {"android_stubs_current", "android_test_stubs_current", "android_system_stubs_current"},
	"framework_non_updatable.aidl": {"android_module_lib_stubs_current"},
}

// sdkFrameworkAidl: the absolute path of the named SDK aidl file, its edges written once; "" with a problem when a stubs module is missing.
func (g *Graph) sdkFrameworkAidl(name, from string) string {
	j := g.java
	out := filepath.Join(g.out, name)
	if done, ok := j.sdkAidl[name]; ok {
		return done
	}
	if j.sdkAidl == nil {
		j.sdkAidl = map[string]string{}
	}
	j.sdkAidl[name] = ""
	tool := g.jtool("sdkparcelables", from)
	var parts []string
	for _, m := range sdkAidlStubs[name] {
		n := g.jnode(m, Device, from+" ("+name+")")
		if n == nil || n.header == "" {
			g.problem("%s: %s needs %s, which provides no header jar", from, name, m)
			return ""
		}
		part := filepath.Join(g.out, "aidl", strings.TrimSuffix(filepath.Base(n.header), ".jar")+".aidl")
		j.edge("sdkparcelables", []string{part}, []string{n.header}, []string{tool}, nil, [][2]string{{"tool", esc(tool)}})
		parts = append(parts, part)
	}
	j.edge("sortu", []string{out}, parts, nil, nil, nil)
	j.sdkAidl[name] = out
	return out
}

const sdkAidlRules = `rule sdkparcelables
  command = rm -f $out && $tool $in $out --guarantee_stable
  description = sdkparcelables $out
rule sortu
  command = rm -f $out && cat $in | sort -u > $out
  description = sort -u $out
`
