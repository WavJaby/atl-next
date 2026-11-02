// SPDX-License-Identifier: MIT

package main

import "strings"

// soong_config_module_type (android/soong_config_modules.go, android/soongconfig/modules.go): a module of such a type carries
// soong_config_variables, applied as a load hook (AppendProperties, before defaults and arch) from the variables' values in the
// type's config_namespace. Values come from Release.ConfigVar (the release-backed lines of android_soong_config_vars.mk); a variable it
// does not know is unset, which selects conditions_default for every kind.

// soongConfigType: one soong_config_module_type definition, its variables by kind (bool / value / list / string).
type soongConfigType struct {
	ns    string
	kinds map[string]string
}

func readSoongConfigType(m *Module) *soongConfigType {
	t := &soongConfigType{ns: m.Props.str("config_namespace"), kinds: map[string]string{}}
	for prop, kind := range map[string]string{"bool_variables": "bool", "value_variables": "value", "list_variables": "list", "variables": "string"} {
		for _, v := range m.Props.strs(prop) {
			t.kinds[v] = kind
		}
	}
	return t
}

// soongConfigProps: m's own properties with its soong_config_variables applied (each variable's PropertiesToApply, appended in
// declaration order).
//   - unset (any kind): conditions_default.
//   - bool: soongConfig.Bool ("1" / "y" / "yes" / "on" / "true", any case) -> the block without conditions_default, else conditions_default.
//   - string: the block named by the value, else conditions_default.
//   - value: the block without conditions_default, every "%s" in its strings replaced by the value.
//   - list: not converted (no release-backed list variable yet): a problem, conditions_default.
func (g *Graph) soongConfigProps(m *Module) *Obj {
	sv := m.Props.obj("soong_config_variables")
	if sv == nil {
		return m.Props
	}
	p := removeKey(m.Props, "soong_config_variables")
	t := g.soongConfigTypes[m.Type]
	for _, v := range sv.keys {
		blk := sv.obj(v)
		if blk == nil {
			continue
		}
		def := blk.obj("conditions_default")
		apply := def
		val, set := "", false
		if t != nil && g.release != nil {
			val, set = g.release.ConfigVar(t.ns, v)
		}
		if set {
			switch kind := t.kinds[v]; kind {
			case "bool":
				switch strings.ToLower(val) {
				case "1", "y", "yes", "on", "true":
					apply = removeKey(blk, "conditions_default")
				}
			case "string":
				if o := blk.obj(val); o != nil {
					apply = o
				}
			case "value":
				apply = sprintfObj(removeKey(blk, "conditions_default"), val).(*Obj)
			default:
				g.problem("%s: soong config variable %s/%s of kind %q is not converted; conditions_default applied", m.Name, t.ns, v, kind)
			}
		}
		if apply != nil {
			p = merge(p, apply)
		}
	}
	return p
}

// sprintfObj: valueVariable.printfIntoPropertyRecursive, "%s" -> val in every string.
func sprintfObj(v Val, val string) Val {
	switch x := v.(type) {
	case string:
		return strings.ReplaceAll(x, "%s", val)
	case []Val:
		out := make([]Val, len(x))
		for i, e := range x {
			out[i] = sprintfObj(e, val)
		}
		return out
	case *Obj:
		out := newObj()
		for _, k := range x.keys {
			out.set(k, sprintfObj(x.m[k], val))
		}
		return out
	}
	return v
}
