// SPDX-License-Identifier: MIT

package main

import (
	"reflect"
	"testing"
)

// android/defaults.go applyDefaults prepends each defaults module in list order (proptools.PrependProperties): the last-listed defaults'
// lists come first, the module's own last; a scalar keeps the module's own value, else the first-listed defaults'. Soong's own ninja for
// android-non-updatable.stubs.module_lib.from-text (defaults module-classpath-java-defaults, then the from-text defaults) puts the second
// defaults' libs before the first's.
func TestDefaultsOrder(t *testing.T) {
	g := graphFor(t, map[string]string{"d/Android.bp": `
java_defaults { name: "a", libs: ["la"], sdk_version: "sa", stem: "ta" }
java_defaults { name: "b", libs: ["lb"], sdk_version: "sb" }
java_library { name: "m", defaults: ["a", "b"], libs: ["lm"], stem: "tm" }
`})
	p := g.resolveDefaults(g.lookup("m", Device), Device, map[string]bool{})
	if got := p.strs("libs"); !reflect.DeepEqual(got, []string{"lb", "la", "lm"}) {
		t.Errorf("libs %v, want [lb la lm]", got)
	}
	if p.str("sdk_version") != "sa" || p.str("stem") != "tm" {
		t.Errorf("sdk_version %q stem %q, want sa tm", p.str("sdk_version"), p.str("stem"))
	}
}
