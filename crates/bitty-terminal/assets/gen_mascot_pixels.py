#!/usr/bin/env python3
"""Vendor the Bittie pixel splash into Rust (issue #1318, CTX-1217).

Reads the checked-in 32x32 master `bittie_mascot_32x32.png` (byte copy of
the workspace parametric sprite
`recording/bitty-mascot/pixel-art/bitty_mascot_32x32/bitty_mascot_32x32.png`)
and emits `mascot_pixels.rs`: the 21-color palette plus one palette-index
byte per pixel (255 = transparent) for `crates/bitty-terminal/src/mascot.rs`
to render as half-block terminal cells at runtime.

Stdlib only (`struct`, `zlib`): no Pillow, no workspace imports, no
absolute paths. Run from this directory:

    python3 gen_mascot_pixels.py

The script fails closed on any surprise so a bad source can never silently
ship: exact 32x32 RGBA8 non-interlaced geometry, 1-bit alpha, every opaque
pixel exactly on the palette below (a copy of
`recording/bitty-mascot/pixel-art/shared/palette.py`), the canonical
unmirrored `> _` eye pixels intact, and sane opaque/transparent coverage.
Re-running over an unchanged source must reproduce `mascot_pixels.rs`
byte-for-byte.
"""

import hashlib
import os
import struct
import sys
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
SRC_NAME = "bittie_mascot_32x32.png"
OUT_NAME = "mascot_pixels.rs"

SIZE = 32

# Copy of the canonical tokens in
# recording/bitty-mascot/pixel-art/shared/palette.py (order matters: the
# emitted indices address this table).
PALETTE_HEX = [
    "#281711",
    "#42281d",  # outlines, eye glyphs
    "#432517",
    "#5c3924",
    "#8b5f43",
    "#b38565",  # fur + stripe
    "#ccb49b",
    "#fae9d4",
    "#fff8ee",  # cream
    "#c76464",
    "#f08585",
    "#fcaeae",
    "#f7b5ab",  # pink + blush
    "#73675a",
    "#b0a394",
    "#ded7cb",  # window frame (unused here)
    "#151922",
    "#1e2430",  # terminal slate (unused here)
    "#e54d42",
    "#f1c40f",
    "#2ecc71",  # window buttons (unused here)
]

TRANSPARENT = 255

# Canonical `> _` eye pixels for the neutral build.py pose
# (squash=0: eye baseline fy = 12). Every listed (x, y) must be the
# outline-dark palette entry so mirroring or palette drift fails loudly.
OUTLINE = 0
PROMPT_EYE = [(11, 12), (12, 12), (13, 13), (11, 14), (12, 14)]
CURSOR_EYE = [(18, 14), (19, 14), (20, 14), (21, 14)]


def fail(message):
    print("gen_mascot_pixels: REFUSING: {0}".format(message), file=sys.stderr)
    sys.exit(1)


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    return b if pb <= pc else c


def read_png(path):
    with open(path, "rb") as handle:
        data = handle.read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        fail("{0} is not a PNG".format(path))
    pos = 8
    width = height = None
    idat = b""
    while pos < len(data):
        if pos + 8 > len(data):
            fail("truncated chunk header")
        (length,) = struct.unpack(">I", data[pos : pos + 4])
        ctype = data[pos + 4 : pos + 8]
        body = data[pos + 8 : pos + 8 + length]
        if len(body) != length:
            fail("truncated {0} chunk".format(ctype))
        if ctype == b"IHDR":
            width, height, bitdepth, colortype, _comp, _filt, interlace = struct.unpack(
                ">IIBBBBB", body
            )
            if (width, height) != (SIZE, SIZE):
                fail("geometry {0}x{1}, want {2}x{2}".format(width, height, SIZE))
            if (bitdepth, colortype, interlace) != (8, 6, 0):
                fail(
                    "want 8-bit RGBA non-interlaced, got depth={0} type={1} interlace={2}".format(
                        bitdepth, colortype, interlace
                    )
                )
        elif ctype == b"IDAT":
            idat += body
        pos += 12 + length
    if width is None:
        fail("missing IHDR")
    try:
        raw = zlib.decompress(idat)
    except zlib.error as err:
        fail("IDAT zlib error: {0}".format(err))
    stride = SIZE * 4
    if len(raw) != SIZE * (stride + 1):
        fail("scanline length {0}, want {1}".format(len(raw), SIZE * (stride + 1)))
    pixels = bytearray(SIZE * SIZE * 4)
    prev = bytearray(stride)
    for y in range(SIZE):
        filt = raw[y * (stride + 1)]
        if filt > 4:
            fail("unknown filter {0} on row {1}".format(filt, y))
        line = bytearray(raw[y * (stride + 1) + 1 : (y + 1) * (stride + 1)])
        for x in range(stride):
            a = line[x - 4] if x >= 4 else 0
            b = prev[x]
            c = prev[x - 4] if x >= 4 else 0
            if filt == 1:
                line[x] = (line[x] + a) & 0xFF
            elif filt == 2:
                line[x] = (line[x] + b) & 0xFF
            elif filt == 3:
                line[x] = (line[x] + (a + b) // 2) & 0xFF
            elif filt == 4:
                line[x] = (line[x] + paeth(a, b, c)) & 0xFF
        pixels[y * stride : (y + 1) * stride] = line
        prev = line
    return pixels


def main():
    src = os.path.join(HERE, SRC_NAME)
    out = os.path.join(HERE, OUT_NAME)
    with open(src, "rb") as handle:
        digest = hashlib.sha256(handle.read()).hexdigest()
    pixels = read_png(src)

    palette = {}
    for index, token in enumerate(PALETTE_HEX):
        rgb = (int(token[1:3], 16), int(token[3:5], 16), int(token[5:7], 16))
        if rgb in palette:
            fail("duplicate palette entry {0}".format(token))
        palette[rgb] = index

    indices = []
    opaque = 0
    transparent = 0
    used = set()
    for i in range(SIZE * SIZE):
        red, green, blue, alpha = pixels[4 * i : 4 * i + 4]
        if alpha == 0:
            transparent += 1
            indices.append(TRANSPARENT)
            continue
        if alpha != 255:
            fail(
                "semi-transparent pixel {0}: RGBA=({1},{2},{3},{4})".format(
                    i, red, green, blue, alpha
                )
            )
        opaque += 1
        if (red, green, blue) not in palette:
            fail("off-palette pixel {0}: ({1},{2},{3})".format(i, red, green, blue))
        index = palette[(red, green, blue)]
        used.add(index)
        indices.append(index)

    if not 100 <= opaque <= 900:
        fail("opaque coverage {0} outside sane bounds".format(opaque))
    if transparent < 100:
        fail("transparent coverage {0} outside sane bounds".format(transparent))
    if len(used) < 8:
        fail("only {0} palette entries used".format(len(used)))
    for x, y in PROMPT_EYE + CURSOR_EYE:
        if indices[y * SIZE + x] != OUTLINE:
            fail(
                "eye pixel ({0},{1}) is not outline-dark: mirroring or drift?".format(
                    x, y
                )
            )

    lines = []
    lines.append("// GENERATED FILE -- do not edit by hand (issue #1318, CTX-1217).")
    lines.append("//")
    lines.append("// Source: `{0}` (SHA-256 `{1}`),".format(SRC_NAME, digest))
    lines.append("// byte copy of the workspace parametric sprite")
    lines.append("// `recording/bitty-mascot/pixel-art/bitty_mascot_32x32/`")
    lines.append("// (neutral pose, canonical unmirrored `> _` eyes).")
    lines.append(
        "// Regenerate with `python3 gen_mascot_pixels.py` in this directory;"
    )
    lines.append("// the generator fails closed on geometry, palette, alpha, eye, and")
    lines.append("// coverage drift, and is byte-stable over an unchanged source.")
    lines.append("")
    lines.append("/// Palette addressed by [`MASCOT_PIXELS`]: (R, G, B) triples in the")
    lines.append("/// order of the workspace `shared/palette.py` canonical tokens.")
    lines.append("pub(crate) const MASCOT_PALETTE: [[u8; 3]; 21] = [")
    for token in PALETTE_HEX:
        lines.append(
            "    [0x{0}, 0x{1}, 0x{2}], // {3}".format(
                token[1:3], token[3:5], token[5:7], token
            )
        )
    lines.append("];")
    lines.append("")
    lines.append("/// Index of a fully transparent pixel in [`MASCOT_PIXELS`].")
    lines.append("pub(crate) const MASCOT_TRANSPARENT: u8 = 255;")
    lines.append("")
    lines.append("/// Row-major sprite pixels `[y][x]`: palette indices, or")
    lines.append("/// [`MASCOT_TRANSPARENT`]. The source alpha is 1-bit, so no")
    lines.append("/// blending is ever needed at render time.")
    lines.append("pub(crate) const MASCOT_PIXELS: [[u8; 32]; 32] = [")
    for y in range(SIZE):
        row = ", ".join("{0:3}".format(indices[y * SIZE + x]) for x in range(SIZE))
        lines.append("    [{0}],".format(row))
    lines.append("];")
    lines.append("")
    with open(out, "w", encoding="utf-8", newline="\n") as handle:
        handle.write("\n".join(lines))
    print(
        "wrote {0}: {1} opaque + {2} transparent px, {3} colors, src sha256 {4}".format(
            OUT_NAME, opaque, transparent, len(used), digest[:12]
        )
    )


if __name__ == "__main__":
    main()
