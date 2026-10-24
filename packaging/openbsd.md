# OpenBSD (Ports)

OpenBSD packages third-party software with the OpenBSD Ports collection
(`pkg_add` as the binary package manager). Bitty ships a ports recipe at
`packaging/openbsd/x11/bitty/` (`Makefile`, `pkg/PLIST`, `pkg/DESCR`,
`distinfo`) so a user can build and install the terminal from source with the
system toolchain.

## Support level

**Best-effort, not supported.** Per `packaging/README.md`, support evidence is
a passing `verify-install` leg or a tag-time publish that completed.
Concretely, for this recipe:

- The `Makefile`, `pkg/PLIST`, and `pkg/DESCR` are maintained in-tree and gated
  by `just distro-recipes-test` (`scripts/tests/check-distro-recipes.test.sh`),
  which checks structure, plist/do-install agreement, and absence of host
  paths.
- No release job builds or installs this port, and `distinfo` checksums are
  computed at tag time via `make makesum`.
- If building the port is not desired, the prebuilt tarballs from GitHub Releases
  or CDN remain available.

## What the port installs

| Path                                          | Content                                                         |
| --------------------------------------------- | --------------------------------------------------------------- |
| `bin/bitty`                                   | the terminal binary                                             |
| `share/applications/run.bitty.Bitty.desktop`  | desktop launcher                                                |
| `share/metainfo/run.bitty.Bitty.metainfo.xml` | AppStream metadata                                              |
| `share/icons/hicolor/*/apps/bitty.{png,svg}`  | icon theme entries (16–512 px + scalable)                       |
| `share/terminfo/b/bitty`                      | terminfo entry, compiled from `terminfo/bitty.ti` with `tic -x` |
| `share/doc/bitty/{README,CHANGELOG}.md`       | bundled documentation                                           |

Runtime libraries: the port declares the binary's direct `NEEDED` libraries
only — fontconfig and freetype2. X11, Wayland, and Vulkan backends are loaded
at runtime with `dlopen` and come from the desktop stack.

## Building from the ports tree

The OpenBSD Ports framework requires building inside a ports tree:

```sh
# 1. Copy the port into the ports tree:
doas cp -R packaging/openbsd/x11/bitty /usr/ports/x11/bitty
```

```sh
# 2. Build and install (as root / with doas):
cd /usr/ports/x11/bitty
doas make makesum     # once distfiles are reachable
doas make install clean
```

Staged package check:

```sh
cd /usr/ports/x11/bitty
make fake
make update-plist
```

## Regenerating `distinfo` at tag time

`packaging/openbsd/x11/bitty/distinfo` carries placeholder documentation.
Run `make makesum` once the release tag is pushed:

```sh
cd /usr/ports/x11/bitty
make makesum
cp distinfo packaging/openbsd/x11/bitty/distinfo
```
