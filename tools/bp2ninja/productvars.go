// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"strconv"
	"strings"
)

// Product variable blocks, `product_variables: { <variable>: { <props> } }`: Soong's VariableMutator (android/variable.go), a pre-deps mutator,
// so it runs after the arch mutator and appends each set variable's block onto the already flattened properties. A block whose variable this
// product does not set is skipped, as Soong skips a nil variable; a bool variable applies only when true. Every string in an applied block
// with a single %d / %s gets the variable's value (printfIntoProperty; %d of a bool = 1).
//
// The variables this build sets: platform_sdk_version = RELEASE_PLATFORM_SDK_VERSION, which is what product config writes into
// Platform_sdk_version (the number even before finalisation, unlike the API level `current` resolves to). Every other variable is unset,
// i.e. a user, non-eng, non-coverage, non-unbundled product.
func (g *Graph) productVariables() map[string]Val {
	if g.release == nil {
		return nil
	}
	sdk := strings.TrimSpace(g.release.Flag("RELEASE_PLATFORM_SDK_VERSION"))
	n, err := strconv.ParseInt(sdk, 10, 64)
	if err != nil {
		g.problem("release %s: RELEASE_PLATFORM_SDK_VERSION %q is not a number", g.release.Name, sdk)
		return nil
	}
	return map[string]Val{"platform_sdk_version": n}
}

// applyProductVariables appends the blocks of `pv` (a module's product_variables object, nil = none) whose variable is set onto base.
func (g *Graph) applyProductVariables(m *Module, base, pv *Obj) *Obj {
	if pv == nil {
		return base
	}
	vars := g.productVariables()
	for _, name := range pv.keys {
		v, set := vars[name]
		if !set {
			continue
		}
		if b, ok := v.(bool); ok && !b {
			continue
		}
		block, ok := pv.m[name].(*Obj)
		if !ok {
			g.problem("%s: product_variables.%s is not a block", m.Name, name)
			continue
		}
		filled, err := printfInto(block, v)
		if err != nil {
			g.problem("%s: product_variables.%s: %v", m.Name, name, err)
			continue
		}
		base = merge(base, filled)
	}
	return base
}

// printfInto is a copy of block with each string (alone or in a list, at any depth) filled from v (Soong printfIntoProperty).
func printfInto(block *Obj, v Val) (*Obj, error) {
	out := newObj()
	for _, k := range block.keys {
		x, err := printfVal(block.m[k], v)
		if err != nil {
			return nil, fmt.Errorf("%s: %w", k, err)
		}
		out.set(k, x)
	}
	return out, nil
}

func printfVal(x, v Val) (Val, error) {
	switch t := x.(type) {
	case string:
		return printfString(t, v)
	case []Val:
		out := make([]Val, len(t))
		for i, e := range t {
			s, err := printfVal(e, v)
			if err != nil {
				return nil, err
			}
			out[i] = s
		}
		return out, nil
	case *Obj:
		return printfInto(t, v)
	}
	return x, nil
}

func printfString(s string, v Val) (string, error) {
	switch strings.Count(s, "%") {
	case 0:
		return s, nil
	case 1:
	default:
		return "", fmt.Errorf("product variable properties only support a single '%%': %q", s)
	}
	switch {
	case strings.Contains(s, "%d"):
		switch t := v.(type) {
		case int64:
			return fmt.Sprintf(s, t), nil
		case bool:
			if t {
				return fmt.Sprintf(s, 1), nil
			}
			return fmt.Sprintf(s, 0), nil
		}
		return "", fmt.Errorf("unsupported type %T for %%d: %q", v, s)
	case strings.Contains(s, "%s"):
		if t, ok := v.(string); ok {
			return fmt.Sprintf(s, t), nil
		}
		return "", fmt.Errorf("unsupported type %T for %%s: %q", v, s)
	}
	return "", fmt.Errorf("unsupported %% in product variable property: %q", s)
}
