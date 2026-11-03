// SPDX-License-Identifier: MIT

package main

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

// Expected values follow GNU make semantics on the fixture (a cut-down envsetup.mk + default_art_config.mk with the same constructs):
// `:=` replaces, `+=` appends, ifeq/ifneq compare expanded operands, `else` takes the other branch, a bool release flag that is false
// expands to "", a soong_config_set call continued into a list expands to nothing. Then product_config.mk's steps: core-icu4j moved to
// the end of the platform jars, APEX jars sorted; then derive_classpath's order: ART jars, platform jars, APEX jars.

const testEnvsetup = `define f
ART_APEX_JARS := wrong
endef
ifeq ($(HOST_OS),linux)
X := 1
endif
ART_APEX_JARS := \
    com.android.art:core-oj \
    com.android.art:core-libart
# comment
ifeq (true,$(EMMA_INSTRUMENT_FRAMEWORK))
  ART_APEX_JARS += com.android.art:jacocoagent
endif
`

const testArtConfig = `ifeq ($(ART_APEX_JARS),)
  $(error ART_APEX_JARS is empty)
endif
PRODUCT_BOOT_JARS := \
    $(ART_APEX_JARS)
PRODUCT_BOOT_JARS += \
    framework-minus-apex \
    ext
PRODUCT_APEX_BOOT_JARS := \
    com.android.tethering:framework-tethering \
    com.android.i18n:core-icu4j \
    com.android.bt:framework-bluetooth \

ifeq ($(RELEASE_ON),true)
    PRODUCT_APEX_BOOT_JARS += \
        com.android.crashrecovery:framework-crashrecovery \

else
    PRODUCT_BOOT_JARS += \
        framework-platformcrashrecovery \

endif
ifeq ($(RELEASE_OFF),true)
    PRODUCT_APEX_BOOT_JARS += com.android.x:off
else
    PRODUCT_BOOT_JARS += off-platform
endif
ifeq ($(RELEASE_STACK),NfcNci)
    PRODUCT_BOOT_JARS += framework-nfc
else
    PRODUCT_APEX_BOOT_JARS += \
        com.android.nfcservices:framework-nfc
    $(call soong_config_set,bootclasspath,nfc_apex_bootclasspath_fragment,true)
endif
ifneq (,$(RELEASE_RANGING))
    PRODUCT_APEX_BOOT_JARS += \
        com.android.uwb:framework-ranging \
    $(call soong_config_set,bootclasspath,release_ranging_stack,true)
endif
ifneq (,$(wildcard art))
  OTHER := x
endif
`

func TestBootJars(t *testing.T) {
	tree := t.TempDir()
	write := func(path, body string) {
		p := filepath.Join(tree, path)
		if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(p, []byte(body), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	write(artBootJarsMk, testEnvsetup)
	write(productBootJarsMk, testArtConfig)
	write(productConfigMk, "PRODUCT_APEX_BOOT_JARS := $(filter-out com.android.i18n:core-icu4j,$(PRODUCT_APEX_BOOT_JARS))\n")
	rel := &Release{flags: map[string]string{"RELEASE_ON": "true", "RELEASE_OFF": "", "RELEASE_STACK": "com.android.nfcservices", "RELEASE_RANGING": "true"}}
	vars := map[string][]string{"ART_APEX_JARS": nil, "PRODUCT_BOOT_JARS": nil, "PRODUCT_APEX_BOOT_JARS": nil}
	for _, f := range []string{artBootJarsMk, productBootJarsMk} {
		if err := readMakeLists(filepath.Join(tree, f), vars, rel); err != nil {
			t.Fatal(err)
		}
	}
	boot, apex, err := bootJarsFinal(tree, vars)
	if err != nil {
		t.Fatal(err)
	}
	var got []string
	for _, b := range append(boot, apex...) {
		got = append(got, b.String())
	}
	want := []string{
		"com.android.art:core-oj", "com.android.art:core-libart",
		"platform:framework-minus-apex", "platform:ext", "platform:off-platform", "com.android.i18n:core-icu4j",
		"com.android.bt:framework-bluetooth", "com.android.crashrecovery:framework-crashrecovery", "com.android.nfcservices:framework-nfc",
		"com.android.tethering:framework-tethering", "com.android.uwb:framework-ranging",
	}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("boot jars\n got %v\nwant %v", got, want)
	}

	// a list assigned under a condition the reader cannot evaluate is an error, not a guess
	write("u.mk", "ifeq ($(UNKNOWN),x)\nPRODUCT_BOOT_JARS += y\nendif\n")
	if err := readMakeLists(filepath.Join(tree, "u.mk"), vars, rel); err == nil || !strings.Contains(err.Error(), "cannot evaluate") {
		t.Errorf("unknown condition: got %v", err)
	}
	// $(error) on a taken branch fails
	empty := map[string][]string{"ART_APEX_JARS": nil, "PRODUCT_BOOT_JARS": nil, "PRODUCT_APEX_BOOT_JARS": nil}
	if err := readMakeLists(filepath.Join(tree, productBootJarsMk), empty, rel); err == nil || !strings.Contains(err.Error(), "ART_APEX_JARS is empty") {
		t.Errorf("$(error): got %v", err)
	}
	// product_config.mk without the core-icu4j move: the mirror is stale
	write(productConfigMk, "\n")
	if _, _, err := bootJarsFinal(tree, vars); err == nil {
		t.Error("stale product_config.mk mirror not detected")
	}
}
