# Fedora and EPEL (Copr)

Fedora and Enterprise Linux (RHEL, Rocky Linux, AlmaLinux, CentOS Stream) use
RPM packages. When Copr publishing is enabled during release, Bitty distributes
official RPMs through Fedora Copr at:

```text
https://copr.fedorainfracloud.org/coprs/xuepoo/bitty-terminal/
```

## User Installation

### Fedora

Enable the Copr repository and install:

```sh
sudo dnf copr enable -y xuepoo/bitty-terminal
sudo dnf install -y bitty
```

### RHEL / Rocky Linux / AlmaLinux / CentOS Stream

Install EPEL and `dnf-plugins-core` first, then enable the repository:

```sh
sudo dnf install -y epel-release dnf-plugins-core
sudo dnf copr enable -y xuepoo/bitty-terminal
sudo dnf install -y bitty # or sudo yum install -y bitty
```

Both `bitty` and `bitty-terminal` are recognized package names (via `Provides: bitty-terminal`).

## Build and Continuous Delivery

The release workflow (`.github/workflows/release.yml`) builds `.rpm` packages
via `nfpm` for both `x86_64` and `aarch64`. Once built and verified through
the install smoke gate (`scripts/install-smoke.sh --distro fedora`), the
`copr` job builds a source RPM from `packaging/bitty.spec` and the published
`.tar.zst` bundles, then submits it with `copr-cli build` to every enabled
x86_64/aarch64 chroot under `environment: release`. Direct RPM upload is not
used because this Copr instance disables it. `copr-cli` is installed into a
per-run Python virtual environment (`python3 -m venv`), and `rpmbuild`/`zstd`
are installed on the runner:

```sh
copr-cli build -r <chroot> ... xuepoo/bitty-terminal <path-to-srpm>
```

## Enabled chroots

Copr receives a single SRPM and rebuilds it per enabled chroot. As of the
`v0.0.31` release the project enabled Fedora (43/44/45/rawhide), EPEL 9/10,
Amazon Linux 2023 and openEuler 24.03, each for `x86_64` and `aarch64`. That
list belongs to the Copr project rather than to this repository, so read it as
a snapshot and confirm with `copr-cli get-project` before promising a target.

`v0.0.31` is missing only `openeuler-24.03-aarch64`: the original task hit the
1800 s `copr-cli --timeout` while `mock` was still installing the buildroot
from a slow `repo.openeuler.org` mirror (1803.6 s total), so the SRPM was never
unpacked. The other 15 chroots built the identical input. The build timeout is
now parameterized (`COPR_BUILD_TIMEOUT` repo variable for `release.yml`,
`copr-build-timeout` dispatch input for `repair-release-channels.yml`), default
10800 s; resubmit the chroot with a raised timeout, or disable the target
deliberately; never re-cut the tag for it.

A standalone RPM spec file is also maintained at `packaging/bitty.spec` for
manual inspection and downstream packaging.
