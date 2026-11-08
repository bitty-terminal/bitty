#!/usr/bin/env bash
# Regenerates `bitty-han-test.ttf`, the tiny CJK regression fixture used by
# `tests/shaped_mixed_script_fallback.rs` (bitty#3021).
#
# The fixture exists so the Han half of the mixed-script regression never
# depends on the host font stack: CI images without a CJK face must still
# exercise the per-cluster fallback retry instead of skipping the test.
# It is NOT part of any production fallback chain — only the regression test
# loads it.
#
# Source: Noto Sans CJK SC Regular (SIL OFL 1.1), the Linux CJK fallback face
# named by `bitty_config::types::CJK_FALLBACK_FAMILY`. The suite runs on
# hosts whose Noto CJK package ships the `.ttc` collection, so the face is
# selected by index (`--font-number=2`, the SC member).
#
# Kept scalars (everything else is dropped):
#
#   U+0020-U+007E  printable ASCII (the `>` prompt prefix and digits)
#   U+4E03 U+4E8C U+4E94 U+533B U+5934 U+7528 U+75DB U+8BF7 U+95EE U+997F
#                  the reported line 请问饿七五二二头痛医头用
#
# The face is renamed to `Bitty Han Test` (OFL section 3: a modified version
# must not use the original reserved name). Licence: SIL OFL 1.1 for the font
# software; see `LICENSES/bitty-han-test.txt`.
#
# Output is byte-reproducible: `head` timestamps are pinned and the subset
# keeps fontTools' deterministic glyph order, so a re-run on the same source
# yields the same SHA-256.
#
# Requirements: `pyftsubset` (fonttools), `python3`. On a host without a
# system fontTools, prefix with `uvx --from fonttools`:
#
#   PATH="$(uv tool dir)/fonttools/bin:$PATH" crates/bitty-render/assets/generate-han-test-font.sh
#
# Usage: `crates/bitty-render/assets/generate-han-test-font.sh`
set -euo pipefail

readonly FAMILY="Bitty Han Test"
readonly SOURCE_INDEX=2
readonly UNICODES="0020-007E,4E03,4E8C,4E94,533B,5934,7528,75DB,8BF7,95EE,997F"
ASSET_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly ASSET_DIR
OUTPUT="$ASSET_DIR/bitty-han-test.ttf"
readonly OUTPUT

# Host discovery: the CN/SC member of the Noto CJK collection. Overridable so
# the script still works where the collection carries a different index.
SOURCE="${BITTY_HAN_TEST_SOURCE:-$(fc-match -f '%{file}' 'Noto Sans CJK SC')}"
readonly SOURCE
if [ ! -f "$SOURCE" ]; then
  echo "error: no CJK source face (fc-match 'Noto Sans CJK SC' -> '$SOURCE')" >&2
  echo "  install Noto Sans CJK SC or set BITTY_HAN_TEST_SOURCE" >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

pyftsubset "$SOURCE" \
  --font-number="$SOURCE_INDEX" \
  --unicodes="$UNICODES" \
  --output-file="$work/subset.ttf" \
  --layout-features='' \
  --no-hinting \
  --desubroutinize \
  --drop-tables+=GSUB,GPOS,GDEF,DSIG,PfEd,gasp \
  --glyph-names=0

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
# A test fixture needs no glyph names; format 3.0 drops the array.
font["post"].formatType = 3.0
# Byte-reproducible output: pin `head` stamps to a fixed date and disable the
# save-time refresh, or every regeneration diffs.
font["head"].created = 3828297600
font["head"].modified = 3828297600
font.recalcTimestamp = False
font.save(dst)
PY

echo "wrote $OUTPUT"
stat -c '%s bytes' "$OUTPUT"
sha256sum "$OUTPUT"
