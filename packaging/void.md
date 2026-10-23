# Void Linux

Void Linux uses the XBPS package manager and the `xbps-src` source packages
collection (`void-packages`).

An official template is maintained at `packaging/void/srcpkgs/bitty/template`.

## Building with `xbps-src`

### 1. Setup void-packages repository

```sh
git clone --depth=1 https://github.com/void-linux/void-packages.git
cd void-packages
./xbps-src binary-bootstrap
```

### 2. Copy the Bitty template

```sh
mkdir -p srcpkgs/bitty
cp /path/to/bitty/packaging/void/srcpkgs/bitty/template srcpkgs/bitty/template
```

### 3. Generate checksum and build

```sh
# Generate authentic sha256 checksum for the release archive
./xbps-src xgensum -a bitty

# Build the package
./xbps-src pkg bitty
```

### 4. Install the package

```sh
xi bitty # or xbps-install --repository hostdir/binpkgs bitty
```

## Features

- Supports both `glibc` and `musl` libc variants across `x86_64` and `aarch64`.
- Uses Void's `build_style=cargo` to build and package `crates/bitty-terminal`.
- Automatically installs desktop entry (`vdesk`), AppStream metadata (`vinstall`),
  Hicolor icons, and compiles the terminfo database (`tic -x`).
