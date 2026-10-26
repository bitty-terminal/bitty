//! Build-time official plugin catalog.
//!
//! Core never fetches registry metadata at runtime. The official plugin set
//! (`registry/official/*.toml` plus each entry's pinned `plugins/<name>`
//! gitlink) is rendered into `crate::official_catalog` by
//! `just catalog-generate` and compiled into the binary. `bitty plugin
//! install <official-id>` resolves an exact id against this catalog, refuses
//! incompatible entries before any network access, then installs the pinned
//! repository revision and verifies the registry's raw manifest hash.
//!
//! The generated file is checked in and gated by `just catalog-check`; see
//! `scripts/generate-official-catalog.py`. The canonical source is the
//! `bitty-plugins` registry at [`SOURCE_REVISION`].
//!
//! # Resolution rules (implemented by the CLI)
//!
//! - Exact id match only (the catalog stores the registry's canonical id).
//! - Unknown ids fail with the bundled + official id list, never with a
//!   network lookup.
//! - Every entry is listed (including entries whose `compat.bitty` excludes
//!   the running host); incompatible entries are refused with the exact
//!   declared range before any fetch.
//! - Every generated entry carries an immutable 40-hex commit pin; there is
//!   no default-branch fallback.

pub use crate::official_catalog::{SOURCE_REPOSITORY, SOURCE_REVISION};

/// One official plugin entry embedded at build time.
///
/// Fields mirror the registry entry and its gitlink pin; the manifest hash is
/// the registry's transport-byte digest (`sha256:<64 hex>`), not the host's
/// canonical manifest hash. Install verifies the raw `bitty-plugin.toml`
/// bytes against it before the package pipeline runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfficialPlugin {
    /// Canonical registry id (`owner.name`).
    pub id: &'static str,
    /// Display name from the registry entry.
    pub name: &'static str,
    /// Clone URL (always `https://…git`, valid for the CLI git allowlist).
    pub repository: &'static str,
    /// Pinned 40-hex commit from the registry's `plugins/<name>` gitlink.
    pub pin: &'static str,
    /// Registry `manifest_hash`: `sha256:` plus the raw file digest.
    pub manifest_hash: &'static str,
    /// Declared `compat.bitty` range, when present.
    pub compat_bitty: Option<&'static str>,
    /// Declared `compatibility.sdk` range, when present.
    pub compat_plugin_api: Option<&'static str>,
    /// One-line registry description.
    pub description: &'static str,
    /// Declared license expression, when present.
    pub license: Option<&'static str>,
}

impl OfficialPlugin {
    /// Raw manifest digest without the registry's `sha256:` prefix.
    #[must_use]
    pub fn manifest_sha256(&self) -> &'static str {
        self.manifest_hash
            .strip_prefix("sha256:")
            .unwrap_or(self.manifest_hash)
    }

    /// Short pin for human output.
    #[must_use]
    pub fn short_pin(&self) -> &'static str {
        self.pin.get(..12).unwrap_or(self.pin)
    }
}

/// Every embedded official entry, sorted by id.
#[must_use]
pub fn official_plugins() -> &'static [OfficialPlugin] {
    crate::official_catalog::OFFICIAL_PLUGINS
}

/// Exact-id lookup (trimmed; case-sensitive like the bundled catalog).
#[must_use]
pub fn official_plugin_for(id: &str) -> Option<&'static OfficialPlugin> {
    let id = id.trim();
    official_plugins().iter().find(|entry| entry.id == id)
}

/// Sorted canonical ids (deterministic diagnostics).
#[must_use]
pub fn official_ids_sorted() -> Vec<&'static str> {
    let mut ids: Vec<&'static str> = official_plugins().iter().map(|entry| entry.id).collect();
    ids.sort_unstable();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_non_empty_sorted_and_unique() {
        let entries = official_plugins();
        assert!(!entries.is_empty(), "official catalog must not be empty");
        let ids = official_ids_sorted();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
        let unique: std::collections::BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "official ids must be unique");
    }

    #[test]
    fn every_official_id_resolves_exactly_once() {
        for entry in official_plugins() {
            let resolved = official_plugin_for(entry.id).expect("id resolves");
            assert_eq!(resolved.id, entry.id);
            assert_eq!(resolved.repository, entry.repository);
            assert!(resolved.repository.ends_with(".git"));
            assert!(resolved.repository.starts_with("https://"));
        }
    }

    #[test]
    fn unknown_ids_do_not_resolve() {
        for id in ["xuepoo.markdown", "bitty-terminal.not-a-plugin", ""] {
            assert!(official_plugin_for(id).is_none());
        }
    }

    #[test]
    fn dashboard_is_part_of_the_official_set() {
        let dashboard = official_plugin_for("bitty-terminal.dashboard")
            .expect("dashboard is part of the complete official set");
        assert_eq!(
            dashboard.repository,
            "https://github.com/bitty-terminal/dashboard.git"
        );
    }

    #[test]
    fn pins_and_hashes_are_well_formed() {
        for entry in official_plugins() {
            let pin = entry.pin;
            assert_eq!(pin.len(), 40, "{} pin length", entry.id);
            assert!(
                pin.bytes().all(|b| b.is_ascii_hexdigit()),
                "{} pin",
                entry.id
            );
            let hash = entry.manifest_sha256();
            assert_eq!(hash.len(), 64, "{} hash length", entry.id);
            assert!(
                hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "{} hash",
                entry.id
            );
            assert!(entry.compat_bitty.is_some(), "{} compat.bitty", entry.id);
            assert!(
                !entry.description.trim().is_empty(),
                "{} description",
                entry.id
            );
        }
    }

    #[test]
    fn source_revision_is_an_immutable_commit() {
        assert_eq!(SOURCE_REVISION.len(), 40);
        assert!(SOURCE_REVISION.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(SOURCE_REPOSITORY.starts_with("https://"));
    }

    #[test]
    fn dashboard_is_listed_with_its_host_floor() {
        let dashboard = official_plugin_for("bitty-terminal.dashboard")
            .expect("dashboard is part of the official catalog");
        assert_eq!(dashboard.compat_bitty, Some(">=0.0.26"));
    }

    #[test]
    fn official_compat_ranges_have_no_upper_bound() {
        for entry in official_plugins() {
            let range = entry
                .compat_bitty
                .expect("every official entry declares compat.bitty");
            assert!(
                !range.contains('<') && !range.contains(','),
                "{}: compat.bitty {range:?} must be minimum-only (no upper bound)",
                entry.id
            );
        }
    }
}
