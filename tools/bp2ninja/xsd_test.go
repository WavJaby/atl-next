// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// xsdc.go GenerateAndroidBuildActions: a cc module's generated_sources take the C++ rule's outputs (`-o cpp -c <args>`, args as the Java rule's), its header dir cpp/include;
// srcs elsewhere still name the Java srcjar; enums_only / parser_only keep one of the two output pairs.
func TestXsdCpp(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"system/tools/xsdc/Android.bp": `java_binary_host { name: "xsdc", srcs: ["X.java"] }`,
		"system/tools/xsdc/X.java":     "",
		"p/Android.bp": `
xsd_config { name: "apex-tiny", srcs: ["A.xsd"], package_name: "com.android.apex", gen_writer: true, tinyxml: true, root_elements: ["apex-info-list"] }
xsd_config { name: "enums", srcs: ["E.xsd"], package_name: "a.b", enums_only: true }
xsd_config { name: "parser", srcs: ["P.xsd"], package_name: "a.c", parser_only: true }
cc_library_static { name: "libvintf_like", srcs: ["v.cpp"], generated_sources: ["apex-tiny"], generated_headers: ["enums", "parser"] }
`,
		"p/A.xsd": "", "p/E.xsd": "", "p/P.xsd": "", "p/v.cpp": "",
	}, nil)
	n := g.node("libvintf_like", Device, Static, "goal")
	if n == nil || len(g.problems) > 0 {
		t.Fatal(g.problems)
	}
	gen := func(m string) string { return filepath.Join(g.out, "gen/device", m) }
	cpp := g.xsdCpp[key{"apex-tiny", Device, Gen}]
	if cpp == nil {
		t.Fatal("no C++ node for apex-tiny")
	}
	wantOuts := []string{gen("apex-tiny") + "/cpp/com_android_apex.cpp", gen("apex-tiny") + "/cpp/com_android_apex_enums.cpp",
		gen("apex-tiny") + "/cpp/include/com_android_apex.h", gen("apex-tiny") + "/cpp/include/com_android_apex_enums.h"}
	if !slices.Equal(cpp.rule.outs, wantOuts) || cpp.rule.broken != "" {
		t.Errorf("outs %q broken %q", cpp.rule.outs, cpp.rule.broken)
	}
	script := g.genScript(cpp)
	if !strings.Contains(script, " p/A.xsd -p com.android.apex -o "+gen("apex-tiny")+"/cpp -c -w -t -r apex-info-list\n") || cpp.rule.script != gen("apex-tiny")+"/cpp.sh" {
		t.Errorf("script %s: %q", cpp.rule.script, script)
	}
	// compiled: the two .cpp, never the headers; their dir on the include path
	var compiled []string
	for _, s := range n.srcs {
		if strings.HasPrefix(s, gen("")) {
			compiled = append(compiled, s)
		}
	}
	if !slices.Equal(compiled, wantOuts[:2]) {
		t.Errorf("compiled %q", compiled)
	}
	if !slices.Contains(n.genIncs, "-I"+gen("apex-tiny")+"/cpp/include") {
		t.Errorf("include flags %q", n.genIncs)
	}
	if e := g.xsdCpp[key{"enums", Device, Gen}]; !slices.Equal(e.rule.outs, []string{gen("enums") + "/cpp/a_b_enums.cpp", gen("enums") + "/cpp/include/a_b_enums.h"}) {
		t.Errorf("enums_only outs %q", e.rule.outs)
	}
	if p := g.xsdCpp[key{"parser", Device, Gen}]; !slices.Equal(p.rule.outs, []string{gen("parser") + "/cpp/a_c.cpp", gen("parser") + "/cpp/include/a_c.h"}) {
		t.Errorf("parser_only outs %q", p.rule.outs)
	}
	// the module's own node is still the Java srcjar, what srcs name
	if j := g.genNode(":apex-tiny", Device, "t"); !slices.Equal(j.rule.outs, []string{gen("apex-tiny") + "/java/com_android_apex_xsdcgen.srcjar"}) {
		t.Errorf("java outs %q", j.rule.outs)
	}
}
