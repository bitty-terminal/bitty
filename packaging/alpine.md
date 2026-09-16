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
- The `verify-alpine` job installs the package into a clean Alpine container
  and runs `bitty --version`.

Runtime library dependencies are declared per distribution in a later item
(034 item 4); the install check installs the font stack explicitly.

The apk can still be produced locally from a musl binary with the single
`nfpm.yaml` source:

```sh
nfpm package --config nfpm.yaml --packager apk --target /tmp/bitty.apk
```

Validation on an Alpine container:

```sh
readelf -l target/release/bitty | grep ld-musl   # musl interpreter, not ld-linux
apk add --allow-untrusted ./dist/bitty-*.apk
bitty --version
```

Alpine 3.22 is the pinned build image; no scripts with unbounded work, and the
`apk` packager uses no-op scripts.
