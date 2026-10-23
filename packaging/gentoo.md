# Gentoo Linux

Gentoo packages software using Portage ebuilds. Bitty provides official ebuilds
for both source compilation (`x11-terms/bitty`) and prebuilt binary installation
(`x11-terms/bitty-bin`).

The packaging files are located under `packaging/gentoo/` and form a valid
standalone Portage overlay repository.

## Overlay Configuration

### Method 1: Local repository via `/etc/portage/repos.conf`

Create `/etc/portage/repos.conf/bitty.conf`:

```ini
[bitty]
location = /var/db/repos/bitty
sync-type = git
sync-uri = https://github.com/bitty-terminal/bitty.git
sync-depth = 1
auto-sync = yes
```

Alternatively, link the cloned `packaging/gentoo` directory directly into
`/etc/portage/repos.conf/bitty.conf`:

```ini
[bitty]
location = /path/to/bitty/packaging/gentoo
auto-sync = no
```

### Method 2: Using `eselect repository`

```sh
eselect repository add bitty git https://github.com/bitty-terminal/bitty.git
emaint sync -r bitty
```

## Installation

### Prebuilt Binary (`x11-terms/bitty-bin`)

Recommended for fast installation without compiling Rust code locally:

```sh
emerge --ask x11-terms/bitty-bin
```

Downloads the verified binary bundle from `https://cdn.bitty.run/releases/v${PV}/`
and installs the binary, desktop file, icon set, and compiled terminfo database.

### Source Build (`x11-terms/bitty`)

Builds directly from source using Gentoo's standard `cargo.eclass`:

```sh
emerge --ask x11-terms/bitty
```

Requires a functioning Rust compiler and `tic` (ncurses).
