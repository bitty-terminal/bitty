# Alpine

Alpine uses apk. The `.apk` published by the release workflow is built from a
native musl binary, not repackaged from the glibc build (CTX-0447):

- The `build-alpine` job runs in an `alpine:3.22` container with `build-base`,
  `pkgconf`, `fontconfig-dev`, and `freetype-dev`, installs the pinned Rust
  toolchain, and builds `x86_64-unknown-linux-musl` natively.
- `RUSTFLAGS="-C target-feature=-crt-static"` produces a dynamically linked
  musl binary. This links fontconfig/FreeType against Alpine's system libraries
  and keeps runtime `dlopen` (Vulkan/X11/Wayland) working; a fully static musl
  binary cannot `dlopen`.
- `nfpm package --packager apk` then packages that binary into
  `dist/bitty-x86_64-unknown-linux-musl.apk`.
- The `verify-install` Alpine leg installs the package into a clean Alpine
  container and runs `bitty --version`, `bitty doctor`, and the headless smoke
  (`bitty --headless`), the same sequence as the Ubuntu/Fedora/Arch legs
  (034 item 5 / CTX-0450).

Runtime library dependencies are declared per distribution (034 item 4): the
apk declares `fontconfig`, `freetype`, and `libgcc` via the `overrides.apk`
block in `nfpm.yaml`, and `scripts/check-runtime-deps.sh` fails the package
when those names diverge from the musl binary's `ldd`/`readelf -d` output.
`scripts/install-smoke.sh` still installs the font stack explicitly, so the
smoke stays green independently of the declared dependencies until the legs
resolve them from the package metadata.

The apk can still be produced locally from a musl binary with the single
`nfpm.yaml` source:

```sh
nfpm package --config nfpm.yaml --packager apk --target /tmp/bitty.apk
```

Validation on an Alpine container:

```sh
readelf -l target/release/bitty | grep ld-musl   # musl interpreter, not ld-linux
scripts/install-smoke.sh --distro alpine --package dist/bitty-x86_64-unknown-linux-musl.apk
```

Alpine 3.22 is the pinned build image; no scripts with unbounded work, and the
`apk` packager uses no-op scripts.

## Upstream aports template

For building from source via Alpine's official packaging system (`abuild`), an
`APKBUILD` template is provided at `packaging/alpine/APKBUILD`:

```sh
# Setup Alpine build environment
apk add alpine-sdk cargo-auditable rust

# Build the package from source
cd packaging/alpine
abuild checksum
abuild -r
```

The template adheres to Alpine aports packaging standards:

- Sets `options="net"` for the `prepare()` phase to execute `cargo fetch --locked`.
- Compiles with `cargo auditable build --release --locked -p bitty-terminal` and
  `RUSTFLAGS="-C target-feature=-crt-static"` for shared musl linkage.
- Installs the binary, AppStream metadata, `.desktop` launcher, Hicolor icon
  hierarchy, and compiled terminfo database (`tic -x`).
- Splits the documentation into `bitty-doc` through an explicit `doc()`
  function, so `abuild validate` finds a split function for every declared
  subpackage.
- Declares `license="MIT OR Apache-2.0"`, matching `Cargo.toml`, the AUR
  recipes, the RPM spec, and `nfpm.yaml`.

## Release timing

Source and checksum steps run **at tag time**. `abuild checksum` fetches
`https://github.com/bitty-terminal/bitty/archive/refs/tags/v$pkgver.tar.gz`,
which does not exist until the release tag is pushed, so `pkgver` is bumped in
the release PR and the checksum is committed with that tag. Running
`abuild checksum` against an unreleased `pkgver` fails closed on the 404
instead of pinning a hash for a tarball that may never be published.

