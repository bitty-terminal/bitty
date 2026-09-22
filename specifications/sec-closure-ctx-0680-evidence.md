# SEC closure batch evidence (CTX-0680; Issues #1081, #1080, #1090, #1093, #1088)

Source: risk-register.md R-014/R-015/R-022; P0-AC-025..029; OQ-084/OQ-085;
evidence-matrix.md Phase E. Branch: `ctx-0680/sec-closure`.
Risk states stay `Open`: these records are `Implemented`-only evidence
pending independent auditor review per RS-1..RS-7. No product code changed
in this task; verification only.

## Method

- `cargo test -p bitty-package --locked` (supply chain, SEC-12).
- `cargo test -p bitty-ipc --locked` plus
  `cargo test -p bitty-runtime --locked --test devtools_observability`
  (redaction/trace, SEC-11).
- `bash scripts/check-supply-chain.sh` (`cargo deny check` + `cargo audit`).
- `rg` over `crates/*/src` for trust-level (`TrustLevel`, level 0-4
  capability domains) and ontology (`Ontology`, identity model) mechanisms
  (SEC-21, SEC-24).
- Evidence-matrix and open-questions registers read for Phase E (SEC-19).

Target dir for all runs: a task-scoped `CARGO_TARGET_DIR` outside the repo,
removed at task close. MSRV 1.85 respected (no code changes, so no new
syntax risk).

## SEC-12 R-015/R-022 supply-chain closure (Issue #1081): CLOSE_OK

- `cargo test -p bitty-package`: 112 lib + 6 compat + 59 hostile +
  16 transaction = 193 passed, 0 failed.
- `scripts/check-supply-chain.sh`: `cargo deny check` advisories/bans/
  licenses/sources ok; `cargo audit` clean (376 deps scanned).
- P0-AC-027 (no install-time exec): structural deny-by-schema — the manifest
  schema carries no hook/script fields — plus the lifecycle gate
  (`crates/bitty-package/src/lifecycle.rs`, `may_execute_code` true only for
  `Activated` after authorization; staging contacts no plugin VM).
- P0-AC-028 (lock/checksum): 7-stage `verify_pipeline`
  (`crates/bitty-package/src/integrity.rs`) with H-A artifact / H-B manifest
  binding / H-C content-root hashes; `Lockfile::digest`
  (`lockfile.rs`); each tampered dimension fails independently
  (`verify_pipeline_each_stage_independently_fails`).
- P0-AC-029 (transactional activation/rollback): staged `activate`/`rollback`
  reproducing the exact prior lock digest
  (`rollback_determinism_restores_exact_lock_digest`); retention bounded and
  never removes current (`environment_retention_bounded_never_removes_current`);
  TOFU fail-closed (`trust.rs`); prune ceiling keeps N=2 (`activation.rs`,
  `RetentionPolicy`).
- Open (not blocking close): independent auditor supply-chain review
  (`package-integrity-rollback-2026-xx`, `install-no-exec-2026-xx`) still
  pending; risk rows stay `Open` until recorded.

## SEC-11 R-014 redaction/trace closure (Issue #1080): CLOSE_OK

- `cargo test -p bitty-ipc`: 429 passed, 0 failed (1 ignored pre-existing).
  `devtools_observability` (bitty-runtime): 3 passed.
- P0-AC-025 (scopes distinct): `debug.inspect`/`trace`/`control` map to
  distinct `Scope::DebugInspect`/`DebugTrace`/`DebugControl`
  (`crates/bitty-ipc/src/scope.rs`); connection alone grants none and
  each operation without its scope is denied (devtools scope-matrix tests).
- P0-AC-026 (minimization/redaction): seeded-secret corpus never appears in
  default outputs (acceptance A1.3, `devtools/tests.rs`); clipboard and raw
  environment bytes redacted by default (`trace.rs`, `record.rs` typed
  sensitive-field lists); spool/export files user-only mode (`0o600`
  asserted); export preview equals actual export byte-for-byte
  (`trace.rs` preview-equals-export assertion).
- Open (not blocking close): auditor artifacts
  `devtools-redaction-2026-xx` + `trace-minimization-2026-xx` still pending;
  R-014 row stays `Open` until recorded. Post-0189 surfaces (#328/#332/#334)
  and Amendment A1 scopes remain under that pending review.

## SEC-21 trust-level model (Issue #1090): EVIDENCE_ONLY, still blocked

- OQ-085 is still `Open` (candidate trust-level model recorded from review,
  no owner decision; current boundaries and P0 gates stay authoritative).
- No level 0-4 trust model and no per-level capability-domain matrix exists
  in code (`rg` for trust-level constructs finds nothing normative).
- No security-corpus revision can land until the owner resolves OQ-085.
  Do not close: resolves only via a follow-up task after the decision.

## SEC-24 core ontology/identity (Issue #1093): EVIDENCE_ONLY, still blocked

- OQ-084 is still `Open` (no ontology document exists; Panel/Execution and
  Restore/Persistence separations are the immediate refinements, undecided).
- No first-class ontology/identity model (Instance/Workspace/Panel/Surface/
  ExecutionContext/Terminal/Session/Resource/Service/Agent ownership,
  lifetime, permission) exists in code beyond incidental identifiers.
- Do not close: resolves only via a follow-up architecture task after the
  owner decision.

## SEC-19 evidence matrix Phase E (Issue #1088): EVIDENCE_ONLY, incomplete

- The matrix itself records `Open` for 19 of 22 risks (only R-005/R-006/R-007
  `Mitigated`); every other row awaits auditor review per RS-1..RS-7.
- Depends on SEC-01..SEC-18, several of which (including this batch's
  SEC-11/SEC-12 auditor reviews and the blocked SEC-21/SEC-24) are not
  recorded yet.
- This batch contributes verified `Implemented`-only rows for R-014/R-015/
  R-022 above; matrix completion resolves via follow-up tasks as auditor
  reviews land.
