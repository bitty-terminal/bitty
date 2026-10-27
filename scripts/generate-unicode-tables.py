#!/usr/bin/env python3
"""Generate the checked-in Unicode Character Database tables.

The generated Rust module is committed: builds and tests never fetch the
network and never need the UCD snapshots on disk. Regeneration is a manual,
reviewed step that takes the pinned UCD files as explicit inputs and records
their versions plus SHA-256 digests in the generated header.

Sources (Unicode 18.0.0):

- ``EastAsianWidth.txt`` for the East_Asian_Width property (UAX #11).
- ``emoji-data.txt`` for the Extended_Pictographic property (UTS #51).
- ``auxiliary/GraphemeBreakProperty.txt`` for the grapheme-cluster break
  classes ``Extend``, ``SpacingMark``, ``Prepend``, and ``Control`` (UAX #29).
- ``DerivedCoreProperties.txt`` for ``InCB; Linker``, ``InCB; Consonant``,
  and ``InCB; Extend`` (UAX #29 ``GB9c``).

Only non-``N`` East_Asian_Width ranges are emitted. UAX #11's ``@missing``
directive defaults every unlisted scalar to ``N``, and the W-default
unassigned CJK / plane 2 / plane 3 ranges appear explicitly in
EastAsianWidth.txt, so the default plus the emitted ranges is the complete
property (not a curated subset). The grapheme properties are emitted
exhaustively as their own sorted range tables.

The emitted Rust is formatted with the pinned ``rustfmt`` before the script
returns, so regeneration is byte-identical without a manual ``cargo fmt``:
CI's formatting gate covers the generated file like any other source.

Usage::

    python3 scripts/generate-unicode-tables.py \\
        --east-asian-width EastAsianWidth-18.0.0.txt \\
        --emoji-data emoji-data-18.0.0.txt \\
        --grapheme-break GraphemeBreakProperty-18.0.0.txt \\
        --derived-core DerivedCoreProperties-18.0.0.txt \\
        --output crates/bitty-term-state/src/unicode_tables.rs
"""

from __future__ import annotations

import argparse
import hashlib
import re
import subprocess
import sys
from pathlib import Path

EAW_CLASSES = ("N", "Na", "A", "H", "W", "F")
EMOJI_PROPERTY = "Extended_Pictographic"
MAX_CODE_POINT = 0x10FFFF
# Grapheme_Cluster_Break classes the segmenter reads. `CR`, `LF`, `ZWJ`,
# `Regional_Indicator`, and the Hangul classes stay algorithmic in
# `grapheme.rs`.
GCB_PROPERTIES = ("Extend", "SpacingMark", "Prepend", "Control")
# Indic_Conjunct_Break classes for GB9c.
INCB_PROPERTIES = ("Linker", "Consonant", "Extend")


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


def parse_incb(path: Path) -> list[tuple[int, int, str]]:
    """Parses ``code[..code] ; InCB; Class`` lines from DerivedCoreProperties."""
    ranges: list[tuple[int, int, str]] = []
    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        fields = [field.strip() for field in line.split(";")]
        if len(fields) != 3 or fields[1] != "InCB":
            continue
        code, _prop, prop = fields
        if prop not in INCB_PROPERTIES:
            raise SystemExit(f"{path}:{number}: unknown InCB class {prop!r}")
        if ".." in code:
            start_text, end_text = code.split("..", 1)
            start, end = int(start_text, 16), int(end_text, 16)
        else:
            start = end = int(code, 16)
        if not (0 <= start <= end <= MAX_CODE_POINT):
            raise SystemExit(f"{path}:{number}: invalid range {code!r}")
        ranges.append((start, end, prop))
    if not ranges:
        raise SystemExit(f"{path}: no InCB ranges found")
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


def select(ranges: list[tuple[int, int, str]], prop: str) -> list[tuple[int, int, str]]:
    """Keeps one property class and re-merges it."""
    return merge_ranges(
        [(start, end, prop) for start, end, class_ in ranges if class_ == prop]
    )


def scalar_count(ranges: list[tuple[int, int, str]]) -> int:
    return sum(end - start + 1 for start, end, _ in ranges)


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


def emit_range_table(const: str, doc: str, ranges: list[tuple[int, int, str]]) -> str:
    lines = [f"/// {doc}", f"pub(crate) const {const}: &[(u32, u32)] = &["]
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
    parser.add_argument(
        "--grapheme-break",
        required=True,
        type=Path,
        help="path to UCD auxiliary/GraphemeBreakProperty.txt",
    )
    parser.add_argument(
        "--derived-core",
        required=True,
        type=Path,
        help="path to UCD DerivedCoreProperties.txt",
    )
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args(argv)

    versions = {
        "EastAsianWidth": ucd_version(
            args.east_asian_width, r"EastAsianWidth-(\d+\.\d+\.\d+)\.txt"
        ),
        "emoji-data": ucd_version(args.emoji_data, r"^# Version: (\d+\.\d+\.\d+)$"),
        "GraphemeBreakProperty": ucd_version(
            args.grapheme_break, r"GraphemeBreakProperty-(\d+\.\d+\.\d+)\.txt"
        ),
        "DerivedCoreProperties": ucd_version(
            args.derived_core, r"DerivedCoreProperties-(\d+\.\d+\.\d+)\.txt"
        ),
    }
    if len(set(versions.values())) != 1:
        detail = ", ".join(f"{name} {version}" for name, version in versions.items())
        raise SystemExit(f"UCD version mismatch: {detail}")
    ucd = next(iter(versions.values()))

    eaw = merge_ranges(
        parse_ranges(
            args.east_asian_width, set(EAW_CLASSES) - {"N"}, "East_Asian_Width"
        )
    )
    extended = merge_ranges(
        parse_ranges(args.emoji_data, {EMOJI_PROPERTY}, EMOJI_PROPERTY)
    )
    gcb = merge_ranges(
        parse_ranges(args.grapheme_break, set(GCB_PROPERTIES), "Grapheme_Cluster_Break")
    )
    extend = select(gcb, "Extend")
    spacing_mark = select(gcb, "SpacingMark")
    prepend = select(gcb, "Prepend")
    control = select(gcb, "Control")
    incb = merge_ranges(parse_incb(args.derived_core))
    incb_linker = select(incb, "Linker")
    incb_consonant = select(incb, "Consonant")
    incb_extend = select(incb, "Extend")

    digests = {
        "eaw": sha256(args.east_asian_width),
        "emoji": sha256(args.emoji_data),
        "grapheme_break": sha256(args.grapheme_break),
        "derived_core": sha256(args.derived_core),
    }

    counts = {
        "GRAPHEME_EXTEND": scalar_count(extend),
        "SPACING_MARK": scalar_count(spacing_mark),
        "PREPEND": scalar_count(prepend),
        "CONTROL": scalar_count(control),
        "INCB_LINKER": scalar_count(incb_linker),
        "INCB_CONSONANT": scalar_count(incb_consonant),
        "INCB_EXTEND": scalar_count(incb_extend),
    }

    header = f'''//! @generated by `scripts/generate-unicode-tables.py`; do not edit.
//!
//! Unicode Character Database tables for cell-width and grapheme-segmentation
//! decisions (UCD {ucd}). Generated data is committed: builds and tests never
//! fetch the network, and the generator takes the pinned UCD snapshots as
//! explicit inputs.
//!
//! License: Unicode Character Database {ucd}, © 2026 Unicode®, Inc. Unicode
//! and the Unicode Logo are registered trademarks of Unicode, Inc. in the
//! U.S. and other countries. Distributed under the Unicode License v3
//! (<https://www.unicode.org/license.txt>); the tables below are unmodified
//! property derivations of the cited snapshots.
//!
//! Provenance:
//!
//! - `EastAsianWidth.txt` {ucd} sha256 `{digests["eaw"]}`
//! - `emoji-data.txt` {ucd} sha256 `{digests["emoji"]}`
//! - `auxiliary/GraphemeBreakProperty.txt` {ucd} sha256 `{digests["grapheme_break"]}`
//! - `DerivedCoreProperties.txt` {ucd} sha256 `{digests["derived_core"]}`
//!
//! Regenerate with:
//!
//! ```text
//! python3 scripts/generate-unicode-tables.py \\
//!     --east-asian-width EastAsianWidth-{ucd}.txt \\
//!     --emoji-data emoji-data-{ucd}.txt \\
//!     --grapheme-break GraphemeBreakProperty-{ucd}.txt \\
//!     --derived-core DerivedCoreProperties-{ucd}.txt \\
//!     --output crates/bitty-term-state/src/unicode_tables.rs
//! ```
//!
//! `East_Asian_Width` covers W/F (two cells), A (policy-dependent), and
//! N/Na/H (one cell); only non-N ranges are stored, so the table stays
//! compact while the UAX #11 `@missing` default supplies the rest. The
//! grapheme tables are exhaustive for their classes: no curated subset and
//! no `@missing` gaps.

/// UCD version every generated table was produced from.
#[cfg(test)]
pub(crate) const UCD_VERSION: &str = "{ucd}";

/// SHA-256 of the `EastAsianWidth.txt` snapshot the EAW table came from.
#[cfg(test)]
pub(crate) const EAST_ASIAN_WIDTH_SHA256: &str =
    "{digests["eaw"]}";

/// SHA-256 of the `emoji-data.txt` snapshot the pictographic table came from.
#[cfg(test)]
pub(crate) const EMOJI_DATA_SHA256: &str =
    "{digests["emoji"]}";

/// SHA-256 of the `GraphemeBreakProperty.txt` snapshot the cluster tables
/// came from.
#[cfg(test)]
pub(crate) const GRAPHEME_BREAK_SHA256: &str =
    "{digests["grapheme_break"]}";

/// SHA-256 of the `DerivedCoreProperties.txt` snapshot the `InCB` tables
/// came from.
#[cfg(test)]
pub(crate) const DERIVED_CORE_SHA256: &str =
    "{digests["derived_core"]}";

/// Pinned scalar counts of the generated grapheme tables.
///
/// A UCD bump that changes any count fails `pinned_scalar_counts_hold`
/// until the generator output and this pin are reviewed together.
#[cfg(test)]
pub(crate) mod counts {{
    pub(crate) const GRAPHEME_EXTEND: usize = {counts["GRAPHEME_EXTEND"]};
    pub(crate) const SPACING_MARK: usize = {counts["SPACING_MARK"]};
    pub(crate) const PREPEND: usize = {counts["PREPEND"]};
    pub(crate) const CONTROL: usize = {counts["CONTROL"]};
    pub(crate) const INCB_LINKER: usize = {counts["INCB_LINKER"]};
    pub(crate) const INCB_CONSONANT: usize = {counts["INCB_CONSONANT"]};
    pub(crate) const INCB_EXTEND: usize = {counts["INCB_EXTEND"]};
}}

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
    in_ranges(EXTENDED_PICTOGRAPHIC_RANGES, cp)
}}

{
        emit_range_table(
            "GRAPHEME_EXTEND_RANGES",
            "`Grapheme_Cluster_Break=Extend` ranges (UAX #29 `GB9`), sorted and\n/// non-overlapping.",
            extend,
        )
    }

/// Whether `cp` is `Grapheme_Cluster_Break=Extend` (UAX #29 `GB9`).
#[must_use]
pub(crate) fn grapheme_extend(cp: u32) -> bool {{
    in_ranges(GRAPHEME_EXTEND_RANGES, cp)
}}

{
        emit_range_table(
            "SPACING_MARK_RANGES",
            "`Grapheme_Cluster_Break=SpacingMark` ranges (UAX #29 `GB9a`), sorted\n/// and non-overlapping.",
            spacing_mark,
        )
    }

/// Whether `cp` is `Grapheme_Cluster_Break=SpacingMark` (UAX #29 `GB9a`).
#[must_use]
pub(crate) fn spacing_mark(cp: u32) -> bool {{
    in_ranges(SPACING_MARK_RANGES, cp)
}}

{
        emit_range_table(
            "PREPEND_RANGES",
            "`Grapheme_Cluster_Break=Prepend` ranges (UAX #29 `GB9b`), sorted and\n/// non-overlapping. Every scalar here is also zero-width in the cell\n/// tables ([`crate::cell::is_zero_width`] reads this same table).",
            prepend,
        )
    }

/// Whether `cp` is `Grapheme_Cluster_Break=Prepend` (UAX #29 `GB9b`).
#[must_use]
pub(crate) fn prepend(cp: u32) -> bool {{
    in_ranges(PREPEND_RANGES, cp)
}}

{
        emit_range_table(
            "CONTROL_RANGES",
            "`Grapheme_Cluster_Break=Control` ranges (UAX #29 `GB4`/`GB5`),\n/// sorted and non-overlapping. `CR`, `LF`, and `ZWJ` are separate classes\n/// handled algorithmically in `grapheme.rs`.",
            control,
        )
    }

/// Whether `cp` is `Grapheme_Cluster_Break=Control` (UAX #29 `GB4`/`GB5`).
#[must_use]
pub(crate) fn control(cp: u32) -> bool {{
    in_ranges(CONTROL_RANGES, cp)
}}

{
        emit_range_table(
            "INCB_LINKER_RANGES",
            "`Indic_Conjunct_Break=Linker` ranges (UAX #29 `GB9c`), sorted and\n/// non-overlapping.",
            incb_linker,
        )
    }

/// Whether `cp` is `Indic_Conjunct_Break=Linker` (UAX #29 `GB9c`).
#[must_use]
pub(crate) fn incb_linker(cp: u32) -> bool {{
    in_ranges(INCB_LINKER_RANGES, cp)
}}

{
        emit_range_table(
            "INCB_CONSONANT_RANGES",
            "`Indic_Conjunct_Break=Consonant` ranges (UAX #29 `GB9c`), sorted and\n/// non-overlapping.",
            incb_consonant,
        )
    }

/// Whether `cp` is `Indic_Conjunct_Break=Consonant` (UAX #29 `GB9c`).
#[must_use]
pub(crate) fn incb_consonant(cp: u32) -> bool {{
    in_ranges(INCB_CONSONANT_RANGES, cp)
}}

{
        emit_range_table(
            "INCB_EXTEND_RANGES",
            "`Indic_Conjunct_Break=Extend` ranges (UAX #29 `GB9c`), sorted and\n/// non-overlapping.",
            incb_extend,
        )
    }

/// Whether `cp` is `Indic_Conjunct_Break=Extend` (UAX #29 `GB9c`).
#[must_use]
pub(crate) fn incb_extend(cp: u32) -> bool {{
    in_ranges(INCB_EXTEND_RANGES, cp)
}}

/// Binary search over one sorted, non-overlapping range table.
#[must_use]
fn in_ranges(ranges: &[(u32, u32)], cp: u32) -> bool {{
    let index = ranges.partition_point(|&(start, _)| start <= cp);
    index > 0 && cp <= ranges[index - 1].1
}}

#[cfg(test)]
mod tests {{
    use super::*;

    fn assert_sorted_disjoint(name: &str, ranges: &[(u32, u32)]) {{
        assert!(!ranges.is_empty(), "{{name}} table is empty");
        for pair in ranges.windows(2) {{
            assert!(
                pair[0].0 <= pair[0].1,
                "{{name}} range start after end: {{pair:?}}"
            );
            assert!(
                pair[0].1 < pair[1].0,
                "{{name}} unsorted or overlapping ranges: {{pair:?}}"
            );
        }}
    }}

    #[test]
    fn provenance_is_recorded() {{
        assert_eq!(UCD_VERSION, "{ucd}");
        for digest in [
            EAST_ASIAN_WIDTH_SHA256,
            EMOJI_DATA_SHA256,
            GRAPHEME_BREAK_SHA256,
            DERIVED_CORE_SHA256,
        ] {{
            assert_eq!(digest.len(), 64, "not a SHA-256 digest: {{digest}}");
        }}
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
        assert_sorted_disjoint("extended pictographic", EXTENDED_PICTOGRAPHIC_RANGES);
    }}

    #[test]
    fn grapheme_ranges_are_sorted_and_disjoint() {{
        assert_sorted_disjoint("Grapheme_Extend", GRAPHEME_EXTEND_RANGES);
        assert_sorted_disjoint("SpacingMark", SPACING_MARK_RANGES);
        assert_sorted_disjoint("Prepend", PREPEND_RANGES);
        assert_sorted_disjoint("Control", CONTROL_RANGES);
        assert_sorted_disjoint("InCB Linker", INCB_LINKER_RANGES);
        assert_sorted_disjoint("InCB Consonant", INCB_CONSONANT_RANGES);
        assert_sorted_disjoint("InCB Extend", INCB_EXTEND_RANGES);
    }}

    #[test]
    fn pinned_scalar_counts_hold() {{
        let count = |ranges: &[(u32, u32)]| -> usize {{
            ranges.iter().map(|&(start, end)| (end - start + 1) as usize).sum()
        }};
        assert_eq!(count(GRAPHEME_EXTEND_RANGES), counts::GRAPHEME_EXTEND);
        assert_eq!(count(SPACING_MARK_RANGES), counts::SPACING_MARK);
        assert_eq!(count(PREPEND_RANGES), counts::PREPEND);
        assert_eq!(count(CONTROL_RANGES), counts::CONTROL);
        assert_eq!(count(INCB_LINKER_RANGES), counts::INCB_LINKER);
        assert_eq!(count(INCB_CONSONANT_RANGES), counts::INCB_CONSONANT);
        assert_eq!(count(INCB_EXTEND_RANGES), counts::INCB_EXTEND);
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
        // UAX #29 classes spot-checked against the pinned snapshots.
        assert!(grapheme_extend(0x0301));
        assert!(grapheme_extend(0x200C));
        assert!(grapheme_extend(0x1F3FB));
        assert!(!grapheme_extend(0x200D));
        assert!(spacing_mark(0x093F));
        assert!(spacing_mark(0x0F3E));
        assert!(!spacing_mark(0x0301));
        assert!(prepend(0x0600));
        assert!(prepend(0x11A84));
        assert!(!prepend(0x0041));
        assert!(control(0x00AD));
        assert!(control(0x180E));
        assert!(control(0xE01F0));
        assert!(!control(0x000A));
        assert!(!control(0x0041));
        assert!(incb_linker(0x094D));
        assert!(!incb_linker(0x200C));
        assert!(incb_consonant(0x0915));
        assert!(!incb_consonant(0x0967));
        assert!(incb_extend(0x200D));
        assert!(incb_extend(0x0300));
    }}
}}
'''

    args.output.write_text(header, encoding="utf-8")
    # The generated Rust is committed as-is and CI's `cargo fmt --check` gate
    # runs over it, so format it here: regeneration is then byte-identical
    # without a manual `cargo fmt` step. `rustfmt` ships with the pinned
    # toolchain (see `rust-toolchain.toml`).
    formatted = subprocess.run(
        ["rustfmt", "--edition", "2024", str(args.output)],
        capture_output=True,
        text=True,
        check=False,
    )
    if formatted.returncode != 0:
        raise SystemExit(f"rustfmt failed on {args.output}: {formatted.stderr.strip()}")
    print(
        f"wrote {args.output} (UCD {ucd}: {len(eaw)} EAW ranges, "
        f"{len(extended)} Extended_Pictographic ranges, "
        f"{len(extend)} Extend, {len(spacing_mark)} SpacingMark, "
        f"{len(prepend)} Prepend, {len(control)} Control, "
        f"{len(incb_linker)} Linker, "
        f"{len(incb_consonant)} InCB Consonant, {len(incb_extend)} InCB Extend ranges)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
