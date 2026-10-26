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
  `strip-components: 1` (bundle layout: `bin/bitty`, `share/...`), installs
  `bin/bitty` into `/app/bin`, and copies `share/` into `/app/share`, which
  carries the desktop entry, hicolor icons, AppStream metainfo, and terminfo.

## Submission checklist (owner)

1. Create the Flathub repository `flathub/run.bitty.Bitty` and copy `run.bitty.Bitty.yaml`
   into it.
2. Verify the pinned `sha256` digests against the release sidecars
   (`.tar.zst.sha256` under the same tag as the download URL).
3. Build locally: `flatpak-builder --user --install --force-clean build-dir
   run.bitty.Bitty.yaml`.
4. Open the Flathub submission. This is the only step that has any release —
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
