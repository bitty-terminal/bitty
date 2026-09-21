# UX invariant guard evidence (first slice, CTX-0613)

> Status: **implemented only for what the guards and artifacts in this
> repository prove** (CTX-0613, `bitty`#1182, backlog item `UX-41`). The two
> candidate sources stay Draft and owner-pending in `bitty-terminal-docs`
> (CTX-0046): this record freezes nothing, accepts nothing, and weakens no
> accepted invariant. `ChromeCadenceModel` is a test-local contract model,
> not product code. `UX-INV-15` / `UX-INV-18` (`F-UX-5`, scene path) and
> live-soak evidence are explicitly out of scope. Theme files and
> `crates/bitty-ui/src/lib.rs` are untouched (parallel task CTX-0612 owns
> the theme contract and the module line).

## Why

The candidate UI/UX invariant set leaves seven rows uncovered with no
executable check: `UX-INV-9` (tab projection identity, `F-UX-1`),
`UX-INV-10` / `UX-INV-11` (modal authority, tier paint order, `F-UX-2`),
`UX-INV-12` / `UX-INV-13` / `UX-INV-16` (motion posture, budget failure,
`F-UX-3`), and `UX-INV-14` (chrome cadence, `F-UX-4`). The candidate
evidence record separately defines frame and behavioral artifacts but ships
no scenario. This slice closes exactly that gap for `F-UX-1` through
`F-UX-4`: one guard test per invariant plus three golden artifacts produced
headlessly from declared scenarios.

## Claim-status legend

Every statement below carries one label.

- **Shipped** — implemented and tested in the `bitty` repository (file and
  symbol cited; reproducible with the repo test suite).
- **Accepted** — decided in an accepted ADR or RFC, whether or not it is
  implemented yet.
- **Candidate** — proposed by a draft record or by this record; binds
  nothing until accepted.
- **Open** — recorded here as follow-up work with no contract of any kind.

## Guard catalog

Owning code: `crates/bitty-ui/tests/ux_invariant_guards.rs` (integration
tests; no `lib.rs` change, no new product module).

| Invariant | Follow-up | Guard test                                                                   | Status  |
| --------- | --------- | ---------------------------------------------------------------------------- | ------- |
| UX-INV-9  | F-UX-1    | `ux_inv_9_tab_reorder_preserves_identity_and_content`                        | Shipped |
| UX-INV-10 | F-UX-2    | `ux_inv_10_second_modal_fails_and_leaves_first_unchanged`                    | Shipped |
| UX-INV-11 | F-UX-2    | `ux_inv_11_tier_paint_order_is_pure_function_of_tier_and_construction_order` | Shipped |
| UX-INV-12 | F-UX-3    | `ux_inv_12_committed_state_identical_across_animation_modes`                 | Shipped |
| UX-INV-13 | F-UX-3    | `ux_inv_13_terminal_content_never_interpolated`                              | Shipped |
| UX-INV-16 | F-UX-3    | `ux_inv_16_budget_overflow_fails_closed_with_prior_state_intact`             | Shipped |
| UX-INV-14 | F-UX-4    | `ux_inv_14_chrome_ignores_keystroke_stream`                                  | Shipped |
| UX-INV-14 | F-UX-4    | `ux_inv_14_cadence_contract_only_revision_or_animation_recomputes`           | Shipped |

Notes on what each guard pins:

- `UX-INV-9` (**Shipped**): reordering a `Stack` changes z-order only; the
  sorted identity set, per-leaf content, the 1:1 panel projection record,
  and the sorted window-scope allocation set are all unchanged.
- `UX-INV-10` (**Shipped**): the second modal fails with `OverlayBusy`,
  the first overlay is byte-unchanged, and dismissing the first releases
  the authority (fail-closed, not fail-stuck).
- `UX-INV-11` (**Shipped**): two insertion orders of the same tiered
  layers produce identical tier chains and identical layouts; same-tier
  order is stable across runs with later-constructed painting above.
- `UX-INV-12` (**Shipped**): layout, visible content, and chrome frames are
  identical across disabled, interrupted, completed, and every gated
  end-mode; the committed state never flickers.
- `UX-INV-13` (**Shipped**): visible cells are a pure function of (state,
  viewport geometry), byte-identical under every presentation mode
  (accepted source: RFC-0002).
- `UX-INV-16` (**Shipped**): overlay, command, and decoration overflows
  refuse with a reported error and leave prior state intact.
- `UX-INV-14` (**Shipped**): a 200-keystroke PTY burst leaves chrome
  frames bit-identical, and the `ChromeCadenceModel` contract test pins
  revision-or-animation as the only wakeup. The model is **Candidate**
  contract text, not a product revision source; product wiring is Open
  (see below).

## Evidence scenarios

Generator: `evidence_leaf_frame_exact_and_reproducible` and
`evidence_window_frame_and_behavioral_log` in the same test file. Each
artifact carries its scenario, scope, and tolerance in its header; each
test renders its artifact twice and asserts byte equality before comparing
with the golden (evidence rule 1: two runs, one revision, comparable
artifacts). Regenerate with `UX_GUARDS_UPDATE_GOLDENS=1`; a missing golden
fails with that hint instead of claiming coverage.

| Scenario              | Scope  | Tolerance | Artifact                                                                    | Status  |
| --------------------- | ------ | --------- | --------------------------------------------------------------------------- | ------- |
| `leaf-text-exact`     | leaf   | exact     | `crates/bitty-ui/tests/testdata/ux-evidence/frame-leaf-exact.txt`           | Shipped |
| `window-tiling-exact` | window | exact     | `crates/bitty-ui/tests/testdata/ux-evidence/frame-window-exact.txt`         | Shipped |
| `modal-focus-order`   | window | exact     | `crates/bitty-ui/tests/testdata/ux-evidence/behavior-modal-focus-order.txt` | Shipped |

Exact-byte comparison is the tolerance for all three: text and geometry
have no antialiasing or timing jitter headlessly, so no allowance is
declared (**Shipped** rule, matching the candidate's default).

## Promotion-gate wiring

For the surfaces this slice covers (**Candidate** wiring, consumed only by
reviewers of this record until a source record accepts it):

- `Experimental Implementation` requires the behavioral artifact
  (`modal-focus-order`).
- `Verified` additionally requires the frame artifact at the claimed scope
  (`leaf-text-exact` for leaf claims, `window-tiling-exact` for window
  claims).
- `Compatible` additionally requires the compatibility milestone's own
  evidence (unchanged; owned elsewhere).

## Redaction and storage

Artifact content is synthetic probe strings and fixed numeric ids; no
clock, cursor blink, async status, or user content participates, so
nothing needs masking (**Shipped**). Goldens are checked in beside the
tests that produce them. Any future live or soak capture follows the
accepted DevTools defaults (user-only storage, redacted, excluded from
the public snapshot path without review) and stays supplementary to this
headless baseline (**Candidate**, inherited from the evidence record).

## Still uncovered (Open)

- `F-UX-5` (`UX-INV-15`, `UX-INV-18`, scene path): no scene types exist in
  `bitty-ui` yet; a follow-up task owns the single-path and
  semantics-mapping guards.
- Chrome product wiring: no revision source exists in the crate, so the
  cadence model cannot be wired to product code yet; the purity guard
  (`layout_with_decoration` is a pure function of tree, bounds, and
  decoration) holds the line until then.
- Motion product wiring: transitions stay gated by
  `PresentationMode::can_transition`; the guards pin that gating, not a
  motion implementation.
- Live soak evidence: supplementary by design; no scenario is claimed.
- Nondeterministic content policy (clock, blink, async status): freeze,
  inject, or mask at scenario-build time when such content first appears.

## References

- `bitty-terminal-docs/specifications/ui-ux-invariant-set-candidate.md`
  (Draft, CTX-0046): `UX-INV-1..18`, `F-UX-1..5`.
- `bitty-terminal-docs/specifications/visual-regression-evidence-candidate.md`
  (Draft, CTX-0046): artifact definition, scopes, tolerances, gates.
- `bitty`#1182: owning issue (`UX-41`, `P1`, `area:ui`, `v0.1.0`).
- Parallel task CTX-0612 (`bitty`#1181): theme token contract; owns theme
  files and the `lib.rs` module line this slice does not touch.
