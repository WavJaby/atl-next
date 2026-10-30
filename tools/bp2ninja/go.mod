// SPDX-License-Identifier: MIT
// Builds only through the go.work `xtask native` generates: it points github.com/google/blueprint at the
// pinned build/blueprint checkout, whose path depends on the workspace (repo or worktree).
module atl-next/bp2ninja

go 1.24

require github.com/google/blueprint v0.0.0
