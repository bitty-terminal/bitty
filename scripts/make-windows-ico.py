#!/usr/bin/env python3
"""Generate packaging/icons/bitty.ico for the Windows shortcut and app list.

install.ps1 ships this file next to bitty.exe and points both the Start Menu
shortcut and the app-list DisplayIcon at it, so Windows Search and Settings
show the Bitty mascot instead of the generic executable icon (bitty#2926).
bitty#2929 embeds the icon in bitty.exe itself; until that lands this
checked-in .ico is the icon source.

Sizes follow the shell surfaces that request them: 16/32 for the taskbar and
tree views, 48/64 for Start Menu and Explorer tiles, 128/256 for large-icon
and app-list views. Every size reuses the hand-tuned PNG of the same size
from packaging/icons/hicolor/ (a plain downscale of the 256 px asset loses
the small-size tuning); 48 has no PNG and is derived from the 64 px asset so
it keeps that tuned design.

Regenerate after changing the hicolor PNGs. Needs Pillow 9.1+ for multi-size
`append_images` support in the ICO writer:

    python3 scripts/make-windows-ico.py
"""

from pathlib import Path

from PIL import Image

REPO_ROOT = Path(__file__).resolve().parent.parent
ICON_DIR = REPO_ROOT / "packaging" / "icons"
HICOLOR_DIR = ICON_DIR / "hicolor"
OUTPUT = ICON_DIR / "bitty.ico"
SIZES = (16, 32, 48, 64, 128, 256)
# 48 px has no hand-tuned PNG; derive it from the closest tuned asset.
FALLBACK_SIZE = 64


def load_frame(size: int) -> Image.Image:
    """Return the tuned RGBA frame for one ICO size."""
    png = HICOLOR_DIR / f"{size}x{size}" / "apps" / "bitty.png"
    if png.is_file():
        return Image.open(png).convert("RGBA")
    fallback = HICOLOR_DIR / f"{FALLBACK_SIZE}x{FALLBACK_SIZE}" / "apps" / "bitty.png"
    return (
        Image.open(fallback)
        .convert("RGBA")
        .resize((size, size), Image.Resampling.LANCZOS)
    )


def main() -> None:
    base = load_frame(256)
    extras = [load_frame(size) for size in SIZES if size != 256]
    base.save(
        OUTPUT,
        format="ICO",
        sizes=[(size, size) for size in SIZES],
        append_images=extras,
    )
    print(f"wrote {OUTPUT.relative_to(REPO_ROOT)} ({OUTPUT.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
