---
title: RUN-23 Command-risk classification analysis
description: Command-risk tier and hard-deny candidate evidence for OQ-087 (CTX-0678, bitty#1054)
category: specifications
audience: contributor
document_type: design-record
status: candidate
---

<!-- markdownlint-disable MD025 -->

# RUN-23 Command-risk classification analysis

> Status: **candidate evidence record** (CTX-0678). This record logs
> fail-closed defaults and a tested structural kernel as evidence for the
> owner ruling. It closes nothing: the tiers, the deny policy, and the
> consent wiring need the OQ-087 acceptance decision first.
>
> Task: `CTX-0678` | Issue: `bitty`#1054 | Backlog: `RUN-23`
> Source: `OQ-087` (open)

## Claim-status legend

- **Shipped** — implemented and tested in the `bitty` repository.
- **Accepted** — decided in an accepted contract.
- **Candidate** — recorded direction with no contract force.
- **Open** — follow-up work with no contract.

## Analysis

[OQ-087](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/open-questions.md)
asks for the command-risk classification and syntax-level audit contract for
agent-initiated commands (tiers, hard-deny classes, consent-ledger
integration, evidence) and its composition with Tool Bus validation. The
candidate direction is recorded in the AI Architecture section "Command risk
classification and syntax-level audit (candidate)" as CRA-1–CRA-5: four risk
tiers sorted before dispatch; structural parsing (shell-AST class, not
string blacklists) so quoting, pipelines, substitution, and simple encodings
cannot hide the operation; hard-deny classes that block and route to the
consent surface; release only through an explicit human decision in the
consent ledger (PP-3), per-command and time-bounded; and an adversarial
corpus plus benign look-alikes as acceptance evidence. The terminal-side
interlock is the OQ-086 companion; the accepted TB-1–TB-7 Tool Bus rules
(registry validation, per-tool capability and consent, budgets, host-only
execution) stay authoritative wherever the two differ.

This record contributes the structural argv kernel that any such audit
needs, stopping exactly where the open parser work begins:

- Tiers: [`RiskTier`] is `ReadOnly`, `Standard`, `StateResetting`, or
  `Restricted`, matching the CRA-1 shapes (inspection, ordinary commands,
  state-resetting commands with notify/audit, consent-gated commands).
- Deny classes: [`HardDeny`] covers pipe-to-interpreter, privilege
  escalation, credential-directory writes, system-configuration writes,
  broad-root recursive force deletion, and raw block-device writes.
- Answers: [`RiskVerdict`] is `Allow(tier)`, `NeedsConsent(tier)`, or
  `Deny(deny)`. Nothing here executes or consents; release lives in the
  PP-3 ledger outside this module.
- Structural matching: [`classify_argv`] matches program basenames (so an
  absolute tool path cannot dodge the check), flag bundles, and path
  prefixes over a caller-supplied post-parse `argv`. There is deliberately
  no command-line splitter: raw strings must be parsed first, and pipelines,
  substitution, and encodings need the shell-AST-class parser that stays
  OQ-087 open work (CRA-2).
- Composition: [`OperationIntent`] (`Read` / `Write` / `Execute`) is
  declared by the Tool Bus tool schema, never inferred from bytes — so
  `cat ~/.ssh/id_rsa` reads while `tee ~/.ssh/authorized_keys` denies, and
  intent can never be smuggled past the check by accident of form.

## Fail-closed defaults (**Candidate**)

- R-1: Empty `argv` cannot be classified and needs consent, never allow.
- R-2: Positive hard-deny matches deny and route to the consent surface;
  no deny hit executes on this answer alone.
- R-3: Destructive shapes (`rm -rf` outside broad roots) need consent even
  without a deny hit.
- R-4: Approval is per-command and time-bounded, never a blanket
  escalation, and cannot widen the caller's scopes.
- R-5: Unknown commands sort to `Standard`, matching the candidate rule
  that ordinary local commands proceed under the accepted scope — while
  every destructive shape above still gates or denies.

## Candidate decisions log

- C-1: Matching is structural over `argv` elements only; substring search
  over raw command lines is rejected as a classification basis.
- C-2: The deny-class table is policy data with a conservative default, not
  a replacement for least privilege (CRA-4).
- C-3: The shell-AST-class parser, the adversarial corpus evidence, and the
  consent-ledger wiring stay OQ-087 open work (CRA-2, CRA-5).

## Evidence (**Shipped** as candidate implementation evidence)

- Implementation: `crates/bitty-runtime/src/execution/command_risk.rs`
  (`RiskTier`, `HardDeny`, `OperationIntent`, `RiskVerdict`,
  `classify_argv`), re-exported from
  `crates/bitty-runtime/src/execution.rs`.
- Unit tests in the same file: escalation wrappers denied (including by
  absolute path), piped interpreters denied, broad-root `rm -rf` denied,
  narrow `rm -rf` gated to consent, `dd of=/dev/...` denied, credential and
  system-config writes denied while reads allow, `git` subcommands sorted
  into tiers, empty `argv` gated, unknown commands standard, verdict tier
  and explicit-decision projections.
- Reproduce: `cargo test -p bitty-runtime --lib command_risk::`.

## Live wiring (CTX-0720)

- Agent-command boundary: `JobRegistry::spawn_as` classifies every spec
  with `classify_argv` under `OperationIntent::Execute` before tracking,
  threading, or execution; `spawn_checked_as` takes the Tool Bus-declared
  intent instead (declared by the tool schema, never inferred from bytes).
  Spawn closes stdin (pipe jobs) or opens a fresh PTY master (PTY jobs),
  so `stdin_piped` is always false at this boundary. A hard-deny match
  fails with `JobError::CommandRiskDenied` (stable `HardDeny` audit name);
  a consent-gated shape fails with `JobError::CommandRiskNeedsConsent`
  (fail-closed: the PP-3 consent ledger does not exist yet, so nothing can
  release it). Spec bounds (`InvalidSpec`) still run before classification.
  The legacy `JobRegistry::spawn` stays ungated host authority.
- Reproduce: `cargo test -p bitty-runtime --test run_wiring risk_`.

## Gates

- `cargo fmt --check`, `cargo clippy --workspace --all-targets` with
  `-D warnings`, and `cargo test -p bitty-runtime` stay green.
- The kernel allocates only for the small `~/`-marker join in the
  credential-path check; all bounds are `const`.

## Open points (owner ruling needed)

OQ-087 must still accept: the tier boundaries and the final deny policy;
the shell-AST-class parser resolving quoting, pipelines, substitution, and
encodings before classification; the PP-3 consent-ledger wiring and
`bitty ctl inspect consent` surface; Tool Bus composition details; and the
CRA-5 adversarial corpus with benign look-alikes plus attributed
denial/consent outcomes. Until that ruling, C-1–C-3 are design candidates
and authorize no dispatch-gating work.

## Verdict

**EVIDENCE_ONLY** — the issue is marked `[BLOCKED: OQ-087]` ("do not start
until the blocking item is resolved"). This record is evidence for that
ruling, not a substitute for it.
