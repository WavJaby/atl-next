// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
)

// JavaConfig: the global Java tool flags Soong applies, read from the pinned tree's build/soong/java/config/{config,droidstubs}.go
// (as SoongConfig reads cc's global.go), so an AOSP upgrade brings its own values. Each field is the expansion of the named Soong
// variable: a string slice literal, `strings.Join([]string{…}, " ")`, or `strings.Join(append([]string{…}, list...), " ")`.
type JavaConfig struct {
	CommonJdkFlags  []string // CommonJdkFlags: every javac
	JavacHeapSize   string   // JavacHeapSize: javac -J-Xmx
	JavacVmFlags    []string // javacVmFlagsList: javac and metalava JVM flags (-J prefixed)
	JavaVmFlags     []string // javaVmFlagsList: plain `java` invocations (jarjar)
	D8Flags         []string // D8Flags: d8 wrapper JVM flags (-J prefixed, wrapper syntax)
	MetalavaFlags   []string // metalavaFlags
	MetalavaAnno    []string // metalavaAnnotationsFlags
	MetalavaAnnoW   []string // metalavaAnnotationsWarningsFlags
	MetalavaAddOpen string   // MetalavaAddOpens
	JlinkVersion    string   // java.base module version: the JDK's own major version (system_modules.go: must match the jlink consuming it)
}

func readJavaConfig(tree, jdk string) (JavaConfig, error) {
	lists := map[string][]string{}
	strs := map[string]string{}
	for _, f := range []string{"build/soong/java/config/config.go", "build/soong/java/config/droidstubs.go"} {
		if err := goConstants(filepath.Join(tree, f), lists, strs); err != nil {
			return JavaConfig{}, err
		}
	}
	c := JavaConfig{
		CommonJdkFlags:  lists["CommonJdkFlags"],
		JavacHeapSize:   strs["JavacHeapSize"],
		JavacVmFlags:    lists["javacVmFlagsList"],
		JavaVmFlags:     lists["javaVmFlagsList"],
		D8Flags:         lists["D8Flags"],
		MetalavaFlags:   lists["metalavaFlags"],
		MetalavaAnno:    lists["metalavaAnnotationsFlags"],
		MetalavaAnnoW:   lists["metalavaAnnotationsWarningsFlags"],
		MetalavaAddOpen: strs["MetalavaAddOpens"],
	}
	for name, v := range map[string]bool{"CommonJdkFlags": len(c.CommonJdkFlags) > 0, "JavacHeapSize": c.JavacHeapSize != "",
		"javacVmFlagsList": len(c.JavacVmFlags) > 0, "javaVmFlagsList": len(c.JavaVmFlags) > 0, "D8Flags": len(c.D8Flags) > 0,
		"metalavaFlags": len(c.MetalavaFlags) > 0, "MetalavaAddOpens": c.MetalavaAddOpen != ""} {
		if !v {
			return c, fmt.Errorf("build/soong/java/config: %s not found", name)
		}
	}
	rel, err := os.ReadFile(filepath.Join(jdk, "release"))
	if err != nil {
		return c, err
	}
	m := regexp.MustCompile(`(?m)^JAVA_VERSION="(\d+)`).FindSubmatch(rel)
	if m == nil {
		return c, fmt.Errorf("%s/release: no JAVA_VERSION", jdk)
	}
	c.JlinkVersion = string(m[1])
	return c, nil
}

// goConstants collects string-slice and string values from one Go file: `var x = []string{…}`, `x = "…"`, and
// `pctx.StaticVariable("Name", <expr>)`. Slices are stored as their elements; a joined string (strings.Join) also as a slice,
// split on the join separator " ", which is how Soong's flags are joined and later split by the shell.
func goConstants(path string, lists map[string][]string, strs map[string]string) error {
	f, err := parser.ParseFile(token.NewFileSet(), path, nil, 0)
	if err != nil {
		return err
	}
	var eval func(e ast.Expr) ([]string, bool)
	eval = func(e ast.Expr) ([]string, bool) {
		switch x := e.(type) {
		case *ast.BasicLit:
			if x.Kind == token.STRING {
				s, err := strconv.Unquote(x.Value)
				return []string{s}, err == nil
			}
		case *ast.Ident:
			l, ok := lists[x.Name]
			return l, ok
		case *ast.CompositeLit:
			var out []string
			for _, el := range x.Elts {
				v, ok := eval(el)
				if !ok {
					return nil, false
				}
				out = append(out, v...)
			}
			return out, true
		case *ast.CallExpr:
			name := callName(x)
			switch {
			case name == "strings.Join" && len(x.Args) == 2:
				l, ok := eval(x.Args[0])
				if !ok {
					return nil, false
				}
				// one flag per element, as written; an element may hold "flag value" (split by the shell later)
				return l, true
			case name == "append" && len(x.Args) >= 1:
				var out []string
				for _, a := range x.Args {
					v, ok := eval(a)
					if !ok {
						return nil, false
					}
					out = append(out, v...)
				}
				return out, true
			}
		}
		return nil, false
	}
	record := func(name string, e ast.Expr) {
		v, ok := eval(e)
		if !ok {
			return
		}
		if _, isLit := e.(*ast.BasicLit); isLit {
			strs[name] = v[0]
			return
		}
		lists[name] = v
	}
	// declaration order: slices must be known before the joins that append them
	ast.Inspect(f, func(n ast.Node) bool {
		switch x := n.(type) {
		case *ast.ValueSpec:
			for i, name := range x.Names {
				if i < len(x.Values) {
					record(name.Name, x.Values[i])
				}
			}
		case *ast.AssignStmt:
			if len(x.Lhs) == len(x.Rhs) {
				for i, l := range x.Lhs {
					if id, ok := l.(*ast.Ident); ok {
						record(id.Name, x.Rhs[i])
					}
				}
			}
		case *ast.CallExpr:
			if callName(x) == "pctx.StaticVariable" && len(x.Args) == 2 {
				if lit, ok := x.Args[0].(*ast.BasicLit); ok {
					if name, err := strconv.Unquote(lit.Value); err == nil {
						record(name, x.Args[1])
					}
				}
			}
		}
		return true
	})
	return nil
}

func callName(c *ast.CallExpr) string {
	switch f := c.Fun.(type) {
	case *ast.Ident:
		return f.Name
	case *ast.SelectorExpr:
		if id, ok := f.X.(*ast.Ident); ok {
			return id.Name + "." + f.Sel.Name
		}
	}
	return ""
}
