# openSUSE (Leap and Tumbleweed)

openSUSE uses RPM packages and the zypper package manager. When the OBS
project is published during release, Bitty distributes packages through the
openSUSE Build Service at:

```text
https://download.opensuse.org/repositories/home:/<user>:/bitty/
```

`<user>` is the OBS account that owns the project (see
`packaging/opensuse/README.md` for the project layout and publication steps).
The repository URL is per distribution, for example
`.../home:/<user>:/bitty/openSUSE_Tumbleweed/` and
`.../home:/<user>:/bitty/openSUSE_Leap_15.6/`.

## Support level

openSUSE is **best-effort, not supported** (see `packaging/README.md`
`Support evidence`). No release `verify-install` leg covers openSUSE, so a
package that builds, installs, and runs is not support evidence: the channel
is published, but the release matrix does not prove it. Treat it as a
convenience install, not a verified one.

## User Installation

### Tumbleweed

```sh
sudo zypper addrepo --refresh \
  https://download.opensuse.org/repositories/home:/<user>:/bitty/openSUSE_Tumbleweed/ bitty
sudo zypper --gpg-auto-import-keys refresh bitty
sudo zypper install bitty
```

### Leap 15.6

```sh
sudo zypper addrepo --refresh \
  https://download.opensuse.org/repositories/home:/<user>:/bitty/openSUSE_Leap_15.6/ bitty
sudo zypper --gpg-auto-import-keys refresh bitty
sudo zypper install bitty
```

`--gpg-auto-import-keys` trusts the OBS project signing key on first refresh;
without it zypper prompts to accept the key. Both `bitty` and `bitty-terminal`
are recognized package names (via `Provides: bitty-terminal`).

Leap ships an older Rust than the workspace MSRV (1.85), which is why the Leap
repository is built against `devel:languages:rust`; that only affects building
the package, not installing it.

## Manual RPM

The release assets include an RPM built by `nfpm` that installs on openSUSE as
well. Prefer the zypper repository above; the manual package has no repository
metadata and no zypper-managed upgrades:

```sh
sudo zypper install --allow-unsigned-rpm ./bitty-<version>.rpm
rpm -qip ./bitty-<version>.rpm   # inspect
rpm -ql bitty                    # list installed files
```

Both the repository package and the manual RPM install the binary, the
`.desktop` launcher, AppStream metainfo, the hicolor icon set, and the compiled
terminfo entry (`infocmp -x bitty`). Install one, not both.

## Build from source

The source recipe used by OBS is `packaging/opensuse/bitty.spec`, with the
source service in `packaging/opensuse/_service`; build and submission steps
that need an OBS account are documented in `packaging/opensuse/README.md`.
Building the RPM locally works without an account:

```sh
cd packaging/opensuse
osc service disabledrun        # needs osc; regenerates the source tarball
osc build --local-package openSUSE_Tumbleweed x86_64
```

A plain source build of the workspace (MSRV 1.85, pinned channel in
`rust-toolchain.toml`) needs the development packages the spec lists:
`cargo rust fontconfig-devel freetype2-devel zlib-devel ncurses-utils`.
