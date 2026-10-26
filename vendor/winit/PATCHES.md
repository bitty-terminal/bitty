# Vendored `winit` patches

This directory is the crates.io release `winit 0.30.13` plus the single
upstream patch recorded below. The workspace consumes it through
`[patch.crates-io]` in the repository root `Cargo.toml`; the version
requirement in `[workspace.dependencies]` stays on the released `0.30.13`, so
dropping the patch is a deletion rather than a dependency bump.

`vendor/winit` is excluded from the Cargo workspace (`exclude` in the root
`Cargo.toml`) so `cargo fmt --all`, `clippy --workspace`, and workspace lint
inheritance never rewrite or re-lint upstream code. Apart from this file and
`patches/`, the tree is kept exactly as upstream ships it, including
`Cargo.toml`, `Cargo.toml.orig`, `.cargo_vcs_info.json`, `LICENSE`, `build.rs`,
`docs/`, `examples/`, and `tests/`. The cargo registry marker `.cargo-ok` is a
local cache artifact and is not copied.

`just vendored-winit` (part of `just check`) fails when the lock or the
resolved graph no longer comes from this directory, and when the tree drifts
from the digest pinned below. See [Integrity](#integrity).

## Provenance

| Item            | Value                                                                                 |
| --------------- | ------------------------------------------------------------------------------------- |
| Crate source    | `winit 0.30.13` (published 2026-03-02)                                                |
| Source path     | `$CARGO_HOME/registry/src/index.crates.io-*/winit-0.30.13/`                           |
| Source revision | `e9809ef54b18499bb4f2cac945719ecc2a61061b` (recorded in `.cargo_vcs_info.json`)       |
| Upstream PR     | <https://github.com/rust-windowing/winit/pull/4710>                                   |
| PR title        | `[0.30] Wayland: emit Occluded for the xdg-shell suspended state`                     |
| Base commit     | `e9809ef54b18499bb4f2cac945719ecc2a61061b` (branch `v0.30.x`)                         |
| Head commit     | `e2cb6c08499b66bad6b2c5890b0e9dd21856d906` (branch `wayland-suspended-occluded-0.30`) |
| PR commits      | one: `e2cb6c08`, authored 2026-09-23                                                  |
| Patch           | `patches/0001-wayland-xdg-suspended-occluded.diff`                                    |
| Patch SHA-256   | `467156b345e49449422a7ba968ff5a259c085e8f953b75b5daec7a7b4c6c220c`                    |
| Fetched         | 2026-10-26, from `https://github.com/rust-windowing/winit/pull/4710.diff`             |
| Apply result    | clean, zero fuzz (`git apply --check` and `patch --dry-run --fuzz=0`)                 |
| Diff stat       | 5 files changed, 65 insertions(+), 10 deletions(-)                                    |
| Tree digest     | `c0c4343e576b91e6c1e88f7f1f2ba3c1d0cd9519533d0b554492385ee0510a47`                    |

The PR base equals the released `0.30.13` commit, which is why the patch applies
to the crates.io source without offsets. Only the five files below differ from
the registry copy (verified with a recursive no-index diff of the two trees).

## Integrity

The tree digest above covers every vendored file rooted at `vendor/winit`,
excluding this file and `patches/` (repository metadata, not upstream source).
It is the SHA-256 of the sorted `sha256sum` manifest, computed exactly as:

```sh
cd vendor/winit
find . -type f ! -name PATCHES.md ! -path './patches/*' \
  -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum
```

`scripts/check-vendored-winit.sh` (recipe `just vendored-winit`) recomputes the
same value and fails on any mismatch, so a stray edit inside the vendored tree
cannot pass `just check` unnoticed. The trust model is deliberately simple: the
digest proves the tree is the one reviewed with this file, not that upstream is
trustworthy. Upstream trust comes from the recorded base/head commits and the
diff review, not from the digest.

Recompute the digest only after an intentional vendor change, in the same
commit, and update the row above (the gate prints the freshly computed value).

## Files and hunks

| File                                                | Hunks                                            | Change                                                                                                                                                                   |
| --------------------------------------------------- | ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `src/changelog/unreleased.md`                       | `@@ -39,3 +39,8 @@`                              | Adds the `Added` changelog entry.                                                                                                                                        |
| `src/event.rs`                                      | `@@ -415,10 +415,26 @@`                          | Documents Wayland support on `WindowEvent::Occluded` and removes Wayland from the unsupported-backend list.                                                              |
| `src/platform_impl/linux/wayland/event_loop/mod.rs` | `@@ -437,6 +437,16 @@`                           | Dispatches `WindowEvent::Occluded(value)` when a pending compositor update carries one.                                                                                  |
| `src/platform_impl/linux/wayland/state.rs`          | `@@ -283,14 +283,22 @@`, `@@ -418,11 +426,20 @@` | Compares `WindowState::is_suspended()` before and after each configure, records the edge in `WindowCompositorUpdate::occluded`, and initialises the new field to `None`. |
| `src/platform_impl/linux/wayland/window/state.rs`   | `@@ -261,6 +261,13 @@`                           | Adds `WindowState::is_suspended()`, reading `XdgWindowState::SUSPENDED` from `last_configure`.                                                                           |

## Why

xdg-shell `xdg_wm_base` version 6 adds a `suspended` toplevel state. A
compositor sets it when a toplevel is not visible (hidden workspace, outputs
off) and clears it on resume. `winit 0.30.13` already stores the state in
`last_configure` — it is excluded from the "requires resize" state-difference
check — but never surfaces it as an event, so a Wayland application cannot know
that its surface stopped being presented.

Bitty's surface-visibility gate (`crates/bitty-platform/src/app.rs`,
`crates/bitty-terminal/src/terminal_app.rs`) consumes
`WindowEventKind::Occluded`: `Occluded(true)` closes the gate before a present,
`Occluded(false)` re-opens it. Without a native Wayland delivery the gate has no
signal on the platform issue #2570 reproduces on (Hyprland workspace
switch-away): the surface keeps receiving frame callbacks or stops presenting
silently, and the synthetic redraw-starvation probe stays quiet whenever the
compositor still serves callbacks to a suspended surface. This patch gives the
gate the protocol-level edge it needs.

Behaviour added by the patch, and nothing else:

- `Occluded(true)` is emitted on the configure where `suspended` becomes set,
  including the first configure of a window that starts suspended
  (`was_suspended == false` because `last_configure` is still `None`).
- `Occluded(false)` is emitted on the configure where `suspended` clears.
- No event is emitted when the state does not change; the edge bookkeeping is
  per configure, so repeated configures in the same state are silent.
- Only the Wayland backend is touched. X11, iOS, macOS, Web, Windows, and
  Orbital paths are unchanged, and no public API, feature, or dependency
  changes.
- `WindowState::configure` still stores `last_configure` before returning, so
  the post-configure `is_suspended()` read observes the new state.

Mechanism check on the vendored graph: winit 0.30.13 binds `xdg_wm_base`
through `smithay-client-toolkit 0.19.2`, whose `XdgShell::API_VERSION_MAX` is
`6` and whose bind call is `globals.bind(qh, 1..=6, GlobalData)`. sctk maps
`xdg_toplevel::State::Suspended` into `XdgWindowState::SUSPENDED`, which is the
bit `is_suspended()` reads. A compositor that advertises v5 or lower never
sends the state, which is exactly the caveat below.

No test is added inside the vendored tree: exercising the edge needs a Wayland
`WindowConfigure` and a live `WlSurface`, and any harness added there would
diverge the vendored source from the upstream PR, which is what the
provenance record is meant to guarantee. The behaviour is covered from Bitty's
side by the existing `Occluded` mapping test
(`crates/bitty-platform/src/event.rs`, `translate_window_event`) and by the
`input-vm-test-support` surface-gate test, which drives the same
`WindowEventKind::Occluded` edges the patch produces.

## Known upstream caveat

Upstream review is not settled, and the patch is carried with that risk stated:

- On the mainline PR #4709, `kchibisov` requested changes on 2026-09-23:
  "if you look at the reason why the original patch got reverted you'll see
  that your patch may not work on some compositors."
- In the 2026-09-25 inline review on `winit-wayland/src/window/state.rs` he
  objected that winit "should never commit on behalf of the user when there's a
  possibility of user rendering", and proposed instead to request a redraw on
  `Suspended` and deliver `Occluded` only after the user's commit.
- #4710 is the `v0.30.x` backport of that revised approach: it does not commit
  on the user's behalf, it asks for a redraw after every configure and relies on
  `pre_present_notify`/frame-callback gating; the added `event.rs` documentation
  spells out that a suspended window receives no frame callbacks and that a
  frame presented with vsync may block until it is shown again.
- As of the fetch date #4710 is still open (no review, no comments), so the
  "may not work on some compositors" caveat stands as an upstream statement
  about compositor support for the v6 `suspended` state. Compositors that never
  set `suspended` simply produce no edge, which is the pre-patch behaviour; the
  gate then relies on the synthetic probe as before.

## Supply chain

`cargo deny check licenses sources` (cargo-deny 0.20.2) passes with the path
source in the graph:

- The `licenses` check reads the vendored `Cargo.toml` (Apache-2.0, already in
  the ADR-0004 allow list), so no license entry is required. Control test at
  vendoring time: temporarily setting the vendored manifest to
  `GPL-3.0-only` makes `cargo deny check licenses` fail and name
  `winit v0.30.13`, which proves the path crate is actually gated rather than
  skipped as a private package. The manifest was restored byte-for-byte
  afterwards.
- Path dependencies have no registry or git source, so `[sources]` needs no
  exception; the existing `allow-registry`/`allow-git` lists are unchanged.
- No `deny.toml` edit is part of this patch. A future move to a git source
  instead of a path source would need an `allow-git` entry.
- Markdown gates: upstream winit ships Markdown (crate README, `FEATURES.md`,
  `docs/`, `src/changelog/`) that does not follow this repository's
  markdownlint configuration. Those exact paths are listed in
  `.markdownlint-cli2.jsonc` `ignores`; `vendor/winit/PATCHES.md` stays linted.
  The vendored source is never reformatted.

### Advisory coverage gap (RustSec)

`cargo audit` and `cargo deny check advisories` match advisory ranges against
packages that carry a registry source. A path package has no `source` and no
`checksum`, so **the vendored winit tree is outside RustSec matching in both
scanners**: a hypothetical advisory against `winit 0.30.13` would go unreported
for this tree, while the same version resolved from crates.io would report it.
This is not a hypothetical: the control test with a synthetic `time 0.1.44`
entry shows the split — registry source matches `RUSTSEC-2020-0071`, the same
entry without a source does not.

Compensating controls, until the patch is dropped:

- **Dated re-audit bound: 2027-03-31**, the same bound `deny.toml`
  `[advisories]` records for its ignored advisories (P0-AC-034 exception
  policy). Re-check the winit advisory feed and winit release notes by then, or
  earlier when a winit release or a RUSTSEC entry for the 0.30 line lands.
  Extending the bound needs a dated ADR-0004 revision.
- **Manual synthetic-lock audit** (what the registry graph would see), from the
  repository root:

  ```sh
  tmp="$(mktemp)"
  python3 - "$tmp" <<'PY'
  import pathlib, sys
  lock = pathlib.Path("Cargo.lock").read_text(encoding="utf-8")
  old = 'name = "winit"\nversion = "0.30.13"\n'
  new = (
      old
      + 'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
      + 'checksum = "a6755fa58a9f8350bd1e472d4c3fcc25f824ec358933bba33306d0b63df5978d"\n'
  )
  assert lock.count(old) == 1, "winit block not found exactly once"
  pathlib.Path(sys.argv[1]).write_text(lock.replace(old, new), encoding="utf-8")
  PY
  cargo audit --no-fetch --file "$tmp" \
    --ignore RUSTSEC-2024-0436 --ignore RUSTSEC-2026-0192 --ignore RUSTSEC-2026-0009
  rm -f "$tmp"
  ```

  The checksum is the published `winit 0.30.13` checksum (the value the lock
  carried before the patch). The same snippet works for any future vendored
  release: take the fresh checksum from `cargo search`/the crates.io API and
  bump the `version` line.

## Update procedure

The `[patch.crates-io]` entry points at this directory, so while it is active
cargo resolves `winit` here even when a newer compatible 0.30.x release exists;
`cargo update -p winit` is a no-op for the version. The patch only stops
applying when the requirement itself moves past it (for example a bump to
`0.30.14` or `0.31`), and then cargo prints a single
`warning: patch for winit was not used in the crate graph` and silently
resolves the registry release. `just vendored-winit` fails on exactly that
state.

To take a newer release and re-apply the patch:

1. Remove `winit = { path = "vendor/winit" }` from `[patch.crates-io]` and the
   `vendor/winit` entry from `exclude` in the root `Cargo.toml`.
2. `cargo update -p winit --precise <new-version>` and confirm
   `cargo tree -p winit` shows the registry source.
3. Re-extract the new release from
   `$CARGO_HOME/registry/src/<registry>/winit-<new-version>/` over
   `vendor/winit`, keeping `PATCHES.md` and `patches/`.
4. `git apply --check --directory=vendor/winit --verbose \
vendor/winit/patches/0001-wayland-xdg-suspended-occluded.diff` and then apply
   it. If it does not apply, rebase the patch on the new release, update the
   base/head and SHA-256 rows above, and re-run the diff-stat check.
5. Restore the `[patch.crates-io]` and `exclude` entries.
6. Recompute the tree digest with the command in [Integrity](#integrity), update
   the `Tree digest` row, and run `just vendored-winit` to confirm.
7. `cargo check --workspace --all-targets`, `cargo test -p bitty-platform`,
   the occlusion-gate test, `cargo fmt --all -- --check`, and
   `cargo deny check` before committing.

If the new release already carries #4710, skip steps 3-6: delete the patch, the
`vendor/winit` tree, and the `exclude` entry, and record the removal in the
commit message.

## Exit plan

Drop this patch when either lands in a release Bitty can adopt:

- `winit 0.30.x` (`>= 0.30.14`) with #4710 merged, or
- a stable `winit 0.31`/later release that contains the #4709 fix.

As of 2026-10-26 the newest published 0.30 release is `0.30.13`
(2026-03-02), the newest published 0.31 release is `0.31.0-beta.4`
(2026-10-24), and both #4709 and #4710 are open. On removal: bump
`[workspace.dependencies] winit`, delete `[patch.crates-io] winit`, delete the
`exclude` entry and this directory, regenerate `Cargo.lock`, then re-run the
gates. The application-side `Occluded` handling needs no change either way,
since it consumes the platform event that upstream would then emit natively.
