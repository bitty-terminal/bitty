# Packaging

This directory holds multi-distro release artifacts for Bitty `0.0.1`.

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

Validation: `bash -n packaging/PKGBUILD && (cd packaging && makepkg --printsrcinfo)`, plus `bash scripts/check-pkgbuild-source.sh` (source recipe installs the artifact declared by `crates/bitty-app/Cargo.toml`), `bash scripts/check-pkgbuild-bin.sh` (template render test) and `bash scripts/check-release-version.sh [--tag vX.Y.Z]` (Cargo version stays aligned with release tags so `bitty --version` matches the tag).

## Homebrew

`packaging/homebrew/bitty.rb` is the single in-repo Homebrew Formula (build-from-source reference). The release workflow regenerates the published formula from release assets in `bitty-terminal/homebrew-tap`. Tested via `brew install --build-from-source packaging/homebrew/bitty.rb && brew test bitty` and `ruby -c`.

## Scoop

`packaging/scoop-bitty.json` is the single in-repo Scoop manifest. The release workflow regenerates the published manifest from release assets in `bitty-terminal/scoop-bucket`. Validated via `python3 -m json.tool` and `checkver`.

## Release Matrix

`.github/workflows/release.yml` builds for:

- linux x64 (`x86_64-unknown-linux-gnu`, ubuntu-latest)
- linux aarch64 (`aarch64-unknown-linux-gnu`, ubuntu-22.04 cross via `aarch64-linux-gnu-gcc`)
- linux x64 musl (`x86_64-unknown-linux-musl`, Alpine 3.22 container, native musl build) — the `.apk`
- windows x64 (`x86_64-pc-windows-msvc`, windows-latest)
- windows aarch64 (`aarch64-pc-windows-msvc`, windows-latest)
- macos x64 (`x86_64-apple-darwin`, macos-14)
- macos aarch64 (`aarch64-apple-darwin`, macos-14)
- macos Universal 2 (`Bitty-<version>-universal.dmg`, `macos-universal` job)

The Universal 2 DMG fuses the two macOS slices into `Bitty.app` with `lipo`
inside `scripts/make-macos-dmg.sh`; the job mounts the DMG and launches both
slices before upload. The app is intentionally unsigned — codesign/notarize is
deferred (034 item 11) — and the bare triple binaries keep shipping.

Plus nfpm packaging for linux x64/aarch64, a runtime-dependency gate (`ldd` + `readelf -d` vs the declared per-distro deps) for the x64 glibc and musl packages, clean-container install smoke jobs for Ubuntu/Fedora/Arch/Alpine, and optional AUR/Homebrew/Scoop bumps gated on secrets.

All packaging keeps bounded contracts: no unbounded file lists, no unsafe, fixed version substitution, scripts are no-ops.
