#!/usr/bin/env python3
"""Generate the checked-in Unicode Character Database tables for cell widths.

The generated Rust module is committed: builds and tests never fetch the
network and never need the UCD snapshots on disk. Regeneration is a manual,
reviewed step that takes the pinned UCD files as explicit inputs and records
their versions plus SHA-256 digests in the generated header.

Sources (Unicode 18.0.0):

- ``EastAsianWidth.txt`` for the East_Asian_Width property (UAX #11).
- ``emoji-data.txt`` for the Extended_Pictographic property (UTS #51).

Only non-``N`` East_Asian_Width ranges are emitted. UAX #11's ``@missing``
directive defaults every unlisted scalar to ``N``, and the W-default
unassigned CJK / plane 2 / plane 3 ranges appear explicitly in
EastAsianWidth.txt, so the default plus the emitted ranges is the complete
property (not a curated subset).

Usage::

    python3 scripts/generate-unicode-tables.py \\
        --east-asian-width EastAsianWidth-18.0.0.txt \\
        --emoji-data emoji-data-18.0.0.txt \\
        --output crates/bitty-term-state/src/width_tables.rs
"""

from __future__ import annotations

import argparse
import hashlib
import re
import sys
from pathlib import Path

EAW_CLASSES = ("N", "Na", "A", "H", "W", "F")
EMOJI_PROPERTY = "Extended_Pictographic"
MAX_CODE_POINT = 0x10FFFF


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_ranges(
    path: Path, want: set[str], property_name: str
) -> list[tuple[int, int, str]]:
    """Parses ``code[..code] ; property`` lines, ignoring comments."""
    ranges: list[tuple[int, int, str]] = []
    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        fields = [field.strip() for field in line.split(";")]
        if len(fields) != 2:
            raise SystemExit(f"{path}:{number}: expected two fields, got {raw!r}")
        code, prop = fields
        if prop not in want:
            continue
        if ".." in code:
            start_text, end_text = code.split("..", 1)
            start, end = int(start_text, 16), int(end_text, 16)
        else:
            start = end = int(code, 16)
        if not (0 <= start <= end <= MAX_CODE_POINT):
            raise SystemExit(f"{path}:{number}: invalid range {code!r}")
        ranges.append((start, end, prop))
    if not ranges:
        raise SystemExit(f"{path}: no {property_name} ranges found")
    return ranges


def merge_ranges(ranges: list[tuple[int, int, str]]) -> list[tuple[int, int, str]]:
    """Sorts and merges adjacent ranges that carry the same property."""
    merged: list[tuple[int, int, str]] = []
    for start, end, prop in sorted(ranges):
        if merged and merged[-1][2] == prop and merged[-1][1] + 1 == start:
            merged[-1] = (merged[-1][0], end, prop)
        else:
            merged.append((start, end, prop))
    for previous, current in zip(merged, merged[1:]):
        if current[0] <= previous[1]:
            raise SystemExit(
                f"overlapping ranges {previous[0]:04X}..{previous[1]:04X} and "
                f"{current[0]:04X}..{current[1]:04X}"
            )
    return merged


def ucd_version(path: Path, pattern: str) -> str:
    match = re.search(pattern, path.read_text(encoding="utf-8"), re.MULTILINE)
    if match is None:
        raise SystemExit(f"{path}: cannot find the UCD version header")
    return match.group(1)


def rust_u32(value: int) -> str:
    return f"0x{value:04X}"


def emit_eaw(ranges: list[tuple[int, int, str]]) -> str:
    lines = [
        "/// East_Asian_Width ranges (UAX #11), sorted and non-overlapping.",
        "///",
        "/// Unlisted scalars are [`EawClass::N`] per the UCD `@missing`",
        "/// directive; the W-default unassigned CJK and plane 2/3 ranges are",
        "/// present in the source file and therefore emitted here.",
        "pub(crate) const EAW_RANGES: &[(u32, u32, EawClass)] = &[",
    ]
    for start, end, prop in ranges:
        lines.append(f"    ({rust_u32(start)}, {rust_u32(end)}, EawClass::{prop}),")
    lines.append("];")
    return "\n".join(lines)


def emit_extended_pictographic(ranges: list[tuple[int, int, str]]) -> str:
    lines = [
        "/// `Extended_Pictographic` ranges (UTS #51), sorted and non-overlapping.",
        "pub(crate) const EXTENDED_PICTOGRAPHIC_RANGES: &[(u32, u32)] = &[",
    ]
    for start, end, _prop in ranges:
        lines.append(f"    ({rust_u32(start)}, {rust_u32(end)}),")
    lines.append("];")
    return "\n".join(lines)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--east-asian-width",
        required=True,
        type=Path,
        help="path to UCD EastAsianWidth.txt",
    )
    parser.add_argument(
        "--emoji-data",
        required=True,
        type=Path,
        help="path to UCD emoji/emoji-data.txt",
    )
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args(argv)

    eaw_version = ucd_version(
        args.east_asian_width, r"EastAsianWidth-(\d+\.\d+\.\d+)\.txt"
    )
    emoji_version = ucd_version(args.emoji_data, r"^# Version: (\d+\.\d+\.\d+)$")
    if eaw_version != emoji_version:
        raise SystemExit(
            f"UCD version mismatch: EastAsianWidth {eaw_version} vs emoji-data {emoji_version}"
        )

    eaw = merge_ranges(
        parse_ranges(
            args.east_asian_width, set(EAW_CLASSES) - {"N"}, "East_Asian_Width"
        )
    )
    extended = merge_ranges(
        parse_ranges(args.emoji_data, {EMOJI_PROPERTY}, EMOJI_PROPERTY)
    )
    eaw_digest = sha256(args.east_asian_width)
    emoji_digest = sha256(args.emoji_data)

    header = f'''//! @generated by `scripts/generate-unicode-tables.py`; do not edit.
//!
//! Unicode Character Database tables for cell-width and grapheme-segmentation
//! decisions (UCD {eaw_version}). Generated data is committed: builds and
//! tests never fetch the network, and the generator takes the pinned UCD
//! snapshots as explicit inputs.
//!
//! Provenance:
//!
//! - `EastAsianWidth.txt` {eaw_version} \
sha256 `{eaw_digest}`
//! - `emoji-data.txt` {emoji_version} \
sha256 `{emoji_digest}`
//!
//! Regenerate with:
//!
//! ```text
//! python3 scripts/generate-unicode-tables.py \\
//!     --east-asian-width EastAsianWidth-{eaw_version}.txt \\
//!     --emoji-data emoji-data-{emoji_version}.txt \\
//!     --output crates/bitty-term-state/src/width_tables.rs
//! ```
//!
//! `East_Asian_Width` covers W/F (two cells), A (policy-dependent), and
//! N/Na/H (one cell); only non-N ranges are stored, so the table stays
//! compact while the UAX #11 `@missing` default supplies the rest.

/// UCD version both generated tables were produced from.
#[cfg(test)]
pub(crate) const UCD_VERSION: &str = "{eaw_version}";

/// SHA-256 of the `EastAsianWidth.txt` snapshot the EAW table came from.
#[cfg(test)]
pub(crate) const EAST_ASIAN_WIDTH_SHA256: &str =
    "{eaw_digest}";

/// SHA-256 of the `emoji-data.txt` snapshot the pictographic table came from.
#[cfg(test)]
pub(crate) const EMOJI_DATA_SHA256: &str =
    "{emoji_digest}";

/// East_Asian_Width property class (UAX #11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EawClass {{
    /// Neutral: one cell under every ambiguous-width policy.
    N,
    /// Narrow: one cell under every ambiguous-width policy.
    Na,
    /// Ambiguous: resolved by the configured policy.
    A,
    /// Halfwidth: one cell.
    H,
    /// Wide: two cells.
    W,
    /// Fullwidth: two cells.
    F,
}}

{emit_eaw(eaw)}

/// Resolves the East_Asian_Width class for `cp`.
///
/// Binary search over [`EAW_RANGES`]; unlisted scalars resolve to
/// [`EawClass::N`] (UAX #11 `@missing` default).
#[must_use]
pub(crate) fn eaw_class(cp: u32) -> EawClass {{
    let index = EAW_RANGES.partition_point(|&(start, _, _)| start <= cp);
    if index > 0 {{
        let (_, end, class) = EAW_RANGES[index - 1];
        if cp <= end {{
            return class;
        }}
    }}
    EawClass::N
}}

{emit_extended_pictographic(extended)}

/// Whether `cp` is `Extended_Pictographic` (UTS #51).
///
/// Binary search over [`EXTENDED_PICTOGRAPHIC_RANGES`]; unlisted scalars are
/// not extended pictographic.
#[must_use]
pub(crate) fn is_extended_pictographic_scalar(cp: u32) -> bool {{
    let index = EXTENDED_PICTOGRAPHIC_RANGES.partition_point(|&(start, _)| start <= cp);
    index > 0 && cp <= EXTENDED_PICTOGRAPHIC_RANGES[index - 1].1
}}

#[cfg(test)]
mod tests {{
    use super::*;

    #[test]
    fn provenance_is_recorded() {{
        assert_eq!(UCD_VERSION, "{eaw_version}");
        assert_eq!(EAST_ASIAN_WIDTH_SHA256.len(), 64);
        assert_eq!(EMOJI_DATA_SHA256.len(), 64);
    }}

    #[test]
    fn eaw_ranges_are_sorted_and_disjoint() {{
        for pair in EAW_RANGES.windows(2) {{
            assert!(pair[0].0 <= pair[0].1, "range start after end: {{pair:?}}");
            assert!(
                pair[0].1 < pair[1].0,
                "unsorted or overlapping EAW ranges: {{pair:?}}"
            );
        }}
        assert!(EAW_RANGES.iter().all(|&(_, _, class)| class != EawClass::N));
    }}

    #[test]
    fn pictographic_ranges_are_sorted_and_disjoint() {{
        for pair in EXTENDED_PICTOGRAPHIC_RANGES.windows(2) {{
            assert!(pair[0].0 <= pair[0].1, "range start after end: {{pair:?}}");
            assert!(
                pair[0].1 < pair[1].0,
                "unsorted or overlapping pictographic ranges: {{pair:?}}"
            );
        }}
    }}

    #[test]
    fn spot_values_match_the_pinned_ucd() {{
        assert_eq!(eaw_class('\\u{{4E2D}}' as u32), EawClass::W);
        assert_eq!(eaw_class('\\u{{FF21}}' as u32), EawClass::F);
        assert_eq!(eaw_class('\\u{{FF61}}' as u32), EawClass::H);
        assert_eq!(eaw_class('\\u{{00A1}}' as u32), EawClass::A);
        assert_eq!(eaw_class('\\u{{0041}}' as u32), EawClass::Na);
        assert_eq!(eaw_class('\\u{{4E00}}' as u32), EawClass::W);
        assert!(is_extended_pictographic_scalar('\\u{{2764}}' as u32));
        assert!(is_extended_pictographic_scalar('\\u{{1F468}}' as u32));
        assert!(!is_extended_pictographic_scalar('\\u{{0041}}' as u32));
        assert!(!is_extended_pictographic_scalar('\\u{{20E3}}' as u32));
    }}
}}
'''

    args.output.write_text(header, encoding="utf-8")
    print(
        f"wrote {args.output} (UCD {eaw_version}: {len(eaw)} EAW ranges, "
        f"{len(extended)} Extended_Pictographic ranges)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
