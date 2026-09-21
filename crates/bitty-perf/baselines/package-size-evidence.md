# CTX-0622 evidence — PB-5 package size measurement (Issue #1060)

- Issue: #1060 (`PERF-06: PB-5 package size measurement`, P2, S, milestone v0.1.0)
- Task: CTX-0622 (in_progress)
- Branch: ctx-0622/pb5-pkgsize (worktree-local; base origin/main)
- Base revision: b0b1155 (`fix(build): sync Cargo.lock ... (#1225)`)
- Workspace version: 0.0.20
- Date: 2026-09-22 (UTC)

## Budget (authoritative sources)

- `crates/bitty-perf/src/lib.rs`: `PB5_BINARY_MB = 25`, `PB5_DIST_MB = 40`
  (`PB-5 package size — release binary ≤ 25 MB, dist ≤ 40 MB`).
- `bitty-terminal-docs/specifications/performance-budget-rfc.md` § PB-5:
  stripped release binary of `bitty-app` ≤ 25 MB per Tier 1 platform;
  default distribution download (compressed) ≤ 40 MB.
  Lower-confidence inference anchor; revisit after the first real link.
  This measurement IS that first-link data point.
- Dependency note: no artifact named `REL-02` exists in-repo; it is read as
  the release/packaging pipeline (`.github/workflows/release.yml` +
  `nfpm.yaml`), which this measurement replicates.

## Method (replicates the release pipeline)

1. `cargo build --release --locked -p bitty-app` (pinned toolchain 1.98.1,
   host `x86_64-unknown-linux-gnu`, Linux x86_64, task-scoped ephemeral
   target dir; same command as the release `build` job native leg).
2. `strip -o <stripped-copy> <binary>` for the stripped-binary budget check.
   (`strip` copies; the release binary itself is untouched.)
3. Staged a packaging dir mirroring the repo layout (`target/release/bitty`,
   `LICENSE`, `README.md`, `CHANGELOG.md`, `packaging/` assets, unmodified
   `nfpm.yaml`) and ran, as the release `validate`/`Package (nfpm)` steps do:
   `nfpm package --config nfpm.yaml --packager deb|rpm|apk|archlinux`.
4. `stat`/`sha256sum` every artifact; `file`, `ldd`, and `bitty --version`
   smoke on the binaries.

## Results — PASS on both budgets

Sizes in bytes, decimal MB (budget unit), and MiB:

| Artifact | Bytes | MB | MiB | Budget | Verdict |
| -------- | ----: | ---: | ---: | ------ | ------- |
| `bitty` release binary, stripped | 13,444,104 | 13.44 | 12.82 | ≤ 25 MB | PASS (46% headroom) |
| `bitty` release binary, as built (unstripped) | 18,221,336 | 18.22 | 17.38 | ≤ 25 MB (info) | under |
| `bitty-x86_64-unknown-linux-gnu.deb` | 6,592,986 | 6.59 | 6.29 | ≤ 40 MB | PASS |
| `bitty-x86_64-unknown-linux-gnu.rpm` | 6,591,349 | 6.59 | 6.29 | ≤ 40 MB | PASS |
| `bitty-x86_64-unknown-linux-gnu.apk` | 6,836,751 | 6.84 | 6.52 | ≤ 40 MB | PASS |
| `bitty-0.0.20-1-x86_64.pkg.tar.zst` (archlinux) | 6,275,396 | 6.28 | 5.99 | ≤ 40 MB | PASS |
| raw dist binary `dist/bitty-x86_64-unknown-linux-gnu` (= as-built binary) | 18,221,336 | 18.22 | 17.38 | ≤ 40 MB | PASS |

SHA-256 (packages built from the as-built binary):

- deb: `0cb9c4a49bcae78e1325211ee353a3bccdc245a2822e2e88cb71e619ad50ea21`
- rpm: `ebd577575ce3ec2ef4bb01f658a8552daa0788975786c34069ae524b3d2bcdbf`
- apk: `5b11c8e38d0ae36f8a4ae881694521607c5ac69a17263e1e54d4d6a68a9890ff`
- pkg.tar.zst: `e24cf8b7a5f7fb24b231399d87d9a7a3dd0aaecaf648474d92819e76c76e20d5`

Provenance checks:

- `file`: ELF 64-bit LSB pie executable, x86-64, dynamically linked,
  interpreter `/lib64/ld-linux-x86-64.so.2`.
- `ldd` NEEDED (`libfontconfig.so.1`, `libfreetype.so.6`, `libgcc_s.so.1`,
  plus base `libm`/`libc`): consistent with the `nfpm.yaml` `overrides`
  runtime deps for deb/rpm/apk/archlinux.
- Smoke: both the as-built and the stripped binary print `0.0.20` for
  `--version` (exit 0); stripping breaks nothing.

## Observations (no fix applied — nothing fails)

- The release pipeline performs NO strip step and there is no
  `[profile.release] strip` setting in the workspace; shipped packages
  therefore embed the unstripped binary. Both variants pass PB-5, so no
  change is proposed here. If a future link approaches the budget, the
  fail-closed lever is `strip = true` (or `strip = "debuginfo"`) in the
  release profile, plus split-debuginfo — recorded as a follow-up option,
  not applied.
- Scope limit: measured Tier 1 Linux x86_64 (gnu) only. Windows/macOS/arm64
  and musl/Alpine legs were not linked on this host; PB-5 is per Tier 1
  platform, so those legs still need their own numbers before #1060 closes.
- The `dist ≤ 40 MB` reading used here is per distributable artifact
  (each nfpm package, plus the raw binary tarball-equivalent). All pass
  with ≥ 80% headroom.

## Verdict

PB-5 PASSES on Linux x86_64 at this revision: stripped binary 13.44 MB
vs 25 MB; largest dist artifact (apk) 6.84 MB vs 40 MB. No code or
packaging change required by this task. Recommend recording these numbers
as the post-first-link anchor for the budget revisit, then closing #1060
only once the remaining Tier 1 platform legs are measured.

## Reproduction

From a clean worktree at the base revision, with the pinned toolchain:

```sh
CARGO_TARGET_DIR="$(mktemp -d)" cargo build --release --locked -p bitty-app
stat -c '%s %n' "$CARGO_TARGET_DIR/release/bitty"
strip -o /tmp/bitty-stripped "$CARGO_TARGET_DIR/release/bitty"
stat -c '%s %n' /tmp/bitty-stripped
nfpm package --config nfpm.yaml --packager deb --target /tmp/bitty.deb
nfpm package --config nfpm.yaml --packager rpm --target /tmp/bitty.rpm
nfpm package --config nfpm.yaml --packager apk --target /tmp/bitty.apk
nfpm package --config nfpm.yaml --packager archlinux --target /tmp/
```

(Release CI additionally requires `target/release/bitty` to exist beside
`nfpm.yaml`; copy the built binary there first, exactly as the
`Package (nfpm)` step does.)
