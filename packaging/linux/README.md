# Linux packaging metadata

Runtime dependency data for the Linux package formats lives here.

## Layout

034 sketched `packaging/linux/{common,deb,rpm,alpine,arch}/` with one nfpm
config per distro. nfpm has no per-distro config files, so the documented
minimal equivalent is used instead:

- `nfpm.yaml` (repo root) stays the single builder config; per-format runtime
  dependencies are declared in its `overrides.<format>.depends` lists.
- `runtime-deps.toml` (this directory) records which package provides each ELF
  soname the release binary links, per nfpm format.

The declarations and the mapping must agree with the linked binary;
`scripts/check-runtime-deps.sh` fails the package when they do not.

## runtime-deps.toml

Each `[sonames."<name>"]` table maps one direct `NEEDED` soname to the
providing package for `deb`, `rpm`, `apk`, and `archlinux`:

- a package name means the format must declare exactly that name (the gate
  fails on a missing declaration and on a declaration without link evidence);
- `base` means the distro base system provides the library (glibc, musl,
  `ld.so`) and it must not be declared;
- an absent key means the soname must not appear in that format's binary.

## Gate

`scripts/check-runtime-deps.sh` reads the direct `NEEDED` sonames with
`readelf -d`, verifies them with `ldd`, maps them through `runtime-deps.toml`,
reads the declared names from `nfpm.yaml` `overrides.<format>.depends`, and
fails on any divergence (unmapped soname, linked but undeclared, declared
without link evidence, or a format with no declarations).

Run it locally after building:

```sh
cargo build --locked -p bitty-app
bash scripts/check-runtime-deps.sh --binary target/debug/bitty --packagers deb,rpm,archlinux
# or: just runtime-deps target/debug/bitty deb,rpm,archlinux
```

CI runs the gate against the debug binary in the Quality gates job. The release
workflow runs it against the built x86_64 glibc binary (`build`, formats
`deb,rpm,archlinux`) and the native musl binary (`build-alpine`, format `apk`).
The gate logic is pinned by `scripts/tests/check-runtime-deps.test.sh`.

## Changing a dependency

1. Change the linked library (Rust side).
2. Update `runtime-deps.toml` for every affected format.
3. Update the matching `overrides.<format>.depends` list in `nfpm.yaml`.
4. Run the gate for the affected target; the package is not releasable until
   the mapping, the declarations, and `readelf -d`/`ldd` agree.

No Wayland/X11/wgpu dependency may be declared without an actual `NEEDED`
entry: the gate rejects declarations with no link evidence.

## Scope

The gate covers the x86_64 glibc packages (`deb`, `rpm`, `archlinux`) and the
musl apk from `build-alpine`. aarch64 deb/rpm stay on the tolerant MVP path
from #819 and are gated when 034 item 8 hardens them; the cross build's
linkage can differ per architecture (the aarch64 binary currently does not
link `libfreetype.so.6` directly), so per-arch verification lands with that
item.
