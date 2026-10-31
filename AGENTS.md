<!-- SPDX-License-Identifier: MIT -->
# AGENTS.md -- coding conventions

Applies to every agent and subagent working in this repo. Tools enforce; this file only explains. **Definition of done for any change: `cargo xtask check` passes** (`cargo xtask check --help` lists its steps). Every subagent runs it before handing back; every delegation brief says so.

## Workspace

- `~/atl-next/` (WSL ext4, never an NTFS drive): `repo/` main checkout; `wt/<task>/` one git worktree per task / subagent; `src/<upstream>/` `xtask fetch` checkouts at pinned commits + origin/main's patches, shared by all checkouts; `src-wt/<task>/<upstream>/` task copies (`<task>` = the repo checkout's branch) of upstreams a task patches, `src-wt/<task>/aosp-tree/` the AOSP link farm over them; `out/<checkout>/<target>/` build output (`bin`, `lib`, `include`, `share`, `gen/`); `cache/` sccache, downloads, test APKs; `tools/` test-environment tools.
- The repo holds only our code, the manifests and `patches/<upstream>/`. Never commit upstream sources, imported AOSP Java or generated code.
- `cargo xtask` is the only build/test driver. Read-only or dry-run by default.

## Tables

- Repo tables (`arches.toml`, `sources.toml`, `deps.toml`, `providers.toml`, `modules.toml`, ...) document their columns in their own header; `cargo xtask check` steps `tables` and `modules` validate them.
- Every column is either checked by a script against its real source or derived and not stored. Adding a column adds its check in the same change. Cross-references are typed and checked to exist; a reason a script must enforce is a field, not a comment.
- Rows are added when a consumer first needs them. Tools that rewrite a table use `toml_edit`, keeping comments and order.

## Upstream patches

- Each upstream has one checkout per repo checkout: its task copy `src-wt/<task>/<upstream>` if it exists, else the shared `src/<upstream>`. One resolver places every upstream path, crate `atl-layout` (`Layout::locate`): xtask uses it directly, build scripts through `atl_bridgegen::buildscript::Upstreams`, whose `rerun_paths` also make cargo rerun when a task copy appears or goes. Never join `src/` by hand.
- `cargo xtask fetch --execute` checks each upstream out (shallow, at the `sources.toml` pin) and, where `patches/<upstream>/` holds patches, puts it on its patch branch = pin + those patches (`git am`): `atl-patches` in `src/`, `atl-patches-<task>` in a task copy. It never drops a commit on the patch branch that `patches/` lacks: it refuses and names the command to run.
- **`src/` holds only origin/main's pins and patches.** Fetch changes `src/<upstream>` only when this repo checkout's pin and `patches/<upstream>/` equal origin/main's (or origin/main has no such row: a new checkout); otherwise it refuses that row, naming `--edit`. So a branch with its own patches or pin never rewrites what other worktrees build from: those go to its task copy, which fetch settles instead. `xtask patch --rebase` follows the same rule.
- **Merging a change to `patches/` or a pin:** in the task worktree `--export`, then `--done`; rebase on origin/main, gates, ff push; then in the main checkout `git pull --ff-only && cargo xtask fetch --execute`, which moves `src/` to the new origin/main. Until that last step `cargo xtask check` on main fails at `drift` (patches/ ahead of src/), naming the same command.
- `cargo xtask native` first runs the drift test on what it builds from (every AOSP checkout, every upstream built for `providers.toml`): an uncommitted edit, an untracked file xtask did not put there (manifest link/copy files and prune markers are expected), or the patch branch differing from `patches/`, fails before the converter; a clean checkout is compiled as it is. Step `drift` of `cargo xtask check` runs the same test on every checkout.
- Change upstream code only in a task copy, on a task branch: `cargo xtask patch --edit <upstream>` makes `src-wt/<task>/<upstream>`, a git worktree of `src/<upstream>` on `atl-patches-<task>` = pin + this checkout's `patches/<upstream>/` (its base in `refs/worktree/atl/base`); it commits as the repo checkout's `user.name` / `user.email`, set in the copy's own worktree config. Commit there, then `cargo xtask patch --export <upstream>` writes the commits to `patches/<upstream>/` (reproducible bytes; the SPDX expression of the patched files is inserted after `---`) with its `LICENSE`; with no commits past the pin it refuses and writes nothing (to drop every patch, delete `patches/<upstream>/`). Export is always explicit: no build writes into the repo. `cargo xtask patch --done <upstream>` removes the copy and its branch; it refuses while the copy holds uncommitted, untracked or unexported work. Never commit in `src/`.
- AOSP task copies: the converter takes the tree whole, so `native` builds from the link farm `src-wt/<task>/aosp-tree/`, regenerated every run (real directories along each edited project's path, absolute links to `src/aosp` entries and to the task copies; bp2ninja follows absolute directory links only). Limitation: a source's `#include "../.."` that leaves an edited project resolves physically, under `src-wt/`, not through the farm. A pinned project nested inside an edited one is refused.
- Every patch commit carries the trailers `Upstream-Status:` (Pending / Submitted [where] / Backport / Denied / Inappropriate [why]) and `Patch-Reason:` (why the launcher, loader or environment cannot do it). Patches stay small and rebase-stable; one whose reason is gone is deleted at the next upgrade.
- `patches/<upstream>/LICENSE` is the upstream's licence text, derived: a copy of the first regular file of `LICENSE`, `COPYING`, `NOTICE` at the pin (AOSP projects ship only `NOTICE` plus an empty `MODULE_LICENSE_*` marker, so theirs is `NOTICE`). `--export` writes it; the drift test fails when it differs from the pin's; step `spdx` fails on a patch without it.
- After a pin bump: `cargo xtask fetch --execute` re-applies `patches/` on the new pin. Where they no longer apply it restores the checkout and says so: `cargo xtask patch --rebase <upstream>`, resolve conflicts, `--export`.

## Languages

- Rust for everything we write, tools and generators included. Java only for the api-impl surface ART requires. Go only for the Blueprint -> ninja converter. No Python. Assembly only inside Rust (`global_asm!` / naked functions).
- Every file carries an SPDX header; our code is `MIT`. A patch carries the licence of the file it patches.

## Format

- `rust-toolchain.toml` pins the toolchain. `rustfmt.toml` uses stable options only; `imports_granularity`, `group_imports`, `wrap_comments` are deliberately unused. `.editorconfig` for everything else. Go: gofmt.
- Patches to AOSP / QEMU follow that upstream's own clang-format, so they rebase cleanly.
- Comments are never wrapped to a column: a line break marks a change of subject.

## Lints

- `[workspace.lints]` in `Cargo.toml`, inherited by every crate with `[lints] workspace = true`: denies `dbg!`, `todo!`, `unimplemented!`, `print!`/`eprint!` (logging only via `tracing`; unimplemented API paths via `unimplemented_api!`), `unwrap()` outside tests (use `expect("reason")` or return the error), unsafe blocks without `// SAFETY:`.
- Per-crate additions go in the crate root: `#![forbid(unsafe_code)]` in every core crate (services, broker, binderd, audio/media logic, input, generators, xtask); `#![deny(missing_docs)]` in `-api` crates.
- `unsafe` only in boundary crates (C ABI exports, JNI, ELF loader / bionic bridge, NativeBridge / QEMU bridge, GPU/dmabuf interop, `dlopen` of optional libs). Each entry point converts raw pointers to safe types at once and calls safe code.

## Idioms

- Errors: `thiserror` in libraries, `anyhow` in binaries and xtask.
- Logging: `tracing`.
- Naming: Android concepts keep their Android names (`ActivityTaskManager`, `InputChannel`, ...).
- Generated code lives in `gen/` with the header `generated by <tool> from <source>, do not edit`; fix the generator, never its output.
- Dependencies: declared once in `[workspace.dependencies]`, one version per crate. A new dependency passes cargo-deny. A new crate, or a new or removed dependency between crates of different modules, updates `modules.toml` in the same change (step `modules` fails otherwise).
- OS-specific code only in the platform `-<os>` crates (`cfg(target_os)` nowhere else). Arch names only in `arches.toml` and per-arch modules.
- Async: `atld` uses tokio; app processes use the GLib main loop; shared core crates are runtime-agnostic (sync, message-driven) so both sides use them and tests need no runtime.

## Tests

- Unit tests in-module under `#[cfg(test)]`; integration tests in `tests/`; golden files in `testdata/`.
- Expected values come from the spec or an independent calculation, never from calling the code under test. Where none exists, keep the literal and say why it is that number.

## Git

- Trunk-based: `main` always green; one short-lived branch per task, one git worktree per subagent; squash merge.
- Conventional Commits (`type(scope): subject`); the PR title becomes the squash commit.
- Branch names task-specific, never an agent or tool name.
