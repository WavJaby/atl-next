// SPDX-License-Identifier: MIT

package main

import (
	"strings"
	"testing"
)

// ruleText: the body of `rule name` in ninja text, up to the next top-level line.
func ruleText(ninja, name string) string {
	_, after, ok := strings.Cut(ninja, "rule "+name+"\n")
	if !ok {
		return ""
	}
	var body []string
	for _, line := range strings.Split(after, "\n") {
		if !strings.HasPrefix(line, "  ") {
			break
		}
		body = append(body, strings.TrimSpace(line))
	}
	return strings.Join(body, "\n")
}

// Rule params mirrored from Soong's registered rules (build/soong at the sources.toml pin): the expected text is Soong's, cited per case, never our output.
func TestRulesMirrorSoong(t *testing.T) {
	g := javaTestGraph(t, map[string]string{"kt/Android.bp": kotlinStdlib, "kt/s.jar": "", "kt/s7.jar": "", "kt/s8.jar": "", "kt/a.jar": ""}, nil)
	cases := []struct {
		rule, text string
		want       []string
	}{
		// cc/builder.go ld: Restat true
		{"link", header, []string{"restat = 1"}},
		// cc/builder.go ar: "rm -f ${out} && $arCmd $arFlags $out @${out}.rsp", Rspfile ${out}.rsp, RspfileContent ${in}
		{"ar", header, []string{"command = rm -f $out && $ar crsD $out @$out.rsp", "rspfile = $out.rsp", "rspfile_content = $in"}},
		// cc/builder.go noAddrSig: "rm -f ${out} && $objcopyCmd --remove-section=.llvm_addrsig ${in} ${out}"
		{"no_addrsig", header, []string{"command = rm -f $out && $objcopy --remove-section=.llvm_addrsig $in $out"}},
		// cc/gen.go lex: "M4=$m4Cmd $lexCmd $flags -o$out $in" (module lex.flags: not supported, gensrc.go)
		{"lex", header, []string{"command = M4=$m4 $flex -o$out $in"}},
		// java/kotlin.go kotlinc: both soong_zip calls -write_if_changed, Restat true
		{"kotlinc", g.java.kotlincRule(g), []string{"-C $classes -D $classes -write_if_changed &&", "-C $hclasses -D $hclasses -write_if_changed &&", "restat = 1"}},
	}
	for _, c := range cases {
		body := ruleText(c.text, c.rule)
		if body == "" {
			t.Errorf("no rule %s", c.rule)
			continue
		}
		for _, w := range c.want {
			if !strings.Contains(body, w) {
				t.Errorf("rule %s lacks %q:\n%s", c.rule, w, body)
			}
		}
	}
}
