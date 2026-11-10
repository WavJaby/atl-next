<!-- SPDX-License-Identifier: MIT -->
# AGENTS.md -- coding conventions

Applies to every agent and subagent working in this repo. Tools enforce; this file only explains. **Definition of done for any change: `cargo xtask check` passes** (`cargo xtask check --help` lists its steps). Every subagent runs it before handing back, except a stop at its context cap: that hands back a state note, and the continuation child runs it.

## Workspace

- `~/atl-next/` (WSL ext4, never an NTFS drive): `repo/` main checkout; `wt/<task>/` one git worktree per task / subagent; `src/<upstream>/` `xtask fetch` checkouts at pinned commits + origin/main's patches, shared by all checkouts; `src-wt/<task>/<upstream>/` task copies (`<task>` = the repo checkout's branch) of upstreams a task patches; `out/slot-<n>/` build output of the checkout holding slot n (`cargo xtask slot`; `native`, `check` and `test` build there by default) (`bin`, `lib`, `include`, `share`, `gen/`); `cache/` sccache, downloads, test APKs; `tools/` test-environment tools.
- The repo holds only our code, the manifests and `patches/<upstream>/`. Never commit upstream sources, imported AOSP Java or generated code.
- `cargo xtask` is the only build/test driver. Read-only or dry-run by default.

## Design

- **Upgradable layers.** Each layer (ART/libcore, guest bionic, QEMU, AOSP-derived libs) takes a new upstream version with little merge pain. Layers meet only at standard seams: JNI invocation API, AOSP libnativeloader API, NativeBridge callbacks, NDK/bionic ABI, public `android.*` API. No layer reaches under its seam; extend the seam instead.
- **Patch scope.** Prefer adapting ATL over patching AOSP. Every AOSP/QEMU patch states why the launcher, loader or environment cannot do it. Small rebase-stable patches are fine; deep or sprawling ones are not. A one-line patch beats widening a seam's contract: a wider contract is a behaviour every user of that seam inherits.
- **System services are Rust by default** (the system_server role): minimal, extended as apps need, checked against AOSP's service code as the reference. A service whose AOSP behaviour changes often goes back to AOSP, or calls a reusable AOSP module already in the app process. A Java-only, fast-moving service is discussed with the owner first.
- **Delete legacy in the change that replaces it.** No compatibility shims, dual paths or "keep the old one for now".
- **No heavy VM.** In-process user-mode translation; native Linux APIs directly (Wayland, GTK4, EGL/Vulkan, PipeWire, portals). Daily-usable desktop software: system theme, IME, HiDPI and notifications follow the Linux desktop.
- **UI.** Android renders all app UI: AOSP framework Java → GPU hwui (Skia/Vulkan) → our `ANativeWindow` → dmabuf → GSK texture in the GTK window. Adapt hwui by implementing the Android interfaces it consumes, not by rewriting its call sites. No per-widget GTK peers: the desktop look comes from mapping portal appearance settings (dark mode, accent, contrast, font scale) into Android theming. GTK only for window chrome, IME bridge, portals and WebView/SurfaceView holes.
- **One implementation per concept, a seam for the next.** Translator engine (QEMU), media backend (ffmpeg) and similar get a minimal interface and one implementation. A trait is shaped by its first implementation's needs and generalised when the second is built. No stubs, and no user choice between parallel implementations until a second exists. In-repo traits carry no version.
- **Host OS behind one platform layer.** Window/surface, input, clipboard, notifications, intents/IPC, file picker, audio and WebView are reached only through it. No Wayland, GTK-backend, D-Bus or portal call outside it. Only `-<os>` crates may depend on GTK, Wayland or Windows crates (cargo-deny `bans`). OS selection is compile-time (`cfg(target_os)` only inside the platform crates). Two groups: `atl-platform-ui` for app processes, `atl-platform-host` for `atld`; each is `-api` / `-<os>` / facade. Trait signatures use only our opaque handles. Platform-decided choices are static; user- or app-decided ones become runtime enums once a second implementation exists. Linux is the only implementation now; native Windows is later, and it gets no GTK.
- **Target is real Linux; WSL2 is a test environment only.** Reference target: the owner's x86_64 Arch laptop. Never change the architecture or add a product code path to make WSL work; test environments get no product code path. WSL is made to fit by environment-side setup only (a kernel module for dmabuf allocation, a nested compositor for missing protocols, an audio tunnel, per-run GPU driver variables). Code above the layer is single-path. What WSL cannot provide is tested on the laptop.
- **No environment-variable switches in product code, and no test-only product paths** (owner rule). Platform differences are environment-side setup, or `cfg(target_os)` inside the platform crates.
- **Bridges (ABI translation) have four things.** (1) Entry shells generated from one interface description. (2) Layouts checked at compile time against upstream headers fetched at build time. (3) Pass-through only where a gate proves the ABI identical, never assumed. (4) A named ground-truth suite (CTS, dEQP or the upstream's own tests).
- **Module boundaries by table.** `modules.toml` declares each module's crates, provided interfaces and allowed `uses`. Cross-module edges reach only the target's exports, unless a `direct_link` entry covers them with its reason (declared, so part of the interface, not a bypass). The gate fails on an undeclared edge or an unused declared one. Diagrams come from `xtask doc`. Hot paths (frames, input, audio) do not cross processes.
- **Feature groups by table.** `features.toml` is the only list of feature groups (planned, not yet created). Never delete public API classes to save space.
- **Arch-neutral.** `arches.toml` is the only list of arches. Per-arch code sits in per-arch modules behind traits. Pointer width is a parameter. Tier 1 hosts: x86_64, arm64, riscv64. Adding an arch edits only its table row and module.
- **Sandbox.** Every app process runs under bubblewrap, Landlock and seccomp; the broker is the only launcher. Rights come from the manifest permissions plus the user's grants (PermissionController), never configured per app by hand. Desktop access only via portals.
- **Old native games are a requirement:** arm32/arm64/x86/x86_64 `.so`; x86_64 first, riscv64 later.

## Clean-room

- Implement from AOSP sources and docs, `current.txt`, Android docs and CTS, and measurements on real Android.
- ATL, bionic_translation and art_standalone code is never copied, and is never open while writing the component it covers.
- Reading it to learn how a behaviour works is allowed. The conclusion is written up as our own design note before implementing; that note is the only input used while writing the component, and it is not committed with the code.
- ATL's behaviour is a hypothesis, never ground truth. A behaviour counts as settled only once confirmed against AOSP source, Android docs or CTS, or a measurement on real Android. The note states which.

## Licence

Goal: our code is MIT; third-party components keep their own licences; copyleft is confined, not banned. `licenses.toml` holds the detail.

- No GPL-3 code linked into the runtime (ART process, `atld`, our libraries) unless it carries a listed exception: `<id> WITH <exception>` in `licenses.toml` (e.g. GCC Runtime Library Exception, Bison exception).
- The one named exception is the QEMU bridge: a separate `dlopen`ed `.so`, distributed as GPL-3.0-or-later. Every licence in a process that loads it must be GPL-3-compatible; `licenses.toml` keeps the runtime allow list a subset of the bridge's. That is why OpenSSL-licensed BoringSSL and dng_sdk (field-of-use patent grant) are not shipped; wolfSSL is out (Conscrypt/BoringSSL instead).
- No GPL-2.0-only code without a linking exception anywhere in a process. `GPL-2.0-only WITH Classpath-exception-2.0` (libcore `ojluni`, libjdwp) is allowed: the exception permits linking with independent modules of any licence.
- Optional user-system libraries loaded at runtime via `dlopen` behind a seam (e.g. distro ffmpeg, GPL-3) are allowed.
- Before any binary with GPL code (QEMU bridge, libcore) is distributed, its corresponding source must be obtainable: the repo public, or a source archive of pins and patches.
- Build-time licence gate: fails on a GPL-2.0-only file without a linking exception in the compiled set, and on any undeclared licence. The checked set is what reaches a shipped binary: linked objects (by symbol), headers pulled in through depfiles, Java sources, resources, guest code. A file's licence is its SPDX tag, else its notice identified by deterministic phrase rules, else an explicit per-path entry in `licenses.toml`. An unrecognised notice fails; no similarity guessing (askalono-style matching confuses GPL-2 and GPL-3 notices).
- Patches carry the licence of the file they patch: AOSP Apache-2.0; QEMU `GPL-2.0-or-later`. Never the deprecated SPDX id `GPL-2.0`, which means GPL-2.0-only.

## Tables

- Repo tables (`arches.toml`, `sources.toml`, `deps.toml`, `providers.toml`, `modules.toml`, ...) document their columns in their own header; `cargo xtask check` steps `tables` and `modules` validate them.
- Every column is either checked by a script against its real source or derived and not stored. Adding a column adds its check in the same change. Cross-references are typed and checked to exist; a reason a script must enforce is a field, not a comment.
- Rows are added when a consumer first needs them. Tools that rewrite a table use `toml_edit`, keeping comments and order.

## AOSP's own code first

- For an AOSP component that does not work as-is on Linux, take the first rung that works; the change (commit, `providers.toml` reason or patch `Patch-Reason:`) states why the rungs above it do not:
  1. build the original source as-is, adapting only build config;
  2. the environment supplies what it reads (files, config, env vars, sandbox layout: `fonts.xml`, the property area);
  3. supply missing definitions/functions so it compiles and links (`atl-bionic`: bionic-only API glibc lacks);
  4. a small patch (below);
  5. replace individual files in Rust (`providers.toml` `srcs`);
  6. a new Rust component where Android relies on a kernel, HAL or system daemon Linux lacks (binderd, launcher, property service, platform layer).
- A Rust reimplementation of AOSP code with the same behaviour is a second copy to maintain: not written, and an existing one is replaced by the AOSP source.

## Upstream patches

- Each upstream has one checkout per repo checkout: its task copy `src-wt/<task>/<upstream>` if it exists, else the shared `src/<upstream>`. One resolver places every upstream path, crate `atl-layout` (`Layout::locate`): xtask uses it directly, build scripts through `atl_bridgegen::buildscript::Upstreams`, whose `rerun_paths` also make cargo rerun when a task copy appears or goes. Never join `src/` by hand.
- `cargo xtask fetch --execute` checks each upstream out (shallow, at the `sources.toml` pin) and, where `patches/<upstream>/` holds patches, puts it on its patch branch = pin + those patches (`git am`): `atl-patches` in `src/`, `atl-patches-<task>` in a task copy. It never drops a commit on the patch branch that `patches/` lacks: it refuses and names the command to run -- unless the repo checkout's history deleted that patch from `patches/<upstream>/` (retired: the text stays in history), which is how a merged patch deletion leaves `src/`; with no patches left the checkout goes back to the bare pin.
- **`src/` holds only origin/main's pins and patches.** Fetch changes `src/<upstream>` only when this repo checkout's pin and `patches/<upstream>/` equal origin/main's (or origin/main has no such row: a new checkout); otherwise it refuses that row, naming `--edit`. So a branch with its own patches or pin never rewrites what other worktrees build from: those go to its task copy, which fetch settles instead. `xtask patch --rebase` follows the same rule.
- **Landing a branch** (any branch, or only its `patches/` or pin changes): `cargo xtask land <branch>` (dry run; `--execute` acts). It checks the branch in its own worktree, where its task copies hold its patches, before the push; then pushes, runs `fetch --execute` in the main checkout (moves `src/` to the new origin/main), checks main, and removes the copies, worktree and branch. Until that fetch, `drift` on main fails (patches/ ahead of src/): checking the branch from main cannot see its patches, since main has no task copies.
- `cargo xtask native` first runs the drift test on what it builds from (every AOSP checkout, every upstream built for `providers.toml`): an uncommitted edit, an untracked file xtask did not put there (manifest link/copy files and prune markers are expected), or the patch branch differing from `patches/`, fails before the converter; a clean checkout is compiled as it is. Step `drift` of `cargo xtask check` runs the same test on every checkout.
- Change upstream code only in a task copy, on a task branch: `cargo xtask patch --edit <upstream>` makes `src-wt/<task>/<upstream>`, a git worktree of `src/<upstream>` on `atl-patches-<task>` = pin + this checkout's `patches/<upstream>/` (its base in `refs/worktree/atl/base`); it commits as the repo checkout's `user.name` / `user.email`, set in the copy's own worktree config. Commit there, then `cargo xtask patch --export <upstream>` writes the commits to `patches/<upstream>/` (reproducible bytes; the SPDX expression of the patched files is inserted after `---`) with its `LICENSE`; with no commits past the pin it refuses and writes nothing (to drop every patch, delete `patches/<upstream>/`). Export is always explicit: no build writes into the repo. `cargo xtask patch --done <upstream>` (`--task <branch>` from main) removes the copy and its branch; it refuses while the copy holds uncommitted, untracked or unexported work. Never commit in `src/`.
- AOSP task copies: `native` and `check` re-execute inside a mount namespace (`bwrap --dev-bind / /`, xtask/src/tree.rs) with each task copy bind-mounted over `src/aosp/<project>`, so the tree is always `src/aosp` and canonical paths match a plain build; the slot's `built-from` names each bound copy and its state. `ninja` run by hand outside it reads the unpatched tree: use `cargo xtask native --shell --host <h>`. A pinned project nested inside an edited one is refused. `cargo xtask test task-bind` gates it.
- Every patch commit carries the trailers `Upstream-Status:` (Pending / Submitted [where] / Backport / Denied / Inappropriate [why]) and `Patch-Reason:` (why the launcher, loader or environment cannot do it). Patches stay small and rebase-stable; one whose reason is gone is deleted at the next upgrade.
- `patches/<upstream>/LICENSE` is the upstream's licence text, derived: a copy of the first regular file of `LICENSE`, `COPYING`, `NOTICE` at the pin (AOSP projects ship only `NOTICE` plus an empty `MODULE_LICENSE_*` marker, so theirs is `NOTICE`). `--export` writes it; the drift test fails when it differs from the pin's; step `spdx` fails on a patch without it.
- After a pin bump: `cargo xtask fetch --execute` re-applies `patches/` on the new pin. Where they no longer apply it restores the checkout and says so: `cargo xtask patch --rebase <upstream>`, resolve conflicts, `--export`.

## Languages

- Rust for everything we write, tools and generators included. Each other language has a named reason:
  - Java: only the api-impl surface ART requires, and forwarders where AOSP Java needs an object implementing a Java interface that JNI cannot create (owner, 2026-11-06; first use: the installer's `ParsingPackageUtils.Callback`). A forwarder only adapts and forwards; no logic.
  - Go: only the Blueprint → ninja converter, because the official Blueprint parser is Go.
  - C++: only cxx-bridge wrapper files inside a Rust boundary crate that call the public API of AOSP C++ classes (`crates/atl-libinput/wrap/`; owner, 2026-11-04). cxx cannot call constructors or build `std::shared_ptr` / `ui::Transform`; an AOSP patch carrying wrappers must be rebased on every upgrade, and mangled C++ symbols called from Rust break silently. Wrappers only adapt types and forward; no logic.
  - C/C++ test fixtures that stand in for third-party app native code (NDK-built `.so` or executables, never shipped; owner, 2026-11-06). The thing under test is NDK clang output linked against bionic and `libc++_shared`, which a Rust build does not exercise (C++ exceptions, libc++, emulated vs ELF TLS).
  - Assembly: only inside Rust (`global_asm!` or naked functions) where Rust cannot express the ABI, e.g. the aarch64 TLSDESC resolver. No separate `.S` files.
- No Python of our own (owner, 2026-10-29).
- Upstream build tools run as-is, never extended or wrapped: meson (QEMU; Mesa, whose only supported build is meson), CMake (upstream BoringSSL; llvm-project runtimes libcxx, libcxxabi, libunwind, compiler-rt builtins for the bionic targets), AOSP genrule scripts (Python, run by upstream's build).
- Soong is retired (2026-11-04, `a84d9b6`): never run it. The converter reads Soong's Go sources as the reference for build behaviour only.
- Every file carries an SPDX header; our code is `MIT`.

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
