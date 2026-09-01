# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
