---
title: Recordings — Persistent Workspace
description: Persistent workspace for bitty recordings, reference snapshots, and dogfooding evidence (replaces tmp, survives reboot per CTX-0092)
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Recordings — Persistent Workspace

`recordings/` is the durable scratch area for bitty (project-local `recordings/` per CTX-0092, plus umbrella `recordings/`). It survives a reboot, unlike `/tmp`, and holds only untrusted research or generated evidence.

## Layout

```text
recordings/
  README.md                         # this file
  references/
    bitty/*.snapshot.json            # headless bounded dumps (Parser -> State) via collect_dumps, 39 as of CTX-0099
    ghostty/ kitty/ wezterm/ alacritty/ xterm/ # read-only reference clones, revision+license in recordings/references/README.md (umbrella) and tmp/references/ mirrors
  manual-smoke/<YYYY-MM-DD>/         # git-ignored windowed `grim`/`hyprctl` PNGs (human-run, not committed)
```

## Policy

- Treat all `recordings/references/` material as untrusted, read-only. Never import as dependency, never edit in place.
- `recordings/references/bitty/` is generated, deterministic, bounded (`80×24`, `<16 KiB` per file, `CANONICAL_HASH_VERSION 1`, `MAX_CORPUS_BYTES 8 KiB`, `MAX_ACTIONS 4096`), `forbid(unsafe)`, no `winit`/`wgpu`/`Window`/`Surface`.
- `recordings/manual-smoke/` is git-ignored (`.gitignore` `recordings/manual-smoke/`). Commit only the filled `docs/product/manual-smoke.md` tables, not PNGs.
- Refresh reference clones via `git clone --depth 1` and record new revision + license in `recordings/references/README.md` (umbrella).

## CTX-0099 snapshot

- Base `a8735d0` (CTX-0098) → 39 snapshots after `cargo run -p bitty-compat-lab --bin collect_dumps --locked` (30 baseline + 9 dogfooding `*dogfooding*.bin`).
- Dogfooding corpora: `tests/compat/*/corpus/*dogfooding*.bin` (zsh/bash/fish, nvim/tmux/fzf/htop/ssh, alt-screen/mouse/resize/OSC 7/8/133/clipboard/Kitty/IME/DPI, each ≤310 bytes, deterministic, bounded).
- Regression: `cargo test -p bitty-compat-lab --test dogfooding_corpus` 6 tests PASS, `cargo test -p bitty-compat-lab --test compare` `total 39 self_passed 39` PASS, `cargo test -p bitty-compat-lab --test harness` `compat_corpus_is_bounded_and_deterministic` 39 PASS, no `unsafe`, no window/GPU leak.
- Differential vs Ghostty/Kitty/WezTerm/Alacritty via `crates/bitty-compat-lab/src/compare.rs` snapshot-to-snapshot (grid hash + Snapshot grid + damage), graceful skip when backend dumps absent; next bugs are differential compatibility.
