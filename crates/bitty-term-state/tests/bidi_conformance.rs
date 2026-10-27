//! UCD `BidiCharacterTest` conformance for the UAX #9 facade (CTX-1612).
//!
//! The checked-in fixture is a deterministic subset of
//! `BidiCharacterTest-18.0.0.txt`; see its header for the exact filter and
//! counts. The harness compares the paragraph level, the resolved level of
//! every scalar not removed by X9, and the visual order (rule L2).
//!
//! The fixture is bounded so the default suite stays offline and fast. A
//! full-file run is deliberately out of S0; the subset keeps every case
//! that exercises an explicit formatting scalar and a deterministic stride
//! over the rest (3000 cases).

use bitty_term_state::{ParaDirection, resolve_text};

const FIXTURE: &str = include_str!("data/BidiCharacterTest-subset-18.0.0.txt");

/// Total source cases in `BidiCharacterTest-18.0.0.txt`.
const SOURCE_CASES: usize = 91_707;
/// Cases dropped for having more than 12 scalars.
const SKIPPED_SCALAR_COUNT: usize = 37;
/// Cases dropped for containing a scalar unassigned in UCD 16.0.0 (the
/// bidi-class tables `unicode-bidi` 0.3.18 ships). A dependency bump that
/// changes this number must update the fixture header and this constant.
const SKIPPED_UCD16_UNASSIGNED: usize = 0;
/// Cases after the two filters.
const ELIGIBLE_CASES: usize = 91_670;
/// Cases checked in.
const SELECTED_CASES: usize = 3000;
/// Selected cases that use an explicit bidi formatting scalar.
const SELECTED_WITH_CONTROLS: usize = 97;
/// Eligible cases dropped by the deterministic stride (no case removed:
/// every one is either selected or sampled out, none is filtered here).
const SAMPLED_OUT_CASES: usize = ELIGIBLE_CASES - SELECTED_CASES;

struct Case {
    scalars: Vec<char>,
    direction: ParaDirection,
    paragraph_level: u8,
    levels: Vec<Option<u8>>,
    visual: Vec<usize>,
}

fn parse_case(line: &str) -> Option<Case> {
    let data = line.split('#').next()?.trim();
    if data.is_empty() {
        return None;
    }
    let mut fields = data.split(';').map(str::trim);
    let scalars = fields
        .next()?
        .split_whitespace()
        .map(|hex| char::from_u32(u32::from_str_radix(hex, 16).ok()?))
        .collect::<Option<Vec<_>>>()?;
    let direction = match fields.next()? {
        "0" => ParaDirection::Ltr,
        "1" => ParaDirection::Rtl,
        "2" => ParaDirection::Auto,
        other => panic!("unknown direction {other}"),
    };
    let paragraph_level = fields.next()?.parse::<u8>().ok()?;
    let levels = fields
        .next()?
        .split_whitespace()
        .map(|token| {
            if token == "x" {
                Some(None)
            } else {
                token.parse::<u8>().ok().map(Some)
            }
        })
        .collect::<Option<Vec<_>>>()?;
    let visual = fields
        .next()?
        .split_whitespace()
        .map(|token| token.parse::<usize>().ok())
        .collect::<Option<Vec<_>>>()?;
    Some(Case {
        scalars,
        direction,
        paragraph_level,
        levels,
        visual,
    })
}

#[test]
fn fixture_counts_match_the_recorded_filter() {
    let mut total = 0usize;
    let mut with_controls = 0usize;
    for line in FIXTURE.lines() {
        let Some(case) = parse_case(line) else {
            continue;
        };
        total += 1;
        if case
            .scalars
            .iter()
            .any(|scalar| bitty_term_state::is_bidi_format_scalar(*scalar))
        {
            with_controls += 1;
        }
    }
    assert_eq!(total, SELECTED_CASES, "fixture case count");
    assert_eq!(
        with_controls, SELECTED_WITH_CONTROLS,
        "explicit-formatting case count"
    );
    assert_eq!(
        ELIGIBLE_CASES,
        SELECTED_CASES + SAMPLED_OUT_CASES,
        "eligible cases cover selected + sampled-out"
    );
    assert_eq!(
        SOURCE_CASES,
        ELIGIBLE_CASES + SKIPPED_SCALAR_COUNT + SKIPPED_UCD16_UNASSIGNED
    );
}

#[test]
fn ucd_bidi_character_test_subset_matches() {
    // The crate's bidi tables and the workspace's generated tables are on
    // different UCD versions; the fixture filters against the older one and
    // records the exact skip count so a dependency bump is visible.
    assert_eq!(bitty_term_state::UNICODE_BIDI_VERSION, (16, 0, 0));
    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for line in FIXTURE.lines() {
        let Some(case) = parse_case(line) else {
            continue;
        };
        checked += 1;
        let text: String = case.scalars.iter().collect();
        let resolved = resolve_text(&text, case.direction)
            .unwrap_or_else(|| panic!("no resolution for {text:?}"));
        if resolved.paragraph_level != case.paragraph_level {
            failures.push(format!(
                "para level for {text:?}: got {}, want {}",
                resolved.paragraph_level, case.paragraph_level
            ));
            continue;
        }
        for (index, expected) in case.levels.iter().enumerate() {
            let Some(expected) = expected else {
                continue;
            };
            let got = resolved.levels.get(index).copied();
            if got != Some(*expected) {
                failures.push(format!(
                    "level {index} for {text:?}: got {got:?}, want {expected}"
                ));
                break;
            }
        }
        if resolved.visual != case.visual {
            failures.push(format!(
                "visual order for {text:?}: got {:?}, want {:?}",
                resolved.visual, case.visual
            ));
        }
    }
    assert_eq!(checked, SELECTED_CASES);
    assert!(
        failures.is_empty(),
        "{} conformance failures, first 5: {:#?}",
        failures.len(),
        failures.iter().take(5).collect::<Vec<_>>()
    );
}
