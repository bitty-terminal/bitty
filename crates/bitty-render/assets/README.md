# Embedded render assets

## `bitty-symbols-nerd.ttf`

Symbols-only Nerd font subset compiled into `bitty` as the last face of every
font fallback chain (`src/embedded.rs`, issue #2931). It covers the Nerd Font
private-use icon range, box drawing, block elements, and braille patterns, so
a stock Windows/macOS install without a Nerd font paints statusline, bar, and
devtools icons instead of tofu.

| Property         | Value                                                                                         |
| ---------------- | --------------------------------------------------------------------------------------------- |
| Source           | Nerd Fonts v3.5.1 `JetBrainsMonoNerdFont-Regular.ttf`                                         |
| Source archive   | `https://github.com/ryanoasis/nerd-fonts/releases/download/v3.5.1/JetBrainsMono.tar.xz`       |
| Archive SHA-256  | `04d5e8f903693f9dd13e16f867e994834e681eb3c72c0d337a770dcda09010cf`                            |
| Tool             | `pyftsubset` (fonttools) plus a fontTools name-table rename                                   |
| Family name      | `Bitty Symbols Nerd` (OFL 1.1 section 3 rename)                                               |
| Codepoint ranges | `U+E000-U+F8FF`, `U+2500-U+257F`, `U+2580-U+259F`, `U+2800-U+28FF`                            |
| Glyphs           | 4131 (3714 PUA icons, 160 box/block, 256 braille)                                             |
| Size             | ~1.0 MiB (1,040,920 bytes)                                                                    |
| SHA-256          | `f8130bb8aa2c5a1e6e72a16045526fd93648dd86a12171b33d24669bdbf2d8e3`                            |
| Licence          | SIL OFL 1.1 (font software) + MIT (Nerd Fonts tooling), see `LICENSES/bitty-symbols-nerd.txt` |

`U+F8FF` (the legacy Apple logo) is **not** mapped: Nerd Fonts removed that
glyph, so the PUA span is the range bound, not a per-codepoint guarantee.

### Size note

The Nerd Font BMP PUA set is 3714 glyphs of detailed outlines. Subsetting to
the full PUA plus the box/block/braille slices lands at ~1.0 MiB after
dropping hinting, layout, and glyph-name tables; there is no smaller subset
that still covers the icon sets terminal UIs use (Font Awesome alone is
~344 KiB, Devicons ~238 KiB, Codicons ~89 KiB, Octicons ~62 KiB). For
comparison, ghostty embeds the 2.4 MiB upstream symbols face. A 400 KiB
budget would only fit Font Awesome plus the graph slices, leaving Devicons,
Codicons, Seti, Weather, and Octicons as tofu on stock Windows/macOS, so
coverage won.

### Regeneration

```sh
crates/bitty-render/assets/generate-symbols-font.sh
```

The script pins the release version, verifies the archive SHA-256, subsets
with `pyftsubset`, renames the face, and rewrites `bitty-symbols-nerd.ttf`.
Output is byte-reproducible: the `head` timestamps are pinned to the release
date and the glyph order is deterministic, so a re-run on the same source
produces the same SHA-256.
Re-run it only when bumping the pinned Nerd Fonts release, then re-run
`cargo test -p bitty-render` (the `embedded` tests assert PUA, box, block, and
braille coverage) and `bitty doctor` (the `font-chain` row reports the
resolved face and PUA coverage).

## `bitty-han-test.ttf`

Tiny CJK test fixture for the mixed-script fallback regression (bitty#3021,
`tests/shaped_mixed_script_fallback.rs`). **Test-only:** it is compiled in by
`src/embedded.rs` as `HAN_TEST_BYTES`, is not a member of any font fallback
chain, and no production path loads it. It exists so the Han half of the
regression resolves on a host that ships no CJK face — a CI image without one
must fail the test when the per-cluster retry regresses, instead of skipping
the reported bug.

| Property         | Value                                                                 |
| ---------------- | --------------------------------------------------------------------- |
| Source           | Noto Sans CJK SC Regular 2.004 (`.ttc` member index 2)                |
| Tool             | `pyftsubset` (fonttools) plus a fontTools name-table rename           |
| Family name      | `Bitty Han Test` (OFL 1.1 section 3 rename)                           |
| Codepoints       | `U+0020-U+007E`, `U+4E03 U+4E8C U+4E94 U+533B U+5934 U+7528 U+75DB U+8BF7 U+95EE U+997F` |
| Glyphs           | 105                                                                   |
| Size             | 9,092 bytes                                                           |
| SHA-256          | `938210454facad6522bd287a2622d223723849bad5052a2dfc620d6ed0ed3e55`    |
| Licence          | SIL OFL 1.1, see `LICENSES/bitty-han-test.txt`                         |

Twelve Han scalars are the reported line `> 请问饿七五二二头痛医头用`; the ASCII
range carries its `>` prompt prefix and digits. Regeneration:

```sh
crates/bitty-render/assets/generate-han-test-font.sh
```

The script resolves the face with `fc-match 'Noto Sans CJK SC'` (override with
`BITTY_HAN_TEST_SOURCE`), pins `head` timestamps, and is byte-reproducible. It
needs `pyftsubset` on `PATH` (`uv tool install fonttools` on a host without a
system fontTools). Extend the `--unicodes` list when the regression line grows,
then re-run `cargo test -p bitty-render --test shaped_mixed_script_fallback`
and `cargo test -p bitty-render embedded` (the fixture coverage test parses the
compiled-in bytes).
