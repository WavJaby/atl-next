// SPDX-License-Identifier: MIT

package main

import (
	"slices"
	"testing"
)

// policy.go transformPolicyToConf: a stable sort by policyConfOrder, attributes and *.te one rank, so public's attributes and .te files stay ahead of
// private's, and macros files move ahead of both whatever their position in srcs.
func TestSortPolicyConf(t *testing.T) {
	srcs := []string{"f/flagging_macros", "pub/security_classes", "pub/attributes", "pub/ioctl_defines", "pub/app.te", "pub/users",
		"priv/security_classes", "priv/attributes", "priv/te_macros", "priv/zygote.te", "priv/port_contexts", "atl/atl.te", "atl/file_contexts"}
	sortPolicyConf(srcs)
	want := []string{"f/flagging_macros", "pub/security_classes", "priv/security_classes", "priv/te_macros", "pub/ioctl_defines", "pub/attributes",
		"pub/app.te", "priv/attributes", "priv/zygote.te", "atl/atl.te", "pub/users", "priv/port_contexts", "atl/file_contexts"}
	if !slices.Equal(srcs, want) {
		t.Fatalf("order\n got %v\nwant %v", srcs, want)
	}
}
