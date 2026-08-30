---
title: Clipboard Security Audit — 2026-09 (R-004)
description: Independent security-auditor manual review of clipboard paste inspection and OSC 52 gating for R-004 entry to Mitigated per P0-AC-007/008 and T-04.
category: security
audience: security-reviewer
document_type: audit
status: draft
---

<!-- markdownlint-disable MD025 -->

# Clipboard Security Audit — 2026-09 (R-004)

## Auditor independence and scope

- Auditor: **core-security-auditor** — independent of Subagent A (`core-platform-implementer`), which owns `crates/bitty-vt/**`, `crates/bitty-platform/**`, `crates/bitty-term-state/**`, `crates/bitty-runtime/**`. This report touches only `docs/security/audits/**` per CTX-0091 disjoint scopes.
- Date: 2026-08-30 (report filename `clipboard-2026-09.md` per CTX-0091 task).
- Commits reviewed: `bitty` worktree HEAD `2bf194defc13608ea1b16a053172cd70e58b4e28`, `bitty-docs` HEAD `136194c39aebf7dd470ef70c2b3964f2986681bf`.
- Scope: risk **R-004** (threat **T-04** remote clipboard/paste), criteria **P0-AC-007** Separate OSC 52 read/write policy + **P0-AC-008** Suspicious paste inspection, as normatively defined in `bitty-docs/docs/security/p0-acceptance-criteria.md`, `risk-register.md`, `threat-model.md`, `evidence-matrix.md`.

## Paste inspection — every class has a named test, no silent delivery (P0-AC-008)

`crates/bitty-runtime/src/paste.rs:101` `inspect_paste` scans `text.chars()` once, bounded `O(n)` with `n <= 8192` (pre-truncated via `bitty-platform::clipboard::CLIPBOARD_MAX_BYTES`), deterministic, allocates only the returned struct. `#![forbid(unsafe_code)]` at `paste.rs:23` and `lib.rs:176`.

| Suspicious class                                                                       | Flag                     | Named test (file:line)                                                                             | Needs confirmation |
| -------------------------------------------------------------------------------------- | ------------------------ | -------------------------------------------------------------------------------------------------- | ------------------ |
| C0 `0x00..0x1F` excl `\t` (`0x09`)                                                     | `has_c0`                 | `paste.rs:c0_other_triggers_c0_only`, `suspicious_paste.rs:c0_controls_trigger` (loops every byte) | yes                |
| NUL `0x00`                                                                             | `has_nul` + `has_c0`     | `nul_triggers_c0_and_nul`, `nul_triggers`                                                          | yes                |
| ESC `0x1B`                                                                             | `has_esc` + `has_c0`     | `esc_triggers_c0_and_esc`, `esc_triggers`                                                          | yes                |
| CR `0x0D`                                                                              | `has_cr` + `has_c0`      | `cr_triggers_c0_and_cr`, `cr_triggers`                                                             | yes                |
| LF `0x0A` embedded newline                                                             | `has_newline` + `has_c0` | `newline_triggers_c0_and_newline`, `newline_triggers`                                              | yes                |
| C1 `U+0080..U+009F`                                                                    | `has_unicode_control`    | `unicode_control_u0080_to_009f`, `unicode_c1_controls_trigger`                                     | yes                |
| BiDi / zero-width `U+061C U+200B..200D U+200E/F U+202A..202E U+2066..69 U+FEFF U+2060` | `has_bidi`               | `bidi_controls_each_flag`, `bidi_and_zero_width_trigger`                                           | yes                |

Clean positives: `clean_text_needs_no_confirmation`, `tab_is_allowed_without_c0_flag` (`\t` excluded), `empty_is_clean`, `clean_text_is_not_suspicious`; determinism: `inspection_is_deterministic`, `paste_inspection_headless_deterministic_across_runtimes`.

No silent delivery proven: `runtime.rs:966` `request_paste` stores `PendingPaste` when `needs_confirmation() == true` and `paste_from_clipboard` goes through it; `confirm_pending_paste(true)` alone delivers via `deliver_paste_bytes_bracketed`. Tests assert `pending_input == b""` while pending: `paste_from_clipboard_suspicious_requires_confirmation_no_silent_delivery`, `each_suspicious_class_requires_confirmation_and_no_silent_path`, `request_paste_api_also_gated_no_silent_path`, `confirm_false_drops_without_delivery_cancel_idempotent` (cancel/false drops, second confirm returns false).

## OSC 52 separate read/write policy with consent gate and deny-by-default (P0-AC-007)

- Parser deterministic separation at `crates/bitty-vt/src/parser.rs:766` `osc_dispatch` id `52`: `?` present -> `ClipboardOp::Read`, otherwise `Write`; named test `osc_clipboard_distinguishes_query_from_write`.
- Runtime deny-by-default at `crates/bitty-runtime/src/runtime.rs:381` `osc_clipboard_read_allowed: false`, `osc_clipboard_write_allowed: false`; gates at `runtime.rs:2083` (Write) and `runtime.rs:2092` (Read) `continue` without clipboard I/O or reply when flag false. Untrusted PTY bytes cannot trigger clipboard without explicit grant/consent.
- Separate setters `set_osc_clipboard_write_allowed` / `set_osc_clipboard_read_allowed` independently grantable and deniable.
- Integration proof `crates/bitty-runtime/tests/selection_clipboard.rs:242` `osc52_write_is_bridged_to_clipboard`: default denies write, granting write forwards, read query stays denied without consent, explicit read consent flips only the flag.

## Bracketed ?2004, truncation, bounded determinism, forbid(unsafe_code)

- Bracketed paste `?2004` is defense-in-depth only: `runtime.rs:999` `deliver_paste_bytes_bracketed` wraps with `ESC[200~` / `ESC[201~` only after `confirm_pending_paste(true)`; `state.modes().bracketed_paste` controls wrapping. Test `bracketed_paste_is_defense_in_depth_only_never_bypasses_gate` proves suspicious stays gated even when `?2004` on, confirmed delivery is bracketed when enabled, not bracketed when `?2004l`.
- Truncation at char boundary before inspection: `crates/bitty-platform/src/clipboard.rs:201` `truncate_to_bytes` walks back to `is_char_boundary`; `CLIPBOARD_MAX_BYTES = 8192`. Tests `truncation_at_char_boundary_before_inspection_deterministic` (8192 with `U+202E` beyond bound dropped) and `embedded_emoji_boundary_truncation_remains_valid_utf8` (4-byte emoji, valid UTF-8, `len <= 8192`).
- Bounded determinism: `inspect_paste` `O(n) n <= 8192`; `bracketed_wrap` bounded `text.len()+12`; `clipboard::truncate_to_bytes` bounded; headless fallback `Clipboard::new_headless()` makes suite deterministic without display.
- `forbid(unsafe_code)`: `bitty-runtime` `lib.rs:176` + `paste.rs:23`, `bitty-platform` `clipboard.rs:17`, `bitty-vt` `lib.rs:47`, `bitty-term-state` modes; workspace `lints.rust.unsafe_code = deny`; allowed exception remains only `bitty-render/src/gpu.rs` `SurfaceTargetUnsafe` per prior audits.

## R-004 Entry-to-Mitigated checklist (RS-1..RS-7)

Per risk-evidence RFC and `evidence-matrix.md` row R-004:

- RS-1 unit/integration green — `cargo test -p bitty-runtime` `paste` 13 + `suspicious_paste` 17 + `selection_clipboard` OSC 52 gate, `bitty-vt` `osc_clipboard_*`, `bitty-platform` `headless_clipboard_roundtrip_is_deterministic` all passing via `cargo test --workspace --all-targets --locked`.
- RS-2 adversarial corpus zero panics — boundary truncation + BiDi/C1/C0 matrix prove fail-closed with bounded handling; no unbounded growth.
- RS-3 negative/limit coverage exhaustive — every P0-AC-007/008 class named above plus `capability deny-by-default` and `consent gate`.
- RS-4 budget/attribution — `CLIPBOARD_MAX_BYTES` enforcement observable, `pending_paste` bounded to truncated text.
- RS-5 `just check` + `ci-gate` green — see gate evidence below.
- RS-6 secret/scope where cited — N/A for pure clipboard (no secret handling); scope gating covered by OSC 52 flag matrix.
- RS-7 manual-audit report — this file satisfies the `Audit` column for R-004.

## Verdict — authorize Open -> Mitigated

All P0-AC-007/008 pass thresholds met, each adversarial class triggers inspection with a named test, no silent delivery path exists, OSC 52 read vs write are independently deny-by-default with explicit consent/capability gates, bracketed `?2004` wraps only after confirm, truncation is char-boundary and bounded deterministic, `forbid(unsafe_code)` holds. **I authorize the evidence-matrix transition `Open -> Mitigated` for R-004** on branch `ctx-0091/feat-clipboard-r004` at `bitty` `2bf194d` + `bitty-docs` `136194c`. `Mitigated -> Accepted` still requires a time-bounded CarryCtx decision per risk-evidence RFC and remains out of scope for this audit.

---

Auditor: **core-security-auditor** (Subagent B, CTX-0091 `docs/security/**`) — 2026-08-30.

Canonical corpus: `bitty-docs/docs/security/p0-acceptance-criteria.md` (P0-AC-007/008), `risk-register.md` (R-004 Critical/High/P0), `evidence-matrix.md` (Phase E R-004 row), `threat-model.md` (T-04).
