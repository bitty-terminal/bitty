# Packaging

This directory holds multi-distro release artifacts for Bitty `0.0.1`.

## Formats

| Format    | File                                           | Distro                                      |
| --------- | ---------------------------------------------- | ------------------------------------------- |
| deb       | `nfpm.yaml` + `deb` packager                   | Debian 12, Ubuntu 24.04                     |
| rpm       | `nfpm.yaml` + `rpm` packager                   | Fedora 40, RHEL 9, OpenSUSE Tumbleweed/Leap |
| apk       | `nfpm.yaml` + `apk` packager                   | Alpine 3.20                                 |
| archlinux | `nfpm.yaml` + `archlinux` packager, `PKGBUILD` | Arch, AUR                                   |

OpenSUSE is rpm-based; the rpm built via nfpm is tested with `rpm -qip` and installs via `zypper install`. Alpine is apk-based; apk is validated via `apk info --allow-untrusted -X`.

## Nfpm

Single source `nfpm.yaml` at repo root covers deb/rpm/apk/archlinux via `nfpm package --packager <type>`. Validation:

```sh
nfpm package --config nfpm.yaml --packager deb --target /tmp/bitty.deb
nfpm package --config nfpm.yaml --packager rpm --target /tmp/bitty.rpm
nfpm package --config nfpm.yaml --packager apk --target /tmp/bitty.apk
nfpm package --config nfpm.yaml --packager archlinux --target /tmp/bitty.pkg.tar.zst
```

Scripts under `packaging/scripts/` are bounded no-ops (exit 0) to keep package hooks honest.

## Nix Flake

`flake.nix` provides `packages.default` via `crane` + `rust-overlay` at `1.97.1`, filtered source bounded, no unsafe. Check:

```sh
nix flake check
nix build .#bitty
```

## AUR

`PKGBUILD` and `packaging/PKGBUILD` are identical Arch source-package recipes. `packaging/PKGBUILD.bin` is the `bitty-bin` template: prebuilt `bitty-x86_64-unknown-linux-gnu` release binary with a real sha256 (never `SKIP` for the binary), `arch=('x86_64')` only, `provides=('bitty')`, `conflicts=('bitty' 'bitty-nightly' 'bitty-git')`. CI publishes both via `AUR_SSH_PRIVATE_KEY` (`aur` job for `bitty`, `aur-bin` job for `bitty-bin`, gated on `vars.AUR_PUBLISH` / `vars.AUR_BIN_PUBLISH`):

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

Validation: `bash -n PKGBUILD && makepkg --printsrcinfo`, plus `bash scripts/check-pkgbuild-source.sh` (source recipe installs the artifact declared by `crates/bitty-app/Cargo.toml`, and root `PKGBUILD`/`packaging/PKGBUILD` stay identical), `bash scripts/check-pkgbuild-bin.sh` (template render test) and `bash scripts/check-release-version.sh [--tag vX.Y.Z]` (Cargo version stays aligned with release tags so `bitty --version` matches the tag).

## Homebrew

`Formula/bitty.rb` (mirrored at `homebrew/Formula/bitty.rb`) is the Homebrew Formula. Tested via `brew install --build-from-source Formula/bitty.rb && brew test bitty` and `ruby -c`.

## Scoop

`bucket/bitty.json` is the Scoop manifest (also mirrored as `packaging/scoop-bitty.json`). Validated via `python3 -m json.tool` and `checkver`.

## Release Matrix

`.github/workflows/release.yml` builds for:

- linux x64 (`x86_64-unknown-linux-gnu`, ubuntu-latest)
- linux aarch64 (`aarch64-unknown-linux-gnu`, ubuntu-latest cross via `aarch64-linux-gnu-gcc`)
- windows x64 (`x86_64-pc-windows-msvc`, windows-latest)
- windows aarch64 (`aarch64-pc-windows-msvc`, windows-latest)
- macos x64 (`x86_64-apple-darwin`, macos-14)
- macos aarch64 (`aarch64-apple-darwin`, macos-14)

Plus nfpm packaging for linux x64/aarch64 and optional AUR/Homebrew/Scoop bumps gated on secrets.

All packaging keeps bounded contracts: no unbounded file lists, no unsafe, fixed version substitution, scripts are no-ops.
