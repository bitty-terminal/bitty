# Packaging

This directory holds multi-distro release artifacts for Bitty `0.0.1`.

## Formats

| Format | File | Distro |
| --- | --- | --- |
| deb | `nfpm.yaml` + `deb` packager | Debian 12, Ubuntu 24.04 |
| rpm | `nfpm.yaml` + `rpm` packager | Fedora 40, RHEL 9, OpenSUSE Tumbleweed/Leap |
| apk | `nfpm.yaml` + `apk` packager | Alpine 3.20 |
| archlinux | `nfpm.yaml` + `archlinux` packager, `PKGBUILD` | Arch, AUR |

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

`PKGBUILD` and `packaging/PKGBUILD` are identical Arch package recipes. CI publishes via `AUR_SSH_PRIVATE_KEY`:

```sh
makepkg --printsrcinfo > .SRCINFO
git push aur@aur.archlinux.org:bitty.git
```

Validation: `bash -n PKGBUILD && makepkg --printsrcinfo`.

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
