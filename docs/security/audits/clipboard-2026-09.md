---
title: Clipboard Security Audit - 2026-09 (R-004)
description: Independent revision-locked security audit of suspicious paste inspection and OSC 52 read/write gating for R-004.
category: security
audience: security-reviewer
document_type: audit
status: draft
---

<!-- markdownlint-disable MD025 -->

# Clipboard Security Audit - 2026-09 (R-004)

## Independence and review scope

- Auditor: **core-security-auditor**, independent of the implementation agent that changed the runtime, platform, and VT crates.
- Task: CTX-0097, correcting the earlier CTX-0091 audit.
- Reviewed repository revision: `de134ec3a09b96ce58ef2cece20d15b0c041879a` (`de134ec`). This is the exact implementation revision under review, not the later worktree tip or the prior audit's `2bf194d`.
- Reviewed implementation paths: `crates/bitty-runtime/src/paste.rs`, `crates/bitty-runtime/src/runtime.rs`, `crates/bitty-runtime/tests/suspicious_paste.rs`, `crates/bitty-runtime/tests/selection_clipboard.rs`, `crates/bitty-platform/src/clipboard.rs`, and `crates/bitty-vt/src/parser.rs` at `de134ec`.
- Delta reviewed after the independent blocker: public `Runtime::paste_text` had no checked-in callers but was externally reachable because it was `pub` and bypassed inspection. It now routes through `request_paste`; `public_paste_text_api_is_also_gated` proves suspicious input is held pending and only delivered after confirmation. This is a production behavior correction, not a test-only boundary.
- Follow-up remediation in this worktree: `request_paste` now applies a local UTF-8-safe truncation helper and the 8192-byte `CLIPBOARD_MAX_BYTES` limit before inspection. This bounds `paste_text`, `paste_text_via_gate`, and `request_paste` consistently without expanding `bitty-platform`'s public API. `string_paste_apis_bound_oversized_ascii` and `string_paste_apis_bound_oversized_multibyte_suspicious_input` cover clean delivery, pending storage, valid UTF-8, and confirmation after truncation. This remediation is uncommitted and therefore has no baseline CI evidence yet.
- Follow-up API-boundary remediation: repository search found no external consumers of `inspect_paste`, `bracketed_wrap`, `PendingPaste`, or `PasteInspection`; their uses were internal runtime code and direct test helpers. Those implementation helpers, the inspection result, and the pending representation are now crate-private, with no public `PendingPaste.text` field, public constructor, or crate-root inspection export. Public runtime paste methods return only the bounded confirmation-required decision (`bool`), while detailed flags remain internal. This remediation is uncommitted and therefore has no baseline CI evidence yet.
- Follow-up pending-state remediation: a suspicious request received while another suspicious paste is pending is rejected with `true`; the first pending text remains available for explicit confirmation or cancellation. `sequential_suspicious_requests_preserve_first_pending_paste` covers preservation and delivery. No pending paste is silently replaced or dropped.
- Security scope: R-004/T-04 and only P0-AC-007 (separate OSC 52 read/write policy) and P0-AC-008 (suspicious paste inspection), as defined by the canonical `bitty-docs` security corpus. This report does not authorize unrelated risks or acceptance of the product as a whole.

## P0-AC-008: suspicious paste inspection

At `crates/bitty-runtime/src/paste.rs:101` in `de134ec`, `inspect_paste` scans characters once and returns flags for C0 controls (except tab), NUL, ESC, CR, LF, C1 controls `U+0080..U+009F`, and the listed BiDi/zero-width characters. Clipboard input is truncated to `CLIPBOARD_MAX_BYTES = 8192` before inspection. The implementation is bounded and uses `#![forbid(unsafe_code)]` in the relevant runtime and platform modules.

The checked-in tests cover the classes and the delivery gate:

| Class or behavior                                   | Implementation flag            | Checked-in test evidence                                                                                                                                                                                                                              |
| --------------------------------------------------- | ------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| C0 `0x00..0x1F`, excluding tab                      | `has_c0`                       | `paste.rs:c0_other_triggers_c0_only`; `suspicious_paste.rs:c0_controls_trigger` iterates every value                                                                                                                                                  |
| NUL                                                 | `has_nul` and `has_c0`         | `nul_triggers_c0_and_nul`; `nul_triggers`                                                                                                                                                                                                             |
| ESC                                                 | `has_esc` and `has_c0`         | `esc_triggers_c0_and_esc`; `esc_triggers`                                                                                                                                                                                                             |
| CR                                                  | `has_cr` and `has_c0`          | `cr_triggers_c0_and_cr`; `cr_triggers`                                                                                                                                                                                                                |
| LF/newline                                          | `has_newline` and `has_c0`     | `newline_triggers_c0_and_newline`; `newline_triggers`                                                                                                                                                                                                 |
| C1 `U+0080..U+009F`                                 | `has_unicode_control`          | `unicode_control_u0080_to_009f`; `unicode_c1_controls_trigger`                                                                                                                                                                                        |
| BiDi and zero-width set                             | `has_bidi`                     | `bidi_controls_each_flag`; `bidi_and_zero_width_trigger`                                                                                                                                                                                              |
| Clean, empty, tab, determinism                      | clean/flag invariants          | `clean_text_needs_no_confirmation`, `tab_is_allowed_without_c0_flag`, `empty_is_clean`, `inspection_is_deterministic`, `paste_inspection_headless_deterministic_across_runtimes`                                                                      |
| No silent delivery and explicit confirmation/cancel | pending paste gate             | `paste_from_clipboard_suspicious_requires_confirmation_no_silent_delivery`, `each_suspicious_class_requires_confirmation_and_no_silent_path`, `request_paste_api_also_gated_no_silent_path`, `confirm_false_drops_without_delivery_cancel_idempotent` |
| Bracketed paste is defense-in-depth                 | confirmation precedes wrapping | `bracketed_paste_is_defense_in_depth_only_never_bypasses_gate`                                                                                                                                                                                        |
| Size and UTF-8 boundary                             | 8192-byte truncation           | `truncation_at_char_boundary_before_inspection_deterministic`, `embedded_emoji_boundary_truncation_remains_valid_utf8`                                                                                                                                |

The baseline checked-in revision `de134ec` contains **19 integration tests** in `crates/bitty-runtime/tests/suspicious_paste.rs`; the current worktree contains **23 integration tests**, **13 unit tests** in `crates/bitty-runtime/src/paste.rs`, and **four remediation tests added in this diff**. The earlier commit message's `+19` is consistent with the baseline; the earlier audit's claim of `suspicious_paste 17` was incorrect. The four remediation tests are not covered by the baseline CI runs.

## P0-AC-007: OSC 52 read/write policy

- At `crates/bitty-vt/src/parser.rs:766`, OSC 52 query payloads dispatch as `ClipboardOp::Read`; non-query payloads dispatch as `ClipboardOp::Write`. `osc_clipboard_distinguishes_query_from_write` covers this separation.
- At `crates/bitty-runtime/src/runtime.rs:381` and `:529`, read and write permissions default to `false` independently. At `:2083` and `:2092`, denied operations return without clipboard I/O or a read reply.
- `set_osc_clipboard_write_allowed` and `set_osc_clipboard_read_allowed` are separate controls. `selection_clipboard.rs:242` (`osc52_write_is_bridged_to_clipboard`) checks default denial, write grant, read denial, and independent read consent.

This evidence supports a fail-closed policy for the two OSC 52 operations in the reviewed runtime path. It does not establish a broader capability model beyond P0-AC-007.

## RS-1..RS-7 evidence

- **RS-1:** The baseline `de134ec` test suite passed in the recorded CI run. The current uncommitted remediation listing independently reproduces 23 `suspicious_paste` tests and 13 `paste` unit tests; the four added tests cover the repaired public API, string bounds, and pending-state preservation and are not evidence from the baseline CI run.
- **RS-2:** The checked-in C0/C1/BiDi matrix, oversized-input truncation tests, and deterministic headless tests provide adversarial and limit coverage. This is test evidence, not a claim of exhaustive fuzzing of all clipboard backends.
- **RS-3:** The baseline names tests for each P0-AC-008 class, clean/empty/tab negatives, confirmation/cancel behavior, bracketed-paste interaction, and P0-AC-007 deny/grant cases. The independent review found the public `paste_text` bypass, so RS-3 was not satisfied by the baseline. The added regression closes that specific implementation path in this uncommitted worktree, but platform-backend and real-window UX sub-cases remain unclosed and are explicitly not treated as passing evidence.
- **RS-4:** The baseline clipboard path was bounded, but the independent review found that public string entry points bypassed that bound. The current uncommitted remediation applies an equivalent 8192-byte UTF-8 char-boundary helper before all string-paste inspections and storage, with focused oversized ASCII and multibyte tests. RS-4 still requires CI evidence on the remediation revision. This is a post-acquisition retained/inspection bound, not a strict peak-memory bound: native `Clipboard::get_text` materializes the OS-provided `String` before truncation, `set_text` truncates before its backend call, and a caller-provided `&str` may already exist in memory before `paste_text` copies and bounds it.
- **RS-5:** Reproducible CI evidence for the exact SHA is available at [CI run 33309613627](https://github.com/bitty-terminal/bitty/actions/runs/33309613627), whose successful Quality gates job is [99252181094](https://github.com/bitty-terminal/bitty/actions/runs/33309613627/job/99252181094). Its recorded steps include Rust format, Clippy, workspace tests, and Actionlint. The same run records successful MSRV, Windows, macOS, Linux X11, Linux Wayland, and supply-chain jobs. The exact-SHA CodeQL evidence is [run 33309613648](https://github.com/bitty-terminal/bitty/actions/runs/33309613648), with successful Rust and Actions jobs [99252180879](https://github.com/bitty-terminal/bitty/actions/runs/33309613648/job/99252180879) and [99252180760](https://github.com/bitty-terminal/bitty/actions/runs/33309613648/job/99252180760). These links are the reproducible CI/HTTP evidence references; CarryCtx CTX-0097 records this correction and its checkpoint.
- **RS-6:** No secret-handling behavior is in this narrow clipboard review. Scope is instead evidenced by the separate OSC 52 read/write permission matrix; unrelated secret, IPC, plugin, and package controls remain outside this report.
- **RS-7:** Safe-mode invariance is not established by this audit. The Risk Evidence RFC requires same-revision hostile-fixture safe-mode re-verification only where the linked P0-AC set intersects safe mode; R-004's P0-AC-007/008 mapping does not itself intersect that safe-mode set. Therefore this report neither claims RS-7 evidence nor treats the audit artifact's existence as RS-7 satisfaction. Any shared safe-mode or startup claim requires its own applicable risk evidence. The existing safe-mode tests in the workspace are evidence for other controls, not a same-revision R-004 RS-7 artifact.

## Residual risks and scope limits

- **Platform clipboard backends:** The deterministic assertions use `Clipboard::new_headless()`, and the new string-bound tests use the local runtime helper without native clipboard access. CI executes platform jobs, but this audit does not independently validate every `arboard` backend's native selection, ownership, encoding, failure, or permission behavior on X11, Wayland, macOS, and Windows. These remain unclosed RS-3 negative/limit dimensions, and backend-specific regressions remain risk. Native clipboard acquisition can materialize an oversized payload before post-acquisition truncation, so the 8192-byte bound must not be read as a strict peak-memory guarantee.
- **Real-window UX:** The tests do not prove that a real window always presents, focuses, and safely handles the confirmation prompt, nor that keyboard/mouse dismissal and concurrent paste requests preserve the gate. This is an unclosed RS-3 integration/negative dimension; windowed UX and human-factors testing remain required before treating the control as operationally complete.
- **P0 boundaries:** The verdict is limited to R-004 and P0-AC-007/008. It does not cover OSC 52 interoperability beyond the reviewed parser/runtime paths, clipboard exfiltration by other integrations, or any other P0 acceptance criterion. `Mitigated -> Accepted` and time-bounded risk acceptance remain outside this audit.

## Verdict

The independent review found and corrected a production-reachable suspicious-paste bypass in `Runtime::paste_text`. The focused regression covers the repaired API in this uncommitted worktree, not the exact baseline `de134ec` CI artifact. RS-3 remains open because platform-backend and real-window UX dimensions lack the required reproducible evidence. Under the Risk Evidence RFC's no-partial-closure rule, this audit **does not authorize R-004 Open -> Mitigated**. R-004 remains **Open** pending a reviewed remediation revision and follow-up evidence for those sub-cases. This is not authorization for `Accepted`.

---

Auditor: **core-security-auditor** (CTX-0097) - 2026-08-31.

Canonical corpus: `bitty-docs/docs/security/p0-acceptance-criteria.md` (P0-AC-007/008), `risk-register.md` (R-004), `threat-model.md` (T-04), and `evidence-matrix.md`.
