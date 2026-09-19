# TODO — Bitty Core program

Working register for the Bitty Core pivot. The owner directive (2026-09-19):
all effort goes to the `bitty` repository until its features, documentation,
and tests are done; plugin and AI work resumes only after Core is stable.
This file tracks the program-level plan; the per-item backlog lives in the
GitHub issue tracker (`bitty-terminal/bitty`, milestone `v0.1.0`) as ten
epics with sub-issues.

Keep this file short: program state, epic index, and the operating rules.
Per-item detail belongs to the issues; completed rows move to git history.

## Program state

- Phase: **Core pivot active** (2026-09-19). `fix`-label backlog cleared;
  docs corpora synchronized; research 044-059 captured.
- Baseline: `bitty` `main` with a 21-member workspace; docs submodule pinned
  to `bitty-terminal-docs` (`docs/`, pin trails `main` by design).
- Backlog: enumerated read-only from the docs corpora and registers
  (~212 rows; ~127 net-new Core items) and filed as GitHub issues.
- Gates: `just check` is the local gate; CI (`Quality gates`/`MSRV`/
  `Windows`/`Linux`/`macOS`/`Supply chain`/`CodeQL`) is the merge gate.

## Epic index (issue tracker)

| Issue | Epic                                        | Focus                                                                   | Backlog section                         |
| ----- | ------------------------------------------- | ----------------------------------------------------------------------- | --------------------------------------- |
| #969  | M1 / v0.1 correctness — the milestone gates | Protocol deltas, verification evidence, cross-platform matrix, sign-off | compatibility, feature gap              |
| #971  | Core mechanisms still unwired               | Folding, hints, composer, Scene, LayoutProvider, modes, drag/resize     | compositor, semantic terminal           |
| #972  | UI/UX candidate program                     | PW-1..PW-10 Panel/Workspace interaction; U-1..U-9 UI runtime            | panel-workspace-interaction, ui-runtime |
| #973  | Runtime / execution frontier                | Job outcomes, resource ceilings, write-lease, authorization             | execution supervisor                    |
| #974  | Performance and evidence                    | PB-1..PB-7 gates, soak automation, compat matrix                        | performance, evidence                   |
| #975  | Security and risk closure                   | Every Open register risk to Verified/Mitigated                          | risk register, P0-AC                    |
| #976  | DevTools track                              | Debug protocol, observability, trace lifecycle, A1-A3 gates             | devtools RFC                            |
| #977  | Release and packaging                       | 0.0.21 gate, deferred formats, docs sync                                | release distribution                    |
| #978  | Documentation synchronization               | Stale rows, status truth, candidate promotion                           | gap analyses, README status             |
| #979  | Housekeeping and infrastructure             | Gates, hygiene, workspace                                               | justfile, AGENTS.md                     |

## Operating rules for this phase

1. **Core first.** No plugin or AI implementation work starts until the Core
   P0 set and the M1 gates are done.
2. **Issue-first.** Every change maps to a GitHub issue (epic + sub-issue) and
   a CarryCtx task; the issue carries labels (`feat`/`fix`/`docs`/`chore` +
   `P0`/`P1`/`P2` + `area:*`) and milestone `v0.1.0`.
3. **Docs stay synchronized.** A change that alters behavior, contracts, or
   status updates the owning document in the same wave (epic 9); the docs
   corpora stay self-contained (no research references).
4. **Evidence, not claims.** Behavior is `Implemented` until independent
   verification lands; only then may a document say `Verified`.
5. **Blocked items stay blocked.** Items marked `BLOCKED` await an owner
   decision on the named open question; do not start them early.
6. **Gates stay green.** `just check`, `cargo clippy -D warnings`, and the
   full test suite must pass before review; main red is the top priority.

## Current priority order

1. Main-health and review items already in flight (present-golden
   reconciliation, parser hardening, chrome/overlay ownership).
2. M1 gaps: fuzz targets, the two missing protocol deltas, evidence
   verification, and the stale-row documentation sync.
3. Accepted-contract violations and unwired mechanisms (#971 and #973).
4. The UI/UX candidate program (#972) and evidence automation (#974).
5. Security closure (#975) as parallel capacity allows; DevTools
   (#976) alongside Core; release (#977) gated on the P0 set.

## Blocked / open

- Epic items blocked on owner decisions: `OQ-050` (anchor identity),
  `OQ-051` (panel content path), `OQ-052` (window forms/Mod),
  `OQ-056` (capability API), `OQ-058` (panel provider), `OQ-083` (write
  lease), `OQ-084` (identity ontology), `OQ-085` (trust levels),
  `OQ-086` (sensitive input), `RFC-OQ-3/8/9` (panel placement/budget/
  save-restore).
- Plugin-lane issue #767 (V-C signature scheme) remains open and is out of
  this phase's scope.
