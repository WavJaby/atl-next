// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"slices"
	"strings"
)

// Turbine header jars (base.go compile + compileJavaHeader, builder.go TransformJavaToHeaderClasses / turbineFlags): every device module
// whose plugins generate no API (javaHeaders) and headers_only modules (turbineHeaders). Dependents' javac compiles against these, so a
// method-body edit stops at the edited module's turbine output (cmp + restat).

// headerSrcs: what turbine compiles for one module, the same java sources, srcjars and javac flags its javac gets; extra = compileJavaHeader's
// extraJars (kotlin header jar, extraCombinedJars such as an app's R jar).
type headerSrcs struct {
	java, srcjars, waits, flags, extra []string
	ver                                javaVersion
}

// turbine is compileJavaHeader's first half: turbine over the module's java sources and srcjars when it has any (`java <JavaVmFlags> -jar
// turbine.jar --output <jar>.tmp --sources @rsp --javacopts <CommonJdkFlags> <javacflags> -source V -target V -- --source_jars <srcjars>
// <--system dir | --bootclasspath jars> --classpath <jars>`, classpath space-separated (FormTurbineClassPath), java9 classpath first for
// Java 9+). Returns localHeaderJars: that jar, then the extra jars; sharded javac compiles against them (javacShards).
func (g *Graph) turbine(n *jNode, h headerSrcs, d javaDeps, jar string) []string {
	var local []string
	if len(h.java)+len(h.srcjars) > 0 {
		turbine := g.jtoolJar("turbine", n.name)
		boot, implicit := "", []string{turbine}
		cp := d.classpath
		if h.ver.modules() {
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
		implicit = append(append(implicit, cp...), h.srcjars...)
		out := filepath.Join(n.dir, "turbine", jar)
		g.java.edge("turbine", []string{out}, absAll(g, h.java), implicit, h.waits, [][2]string{
			{"flags", ninjaRaw(h.flags)}, {"ver", h.ver.String()}, {"srcjars", join(h.srcjars)}, {"bootcp", boot}, {"cp", join(cp)}, {"turbine", esc(turbine)},
		})
		local = append(local, out)
	}
	return append(local, h.extra...)
}

// compileJavaHeader's second half: the local header jars (turbine) always merged with the static libraries' headers into turbine-combined,
// META-INF/TRANSITIVE stripped. Returns that jar.
func (g *Graph) compileJavaHeader(n *jNode, local []string, d javaDeps, jar string, validations []string) string {
	// depset PREORDER of the local header jars and TransitiveStaticLibsHeaderJars (a repackaged lib contributes both of its headers)
	header := filepath.Join(n.dir, "turbine-combined", jar)
	g.java.edgeV("combine", []string{header}, dedupKeep(append(slices.Clone(local), d.staticHeaders...)), nil, nil, validations, [][2]string{
		{"args", "-stripDir META-INF/TRANSITIVE -stripFile module-info.class -stripFile 'META-INF/versions/*/module-info.class'"}})
	return header
}

// turbineHeaders is headers_only: the turbine header through jarjar_rules ("turbine") and the inherited rename rules ("repackage-turbine")
// is the module's header and implementation alike; nothing else is compiled.
func (g *Graph) turbineHeaders(n *jNode, h headerSrcs, d javaDeps, jar, repackage string) {
	header := g.compileJavaHeader(n, g.turbine(n, h, d, jar), d, jar, nil)
	if rules := g.jarjarRulesFile(n); rules != "" {
		jj := filepath.Join(n.dir, "jarjar", "turbine", jar)
		g.jarjar(n, jj, header, rules, 1)
		header = jj
	}
	header = g.repackage(n, header, repackage, "repackage-turbine", jar)
	n.header, n.impl, n.implRes = header, []string{header}, header
}

// turbineRule: the rule of compileJavaHeader (cwd the tree root, sources tree-relative as javac's); cmp + restat as Soong's turbine rule.
func (j *javaGraph) turbineRule() string {
	c := j.cfg
	return "rule turbine\n  command = cd $root && $jdk/bin/java " + strings.Join(c.JavaVmFlags, " ") + " -jar $turbine --output $out.tmp --sources @$out.rsp --javacopts " +
		strings.Join(c.CommonJdkFlags, " ") + " $flags -source $ver -target $ver -- --source_jars $srcjars $bootcp --classpath $cp && " + commitIfChanged + "\n" +
		"  rspfile = $out.rsp\n  rspfile_content = $in_newline\n  restat = 1\n  description = turbine $out\n"
}
