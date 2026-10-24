# Solus Linux

Solus Linux uses the `eopkg` package manager and the `ypkg` build tool
from the official `solus-project/packages` repository.

An official package recipe is maintained at `packaging/solus/package.yml`.

## Building with `ypkg`

### 1. Setup Solus packaging environment

Ensure `solbuild` and `ypkg` are installed on your Solus system:

```sh
sudo eopkg install solbuild ypkg
sudo solbuild init -u
```

### 2. Copy the Bitty recipe

```sh
mkdir -p bitty
cp /path/to/bitty/packaging/solus/package.yml bitty/package.yml
cd bitty
```

### 3. Generate checksum and build

Source and checksum steps run **at tag time**: the recipe ships with
`SKIP` because its distfile is the GitHub tag archive, which only exists once
the release tag is pushed.

```sh
# Calculate sha256 checksum for the release archive
curl -fsSLO https://github.com/bitty-terminal/bitty/archive/refs/tags/v0.0.28.tar.gz
sha256sum v0.0.28.tar.gz

# Replace SKIP in package.yml with the sha256 digest:
sed -i 's/SKIP/<computed-sha256>/' package.yml

# Build the package inside a clean solbuild chroot
sudo solbuild build package.yml
```

A recipe submitted to upstream `solus-project/packages` must carry the real
sha256 digest, never `SKIP`; the in-tree copy is a draft until then.

### 4. Install the package

```sh
sudo eopkg install bitty-0.0.28-1-1-x86_64.eopkg
```

## Features

- Uses Solus `%cargo_build` and `%cargo_install` macros targeting `bitty-terminal`.
- Automatically installs desktop entry (`run.bitty.Bitty.desktop`), AppStream
  metadata (`run.bitty.Bitty.metainfo.xml`), and Hicolor icons (16px to 512px + SVG).
- Compiles the terminfo database into `/usr/share/terminfo` using `tic -x`.
