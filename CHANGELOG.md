# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Configurable panel content inset (CTX-0333):** `decoration.content_inset`
  (logical px, default `6`, range `0..=32`, safe mode `0`) pads the painted
  content inside each view frame, so text no longer sits flush against the
  panel margin line. Exposed via `init.lua`, validated fail-closed, and
  mirrored in `bitty check`/`bitty inspect`.

### Changed

- **Unified panel gaps (CTX-0333):** `decoration.gaps_in` now defaults to `6`
  (was `4`), matching `decoration.gaps_out`, so the default sibling
  (panel-to-panel / panel-to-terminal) and container gaps read as one spacing.
  The coherent model is `effective gap = decoration.gap * DPI_scale +
layout.gap_cells * cell_axis`; with the default `layout` cell gaps of `0`
  both effective gaps are `6` logical px. Views now paint inside the
  `border + content_inset` padding, which changes default tiled content grids.

### Help popup occludes grid text (CTX-0336, issue #559)

- Fixed the `Mod+backtick` which-key help popup (`Mod+?`) painting the
  underlying terminal text through its opaque background and border: `DrawList`
  composites every glyph after every fill, so base-grid glyphs repainted over
  the panel. The panel now drops pre-existing glyphs whose pixel box intersects
  its frame, keeping the popup fully opaque regardless of `window.opacity`.
- Added a headless regression (`help_panel_occludes_grid_inside_its_frame`)
  that brackets the panel from its border pixels and proves the underlay no
  longer bleeds through.

## [0.0.20] - 2026-09-11

### Release highlights

- **Plugin runtime Gap A (CTX-0328):** per-plugin `!Send` VM lifecycle and
  atomic activation landed across `bitty-plugin-host`, `bitty-lua`,
  `bitty-runtime`, and `bitty-app`; bundled plugins (including the activity
  plugin) now load and activate, and `--safe` starts with no third-party VM.
- **`ctl` D1/D2/D3 correctness (CTX-0321/0322/0323):** terminal text commands
  render grid text instead of the Rust `Debug` snapshot; workspace ids are
  stable `ws{seq}` values across `new`/`list`/`focus`/`close`/`move`;
  `terminal spawn` creates an observable pane session.
- **Kitty graphics:** APC `G` parser wiring with base64 unwrap (CTX-0255/0256),
  bounded PNG/RGB decode (CTX-0247), placement/rasterize/composite into the
  present path (CTX-0248), per-pane origin binding with a per-frame blit
  budget and raster cache (CTX-0252/0254), GPU display gate and saturating
  origin math (CTX-0253), and real-GPU texture upload + blit (CTX-0291).
- **Workspace decoration and presentation (CTX-0292):** Core-owned gaps px,
  border, and radius, plus SDF rounded decoration fills and inner-arc glyph
  clipping (CTX-0311) and scrollbar overlay tracking the decorated content
  frame (CTX-0313); `window.padding`/`opacity` wiring, premultiplied alpha,
  theme cursor hue, tall-glyph clipping, and session-less split present fixes.
- **Configuration matrix:** arrow-key default variants for HJKL actions
  (CTX-0262), Mod+M/HJKL pinned to both mods with a Mod-aware resize variant
  (CTX-0258), configurable leader/mod key, F-keys plus INS/DEL/HM/END/PU/PD as
  bindable chrome chords, per-window font zoom, which-key help popup,
  focus-follows-mouse, and knob-effect config tests (CTX-0295).
- **Plugin CLI (CTX-0150):** `bitty plugin` subcommands with capability gates;
  `required_services` manifest resolution (CTX-0277) and 0600 managed-manifest
  permissions (CTX-0293).
- **CarryCtx snapshot publication:** moved in-repo with the external mirror
  retired (CTX-0319); publish pipeline unified with provenance and `ctxpack`
  `format_version` 2 validation (CTX-0314/0317), export-time secret redaction,
  scratch/host-path lint gates, and `just workflow-import` mirror restore
  (CTX-0316).
- **In-repo module splits:** `bitty-app` `main.rs`/`ctl.rs`, IPC
  `devtools.rs`, runtime `registry.rs`, VT `parser.rs`, and term-state
  `state.rs` decomposed into focused modules (CTX-0305/0306/0307/0308/0309/
  0310); remaining hardcode-audit literals named and host identifiers
  sanitized (CTX-0299/0300/0301).
- **Platform and terminal correctness:** Windows ConPTY Tier-1 slice
  (CTX-0268), `terminal.scrollback` capacity and `terminal.shell` honored
  (CTX-0297/0298), dropped mouse validation restored (CTX-0303), narrow reflow
  tail rewrap (CTX-0266), and live split resize updating primary grid plus PTY
  winsize (CTX-0269).

### Release engineering

- Workspace version bumped `0.0.1 -> 0.0.20` (the first Cargo bump since the
  `0.0.1` leaf release; tags `v0.0.2`-`v0.0.19` never moved the in-repo
  version), applied to `Cargo.toml`, `Cargo.lock`, internal `path` dependency
  pins, root and `packaging/PKGBUILD*` `pkgver`, `nfpm.yaml`, and the
  `flake.nix` fallback. `scripts/check-release-version.sh` now keeps
  `bitty --version`, packaging metadata, and the release tag in agreement.

### Plugin host runtime: per-plugin VM lifecycle and activation (CTX-0328)

- Ratified `plugin-host-runtime-rfc` Gap A implemented across the policy,
  mechanism, orchestration, and application layers. `bitty-plugin-host` stays
  VM-free; `bitty-runtime` gains a `plugin_runtime` module owning one `!Send`
  `piccolo` VM per `(PluginId, generation)` on a single executor thread with
  the `Unloaded -> Loading -> Activating -> Active -> Suspended -> Disposing
-> Disposed` lifecycle.
- `bitty-lua` gains the VM seam: read-only `bitty` host-module injection,
  rooted source-only `require`, bounded depth/node/byte marshalling,
  generation-scoped registration capture, and budget-enforced callback
  invocation. The retained stdlib piccolo omits is installed (`utf8`,
  restricted `os.time/clock/date`, `string.byte/char/format`, `table.concat`,
  `table.sort`).
- Activation runs the fixed `init.lua`, validates the captured registrations
  against the manifest, and commits atomically; any failure purges the policy
  host generation (identity, command ownership, subscriptions) and records a
  terminal failure, so no partial activation survives and a retry starts clean.
  Minimal host services (`terminal.snapshot` sync bounded, `notify.show` async
  hand-off, `store.*` sync atomic quota-bounded, `settings.*` read-only) and
  typed `E_TIMEOUT`/`E_*` errors.
- `bitty-app` discovers plugin packages at startup and activates them; a new
  `--safe` flag creates no third-party VM. Safe-mode eligibility comes from the
  discovery root's provenance (`SourceClass`), never a self-declared manifest
  id, so a package cannot claim bundled trust by naming itself `bitty.*`.
  Bounds: 256 KiB manifest, 4096 module files, 16 MiB tree, 1024-byte path.
- Follow-ups: Gap B source staging/integrity (CTX-0329) and Gap C hardening
  (CTX-0330).

### `terminal.shell` honored at startup spawn (CTX-0298, issue #495)

- The effective `terminal.shell` value is now used when no explicit CLI
  program is given. Precedence: CLI program > configured `terminal.shell` >
  `$SHELL` > `/bin/sh`. The frozen startup spawn recipe (split panes,
  headless fallback) replays the same resolution.
- Direct `argv[0]` throughout: no shell interpolation, no word splitting.
  The project-layer trust gate that rejects `terminal.shell` is unchanged.
- Fail-closed: config validation rejects control characters in
  `terminal.shell`; the spawn resolver trims and skips blank, oversized, or
  control-laden values, and a configured shell that fails to spawn retries
  `/bin/sh` exactly like the existing `$SHELL` failure path.
- Tests: pure precedence/fail-closed unit coverage plus bounded live-spawn
  effect tests proving the configured shell executes as `argv[0]`.

### `bitty plugin` CLI-first management (CTX-0150, issue #244)

- New `bitty plugin list|install|remove|enable|disable|info` subcommands over
  the existing draft `bitty-plugin-host` machinery (static bundled manifests,
  closed capability grammar, hash binding). Local class: no instance, no IPC,
  no plugin VM, and no plugin code is executed.
- One machine-managed manifest at
  `$XDG_CONFIG_HOME/bitty/bitty-plugins.toml` (or beside an explicit
  `--config` path): a strict bounded format whose unknown keys, malformed
  values, and over-limit files fail closed. `remove` requires `--force` and
  keeps a `.bak` of the previous manifest; installs pin
  `PluginManifest::manifest_hash()` and `enable` re-checks that pin.
- Capability consent: the prompt lists every requested capability with its
  plain-language effect and high-risk marker; `--yes` approves
  non-interactively, the interactive `[y/N]` path fails closed on EOF or a
  decline, and an update that adds capabilities blocks until re-approved
  while narrowed/unchanged sets carry forward (P0-AC-030 pattern).
- `bitty-plugin-host`: new `CapabilityRequests::all_ids()` exposes the exact
  requested-capability expansion (flat ids plus `fs.read`/`fs.write`
  patterns) shared by host activation and the CLI consent surface.
- v1 installs bundled ids only (`bitty-terminal.*`); registry/Git/local-path
  sources remain deferred with the package manager and fail closed with a
  clear error. Tests: 16 unit (`plugin`) + 1 dispatch-parse unit + 6
  end-to-end binary cases, all headless.

### Kitty placement + rasterize + composite into present path (CTX-0248, issue #426)

- New `bitty-rich` `kitty_place` layer: `a=t`/`T`/unsupported action mapping, cursor-anchored cell rects, scroll-with-content, alt-screen clear, CTX-0247 decode caps reused, no allocation before validation.
- New `DrawList.images` present layer in `bitty-render`: topmost blits above fills and glyphs, never grid truth; software and headless CPU compositors blend them. Runtime tick overlay feeds placed images into the present path.
- Known limitation: the real-GPU (wgpu) path ignores `DrawList.images` until a texture-upload path lands (documented in `crates/bitty-render/src/gpu.rs`); software/headless paths composite.
- Tests: 21 rich + 5 render + 8 runtime present tests, all headless.

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
