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

impl ResolvedPrimary {
    /// True when the host font database has one of the attempted faces.
    ///
    /// This is the only honest answer to "is the configured family really
    /// installed?": `SwashSingle::load_font` succeeds for an absent family by
    /// loading a monospaced stand-in (`generic_monospace_face`), so a loaded
    /// face says nothing about which family it came from. `false` here means
    /// the renderer paints with that stand-in, so a caller asserting a
    /// property of the configured face (programming ligatures, advance
    /// widths, Nerd icons) has nothing to assert on this host.
    #[must_use]
    pub fn installed(&self) -> bool {
        self.attempts.iter().any(|attempt| attempt.present)
    }
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

/// Ordering key for the deterministic monospaced pick: primary family name,
/// then PostScript name, so the choice is stable across discovery order and
/// the persistent index.
fn face_order(face: &fontdb::FaceInfo) -> (&str, &str) {
    let family = face
        .families
        .first()
        .map_or("", |(family, _)| family.as_str());
    (family, face.post_script_name.as_str())
}

/// Resolves the generic monospace face `db` can actually hand to the shaper.
///
/// `Family::Monospace` is a *name* lookup in `fontdb`: it matches the alias
/// string the database carries, which is fontdb's platform-neutral default
/// (`Courier New`) until fontconfig discovery overlays the host alias. A host
/// whose alias is unset, stale, or names a face that is not installed — a
/// container image with fonts but no fontconfig configuration, for example —
/// therefore misses on the generic query while holding perfectly usable
/// monospaced faces. Blanking every cell there is a product defect, so the
/// miss falls through to the installed monospaced faces (`face_order` picks
/// the first in family/PostScript order). The embedded symbols face is never
/// a text primary: its families are icon ranges, not text.
///
/// Bounded: one `fontdb` query, and a single face scan only when that misses.
#[must_use]
pub(crate) fn generic_monospace_face(db: &Database) -> Option<fontdb::ID> {
    let generic = [Family::Monospace];
    let alias = Query {
        families: &generic,
        weight: Weight::NORMAL,
        style: Style::Normal,
        stretch: Stretch::Normal,
    };
    db.query(&alias).or_else(|| {
        db.faces()
            .filter(|face| face.monospaced)
            .filter(|face| {
                !face
                    .families
                    .iter()
                    .any(|(family, _)| family.eq_ignore_ascii_case(EMBEDDED_SYMBOLS_FAMILY))
            })
            .min_by(|left, right| face_order(left).cmp(&face_order(right)))
            .map(|face| face.id)
    })
}

/// True when `db` resolves the family the config asked for.
///
/// Deliberately not derived from [`ResolvedPrimary::attempts`]: the resolver
/// returns at the first installed candidate, so on a host where a preference
/// face wins (SF Mono on macOS, Cascadia Mono on Windows 11) the configured
/// family — Menlo, Consolas — never appears in that truncated list even
/// though the host ships it.
fn configured_family_present(db: &Database, resolution: &ResolvedPrimary) -> bool {
    family_present(db, &resolution.configured)
}

/// True when `db` resolves `family` by name.
///
/// Uses the same query shape as the rasterizer's face loading (normal weight
/// and slant), so a present result means `load_font` would find a face. The
/// CSS generic `monospace` is resolved through the same substitution
/// `SwashSingle::load_font` performs (`generic_monospace_face`) — so the
/// documented chain entry is not reported as missing on a host whose
/// fontconfig alias points at nothing while monospaced faces are installed.
#[must_use]
pub fn family_present(db: &Database, family: &str) -> bool {
    let generic = matches!(
        family.trim().to_ascii_lowercase().as_str(),
        "monospace" | "mono" | "fixed"
    );
    if generic {
        return generic_monospace_face(db).is_some();
    }
    let families = [Family::Name(family)];
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
    /// True when the platform preference list picked another installed face
    /// than the configured one (SF Mono for Menlo, Cascadia Mono for Consolas).
    /// A generic monospace stand-in for an absent family is not a
    /// substitution.
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
    // The configured family, asked of the database directly: `attempts` stops
    // at the winning candidate, so a preference face (SF Mono, Cascadia Mono)
    // hides the configured one (Menlo, Consolas) from that list. Inferring
    // presence from `attempts` made doctor tell macOS/Windows users to install
    // the font their OS ships.
    let primary_present = configured_family_present(&db, &resolution);
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
    // `substituted` means the platform-preference list chose another installed
    // face (SF Mono for Menlo, Cascadia Mono for Consolas). Deriving it from
    // "the loaded face differs from the configured one" mislabels a generic
    // monospace stand-in for an absent explicit family as a platform default;
    // `resolution` already tracks the real case.
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
        assert!(resolution.installed());
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
        assert!(resolution.installed());
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
        assert!(
            !resolution.installed(),
            "no candidate is installed, so the renderer will paint a stand-in"
        );
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

    /// Test-only database with `faces` = `(family, monospaced)` and fontdb's
    /// platform-neutral generic aliases (no fontconfig overlay), i.e. the
    /// `monospace` alias names no installed face. That is the CI container's
    /// condition, reproduced without touching the host font configuration.
    fn synthetic_db(faces: &[(&str, bool)]) -> Database {
        let index = crate::font_index::FontIndex {
            faces: faces
                .iter()
                .enumerate()
                .map(
                    |(index, (family, monospaced))| crate::font_index::IndexFace {
                        path: std::path::PathBuf::from(format!("/nonexistent/{index}.ttf")),
                        face_index: 0,
                        families: vec![(*family).to_string()],
                        post_script_name: format!("{}-Regular", family.replace(' ', "")),
                        style: 0,
                        weight: 400,
                        stretch: 5,
                        monospaced: *monospaced,
                        file_size: 0,
                        mtime_secs: -1,
                        mtime_nanos: 0,
                    },
                )
                .collect(),
            generic: crate::font_index::GenericFamilies::default(),
        };
        crate::font_index::database_from_index(&index)
    }

    #[test]
    fn configured_primary_is_present_when_a_preferred_face_wins() {
        // The macOS / Windows 11 shape: the preference face (SF Mono,
        // Cascadia Mono) is installed, so `resolve_with` returns at the first
        // candidate and the configured family (Menlo, Consolas) never enters
        // `attempts` — although the host ships it. Doctor must not tell that
        // user to install the font their OS already has.
        let db = synthetic_db(&[("SF Mono", true), ("Menlo", true)]);
        let resolution = ResolvedPrimary {
            configured: "Menlo".to_string(),
            family: "SF Mono".to_string(),
            substituted: true,
            attempts: vec![FaceAttempt {
                family: "SF Mono".to_string(),
                present: true,
            }],
        };
        assert!(
            resolution
                .attempts
                .iter()
                .all(|attempt| !attempt.family.eq_ignore_ascii_case("Menlo")),
            "precondition: the winning candidate truncates the attempt list"
        );
        assert!(
            configured_family_present(&db, &resolution),
            "the configured family is installed even though it is not attempted"
        );
    }

    #[test]
    fn configured_primary_stays_absent_when_only_a_stand_in_loads() {
        // The other direction: a stand-in face loading says nothing about the
        // configured family, so presence must still be false and doctor must
        // still ask for the missing font.
        let db = synthetic_db(&[("DejaVu Sans Mono", true)]);
        let resolution = ResolvedPrimary {
            configured: "No Such Family XYZ".to_string(),
            family: "No Such Family XYZ".to_string(),
            substituted: false,
            attempts: vec![FaceAttempt {
                family: "No Such Family XYZ".to_string(),
                present: false,
            }],
        };
        assert!(!configured_family_present(&db, &resolution));
    }

    #[test]
    fn generic_monospace_reports_an_installed_face_when_the_alias_is_missing() {
        // Issue #2931 CI: the runner has DejaVu Sans Mono installed but no
        // fontconfig alias, so `Family::Monospace` (fontdb's platform default
        // "Courier New") matched nothing and the whole chain reported
        // `monospace absent` while a usable monospaced face sat in the
        // database. Reporting it missing is also what starved the primary
        // load, so both must agree on the substitution.
        let db = synthetic_db(&[("Proportional Serif", false), ("DejaVu Sans Mono", true)]);
        assert_eq!(db.family_name(&Family::Monospace), "Courier New");
        assert!(!family_present(&db, "Courier New"));
        assert!(family_present(&db, "monospace"));
        assert!(family_present(&db, "mono"));
        let face = generic_monospace_face(&db).expect("installed monospaced face must resolve");
        assert_eq!(
            db.face(face).map(|face| face.families[0].0.clone()),
            Some("DejaVu Sans Mono".to_string())
        );
    }

    #[test]
    fn generic_monospace_never_picks_the_embedded_symbols_face() {
        // The embedded face is registered in the same database and is built
        // monospaced here (the shipped flag is not what guards this): picking
        // it as a text primary would paint every cell as an icon, so it stays
        // out of the generic text substitution.
        let db = synthetic_db(&[(EMBEDDED_SYMBOLS_FAMILY, true), ("Adwaita Mono", true)]);
        assert_eq!(db.family_name(&Family::Monospace), "Courier New");
        let face = generic_monospace_face(&db).expect("a text monospaced face must resolve");
        assert_eq!(
            db.face(face).map(|face| face.families[0].0.clone()),
            Some("Adwaita Mono".to_string())
        );
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

    #[test]
    fn probe_resolves_a_stand_in_when_the_configured_family_is_absent() {
        // The CI condition in the small: the configured family is not
        // installed, so the primary load must still land on a real face
        // (the generic monospace substitution) instead of reporting no face
        // and rendering blank cells.
        if SwashSingle::new().is_err() {
            eprintln!("skipped: no system fonts on this host");
            return;
        }
        let absent = "No Such Family XYZ";
        let probe = probe_chain(&FontConfig {
            family: absent.to_string(),
            ..Default::default()
        });
        assert!(!probe.primary_present, "{probe:?}");
        let resolved = probe
            .resolved
            .as_deref()
            .expect("a fonted host must still load a stand-in primary");
        assert_ne!(resolved, absent, "{probe:?}");
        assert!(
            !probe.substituted,
            "a generic stand-in is not a platform-default substitution: {probe:?}"
        );
        assert!(!probe.attempts.is_empty(), "{probe:?}");
        assert!(probe.embedded_present, "{probe:?}");
        assert!(probe.pua_covered, "{probe:?}");
        assert!(probe.uncovered.probed > 0, "{probe:?}");
    }
}
