# SEC closure batch evidence (CTX-0691; Issues #1089, #1091, #1092, #1094, #1095, #1096)

Source: epic #975; OQ-029 (accepted 2026-08-28) and OQ-054 / OQ-055 /
OQ-057 / OQ-068 / OQ-083 (open) in the `bitty-docs` open-questions
register. Branch: `ctx-0691/sec-small`. Risk states stay `Open`: the
CLOSE_OK row below is `Implemented`-only evidence pending independent
auditor review per RS-1..RS-7. No product code changed in this task;
verification plus this evidence record only.

## Method

- Read the open-questions register (`bitty-docs`
  `docs/decisions/open-questions.md`) for OQ-029, OQ-054, OQ-055, OQ-057,
  OQ-068, OQ-083 status lines.
- `rg` over `crates/*/src` for the positive anchors (V-C signature
  verification, lease kernel, secret store, capability intersection,
  chat-message `Role`) and the negative anchors (`api_key_env` /
  `api_key_cmd`, secret-storage tiers, `.wheel/` project directory,
  capability-enforced role contract).
- Ran the existing suites that pin each anchor (see Gates). Target dir
  for all runs: task-scoped `CARGO_TARGET_DIR` outside the repo,
  removed at task close. MSRV 1.85 respected (docs only; plus
  `cargo +1.85 check` on the affected crates stays green, so no new
  syntax risk).

## Verdicts

| Issue | Backlog                                         | OQ              | Verdict                      |
| ----- | ----------------------------------------------- | --------------- | ---------------------------- |
| #1089 | SEC-20 package signature scheme V-C             | OQ-029 accepted | CLOSE_OK                     |
| #1095 | SEC-26 panel write-lease contract               | OQ-083 open     | EVIDENCE_ONLY, still blocked |
| #1092 | SEC-23 `api_key_env` vs `api_key_cmd` semantics | OQ-054 open     | EVIDENCE_ONLY, still blocked |
| #1091 | SEC-22 secret-storage tiers                     | OQ-055 open     | EVIDENCE_ONLY, still blocked |
| #1094 | SEC-25 role contract for multi-agent work       | OQ-057 open     | EVIDENCE_ONLY, still blocked |
| #1096 | SEC-27 wheel project-definition directory       | OQ-068 open     | EVIDENCE_ONLY, still blocked |

## SEC-20 V-C signature scheme (Issue #1089): CLOSE_OK

- OQ-029 is `Accepted` (package follow-up RFC, closed 2026-08-28); the
  prior blocker (issue #767, forgeable SHA-256 stub) is closed with a
  real scheme shipped.
- `verify_signature` (`crates/bitty-package/src/trust.rs`) verifies
  Ed25519 via `ed25519-dalek` (pure Rust, no I/O): the record's digests
  must equal the expected lock digests (no replay across releases), the
  `key_id` must resolve to an enrolled, non-revoked key, and the
  signature must verify over `signing_message`. Unknown keys, revoked
  keys, digest mismatch, malformed keys/signatures, and cryptographic
  mismatch all fail closed.
- Key directory: `KeyStore::insert` (enrollment) and `KeyStore::revoke`
  (revocation, rotation by enroll-successor plus revoke-predecessor);
  `verify_signature` always resolves against current store state, so
  revoked or removed keys fail.
- Integrity chain underneath: 7-stage `verify_pipeline`
  (`crates/bitty-package/src/integrity.rs`) plus the 6-state lifecycle
  (`crates/bitty-package/src/lifecycle.rs`, `may_execute_code` true
  only for `Activated` after authorization).
- Tests: `cargo test -p bitty-package` gives 112 lib + 6 compat + 59
  hostile + 16 transaction = 193 passed, 0 failed — including the V-C
  round-trip (`signature_valid_round_trip_verifies`), wrong-key,
  revoked-key, unknown-key, tampered-bytes, and forged-public-key-id
  rejections (lib `trust::tests`, 10 passed; `signature` filter, 4 lib
  plus 7 hostile passed).
- Open (not blocking close): independent auditor review of the V-C
  scheme and key-directory contracts still pending; risk rows stay
  `Open` until recorded.

## SEC-26 panel write-lease contract (Issue #1095): EVIDENCE_ONLY, still blocked

- Scope is `See RUN-21; contract owner is OQ-083`, and OQ-083 is still
  `Open` (candidate mapping only; no lease, description, roaming, or
  handoff mechanism accepted).
- What exists is candidate evidence from CTX-0678 (issue #1281, closed):
  the pure transition kernel `PanelLease` with `LeaseState`
  (`Idle` / `Occupied { holder }`), `acquire` / `release` / `handoff`,
  and `LeaseEvent` / `LeaseError`
  (`crates/bitty-runtime/src/execution/lease.rs`), plus the candidate
  analysis record `specifications/run-21-panel-lease-handoff.md`, which
  states explicitly that nothing there authorizes write-lease
  enforcement and that SEC-26 waits on the same OQ-083 ruling.
- Tests: `cargo test -p bitty-runtime --lib lease` gives 11 passed,
  0 failed (round trip, double-acquire refusal, non-holder
  release/handoff refusals, idle refusals, title/description bounds).
- Do not close: the write-lease enforcement contract resolves only via
  a follow-up task after the OQ-083 acceptance decision.

## SEC-23 credential-reference semantics (Issue #1092): EVIDENCE_ONLY, still blocked

- OQ-054 is still `Open` (`[BLOCKED: OQ-054]`, "do not start until the
  blocking item is resolved"): candidate direction MPC-1..MPC-4 only;
  no provider schema, credential resolution, or project-override
  mechanism exists.
- `rg` for `api_key_env` / `api_key_cmd` (plus `ApiKey` and
  provider-credential shapes) over `crates/` finds nothing: no
  resolution order and no project-level override boundary exist in
  code, so none is claimed here.
- Adjacent shipped mechanism (not the OQ-054 contract): `SecretStore`
  with host-mediated child-env injection (`resolve_env_for_spawn`),
  explicit-env bound `MAX_RESOLVED_ENV_VARS` (64), sensitive-name
  detection (`is_sensitive_env_name`), and agent-visible sanitized views
  (`crates/bitty-plugin-host/src/secrets.rs`); covered by the
  `bitty-plugin-host` lib suite (227 passed, 0 failed).
- Do not close: resolves only via a follow-up task after the OQ-054
  owner decision.

## SEC-22 secret-storage tiers (Issue #1091): EVIDENCE_ONLY, still blocked

- OQ-055 is still `Open`: ADR-0006 already fixes `os.getenv` denial,
  `bitty.env.get` allowlisting, and audit/redaction, but the storage
  tiers (host-consumed environment, `secrets.env` mode `0600`, OS
  keyring, `pass`/1Password command references) and per-tier consent,
  audit, and redaction are candidate only and unimplemented.
- `rg` over `crates/` and `specifications/` for tier constructs
  (`secrets.env`, keyring, `1Password`, `RUN-11` wiring) finds no tier
  model: no tier beyond env plus handle-mediated resolution is claimed.
- Do not close: resolves only via a follow-up task after the OQ-055
  acceptance decision (composes with RUN-11).

## SEC-25 role contract (Issue #1094): EVIDENCE_ONLY, still blocked

- OQ-057 is still `Open` (`[BLOCKED: OQ-057]`): candidate Commander /
  Implementer / Tester / Reviewer table only; prompts never grant
  authority; the contract must also cover the execution-sandbox layer,
  which tool capability alone does not close.
- `Role` in `bitty-agent` (`crates/bitty-agent/src/message.rs`) is only
  a chat-message role (System / User / Assistant / Tool); no
  role-to-authority map and no enforcement points exist in code.
- Adjacent shipped mechanism (not the role contract): the
  effective-capability intersection engine
  (`crates/bitty-plugin-host/src/effective.rs`,
  HostCeiling > UserPolicy > ProjectPolicy > ParentDelegation >
  TaskGrant > AgentRequest, deny-by-default, self-grant prohibition),
  whose non-goals state explicitly that role/organization models stay in
  `bitty-ai` and the engine only re-authorizes at the host.
- Tests: `cargo test -p bitty-agent --lib` gives 51 passed, 0 failed.
- Do not close: resolves only via a follow-up task after the OQ-057
  owner decision.

## SEC-27 wheel directory (Issue #1096): EVIDENCE_ONLY, still blocked

- OQ-068 is still `Open` (`[BLOCKED: OQ-068]`): candidate `.wheel/`
  definition (`project.toml`, `agents/`, `workflows/`, `prompts/`,
  `policies/`, `tools/`, `skills/`), declarative-data-only and
  Git-tracked, with candidate discovery order `.wheel/` then `.agents/`
  then contextual convention files; no project directory is implemented
  or honored.
- `rg -w 'wheel'` over `crates/` and `specifications/` finds only
  mouse-wheel input handling (false positives: `?1007` alternate-screen
  wheel, `LineDelta`/`PixelDelta` scroll steps); no `.wheel/` layout,
  schema, trust split, or `.agents/` compatibility resolution exists in
  code.
- Do not close: resolves only via a follow-up task after the OQ-068
  acceptance decision.

## Gates

- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets --locked` (`-D warnings`):
  clean.
- `cargo check --workspace --locked`: clean.
- `cargo +1.85 check -p bitty-package -p bitty-runtime -p bitty-agent
-p bitty-plugin-host --locked`: clean (MSRV 1.85, no let-chains;
  docs-only change).
- `cargo test -p bitty-package --locked`: 112 lib + 6 compat + 59
  hostile + 16 transaction = 193 passed, 0 failed.
- `cargo test -p bitty-runtime --lib lease --locked`: 11 passed,
  0 failed.
- `cargo test -p bitty-agent --lib --locked`: 51 passed, 0 failed.
- `cargo test -p bitty-plugin-host --lib --locked`: 227 passed,
  0 failed.
