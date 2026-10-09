# Packaging

This directory holds multi-distro packaging recipes and helpers for Bitty releases.

## Formats

| Format    | File                                                     | Distro                                           |
| --------- | -------------------------------------------------------- | ------------------------------------------------ |
| deb       | `nfpm.yaml` + `deb` packager                             | Debian 12, Ubuntu 24.04                          |
| rpm       | `nfpm.yaml` + `rpm` packager                             | Fedora 40, RHEL 9, OpenSUSE Tumbleweed/Leap      |
| apk       | `nfpm.yaml` + `apk` packager                             | Alpine 3.22 (native `x86_64-unknown-linux-musl`) |
| archlinux | `nfpm.yaml` + `archlinux` packager, `packaging/PKGBUILD` | Arch, AUR                                        |

OpenSUSE is rpm-based; the rpm built via nfpm is tested with `rpm -qip` and installs via `zypper install`. Alpine is apk-based; the apk is a native musl build (see `packaging/alpine.md`).

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

## Application icons

`packaging/icons/hicolor/` holds the launcher icons installed by every package format: `apps/bitty.png` at 16, 32, 64, 128, 256, and 512 px plus the scalable `scalable/apps/bitty.svg`. They are generated from the approved Bitty mascot artwork (the mascot peeking over a dark terminal window on a cream rounded-square background) and share one square framing. The PNGs carry transparent corners, so the icon reads on both light and dark launchers. Regenerate every size together when the approved artwork changes.

After installing or replacing icons, refresh the desktop icon cache so launchers stop showing a cached (possibly old) image:

```sh
gtk-update-icon-cache -f -t /usr/share/icons/hicolor   # adjust the prefix if installing elsewhere
```

## Desktop integration and AppStream

The fixed Linux application ID is `run.bitty.Bitty` (the reverse-DNS form of
`bitty.run`). One ID is used everywhere so desktop grouping, D-Bus, and any
future Flatpak identity agree from the start; the choice is recorded here and
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

```sh
makepkg --printsrcinfo > .SRCINFO
git push aur@aur.archlinux.org:bitty.git
git push aur@aur.archlinux.org:bitty-bin.git  # first push registers the package
```

Install from the AUR (prebuilt binary, no local compile):

```sh
paru -S bitty-bin   # or: yay -S bitty-bin
```

The source package (`bitty`) compiles the whole workspace locally and needs the Rust toolchain; prefer `bitty-bin` unless you specifically need a source build. `bitty` and `bitty-bin` conflict, so install one or the other.

Validation: `bash -n packaging/PKGBUILD && (cd packaging && makepkg --printsrcinfo)`, plus `bash scripts/check-pkgbuild-source.sh` (source recipe installs the artifact declared by `crates/bitty-terminal/Cargo.toml`), `bash scripts/check-pkgbuild-bin.sh` (template render test) and `bash scripts/check-release-version.sh [--tag vX.Y.Z]` (Cargo version stays aligned with release tags so `bitty --version` matches the tag).

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

The published Homebrew Formula is generated by `.github/workflows/release.yml` from the release assets and committed to the external tap `bitty-terminal/homebrew-tap` at tag time. The repository keeps no in-repo formula copy; the former build-from-source `0.0.1` reference was retired as stale.

## Scoop

The published Scoop manifest is generated by `.github/workflows/release.yml` from the release assets and committed to the external bucket `bitty-terminal/scoop-bucket` at tag time. The repository keeps no in-repo manifest copy; the former `0.0.1` reference was retired as stale.

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

Plus nfpm packaging for linux x64/aarch64, a runtime-dependency gate (`ldd` + `readelf -d` vs the declared per-distro deps) for the x64 glibc and musl packages, clean-container install smoke jobs for Ubuntu/Fedora/Arch/Alpine, and optional AUR/Homebrew/Scoop bumps gated on secrets.

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

Bucket `bitty`. Every key keeps the `bitty/` root prefix so the one shipped
and verified prefix never moves:

| Prefix                                                             | Class                                 | State                                                    |
| ------------------------------------------------------------------ | ------------------------------------- | -------------------------------------------------------- |
| `bitty/releases/<TAG>/<artifact>`                                  | Immutable versioned release payload   | Shipped and verified by the `r2-mirror` job              |
| `bitty/install/latest.txt`                                         | Mutable stable version pointer        | Published by the `r2-stable` job (new in this task)      |
| `bitty/install/install.sh`                                         | Mutable stable script object          | Reserved; lands with CTX-1047 Phase 2, same cache policy |
| `bitty/install/install.ps1`                                        | Mutable stable script object          | Reserved; lands with CTX-1047 Phase 2, same cache policy |
| `bitty/components/<name>/<version>/<target>.tar.gz` + `SHA256SUMS` | Immutable versioned component payload | Reserved for #1792, not uploaded here                    |
| `bitty/docs/<version>/<lang>.tar.gz`                               | Immutable versioned docs payload      | Reserved manual track, not uploaded here                 |

### CDN key-to-URL mapping

The CDN serves the bucket root one-to-one: the URL path is the full R2 key,
including the `bitty/` prefix. Scripts must not strip it:

```text
https://cdn.bitty.run/<R2-key>
https://cdn.bitty.run/bitty/releases/v0.0.23/SHA256SUMS
https://cdn.bitty.run/bitty/install/latest.txt
```

### Pointer classes and cache policy

| Class                                                            | Keys                                                               | `Cache-Control`                                                                                   |
| ---------------------------------------------------------------- | ------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------- |
| Immutable versioned (`releases/`, future `components/`, `docs/`) | Every versioned artifact, sidecar, `SHA256SUMS`, `provenance.json` | `public, max-age=31536000, immutable` (already emitted by `r2-mirror`)                            |
| Mutable stable (`install/`)                                      | `latest.txt`, `install.sh`, `install.ps1`                          | `public, max-age=300, must-revalidate` (5 minutes, bounded staleness; scripts re-fetch every run) |

### Checksums and signatures

Each artifact ships a `<artifact>.sha256` sidecar (one line,
`<hex>  <filename>`, GNU `sha256sum` format) for single-file script-side
verification, plus the aggregate `SHA256SUMS` and `provenance.json`
(`commit`, `tag`, `date`, `toolchain`, `build_runner`) as the authoritative
release manifest. Bootstrap scripts verify the sidecar and fail closed on
mismatch. The FreeBSD binary and tarball share one
`bitty-x86_64-unknown-freebsd.sha256` sidecar listing both files. There are no signatures in 0.1.0: the Windows build is
intentionally unsigned (paid signing deferred past 0.2.0 per #1810), so
0.1.0 verifies by hash only. Sigstore or cosign stays a follow-up, not a
silent addition.

### Bootstrap artifacts per OS

`install.sh` (Linux, macOS) and `install.ps1` (Windows) fetch exactly one
artifact per OS and arch. OS and arch segments below are literal; only `TAG`
and `VERSION` resolve at install time via `latest.txt`:

| Script OS and arch         | Bootstrap artifact (literal template)                                                          |
| -------------------------- | ---------------------------------------------------------------------------------------------- |
| Linux x86_64 glibc         | `https://cdn.bitty.run/bitty/releases/<TAG>/bitty-<VERSION>-x86_64-unknown-linux-gnu.tar.zst`  |
| Linux aarch64 glibc        | `https://cdn.bitty.run/bitty/releases/<TAG>/bitty-<VERSION>-aarch64-unknown-linux-gnu.tar.zst` |
| Linux x86_64 musl (Alpine) | `https://cdn.bitty.run/bitty/releases/<TAG>/bitty-<VERSION>-x86_64-unknown-linux-musl.tar.zst` |
| macOS arm64                | `https://cdn.bitty.run/bitty/releases/<TAG>/bitty-aarch64-apple-darwin`                        |
| macOS x86_64               | `https://cdn.bitty.run/bitty/releases/<TAG>/bitty-x86_64-apple-darwin`                         |
| Windows x64                | `https://cdn.bitty.run/bitty/releases/<TAG>/bitty-<VERSION>-windows-x86_64.zip`                |
| Windows arm64              | `https://cdn.bitty.run/bitty/releases/<TAG>/bitty-aarch64-pc-windows-msvc.exe`                 |

Each row has a `<artifact>.sha256` sidecar at the same URL with `.sha256`
appended, which the script verifies before installing. Worked example at
`TAG=v0.0.23`, `VERSION=0.0.23`:

```text
https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-0.0.23-x86_64-unknown-linux-gnu.tar.zst
https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-0.0.23-aarch64-unknown-linux-gnu.tar.zst
https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-0.0.23-x86_64-unknown-linux-musl.tar.zst
https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-aarch64-apple-darwin
https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-x86_64-apple-darwin
https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-0.0.23-windows-x86_64.zip
https://cdn.bitty.run/bitty/releases/v0.0.23/bitty-aarch64-pc-windows-msvc.exe
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

### Stable entry points

```sh
curl -fsSL https://cdn.bitty.run/bitty/install/install.sh | bash
```

```powershell
irm https://cdn.bitty.run/bitty/install/install.ps1 | iex
```

Both scripts resolve the version through one pointer:

```sh
curl -fsSL https://cdn.bitty.run/bitty/install/latest.txt
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
