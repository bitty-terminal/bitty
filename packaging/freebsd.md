# FreeBSD (Ports)

FreeBSD packages third-party software with the Ports framework (`pkg` as the
binary package manager). Bitty ships a ports recipe at
`packaging/freebsd/x11/bitty/` (`Makefile`, `pkg-plist`, `pkg-descr`,
`distinfo`) so a user can build and install the terminal from source with the
system toolchain.

## Support level

**Best-effort, not supported.** Per `packaging/README.md`, support evidence is
a passing `verify-install` leg or a tag-time publish that completed; the
`build-freebsd` job builds an `x86_64-unknown-freebsd` binary and runs the core
headless suites, but nothing installs a `.pkg` in a clean FreeBSD environment.
Concretely, for this recipe:

- The Makefile, `pkg-plist`, and `pkg-descr` are maintained in-tree and gated
  by `just distro-recipes-test` (`scripts/tests/check-distro-recipes.test.sh`),
  which checks structure, plist/do-install agreement, and absence of host
  paths. That gate cannot run the FreeBSD build.
- No release job builds or installs this port, and no checksum is published
  for it: `distinfo` is still empty by design (see below).
- If a build or install fails, the prebuilt
  `bitty-<version>-x86_64-unknown-freebsd.tar.xz` from
  `https://cdn.bitty.run/releases/v<version>/` (the artifact `scripts/install.sh`
  points FreeBSD users at) remains the supported download path.

## What the port installs

| Path | Content |
| ---- | ------- |
| `bin/bitty` | the terminal binary |
| `share/applications/run.bitty.Bitty.desktop` | desktop launcher |
| `share/metainfo/run.bitty.Bitty.metainfo.xml` | AppStream metadata |
| `share/icons/hicolor/*/apps/bitty.{png,svg}` | icon theme entries (16–512 px + scalable) |
| `share/terminfo/b/bitty` | terminfo entry, compiled from `terminfo/bitty.ti` with `tic -x` |
| `share/doc/bitty/{README,CHANGELOG}.md` | bundled documentation |

Runtime libraries: the port declares the binary's direct `NEEDED` libraries
only — fontconfig and freetype2, the same set as
`packaging/linux/runtime-deps.toml` and the apk. X11, Wayland, and Vulkan are
loaded at runtime with `dlopen` and come from the desktop stack, so they are
not hard dependencies.

## Building from the ports tree

The Ports framework does not build from an arbitrary source checkout: the port
directory has to live inside a ports tree.

```sh
# 1. Put the port in a tree. Either the system tree:
sudo cp -R packaging/freebsd/x11/bitty /usr/ports/x11/bitty

#    ...or a separate overlay tree, which leaves /usr/ports untouched:
mkdir -p ~/ports/x11
cp -R packaging/freebsd/x11/bitty ~/ports/x11/
export PORTSDIR=$HOME/ports     # only for the overlay variant
```

```sh
# 2. Build and install (as root for the install step).
cd "${PORTSDIR:-/usr/ports}/x11/bitty"
make makesum            # only needed while distinfo is empty; see below
make install clean
```

Staged build without installing, plus the plist check:

```sh
cd "${PORTSDIR:-/usr/ports}/x11/bitty"
make stage
make check-plist        # every installed file must appear in pkg-plist
make deinstall install  # or: pkg install work/pkg/bitty-<version>.pkg
```

Useful knobs:

- `make cargo-crates` re-derives the `CARGO_CRATES` list from `Cargo.lock`;
  portfmt's `make cargo-crates-merge` writes it in place.
- `make -DDISABLE_LICENSES` skips the license prompt in scripted builds; the
  port declares `LICENSE= MIT APACHE20` (`dual`, matching `Cargo.toml`,
  `nfpm.yaml`, and `packaging/PKGBUILD`).
- The port builds with `USES=cargo`, so crates are vendored from the distfiles
  and the build runs without network access. The port framework supplies
  `RUSTFLAGS`, stripping, and `--release`; do not pass `--locked` (the
  framework rewrites `Cargo.lock` for the vendored source).
- Only `amd64` is enabled (`ONLY_FOR_ARCHS= amd64`): release CI builds and
  tests the FreeBSD target there only. aarch64 may compile but has no coverage,
  so it stays excluded rather than silently claiming support.

## Regenerating `distinfo` at tag time

`packaging/freebsd/x11/bitty/distinfo` carries only a comment. It cannot hold
real checksums before the `v0.0.29` tag exists, because `make makesum` hashes
the published tarball and every vendored crate. Run it as part of the release:

```sh
# once the tag is pushed and the tarball is reachable
cd packaging/freebsd/x11/bitty
make makesum                              # writes SHA256 + SIZE entries
git diff -- distinfo                      # review: tarball + one line per crate
```

`make makesum` needs `DISTDIR` writable (default `/usr/ports/distfiles`, or
`$PORTSDIR/distfiles` for an overlay) and network access to
`github.com`/`crates.io`. If `Cargo.lock` changed since the last release, run
`make cargo-crates` first and commit the refreshed `CARGO_CRATES` block
together with the new `distinfo`; the two must not drift, or `make checksum`
fails closed.

Until `make makesum` runs, `make fetch` and `make checksum` fail with missing
distinfo entries. That failure is intentional: a checksum written by hand
would not describe the tag content.

## Not covered

- No FreeBSD package repository, no `pkg install bitty` from a Bitty-run repo,
  and no submission to the FreeBSD ports tree yet; this directory is the
  upstream recipe a maintainer would start from.
- No aarch64 package, no install smoke, and no published checksum for the port.
- `scripts/install.sh` does not bootstrap FreeBSD (see the release matrix in
  `packaging/README.md`); FreeBSD users either build this port or unpack the
  published `.tar.xz` by hand.
