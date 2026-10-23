# Gentoo Linux

Gentoo packages software using Portage ebuilds. Bitty ships two ebuilds in the
`bitty` overlay:

- `x11-terms/bitty-bin` — prebuilt release binary, tag-pinned (the reproducible
  package; prefer it).
- `x11-terms/bitty` — live source build that tracks `main` (`bitty-9999`). The
  workspace links its own crates by path, so no versioned source ebuild can
  vendor itself from crates.io; `9999` is the only honest source recipe (see
  "Why there is no versioned source ebuild").

Overlay metadata (`profiles/repo_name`, `metadata/layout.conf`) lives in
`packaging/gentoo/`, **not** at the repository root. Add that directory as the
overlay: adding the bitty repository root produces an overlay Portage rejects
for a missing `profiles/repo_name`.

## Overlay Configuration

### Method 1: point `repos.conf` at the in-tree overlay (works from this tree)

Clone the repository and register its `packaging/gentoo` subdirectory as the
overlay location. This is the supported method for testing the recipes in this
repository.

```sh
git clone https://github.com/bitty-terminal/bitty.git /path/to/src/bitty
```

Create `/etc/portage/repos.conf/bitty.conf`:

```ini
[bitty]
location = /path/to/src/bitty/packaging/gentoo
auto-sync = no
```

Refresh the overlay after pulling:

```sh
git -C /path/to/src/bitty pull --ff-only
emaint sync -r bitty
```

### Method 2: a dedicated overlay repository, added with `eselect repository`

`eselect repository add` (and any `sync-uri`) needs a git repository whose
**root** is the overlay. Split the metadata-complete subtree out of this
repository and add the resulting repository:

```sh
git clone https://github.com/bitty-terminal/bitty.git /path/to/src/bitty
git -C /path/to/src/bitty subtree split --prefix=packaging/gentoo -b bitty-overlay
git clone --branch bitty-overlay /path/to/src/bitty /path/to/src/bitty-overlay

eselect repository add bitty git file:///path/to/src/bitty-overlay
emaint sync -r bitty
```

`bitty-overlay` has `profiles/repo_name`, `metadata/layout.conf`, and the
`x11-terms/` categories at its root, so Portage accepts it. To share it, push
that branch to any git host and pass that URL to `eselect repository add`; a
repository whose root is the bitty source tree is not a valid overlay.

## Installation

### Prebuilt Binary (`x11-terms/bitty-bin`)

Recommended: no local Rust build.

```sh
emerge --ask x11-terms/bitty-bin
```

Downloads the verified binary bundle from `https://cdn.bitty.run/releases/v${PV}/`
and installs the binary, desktop file, icon set, and compiled terminfo database.
This channel resolves its sources and checksums **at tag time**: the release
archive only exists once the release tag is pushed, so the ebuild is bumped in
the release PR, not ahead of it.

### Source Build (`x11-terms/bitty`, live `9999`)

```sh
emerge --ask =x11-terms/bitty-9999
```

`git-r3` checks out `main`, `cargo_live_src_unpack` vendors the dependency
graph, and `cargo_src_compile` builds `-p bitty-terminal --bin bitty`. Needs a
Rust toolchain (`virtual/rust`) plus `tic`, which ships in `sys-libs/ncurses`
(there is no `sys-apps/tic` package). A live ebuild is never keyworded and is
not a reproducible package: use it to track development, not for a
bit-reproducible install.

## Runtime dependencies

`RDEPEND` declares only the direct `NEEDED` sonames from
`packaging/linux/runtime-deps.toml`: `media-libs/fontconfig` and
`media-libs/freetype`. `libgcc_s.so.1` comes from the `sys-devel/gcc` in
`@system`, `libc`/`libm` from the base system. The X11 (`x11-libs/libX11`,
`x11-libs/libxkbcommon`), Wayland (`dev-libs/wayland`), and Vulkan
(`media-libs/vulkan-loader`) backends are `dlopen()`ed at runtime, so they are
not hard dependencies — install the ones your session uses.

## Why there is no versioned source ebuild

`cargo.eclass` expands `CRATES` into `CARGO_CRATE_URIS` for crates.io
dependencies. Bitty's workspace links its own crates by path, so that list
cannot be generated; an empty `CRATES` sends `src_unpack` into
`cargo_live_src_unpack`, which refuses any `PV` other than `9999`. A
`bitty-<version>.ebuild` would therefore be a recipe that cannot merge. The
versioned channel is `bitty-bin`; source builds come from the live ebuild.
