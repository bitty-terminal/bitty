#!/usr/bin/env bash
# Regenerates `bitty-symbols-nerd.ttf`, the symbols-only fallback face
# embedded by `crates/bitty-render/src/embedded.rs` (issue #2931).
#
# Source: Nerd Fonts v3.5.1 `JetBrainsMonoNerdFont-Regular.ttf` — a
# Nerd-Fonts-patched JetBrains Mono, so the subset carries the PUA icon
# range, box drawing, block elements, and braille patterns in one face with
# one set of metrics. Nerd Fonts' `SymbolsNerdFont-Regular.ttf` is not used:
# it has no box/braille coverage, so a terminal would need a second embedded
# face for ranges the chain already relies on.
#
# Kept ranges (everything else is dropped):
#
#   U+E000-U+F8FF  private-use icon range (Powerline, Font Awesome, Devicons,
#                  Codicons, Seti, Weather, Octicons, Font Logos, Pomicons)
#   U+2500-U+257F  box drawing
#   U+2580-U+259F  block elements
#   U+2800-U+28FF  braille patterns (btop-style TUI graphs)
#
# The face is renamed to `Bitty Symbols Nerd` (OFL section 3: a modified
# version must not use the original reserved name) and checked in at
# `bitty-symbols-nerd.ttf`. Licence: SIL OFL 1.1 for the font software,
# MIT for the Nerd Fonts tooling. See `LICENSES.md` in this directory.
#
# Requirements: `curl`, `tar`, `pyftsubset` (fonttools), `python3`.
# Usage: `crates/bitty-render/assets/generate-symbols-font.sh`
set -euo pipefail

readonly NERD_FONTS_VERSION="3.5.1"
readonly SOURCE_URL="https://github.com/ryanoasis/nerd-fonts/releases/download/v${NERD_FONTS_VERSION}/JetBrainsMono.tar.xz"
# SHA-256 of the pinned release archive, so a moved or replaced asset is a
# hard failure instead of a silent supply-chain change.
readonly SOURCE_SHA256="04d5e8f903693f9dd13e16f867e994834e681eb3c72c0d337a770dcda09010cf"
readonly SOURCE_FACE="JetBrainsMonoNerdFont-Regular.ttf"
readonly FAMILY="Bitty Symbols Nerd"
ASSET_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly ASSET_DIR
OUTPUT="$ASSET_DIR/bitty-symbols-nerd.ttf"
readonly OUTPUT

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

curl --fail --silent --show-error --location --max-time 180 \
  --output "$work/nerd-fonts.tar.xz" "$SOURCE_URL"

actual="$(sha256sum "$work/nerd-fonts.tar.xz" | cut -d' ' -f1)"
if [ "$actual" != "$SOURCE_SHA256" ]; then
  echo "error: $SOURCE_URL checksum mismatch" >&2
  echo "  expected $SOURCE_SHA256" >&2
  echo "  actual   $actual" >&2
  exit 1
fi

tar -xf "$work/nerd-fonts.tar.xz" -C "$work" "$SOURCE_FACE"

# Drop hinting, layout, and glyph-name tables: terminal icons are rasterized
# from outlines, so the bytecode only costs size.
pyftsubset "$work/$SOURCE_FACE" \
  --unicodes="E000-F8FF,2500-257F,2580-259F,2800-28FF" \
  --output-file="$work/subset.ttf" \
  --layout-features='' \
  --no-hinting \
  --drop-tables+=GSUB,GPOS,GDEF,DSIG,PfEd,gasp \
  --glyph-names=0

# Rename the face (OFL section 3) and drop the source's vendor URLs.
python3 - "$work/subset.ttf" "$OUTPUT" "$FAMILY" <<'PY'
import sys
from fontTools.ttLib import TTFont

src, dst, family = sys.argv[1], sys.argv[2], sys.argv[3]
font = TTFont(src)
name = font["name"]
for name_id in (1, 3, 4, 6, 16, 17, 18, 20, 21):
    name.removeNames(nameID=name_id)
name.setName(family, 1, 3, 1, 0x409)
name.setName(family, 4, 3, 1, 0x409)
name.setName(family.replace(" ", ""), 6, 3, 1, 0x409)
name.setName(family, 16, 3, 1, 0x409)
name.setName("Regular", 17, 3, 1, 0x409)
# Glyph names are dead weight for a terminal icon face; format 3.0 drops the
# per-glyph name array (~66 KiB here).
font["post"].formatType = 3.0
# Reproducible output: fontTools stamps `head` with the save time by default,
# which would make every regeneration a different commit. Pin both stamps to
# the Nerd Fonts v3.5.1 release date (2025-04-24 UTC, Mac epoch) and disable
# the timestamp refresh.
font["head"].created = 3828297600
font["head"].modified = 3828297600
font.recalcTimestamp = False
font.save(dst)
PY

echo "wrote $OUTPUT"
stat -c '%s bytes' "$OUTPUT"
