# AppImage (Standalone Portable Linux Executable)

Bitty provides official standalone, single-file AppImage bundles for Linux
systems across x86_64 and aarch64 architectures.

AppImages run directly on the host system without container or sandbox
isolation, preserving complete access to the user's `$SHELL`, environment
variables, host packages, GPU acceleration, and desktop themes.

## Download and Execution

### 1. Download

Download the release AppImage and verification checksum from the GitHub Releases
page or the official Cloudflare R2 CDN mirror:

```sh
# Example for x86_64
curl -fsSL -O https://cdn.bitty.run/releases/v0.0.29/bitty-x86_64-unknown-linux-gnu.AppImage
curl -fsSL -O https://cdn.bitty.run/releases/v0.0.29/bitty-x86_64-unknown-linux-gnu.AppImage.sha256
sed 's#  dist/#  #' bitty-x86_64-unknown-linux-gnu.AppImage.sha256 | sha256sum -c -
```

### 2. Make Executable and Run

```sh
chmod +x bitty-x86_64-unknown-linux-gnu.AppImage
./bitty-x86_64-unknown-linux-gnu.AppImage
```

## Desktop Integration

### Automatic Desktop Integration

If you use an AppImage manager such as **AppImageLauncher** or **Geary/Geary-AppImage**,
simply opening the AppImage will prompt you to integrate Bitty into your application
menu with full icon and desktop entry support.

### Manual Desktop Integration

To manually install Bitty into your user application menu:

```sh
mkdir -p ~/.local/bin ~/.local/share/applications ~/.local/share/icons/hicolor/256x256/apps

# Copy the executable
cp bitty-x86_64-unknown-linux-gnu.AppImage ~/.local/bin/bitty
chmod +x ~/.local/bin/bitty

# Extract and copy desktop entry and icons
./bitty-x86_64-unknown-linux-gnu.AppImage --appimage-extract bitty.png
cp squashfs-root/bitty.png ~/.local/share/icons/hicolor/256x256/apps/bitty.png
rm -rf squashfs-root
```

Create `~/.local/share/applications/run.bitty.Bitty.desktop`:

```ini
[Desktop Entry]
Type=Application
Name=Bitty
Comment=Minimal correct terminal workspace
Exec=/home/%u/.local/bin/bitty
Icon=bitty
Terminal=false
Categories=System;TerminalEmulator;
StartupWMClass=run.bitty.Bitty
StartupNotify=true
```

## Build and Assembly

AppImage artifacts are assembled during the Linux `build` workflow in
`.github/workflows/release.yml` using `scripts/make-appimage.sh` and the official
`appimagetool` release with `APPIMAGE_EXTRACT_AND_RUN=1`.
