---
title: Text Compatibility (draft)
description: Draft Unicode text-domain and IME-adjacent compatibility contract for Phase C deep — CTX-0079 (wide, combining, emoji ZWJ, ambiguous, terminfo, IME overlay vs commit)
category: specifications
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Text Compatibility (draft)

## Status and provenance

- Status: **draft**. Text-domain companion to `docs/product/unicode-ime.md` for **CTX-0079** — *Implement Unicode/Text compatibility and IME model (Phase C deep)* — branch `ctx-0079/unicode-ime` at `91705be`, agent `opencode-commander`.
- Ownership: bitty **CTX-0079**. Does not close OQ-004, does not accept the `v0.2` slice, does not weaken normative security controls.
- Authority: when `compatibility-milestone-rfc.md` accepts the differential contract, this draft folds into the accepted spec and `docs/product/unicode-ime.md` § Unicode is retired or merged accordingly. Nothing here is normative until an ADR/RFC accepts it.

## Scope

Unicode scalar width, grapheme-adjacent handling, and IME preedit/commit boundaries only. Editor shaping, bidi, and fallback are explicitly out of scope (pending text RFC per ADR-0004). The VT parser obligations and Terminal State RFC remain the normative parents; this file clarifies the text-domain choices that sit between them.

## Unicode — width

- **Single implementation:** `crates/bitty-term-state/src/cell.rs::char_cell_width`.
- **Algorithm:** `if cp < 0x0300 { 1 } else if is_zero_width(cp) { 0 } else if is_wide(cp) { 2 } else { 1 }`. Pure `matches!` tables, no allocation, no `unsafe`, deterministic, headless.
- **Zero-width:** combining marks (`0300..036F` + script ranges), variation selectors (`FE00..FE0F`, `E0100..E01EF`), zero-width spaces/marks (`200B..200F`, `2060..2064`, `FEFF`), direction controls (`202A..202E`), ZWJ (`200D` zero-width) — see `is_zero_width`.
- **Wide:** CJK blocks, Hangul, Fullwidth, emoji (`1F300..1F9FF`, `1F680..1F6FF`, `1F900..1F9FF`), Extension B/C (`20000..3FFFD`) — see `is_wide`. Compact approximation; authoritative tables follow the future text RFC.
- **Ambiguous:** treated as `1` (no `EAW=A → 2` rule). Documented divergence from some platform `wcwidth` tables; differential harness may list it as known divergence.
- **State mapping:** `char_cell_width` → `Cell::width`/`Cell::wide_spacer` → `State::check_invariants` (`width ∈ {1,2}`, no orphan spacers). All `forbid(unsafe)`, bounded by grid geometry, and exercised headlessly. Invalid UTF-8 → `U+FFFD` one cell via `vte` collector.

## IME — overlay vs commit

- See `docs/product/unicode-ime.md` (§ IME model) for the full pipeline draft. This draft restates only the text-domain bite:
- **Preedit** is ephemeral overlay, never `Snapshot`/`State`/`scrollback`/`damage`. Rendering is inline decorated overlay (underline + caret) above the cursor; grid invariants are untouched.
- **Commit** is bounded UTF-8 bytes through the single PTY writer queue (same path as `encode_key_event`). `IME_PREEDIT_MAX_SCALARS = 256`, `IME_COMMIT_MAX_BYTES = 256` per commit; truncation is deterministic char-boundary cut (same as `BoundedString`).

## Terminfo

- See `docs/product/unicode-ime.md` (§ Terminfo / `TERM` contract) and `terminfo/README.md`. Default `TERM=xterm-256color` until a `bitty` entry is published and the compatibility matrix updates.

## Next

- Pin the authoritative width tables per the text RFC (ADR-0004) and replace the compact `char_cell_width` tables with generated ones when accepted.
- Land `terminfo/bitty.ti` draft and `bitty-platform` IME seam.

## References

- `bitty-docs/docs/specifications/terminal-state-rfc.md`, `crates/bitty-term-state/src/cell.rs`, `crates/bitty-vt/src/parser.rs`, `crates/bitty-pty/src/builder.rs`, `tests/compat/harness.rs`, `docs/product/unicode-ime.md`.
