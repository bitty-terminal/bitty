# NetBSD (pkgsrc)

NetBSD packages third-party software with the cross-platform `pkgsrc` framework
(`pkgin` / `pkg_add` as the binary package managers). Bitty ships a pkgsrc recipe
at `packaging/netbsd/x11/bitty/` (`Makefile`, `PLIST`, `DESCR`, `distinfo`)
so users on NetBSD, SmartOS, macOS, and Linux can build and install the terminal
from source with pkgsrc.

## Support level

**Best-effort, not supported.** Per `packaging/README.md`, support evidence is
a passing `verify-install` leg or a tag-time publish that completed.
Concretely, for this recipe:

- The `Makefile`, `PLIST`, and `DESCR` are maintained in-tree and gated
  by `just distro-recipes-test` (`scripts/tests/check-distro-recipes.test.sh`),
  which checks structure, plist/do-install agreement, and absence of host
  paths.
- No release job builds or installs this package, and `distinfo` checksums are
  computed at tag time via `bmake makesum`.
- If building the package is not desired, the prebuilt standalone release tarballs
  from GitHub Releases or CDN remain available.

## What the package installs

| Path                                          | Content                                                         |
| --------------------------------------------- | --------------------------------------------------------------- |
| `bin/bitty`                                   | the terminal binary                                             |
| `share/applications/run.bitty.Bitty.desktop`  | desktop launcher                                                |
| `share/metainfo/run.bitty.Bitty.metainfo.xml` | AppStream metadata                                              |
| `share/icons/hicolor/*/apps/bitty.{png,svg}`  | icon theme entries (16–512 px + scalable)                       |
| `share/terminfo/b/bitty`                      | terminfo entry, compiled from `terminfo/bitty.ti` with `tic -x` |
| `share/doc/bitty/{README,CHANGELOG}.md`       | bundled documentation                                           |

Runtime libraries: the package links fontconfig and freetype2 via pkgsrc
`buildlink3.mk`. X11, Wayland, and Vulkan backends are loaded at runtime
with `dlopen`.

## Building with pkgsrc

The pkgsrc framework requires building inside a pkgsrc tree:

```sh
# 1. Copy the package into the pkgsrc tree:
sudo cp -R packaging/netbsd/x11/bitty /usr/pkgsrc/x11/bitty
```

```sh
# 2. Build and install with bmake:
cd /usr/pkgsrc/x11/bitty
bmake makesum     # once distfiles are reachable
bmake install clean
```

Staged package check:

```sh
cd /usr/pkgsrc/x11/bitty
bmake package
bmake print-PLIST
```

## Regenerating `distinfo` at tag time

`packaging/netbsd/x11/bitty/distinfo` carries placeholder documentation.
Run `bmake makesum` once the release tag is pushed:

```sh
cd /usr/pkgsrc/x11/bitty
bmake makesum
cp distinfo packaging/netbsd/x11/bitty/distinfo
```
