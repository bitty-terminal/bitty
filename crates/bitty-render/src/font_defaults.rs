//! Platform default primary resolution and chain coverage probing
//! (issue #2931).
//!
//! The documented chain (`bitty_config::types::FONT_FALLBACK_CHAIN`) is a
//! fixed list, but the *primary* face has to adapt to the host: Windows
//! prefers Cascadia Mono when installed and falls back to Consolas, macOS
//! prefers SF Mono when installed and falls back to Menlo. The configured
//! default (`bitty_config::types::DEFAULT_FONT_FAMILY`) is the guaranteed
//! face; [`resolve_primary`] returns the first installed candidate while the
//! configured family equals that default. Any other configured family — a
//! `terminal.font.family` value or a `--font-family` override — is returned
//! verbatim, so config stays authoritative.
//!
//! [`probe_chain`] is the doctor-facing view of the same machinery: the
//! configured family, every attempted face, the face actually loaded, whether
//! the embedded symbols face covers the PUA icon range, and a bounded count
//! of probed scalars that fell through the whole chain.
//!
//! # Bounds and safety
//!
//! Presence checks are in-memory `fontdb` queries; coverage probes rasterize
//! a bounded sample (`PROBE_CODEPOINTS`, at most [`MAX_PROBE_CODEPOINTS`])
//! through the real [`FallbackRasterizer`] chain, so the work is bounded by
//! the chain depth and never walks the host font set. No host input can panic
//! this module: failures become `present = false` / `covered = false`.

use bitty_config::types::{
    DEFAULT_FONT_FAMILY, DEFAULT_FONT_FAMILY_CANDIDATES, EMBEDDED_SYMBOLS_FAMILY, FontConfig,
};
use fontdb::{Database, Family, Query, Stretch, Style, Weight};

use crate::fallback::FallbackRasterizer;
use crate::glyph::{FontQuery, FontStyle, GlyphRasterizer, RasterKey};
use crate::shaped::SwashSingle;

/// Maximum scalars one chain probe covers.
pub const MAX_PROBE_CODEPOINTS: usize = 32;

/// Maximum uncovered example scalars reported.
pub const MAX_PROBE_EXAMPLES: usize = 3;

/// Representative PUA icons the embedded face must map: the range start, a
/// Powerline separator, and a Font Awesome icon. `U+F8FF` is deliberately
/// absent — Nerd Fonts 3.5.1 ships no glyph for it (see `embedded`).
pub const PUA_PROBE_ICONS: &[char] = &['\u{E000}', '\u{E0B0}', '\u{F017}'];

/// Scalars a chain probe checks, in order.
///
/// Deliberately small and representative: the PUA icon bounds and common
/// icons, the box/block/braille slices the chain exists for, and a handful of
/// scalars no stock host face is guaranteed to cover (the counter's
/// evidence). Doctor reports the count as "of N probed", never as a full
/// Unicode census.
pub const PROBE_CODEPOINTS: &[char] = &[
    '\u{E000}',
    '\u{E0B0}',
    '\u{E800}',
    '\u{F017}',
    '\u{F8FF}',
    '\u{2500}',
    '\u{2502}',
    '\u{2588}',
    '\u{2591}',
    '\u{2800}',
    '\u{28FF}',
    '\u{2603}',
    '\u{2764}',
    '\u{1F600}',
    '\u{1F680}',
];

/// One face the resolver considered, with its availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaceAttempt {
    /// Family name as queried.
    pub family: String,
    /// True when the host font database resolves the family.
    pub present: bool,
}

/// Result of resolving the primary face for a configured family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPrimary {
    /// Family the config asked for (trimmed).
    pub configured: String,
    /// Family the renderer loads.
    pub family: String,
    /// True when `family` differs from `configured`.
    pub substituted: bool,
    /// Faces considered, in attempt order, up to and including the winner.
    pub attempts: Vec<FaceAttempt>,
}

/// Resolves the primary family against the process-wide font database.
///
/// See the module docs: preference resolution only applies while the
/// configured family equals `DEFAULT_FONT_FAMILY`.
#[must_use]
pub fn resolve_primary(configured: &str) -> ResolvedPrimary {
    let db = crate::shaped::system_db();
    resolve_primary_in(&db, configured)
}

/// Resolves the primary family against `db` (unit-testable).
#[must_use]
pub fn resolve_primary_in(db: &Database, configured: &str) -> ResolvedPrimary {
    resolve_with(configured, |family| family_present(db, family))
}

/// Resolves the primary family with an injected availability predicate.
///
/// The predicate is the seam that keeps the platform-preference tests
/// hermetic (no host font installation required): production passes
/// [`family_present`] over the shared database.
#[must_use]
pub fn resolve_with<F>(configured: &str, mut present: F) -> ResolvedPrimary
where
    F: FnMut(&str) -> bool,
{
    let configured = configured.trim();
    if !configured.eq_ignore_ascii_case(DEFAULT_FONT_FAMILY) {
        return ResolvedPrimary {
            configured: configured.to_string(),
            family: configured.to_string(),
            substituted: false,
            attempts: vec![FaceAttempt {
                family: configured.to_string(),
                present: present(configured),
            }],
        };
    }
    let mut attempts = Vec::with_capacity(DEFAULT_FONT_FAMILY_CANDIDATES.len());
    for candidate in DEFAULT_FONT_FAMILY_CANDIDATES {
        let available = present(candidate);
        attempts.push(FaceAttempt {
            family: (*candidate).to_string(),
            present: available,
        });
        if available {
            return ResolvedPrimary {
                configured: configured.to_string(),
                family: (*candidate).to_string(),
                substituted: !candidate.eq_ignore_ascii_case(configured),
                attempts,
            };
        }
    }
    // Nothing installed: keep the configured face so the failure surfaces as
    // `FontNotFound` exactly as before (the runtime then degrades).
    ResolvedPrimary {
        configured: configured.to_string(),
        family: configured.to_string(),
        substituted: false,
        attempts,
    }
}

/// True when `db` resolves `family` by name.
///
/// Uses the same query shape as the rasterizer's face loading (normal weight
/// and slant), so a present result means `load_font` would find a face. The
/// CSS generic `monospace` is resolved through `Family::Monospace` — the
/// same generic substitution `SwashSingle::load_font` performs — so the
/// documented chain entry is not reported as missing.
#[must_use]
pub fn family_present(db: &Database, family: &str) -> bool {
    let generic = matches!(
        family.trim().to_ascii_lowercase().as_str(),
        "monospace" | "mono" | "fixed"
    );
    let families = [if generic {
        Family::Monospace
    } else {
        Family::Name(family)
    }];
    let query = Query {
        families: &families,
        weight: Weight::NORMAL,
        style: Style::Normal,
        stretch: Stretch::Normal,
    };
    db.query(&query).is_some()
}

/// Bounded uncovered-scalar count with a few examples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncoveredReport {
    /// Number of probed scalars no face covered.
    pub total: usize,
    /// Up to [`MAX_PROBE_EXAMPLES`] example scalars, in probe order.
    pub examples: Vec<char>,
    /// Number of scalars probed (at most [`MAX_PROBE_CODEPOINTS`]).
    pub probed: usize,
}

/// Counts scalars `covers` rejects, keeping the first few examples.
///
/// Pure and bounded: at most [`MAX_PROBE_CODEPOINTS`] scalars are consulted
/// and at most [`MAX_PROBE_EXAMPLES`] examples are retained, so a hostile or
/// oversized candidate list cannot grow the report.
#[must_use]
pub fn count_uncovered<I, F>(candidates: I, mut covers: F) -> UncoveredReport
where
    I: IntoIterator<Item = char>,
    F: FnMut(char) -> bool,
{
    let mut report = UncoveredReport {
        total: 0,
        examples: Vec::new(),
        probed: 0,
    };
    for c in candidates.into_iter().take(MAX_PROBE_CODEPOINTS) {
        report.probed += 1;
        if covers(c) {
            continue;
        }
        report.total += 1;
        if report.examples.len() < MAX_PROBE_EXAMPLES {
            report.examples.push(c);
        }
    }
    report
}

/// Doctor-facing result of one chain probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainProbe {
    /// Configured primary family.
    pub configured: String,
    /// Face the chain actually loaded, or `None` when no primary resolved.
    pub resolved: Option<String>,
    /// True when the configured primary itself is installed.
    pub primary_present: bool,
    /// True when the loaded primary differs from the configured one.
    pub substituted: bool,
    /// Faces considered in order (preference candidates, then chain tails).
    pub attempts: Vec<FaceAttempt>,
    /// Family name of the embedded symbols face.
    pub embedded_family: String,
    /// True when the embedded face is registered in the font database.
    pub embedded_present: bool,
    /// True when the embedded face maps the representative PUA icons
    /// (`U+E000`, `U+E0B0`, `U+F017`).
    pub pua_covered: bool,
    /// Bounded uncovered-scalar count for the probe sample.
    pub uncovered: UncoveredReport,
}

/// Probes the effective chain for `config`.
///
/// Loads the resolved primary and walks the real
/// [`FallbackRasterizer`] chain over [`PROBE_CODEPOINTS`]. A primary that
/// cannot load yields `resolved = None` with the attempt list intact; every
/// other failure degrades to `covered = false` (a counted uncovered scalar),
/// never an error.
#[must_use]
pub fn probe_chain(config: &FontConfig) -> ChainProbe {
    let db = crate::shaped::system_db();
    let resolution = resolve_primary_in(&db, &config.family);
    let primary_present = resolution
        .attempts
        .iter()
        .find(|attempt| attempt.family.eq_ignore_ascii_case(&resolution.configured))
        .map(|attempt| attempt.present)
        .unwrap_or(false);
    let mut attempts = resolution.attempts.clone();
    for family in config.fallback_tails() {
        if attempts
            .iter()
            .any(|attempt| attempt.family.eq_ignore_ascii_case(&family))
        {
            continue;
        }
        let present = family_present(&db, &family);
        attempts.push(FaceAttempt { family, present });
    }
    let embedded_present = family_present(&db, EMBEDDED_SYMBOLS_FAMILY);
    let mut probe = ChainProbe {
        configured: resolution.configured.clone(),
        resolved: None,
        primary_present,
        substituted: false,
        attempts,
        embedded_family: EMBEDDED_SYMBOLS_FAMILY.to_string(),
        embedded_present,
        pua_covered: PUA_PROBE_ICONS
            .iter()
            .all(|c| crate::embedded::embedded_face_covers(*c)),
        uncovered: UncoveredReport {
            total: 0,
            examples: Vec::new(),
            probed: 0,
        },
    };
    let Ok(shaped) = SwashSingle::new() else {
        return probe;
    };
    let mut raster = FallbackRasterizer::with_config(shaped, config);
    let query = FontQuery {
        family: resolution.family.clone(),
        style: FontStyle::Normal,
        point_size: config.size,
    };
    let Ok(primary) = raster.load_font(&query) else {
        return probe;
    };
    // The face actually used, not the queried name: a missing family can
    // resolve through the generic monospace substitution, and the platform
    // default resolves to a preference candidate.
    probe.resolved = Some(
        raster
            .inner()
            .loaded_face_family(primary)
            .unwrap_or_else(|| resolution.family.clone()),
    );
    probe.substituted = resolution.substituted;
    let point_size = config.size;
    probe.uncovered = count_uncovered(PROBE_CODEPOINTS.iter().copied(), |c| {
        let Ok(key) = RasterKey::new(c, primary, point_size) else {
            return false;
        };
        match raster.resolve(key) {
            Ok(glyph) => glyph.covered,
            Err(_) => false,
        }
    });
    probe
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedded::PUA_LAST;

    /// The preferred face for this platform (first candidate).
    fn preferred_face() -> &'static str {
        DEFAULT_FONT_FAMILY_CANDIDATES[0]
    }

    #[test]
    fn platform_default_resolves_to_the_preferred_installed_face() {
        // Issue #2931: Cascadia Mono on Windows, SF Mono on macOS, the
        // Nerd-patched JetBrains face on Linux — when installed.
        let resolution = resolve_with(DEFAULT_FONT_FAMILY, |family| family == preferred_face());
        assert_eq!(resolution.family, preferred_face());
        assert_eq!(
            resolution.substituted,
            preferred_face() != DEFAULT_FONT_FAMILY
        );
        assert_eq!(resolution.attempts.len(), 1);
        assert!(resolution.attempts[0].present);
    }

    #[test]
    fn platform_default_falls_back_to_the_guaranteed_face() {
        // Only the guaranteed candidate (the configured default) is
        // installed: Windows without Cascadia Mono loads Consolas, macOS
        // without SF Mono loads Menlo, Linux without the Nerd-patched face
        // falls through the unpatched JetBrains Mono entry first.
        let resolution = resolve_with(DEFAULT_FONT_FAMILY, |family| family == DEFAULT_FONT_FAMILY);
        assert_eq!(resolution.family, DEFAULT_FONT_FAMILY);
        assert!(!resolution.substituted);
        let expected_attempts = DEFAULT_FONT_FAMILY_CANDIDATES
            .iter()
            .position(|candidate| *candidate == DEFAULT_FONT_FAMILY)
            .unwrap()
            + 1;
        assert_eq!(resolution.attempts.len(), expected_attempts);
        assert!(
            resolution
                .attempts
                .iter()
                .all(|attempt| { attempt.present == (attempt.family == DEFAULT_FONT_FAMILY) })
        );
    }

    #[test]
    fn explicit_family_is_never_substituted() {
        // `terminal.font.family` / `--font-family` stay authoritative: even a
        // face that is installed must not be swapped for a platform default.
        let resolution = resolve_with("  Maple Mono  ", |_| true);
        assert_eq!(resolution.configured, "Maple Mono");
        assert_eq!(resolution.family, "Maple Mono");
        assert!(!resolution.substituted);
        assert_eq!(
            resolution.attempts,
            vec![FaceAttempt {
                family: "Maple Mono".to_string(),
                present: true,
            }]
        );
    }

    #[test]
    fn missing_everything_keeps_the_configured_default() {
        let resolution = resolve_with(DEFAULT_FONT_FAMILY, |_| false);
        assert_eq!(resolution.family, DEFAULT_FONT_FAMILY);
        assert!(!resolution.substituted);
        assert!(resolution.attempts.iter().all(|attempt| !attempt.present));
    }

    #[test]
    fn counter_counts_an_uncovered_codepoint() {
        let report = count_uncovered([PUA_LAST, '\u{E000}'], |c| c != PUA_LAST);
        assert_eq!(report.probed, 2);
        assert_eq!(report.total, 1);
        assert_eq!(report.examples, vec![PUA_LAST]);
    }

    #[test]
    fn counter_is_bounded() {
        let report = count_uncovered(std::iter::repeat_n('\u{FFFD}', 4096), |_| false);
        assert_eq!(report.probed, MAX_PROBE_CODEPOINTS);
        assert_eq!(report.total, MAX_PROBE_CODEPOINTS);
        assert_eq!(report.examples.len(), MAX_PROBE_EXAMPLES);
        assert!(report.examples.iter().all(|c| *c == '\u{FFFD}'));
    }

    #[test]
    fn probe_reports_the_resolved_chain_on_a_fonted_host() {
        // Host-dependent by nature (a fontless CI image skips), but the
        // contract is host-independent: a loaded primary names a resolved
        // face, the embedded face is registered, and the PUA start is
        // covered by it.
        if SwashSingle::new().is_err() {
            eprintln!("skipped: no system fonts on this host");
            return;
        }
        let probe = probe_chain(&FontConfig::default());
        assert!(probe.resolved.is_some(), "primary must resolve: {probe:?}");
        assert_eq!(probe.embedded_family, EMBEDDED_SYMBOLS_FAMILY);
        assert!(probe.embedded_present);
        assert!(probe.pua_covered);
        assert!(!probe.attempts.is_empty());
        assert!(probe.uncovered.probed > 0);
    }
}
