# Flathub

Issue: bitty-terminal/bitty#2518

This directory holds the Flatpak manifest for the Flathub submission of
Bitty (`run.bitty.Bitty`). The manifest targets the Freedesktop SDK runtime
(`org.freedesktop.Platform` / `org.freedesktop.Sdk`) and packages the same
versioned `.tar.zst` Linux bundles the release workflow publishes as GitHub
release assets, one source per architecture (x86_64, aarch64).

## Status

The Flathub submission itself is an **owner action**: the manifest is authored
in-tree, but nothing under `packaging/flathub/` is wired into CI, and nobody
here may open a Flathub submission or publish on release — that step belongs
to the owner. Note the bundle's built-from-release-binary nature in the body
of the Flathub submission; Flathub may request a justification for
`--filesystem=host-os` and the Flatpak spawn portal, which the manifest header
comments (and this file) already document.

## What the manifest uses

- `finish-args`: the standard terminal emulator set (`--share=ipc`,
  `--socket=wayland`, `--socket=fallback-x11`, `--device=dri`) plus
  `--talk-name=org.freedesktop.Flatpak` (allowing `flatpak-spawn --host`) and
  `--filesystem=host-os:ro` so the terminal can see and launch host shells.
  The same pattern is used by WezTerm and GNOME Console on Flathub.
- Module: extracts the release `.tar.zst` bundle with
  `strip-components: 1` (bundle layout: `bin/bitty`, `bin/bittyd`, `share/...`,
  root-level `terminfo/bitty.ti`), installs
  `bin/bitty` into `/app/bin`, copies `share/` (desktop entry, hicolor
  icons, AppStream metainfo) into `/app/share`, drops the bundle's systemd
  user unit (see below), and compiles the terminfo
  entry with `ncurses` `tic -x -o /app/share/terminfo terminfo/bitty.ti`
  (the same pattern PKGBUILD/bitty.spec use).

## Daemon: `bittyd` is not shipped

The release bundle carries `bin/bittyd` and
`share/systemd/user/bittyd.service`, but the manifest installs neither.
bittyd is deliberately not installed, for three reasons:

- The Flatpak sandbox has no systemd user manager: `org.freedesktop.Platform`
  runs no per-user systemd instance, so `systemctl --user enable bittyd` inside
  the sandbox has nothing to talk to and the unit could never be enabled.
- `bittyd` hosts the user's shells through the per-user containment plan.
  Inside the sandbox every process it spawned would inherit the app's own
  filesystem and device view, so it could not be the session host described by
  ADR-0019 and the `bittyd` session-host contract.
- `command: bitty` starts the terminal; nothing in the sandbox supervises a
  background process, so a shipped binary would be dead weight.

Flathub is therefore a **no daemon** channel: the terminal works, `bittyd` does
not exist in it. A user who needs the session daemon installs a channel that
ships it (`.deb`/`.rpm`/AUR/prebuilt bundle or `install.sh`), where the unit
lands on disk and is enabled per user with
`systemctl --user enable --now bittyd`. There is no way to run `bittyd` from
the Flatpak build: the binary is not installed, so
`flatpak run --command=bittyd run.bitty.Bitty start` fails.

## Submission checklist (owner)

1. Create the Flathub repository `flathub/run.bitty.Bitty` and copy `run.bitty.Bitty.yaml`
   into it.
2. Verify the pinned `sha256` digests against the release sidecars
   (`.tar.zst.sha256` under the same tag as the download URL).
3. Build locally: `flatpak-builder --user --install --force-clean build-dir
   run.bitty.Bitty.yaml`.
4. State in the submission body that the build is terminal-only: it installs
   `bin/bitty` and deliberately omits `bin/bittyd` and its systemd user unit,
   because the sandbox has no systemd user manager. Point users who need the
   session daemon at a channel that ships it; there is no way to run `bittyd`
   from the Flatpak build.
5. Open the Flathub submission. This is the only step that has any release —
   the manifest will not reach Flathub through any automation in this repo.

## Refresh procedure (each release)

For every release, bump these pinned values in `run.bitty.Bitty.yaml`:

- `url` (both architectures): tag segment and bundle asset name,
- `sha256` (both architectures): digest of that release's `.tar.zst` bundle
  asset, taken from its published `*.sha256` sidecar,
- `runtime-version`: only when the default Flathub Freedesktop runtime
  advances a major minor.

Keep `packaging/run.bitty.Bitty.metainfo.xml` in sync: its `<releases>` block
must list the newest version so Flathub shows up-to-date release notes.
