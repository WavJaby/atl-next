// SPDX-License-Identifier: MIT

package main

import "testing"

// soongPermissionsHead: the license comment and root element sdkLibraryXml writes (sdk_library_internal.go:895-910), written out
// from the template strings, not produced by sdkLibraryXML.
const soongPermissionsHead = `<?xml version="1.0" encoding="utf-8"?>
<!-- Copyright (C) 2018 The Android Open Source Project

    Licensed under the Apache License, Version 2.0 (the "License");
    you may not use this file except in compliance with the License.
    You may obtain a copy of the License at

        http://www.apache.org/licenses/LICENSE-2.0

    Unless required by applicable law or agreed to in writing, software
    distributed under the License is distributed on an "AS IS" BASIS,
    WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
    See the License for the specific language governing permissions and
    limitations under the License.
-->
<permissions>
`

// org.apache.http.legacy as external/apache-http/Android.bp declares it (java_sdk_library at :46-110): no uses_libs, no
// on_bootclasspath_*, so the <library> row has a name and a file and nothing else.
func TestSdkLibraryXMLApacheHttpLegacy(t *testing.T) {
	const want = soongPermissionsHead + `    <library
        name="org.apache.http.legacy"
        file="/system/framework/org.apache.http.legacy.jar"
    />
</permissions>
`
	got, err := sdkLibraryXML("org.apache.http.legacy", newObj())
	if err != nil {
		t.Fatal(err)
	}
	if got != want {
		t.Fatalf("permission file differs from sdkLibraryXml's template:\n%s", got)
	}
}

// uses_libs becomes the dependency attribute, colon-joined, after file (formattedDependenciesAttribute :868-873).
func TestSdkLibraryXMLDependency(t *testing.T) {
	const want = soongPermissionsHead + `    <library
        name="android.test.runner"
        file="/system/framework/android.test.runner.jar"
        dependency="android.test.base:android.test.mock"
    />
</permissions>
`
	props := newObj()
	props.set("uses_libs", strVals([]string{"android.test.base", "android.test.mock"}))
	got, err := sdkLibraryXML("android.test.runner", props)
	if err != nil {
		t.Fatal(err)
	}
	if got != want {
		t.Fatalf("dependency attribute:\n%s", got)
	}
}

// An unmodelled attribute is refused, not dropped: a silently missing on-bootclasspath-before changes which apps see the library.
func TestSdkLibraryXMLRefusesUnmodelledAttribute(t *testing.T) {
	props := newObj()
	props.set("on_bootclasspath_before", "Q")
	if _, err := sdkLibraryXML("org.apache.http.legacy", props); err == nil {
		t.Fatal("on_bootclasspath_before accepted")
	}
}
