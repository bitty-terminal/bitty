# `bitty-test-vm`

> Part of the `bitty` workspace. Canonical platform documentation lives in
> `bitty-terminal-docs` (mounted at `docs/`) and shared governance in
> `bitty-docs`; this file is a crate-local map, not a canonical contract.
> The normative VM-tier policy is `specifications/vm-tier-policy.md`.

## Purpose

`bitty-test-vm` is the first slice of the VM test tier from research 043: it
encodes the **base-image + qcow2 overlay policy** (a run never installs or
boots from an ISO), the staged guest cadence (PR: Arch; main adds Ubuntu,
Fedora, Alpine; nightly adds Linux ARM64 under TCG), and a small gated
controller, `bitty-vm`.

## Boundaries

- Dependency-free (std only), `forbid(unsafe_code)`, and portable: every
  platform compiles it; host probes report "unavailable" off Linux instead
  of guessing.
- No host paths are hardcoded. The VM root comes from `BITTY_VM_ROOT` or
  `--root`; `$ISO_PATH` is documentation-only input for the manual,
  far-future base-image creation step and is never read to run a VM.
- `plan` executes nothing. `smoke` executes only the bounded QEMU
  accelerator probe and qcow2 overlay creation, and only with `--execute` /
  `BITTY_VM_LIVE`; every other stage is reported `gated` or `deferred` with
  a reason. Guest boot and SSH test execution are follow-up work.
- Overlay creation is refused unless the plan passes policy validation, the
  prepared base image exists, and no overlay with the same run id exists.

## Usage

```text
cargo run -p bitty-test-vm --bin bitty-vm -- guests
cargo run -p bitty-test-vm --bin bitty-vm -- doctor
BITTY_VM_ROOT=/srv/bitty-vm cargo run -p bitty-test-vm --bin bitty-vm -- plan --guest arch --xml
BITTY_VM_ROOT=/srv/bitty-vm cargo run -p bitty-test-vm --bin bitty-vm -- smoke --guest arch --execute --require
```

Environment: `BITTY_VM_ROOT` (VM root), `BITTY_VM_LIVE` (live opt-in),
`BITTY_VM_FORCE_SKIP` (force live stages gated), `ISO_PATH` (manual
base-image creation only). Exit codes: 0 ok, 1 failed, 2 usage, 3 gated
under `--require`.

Enabled live integration tests (skipped by default, like the compat-lab
pattern):

```text
BITTY_VM_LIVE=1 cargo test -p bitty-test-vm --test live_kvm --test live_overlay
```

## Layout

- `Cargo.toml` — package metadata; no dependency section.
- `src/policy.rs` — guests, cadence matrix, run plan, base/overlay path
  derivation, `validate_paths` policy core, dry-run domain XML.
- `src/config.rs` — environment accessors with pure, testable derivations.
- `src/capability.rs` — `PATH` lookup and host capability probes.
- `src/overlay.rs` — `qemu-img` overlay creation, backing-file readback,
  shell-safe command rendering.
- `src/kvm.rs` — bounded QEMU accelerator probe (QMP handshake + quit).
- `src/smoke.rs` — gated stage orchestration and stable report rendering.
- `src/cli.rs` — argument parsing and command dispatch.
- `src/bin/bitty-vm.rs` — thin entry point.
- `tests/live_kvm.rs`, `tests/live_overlay.rs` — env-gated live checks.
