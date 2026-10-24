# Packaging

This directory holds multi-distro packaging recipes and helpers for Bitty releases.

## Support evidence

A binary that CI builds for a platform is not support evidence for that
platform. Support evidence is limited to passing `verify-install` legs
(Ubuntu, Fedora, Arch, Alpine), passing `verify-*` jobs listed under
`Release Matrix`, and tag-time publishes that completed successfully
(AUR, Homebrew, Scoop, all gated on secrets). Anything else is best-effort
and is labeled as such below.

## Formats

| Format       | File                                                     | Distro                                                      |
| ------------ | -------------------------------------------------------- | ----------------------------------------------------------- |
| deb          | `nfpm.yaml` + `deb` packager                             | Ubuntu 24.04 (verified); Debian 12 best-effort              |
| rpm          | `nfpm.yaml` + `rpm` packager                             | Fedora 40 (verified); RHEL 9, OpenSUSE best-effort          |
| rpm (OBS)    | `packaging/opensuse/bitty.spec` + `_service`             | openSUSE Tumbleweed and Leap 15.6 best-effort (zypper)      |
| apk          | `nfpm.yaml` + `apk` packager                             | Alpine 3.22 (verified; native `x86_64-unknown-linux-musl`)  |
| archlinux    | `nfpm.yaml` + `archlinux` packager, `packaging/PKGBUILD` | Arch (verified); AUR via release publish, gated             |
| apk (aports) | `packaging/alpine/APKBUILD`                              | In-tree aports recipe, not published to aports; best-effort |
| ebuild       | `packaging/gentoo/` (`bitty-bin`, live `bitty-9999`)     | In-tree overlay, not in the gentoo tree; best-effort        |
| xbps         | `packaging/void/srcpkgs/bitty/template`                  | In-tree template, not in void-packages; best-effort         |
| ypkg         | `packaging/solus/package.yml`                            | In-tree Solus recipe, not in solus-project; best-effort     |
| freebsd      | `packaging/freebsd/x11/bitty/` (ports recipe)            | FreeBSD 15.1 amd64 build-only; best-effort, not supported   |
| winget       | `packaging/windows/winget/`                              | Windows Package Manager (`BittyTerminal.Bitty`)             |

The upstream distro recipes (Alpine aports, Gentoo overlay, Void `xbps-src`,
Solus `ypkg`, FreeBSD Ports) are in-tree drafts: nothing is published to
`alpine/aports`, `gentoo/gentoo`, `void-linux/void-packages`, `solus-project`,
or the FreeBSD ports tree, so none of them is support evidence. Their source
and checksum steps run **at tag time**, because the distfiles are GitHub tag
archives that do not exist before the release tag is pushed.
`bash scripts/tests/check-distro-recipes.test.sh` (also
`just distro-recipes-test`, run by the CI packaging step) keeps the recipes
internally consistent and version-agnostic. Manifests for `winget` are validated
by `bash scripts/tests/check-package-manifests.test.sh` (`just package-manifests-test`).

Debian and Ubuntu users can install via the R2-backed APT repository (see `packaging/debian.md`). Fedora and Enterprise Linux users can install via Fedora Copr (see `packaging/fedora.md`). OpenSUSE consumes RPMs two ways: the release `nfpm` RPM passes `rpm -qip` and a local `zypper install` check, and `packaging/opensuse/bitty.spec` is the OBS source recipe that publishes a zypper repository (see `packaging/opensuse/README.md` and `packaging/opensuse.md`). No release `verify-install` leg covers OpenSUSE, so both stay best-effort, not supported. Alpine is apk-based; the apk is a native musl build (see `packaging/alpine.md`).

## Repairing a published channel

When a release tag is already public but a derived channel failed (for example
the 0.0.27 APT and Copr jobs), dispatch the manual repair workflow instead of
moving the tag:

```sh
gh workflow run repair-release-channels.yml \
  --ref main -f tag=v0.0.27 -f channels=all
```

`channels` is `all`, `apt`, or `copr`. The workflow downloads the published
`.deb`/`.tar.zst` assets plus `SHA256SUMS` from the GitHub Release, verifies
every checksum, republishes only the derived channel state, and reads the APT
indices back from R2. The Copr leg builds a source RPM from the published
`.tar.zst` bundles and submits it with `copr-cli build`. The workflow never
moves a tag, edits a release, or re-uploads an asset. The optional Nix leg
(`channels=all`) adopts a newer, validated `flake.lock` from the default branch
only when it is newer than the tag's lock and passes `nix flake metadata`;
otherwise it skips with a notice, leaving the published tag untouched.

## Install smoke

Every Linux package is install-smoked in a clean container of its own distro
before a release can be published (034 item 5): Ubuntu 24.04 (`dpkg -i`),
Fedora (`dnf install ./bitty.rpm`), Arch (`pacman -U`), and Alpine 3.22
(`apk add --allow-untrusted ./bitty.apk`). Each leg then runs `bitty --version`,
`bitty doctor`, and the headless smoke (`bitty --headless`).

`.github/workflows/release.yml` runs the four legs as the `verify-install`
matrix and the `release` job gates on it, so a package that installs or runs
nowhere cannot be published. Run one leg locally with:

```sh
scripts/install-smoke.sh --distro ubuntu --package dist/bitty-x86_64-unknown-linux-gnu.deb
# or point at a directory of downloaded artifacts:
scripts/install-smoke.sh --distro arch --dir pkg
```

## Reproducible release binaries (`CTX-2402`)

Shipped binaries carry no builder paths and no debug info. Both mechanisms
are applied by every release build: `just release-build` locally and the
`build`, `build-alpine`, and `build-freebsd` jobs in
`.github/workflows/release.yml`.

- **Builder-root remapping.** Rust embeds the absolute source path of every
  panic location in read-only runtime data, so a plain release build records
  the builder's cargo registry/git checkouts and rustup toolchain.
  `scripts/release-rustflags.sh` derives those roots from the build
  environment (never from a committed path) and emits
  `--remap-path-prefix=<root>=/build/...`:

  | Builder root                                  | Recorded as        |
  | --------------------------------------------- | ------------------ |
  | repository checkout                           | `/build/workspace` |
  | `CARGO_TARGET_DIR`, when outside the checkout | `/build/target`    |
  | `CARGO_HOME`                                  | `/build/cargo`     |
  | `RUSTUP_HOME`                                 | `/build/rustup`    |
  | rustc sysroot, when outside `RUSTUP_HOME`     | `/build/rustup`    |

  Panic messages stay readable without naming the builder:

  ```text
  panicked at /build/workspace/crates/bitty-vt/src/lib.rs:42
  ```

  `/build/workspace` maps back to the reader's own checkout, `/build/cargo`
  and `/build/rustup` to their cargo and rustup roots. Mappings are kept
  disjoint on purpose: a nested root is skipped instead of relying on how
  rustc resolves overlapping prefixes.

- **Debug-info stripping.** `[profile.release] strip = "debuginfo"` (root
  `Cargo.toml`) removes DWARF/STABS and the source-file symbols that travel
  with them. The `.symtab` symbol table is deliberately kept because it is
  what makes `RUST_BACKTRACE=1` name frames; stripping symbols as well leaves
  a panic backtrace with zero frames (measured with the pinned 1.98
  toolchain). Stripping cannot remove panic-location paths either way: they
  are allocated runtime data, verified on the 0.0.25 artifact where
  `strip --strip-all` still carried all of them.

Size trade-off, measured on the 0.0.25 `x86_64-unknown-linux-gnu` artifact
with the pinned toolchain:

| Variant                                        | Uncompressed          | `zstd -19` (bundle) |
| ---------------------------------------------- | --------------------- | ------------------- |
| 0.0.25 baseline (unstripped, unremapped)       | 23,461,272 B          | 5,864,294 B         |
| release policy (`strip = "debuginfo"` + remap) | 22,478,448 B (-4.2%)  | 5,844,998 B (-0.3%) |
| `strip = "symbols"` opt-in                     | 17,657,160 B (-24.7%) | 5,357,472 B (-8.6%) |

The opt-in row costs symbolized backtraces, so it is not the default. A
packaging channel that prefers the smaller download can take it explicitly:

```sh
RUSTFLAGS="$(scripts/release-rustflags.sh)" \
  cargo build --release --locked -p bitty-terminal \
  --config 'profile.release.strip="symbols"'
```

For a symbolized binary for triage, rebuild the same commit with the default
profile (`just release-build`, which keeps `.symtab` and adds no DWARF).
Builds are not bit-identical across hosts: codegen, linker, and libc/target
versions still differ, and only builder path metadata is normalized.

`scripts/check-release-paths.sh` is the fail-closed gate. It scans the built
bytes for the derived roots, Linux and macOS home shapes, and Windows
user-profile paths, and fails the release before an artifact is renamed or
packaged. Run it locally with `just release-paths <binary>`; its fixture test
is part of `just check`.

## Nfpm

Single source `nfpm.yaml` at repo root covers deb/rpm/apk/archlinux via `nfpm package --packager <type>`. Validation:

```sh
nfpm package --config nfpm.yaml --packager deb --target /tmp/bitty.deb
nfpm package --config nfpm.yaml --packager rpm --target /tmp/bitty.rpm
nfpm package --config nfpm.yaml --packager apk --target /tmp/bitty.apk
nfpm package --config nfpm.yaml --packager archlinux --target /tmp/bitty.pkg.tar.zst
```

Runtime dependencies are declared per format in the `overrides` block:

| Format    | Runtime dependencies                          |
| --------- | --------------------------------------------- |
| deb       | `libfontconfig1`, `libfreetype6`, `libgcc-s1` |
| rpm       | `fontconfig`, `freetype`, `libgcc`            |
| apk       | `fontconfig`, `freetype`, `libgcc`            |
| archlinux | `fontconfig`, `freetype2`, `gcc-libs`         |

`packaging/linux/runtime-deps.toml` maps every linked ELF soname to those
packages, and `scripts/check-runtime-deps.sh` compares the mapping and the
declarations against `ldd` + `readelf -d` on the final binary, failing the
package on divergence. See `packaging/linux/README.md`.

Scripts under `packaging/scripts/` are bounded no-ops (exit 0) to keep package hooks honest.

## Terminfo distribution (`terminfo/bitty.ti`, CTX-1121 #1764)

Canonical source `terminfo/bitty.ti` (`use=xterm-256color` + audited `Tc`,
`Su`/`Setulc`/`Smulx`, `Sync`, `Hls`, `pairs#32767`, `Ms@` cancel; see
`terminfo/README.md`). Never install the raw source to
`/usr/share/terminfo`; always compile with `tic -x` and install the
compiled database entry. `scripts/check-terminfo.sh` fails on the retired
placeholder path, on raw-source installs, on aspirational caps, and on a
compiled entry containing `Ms`.

Linux (`.deb`/`.rpm`/`.pkg.tar.zst`/AUR) — compile at package build time:

```sh
tic -x -o "$pkgdir/usr/share/terminfo" terminfo/bitty.ti
# verifies as: infocmp -x bitty | grep -E 'Tc|Smulx|Sync|Hls'
```

- AUR source (`packaging/PKGBUILD`) and prebuilt (`packaging/PKGBUILD.bin`)
  run the same `tic -x` line in `package()` and install the compiled
  `b/bitty` entry (requires `ncurses` providing `tic` at build time).
- `nfpm.yaml` (`.deb`/`.rpm`/`.apk`/`.pkg.tar.zst` via nfpm) maps the staged
  compiled entry. Before `nfpm package`, run:

  ```sh
  tic -x -o ./target/terminfo terminfo/bitty.ti
  ```

  You can also run `tic -x terminfo/bitty.ti` post-install and `rm -f
/usr/share/terminfo/b/bitty` on remove. No new hook is added; the
  existing `packaging/scripts/*-postinstall.sh` stays a bounded no-op per
  pipeline convention.

Homebrew/macOS — install the entry into the user or system terminfo dir:

```sh
tic -x -o ~/.terminfo terminfo/bitty.ti
# or system-wide: sudo tic -x -o /usr/share/terminfo terminfo/bitty.ti
infocmp -x bitty
```

Windows — terminfo is irrelevant. Windows uses ConPTY passthrough; there is
no native terminfo database. Keep `TERM=xterm-256color` (the default) and
do not set `TERM=bitty` on Windows. `cargo check --target
x86_64-pc-windows-gnu` must stay green; no Windows packaging change ships
a terminfo file.

## Application icons

`packaging/icons/hicolor/` holds the launcher icons installed by every package format: `apps/bitty.png` at 16, 32, 64, 128, 256, and 512 px plus the scalable `scalable/apps/bitty.svg`. They are generated from the approved Bitty mascot artwork (the mascot peeking over a dark terminal window on a cream rounded-square background) and share one square framing. The PNGs carry transparent corners, so the icon reads on both light and dark launchers. Regenerate every size together when the approved artwork changes.

After installing or replacing icons, refresh the desktop icon cache so launchers stop showing a cached (possibly old) image:

```sh
gtk-update-icon-cache -f -t /usr/share/icons/hicolor   # adjust the prefix if installing elsewhere
```

## Desktop integration and AppStream

The fixed Linux application ID is `run.bitty.Bitty` (the reverse-DNS form of
`bitty.run`). One ID is used everywhere so desktop grouping, D-Bus, and any
future Flatpak identity agree from the start (no Flatpak artifact ships in
0.1.0; Flatpak stays future, not supported); the choice is recorded here and
must not be renamed silently:

- `.desktop` file: `packaging/run.bitty.Bitty.desktop`, installed to
  `/usr/share/applications/run.bitty.Bitty.desktop`, with
  `StartupWMClass=run.bitty.Bitty`.
- AppStream metainfo: `packaging/run.bitty.Bitty.metainfo.xml`, installed to
  `/usr/share/metainfo/run.bitty.Bitty.metainfo.xml`; `<id>` and
  `<launchable type="desktop-id">` use the same ID.
- Window identity: `const APP_ID` in `crates/bitty-platform/src/app.rs` sets the
  Wayland `app_id` and the X11 `WM_CLASS` pair to the same value.
- Icons keep the icon-theme name `bitty` (`Icon=bitty`); the theme name is not
  the application ID.

The metainfo carries name, summary, description, project/metadata license,
homepage, bug tracker, categories, release info, and the stock `bitty` icon.
Store screenshots are not produced yet (034 item 7 keeps screenshot production
out of scope); `appstreamcli validate` passes without them.

Validation: `bash scripts/check-desktop-integration.sh` asserts the ID agreement
across these files and runs `xmllint`, `appstreamcli validate`, and
`desktop-file-validate` when installed; `bash scripts/tests/check-desktop-integration.test.sh`
covers each drift case. Both run in `just check` and the CI Quality gates job,
and the release `validate` job reruns the gate.

## Nix Flake

`flake.nix` provides `packages.default` via `crane` + `rust-overlay` at `1.98.1`, filtered source bounded, no unsafe. Check:

```sh
nix flake check
nix build .#bitty
```

## AUR

`packaging/PKGBUILD` is the single Arch source-package recipe (the former root `PKGBUILD` mirror was retired). `packaging/PKGBUILD.bin` is the `bitty-bin` template: prebuilt `bitty-x86_64-unknown-linux-gnu` release binary with a real sha256 (never `SKIP` for the binary), `arch=('x86_64')` only, `provides=('bitty')`, `conflicts=('bitty' 'bitty-nightly' 'bitty-git')`. CI publishes both via `AUR_SSH_PRIVATE_KEY` (`aur` job for `bitty`, `aur-bin` job for `bitty-bin`, gated on `vars.AUR_PUBLISH` / `vars.AUR_BIN_PUBLISH`):

`bitty-bin` downloads the binary from the R2-backed CDN
(`https://cdn.bitty.run/releases/<TAG>/bitty-x86_64-unknown-linux-gnu`), the
preferred source for every package-manager channel (CTX-1413, #1847). The same
bytes are published on GitHub Releases, which the template documents as the
fallback; the metadata tarball is not mirrored, so it keeps the GitHub archive
URL. `makepkg` verifies the binary sha256 before packaging. The publish job
waits for `r2-mirror` and verifies the downloaded asset against its `.sha256`
sidecar before updating the recipe. It then resolves the rendered
`source_x86_64` entry and checks the `.SRCINFO` it ships through
`scripts/assert-aur-bin-render.sh`, so the recipe and the published metadata
must both resolve to the CDN URL and the verified digest; a recipe that
resolves anywhere else fails the channel.

```sh
makepkg --printsrcinfo > .SRCINFO
git push aur@aur.archlinux.org:bitty.git
git push aur@aur.archlinux.org:bitty-bin.git  # first push registers the package
```

Install from the AUR (prebuilt binary, no local compile):

```sh
paru -S bitty-bin   # or: yay -S bitty-bin
```

The source package (`bitty`) compiles the whole workspace locally and needs the Rust toolchain; prefer `bitty-bin` unless you specifically need a source build. `bitty` and `bitty-bin` conflict, so install one or the other. Only a tag-time published recipe counts as supported; a locally built package is not support evidence.

Validation: `bash -n packaging/PKGBUILD && (cd packaging && makepkg --printsrcinfo)`, plus `bash scripts/check-pkgbuild-source.sh` (source recipe installs the artifact declared by `crates/bitty-terminal/Cargo.toml`), `bash scripts/check-pkgbuild-bin.sh` (template render test), `bash scripts/tests/check-aur-bin-render.test.sh` (publish-assertion regression) and `bash scripts/check-release-version.sh [--tag vX.Y.Z]` (Cargo version stays aligned with release tags so `bitty --version` matches the tag).

## Cargo

There is no crates.io install path for the binary. `bitty-terminal` is
`publish = false` (the thin composition root is never published), and plain
`cargo install bitty` cannot work because the `bitty` name on crates.io is an
unrelated project. The supported cargo path builds from the Git repository:

```sh
cargo +1.98.1 install --git https://github.com/bitty-terminal/bitty.git bitty-terminal --locked
```

This installs the executable `bitty` (crate `bitty-terminal`, binary `bitty`) and
builds the workspace from source. Pin the toolchain with `+1.98.1` (rustup
installs it on demand): `cargo install --git` runs from your current directory
and does not read the repository's `rust-toolchain.toml`, so the pinned channel
must be selected explicitly. Pass `--force` to overwrite an existing install.
No release artifact, checksum, or install-smoke leg covers this path — it is a
convenience source install, not a packaged one.

## Homebrew

On release tags where the publish job runs, the Homebrew formula is generated by `.github/workflows/release.yml` from the release assets and committed to the external tap `bitty-terminal/homebrew-tap` at tag time. The formula downloads the per-arch binaries from the R2-backed CDN (`https://cdn.bitty.run/releases/<TAG>/bitty-<arch>-apple-darwin`) and embeds the published sha256; the generated formula documents the GitHub Releases URL as the fallback. The publish job waits for `r2-mirror` and verifies each artifact against its `.sha256` sidecar before rendering. Only a tag-time published formula counts as supported; a CI-built macOS binary is not support evidence. The repository keeps no in-repo formula copy; the former build-from-source `0.0.1` reference was retired as stale.

## Scoop

On release tags where the publish job runs, the Scoop manifest is generated by `.github/workflows/release.yml` from the release assets and committed to the external bucket `bitty-terminal/scoop-bucket` at tag time. Both architecture URLs and the `autoupdate` templates point at the R2-backed CDN (`https://cdn.bitty.run/releases/<TAG>/bitty-<arch>-pc-windows-msvc.exe`), the manifest embeds the published sha256 hash, and its `notes` field documents the GitHub Releases fallback. The publish job waits for `r2-mirror` and verifies each artifact against its `.sha256` sidecar before rendering. Only a tag-time published manifest counts as supported; a CI-built Windows binary is not support evidence. The repository keeps no in-repo manifest copy; the former `0.0.1` reference was retired as stale. Scoop stays portable: it ships the bare `bitty-*.exe` assets, never the setup installer below, so it sidesteps the setup-installer dialog (Windows may still show a first-run reputation check).

## Windows setup installer (CTX-1064, #1810)

The Windows setup track ships a setup-style wizard installer alongside the
portable ZIP. Tech choice: Inno Setup 6 (`packaging/windows/bitty.iss`) over
WiX v4/v5 and NSIS — the stock modern wizard already provides
install/uninstall pages, Start Menu plus opt-in Desktop shortcuts, and the
per-user/per-machine registry uninstall entry from declarative
`[Setup]`/`[Tasks]`/`[Files]`/`[Icons]` sections; the compiler is a single
`ISCC.exe` installed on `windows-latest` via Chocolatey with no paid infra
and no .NET/WiX toolchain, while NSIS would need hand-written
registry/uninstall scripting for the same bar.

- Source: `packaging/windows/bitty.iss`. The version arrives as
  `/DMyAppVersion=X.Y.Z` (release workflow: tag, then workflow input, then
  the Cargo workspace version — never hardcoded) and the payload stages from
  `/DMySourceDir=<absolute stage>` carrying `bitty.exe` plus `LICENSE`,
  `README.md`, and `CHANGELOG.md` (the portable-ZIP payload).
- Wizard: `WizardStyle=modern`, per-user default (`PrivilegesRequired=lowest`
  with a per-user/per-machine dialog override), `x64compatible` only.
  `[Tasks]` offers the `desktopicon` opt-in on the standard Additional Icons
  page; `[Icons]` writes the Start Menu `Bitty` group (launch + uninstall)
  and the Desktop `Bitty` link only when that task is selected; `[Run]`
  offers the post-install launch. No PATH or file-association writes ship in
  0.1.0 — those stay deferred decisions.
- Registry: Inno writes the uninstall entry automatically under
  `Software\Microsoft\Windows\CurrentVersion\Uninstall\<AppId>_is1` (HKCU for
  the per-user default, HKLM for per-machine) with `DisplayName`,
  `DisplayVersion`, and `UninstallString`; there is no manual `[Registry]`
  block for uninstall data.
- Unsigned 0.1.0: no `SignTool` is configured (only a commented stub for the
  post-0.2.0 Azure Trusted Signing / OV revisit per the owner decision on
  #1810). The installer must not claim trust it does not have: SmartScreen
  will show the standard unsigned warning and the click-through guidance
  (More info -> Run anyway) lives in `packaging/WINDOWS-SMARTSCREEN.md`
  (see #1866). The verify job asserts
  `Get-AuthenticodeSignature` reports `NotSigned` so a silent signing without
  updating the script and this README fails CI instead of shipping quietly.
- Artifact: `bitty-<VERSION>-windows-x86_64-setup.exe` plus its `.sha256`
  sidecar (`windows-installer` job), published to the GitHub Release and the
  R2 mirror like every other artifact. It is a manual-download path:
  `install.ps1` keeps fetching the portable ZIP and Scoop keeps shipping the
  bare exes. Windows arm64 stays a bare `.exe` until the ZIP matrix extends.
- Verification: `verify-windows-installer` runs a silent per-user
  install/uninstall round-trip on a clean Windows runner into a temp
  directory (`/SILENT /TASKS="desktopicon"`), asserting the setup name, the
  `NotSigned` status, `bitty.exe --version`, the payload, the Start Menu and
  Desktop shortcuts, and the registry uninstall entry with its version and
  `UninstallString`, then asserting full cleanup after silent uninstall.
- Static gate: `scripts/check-windows-installer.sh` (run by
  `just windows-installer-test` with `scripts/tests/check-windows-installer.test.sh`)
  pins this contract on Linux; `ISCC.exe` and Windows never run locally.

## Release Matrix

`.github/workflows/release.yml` builds for:

- linux x64 (`x86_64-unknown-linux-gnu`, ubuntu-latest)
- linux aarch64 (`aarch64-unknown-linux-gnu`, ubuntu-22.04 cross via `aarch64-linux-gnu-gcc`; the artifact's ELF machine and interpreter are asserted before packaging via `scripts/check-binary-arch.sh`)
- linux x64 musl (`x86_64-unknown-linux-musl`, Alpine 3.22 container, native musl build) — the `.apk`
- versioned Linux bundles (`bitty-<version>-<target>.tar.zst` for the three
  Linux targets, `unix-bundle` job)
- windows x64 (`x86_64-pc-windows-msvc`, windows-latest)
- windows aarch64 (`aarch64-pc-windows-msvc`, windows-latest)
- windows x64 portable ZIP (`bitty-<version>-windows-x86_64.zip`, `windows-zip` job)
- windows x64 setup installer (`bitty-<version>-windows-x86_64-setup.exe`, `windows-installer` job; verified by `verify-windows-installer`)
- macos x64 (`x86_64-apple-darwin`, macos-14)
- macos aarch64 (`aarch64-apple-darwin`, macos-14)
- macos Universal 2 (`Bitty-<version>-universal.dmg`, `macos-universal` job)

The Universal 2 DMG fuses the two macOS slices into `Bitty.app` with `lipo`
inside `scripts/make-macos-dmg.sh`; the job mounts the DMG and launches both
slices before upload. The app is intentionally unsigned — codesign/notarize is
deferred (034 item 11) — and the bare triple binaries keep shipping.

The portable ZIP packs `bitty.exe` with `LICENSE`, `README.md` and
`CHANGELOG.md` at the archive root (assembled by `scripts/make-windows-zip.sh`,
no installer); the bare `.exe` assets keep shipping for Scoop and direct
downloads. The `verify-windows-zip` job unzips on a clean Windows runner and
runs `bitty.exe --version`.

The versioned Linux bundles pack `bin/bitty` with `share/` (desktop entry,
icons, metainfo) plus `LICENSE`, `README.md`, and `CHANGELOG.md` under one
top-level `bitty-<version>-<target>/` directory (assembled by
`scripts/make-windows-zip.sh`'s sibling `scripts/make-unix-bundle.sh`; the
bare triple binaries keep shipping for now). The `verify-unix-bundle` job
expands each archive on a clean Ubuntu runner and runs `bin/bitty --version`.

Plus nfpm packaging for linux x64/aarch64, a runtime-dependency gate (`ldd` + `readelf -d` vs the declared per-distro deps) for the x64 glibc and musl packages, clean-container install smoke jobs for Ubuntu/Fedora/Arch/Alpine, and optional AUR/Homebrew/Scoop bumps gated on secrets. Only legs that passed and publishes that completed successfully count as support evidence (see `Support evidence` above); a build-only cell without a `verify-*` leg stays best-effort.

All packaging keeps bounded contracts: no unbounded file lists, no unsafe, fixed version substitution, scripts are no-ops.

## R2 bucket layout and CDN mapping (CTX-1056, closes #1794)

Decision record for the R2/CDN publishing convention. R2 upload and CDN
transport are owner-operated; this section decides bucket key names, the
key-to-URL mapping, pointer classes with cache policies, checksum artifact
names, and which artifact per OS the bootstrap scripts fetch. It uploads
nothing. CTX-1047 Phase 2 implements `install.sh` and `install.ps1` against
the literal URL templates below.

`TAG` is the git tag with its leading `v` (for example `v0.0.23`) and doubles
as the R2 path segment. `VERSION` is the same number without the leading `v`
(for example `0.0.23`) and is what bundle, ZIP, and DMG file names embed.

### Bucket and prefixes

Bucket `bitty`. Object addresses below are written `bitty/<key>` (bucket
name, then key) to match the `wrangler r2 object put "bitty/<key>"`
invocations in the release workflow; the key itself is the part after the
bucket name. Every object address keeps the `bitty/` bucket prefix so the one
shipped and verified bucket never moves:

| Prefix                                                             | Class                                 | State                                                                                                             |
| ------------------------------------------------------------------ | ------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `bitty/releases/<TAG>/<artifact>`                                  | Immutable versioned release payload   | Shipped and verified by the `r2-mirror` job                                                                       |
| `bitty/install/latest.txt`                                         | Mutable stable version pointer        | Published by the `r2-stable` job (new in this task)                                                               |
| `bitty/install/install.sh`                                         | Mutable stable script object          | Reserved; lands with CTX-1047 Phase 2, same cache policy                                                          |
| `bitty/install/install.ps1`                                        | Mutable stable script object          | Reserved; lands with CTX-1047 Phase 2, same cache policy                                                          |
| `bitty/extensions/<name>/<version>/<target>.tar.gz` + `SHA256SUMS` | Immutable versioned extension payload | Packed by `scripts/make-component-dist.sh` (#1792); published by the extension release CD jobs                    |
| `bitty/extensions/catalog.v1`                                      | Mutable registry metadata             | Published (schema 1, 2026-10-22); generated and read-back-verified by `scripts/make-extension-catalog.sh` (#2440) |
| `bitty/docs/<version>/<lang>.tar.gz`                               | Immutable versioned docs payload      | Reserved manual track, not uploaded here                                                                          |

See `bitty-terminal-docs/architecture/directory-semantics.md` for extension directory semantics.

### CDN key-to-URL mapping

The CDN serves the bucket root one-to-one: the URL path is the R2 object
key, which is the part after the `bitty/` bucket name in
`wrangler r2 object put "bitty/<key>"`. Scripts must use the key without
the bucket name:

```text
https://cdn.bitty.run/<R2-key>
https://cdn.bitty.run/releases/v0.0.23/SHA256SUMS
https://cdn.bitty.run/install/latest.txt
```

### Pointer classes and cache policy

| Class                                                     | Keys                                                               | `Cache-Control`                                                                                   |
| --------------------------------------------------------- | ------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------- |
| Immutable versioned (`releases/`, `extensions/`, `docs/`) | Every versioned artifact, sidecar, `SHA256SUMS`, `provenance.json` | `public, max-age=31536000, immutable` (already emitted by `r2-mirror`)                            |
| Mutable registry metadata (`extensions/catalog.v1`)       | Extension name/version/target/digest index                         | `public, max-age=300, must-revalidate` (5 minutes; readers fail closed on a fetched bad object)   |
| Mutable stable (`install/`)                               | `latest.txt`, `install.sh`, `install.ps1`                          | `public, max-age=300, must-revalidate` (5 minutes, bounded staleness; scripts re-fetch every run) |

### Checksums and signatures

Every bare binary and archive ships a `<artifact>.sha256` sidecar (one line,
`<hex>  <filename>`, GNU `sha256sum` format) for single-file script-side
verification: the Linux, macOS, and Windows bare binaries, the Linux `.tar.zst`
bundles, the portable Windows ZIP, the Windows setup `.exe`, and the macOS DMG.
The aggregate `SHA256SUMS` (`<hex>  <filename>` for every file under `dist/`)
and `provenance.json` (`commit`, `tag`, `date`, `toolchain`, `build_runner`)
travel alongside the payload as the authoritative release manifest. Bootstrap
scripts verify the sidecar and fail closed on mismatch.

Two documented exceptions to "every artifact has a sidecar":

- `.deb`, `.rpm`, `.apk`, and the pacman `.pkg.tar.zst` are assembled by nfpm
  in the build jobs, after the per-file checksum step, and ship **no**
  `<package>.sha256` sidecar. Verify them against their `SHA256SUMS` entry (one
  line per package) or the package manager's own metadata chain.
- FreeBSD publishes one `bitty-x86_64-unknown-freebsd.sha256` sidecar named
  after the binary that lists **both** payloads (the bare binary and the
  `.tar.xz` bundle, two lines) instead of one sidecar per file.

The tag-time channel publishers (`aur-bin`, `homebrew`, `scoop`) verify the
published sidecar through `scripts/verify-cdn-sidecar.sh` before they write a
recipe or manifest. A `channel-summary` job always runs on tag builds and lists
every package channel's result in the run summary, failing when any enabled
channel did not publish, so one stale channel cannot hide behind the green
ones. There are no signatures in 0.1.0: the Windows build is
intentionally unsigned (paid signing deferred past 0.2.0 per #1810), so
0.1.0 verifies by hash only. Sigstore or cosign stays a follow-up, not a
silent addition.

### Bootstrap artifacts per OS

`install.sh` (Linux, macOS) and `install.ps1` (Windows) fetch exactly one
artifact per OS and arch. OS and arch segments below are literal; only `TAG`
and `VERSION` resolve at install time via `latest.txt`:

| Script OS and arch         | Bootstrap artifact (literal template)                                                    |
| -------------------------- | ---------------------------------------------------------------------------------------- |
| Linux x86_64 glibc         | `https://cdn.bitty.run/releases/<TAG>/bitty-<VERSION>-x86_64-unknown-linux-gnu.tar.zst`  |
| Linux aarch64 glibc        | `https://cdn.bitty.run/releases/<TAG>/bitty-<VERSION>-aarch64-unknown-linux-gnu.tar.zst` |
| Linux x86_64 musl (Alpine) | `https://cdn.bitty.run/releases/<TAG>/bitty-<VERSION>-x86_64-unknown-linux-musl.tar.zst` |
| macOS arm64                | `https://cdn.bitty.run/releases/<TAG>/bitty-aarch64-apple-darwin`                        |
| macOS x86_64               | `https://cdn.bitty.run/releases/<TAG>/bitty-x86_64-apple-darwin`                         |
| Windows x64                | `https://cdn.bitty.run/releases/<TAG>/bitty-<VERSION>-windows-x86_64.zip`                |
| Windows arm64              | `https://cdn.bitty.run/releases/<TAG>/bitty-aarch64-pc-windows-msvc.exe`                 |

Each row has a `<artifact>.sha256` sidecar at the same URL with `.sha256`
appended, which the script verifies before installing. Worked example at
`TAG=v0.0.23`, `VERSION=0.0.23`:

```text
https://cdn.bitty.run/releases/v0.0.23/bitty-0.0.23-x86_64-unknown-linux-gnu.tar.zst
https://cdn.bitty.run/releases/v0.0.23/bitty-0.0.23-aarch64-unknown-linux-gnu.tar.zst
https://cdn.bitty.run/releases/v0.0.23/bitty-0.0.23-x86_64-unknown-linux-musl.tar.zst
https://cdn.bitty.run/releases/v0.0.23/bitty-aarch64-apple-darwin
https://cdn.bitty.run/releases/v0.0.23/bitty-x86_64-apple-darwin
https://cdn.bitty.run/releases/v0.0.23/bitty-0.0.23-windows-x86_64.zip
https://cdn.bitty.run/releases/v0.0.23/bitty-aarch64-pc-windows-msvc.exe
```

Rationale per OS:

- Linux fetches the versioned `.tar.zst` bundle, not the bare triple binary.
  The bundle (`scripts/make-unix-bundle.sh`, verified by `verify-unix-bundle`)
  carries `bin/bitty` plus the desktop entry, icons, AppStream metainfo, and
  `LICENSE`/`README.md`/`CHANGELOG.md` under one top-level directory; the bare
  binary has no desktop integration. `install.sh` extracts `bin/bitty` and
  installs `share/` alongside it. Musl/Alpine uses its own native-musl bundle,
  never the glibc one.
- macOS fetches the bare per-arch binary, not the Universal DMG. The DMG is an
  interactive drag-to-install image (`hdiutil` mount plus Finder copy),
  unsuitable for headless `curl | bash`; a bare binary is a single directly
  executable file and is already the Homebrew fetch artifact. The DMG stays
  the manual-download path.
- Windows x64 fetches the portable ZIP (`scripts/make-windows-zip.sh`,
  verified by `verify-windows-zip`): `bitty.exe` plus docs at the archive
  root, expanded with `Expand-Archive`. Windows arm64 fetches the bare
  `bitty-aarch64-pc-windows-msvc.exe` because the ZIP matrix builds x64 only;
  both arches install first-class, only the container differs until the ZIP
  matrix extends (recorded follow-up, not a parity gap).
- FreeBSD Tier 2 is not a bootstrap target: no `install.sh` path serves the
  `bitty-<VERSION>-x86_64-unknown-freebsd.tar.xz`; it stays manual download
  and scripts fail closed there with guidance.

### Channel download sources (CTX-1413, #1847)

The R2-backed CDN is the preferred source for every package-manager channel.
GitHub Releases always carry the same bytes and stay the documented
fallback (per-channel comments, `notes`, or the AUR metadata tarball above).
`r2-mirror` publishes the immutable payload and verifies it by read-back hash
before any channel publisher runs:

| Channel         | Primary URL                                                               | Fallback                   | sha256 check                          |
| --------------- | ------------------------------------------------------------------------- | -------------------------- | ------------------------------------- |
| AUR `bitty-bin` | `https://cdn.bitty.run/releases/<TAG>/bitty-x86_64-unknown-linux-gnu`     | GitHub Releases binary URL | `sha256sums_x86_64` at `makepkg` time |
| Homebrew        | `https://cdn.bitty.run/releases/<TAG>/bitty-<arch>-apple-darwin`          | GitHub Releases comment    | formula `sha256`                      |
| Scoop           | `https://cdn.bitty.run/releases/<TAG>/bitty-<arch>-pc-windows-msvc.exe`   | GitHub Releases `notes`    | manifest `hash`                       |
| `install.sh`    | `https://cdn.bitty.run/releases/<TAG>/bitty-<VERSION>-<target>.tar.zst`   | GitHub Releases asset      | `.sha256` sidecar, fail closed        |
| `install.ps1`   | `https://cdn.bitty.run/releases/<TAG>/bitty-<VERSION>-windows-x86_64.zip` | GitHub Releases asset      | `.sha256` sidecar, fail closed        |

`scripts/render-package-manifests.sh` renders the Homebrew formula and Scoop
manifest for those URLs; `scripts/tests/check-package-manifests.test.sh` pins
the R2 host, the expected digest, and the fallback documentation without
network access.

### Stable entry points

```sh
curl -fsSL https://cdn.bitty.run/install/install.sh | bash
```

```powershell
irm https://cdn.bitty.run/install/install.ps1 | iex
```

Both scripts resolve the version through one pointer:

```sh
curl -fsSL https://cdn.bitty.run/install/latest.txt
```

### latest.txt format

Single line holding `TAG` with a trailing newline, for example `v0.0.23`.
Scripts strip the leading `v` to derive `VERSION` for bundle and ZIP names.
The `r2-stable` job writes and read-back-verifies this exact byte content on
every non-prerelease tag push after `r2-mirror` succeeds, skipping the write
when the candidate is older than the stored pointer.

### Lifecycle and prune policy

Versioned prefixes are immutable once published: re-runs only overwrite a key
with byte-identical content (the existing `r2-mirror` convergence comment).
Stable `install/` pointers move forward only: `r2-stable` runs on
non-prerelease version tags, compares the candidate against the stored
`latest.txt` (a missing key means first publish and proceeds), skips the write
when the candidate is older, and verifies the retained pointer on skip.
Prerelease tags (any tag containing `-`) never update `latest.txt`. No prefix
is pruned pre-0.1.0; any future retention rule needs its own decision, never a
silent delete.

### Workflow wiring

`r2-mirror` keeps mirroring `dist/*` to `bitty/releases/<TAG>/` with immutable
cache-control and read-back hash verification (unchanged). The new `r2-stable`
job (needs `r2-mirror`) publishes `latest.txt` with the mutable cache policy
and read-back-verifies it. `install.sh` and `install.ps1` uploads join
`r2-stable` when CTX-1047 Phase 2 lands the scripts; their keys and cache
policy are reserved here so Phase 2 changes no convention.

### Extension dist layout (CTX-1063, #1792)

Prebuilt extension distribution for machines without cargo and without a Rust
network stack in `bitty`. The client side is implemented: `bitty extension
install <name>` reads this layout through the catalog and verifies the
`SHA256SUMS` digest before staging (the minimal seed that bootstraps the
manager is #1791). This section ships the artifact side only: pack mechanics,
manifest generation, and the upload contract. Core uploads nothing.

`VERSION` is the strict `X.Y.Z` extension version from `bitty-component.toml`
(the internal descriptor file name); pack policy takes only the `X.Y.Z` core
(no leading zeros, no prerelease or build metadata), intentionally narrower
than the full semver the installed descriptor parser accepts. `TARGET` is the Rust target triple the binary was
built for. Both segments below are literal; only `NAME`, `VERSION`, and
`TARGET` vary:

```text
https://cdn.bitty.run/extensions/<name>/<version>/<target>.tar.gz
https://cdn.bitty.run/extensions/<name>/<version>/SHA256SUMS
```

Worked example (`net`, `0.0.23`, Linux x86_64 glibc):

```text
https://cdn.bitty.run/extensions/net/0.0.23/x86_64-unknown-linux-gnu.tar.gz
https://cdn.bitty.run/extensions/net/0.0.23/SHA256SUMS
```

Pack one target with `scripts/make-component-dist.sh` (needs only `tar`,
`gzip`, and `sha256sum` (or `shasum -a 256`)):

```sh
scripts/make-component-dist.sh --source ./dist/net-stage --version 0.0.23 \
  --target x86_64-unknown-linux-gnu --binary ./dist/bitty-net \
  --output ./dist/extensions/x86_64-unknown-linux-gnu.tar.gz
scripts/make-component-dist.sh --print-url --name net --version 0.0.23 \
  --target x86_64-unknown-linux-gnu
```

The tarball holds exactly two members at the archive root (no wrapper
directory): the executable `bitty-<name>` (mode 755) and
`bitty-component.toml`. The client extracts into the version directory it
creates (`<root>/<name>/<version>/`), so a wrapper directory would nest one
level too deep. The source descriptor is validated against the installed
parser rules (closed `[component]` table, `[a-z][a-z0-9-]{0,31}` name,
descriptor version equal to `--version`, `1 <= protocol min <= max`,
`executable` equal to `bitty-<name>`, required lowercase-hex `sha256` that
must match the binary); anything else fails closed before any output is
written, and the `--output` basename must be `<target>.tar.gz` so the R2 key
name cannot drift. Each tarball gets a `<target>.tar.gz.sha256` sidecar, and
the script regenerates the aggregate `SHA256SUMS` over every target tarball
in the output directory. The `.github/workflows/ci.yml` quality job runs the
fixture test (`scripts/tests/make-component-dist.test.sh`, also
`just component-dist-test`), which pins the member list, the digest chain,
the literal URL templates, and a hostile-URL corpus (traversal, schemes,
lookalike hosts, non-https bases) refused before any emission.

Extension repositories publish their own `<name>/<version>/` directory
(tarballs, sidecars, `SHA256SUMS`) under `bitty/extensions/` with the
immutable cache policy (`public, max-age=31536000, immutable`) and read-back
hash verification. `bitty-graphics` publishes `graphics` and `bitty-network`
publishes `net`; this repository does not upload extension payloads.
Versioned extension prefixes are immutable once published, like `releases/`.

### Extension catalog (v1, #2111, #2440)

The extension registry backend is a single bounded metadata object on the same
bucket and CDN host as the payloads. It is published:

```text
R2 key:   bitty/extensions/catalog.v1
CDN URL:  https://cdn.bitty.run/extensions/catalog.v1
```

Live object (verified by read-back on 2026-10-22; 417 bytes, SHA-256
`6d9018f0b880aa179efddf0d53c5af0b18583f4ea77db8924a8d01093b4fd6c8`,
schema 1):

```text
# bitty extension catalog v1
schema 1
extension graphics 0.0.24 x86_64-unknown-linux-gnu 7e331044e6bbede32909fb04729f563e4645d5c1f1ba8c32d00aeb79a8631a02
extension net 0.0.1 x86_64-unknown-linux-gnu 1764de2db9eb1a00dd80535dfcf744b28d4d74dae2af12c4a31e01c4156b1615
```

It stays on `cdn.bitty.run` rather than the plugin store host because the
installer egress allowlist is exactly one first-party host
(`cdn.bitty.run:443`); adding a second host for metadata would widen the
trust boundary for no security gain. Lua plugin registry metadata
(`generated/registry.json` in `bitty-plugins`) is a separate corpus and is
not read by `bitty extension install`.

`bitty extension install <name>` fetches this object after the explicit
download consent and, when `--version` is omitted, selects the highest strict
`X.Y.Z` record for the exact `(name, host target)` pair. `--version X.Y.Z`
still pins exactly one `(name, version, host target)` key. The catalog is a
digest index plus a latest selector, not a version solver: prereleases are not
published by the pack policy, so they never participate. A latest install
prints the resolved version before the release download. A pinned install
keeps the offline paths described below (release `SHA256SUMS` fallback and
verified local reuse); a latest install cannot fall back (there is no version)
and fails closed with a `--version` hint.

#### Grammar

UTF-8, line-oriented, bounded. Blank lines and lines starting with `#` are
ignored. Exactly one `schema 1` line must appear before any record. Every
record is one published target artifact:

```text
schema 1
extension <name> <version> <target> <sha256>
```

- `<name>` uses the extension-name grammar `[a-z][a-z0-9-]{0,31}`.
- `<version>` uses the strict `X.Y.Z` pack policy (no prerelease or build
  metadata).
- `<target>` uses the lowercase Rust target-triple grammar.
- `<sha256>` is exactly 64 lowercase hex characters: the digest of
  `<target>.tar.gz`, byte-identical to the entry in that release's
  `SHA256SUMS`.
- Duplicate `(name, version, target)` records, unknown directives, records
  before `schema 1`, extra fields, and non-UTF-8 input fail closed.

Client bounds: 256 KiB object, 4096 lines, 1024 bytes per line, 1024 entries.
The installer parses the object only after a successful fetch within those
bounds. The live object (2026-10-22) carries the two published artifacts; the
records are derived from their `SHA256SUMS`, never hand-written.

#### Read path and fail-closed matrix

| Condition                                                                             | Installer behavior                                                                                                            |
| ------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| Catalog fetched, entry present, digests agree                                         | Continue: catalog digest and `SHA256SUMS` both must equal the tarball; the exact-two-member audit still runs                  |
| Catalog fetched, entry absent                                                         | Fail closed (exit `4`) before the tarball fetch; absence is authoritative, never "fall back and guess"                        |
| Catalog fetched, malformed/oversize/duplicate                                         | Fail closed (exit `4`) before the tarball fetch                                                                               |
| Catalog transport failure, pinned `--version`, version installed and locally verified | Reuse the installed version with no download (offline idempotence); a tampered local copy does not qualify                    |
| Catalog transport failure, pinned `--version`, otherwise                              | Explicit offline fallback: fetch the release `SHA256SUMS` and verify it as before; explicit consent already applied           |
| Catalog transport failure, bare latest                                                | Fail closed (exit `4`) with a concrete `--version` example (HTTP 404 is named as "not published"); a version is never guessed |
| Catalog digest differs from `SHA256SUMS` or the tarball                               | Fail closed (exit `4`) before unpacking or staging                                                                            |
| `--digest` supplied and differing                                                     | Fail closed (exit `4`); caller input is an extra constraint, never a replacement for canonical metadata                       |

The offline fallback is deliberately narrow: it applies only to a pinned
install whose catalog transport failed, keeps the original hash-verified
single-release pipeline and exact-two-member archive audit, and never enables
a silent install. A successful catalog response is authoritative and cannot be
bypassed. Latest selection is a convenience, not a trust upgrade: the catalog
stays mutable metadata, so reproducible automation should pin `--version`.

#### Generation and publication (#2440)

`scripts/make-extension-catalog.sh` is the only writer of `catalog.v1`. It
cannot drift from `SHA256SUMS` by construction:

1. Discovery lists `extensions/` through the Cloudflare R2 API (the CDN has no
   directory listing) and keeps every `<name>/<version>/SHA256SUMS`.
2. For each manifest it parses every `<target>.tar.gz` line, downloads the
   artifact, and re-hashes it; a missing or mismatched artifact aborts before
   any output is written.
3. Records are emitted in canonical order (name, numeric version, target) with
   the fixed header, so the same inputs always produce byte-identical output.

```sh
# Generate locally (stdout or --output); refuses an empty catalog.
scripts/make-extension-catalog.sh --output /tmp/catalog.v1

# Drift gate: exit 1 and print a diff when the live object differs.
scripts/make-extension-catalog.sh --check

# Upload, read the object back from R2, then verify the CDN URL (bounded
# retries for the 5-minute mutable cache). Credentials come from the
# environment; never commit them.
scripts/make-extension-catalog.sh --publish
```

Requires `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`, and `wrangler`
(`R2_BUCKET` defaults to `bitty`). The token is fed to `curl` through a stdin
config, never argv. Publishing uses `public, max-age=300, must-revalidate`
and `text/plain; charset=utf-8`; an edge that cached the old unpublished 404
may serve it until that TTL expires, so retry or purge before calling a
failed read-back a publication failure.
`--publish` is append-only for published versions: a live record that no
longer appears in the generated catalog aborts unless `--allow-yank` is
passed, because removing an entry is an explicit yank decision. A catalog
that lags its payloads fails closed (entry absent) rather than accepting an
unpublished release; a catalog that leads its payloads fails closed on digest
mismatch. Versioned payload prefixes stay immutable; only `catalog.v1` is
mutable metadata.

The fixture test (`scripts/tests/make-extension-catalog.test.sh`, also
`just extension-catalog-test`) pins canonical bytes, numeric ordering, the
digest chain, and the fail-closed cases (missing/malformed/duplicate
manifests, tampered artifacts, empty discovery). The Rust client parses the
same checked-in golden fixture, so the publisher and the installer share one
grammar and one document.

#### Distribution track (not per Core release)

Extensions are published on their own release track. A Core release tag (for
example `v0.0.25`) does not imply a `0.0.25` extension payload: only versions
listed in `catalog.v1` exist, and `latest` selects the highest strict version
for the host target from that list. Never infer extension availability from
the Core version; use the catalog or fetch
`https://cdn.bitty.run/extensions/<name>/<version>/SHA256SUMS`.

#### Trust

Unsigned 0.1.0 verifies by hash only, so the catalog is canonical metadata,
not a signature. HTTPS pins the host and path; the catalog digest and the
release `SHA256SUMS` are compared, and the first successful install records
a TOFU pin under `<user>/<name>/tarball.sha256`. A later catalog or payload
substitution for an installed extension fails closed as pin-mismatch until
an explicit `--force` re-pin. Signatures (Sigstore/cosign or an Ed25519 key
directory with a rotation story) remain a follow-up, not a silent addition.
