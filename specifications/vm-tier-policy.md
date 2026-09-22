# VM test-tier policy (first slice, CTX-0507)

> Status: **implemented only for what the controller and tests can prove**;
> everything else in this document is recorded plan and is marked as such.
> Owning code: `crates/bitty-test-vm` (`cargo run -p bitty-test-vm --bin
bitty-vm -- <command>`). Research record 043 and the testing-infrastructure
> capture in `bitty-docs` define the wider three-tier test architecture
> (native, VM, physical); this document fixes the first slice of its VM tier.

## Why

`cargo test` alone cannot exercise OS integration (PTY/ConPTY, display
servers, distributions, install behavior). The VM tier runs those tests in
QEMU guests, and it must be reproducible without reinstalling an operating
system per run. That requirement produces exactly one image/overlay policy
and one gating rule.

## Base images and overlays (policy)

1. A guest operating system is installed **once** into a prepared base image
   at `<vm-root>/images/base/<guest>.qcow2`. The base image is never a boot
   disk and is never mutated by a test run.
2. Every test run boots a **disposable qcow2 overlay** at
   `<vm-root>/runs/<run-id>/<guest>-overlay.qcow2`, whose only backing file
   is that base image (`qemu-img create -f qcow2 -F qcow2 -b <base>
<overlay>`).
3. **No run installs or boots from an ISO.** Installation media exists only
   in the manual base-image creation step below. `RunPlan::validate`
   refuses ISO-shaped disks, and the rendered domain XML contains exactly
   one disk: the overlay.
4. Run identifiers are single-use: overlay creation fails if the overlay
   already exists. Reruns use a new run id, so no run inherits another
   run's disk state.
5. The run directory and its overlay are removed when the run ends; nothing
   in the policy keeps per-run state.
6. Guest access for test execution is SSH (`<guest-ssh-user>@<domain>`),
   with the guest IP learned from the QEMU guest agent
   (`virsh domifaddr --source agent`). libvirt owns lifecycle; the
   controller renders the libvirt domain and `qemu-img` commands.

## VM root layout

```text
$BITTY_VM_ROOT/
  images/base/            prepared base images (manual step, read-only by convention)
    arch.qcow2            one per guest id in the matrix
  runs/                   per-run scratch (safe to delete wholesale)
    <run-id>/             single-use; removed after the run
      <guest>-overlay.qcow2
```

Configuration is environment-first; nothing derives from a developer's
checkout, home directory, or hostname:

| Variable              | Meaning                                                                                                      |
| --------------------- | ------------------------------------------------------------------------------------------------------------ |
| `BITTY_VM_ROOT`       | VM root; required by `plan` and `smoke` (`--root` overrides)                                                 |
| `BITTY_VM_LIVE`       | any value enables live stages, same as `smoke --execute`                                                     |
| `BITTY_VM_FORCE_SKIP` | any value forces every live stage to report `gated`                                                          |
| `ISO_PATH`            | user-supplied installation media for **manual** base-image creation; the controller never reads or mounts it |

## Staged cadence

| Cadence | Guests                                       | CI status                                  |
| ------- | -------------------------------------------- | ------------------------------------------ |
| PR      | Arch Linux x86_64 (KVM)                      | encoded; not wired (no KVM CI runner)      |
| main    | adds Ubuntu LTS, Fedora, Alpine x86_64 (KVM) | encoded; not wired                         |
| nightly | adds Arch Linux ARM64 under QEMU TCG         | encoded; not wired (no ARM base image yet) |

The matrix is encoded in `policy::GUESTS` and enforced by `--cadence`
cross-checks; scheduling belongs to a later slice that owns KVM-capable
runners. Windows 11 is part of research 043's PR row and stays deferred
here: it needs image provisioning, OpenSSH/WinRM wiring, and licensing that
this slice does not own.

## Base-image creation (manual, far future)

Automatic ISO installation is explicitly out of scope; there is no ISO
automation in the repository. When a base image is eventually prepared by
hand, the operator installs the guest from user-supplied media under
`$ISO_PATH` into `<vm-root>/images/base/<guest>.qcow2` and provisions at
least: OpenSSH server, the QEMU guest agent, Git, and the Bitty test
dependencies. The image then stays fixed until a deliberate refresh; runs
only ever touch overlays.

## Controller and gating semantics

`bitty-vm` commands: `guests` (matrix), `doctor` (capabilities and
configuration), `plan` (dry-run; executes nothing, renders the `qemu-img`
and libvirt commands plus domain XML), `smoke` (gated stages: `policy`,
`accel`, `overlay`, `guest-boot`). Stage states are `ok`, `dry-run`,
`gated`, `deferred`, and `failed`; a gated stage is reported with its exact
missing prerequisite and does not fail the command unless `--require` is
passed (exit 3).

What this slice actually executes:

- **Accelerator probe** (implemented): a paused QEMU machine starts with
  `-accel kvm` (or `tcg`), negotiates QMP, and quits under a hard deadline.
  This is a real KVM smoke that needs no guest image. It is skipped where
  `/dev/kvm` or the QEMU binary is absent, and it only ever kills the child
  it spawned.
- **Overlay creation** (implemented): real `qemu-img` overlay over a
  prepared base image, with single-use enforcement.
- **Guest boot and SSH test execution** (deferred): the plan renders the
  intended domain XML and commands; starting domains, waiting for SSH,
  uploading artifacts, and collecting logs are follow-up work.

Live integration tests (`tests/live_kvm.rs`, `tests/live_overlay.rs`) are
skipped unless `BITTY_VM_LIVE` is set, mirroring the compat-lab live-test
pattern.

## Open items

- KVM-capable CI runners, image storage, and refresh cadence are undecided;
  nothing in this policy is scheduled in CI.
- Guest boot, SSH execution, artifact collection, and libvirt domain
  definition are unimplemented follow-up work.
- Full in-guest suite candidacy is recorded plan, not scheduled work: the
  code-reviewed target is `policy::GUEST_SUITE_CANDIDATES`. Functional
  suites (`compat-matrix`, `parser-corpus`, `pty-integration`) are candidates
  on every guest including emulated (TCG) ones; timing suites
  (`startup-bench`, `latency-bench`, `idle-bench`) are KVM-only candidates
  because emulated timing is host-scheduler noise, never a budget signal.
- The declared PERF-14 dependency names no filed issue in the tracker, so
  this slice carries no backlog dependency; a real PERF-14 must be filed and
  linked before it can order this work.
- virtio-gpu/virgl coverage, benchmark VMs, Windows guests, and physical
  GPU runners remain outside this slice, consistent with research 043.
