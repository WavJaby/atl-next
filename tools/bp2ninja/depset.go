// SPDX-License-Identifier: MIT

package main

import "slices"

// depSet is build/blueprint/depset (the spec) with the one order the resource code uses, TOPOLOGICAL (depset.go New, collect, ToList): a DAG
// of direct elements and transitive sets, flattened by a post-order walk over reversed inputs, first copy of each element kept, then reversed,
// so every set's elements come before those of the sets it names. nil is Soong's zero DepSet.
type depSet[T comparable] struct {
	direct     []T
	transitive []*depSet[T]
}

// newDepSet is depset.New(TOPOLOGICAL, direct, transitive): inputs are reversed so the final reversal restores their order, and a set with
// no direct elements and no transitive input at all is the zero set (nil; the check counts the input list, as Soong's does, zero sets included).
func newDepSet[T comparable](direct []T, transitive []*depSet[T]) *depSet[T] {
	if len(direct) == 0 && len(transitive) == 0 {
		return nil
	}
	d := &depSet[T]{direct: slices.Clone(direct)}
	slices.Reverse(d.direct)
	for i := len(transitive) - 1; i >= 0; i-- {
		if transitive[i] != nil {
			d.transitive = append(d.transitive, transitive[i])
		}
	}
	return d
}

// toList is DepSet.ToList.
func (d *depSet[T]) toList() []T {
	if d == nil {
		return nil
	}
	visited := map[*depSet[T]]bool{}
	var list []T
	var dfs func(*depSet[T])
	dfs = func(d *depSet[T]) {
		visited[d] = true
		for _, t := range d.transitive {
			if !visited[t] {
				dfs(t)
			}
		}
		list = append(list, d.direct...)
	}
	dfs(d)
	seen := map[T]bool{}
	out := list[:0]
	for _, e := range list {
		if !seen[e] {
			seen[e] = true
			out = append(out, e)
		}
	}
	slices.Reverse(out)
	return out
}
