# Verify-first batch evidence (CTX-0701; Issues #1119, #1088)

Source: epic #978 (DOC-05 scope: each OQ closure needs its topic
document and decision register updated in the same change) and the
sibling epic #975 (SEC-19 scope: every R-xxx row with unit /
integration / adversarial / ci-gate / manual-audit columns complete
and linked).
Branch: `ctx-0701/verify2`. No product code changed in this task;
verification plus this evidence record only. References to the
`bitty-docs`, `bitty-terminal-docs`, `bitty-plugins-docs`, and
`bitty-ai-docs` checkouts are read-only; cross-repository edits are
out of scope for this worktree.

## Method

- Read the issue bodies via `gh api` (#1119 DOC-05, #1088 SEC-19,
  parents #978 and #975). Neither issue carries comments.
- Parsed the `bitty-docs` open-questions register
  (`docs/decisions/open-questions.md`): 100 OQ rows, 41 `Accepted`,
  59 `Open`. Every `Accepted` row cites its closing ADR, RFC,
  specification, or decision record.
- Checked the `bitty-docs` decision register (`docs/decisions/index.md`)
  for a closure entry per `Accepted` OQ: the "Accepted foundation
  artifacts" table plus the accepted-contract bullet list.
- Swept the closing artifacts' frontmatter in the sibling checkouts:
  21 RFC / decision documents, all `status: accepted` (plus ADR 0010
  sampled `accepted`).
- Spot-checked topic-document updates for the September closures
  (OQ-033/034/035, OQ-039/040/041/042/045, OQ-053).
- Swept in-repo OQ state claims over `specifications/*.md` against the
  register (22 claims).
- Parsed the `bitty-docs` evidence matrix
  (`docs/security/evidence-matrix.md`): 22 R rows, 8 columns each;
  recorded per-row `State`.
- Confirmed existence of the cited auditor artifacts and fuzz targets
  from the owning checkouts (`docs/security/audits/` in the `bitty`
  checkout with the `docs/` submodule mounted; `fuzz/fuzz_targets/`;
  `5daf686` is an ancestor of this head).

## Verdicts

| Issue | Backlog item                            | Verdict                  |
| ----- | --------------------------------------- | ------------------------ |
| #1119 | DOC-05 OQ register / topic-doc sync     | CLOSE_OK                 |
| #1088 | SEC-19 evidence matrix Phase E          | EVIDENCE_ONLY, blocked   |

## DOC-05 OQ register / topic-doc sync (Issue #1119): CLOSE_OK

- Decision-register half is complete for all 41 `Accepted` OQs. The
  "Accepted foundation artifacts" table closes OQ-001..OQ-032 except
  OQ-020, which is recorded in the accepted-contract bullets (ADR 0008
  headless, accepted 2026-08-28, gating OQ-020). The same bullet list
  records OQ-033 / OQ-034 / OQ-035 (plugin-host-runtime RFC plus
  ADR 0010, accepted 2026-09-11), OQ-039 (RFC-0001, closed 2026-09-12),
  OQ-040 (RFC-0002, closed 2026-09-12), OQ-041 / OQ-045 (RFC-0001
  per-View and outline-width contracts, docs CTX-0163, closed
  2026-09-12), OQ-042 (RFC-0001 background-image contract, docs
  CTX-0159, closed 2026-09-12), and OQ-053 (bundled-plugin split
  decision, closed 2026-09-14). A mechanical mention sweep finds an
  `Accepted`-context register entry for every one of the 41.
- Closing-artifact half is complete: all 21 cited RFC / decision
  documents carry `status: accepted` frontmatter (performance-budget,
  compatibility-milestone, terminal-state, configuration-model,
  plugin-platform, package-lifecycle, lua-runtime, rich-presentation,
  isolation-resource, cli-contract, package-followup, devtools,
  default-distribution, ipc-agent, governance, website-delivery,
  risk-evidence, plugin-host-runtime, bundled-plugin-split-decision,
  RFC-0001, RFC-0002); ADR 0010 sampled `accepted`.
- Topic-document half is updated for every September closure: the
  configuration topic doc records the shipped OQ-039 outline keys and
  the accepted-but-unshipped OQ-041 / OQ-045 override and OQ-042
  background-image contracts with their 2026-09-12 resolution dates;
  the plugin roadmap and the default-distribution amendment record the
  OQ-053 split; the plugin-host-runtime RFC records the OQ-033 /
  OQ-034 / OQ-035 resolutions. August closures cite their artifacts
  in-register and those artifacts exist as accepted above; topic
  content was absorbed at closure time (literal OQ-ID strings are not
  required in topic prose).
- In-repo references are consistent: 22 `OQ-xxx` state claims swept
  across `specifications/*.md` (OQ-018 recorded closed, OQ-029
  accepted, OQ-008 / OQ-011..OQ-014 accepted, OQ-054 / OQ-055 / OQ-057 /
  OQ-066 / OQ-068 / OQ-083..OQ-087 open) with zero real mismatches
  against the register. The single raw hit is a historical narrative
  sentence in `status-drift-gate.md` describing a past contradiction as
  the gate's motivation; that file's own OQ table records OQ-018 as
  closed there.
- No remaining work in this repository. Future OQ closures stay
  governed by the register's standing rule (close only with linked
  evidence; update topic document and decision register in the same
  change), which needs no further action from this issue.

## SEC-19 evidence matrix Phase E (Issue #1088): EVIDENCE_ONLY, blocked

Phase E completion concretely requires every R-001..R-022 row moved
out of `Open` per the matrix's own review gates (unit / integration
green, adversarial corpus zero crashes / hangs, negative / limit
coverage exhaustive, budget / attribution observable, `just check`
plus ci-gate green, secret / scope assertions where cited, manual-audit
report where cited, safe-mode re-verified where intersecting), with the
auditor-recorded `Open -> Mitigated` transition. That bar is not met:

- Row states parsed from the matrix: R-005 / R-006 / R-007
  `Mitigated`; the other 19 rows `Open`. All 22 rows present all 8
  evidence columns, so the gap is completion (recorded moves), not
  missing columns.
- R-001 / R-002 carry merged auditor artifacts that authorize
  `Open -> Mitigated` (`docs/security/audits/vt-parser-2026-09.md` at
  `8c41f1e` PR #130; `docs/security/audits/rich-image-2026-09.md` at
  `8e6c8a9` PR #132; both files exist in the owning checkout) but the
  matrix rows do not record the move, so both stay `Open`.
- R-001's P0-AC-002 long-running `cargo-fuzz` campaign is still
  outstanding: `5daf686` (PR #1160, Issue #1132) landed the
  `fuzz/fuzz_targets/` targets (`vt_parser.rs`, `osc_string.rs`,
  `dcs_apc_string.rs`, all present and merged) but its recorded runs
  are bounded smokes, not the long-running campaign.
- R-004 remains `Open` at `7a4ee41` per the 2026-08-31 clipboard audit
  (`docs/security/audits/clipboard-2026-09.md`, exists), which
  explicitly does not authorize `Open -> Mitigated` (residual
  platform-backend, real-window UX, and `8192`-byte bound-scope gaps).
- The 2026-09-07 FIND-0002 remediation wave rows are `Implemented`-only
  by construction and move nothing pending recorded auditor review.
- Do not close: Phase E completes only via follow-up auditor reviews
  that record the `Open -> Mitigated` moves (nearest: R-001 / R-002
  matrix transitions plus the R-001 fuzz campaign), owned by the
  security track, not this task.

## Gates

- `markdownlint-cli2`: zero findings in the new file. (The repo-wide
  run reports 32 pre-existing MD060 table-style findings in
  `crates/bitty-perf/baselines/*.md`, untouched by this task.)
- `./scripts/check-status-drift.sh`: pass, no new drift introduced.
- `./scripts/check-scratch-paths.sh`: pass, no hardcoded host paths.
- Full `just check` (clippy / workspace tests / supply chain) not
  re-run: markdown-only addition, no Rust or workflow inputs changed.
  Target dir for any ad-hoc runs would be the task-scoped
  `CARGO_TARGET_DIR` outside the repo, removed at task close.
