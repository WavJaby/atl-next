<!-- SPDX-License-Identifier: MIT -->
# ATL-next

Run Android apps natively on desktop Linux: ART 16, the AOSP framework rendering through GPU-enabled hwui, and in-process QEMU translation for foreign-arch native code. Early bootstrap; nothing runs yet.

Development: install `rustup` (Debian / Arch package), then `cargo xtask deps --check` and `cargo xtask check` (conventions in [AGENTS.md](AGENTS.md)). Licence: MIT for our code ([LICENSE](LICENSE)); patches under `patches/` carry the licence of the file they patch.
