// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"strings"
)

// Turbine for headers_only modules only (base.go compile: headers_only needs Turbine; every other module takes the javac path, J3), as
// builder.go TransformJavaToHeaderClasses: `java <JavaVmFlags> -jar turbine.jar --output <jar>.tmp --sources @rsp --javacopts
// <CommonJdkFlags> <javacflags> -source V -target V -- --source_jars <srcjars> <--system dir | --bootclasspath jars> --classpath <jars>`,
// the classpath space-separated (FormTurbineClassPath), java9 classpath first for Java 9+; compileJavaHeader then merges it with the static
// libraries' header jars (here their combined jars) into turbine-combined. The header jar then goes through jarjar_rules and
// the inherited rename rules as the javac path's would (jarjarIfNecessary "turbine", repackageFlagsIfNecessary "repackage-turbine"); it is
// the module's header and implementation alike.

func (g *Graph) turbineHeaders(n *jNode, java, kt, srcjars, waits, flags []string, ver javaVersion, d javaDeps, jar, repackage string) {
	if len(kt) > 0 {
		g.problem("%s: compiling headers_only with .kt not supported", n.name)
		return
	}
	turbine := g.jtoolJar("turbine", n.name)
	boot, implicit := "", []string{turbine}
	cp := d.classpath
	if ver.modules() {
		switch {
		case d.sys != nil:
			boot = "--system " + esc(d.sys.sysDir)
			implicit = append(implicit, d.sys.sysDeps...)
		case n.side == Device:
			boot = `--bootclasspath ""`
		}
		cp = append(append([]string{}, d.java9...), cp...)
	} else {
		implicit = append(implicit, d.boot...)
		if len(d.boot) == 0 && n.side == Device {
			boot = `--bootclasspath ""`
		} else if len(d.boot) > 0 {
			boot = "--bootclasspath " + join(d.boot)
		}
	}
	implicit = append(append(implicit, cp...), srcjars...)
	out := filepath.Join(n.dir, "turbine", jar)
	g.java.edge("turbine", []string{out}, absAll(g, java), implicit, waits, [][2]string{
		{"flags", ninjaRaw(flags)}, {"ver", ver.String()}, {"srcjars", join(srcjars)}, {"bootcp", boot}, {"cp", join(cp)}, {"turbine", esc(turbine)},
	})
	// compileJavaHeader: always combined with the static libraries' header jars (TransitiveStaticLibsHeaderJars: a repackaged lib
	// contributes both of its headers) into turbine-combined, META-INF/TRANSITIVE stripped
	header := filepath.Join(n.dir, "turbine-combined", jar)
	g.java.edge("combine", []string{header}, dedupKeep(append([]string{out}, d.staticHeaders...)), nil, nil, [][2]string{
		{"args", "-stripDir META-INF/TRANSITIVE -stripFile module-info.class -stripFile 'META-INF/versions/*/module-info.class'"}})
	if rules := g.jarjarRulesFile(n); rules != "" {
		jj := filepath.Join(n.dir, "jarjar", "turbine", jar)
		g.jarjar(n, jj, header, rules, 1)
		header = jj
	}
	header = g.repackage(n, header, repackage, "turbine", jar)
	n.header, n.impl, n.implRes = header, []string{header}, header
}

// turbineRule: the rule of turbineHeaders (cwd the tree root, sources tree-relative as javac's); cmp + restat as Soong's turbine rule.
func (j *javaGraph) turbineRule() string {
	c := j.cfg
	return "rule turbine\n  command = cd $root && $jdk/bin/java " + strings.Join(c.JavaVmFlags, " ") + " -jar $turbine --output $out.tmp --sources @$out.rsp --javacopts " +
		strings.Join(c.CommonJdkFlags, " ") + " $flags -source $ver -target $ver -- --source_jars $srcjars $bootcp --classpath $cp && " + commitIfChanged + "\n" +
		"  rspfile = $out.rsp\n  rspfile_content = $in_newline\n  restat = 1\n  description = turbine $out\n"
}
