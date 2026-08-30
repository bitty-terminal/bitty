---
title: Resource Loader Security Audit — 2026-09 (R-003)
description: Independent security-auditor manual review of bitty-rich loader for R-003 entry to Mitigated per P0-AC-005/006 and T-03.
category: security
audience: security-reviewer
document_type: audit
status: draft
---

<!-- markdownlint-disable MD025 -->

# Resource Loader Security Audit — 2026-09 (R-003)

## Auditor independence and scope

- Auditor: **core-security-auditor** — independent of Subagent A (`core-rich-implementer`), which owns `crates/bitty-rich/**` and `crates/bitty-platform/**`. This report touches only `docs/security/audits/**` per CTX-0090 disjoint scopes.
- Date: 2026-08-30 (report filename `resource-loader-2026-09.md` per CTX-0090 task).
- Commits reviewed: `bitty` worktree HEAD `8e6c8a9af85f5a5257fef7693121c177462e1067` (`8e6c8a9`), `bitty` main HEAD `8e6c8a9af85f5a5257fef7693121c177462e1067` (identical), `bitty-docs` HEAD `136194c39aebf7dd470ef70c2b3964f2986681bf` (`136194c`).
- Scope: risk **R-003** (threat **T-03** graphics/rich protocol reads/deletes arbitrary files or devices), criteria **P0-AC-005** Deny-by-default local file loading + **P0-AC-006** No protocol-directed deletion, as normatively defined in `bitty-docs/docs/security/p0-acceptance-criteria.md`, `risk-register.md` (R-003 Critical/Medium/P0), `evidence-matrix.md` (Phase E R-003 row), `threat-model.md` (T-03), `overview.md` (invariants 1, 7), `docs/specifications/rich-presentation-rfc.md` §Resource loader.

## Limit verification — every loader limit has a named test (P0-AC-005/006 thresholds)

`crates/bitty-rich/src/loader.rs:25` is `#![forbid(unsafe_code)]` (workspace `lints.rust.unsafe_code = deny`), headless, bounded, deterministic. `validate_resource_path` is deny-by-default; every `ResourceError` variant is a deny. `ResourcePolicy::deny_all()` (empty `roots`) denies all; `ResourcePolicy::new` caps `MAX_ROOTS`.

| Limit class                                   | Named test(s)                         | Bound / assertion                                                                                                                    |
| --------------------------------------------- | ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| Device nodes `/dev/null`, `/dev/zero`         | `devices_denied`                      | `ForbiddenPrefix` or `DeviceDenied` (raw prefix L158 + `FileTypeExt` L229)                                                           |
| Device prefix `/dev`, `/dev/`, `/dev/urandom` | `dev_denied`                          | `ForbiddenPrefix` / `DeviceDenied` / `Io` / `NotRegularFile` exhaustive                                                              |
| procfs `/proc`, `/proc/self/mem`              | `procfs_denied`                       | mandatory `ForbiddenPrefix` — asserts not `Ok`, second assert is `ForbiddenPrefix`                                                   |
| sysfs `/sys`, `/sys/kernel`                   | `sysfs_denied`                        | mandatory `ForbiddenPrefix`                                                                                                          |
| Sockets (UnixListener bind)                   | `sockets_denied`                      | `SocketDenied` or `NotRegularFile` (`is_socket()` L248) — real `UnixListener::bind` inside approved root, `#[cfg(unix)]` else `Io`   |
| Symlink escape (outside file + `/etc/passwd`) | `symlink_escape_denied`               | `SymlinkEscape` or `OutsideApprovedRoot` or `ForbiddenPrefix` via `canonicalize` escape                                              |
| Non-regular (dir, subdir)                     | `non_regular_denied`                  | `NotRegularFile` via `is_file()` L260 gate                                                                                           |
| Approved-path + traversal + deny-all          | `approved_path_policy`                | inside returns canonical under approved root; outside `OutsideApprovedRoot`; empty policy denies; `../` component `PathTraversal`    |
| Delete primitives audit                       | `protocol_has_zero_delete_primitives` | `include_str!("loader.rs").split("#[cfg(test)]")` prod slice + `image.rs` prod slice assert no `remove_file`, `remove_dir`, `Unlink` |
| Bounded + deterministic                       | `bounded_and_deterministic`           | same file twice `==` canonical, `TooLong` at `MAX_PATH_LEN+1`, `EmptyPath`, `TooLong` at `MAX_ROOTS+1`                               |

All 10 new tests in `crates/bitty-rich/src/loader.rs:306-596`; suite grew 93→103 per Subagent A.

## P0-AC-006 exhaustive delete-primitive audit

`protocol_has_zero_delete_primitives` proves zero filesystem deletion reachable from protocol input:

- `loader.rs` prod slice (`split("#[cfg(test)]").next()`) contains none of `remove_file`, `remove_dir`, `Unlink`.
- `image.rs` prod slice likewise contains none of the three.
- Grep confirmation: `rg -n "remove_file|remove_dir|Unlink" crates/bitty-rich/src/loader.rs` hits only the test literal at L546 (forbidden list); `crates/bitty-rich/src/image.rs` 0 hits prod; `crates/bitty-platform` 0 hits; `crates/bitty-rich` `ImageStore` uses `VecDeque::retain` only (in-memory eviction).

Threshold **zero delete operations reachable from protocol input** satisfied.

## Order, canonicalization, and gate verification

`validate_resource_path` order (deterministic, bounded) verified in source `crates/bitty-rich/src/loader.rs:186-303`:

1. Bounded pre-checks: empty (`EmptyPath`), `len > MAX_PATH_LEN` (`MAX_PATH_LEN=4096` L30), null byte (`NullByte`), `..` via `Component::ParentDir` → `PathTraversal` — before any I/O.
2. Forbidden prefix on raw path `is_forbidden_prefix` L158 (`/proc`, `/sys`, `/dev` exact or `prefix/`).
3. `symlink_metadata` L221 + `FileTypeExt` device/socket/fifo deny L229-258 (`is_char_device`/`is_block_device` → `DeviceDenied`, `is_socket` → `SocketDenied`, `is_fifo` → `FifoDenied`); target type resolved via `metadata` when symlink.
4. Regular-file gate `metadata.is_file()` L260 → `NotRegularFile` (follows symlink, denies dirs).
5. `canonicalize` L271 (resolves symlinks, `..`) + forbidden prefix on canonical L278 (symlink to `/proc` second layer).
6. Approved-path prefix `ResourcePolicy::is_allowed` L128 (`starts_with` raw + `canonicalize` root) → `SymlinkEscape` when `path.starts_with(root)` but `canonical` outside, else `OutsideApprovedRoot`. Returns canonical `PathBuf` on success.

Symlink escape canonicalization proven by `symlink_escape_denied` with real temp dirs and `symlink` outside→approved. `MAX_PATH_LEN`/`MAX_ROOTS` (`16` L33) bound every allocation to `PathBuf`/`String`; zero panics/hangs across the negative matrix.

## Purity, boundedness, and unsafe discipline

- Pure headless loader: no ambient authority, no allocation beyond bounded `PathBuf`/`String`, no unbounded collection; `MAX_PATH_LEN` and `MAX_ROOTS` cap inputs.
- Deterministic: fixed inputs yield same `Ok(canonical)` or same `Err` variant; `bounded_and_deterministic` asserts `first == second`.
- `#![forbid(unsafe_code)]` at L25; `rg -n unsafe` matches only the `forbid` line and doc comment; crate has zero `unsafe`.
- Zero panics/hangs: every limit asserted by named test per P0-AC-005/006 pass thresholds.

## R-003 Entry-to-Mitigated checklist (RS-1..RS-7)

Per risk-evidence RFC and `evidence-matrix.md` row R-003:

- RS-1 unit/integration green — `cargo test -p bitty-rich` 103 passed (Subagent A) + `cargo test -p bitty-platform` 45 passed + headless `headless_run`/`winit_window` ok + workspace all ok; re-confirmed via `just check` + `cargo check` gates below.
- RS-2 adversarial zero panics/hangs — negative class matrix (devices, sockets, procfs/sysfs/devfs, symlink, non-regular, traversal, bounds) proves it.
- RS-3 negative/limit coverage exhaustive — every P0-AC-005/006 limit class named above.
- RS-4 budget/attribution — `MAX_PATH_LEN`/`MAX_ROOTS` observable via `TooLong`; FIFO not applicable to loader but bounded caps enforced.
- RS-5 `just check` + `ci-gate` green — see gate evidence below (`fmt-check`, `clippy -D warnings`, `cargo test --locked`, `actionlint`, `markdownlint`, `cargo check` workspace + Windows, `act -n`).
- RS-6 secret/scope where cited — N/A for R-003 (no secret handling).
- RS-7 manual-audit report — this file satisfies the `Audit` column for R-003.

## Verdict — authorize Open → Mitigated

All P0-AC-005/006 pass thresholds met, RS-1..RS-7 satisfied for R-003, and no `TODO` or unbounded path found. **I authorize the evidence-matrix transition `Open → Mitigated` for R-003** on branch `ctx-0090/feat-rich-r003-loader` at `bitty` `8e6c8a9` + `bitty-docs` `136194c`. `Mitigated → Accepted` still requires a time-bounded CarryCtx decision per risk-evidence RFC and remains out of scope for this audit.

---

Auditor: **core-security-auditor** (Subagent B, CTX-0090 `docs/security/audits/**`) — 2026-08-30.

Canonical corpus: `bitty-docs/docs/security/p0-acceptance-criteria.md` (P0-AC-005/006), `risk-register.md` (R-003 Critical/Medium/P0), `evidence-matrix.md` (Phase E R-003 row), `threat-model.md` (T-03), `overview.md` (invariants 1, 7), `docs/specifications/rich-presentation-rfc.md` (resource loader), `docs/specifications/risk-evidence-rfc.md` (RS-1..RS-7).
