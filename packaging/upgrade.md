# Upgrading Bitty

Part of: bitty-terminal/bitty#2925 | Task: CTX-1719 | Milestone: v0.1.0

`bitty upgrade` removes the re-run-the-installer step for self-managed
installs and delegates everything else to the channel that owns the binary.
It never elevates, never fights a package manager, and never writes outside
the detected install directory.

```sh
bitty upgrade --check      # read-only: current vs published release
bitty upgrade --dry-run    # print the plan for the detected channel
bitty upgrade              # script/AppImage: replace atomically
bitty upgrade --rollback   # script/AppImage: restore bitty.prev
```

## Exit codes

| Code | Meaning                                                                                                                     |
| ---- | --------------------------------------------------------------------------------------------------------------------------- |
| `0`  | Done, already current, or a plan/command was printed                                                                        |
| `1`  | `--check` found an update, or a fetch/verify/replace failed, or the channel is unknown                                      |
| `2`  | Usage error (contradictory flags, stray positional)                                                                         |
| `7`  | Permission class: system-owned channel (without `--dry-run`), safe-mode write refusal, or a Windows self-managed install that needs the installer |

`bitty upgrade --check` is read-only and allowed under `--safe`; `--dry-run`
is also allowed. Any path that would download, replace, or execute is refused
under `--safe` with exit `7`. A `--dry-run` never exits `7`: it prints the
plan for whatever channel was detected and stops.

## Detection

Detection is read-only and ordered; the first match wins.

| Channel    | Evidence                                                                     | Scope  | What `bitty upgrade` does                                           |
| ---------- | ---------------------------------------------------------------------------- | ------ | ------------------------------------------------------------------- |
| `script`   | install marker `channel = "script"` whose `bin_dir` holds the running binary | user   | downloads, verifies, replaces atomically                            |
| `appimage` | `$APPIMAGE` names the running executable                                     | user   | same, replacing the AppImage file                                   |
| `homebrew` | binary under the Homebrew cellar and `brew` on `PATH`                        | user   | prints `brew upgrade bitty`; runs it only with `--yes`              |
| `scoop`    | binary under `%SCOOP%\apps\bitty` and `scoop` on `PATH`                      | user   | prints `scoop update bitty`; runs it only with `--yes`              |
| `aur`      | `pacman -Qo` owns the binary, `pacman -Si` cannot find the package           | user   | prints `yay`/`paru -S --noconfirm bitty`; runs it only with `--yes` |
| `nix`      | binary under the Nix store                                                   | system | prints `nix profile upgrade bitty`, exit `7`                        |
| `apt`      | `dpkg-query -S` owns the binary                                              | system | prints `apt-get ... --only-upgrade bitty`, exit `7`                 |
| `dnf`      | `rpm -qf` owns the binary and `dnf` is present                               | system | prints `dnf upgrade bitty`, exit `7`                                |
| `rpm`      | `rpm -qf` owns the binary without `dnf`                                      | system | prints the `rpm -U` command, exit `7`                               |
| `pacman`   | `pacman -Qo` owns the binary and `pacman -Si` finds the package              | system | prints `pacman -Syu bitty`, exit `7`                                |
| `unknown`  | none of the above                                                            | —      | prints manual commands and this page, exit `1`                      |

System-owned channels only print the exact command: `bitty` never runs `sudo`
and never shells into a package manager on your behalf.

## Per-channel upgrade

### Install script (Linux, macOS)

```sh
bitty upgrade --check      # exit 0 when current, 1 when an update exists
bitty upgrade --dry-run    # asset URL, checksum source, backup path
bitty upgrade              # download, verify, replace
```

The script install is recognised through its marker file:

- `$XDG_DATA_HOME/bitty/install.json` (fallback `~/.local/share/bitty/install.json`)
- `%LOCALAPPDATA%\bitty\install.json` on Windows

The marker is written by `install.sh` and `install.ps1` after a successful
install and refreshed by every successful `bitty upgrade`:

```json
{
  "schema": 1,
  "channel": "script",
  "prefix": "<install prefix>",
  "bin_dir": "<install prefix>/bin",
  "version": "0.0.33",
  "binary_sha256": "<64 hex>",
  "installed_at": "2026-11-06T12:00:00Z"
}
```

The update downloads the bare per-target release asset (the same bytes the
Linux bundle's `bin/bitty` carries), verifies it against the release
`SHA256SUMS`, stages it inside the install directory, and `rename(2)`s it over
the running binary. The previous binary stays as `bitty.prev`.

### Windows installer

A running `bitty.exe` cannot be replaced in place, so the Windows script
channel prints the installer command and exits `7` instead of pretending to
upgrade:

```powershell
irm https://cdn.bitty.run/install/install.ps1 | iex
```

Re-run it in a new terminal. In-place Windows replacement is deferred.

### AppImage

When Bitty runs from an AppImage, `$APPIMAGE` names the file. `bitty upgrade`
replaces that file atomically (keeping `<name>.prev`), so the next launch uses
the new build. See [`appimage.md`](appimage.md).

### Homebrew

```sh
brew upgrade bitty          # or: bitty upgrade --yes
```

`bitty upgrade` prints the command and only executes it with `--yes`.

### Scoop

```powershell
scoop update bitty          # or: bitty upgrade --yes
```

### AUR

```sh
yay -S --noconfirm bitty    # or: bitty upgrade --yes (yay/paru)
```

AUR packages are built locally, so the channel is user-scoped. `bitty upgrade`
uses `yay` when present, else `paru`, else prints the `yay` command.

### pacman (Arch repositories)

```sh
sudo pacman -Syu bitty
```

System-owned: printed only, exit `7`.

### apt (Debian, Ubuntu)

```sh
sudo apt-get update && sudo apt-get install --only-upgrade bitty
```

System-owned: printed only, exit `7`. See [`debian.md`](debian.md).

### dnf / rpm (Fedora, RHEL, openSUSE)

```sh
sudo dnf upgrade bitty
```

System-owned: printed only, exit `7`. Without `dnf` the channel prints the
matching `rpm -U` command. See [`fedora.md`](fedora.md) and
[`opensuse.md`](opensuse.md).

### Nix

```sh
nix profile upgrade bitty
```

System-owned: printed only, exit `7`. A NixOS system package needs
`sudo nixos-rebuild switch --upgrade` instead. See [`nix.md`](nix.md).

### Unknown

When no channel matches, `bitty upgrade` refuses (exit `1`) and prints the
manual command for every supported channel plus a link to this page. Install
with a documented channel to make later upgrades one command.

## Rollback

`bitty upgrade --rollback` restores `bitty.prev` for `script` and `AppImage`
installs. Before swapping, it runs the backup's `--version` and requires a
well-formed `bitty X.Y.Z` answer, so a truncated or foreign file is never
promoted. The restored binary replaces the current one (a later upgrade
recreates the backup) and the marker follows the restored version.

```sh
bitty upgrade --rollback --dry-run   # verify the backup, write nothing
bitty upgrade --rollback
```

Package-manager channels have no rollback here: use the channel's own
downgrade path (`pacman -U`, `apt-get install bitty=<version>`, ...).

## Trust model

Verification is **hash-only** today:

- the asset's SHA-256 is read from the release `SHA256SUMS` and compared
  before any replace; a mismatch leaves the installed binary untouched;
- the fetch runs through the system `curl` with `--proto =https` (only a
  loopback test base may use plain HTTP), `--proto-redir =https` (a redirect
  to a non-HTTPS URL is refused), bounded connect/total time, bounded
  redirects, and a hard response-size cap;
- secret-shaped environment variables (`*_TOKEN`, `*_KEY`, `*_SECRET`) never
  reach the fetch child; proxy and CA discovery stay inherited;
- nothing is written outside the detected install directory.

Because `SHA256SUMS` is fetched from the same origin as the asset, it proves
integrity of the transfer, not origin. A party controlling the CDN (or a
redirect target) could serve a self-consistent pair. A signed manifest —
Sigstore/cosign or an Ed25519 key directory with a rotation story — is the
follow-up; until it lands the trust anchor is TLS plus the release workflow's
own `sha256sum -c` gate over `dist/*`.

`--dry-run` downloads nothing and `--check` only reads the version pointer.

## Test override

`BITTY_CDN_BASE` overrides the release host (test and manual use only). The
value must be HTTPS unless the host is loopback, where plain HTTP is accepted
so the test suite can serve a release from `127.0.0.1`. Treat it as a
development knob, never as a supported mirror mechanism.
