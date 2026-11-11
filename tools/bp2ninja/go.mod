// SPDX-License-Identifier: MIT
// Builds only through the go.work xtask writes (xtask converter.rs go_work): it points each module below at its pinned checkout, whose path depends on the workspace (repo or worktree).
// android/soong: its toolchain config packages are read as data (soongflags.go); google.golang.org/protobuf is what Soong's android package imports.
module atl-next/bp2ninja

go 1.24

require (
	android/soong v0.0.0
	github.com/google/blueprint v0.0.0
)

require google.golang.org/protobuf v0.0.0 // indirect
