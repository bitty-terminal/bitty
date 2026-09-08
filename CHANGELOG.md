# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### `frameHash` lossless frame digest (CTX-0244, issue #420)

- New `bitty.debug/frameHash` IPC method: SHA-256 (std-only, no new
  dependency) over canonical `BFH1 || width_be32 || height_be32 ||
frameSeq_be64 || headless_rgba`, answering the CTX-0242 V1-V3 equality
  question in 32 bytes with zero pixel bytes on the wire.
- Same CTX-0188 consent minter, no new scope (`debug.trace` +
  `terminal.inspect`), new `AutomationFamily::FrameDigest` (never widens
  `Capture`), TTL cap 120 s (default minter refuses digest grants),
  2 digests/s rate ceiling, local-attested transport only (fail-closed
  `ScopeDenied` otherwise), `Unavailable` on empty surfaces (never a hash
  of nothing), granted AND denied calls audited in the existing bounded
  (64, drop-oldest) log with the served digest hex.
- Runtime publishes the presented headless frame only while a digest grant
  is live (zero clone cost otherwise) and only for headless presents.
- Digest tests run headless/in-process in CI; the live-grant ceremony test
  is `#[ignore]`-gated local-manual only. Raw pixel channel stays
  deferred indefinitely (no pixel bytes cross IPC under any grant).

### Panel live V1-V3 gates on `frameHash` equality (CTX-0242)

- New `crates/bitty-runtime/tests/panel_live_framehash.rs`: headless,
  deterministic, synthetic-only equality gates wiring V1-V3 to the CTX-0244
  digest (`frame_digest_hex`, the same function the server hashes with).
  V1 pins the gap-band paint contract (gapped digest differs from the
  no-gap baseline for identical content, fresh re-run reproduces it
  exactly, plus a direct gap-pixel == theme bg assertion); V2 pins
  focus-switch diff plus switch-back restore (primary follows focus,
  CTX-0234); V3 pins workspace-alias vs tabs-shim digest equality for
  identical content (layout parity consequence of the `tabs_compat` guard).
- New socket-level lossless proof in the same file (CTX-0188 harness
  pattern: real Unix socket, file-local serial guard, reply correlation):
  a real `Runtime` headless frame published via `publish_frame_rgba`
  digests identically through `bitty.debug/frameHash`, with zero pixel
  bytes on the wire. The live-grant ws4 ceremony (V1 gaps, V2 tab focus,
  V3 alias parity per focus step, expiry `ScopeDenied`, digest audit) is
  `#[ignore]`-gated local-manual only and never runs in CI.

### AUR bitty-bin prebuilt package (CTX-0138, issue #227)

- New `packaging/PKGBUILD.bin` template for AUR `bitty-bin`: installs the prebuilt `bitty-x86_64-unknown-linux-gnu` release binary to `/usr/bin/bitty` (no user-side compile), real sha256 filled by the release workflow (never `SKIP` for the binary), `arch=('x86_64')` only, `provides=('bitty')`, `conflicts=('bitty' 'bitty-nightly' 'bitty-git')`, plus `.desktop`/icons from the release source tarball.
- Release workflow gains an `aur-bin` publish job (same SSH host-key pinning and isolated `BUILD_DIR` pattern as the `aur` job; first push registers `bitty-bin` on AUR; gated on `vars.AUR_BIN_PUBLISH`).
- New `scripts/check-release-version.sh` keeps the Cargo workspace version aligned with release tags (plus `PKGBUILD*`/`nfpm.yaml` consistency), enforced in the release `validate` job; `scripts/check-pkgbuild-bin.sh` render-tests the template. No version bump in this change; the bump ships with the release that enables the publish job. Asset-name dependency on CTX-0164 (#264) noted in the template: the `bitty-<target>` dist name must stay stable across the `bitty-app` -> `bitty` inner rename.

### Rename binary artifact `bitty-app` -> `bitty` (CTX-0164, #264)

- Crate `bitty-app` keeps its name; only the binary artifact renames via
  `[[bin]] name = "bitty"` in `crates/bitty-app/Cargo.toml`, so
  `target/debug/bitty`, `target/release/bitty`, `ps`, and fastfetch show
  `bitty`.
- Packaging follows the artifact: root `PKGBUILD` + `packaging/PKGBUILD`
  install `target/release/bitty` to `/usr/bin/bitty`; `Formula/bitty.rb` +
  `homebrew/Formula/bitty.rb` install `target/release/bitty`; `nfpm.yaml`
  already used `src: ./target/release/bitty` (no change);
  `.github/workflows/release.yml` builds `bitty` directly and keeps
  conventional assets `dist/bitty-<target>` (`-x86_64-unknown-linux-gnu`,
  `-aarch64-apple-darwin`, `-x86_64-pc-windows-msvc.exe`, etc.) plus
  `.sha256`, `SHA256SUMS`, `provenance.json`, and `dist/bitty-<target>.deb`
  / `.rpm` / `.apk` / `.pkg.tar.zst`; Homebrew/Scoop jobs already fetch
  `bitty-<target>` assets and install as `bitty` (no URL change).
- AUR package name `bitty` unchanged. `packaging/bitty.desktop` `Exec=bitty`
  verified unchanged. No shell completions exist in-repo (no change).
- Docs updated where `bitty-app` was used as the command (`soak-0.0.1.md`
  headless/window paths, `perf-baseline.md` next step, `tools/perf/*`,
  `scripts/visual-smoke.sh`, `scripts/dogfood.sh` messages); `cargo -p
 bitty-app` crate selectors and historical `0.0.1` release records kept.

### Post-0.0.1 maintenance — triage 2026-09-01 (CTX-0117, docs-only)

- Triage 0.0.1: GitHub Issues 0 open (verified `gh issue list` 2026-09-01, no new bugs from 0.0.1); crates.io 9/9 at 0.0.1 verified (`cargo info bitty-*` all show 0.0.1, docs.rs 302 to `bitty_*`), GitHub Release `v0.0.1` prerelease 3 assets (`bitty-v0.0.1-linux-x64.tar.gz` 3.3 MiB `18f9ceeef4930f08cc825541a63f4e7024bf19ec3ea69ca9621be95407358838`, `SHA256SUMS`, `provenance.json`) downloadCount 0 each (expected immediately post-release, no user feedback yet), `recordings/compat-matrix-2026-09-01.json` 14 surfaces all self PASS.
- Fix: docs-only regression — README pre-implementation disclaimer clarified from “no published artifacts beyond dry-run” to note 9 crates at 0.0.1 on crates.io plus Linux x64 binary preview on GitHub Releases (binary `publish = false`, never on crates.io); no code, no version bump, no publish.
- Gates: `just check` PASS (fmt-check + clippy -D warnings + test + actionlint + markdownlint, 1394+ tests, 61 files), `cargo check --target x86_64-pc-windows-gnu` PASS, `cargo audit` PASS (1235 advisories, 2 allowed paste/ttf-parser `RUSTSEC-2024-0436`/`RUSTSEC-2026-0192`), `cargo deny check` PASS, no `cargo publish` (docs-only), no pin drift (toolchain `1.97.1`, MSRV `1.85`, edition `2024`, resolver `3`).
- Crates.io/docs.rs monitoring 2026-09-01: 9 published crates remain indexed, docs.rs redirects 302 for `vt`/`term-state`/`render` verify built docs; release feedback deferred to weekly patrol (next check `cargo audit`/`cargo info`).

- Post-0.0.1 development continues on `main` (tail crates `plugin-host`, `rich`, `ipc`, `agent`, `runtime`, `app`, `core` deferred to `0.1.0`).

## [0.0.1] - 2026-09-01

### Formal leaf release (Groups 1-3, 9 crates at 0.0.1)

Published in DAG order with index propagation waits and crates.io rate-limit handling (5 new crates per ~10 min window; G1 5 + lua hit 429 at 14:46:23 retried 14:46:39, G3 hit 429 until 15:06:23).

**Group 1 — Leaves (no workspace deps):** `bitty-vt` 0.0.1 (vt 0.15, 24 files 118.4 KiB 27.7 compressed), `bitty-pty` 0.0.1 (portable-pty 0.9, 14 files 75.1 KiB), `bitty-platform` 0.0.1 (winit 0.30 + raw-window-handle 0.6.2, 17 files 187.6 KiB), `bitty-config` 0.0.1 (13 files 120.7 KiB), `bitty-package` 0.0.1 (18 files 269.7 KiB), `bitty-lua` 0.0.1 (piccolo 0.3.3, 6 files 60.8 KiB).

**Group 2 — Terminal Truth:** `bitty-term-state` 0.0.1 (depends on `bitty-vt = "0.0.1"`, 25 files 225.8 KiB).

**Group 3 — Presentation branch (parallel after Group 2):** `bitty-ui` 0.0.1 (depends on `term-state`, 12 files 158.9 KiB), `bitty-render` 0.0.1 (depends on `term-state` + `platform`, 19 files 329.9 KiB, wgpu 26.0 + crossfont 0.9).

Seven tail crates remain `publish = false` at `0.0.1` (deferring `0.1.0`): `plugin-host`, `rich`, `ipc`, `agent`, `runtime`, `app`, `core`.

Toolchain pinned `1.97.1`, MSRV `1.85`, edition `2024`, resolver `3`, `just check` PASS (fmt-clippy-test-actionlint-markdownlint), `cargo check --target x86_64-pc-windows-gnu` PASS, `act -n` DRYRUN PASS, cargo audit/deny checked via CI.

Fix: `bitty-term-state` dev-dep `bitty-ui` version pin removed (path-only) to break cycle `term-state dev-> ui -> term-state` that blocked Group 2 publish after vt indexed; tests remain headless bounded (61+4+3+4+8).

### Binary preview — `bitty-app` 0.0.1 (publish = false, GitHub Releases only)

`bitty-app` never on crates.io. This tag publishes a Linux x86_64 preview: `bitty-v0.0.1-linux-x64.tar.gz` (bitty-app 11 MiB + LICENSE + README + CHANGELOG, 3.3 MiB compressed) + `SHA256SUMS` + `provenance.json` (commit, toolchain 1.97.1, Cargo.lock hash, target `x86_64-unknown-linux-gnu`). Build `cargo build -p bitty-app --release --locked`, verified `--headless` smoke (fills 1921 glyphs 21, layout-proof split/stack/overlay distinct deterministic rgba).

Future nightly `nightly-YYYYMMDD+sha` will reuse this shape with matrix linux-x64 / macos-arm64 / windows-x64, retention 14, gated on `just check` + supply-chain + Windows.
