---
title: RUN-22 Sensitive-input detection signal analysis
description: Sensitive-input interlock candidate evidence for OQ-086 (CTX-0678, bitty#1053)
category: specifications
audience: contributor
document_type: design-record
status: candidate
---

<!-- markdownlint-disable MD025 -->

# RUN-22 Sensitive-input detection signal analysis

> Status: **candidate evidence record** (CTX-0678). This record logs
> fail-closed defaults and a tested policy kernel as evidence for the owner
> ruling. It closes nothing: the observation mechanism, the typed denial
> shape, and the acceptance evidence need the OQ-086 decision first.
>
> Task: `CTX-0678` | Issue: `bitty`#1053 | Backlog: `RUN-22`
> Source: `OQ-086` (open); threat-model candidate

## Claim-status legend

- **Shipped** — implemented and tested in the `bitty` repository.
- **Accepted** — decided in an accepted contract.
- **Candidate** — recorded direction with no contract force.
- **Open** — follow-up work with no contract.

## Analysis

[OQ-086](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/open-questions.md)
asks which signal detects an interactive sensitive-input prompt (kernel PTY
no-echo state versus output heuristics), how interactions are classified,
and what interlock, consent, and snapshot-exclusion rules follow. The
candidate direction is recorded in the IPC and Agent RFC section "Candidate
sensitive-input interlock and interaction policy (OQ-086)" as SI-1–SI-6: the
termios `ECHO` bit observed from the PTY master side is the detection
signal; output text matching is rejected as the primary signal (spoofable,
locale-dependent); three interaction classes sort the policy; the interlock
fails closed with a typed denial while no-echo holds; no-echo bytes are not
captured anywhere; Core owns observation and lockout while the AI stack owns
command audit and redaction. The accepted rule that `terminal.input`
requires a separate per-client consent grant stays authoritative throughout.

This record contributes the policy kernel that any such interlock needs,
with text excluded from the decision by construction:

- Signal: [`EchoState`] carries the observed `ECHO` bit (`of_echo_bit`
  maps the bit; observation itself stays OQ-086 open work). The kernel takes
  no prompt text, so spoofed prompt-looking output cannot change the answer.
- Classes: [`InteractionClass`] sorts into `SecretInput`,
  `PrivilegedConfirmation`, or `SafeInteractive` via `classify`, where the
  echo state wins over the command-risk flag — no-echo is always secret.
- Gate: [`automated_input_allowed`] re-checks the echo state on every call:
  no-echo denies every class with
  `SecureInputDenial::TargetInSecureInputMode`; echo-on privileged
  confirmation denies with `ConfirmationRequiresHuman`; only echo-on safe
  interactive input is allowed, still under the caller's own dispatch
  authority and audit attribution enforced by the host.
- Capture: [`may_capture`] is false for no-echo, extending the minimization
  posture to the live input path across grid, scrollback, snapshots, traces,
  and agent observations.

## Fail-closed defaults (**Candidate**)

- S-1: No-echo denies automated input for every class, including labels
  computed before echo cleared; a stale safe label never leaks through.
- S-2: An inconsistent `SecretInput` label with echo on still refuses rather
  than guessing safe.
- S-3: Denial is a typed error, never a silent drop and never a
  queue-and-replay.
- S-4: No-echo bytes are never capturable; a snapshot read sees no content
  for that span.
- S-5: The grant itself is unchanged by a denial; dispatch suspends only for
  the duration of the no-echo state, and the human keyboard path is
  unaffected.

## Candidate decisions log

- C-1: Kernel PTY no-echo state is the only signal; text heuristics may add
  conservative suspicion at most and may never authorize automation.
- C-2: The command-risk flag feeding `classify` comes from the OQ-087 audit;
  this kernel classifies nothing about commands itself (SI-5 seam).
- C-3: The observation mechanism (poller, kernel notification, or another
  path) and the final typed-denial wire shape stay OQ-086 open work.

## Evidence (**Shipped** as candidate implementation evidence)

- Implementation: `crates/bitty-runtime/src/execution/sensitive_input.rs`
  (`EchoState`, `InteractionClass`, `SecureInputDenial`,
  `automated_input_allowed`, `may_capture`), re-exported from
  `crates/bitty-runtime/src/execution.rs`.
- Unit tests in the same file cover both directions: no-echo programs deny
  every class, echo-on safe prompts allow, echo-on confirmations require a
  human, stale labels stay denied, and no-echo capture is excluded.
- Reproduce: `cargo test -p bitty-runtime --lib sensitive_input::`.

## Gates

- `cargo fmt --check`, `cargo clippy --workspace --all-targets` with
  `-D warnings`, and `cargo test -p bitty-runtime` stay green.
- The kernel performs no syscalls, reads no device state, and grants no new
  data access; it tightens, never relaxes, the accepted per-client consent
  rule.

## Open points (owner ruling needed)

OQ-086 must still accept: the observation mechanism for the `ECHO` bit; the
final typed-denial shape; the SI-6 fixture evidence in both directions
(no-echo programs enter the interlock while echo-on prompts and spoofed
output do not, with the no-capture assertion holding across every snapshot
surface); and independent security review. Until that ruling, C-1–C-3 are
design candidates and authorize no dispatch-gating work.

## Verdict

**EVIDENCE_ONLY** — the issue is marked `[BLOCKED: OQ-086]` ("do not start
until the blocking item is resolved"). This record is evidence for that
ruling, not a substitute for it.
