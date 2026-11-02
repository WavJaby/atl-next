// SPDX-License-Identifier: MIT

package main

import (
	"path/filepath"
	"strings"
)

// Kotlin sources, following build/soong/java/kotlin.go kotlinCompile (spec) and base.go compile's Kotlin branch, with the pinned prebuilt
// compiler external/kotlinc (build-only, J2).
//   - kotlinc compiles the .kt sources against bootclasspath + classpath, reading the module's .java sources and srcjars only to resolve
//     types; javac then compiles the .java sources with kotlinc's header jar (jvm-abi-gen) first on its classpath. The implementation jar
//     is kotlinc's classes, then javac's.
//   - Flags: kotlincflags, then -language-version 1.9 or the "2" flags (kotlin_lang_version, default the RELEASE_KOTLIN_LANG_VERSION build flag),
//     -Xsam-conversions=class, -no-stdlib, -no-jdk on device, -Xplugin= per kotlin_plugins jar.
//   - The module file gen-kotlin-build-file.py writes (a python_binary_host) is written by the command itself: the same XML, every path
//     absolute, sources from the rsp file and the unpacked srcjars' list.
//   - kotlin-stdlib, -jdk7, -jdk8 and kotlin-annotations are added to static_libs (libs with static_kotlin_stdlib: false) when srcs name .kt
//     files, as Soong's deps() does from the literal srcs.
//   - Not supported, one problem each: kapt (annotation processors on a module with Kotlin sources), common_srcs, compose.

// implicitJavaLibs: the libraries deps() adds for source kinds named in srcs (kotlin stdlib; protobuf runtime of proto.type), appended to
// static_libs (libs for a kotlin stdlib that is not static).
func (g *Graph) implicitJavaLibs(n *jNode, srcs []string) {
	p := n.props
	has := func(ext string) bool {
		for _, s := range srcs {
			if filepath.Ext(s) == ext {
				return true
			}
		}
		return false
	}
	var static, libs []string
	if has(".proto") && p.obj("proto").str("plugin") == "" {
		switch t := p.obj("proto").str("type"); t {
		case "micro", "nano":
			static = append(static, "libprotobuf-java-"+t)
		case "lite", "":
			static = append(static, "libprotobuf-java-lite")
		case "full":
			if n.side == Host {
				static = append(static, "libprotobuf-java-full")
			} else {
				g.problem("%s: full java protos only supported on the host", n.name)
			}
		}
	}
	if has(".kt") {
		std := []string{"kotlin-stdlib", "kotlin-stdlib-jdk7", "kotlin-stdlib-jdk8", "kotlin-annotations"}
		if p.boolean("static_kotlin_stdlib", true) {
			static = append(static, std...)
		} else {
			libs = append(libs, std...)
		}
	}
	if len(p.strs("common_srcs")) > 0 {
		g.problem("%s: common_srcs is unsupported", n.name)
	}
	if len(static) > 0 {
		p.set("static_libs", strVals(append(p.strs("static_libs"), static...)))
	}
	if len(libs) > 0 {
		p.set("libs", strVals(append(p.strs("libs"), libs...)))
	}
}

// kotlinc writes the kotlinc edge: classes jar out, header jar hdr.
func (g *Graph) kotlinc(n *jNode, out, hdr string, java, kt, srcjars, waits []string, ver javaVersion, d javaDeps) {
	p := n.props
	c := g.java.cfg
	flags := append([]string{}, p.strs("kotlincflags")...)
	// base.go: the build flag when declared (it is in every release this converter reads; Release.Flag panics otherwise), else 1.9
	lang := g.release.Flag("RELEASE_KOTLIN_LANG_VERSION")
	if v := p.str("kotlin_lang_version"); v != "" {
		lang = v
	}
	switch lang {
	case "1.9":
		flags = append(flags, "-language-version 1.9")
	case "2":
		flags = append(flags, "-Xsuppress-version-warnings", "-Xconsistent-data-class-copy-visibility")
	default:
		g.problem("%s: kotlin_lang_version must be one of `1.9` or `2`", n.name)
	}
	flags = append(flags, "-Xsam-conversions=class", "-no-stdlib")
	if n.side == Device {
		flags = append(flags, "-no-jdk")
	}
	var plugins []string
	for _, r := range p.strs("kotlin_plugins") {
		if pl := g.jnode(r, Host, n.name); pl != nil && pl.implRes != "" {
			flags = append(flags, "-Xplugin="+pl.implRes)
			plugins = append(plugins, pl.implRes)
		}
	}
	cp := append(append([]string{}, d.boot...), d.classpath...)
	// kotlin.go kotlinName: <module dir>/<variant>/<name>, `/` -> `__`; kotlinc names META-INF/<name>.kotlin_module after it
	variant := "android_common"
	if n.side == Host {
		variant = "linux_glibc_common"
	}
	name := strings.ReplaceAll(filepath.Join(n.mod.Dir, variant, n.name), "/", "__")
	target := ver.String()
	if ver == 9 {
		target = "9"
	}
	dir := filepath.Dir(filepath.Dir(out))
	implicit := append(append(append([]string{}, cp...), plugins...), srcjars...)
	implicit = append(implicit, g.abs(c.Kotlinc), g.abs(c.KotlinAbiGen))
	g.java.edge("kotlinc", []string{out, hdr}, absAll(g, append(append([]string{}, java...), kt...)), implicit, waits, [][2]string{
		{"name", name}, {"cp", esc(strings.Join(cp, " "))}, {"srcjars", join(srcjars)}, {"flags", ninjaRaw(flags)}, {"target", target},
		{"classes", esc(filepath.Join(dir, "kotlinc", "classes"))}, {"hclasses", esc(filepath.Join(dir, "kotlinc", "header_classes"))},
		{"srcjardir", esc(filepath.Join(dir, "kotlinc", "srcJars"))}, {"buildfile", esc(filepath.Join(dir, "kotlinc-build.xml"))},
		{"emptydir", esc(filepath.Join(dir, "kotlinc", "empty"))}, {"kjar", esc(out)}, {"hdr", esc(hdr)},
	})
}

// kotlincRule: kotlin.go's kotlinc rule. The module XML is gen-kotlin-build-file.py's: classpath entries, then each source of the rsp file
// and the srcjars' list (javaSourceRoots for .java, sources for .kt).
func (j *javaGraph) kotlincRule(g *Graph) string {
	c := j.cfg
	xml := `{ printf '<modules>\n  <module name="%s" type="java-production" outputDir="%s">\n' "$name" "$classes"; ` +
		`for f in $cp; do printf '    <classpath path="%s"/>\n' "$$f"; done; ` +
		`for f in $$(cat $kjar.rsp $srcjardir/list); do case $$f in *.java) printf '    <javaSourceRoots path="%s"/>\n' "$$f";; *.kt) printf '    <sources path="%s"/>\n' "$$f";; *) echo "unknown source $$f"; exit 1;; esac; done; ` +
		`printf '  </module>\n</modules>\n'; } > $buildfile`
	unpack := `(for j in $srcjars; do if unzip -Z1 $$j 2>/dev/null | grep -q '\.\(java\|kt\)$$'; then unzip -qo $$j '*.java' '*.kt' -d $srcjardir 2>/dev/null; rc=$$?; [ $$rc -le 1 ] || [ $$rc -eq 11 ] || exit 1; fi; done) && find $srcjardir -name '*.java' -o -name '*.kt' | sort > $srcjardir/list`
	return "rule kotlinc\n" +
		"  command = rm -rf $classes $hclasses $srcjardir $buildfile $emptydir && mkdir -p $classes $hclasses $srcjardir $emptydir && " + unpack + " && " + xml + " && " +
		"JAVA_HOME=$jdk PATH=$jdk/bin:$$PATH " + esc(g.abs(c.Kotlinc)) + " " + strings.Join(c.KotlincGlobal, " ") + " " + strings.Join(c.KotlincJdk9Opts, " ") +
		" -J-Xmx" + c.JavacHeapSize + " $flags -jvm-target $target -Xbuild-file=$buildfile -kotlin-home $emptydir -Xplugin=" + esc(g.abs(c.KotlinAbiGen)) +
		" -P plugin:org.jetbrains.kotlin.jvm.abi:outputDir=$hclasses && $soongzip -jar -o $kjar -C $classes -D $classes && $soongzip -jar -o $hdr -C $hclasses -D $hclasses && rm -rf $srcjardir $classes $hclasses\n" +
		"  rspfile = $kjar.rsp\n  rspfile_content = $in\n  description = kotlinc $kjar\n"
}
