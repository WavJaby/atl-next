// SPDX-License-Identifier: MIT

package main

import (
	"strings"
	"testing"
)

// A cc_test with gtest: false is a binary (dEQP's deqp-binary); a gtest one stays unsupported, since its gtest libraries and flags are not modelled.
func TestCcTest(t *testing.T) {
	g := graphFor(t, map[string]string{"Android.bp": `
cc_defaults { name: "nogtest", gtest: false }
cc_test { name: "plain", gtest: false, srcs: ["a.cpp"] }
cc_test { name: "viadefaults", defaults: ["nogtest"], srcs: ["a.cpp"] }
cc_test { name: "gtested", srcs: ["a.cpp"] }
`})
	for _, name := range []string{"plain", "viadefaults"} {
		if n := g.node(name, Device, Binary, "goal"); n == nil || n.kind != Binary {
			t.Errorf("%s: node %v, problems %q", name, n, g.problems)
		}
	}
	if n := g.node("gtested", Device, Binary, "goal"); n != nil {
		t.Errorf("gtest cc_test built as %v", n.kind)
	}
	if len(g.problems) != 1 || !strings.Contains(g.problems[0], "gtested is a gtest cc_test") {
		t.Errorf("problems = %q", g.problems)
	}
}
