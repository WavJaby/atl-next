// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"strings"
)

// sdkLibXmlDir: the permission files of the platform shared libraries the goals install, under -out. Each is installed at
// /system/etc/permissions/<lib>.xml by the runtime (atld_run.rs platform_rows), where SystemConfig reads it (system_config.rs).
const sdkLibXmlDir = "gen/etc/permissions"

// sdkLibraryXML: sdkLibraryXml.permissionsContents (build/soong/java/sdk_library_internal.go:875-922) for a java_sdk_library
// that is a shared library: the <library> row with its implementation path /system/framework/<lib>.jar (implPath :815-833, platform
// partition) and the uses_libs dependency. on_bootclasspath_* and min/max_device_sdk are not modelled: a goal naming one is an error.
func sdkLibraryXML(lib string, props *Obj) (string, error) {
	for _, k := range []string{"on_bootclasspath_since", "on_bootclasspath_before", "min_device_sdk", "max_device_sdk"} {
		if props.has(k) {
			return "", fmt.Errorf("%s: %s is not modelled in the permission file (sdk_library_internal.go permissionsContents)", lib, k)
		}
	}
	dependency := ""
	if deps := props.strs("uses_libs"); len(deps) > 0 {
		dependency = fmt.Sprintf("        dependency=\"%s\"\n", strings.Join(deps, ":"))
	}
	return strings.Join([]string{
		"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
		"<!-- Copyright (C) 2018 The Android Open Source Project\n",
		"\n",
		"    Licensed under the Apache License, Version 2.0 (the \"License\");\n",
		"    you may not use this file except in compliance with the License.\n",
		"    You may obtain a copy of the License at\n",
		"\n",
		"        http://www.apache.org/licenses/LICENSE-2.0\n",
		"\n",
		"    Unless required by applicable law or agreed to in writing, software\n",
		"    distributed under the License is distributed on an \"AS IS\" BASIS,\n",
		"    WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.\n",
		"    See the License for the specific language governing permissions and\n",
		"    limitations under the License.\n",
		"-->\n",
		"<permissions>\n",
		"    <library\n",
		fmt.Sprintf("        name=\"%s\"\n", lib),
		fmt.Sprintf("        file=\"/system/framework/%s.jar\"\n", lib),
		dependency,
		"    />\n",
		"</permissions>\n",
	}, ""), nil
}

// sdkLibraryXMLs writes the permission file of each shared java_sdk_library whose implementation library a goal names (<lib>.impl).
// Soong installs the file as a Required of the library's module (sdk_library.go AndroidMkEntries :1617-1622), so a build that
// carries the implementation carries its file.
func (g *Graph) sdkLibraryXMLs(out string, goals []string) {
	for _, goal := range goals {
		lib, ok := strings.CutSuffix(goal, ".impl")
		if !ok {
			continue
		}
		m := g.lookupJava(lib, Device)
		if m == nil || g.baseType(m.Type) != "java_sdk_library" || !m.Props.boolean("shared_library", true) {
			continue
		}
		xml, err := sdkLibraryXML(lib, m.Props)
		if err != nil {
			g.problem("%v", err)
			continue
		}
		g.genFile(filepath.Join(out, sdkLibXmlDir, lib+".xml"), xml)
	}
}
