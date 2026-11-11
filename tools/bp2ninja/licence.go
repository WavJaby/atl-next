// SPDX-License-Identifier: MIT

package main

import (
	"fmt"
	"path/filepath"
	"strings"
)

// The licence gate as one edge of the build (-licence-gate): its inputs are the device link outputs, the shipped artifacts; the command is the caller's (xtask's gate), which reads them from `$out.rsp`,
// writes its report to `$out` and every file it read to the depfile `$out.d`. The rspfile content is part of ninja's command hash, so a changed artifact list reruns it too.
// Its findings are the report's, not the edge's exit status: a red report is an up-to-date output, so the build's other steps and the caller's verdict proceed as with any gate.

// licenceReport: the gate edge's output, in the out dir.
const licenceReport = "licence-gate.json"

// licenceNinja: the gate rule and edge over g.deviceLinks (call after emit), a default target; "" without a command.
func (g *Graph) licenceNinja(command string) string {
	if command == "" {
		return ""
	}
	report := filepath.Join(g.out, licenceReport)
	return "\nrule licence_gate\n  command = " + strings.ReplaceAll(command, "\n", " ") + "\n  rspfile = $out.rsp\n  rspfile_content = $in\n  depfile = $out.d\n  deps = gcc\n" +
		fmt.Sprintf("  description = licence gate over %d device artifacts\n", len(g.deviceLinks)) +
		"build " + esc(report) + ": licence_gate " + join(g.deviceLinks) + "\ndefault " + esc(report) + "\n"
}
