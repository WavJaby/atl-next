// SPDX-License-Identifier: MIT

package main

import (
	"reflect"
	"strings"
	"testing"
)

// Expected values follow Soong: a python_binary_host's par puts its own and its python_library_host libs' sources (no pkg_path) at the
// top level (python.go getPkgPath), so they run as is when they share main's directory; a Java module's output tags are base.go
// setOutputFiles: {.jar} = implementation and resources jar, {.hjar} = header jar; {.generated_srcjars} = a java_aconfig_library's srcjar.

func TestPythonToolLibs(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"s/Android.bp": `
python_binary_host { name: "fixer", main: "fixer.py", srcs: ["fixer.py"], libs: ["utils"] }
python_library_host { name: "utils", srcs: ["manifest.py"] }
python_binary_host { name: "far", main: "far.py", srcs: ["far.py"], libs: ["other"] }
python_library_host { name: "other", srcs: ["sub/x.py"] }
python_binary_host { name: "pkg", main: "pkg.py", srcs: ["pkg.py"], libs: ["withpkg"] }
python_library_host { name: "withpkg", srcs: ["y.py"], pkg_path: "p" }
`,
		"s/fixer.py": "", "s/manifest.py": "", "s/far.py": "", "s/sub/x.py": "", "s/pkg.py": "", "s/y.py": "",
	}, nil)
	files, why := pythonTool(g, g.lookup("fixer", Host))
	if want := []string{"s/fixer.py", "s/manifest.py"}; why != "" || !reflect.DeepEqual(files, want) {
		t.Errorf("fixer: %v %q, want %v", files, why, want)
	}
	if _, why := pythonTool(g, g.lookup("far", Host)); !strings.Contains(why, "outside main's directory") {
		t.Errorf("far: %q", why)
	}
	if _, why := pythonTool(g, g.lookup("pkg", Host)); !strings.Contains(why, "pkg_path") {
		t.Errorf("pkg: %q", why)
	}
}

func TestJavaOutputTags(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"j/Android.bp": `java_library { name: "lib", srcs: ["A.java"], sdk_version: "none", system_modules: "none" }`,
		"j/A.java":     "",
	}, nil)
	n := g.jnode("lib", Device, "test")
	if n == nil {
		t.Fatalf("no node: %v", g.problems)
	}
	for tag, want := range map[string]string{".jar": n.implRes, ".hjar": n.header} {
		files, ok := g.resolveSrc("j", Device, ":lib{"+tag+"}", "test")
		if !ok || len(files) != 1 || files[0].path != want || want == "" {
			t.Errorf("%s: %+v ok %v, want %s", tag, files, ok, want)
		}
	}
	// no generated srcjar: a problem, not a silent empty source
	if _, ok := g.resolveSrc("j", Device, ":lib{.generated_srcjars}", "test"); ok {
		t.Error("{.generated_srcjars} of a plain library resolved")
	}
}

// Untagged `:name` of a Java module = base.go outputFile: the dex jar when it dexes (java_binary is installable), else the implementation and resources jar.
// Break: without the case a genrule src `:deployagent` (adb's bin2c_fastdeployagent) is unresolved.
func TestJavaUntaggedOutput(t *testing.T) {
	g := javaTestGraph(t, map[string]string{
		"j/Android.bp": `
java_library { name: "lib", srcs: ["A.java"], sdk_version: "none", system_modules: "none" }
java_binary { name: "bin", srcs: ["A.java"], sdk_version: "none", system_modules: "none" }
`,
		"j/A.java": "",
	}, nil)
	lib, bin := g.jnode("lib", Device, "test"), g.jnode("bin", Device, "test")
	if lib == nil || bin == nil || bin.dex == "" {
		t.Fatalf("lib %v, bin %v: %v", lib, bin, g.problems)
	}
	for name, want := range map[string]string{"lib": lib.implRes, "bin": bin.dex} {
		files, ok := g.resolveSrc("j", Device, ":"+name, "test")
		if !ok || len(files) != 1 || files[0].path != want {
			t.Errorf(":%s: %+v ok %v, want %s", name, files, ok, want)
		}
	}
}
