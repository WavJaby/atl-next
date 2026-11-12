// SPDX-License-Identifier: MIT

package main

import "testing"

// TestSoongExpandEscapes: variable-free strings come back as Ninja would read them. Expected values follow Ninja's escapes ("$$" -> "$") and blueprint's parser, which keeps a leading space (Soong's riscv64 ToolchainCflags is " ") by guarding it as "$ " or a nil-variable reference.
func TestSoongExpandEscapes(t *testing.T) {
	for _, c := range []struct{ in, want string }{
		{"", ""},
		{"-a -b", "-a -b"},
		{" ", " "},
		{" -a", " -a"},
		{"$$x", "$x"},
		{"a$$$$b", "a$$b"},
		{" $$x", " $x"},
	} {
		got, err := soongExpand(soongCcConfig, c.in)
		if err != nil {
			t.Errorf("%q: %v", c.in, err)
		} else if got != c.want {
			t.Errorf("%q -> %q, want %q", c.in, got, c.want)
		}
	}
}
