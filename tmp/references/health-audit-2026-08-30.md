# Health audit — 2026-08-30 (CTX-0083, P0 ui)

Owner: `opencode-commander` — health patrol, `bitty` repo main `1ab5fb9`
Issue: [#119](https://github.com/bitty-terminal/bitty/issues/119) — labels `docs,area:ui,P0`, milestone `v0.1.0`
Task: `CTX-0083` — Health audit: crates integrity and gates (P0 ui) — RFC OQ-004/compat
Worktree: `.worktrees/ctx-0083-health-audit` — branch `ctx-0083/health-audit` — dirty, no commit yet
Scope: read-only audit at `1ab5fb9` (`test(package): add hostile transaction and isolation evidence (CTX-0080) (#116)`), descendant of `91705be`

## Verdict — PASS, no window/GPU leak, forbid(unsafe) intact

All mandatory local gates green before any PR. No `TODO`/`FIXME` introduced. References `hyprland`/`waybar` remain read-only under `tmp/references/`, never executed or imported.

## Gates (mandatory before any PR) — all green

Executed in `bitty/` (workspace root) at `1ab5fb9` on `1.97.1` channel `rust-toolchain.toml:1`.

| Gate | Command | Result | Evidence |
|------|---------|--------|----------|
| host check | `cargo check --workspace --all-targets --locked` | PASS | `Finished dev [unoptimized+debuginfo] in 0.18s` — exit 0 |
| windows cross | `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked` | PASS | `Finished dev` — exit 0 — `Cargo.toml:workspace.package` `rust-version 1.85` compatible |
| clippy | `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS | `Finished dev in 0.14s` — exit 0 — `cargo clippy --workspace --all-targets --locked -- -D warnings` (`justfile:clippy`) |
| tests | `cargo test --workspace --all-targets --locked` | **983 passed, 0 failed** | 30 suites — `awk sum` 983 — `cargo test --workspace --all-targets --locked` (`justfile:test`) — matches task spec `983 passed` |
| just check | `just check` = `fmt-check + clippy + test + actionlint + markdownlint` | PASS | `cargo fmt --all -- --check` PASS; `clippy` PASS; `cargo test` PASS (re-run, same 983); `actionlint -color` PASS; `bunx --bun markdownlint-cli2@0.23.1` — `Linting: 44 files / Summary: 0 issues in 0 files` — exit 0 |
| workflows | `act -n` (dry-run) for `.github/workflows/ci.yml` + `codeql.yml` | PASS — 7 jobs | `Quality gates`, `MSRV 1.85 check`, `Linux Wayland`, `Linux X11 (xvfb)`, `Supply chain (deny/audit)`, `CodeQL Analyze (rust)`, `CodeQL Analyze (actions)` — each `🏁 Job succeeded` — 2 skipped `macos-14`/`windows-latest` unsupported locally as expected |
| supply chain | `cargo deny check` | PASS | `advisories ok, bans ok, licenses ok, sources ok` — `deny.toml` `allow-licenses` MIT/Apache-2.0/BSD-3-Clause etc. — 2 duplicate `thiserror` 1.0/2.0 warnings only (allowed `multiple-versions = warn`) |
| audit | `cargo audit` (`deny.toml:advisories.ignore`) | PASS — 2 allowed warnings | `RUSTSEC-2024-0436 paste 1.0.15 unmaintained` + `RUSTSEC-2026-0192 ttf-parser 0.25.1 unmaintained` — both in `deny.toml:[advisories].ignore` — `cargo audit` exits 0 with `warning: 2 allowed warnings found` |

No `file:line` defects to report — all gates exit 0, zero clippy warnings (`-D warnings`), zero fmt diff, zero markdownlint issues.

## No window/GPU leak — PASS

- `tests/compat/harness.rs:1` — `//! Compatibility lab harness — headless, bounded, forbid(unsafe).` — `MAX_CORPUS_BYTES = 8 KiB`, `MAX_ACTIONS = 4096`, no `winit`/`wgpu`/`Window`/`Surface`.
- `crates/bitty-compat-lab/src/lib.rs:1` + `crates/bitty-compat-lab/tests/harness.rs:1` + `tests/compat/harness.rs:1` — all `#![forbid(unsafe_code)]`.
- `crates/bitty-compat-lab/tests/harness.rs:157` — `fn no_window_gpu_leak_in_corpora()` — PASS (3/3 harness tests ok: `compat_corpus_is_bounded_and_deterministic`, `vttest_corpora_present_and_bounded`, `no_window_gpu_leak_in_corpora`).
- `crates/bitty-platform` / `crates/bitty-render` / `crates/bitty-runtime` — `winit 0.30` + `wgpu` are workspace-owned, feature-gated, never imported in `bitty-compat-lab`, `bitty-vt`, `bitty-term-state`, `bitty-core`, `bitty-ipc`, `bitty-package`, `bitty-lua` (all `forbid(unsafe)`, headless corpora).
- `bitty-runtime` docs `crates/bitty-runtime/src/lib.rs:134` — enforces `#![forbid(unsafe_code)]` with no exception; `bitty-platform/src/lib.rs:28` — `#![forbid(unsafe_code)]` with owned vocabulary over `winit`/`wgpu` types.

## forbid(unsafe) intact — PASS

- `Cargo.toml:[workspace.lints.rust] unsafe_code = "deny"` — workspace-level deny, every crate inherits `lints.workspace = true`.
- `rg -l "forbid\(unsafe_code\)" crates/ | wc -l` → **55 files** with explicit `#![forbid(unsafe_code)]` (60 `rg` hits if counting TOML `lints.rust` lines); zero `unsafe` blocks in audited crates.
- Spot-checked `file:line`: `crates/bitty-app/src/main.rs:158`, `crates/bitty-vt/src/lib.rs:47`, `crates/bitty-term-state/src/lib.rs:54`, `crates/bitty-platform/src/lib.rs:89`, `crates/bitty-runtime/src/lib.rs:176`, `crates/bitty-pty/src/lib.rs:86`, `crates/bitty-ipc/src/lib.rs:106`, `crates/bitty-compat-lab/src/lib.rs:1`, `crates/bitty-render/src/lib.rs` (headless `forbid`), `crates/bitty-ui/src/lib.rs:54`, `crates/bitty-agent/src/lib.rs:205`, `crates/bitty-package/src/lib.rs:116`, `crates/bitty-lua/src/lib.rs:53` — all intact.
- `bitty-render/crates` remain `forbid(unsafe)` even though they wrap `wgpu` — `gpu.rs` delegates unsafety to `wgpu` crate boundary, no `unsafe` in Bitty code.

## Dependencies — 18 members, 343 crates, licenses clean

- Workspace members (18): `bitty-agent`, `bitty-app`, `bitty-compat-lab`, `bitty-config`, `bitty-core`, `bitty-ipc`, `bitty-lua`, `bitty-package`, `bitty-perf`, `bitty-platform`, `bitty-plugin-host`, `bitty-pty`, `bitty-render`, `bitty-rich`, `bitty-runtime`, `bitty-term-state`, `bitty-ui`, `bitty-vt` — `Cargo.toml:members` + `cargo metadata --no-deps`.
- `Cargo.lock` — 343 crate dependencies (`grep "^name =" | wc -l`). `cargo tree | wc -l` 492 lines. Resolver 3, edition 2024, MSRV 1.85 (`Cargo.toml:workspace.package`).
- `deny.toml: licenses.allow` — MIT, Apache-2.0, BSD-3/2, ISC, Zlib, Unicode-3.0/DFS-2016, MPL-2.0, CDLA-Permissive-2.0, CC0-1.0, BSL-1.0 — `confidence-threshold 0.8`.
- `deny.toml:[graph]` — `targets = []`, `all-features = false` — bans `multiple-versions = warn` (the 2× `thiserror` 1.0/2.0 duplicates are allow-listed as warning only).
- `deny.toml:[advisories]` — `ignore = ["RUSTSEC-2026-0192", "RUSTSEC-2024-0436"]` — `cargo audit` therefore PASS with 2 allowed warnings; `deny` reports `advisories ok`.
- Pin discipline: `justfile` pins `prettier 3.9.6`, `markdownlint-cli2 0.23.1`, `actionlint 1.7.12` via `bunx --bun`; `rust-toolchain.toml` pins `1.97.1`.

## hyprland/waybar — read-only references, not executed, not imported

- `tmp/references/hyprland` @ `c91fa5a` (`c91fa5ab4d566206888c708dba66fca3646c382e`, `fullscreen: fix missing early return (#16063)`) — BSD-3-Clause (`LICENSE` Copyright 2022-2026 vaxerski) — `git clone --depth 1 https://github.com/hyprwm/Hyprland` 2026-08-30.
- `tmp/references/waybar` @ `6d60c8e` (`6d60c8e02be67bb85bb9b1ea803f2fbcf0722002`, `Merge PR #5222`) — MIT (`LICENSE` Copyright 2025 Alex) — `git clone --depth 1 https://github.com/Alexays/Waybar` 2026-08-30.
- Also retained: `ghostty@8867c37` MIT, `kitty@087b8c3` GPL-3.0, `neovim@a1de074` Apache-2.0/Vim, `wezterm@f93d903` MIT, `vttest` synthetic corpora (<8 KiB) — see `tmp/references/README.md` table and `tmp/references/panel-tabs-research-2026-08-30.md`.
- Verification (read-only, no build):
  ```bash
  git -C tmp/references/hyprland rev-parse HEAD  # c91fa5ab...
  git -C tmp/references/waybar  rev-parse HEAD  # 6d60c8e...
  head -5 tmp/references/hyprland/LICENSE      # BSD 3-Clause
  head -5 tmp/references/waybar/LICENSE        # MIT
  ```
- Research distilled in `tmp/references/panel-tabs-research-2026-08-30.md` (exa searches 2026-08-30): Hyprland dwindle BSP / Waybar `AModule`/`ALabel` provider / `winit` vs `smithay-client-toolkit` layer-shell (`wlr-layer-shell`) patterns for future `bitty-ui::layout` and optional `bitty-panel` crate behind `backend-wlr`. **No dependencies added, no code executed, no imports** — snapshots are untrusted, excluded by `bitty/.gitignore` (`/target`, `.worktrees`, `.carryctx/config.local.toml`) and not referenced in `Cargo.toml`.
- Policy: never `cargo add` hyprland/waybar; future panel work starts with pure `bitty-ui::layout` unit tests mirroring `hyprgate` `insertion_plan`/`resize_plan`, then feature-gated `bitty-panel` per `fono 4bc83bc` fallback table.

## Evidence — file:line pointers (no defects)

- `justfile:4` `fmt-check: cargo fmt --all -- --check` — 0 diff
- `justfile:7` `clippy: cargo clippy --workspace --all-targets --locked -- -D warnings` — 0 warnings
- `justfile:10` `test: cargo test --workspace --all-targets --locked` — 983/983
- `justfile:13` `actionlint: actionlint -color` — 0 issues
- `justfile:16` `markdownlint: bunx --bun markdownlint-cli2@0.23.1` — 44 files 0 issues
- `.github/workflows/ci.yml` — 5 jobs dry-run ok; `.github/workflows/codeql.yml` — 2 jobs dry-run ok (`act 0.2.x`)
- `deny.toml:1` `advisories.ignore` list; `deny.toml:20` `licenses.allow` list
- `Cargo.toml:3` 18 members; `rust-toolchain.toml:1` channel 1.97.1

## Worktree state

- Branch `ctx-0083/health-audit` at `1ab5fb9` — created via `carryctx worktree create CTX-0083 --branch ctx-0083/health-audit --path .worktrees/ctx-0083-health-audit --agent opencode-commander` — dirty, contains only this report at `tmp/references/health-audit-2026-08-30.md` (untracked, ignored-noise-free), no commit yet as instructed.
- Next: PR may be opened only after this audit is recorded; all gates already green locally so CI expected green.

---
*Generated 2026-08-30 by `opencode-commander` — `bitty` repo `main 1ab5fb9` — `CTX-0083` health patrol.*
