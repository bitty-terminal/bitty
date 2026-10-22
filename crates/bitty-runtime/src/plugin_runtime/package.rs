//! Package-manager staging side of the XDG plugin store (Gap B).
//!
//! The runtime loads packages read-only from the ratified store
//! (`$XDG_DATA_HOME/bitty/plugins/`); this module is the writer the
//! `bitty plugin` CLI adapts. It owns exactly one transaction:
//!
//! ```text
//! validate source (manifest, entry point, bounds)
//!   -> compatibility check (compat.bitty / compat.plugin-api)
//!   -> capability diff (added capabilities need explicit approval)
//!   -> quarantine copy under packages/<id>/.tmp-*
//!   -> content digest of the staged module tree
//!   -> replace/rename to packages/<id>/<version> (old tree kept as .tmp-*)
//!   -> atomic current.json pointer write (commit point)
//!   -> drop the superseded tree
//! ```
//!
//! The commit point is the `current.json` write: before it, any failure rolls
//! the version directory back to its prior contents (or removes it), so a
//! successful command always leaves the package and index consistent and a
//! failed one leaves neither change. A crashed process can still leave an
//! unindexed version directory; [`scan_orphans`] surfaces it and the next
//! install either adopts it (same staged bytes) or replaces it after explicit
//! confirmation (`--force`).
//!
//! Installation executes zero plugin code: the manifest and module tree are
//! treated as untrusted data. A local directory source stages as
//! [`SourceClass::LocalPath`] provenance with a store-relative root: it never
//! claims registry or signature provenance, is displayed as unverified, and
//! `--safe` never creates a VM for it. Unlike the read-only development flow
//! (an absolute canonical path re-digested on every load), the staged copy is
//! immutable: a digest mismatch is a store integrity failure and fails closed.
//!
//! `git` and registry sources remain with the package-manager follow-up; the
//! CLI rejects them explicitly instead of pretending to resolve them. The
//! official-catalog path stages a pinned git checkout through the same
//! pipeline and records [`SourceClass::Registry`] provenance.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use bitty_package::requirement::VersionReq;
use bitty_package::version::Version;
use bitty_plugin_host::manifest::PluginManifest;

use super::resolution::{self, PluginRecord};
use super::{
    PLUGIN_MANIFEST_MAX_BYTES, PLUGIN_MODULE_MAX_FILES, PLUGIN_MODULE_TREE_MAX_BYTES, SourceClass,
};

/// Manifest file name (accepted v1 contract).
pub const MANIFEST_FILE_NAME: &str = "bitty-plugin.toml";
/// Store subdirectory holding all staged package trees.
pub const PACKAGES_DIR: &str = "packages";
/// Store subdirectory where repair moves broken/extra trees instead of deleting.
pub const QUARANTINE_DIR: &str = "quarantine";
/// Prefix of an in-flight quarantine copy (never a valid plugin id or version).
const STAGING_PREFIX: &str = ".tmp-";
/// Prefix of a superseded version tree kept until the index commit lands.
///
/// Shares the `.tmp-` prefix so the existing stale-staging GC also bounds
/// crash leftovers from an interrupted replacement.
const REPLACED_PREFIX: &str = ".tmp-replaced-";
/// Maximum plugin directories inspected by one orphan scan.
///
/// The store is user-writable, so a read-only `bitty plugin list` must bound
/// its work even when the directory tree is hostile; entries beyond the cap
/// are left unreported rather than failing the command.
pub const ORPHAN_SCAN_MAX_PLUGINS: usize = 512;
/// Maximum version directories inspected per plugin by one orphan scan.
pub const ORPHAN_SCAN_MAX_VERSIONS: usize = 64;
/// Age after which doctor repair removes a leftover staging directory.
///
/// Shorter than the install-path GC so an interrupted operation can be
/// repaired promptly, while a concurrent install in another process (staged
/// seconds ago) is left alone.
pub const DOCTOR_STAGING_MAX_AGE_SECS: u64 = 5 * 60;
/// Maximum age of a quarantine `.tmp-*` dir before it is considered stale.
///
/// Crashed installs leave these behind; an in-flight staging copy is only
/// minutes old, so 24h never collects live work while still bounding disk
/// growth from repeated crashes.
pub const STALE_TMP_MAX_AGE_SECS: u64 = 24 * 60 * 60;
/// Maximum `.tmp-*` entries scanned per GC pass (bounds work on a hostile store).
pub const STALE_TMP_MAX_SCAN: usize = 64;
/// Maximum stale dirs removed per GC pass (bounds delete work per install).
pub const STALE_TMP_MAX_REMOVE: usize = 32;
/// Maximum bytes removed per GC pass (bounds delete work per install).
///
/// Each quarantine copy is itself bounded by the module-tree ceilings
/// (16 MiB), so this caps pathological stores without extra walks on the
/// happy path.
pub const STALE_TMP_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum chars kept in a prune/GC warning detail (bounded, char-safe).
const PRUNE_WARNING_MAX_CHARS: usize = 512;
/// Owner-only mode for staged package files (Unix).
#[cfg(unix)]
const STORE_FILE_MODE: u32 = 0o600;
/// Owner-only mode for store directories (Unix).
#[cfg(unix)]
const STORE_DIR_MODE: u32 = 0o700;

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// In-process mutual exclusion for store mutation transactions (PLUG-REG-011).
static STORE_MUTEX: Mutex<()> = Mutex::new(());

/// One local-directory install or update request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalInstallOptions {
    /// Desired load state for fresh installs when no prior record exists in the store.
    ///
    /// For local developer workflows and explicit CLI installs, this defaults to `true`.
    /// When adhering to the official plugin onboarding contract (rules 4 and 7),
    /// fresh installs default to staged-and-disabled (`false`).
    pub enable: bool,

    /// Explicit override for load state on update/reinstall.
    ///
    /// When `None` (the default), existing records preserve their previous
    /// `enabled` state across updates, reinstalls, and capability changes
    /// (PLUG-REG-009). When `Some(state)`, explicitly forces the record's
    /// `enabled` state to `state`.
    pub explicit_override: Option<bool>,

    /// Provenance class recorded for the staged package.
    ///
    /// Defaults to [`SourceClass::LocalPath`] for CLI directory/git/archive
    /// installs. The official-catalog install path sets
    /// [`SourceClass::Registry`] after verifying the registry's pinned
    /// revision and manifest hash; the store transaction itself is identical.
    pub source_class: SourceClass,

    /// Replace an existing version directory without a repair prompt.
    ///
    /// This covers two cases that are otherwise confirmation-gated: an
    /// unindexed/partial directory left by an interrupted or pre-index
    /// install, and a same-version reinstall whose staged content differs
    /// from what is recorded (a source checkout rebuilt without a version
    /// bump). Capability consent is unaffected: added capabilities still
    /// need approval.
    pub force_repair: bool,
}

impl Default for LocalInstallOptions {
    fn default() -> Self {
        Self {
            enable: true,
            explicit_override: None,
            source_class: SourceClass::LocalPath,
            force_repair: false,
        }
    }
}

impl LocalInstallOptions {
    /// Staged-and-disabled options adhering to the official plugin onboarding
    /// contract (rules 4 and 7): fresh installs start disabled, updates preserve
    /// existing desired state.
    #[must_use]
    pub fn staged_and_disabled() -> Self {
        Self {
            enable: false,
            explicit_override: None,
            source_class: SourceClass::LocalPath,
            force_repair: false,
        }
    }
}

/// Why an existing version directory cannot be reused as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementKind {
    /// The index records this version, but the staged manifest or module tree
    /// differs (same-version reinstall after a source change).
    SameVersionContent,
    /// The plugin has no index record at all: an interrupted install or a
    /// package tree copied into the store by hand.
    Orphaned,
    /// The directory is the recorded retained predecessor of the active
    /// version and the staged content differs from it.
    RetainedPredecessor,
    /// The plugin has an index record for another version, and this directory
    /// is not its retained predecessor.
    UnreferencedVersion,
}

impl ReplacementKind {
    /// Stable label for diagnostics and tests.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SameVersionContent => "same-version-content",
            Self::Orphaned => "orphaned",
            Self::RetainedPredecessor => "retained-predecessor",
            Self::UnreferencedVersion => "unreferenced-version",
        }
    }
}

/// One existing version directory that would be replaced by this install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacementRequest {
    /// Why the directory cannot be reused.
    pub kind: ReplacementKind,
    /// Store-relative directory that would be replaced.
    pub root: String,
    /// Bounded, user-facing explanation shown before the repair prompt.
    pub detail: String,
}

/// One package version directory that `current.json` does not reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanPackage {
    /// Owner-qualified plugin id from the directory name.
    pub plugin_id: String,
    /// Version from the directory name.
    pub version: String,
    /// Store-relative directory path.
    pub root: String,
    /// Why the directory is unreferenced.
    pub kind: OrphanKind,
    /// Whether the directory has a readable manifest and Lua entry point.
    pub complete: bool,
}

/// Why a version directory is not referenced by the store index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanKind {
    /// The plugin has no `current.json` record (interrupted or pre-index
    /// install).
    NoIndexRecord,
    /// The plugin has a record for a different version, and this directory is
    /// neither the active version nor its recorded retained predecessor.
    UnreferencedVersion,
}

impl OrphanKind {
    /// Stable label for output and tests.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoIndexRecord => "no-index-record",
            Self::UnreferencedVersion => "unreferenced-version",
        }
    }
}

/// The exact staged snapshot a human reviewer is asked to approve.
///
/// It carries the canonical manifest hash and content digest of the quarantine
/// copy that consent is bound to, plus the capability diff. The transaction
/// that produced it commits that same snapshot: a manifest swapped in the
/// source directory while the prompt is open can never widen the installed
/// grant, because the installer no longer re-reads the source after consent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentRequest {
    /// Owner-qualified plugin id.
    pub plugin_id: String,
    /// Reviewed version.
    pub version: String,
    /// Canonical manifest hash of the staged body.
    pub manifest_hash: String,
    /// Content digest of the staged module tree.
    pub content_digest: String,
    /// Capabilities added over the recorded grant.
    pub added: Vec<String>,
    /// Full granted capability set that will be recorded on approval.
    pub granted: Vec<String>,
    /// Existing version directory that this install would replace.
    ///
    /// `None` when nothing is overwritten (a fresh install, an identical
    /// reinstall, or a `force_repair` install that already opted in). When
    /// present the caller must confirm the replacement before returning
    /// `true`; declining commits nothing.
    pub replacement: Option<ReplacementRequest>,
}

/// Outcome of one successful install/update transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// Owner-qualified plugin id from the verified manifest.
    pub plugin_id: String,
    /// Installed version.
    pub version: String,
    /// Provenance class (`local-path` for this entry point).
    pub source_class: SourceClass,
    /// Consent-bound canonical manifest hash.
    pub manifest_hash: String,
    /// Digest of the staged module tree.
    pub content_digest: String,
    /// Store-relative package root.
    pub root: String,
    /// Granted capability identifiers after this transaction.
    pub granted: Vec<String>,
    /// Capabilities added over the previous grant (empty when carried forward).
    pub added: Vec<String>,
    /// Whether an existing record was replaced.
    pub updated: bool,
    /// Whether an existing version directory's content was replaced in place.
    ///
    /// `true` for an orphan/partial directory repair and for a same-version
    /// reinstall whose staged bytes differ from the recorded tree.
    pub content_replaced: bool,
    /// Version replaced by this transaction, when it was an update.
    pub previous_version: Option<String>,
    /// Post-commit cleanup warning (CTX-0417).
    ///
    /// `None` on the happy path. When stale `.tmp-*` GC or retained-version
    /// pruning fails after `current.json` is committed, the install itself
    /// already succeeded, so the failure is reported here instead of
    /// returning `Err` and misleading the caller into thinking nothing
    /// changed. The leftover dirs are inert (no record points at them) and
    /// are retried on the next install.
    pub prune_warning: Option<PruneWarning>,
}

/// Non-fatal post-commit cleanup warning (CTX-0417).
///
/// Carries a bounded human-readable detail; the install already succeeded
/// when this is produced. Callers must surface it (log/stderr) but must not
/// treat it as a transaction failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneWarning {
    /// Bounded detail (at most `PRUNE_WARNING_MAX_CHARS` chars).
    pub detail: String,
}

impl PruneWarning {
    /// Build a warning, truncating `detail` on a char boundary.
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        if detail.chars().count() <= PRUNE_WARNING_MAX_CHARS {
            return Self { detail };
        }
        let truncated: String = detail.chars().take(PRUNE_WARNING_MAX_CHARS).collect();
        Self { detail: truncated }
    }
}

impl std::fmt::Display for PruneWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "post-commit cleanup warning: {}", self.detail)
    }
}

impl std::error::Error for PruneWarning {}

/// Stats from one stale `.tmp-*` GC pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StaleTmpGcStats {
    /// Staging entries scanned (bounded by `STALE_TMP_MAX_SCAN`).
    pub scanned: usize,
    /// Stale dirs removed.
    pub removed: usize,
    /// Approximate bytes removed (bounded walk, early stop at budget).
    pub bytes_removed: u64,
    /// Entries skipped by scan/remove/byte caps or fresh mtime.
    pub skipped: usize,
}

/// Outcome of one uninstall transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallReport {
    /// Removed plugin id.
    pub plugin_id: String,
    /// Version that was active at removal time (`-` for an orphan-only cleanup).
    pub version: String,
    /// Whether a store index record was removed.
    pub record_removed: bool,
    /// Whether the staged tree was removed (false when it was already absent).
    pub tree_removed: bool,
}

/// Bounded, owned failure for every package-manager operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageOpError {
    /// The source cannot be resolved to a readable directory.
    ///
    /// A symlink to a directory is accepted and canonicalized; only an
    /// unresolvable path or a non-directory target is rejected here.
    InvalidSource(String),
    /// The manifest is absent, unreadable, over-limit, or schema-invalid.
    Manifest {
        /// Plugin id or source path the failure belongs to.
        plugin: String,
        /// Bounded detail.
        detail: String,
    },
    /// The package id is reserved by the application-shipped bundled catalog.
    BundledIdReserved {
        /// The reserved plugin id.
        plugin: String,
    },
    /// The module tree violates the ratified bounds or contains a native artifact.
    ModuleTree {
        /// Plugin id the failure belongs to.
        plugin: String,
        /// Bounded detail.
        detail: String,
    },
    /// Declared compatibility does not include this host.
    Incompatible {
        /// Plugin id.
        plugin: String,
        /// Field that failed (`compat.bitty` or `compat.plugin-api`).
        field: String,
        /// Requested range.
        requested: String,
        /// Host version evaluated against the range.
        host: String,
    },
    /// Capability consent was aborted (for example the input stream ended).
    ConsentAborted(String),
    /// The store could not be read or written safely.
    Store(String),
}

impl std::fmt::Display for PackageOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSource(detail) => write!(f, "invalid source: {detail}"),
            Self::Manifest { plugin, detail } => {
                write!(f, "manifest for '{plugin}': {detail}")
            }
            Self::BundledIdReserved { plugin } => write!(
                f,
                "'{plugin}' is reserved by the application-shipped bundled catalog and cannot be installed from a store source"
            ),
            Self::ModuleTree { plugin, detail } => {
                write!(f, "module tree for '{plugin}': {detail}")
            }
            Self::Incompatible {
                plugin,
                field,
                requested,
                host,
            } => write!(
                f,
                "'{plugin}' declares {field} = '{requested}', which does not include host version {host}"
            ),
            Self::ConsentAborted(detail) => write!(f, "consent aborted: {detail}"),
            Self::Store(detail) => write!(f, "plugin store: {detail}"),
        }
    }
}

impl std::error::Error for PackageOpError {}

impl From<super::PluginRuntimeError> for PackageOpError {
    fn from(error: super::PluginRuntimeError) -> Self {
        Self::Store(error.to_string())
    }
}

/// Install or update one plugin from a local directory.
///
/// The source directory must contain `bitty-plugin.toml` at its root; the
/// module tree is `lua/` when present, else the package root. The verified
/// manifest body and module tree are copied into a quarantine directory and
/// digested first. If that snapshot adds capabilities over the recorded grant,
/// or an existing version directory has to be replaced, `consent` is asked to
/// approve **that snapshot** (its canonical `manifest_hash` and
/// `content_digest`), and the same snapshot is committed. The source is never
/// re-read after consent, so a manifest swapped while the prompt is open can
/// never widen the installed grant; the staged body is re-verified against the
/// reviewed hash before commit as defense in depth.
///
/// Returns `Ok(None)` when consent is denied (nothing is committed).
///
/// # Errors
///
/// [`PackageOpError`] for an invalid source, a bundled-reserved id, a manifest
/// or module tree that violates the accepted contract, an incompatible
/// `compat` declaration, an aborted consent callback, or a store write
/// failure. Nothing is committed unless every gate passes.
pub fn install_local_dir(
    store_root: &Path,
    source: &Path,
    options: &LocalInstallOptions,
    consent: &mut dyn FnMut(&ConsentRequest) -> Result<bool, PackageOpError>,
) -> Result<Option<InstallReport>, PackageOpError> {
    install_local_dir_with_index_writer(store_root, source, options, consent, &|root, records| {
        resolution::write_index(root, records)
    })
}

/// Injected `current.json` commit used by rollback tests.
type IndexWriter<'a> = &'a dyn Fn(&Path, &[PluginRecord]) -> Result<(), super::PluginRuntimeError>;

/// [`install_local_dir`] with an injectable index commit, for rollback tests.
fn install_local_dir_with_index_writer(
    store_root: &Path,
    source: &Path,
    options: &LocalInstallOptions,
    consent: &mut dyn FnMut(&ConsentRequest) -> Result<bool, PackageOpError>,
    write_index: IndexWriter<'_>,
) -> Result<Option<InstallReport>, PackageOpError> {
    let source_root = std::fs::canonicalize(source).map_err(|error| {
        PackageOpError::InvalidSource(format!("'{}': {error}", source.display()))
    })?;
    if !source_root.is_dir() {
        return Err(PackageOpError::InvalidSource(format!(
            "'{}' is not a directory",
            source.display()
        )));
    }
    let source_label = source_root.display().to_string();
    let manifest_path = source_root.join(MANIFEST_FILE_NAME);
    let raw = std::fs::read(&manifest_path).map_err(|error| PackageOpError::Manifest {
        plugin: source_label.clone(),
        detail: format!("cannot read '{}': {error}", manifest_path.display()),
    })?;
    if raw.len() > PLUGIN_MANIFEST_MAX_BYTES {
        return Err(PackageOpError::Manifest {
            plugin: source_label.clone(),
            detail: "manifest exceeds the 256 KiB ceiling".to_string(),
        });
    }
    let manifest =
        super::manifest_toml::parse_manifest(&raw).map_err(|detail| PackageOpError::Manifest {
            plugin: source_label.clone(),
            detail,
        })?;
    manifest
        .validate()
        .map_err(|error| PackageOpError::Manifest {
            plugin: manifest.identity.id.as_str().to_string(),
            detail: error.to_string(),
        })?;
    let plugin_id = manifest.identity.id.as_str().to_string();
    // A store package must never shadow a catalog id: discovery would prefer
    // the bundled package, and the CLI could not remove the inert store record.
    if bitty_plugin_host::bundled::bundled_manifest_for(&plugin_id).is_some() {
        return Err(PackageOpError::BundledIdReserved { plugin: plugin_id });
    }

    let source_module_root = resolution::module_root_for(&source_root);
    if super::entry_point(&source_module_root, manifest.id()).is_none() {
        return Err(PackageOpError::ModuleTree {
            plugin: plugin_id.clone(),
            detail: "no init.lua entry point found (root/init.lua or root/<module>/init.lua)"
                .to_string(),
        });
    }
    // Pre-scan with the installer's own rules (VCS dirs skipped, symlinks
    // rejected) so the pre-scan and the copy agree on what is a valid tree.
    resolution::scan_module_tree_source(&plugin_id, &source_module_root).map_err(|error| {
        PackageOpError::ModuleTree {
            plugin: plugin_id.clone(),
            detail: error.to_string(),
        }
    })?;

    check_compat(&manifest)?;

    let records = resolution::load_index(store_root)?;
    let existing = records
        .iter()
        .find(|record| record.plugin_id == plugin_id)
        .cloned();
    let granted = granted_ids(&manifest)?;
    let previous_grant: BTreeSet<String> = existing
        .as_ref()
        .map(|record| record.granted.iter().cloned().collect())
        .unwrap_or_default();
    let added: Vec<String> = granted.difference(&previous_grant).cloned().collect();

    let version = manifest.identity.version.clone();
    let manifest_hash = manifest.manifest_hash();
    let package_dir = store_root.join(PACKAGES_DIR).join(&plugin_id);
    std::fs::create_dir_all(&package_dir).map_err(|error| {
        PackageOpError::Store(format!(
            "cannot create '{}': {error}",
            package_dir.display()
        ))
    })?;
    harden_dirs(store_root, &[PACKAGES_DIR, &plugin_id])?;
    // CTX-0417: best-effort GC of crashed-install `.tmp-*` quarantine dirs
    // before staging. Failures are non-fatal and merged into the post-commit
    // warning so a dirty store never blocks a fresh install.
    let (_, gc_warning) = gc_stale_staging_dirs(&package_dir);
    let staging = package_dir.join(format!(
        "{STAGING_PREFIX}{}-{}",
        std::process::id(),
        STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|error| {
            PackageOpError::Store(format!("cannot clear '{}': {error}", staging.display()))
        })?;
    }
    copy_package(&source_root, &raw, &staging, &plugin_id)?;
    harden_tree(&staging)?;
    let content_digest =
        match resolution::scan_module_tree(&plugin_id, &resolution::module_root_for(&staging)) {
            Ok(digest) => digest,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(PackageOpError::ModuleTree {
                    plugin: plugin_id,
                    detail: error.to_string(),
                });
            }
        };

    // Existing target inspection is bound into the consent request so the
    // human approves the same snapshot and the same replacement. The source
    // is never re-read after consent.
    let target = package_dir.join(&version);
    let root = format!("{PACKAGES_DIR}/{plugin_id}/{version}");
    let target_present = target.exists();
    let target_matches = target_present
        && package_matches_staged(
            &target,
            &plugin_id,
            &version,
            &manifest_hash,
            &content_digest,
        );
    let replacement = if target_present && !target_matches {
        Some(replacement_request(
            &plugin_id,
            &version,
            existing.as_ref(),
            &package_dir,
        ))
    } else {
        None
    };
    let repair_needs_consent = replacement.is_some() && !options.force_repair;
    let consent_needed = !added.is_empty() || repair_needs_consent;
    if consent_needed {
        let request = ConsentRequest {
            plugin_id: plugin_id.clone(),
            version: version.clone(),
            manifest_hash: manifest_hash.clone(),
            content_digest: content_digest.clone(),
            added: added.clone(),
            granted: granted.iter().cloned().collect(),
            replacement: replacement.clone().filter(|_| repair_needs_consent),
        };
        match consent(&request) {
            Ok(true) => {}
            Ok(false) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Ok(None);
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(error);
            }
        }
        // Defense in depth: whatever ran during consent, only the reviewed
        // snapshot may be committed. A changed staged body fails closed.
        let staged = std::fs::read(staging.join(MANIFEST_FILE_NAME)).map_err(|error| {
            let _ = std::fs::remove_dir_all(&staging);
            PackageOpError::Store(format!("cannot re-read staged manifest: {error}"))
        })?;
        let staged_hash = super::manifest_toml::parse_manifest(&staged)
            .map(|manifest| manifest.manifest_hash())
            .ok();
        if staged_hash.as_deref() != Some(manifest_hash.as_str()) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(PackageOpError::Store(
                "staged manifest changed after consent; nothing committed".to_string(),
            ));
        }
    }

    // PLUG-REG-011: serialize complete store mutation transactions under
    // mutual exclusion, reload under protection to preserve unrelated updates,
    // and revalidate baseline around consent to reject concurrent mutations.
    let _lock = STORE_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let current_records = resolution::load_index(store_root)?;
    let current_existing = current_records
        .iter()
        .find(|record| record.plugin_id == plugin_id)
        .cloned();

    // Revalidate around consent: if this plugin was modified in the store
    // while consent/staging was in flight, fail closed with a retryable conflict.
    if current_existing != existing {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(PackageOpError::Store(format!(
            "concurrent transaction conflict on '{plugin_id}': store record changed during install; retry operation"
        )));
    }
    // The target directory can also change without an index change (a
    // concurrent repair of an orphan). Revalidate the exact state the consent
    // prompt described before overwriting anything.
    let current_target_present = target.exists();
    let current_target_matches = current_target_present
        && package_matches_staged(
            &target,
            &plugin_id,
            &version,
            &manifest_hash,
            &content_digest,
        );
    if (current_target_present, current_target_matches) != (target_present, target_matches) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(PackageOpError::Store(format!(
            "concurrent transaction conflict on '{plugin_id}': package directory changed during install; retry operation"
        )));
    }

    let enabled = if let Some(explicit) = options.explicit_override {
        explicit
    } else if let Some(ref existing_record) = existing {
        // PLUG-REG-009: Preserve existing desired state (enabled/disabled)
        // across updates, unchanged reinstalls, and capability changes,
        // unless explicitly overridden.
        existing_record.enabled
    } else {
        // Fresh install: apply fresh-install default policy.
        options.enable
    };

    let mut updated_records = current_records;
    updated_records.retain(|record| record.plugin_id != plugin_id);
    updated_records.push(PluginRecord {
        source_class: options.source_class,
        plugin_id: plugin_id.clone(),
        version: version.clone(),
        root: root.clone(),
        manifest_hash: manifest_hash.clone(),
        content_digest: content_digest.clone(),
        enabled,
        granted: granted.iter().cloned().collect(),
    });

    // Commit sequence (the index write is the commit point):
    //   1. move a differing target aside (not an identical one),
    //   2. move the staged tree into place,
    //   3. write current.json,
    //   4. drop the superseded tree.
    // Any failure in 1-3 restores the exact pre-install state.
    let content_replaced = target_present && !target_matches;
    let mut backup: Option<PathBuf> = None;
    if content_replaced {
        let aside = package_dir.join(format!(
            "{REPLACED_PREFIX}{}-{}",
            std::process::id(),
            STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(error) = std::fs::rename(&target, &aside) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(PackageOpError::Store(format!(
                "cannot replace '{}': {error}",
                target.display()
            )));
        }
        backup = Some(aside);
    }
    if target_matches {
        // Identical bytes are already in place (including the common crash
        // window after the rename but before the index write): just publish
        // the index entry.
        let _ = std::fs::remove_dir_all(&staging);
    } else if let Err(error) = std::fs::rename(&staging, &target) {
        let _ = std::fs::remove_dir_all(&staging);
        restore_backup(backup.as_deref(), &target);
        return Err(PackageOpError::Store(format!(
            "cannot commit '{}': {error}",
            target.display()
        )));
    } else if let Err(error) = harden_tree(&target) {
        let _ = remove_path(&target);
        restore_backup(backup.as_deref(), &target);
        return Err(error);
    }
    if let Err(error) = write_index(store_root, &updated_records) {
        if !target_matches {
            let _ = remove_path(&target);
        }
        restore_backup(backup.as_deref(), &target);
        return Err(PackageOpError::Store(format!(
            "plugin index commit failed; install rolled back: {error}"
        )));
    }
    // Committed. Cleanup failures are non-fatal and reported as warnings.
    let mut post_commit_warning = gc_warning;
    if let Some(aside) = backup.as_deref() {
        if let Err(error) = remove_path(aside) {
            post_commit_warning = merge_warnings(
                post_commit_warning,
                Some(PruneWarning::new(format!(
                    "cannot drop superseded '{}': {error}",
                    aside.display()
                ))),
            );
        }
    }
    if let Err(error) = harden_index(store_root) {
        post_commit_warning = merge_warnings(
            post_commit_warning,
            Some(PruneWarning::new(error.to_string())),
        );
    }

    let previous_version = existing.as_ref().map(|record| record.version.clone());
    // PLUG-REG-008: Keep predecessor/history metadata independently so that an
    // unchanged reinstall does not advance retention and prune the genuinely
    // retained predecessor.
    let retained_predecessor = if let Some(ref existing_record) = existing {
        if existing_record.version != version {
            // Version updated: record the superseded version as the new predecessor.
            if let Err(error) = save_predecessor(&package_dir, &existing_record.version) {
                post_commit_warning = merge_warnings(
                    post_commit_warning,
                    Some(PruneWarning::new(error.to_string())),
                );
            }
            Some(existing_record.version.clone())
        } else {
            // Unchanged reinstall: do not advance retention; keep the previously
            // retained predecessor if its tree is still present.
            load_predecessor(&package_dir)
                .filter(|prev| prev != &version && package_dir.join(prev).is_dir())
        }
    } else {
        // Fresh install: no predecessor.
        None
    };

    // CTX-0417: prune runs after the current.json commit, so the update is
    // already active here. A prune failure must not masquerade as a failed
    // install: report it as a typed warning and let the leftover dirs be
    // retried on the next install. Happy path stays `None`.
    let prune_warning = match prune_retained_versions(
        store_root,
        &plugin_id,
        &version,
        retained_predecessor.as_deref(),
    ) {
        Ok(()) => post_commit_warning,
        Err(error) => {
            let prune = PruneWarning::new(error.to_string());
            merge_warnings(post_commit_warning, Some(prune))
        }
    };

    Ok(Some(InstallReport {
        plugin_id,
        version,
        source_class: options.source_class,
        manifest_hash,
        content_digest,
        root,
        granted: granted.into_iter().collect(),
        added,
        updated: previous_version.is_some(),
        content_replaced,
        previous_version,
        prune_warning,
    }))
}

/// Combine two optional post-commit warnings into one bounded warning.
fn merge_warnings(
    first: Option<PruneWarning>,
    second: Option<PruneWarning>,
) -> Option<PruneWarning> {
    match (first, second) {
        (Some(a), Some(b)) => Some(PruneWarning::new(format!("{}; {}", a.detail, b.detail))),
        (Some(a), None) => Some(a),
        (None, second) => second,
    }
}

/// Restore a superseded tree after a failed commit attempt.
fn restore_backup(backup: Option<&Path>, target: &Path) {
    if let Some(backup) = backup {
        let _ = std::fs::rename(backup, target);
    }
}

/// Remove a file or directory tree (best effort helper for staged paths).
fn remove_path(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Whether an existing version directory already holds exactly the staged
/// package (manifest identity and module-tree digest).
fn package_matches_staged(
    target: &Path,
    plugin_id: &str,
    version: &str,
    manifest_hash: &str,
    content_digest: &str,
) -> bool {
    let Ok(bytes) = std::fs::read(target.join(MANIFEST_FILE_NAME)) else {
        return false;
    };
    let Ok(manifest) = super::manifest_toml::parse_manifest(&bytes) else {
        return false;
    };
    if manifest.id().as_str() != plugin_id
        || manifest.identity.version != version
        || !manifest.manifest_hash().eq_ignore_ascii_case(manifest_hash)
    {
        return false;
    }
    resolution::scan_module_tree(plugin_id, &resolution::module_root_for(target))
        .map(|digest| digest.eq_ignore_ascii_case(content_digest))
        .unwrap_or(false)
}

/// Classify an existing version directory that differs from the staged tree.
fn replacement_request(
    plugin_id: &str,
    version: &str,
    existing: Option<&PluginRecord>,
    package_dir: &Path,
) -> ReplacementRequest {
    let predecessor = load_predecessor(package_dir);
    let kind = match existing {
        Some(record) if record.version == version => ReplacementKind::SameVersionContent,
        None => ReplacementKind::Orphaned,
        Some(_) if predecessor.as_deref() == Some(version) => ReplacementKind::RetainedPredecessor,
        Some(_) => ReplacementKind::UnreferencedVersion,
    };
    let detail = match kind {
        ReplacementKind::SameVersionContent => format!(
            "'{plugin_id}' version {version} is recorded with different content \
             (the manifest or Lua files changed without a version bump)"
        ),
        ReplacementKind::Orphaned => format!(
            "'{plugin_id}' version {version} exists on disk but no store index record \
             refers to it (interrupted or partial install)"
        ),
        ReplacementKind::RetainedPredecessor => format!(
            "'{plugin_id}' version {version} is the retained predecessor of the active \
             version and its content differs"
        ),
        ReplacementKind::UnreferencedVersion => format!(
            "'{plugin_id}' version {version} is not referenced by the store index \
             (the active version is {})",
            existing.map_or("-", |record| record.version.as_str())
        ),
    };
    ReplacementRequest {
        kind,
        root: format!("{PACKAGES_DIR}/{plugin_id}/{version}"),
        detail,
    }
}

/// Set the desired load state of one stored record (atomic index write).
///
/// # Errors
///
/// [`PackageOpError::Store`] when the record is unknown or the index cannot be
/// rewritten.
pub fn set_enabled(
    store_root: &Path,
    plugin_id: &str,
    enabled: bool,
) -> Result<bool, PackageOpError> {
    let _lock = STORE_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut records = resolution::load_index(store_root)?;
    let record = records
        .iter_mut()
        .find(|record| record.plugin_id == plugin_id)
        .ok_or_else(|| {
            PackageOpError::Store(format!(
                "'{plugin_id}' is not installed in the plugin store"
            ))
        })?;
    if record.enabled == enabled {
        return Ok(false);
    }
    record.enabled = enabled;
    resolution::write_index(store_root, &records)?;
    harden_index(store_root)?;
    Ok(true)
}

/// Remove one stored record and its staged package tree.
///
/// The index is switched first (desired state wins) and the tree is deleted
/// afterwards; a tree that cannot be deleted is reported by the caller and is
/// inert because no record points at it. When no index record exists but the
/// package directory does (the orphan case from an interrupted install), the
/// directory is still removed: `remove <id> --force` is the documented repair
/// command, and a second run is a no-op instead of an error.
///
/// # Errors
///
/// [`PackageOpError::Store`] when neither a record nor a package directory
/// exists, or the index cannot be rewritten/deleted.
pub fn uninstall(store_root: &Path, plugin_id: &str) -> Result<UninstallReport, PackageOpError> {
    let _lock = STORE_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut records = resolution::load_index(store_root)?;
    let index = records
        .iter()
        .position(|record| record.plugin_id == plugin_id);
    let removed = match index {
        Some(index) => {
            let removed = records.remove(index);
            resolution::write_index(store_root, &records)?;
            harden_index(store_root)?;
            Some(removed)
        }
        None => None,
    };
    let package_dir = store_root.join(PACKAGES_DIR).join(plugin_id);
    let tree_removed = if package_dir.exists() {
        remove_path(&package_dir).map_err(|error| {
            PackageOpError::Store(format!(
                "record removed but '{}' could not be deleted: {error}",
                package_dir.display()
            ))
        })?;
        true
    } else {
        false
    };
    if removed.is_none() && !tree_removed {
        return Err(PackageOpError::Store(format!(
            "'{plugin_id}' is not installed in the plugin store"
        )));
    }
    Ok(UninstallReport {
        plugin_id: plugin_id.to_string(),
        version: removed
            .as_ref()
            .map_or_else(|| "-".to_string(), |record| record.version.clone()),
        record_removed: removed.is_some(),
        tree_removed,
    })
}

/// One package version directory that `current.json` does not reference.
///
/// This is the visibility contract for read-only commands: an interrupted or
/// partial install is never silently hidden. Incomplete directories are also
/// reported (`complete == false`) so `bitty plugin doctor --repair` can
/// quarantine them instead of rebuilding a record for a broken tree.
///
/// # Errors
///
/// [`PackageOpError::Store`] when the index is unreadable/malformed. A
/// read-only caller must surface that as a repair path rather than treating
/// the store as empty.
pub fn scan_orphans(store_root: &Path) -> Result<Vec<OrphanPackage>, PackageOpError> {
    let records = resolution::load_index(store_root)?;
    let packages = store_root.join(PACKAGES_DIR);
    let Ok(listing) = std::fs::read_dir(&packages) else {
        return Ok(Vec::new());
    };
    let mut plugin_dirs: Vec<PathBuf> = listing
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .filter(|path| !is_hidden_entry(path))
        .collect();
    plugin_dirs.sort();
    let mut orphans = Vec::new();
    for plugin_dir in plugin_dirs.into_iter().take(ORPHAN_SCAN_MAX_PLUGINS) {
        let Some(plugin_id) = file_name(&plugin_dir) else {
            continue;
        };
        let record = records.iter().find(|record| record.plugin_id == plugin_id);
        let predecessor = load_predecessor(&plugin_dir);
        let Ok(listing) = std::fs::read_dir(&plugin_dir) else {
            continue;
        };
        let mut version_dirs: Vec<PathBuf> = listing
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .filter(|path| !is_hidden_entry(path))
            .collect();
        version_dirs.sort();
        for version_dir in version_dirs.into_iter().take(ORPHAN_SCAN_MAX_VERSIONS) {
            let Some(version) = file_name(&version_dir) else {
                continue;
            };
            if record.is_some_and(|record| record.version == version)
                || predecessor.as_deref() == Some(version.as_str())
            {
                continue;
            }
            orphans.push(OrphanPackage {
                plugin_id: plugin_id.clone(),
                version: version.clone(),
                root: format!("{PACKAGES_DIR}/{plugin_id}/{version}"),
                kind: if record.is_none() {
                    OrphanKind::NoIndexRecord
                } else {
                    OrphanKind::UnreferencedVersion
                },
                complete: package_looks_complete(&version_dir, &plugin_id, &version),
            });
        }
    }
    Ok(orphans)
}

/// Whether a directory entry name starts with `.` (never a valid version).
fn is_hidden_entry(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy().starts_with('.'))
        .unwrap_or(true)
}

/// Owned file name of a path, if it is valid UTF-8.
fn file_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string)
}

/// Health category for one store problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreIssueKind {
    /// `current.json` exists but cannot be loaded (malformed/over-limit).
    CorruptIndex,
    /// A version directory is not referenced by the index.
    OrphanPackage,
    /// A version directory is missing its manifest, entry point, or a valid
    /// module tree.
    IncompletePackage,
    /// The index record points at a version directory that is missing,
    /// incomplete, or does not match the recorded hashes.
    DanglingRecord,
    /// A newer complete version directory exists than the indexed version.
    StaleVersion,
    /// A `.tmp-*` staging directory was left by an interrupted operation.
    StaleStaging,
}

impl StoreIssueKind {
    /// Stable label for table/JSON output and tests.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CorruptIndex => "corrupt-index",
            Self::OrphanPackage => "orphan-package",
            Self::IncompletePackage => "incomplete-package",
            Self::DanglingRecord => "dangling-record",
            Self::StaleVersion => "stale-version",
            Self::StaleStaging => "stale-staging",
        }
    }

    /// Whether `bitty plugin doctor --repair` can resolve this issue.
    #[must_use]
    pub fn repairable(self) -> bool {
        matches!(
            self,
            Self::CorruptIndex
                | Self::OrphanPackage
                | Self::IncompletePackage
                | Self::DanglingRecord
                | Self::StaleVersion
                | Self::StaleStaging
        )
    }
}

/// One actionable store problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreIssue {
    /// Problem category.
    pub kind: StoreIssueKind,
    /// Plugin id, or `-` for store-wide issues.
    pub plugin_id: String,
    /// Version, or `-` when not version-scoped.
    pub version: String,
    /// Store-relative path involved.
    pub root: String,
    /// Bounded human-readable explanation.
    pub detail: String,
}

/// Read-only store health snapshot used by `bitty plugin list`/`doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreInspection {
    /// Whether a `current.json` file exists.
    pub index_present: bool,
    /// Number of records in a loadable index (0 when corrupt/absent).
    pub record_count: usize,
    /// Load failure detail when the index exists but is unreadable.
    pub index_error: Option<String>,
    /// Every problem found, in deterministic plugin/version order.
    pub issues: Vec<StoreIssue>,
}

/// Inspect the store without mutating it.
///
/// Unlike [`load_index`], a malformed index is reported as a
/// [`StoreIssueKind::CorruptIndex`] issue instead of aborting the inspection,
/// so the doctor can name the exact repair command. Package directories are
/// still inspected in that case because they are the rebuild source.
///
/// # Errors
///
/// [`PackageOpError::Store`] only when the store root itself cannot be read.
pub fn inspect_store(store_root: &Path) -> Result<StoreInspection, PackageOpError> {
    let index_present = store_root.join(resolution::CURRENT_POINTER_FILE).is_file();
    let (records, index_error) = match resolution::load_index(store_root) {
        Ok(records) => (records, None),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    let packages = read_package_dirs(store_root)?;
    let mut issues = Vec::new();
    if let Some(detail) = &index_error {
        issues.push(StoreIssue {
            kind: StoreIssueKind::CorruptIndex,
            plugin_id: "-".to_string(),
            version: "-".to_string(),
            root: resolution::CURRENT_POINTER_FILE.to_string(),
            detail: detail.clone(),
        });
    }
    let mut seen_versions: BTreeSet<(String, String)> = BTreeSet::new();
    for (plugin_id, dirs) in &packages {
        let record = records.iter().find(|record| record.plugin_id == *plugin_id);
        let predecessor = load_predecessor(&store_root.join(PACKAGES_DIR).join(plugin_id));
        for dir in dirs {
            seen_versions.insert((plugin_id.clone(), dir.version.clone()));
            let active = record.is_some_and(|record| record.version == dir.version);
            let retained = predecessor.as_deref() == Some(dir.version.as_str());
            let hash_matches = record.is_some_and(|record| {
                dir.manifest_hash
                    .as_deref()
                    .is_some_and(|hash| hash.eq_ignore_ascii_case(&record.manifest_hash))
                    && dir
                        .content_digest
                        .as_deref()
                        .is_some_and(|digest| digest.eq_ignore_ascii_case(&record.content_digest))
            });
            if active {
                if !dir.complete {
                    issues.push(StoreIssue {
                        kind: StoreIssueKind::DanglingRecord,
                        plugin_id: plugin_id.clone(),
                        version: dir.version.clone(),
                        root: dir.root.clone(),
                        detail: format!(
                            "current.json points at an unusable package: {}",
                            dir.detail
                        ),
                    });
                } else if !hash_matches {
                    issues.push(StoreIssue {
                        kind: StoreIssueKind::DanglingRecord,
                        plugin_id: plugin_id.clone(),
                        version: dir.version.clone(),
                        root: dir.root.clone(),
                        detail:
                            "index record does not match the package tree (hash or digest mismatch)"
                                .to_string(),
                    });
                }
            } else if !retained {
                let stale_pointer = record.is_some_and(|record| {
                    dir.complete && version_is_newer(&dir.version, &record.version)
                });
                issues.push(StoreIssue {
                    kind: if !dir.complete {
                        StoreIssueKind::IncompletePackage
                    } else if stale_pointer {
                        StoreIssueKind::StaleVersion
                    } else {
                        StoreIssueKind::OrphanPackage
                    },
                    plugin_id: plugin_id.clone(),
                    version: dir.version.clone(),
                    root: dir.root.clone(),
                    detail: if !dir.complete {
                        dir.detail.clone()
                    } else if stale_pointer {
                        format!(
                            "a newer complete version is installed but current.json still points at {}",
                            record.map_or("-", |record| record.version.as_str())
                        )
                    } else {
                        "package directory is not referenced by current.json".to_string()
                    },
                });
            }
        }
    }
    for record in &records {
        if !seen_versions.contains(&(record.plugin_id.clone(), record.version.clone())) {
            issues.push(StoreIssue {
                kind: StoreIssueKind::DanglingRecord,
                plugin_id: record.plugin_id.clone(),
                version: record.version.clone(),
                root: record.root.clone(),
                detail: "current.json record points at a package directory that does not exist"
                    .to_string(),
            });
        }
    }
    for staging in read_staging_dirs(store_root)? {
        issues.push(StoreIssue {
            kind: StoreIssueKind::StaleStaging,
            plugin_id: staging.plugin_id,
            version: "-".to_string(),
            root: staging.relative,
            detail: "leftover staging directory (interrupted install/uninstall); \
                     `bitty plugin doctor --repair` removes it"
                .to_string(),
        });
    }
    issues.sort_by(|a, b| {
        (
            &a.plugin_id,
            &a.version,
            a.kind.as_str(),
            &a.root,
            &a.detail,
        )
            .cmp(&(
                &b.plugin_id,
                &b.version,
                b.kind.as_str(),
                &b.root,
                &b.detail,
            ))
    });
    Ok(StoreInspection {
        index_present,
        record_count: records.len(),
        index_error,
        issues,
    })
}

/// Outcome of one `bitty plugin doctor --repair` store pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreRepairReport {
    /// Moved package/index paths (original -> quarantine), in execution order.
    pub quarantined: Vec<(String, String)>,
    /// Staging directories removed.
    pub staging_removed: Vec<String>,
    /// Whether `current.json` was rebuilt from package directories.
    pub index_rebuilt: bool,
    /// Number of records published.
    pub records_written: usize,
    /// Non-fatal cleanup warnings.
    pub warnings: Vec<String>,
}

/// Repair the store from complete package directories.
///
/// The rebuild chooses, per plugin, the indexed version when its tree and
/// hashes still match, else the highest complete version. Incomplete or
/// unreferenced directories are moved to `<store>/quarantine/` (never
/// deleted), stale `.tmp-*` staging is removed, and `current.json` is written
/// last with an atomic rename. Rebuilt records never invent authority: when
/// the manifest hash changes, grants are reset and the record is disabled
/// unless the manifest requests no capabilities.
///
/// # Errors
///
/// [`PackageOpError::Store`] when the store cannot be read/written. Moves are
/// rolled back on failure so a failed repair leaves the previous layout
/// intact (cleanup after the index commit is warning-only).
pub fn repair_store(store_root: &Path) -> Result<StoreRepairReport, PackageOpError> {
    let _lock = STORE_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut report = StoreRepairReport::default();
    let (records, index_error) = match resolution::load_index(store_root) {
        Ok(records) => (records, None),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    let packages = read_package_dirs(store_root)?;
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut rebuilt: Vec<PluginRecord> = Vec::new();
    let mut predecessor_changes: Vec<(PathBuf, Option<String>)> = Vec::new();
    let mut changed = index_error.is_some();

    // Quarantine the corrupt index first so a failed rebuild can restore it.
    if index_error.is_some() {
        let index = store_root.join(resolution::CURRENT_POINTER_FILE);
        if index.exists() {
            let destination = quarantine_path(store_root, "_index", "current.json", &mut report)?;
            std::fs::rename(&index, &destination).map_err(|error| {
                PackageOpError::Store(format!(
                    "cannot quarantine corrupt '{}': {error}",
                    index.display()
                ))
            })?;
            moves.push((index, destination));
            report.quarantined.push((
                resolution::CURRENT_POINTER_FILE.to_string(),
                relative_to(store_root, moves.last().expect("pushed").1.as_path()),
            ));
        }
        report.index_rebuilt = true;
    }

    for (plugin_id, dirs) in &packages {
        let package_dir = store_root.join(PACKAGES_DIR).join(plugin_id);
        let old = records
            .iter()
            .find(|record| record.plugin_id == *plugin_id)
            .cloned();
        let mut sorted: Vec<&PackageInspection> = dirs.iter().collect();
        sorted.sort_by(|a, b| compare_versions_desc(&a.version, &b.version));
        // Keep the old record only when the exact tree still matches it.
        // The rebuild target is the highest complete version on disk, not
        // necessarily the (possibly stale) indexed version.
        let selected = sorted.iter().copied().find(|dir| dir.complete);
        let preserve_old = match (selected, old.as_ref()) {
            (Some(selected), Some(record)) => {
                selected.version == record.version
                    && selected
                        .manifest_hash
                        .as_deref()
                        .is_some_and(|hash| hash.eq_ignore_ascii_case(&record.manifest_hash))
                    && selected
                        .content_digest
                        .as_deref()
                        .is_some_and(|digest| digest.eq_ignore_ascii_case(&record.content_digest))
            }
            _ => false,
        };
        let old_version_complete = old.as_ref().is_some_and(|old| {
            sorted
                .iter()
                .any(|dir| dir.version == old.version && dir.complete)
        });
        let predecessor = match (selected, old.as_ref()) {
            (Some(selected), Some(old))
                if old.version != selected.version && old_version_complete =>
            {
                // The previously active version stays available as rollback.
                Some(old.version.clone())
            }
            _ => load_predecessor(&package_dir).filter(|version| {
                sorted
                    .iter()
                    .any(|dir| dir.version == *version && dir.complete)
            }),
        };
        if let Some(selected) = selected {
            let record = if preserve_old {
                old.as_ref().expect("preserve_old implies old").clone()
            } else {
                PluginRecord {
                    source_class: SourceClass::LocalPath,
                    plugin_id: plugin_id.clone(),
                    version: selected.version.clone(),
                    root: selected.root.clone(),
                    manifest_hash: selected.manifest_hash.clone().unwrap_or_default(),
                    content_digest: selected.content_digest.clone().unwrap_or_default(),
                    enabled: selected.requested.is_empty(),
                    granted: Vec::new(),
                }
            };
            if old.as_ref() != Some(&record) {
                changed = true;
            }
            rebuilt.push(record);
            for dir in sorted {
                if dir.version == selected.version {
                    continue;
                }
                if predecessor.as_deref() == Some(dir.version.as_str()) && dir.complete {
                    continue;
                }
                let destination =
                    quarantine_path(store_root, plugin_id, &dir.version, &mut report)?;
                let source = store_root.join(&dir.root);
                if let Err(error) = std::fs::rename(&source, &destination) {
                    rollback_moves(&moves);
                    return Err(PackageOpError::Store(format!(
                        "cannot quarantine '{}': {error}",
                        source.display()
                    )));
                }
                moves.push((source, destination));
                report.quarantined.push((
                    dir.root.clone(),
                    relative_to(store_root, moves.last().expect("pushed").1.as_path()),
                ));
                changed = true;
            }
            predecessor_changes.push((package_dir.clone(), predecessor.clone()));
        } else {
            // No complete tree: drop the record and quarantine every dir.
            if old.is_some() {
                changed = true;
            }
            for dir in sorted {
                let destination =
                    quarantine_path(store_root, plugin_id, &dir.version, &mut report)?;
                let source = store_root.join(&dir.root);
                if let Err(error) = std::fs::rename(&source, &destination) {
                    rollback_moves(&moves);
                    return Err(PackageOpError::Store(format!(
                        "cannot quarantine '{}': {error}",
                        source.display()
                    )));
                }
                moves.push((source, destination));
                report.quarantined.push((
                    dir.root.clone(),
                    relative_to(store_root, moves.last().expect("pushed").1.as_path()),
                ));
                changed = true;
            }
        }
    }
    // Records whose plugin has no directory at all are dropped.
    if records
        .iter()
        .any(|record| !packages.contains_key(&record.plugin_id))
    {
        changed = true;
    }
    rebuilt.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
    if changed {
        if let Err(error) = resolution::write_index(store_root, &rebuilt) {
            rollback_moves(&moves);
            return Err(PackageOpError::Store(format!(
                "cannot rewrite current.json; repair rolled back: {error}"
            )));
        }
        report.index_rebuilt = true;
        report.records_written = rebuilt.len();
        // Predecessor markers are advisory (rollback retention only), so they
        // are applied after the index commit and never block the rebuild.
        for (package_dir, predecessor) in predecessor_changes {
            let result = match predecessor.as_deref() {
                Some(version) => save_predecessor(&package_dir, version),
                None => remove_predecessor(&package_dir),
            };
            if let Err(error) = result {
                report.warnings.push(error.to_string());
            }
        }
    }
    let staging = read_staging_dirs(store_root)?;
    for entry in staging {
        let path = store_root.join(&entry.relative);
        if !is_stale_staging(&path) {
            report.warnings.push(format!(
                "left '{}' in place: too recent to prove it was not an in-flight install",
                entry.relative
            ));
            continue;
        }
        if let Err(error) = remove_path(&path) {
            report
                .warnings
                .push(format!("cannot remove '{}': {error}", entry.relative));
        } else {
            report.staging_removed.push(entry.relative);
        }
    }
    Ok(report)
}

/// Cheap completeness check for read-only list output.
///
/// Verifies the manifest identity and entry point only; the full module-tree
/// digest is reserved for `doctor`/install so listing a store with many
/// orphaned trees stays a bounded directory walk.
fn package_looks_complete(version_dir: &Path, plugin_id: &str, version: &str) -> bool {
    let Ok(bytes) = std::fs::read(version_dir.join(MANIFEST_FILE_NAME)) else {
        return false;
    };
    let Ok(manifest) = super::manifest_toml::parse_manifest(&bytes) else {
        return false;
    };
    manifest.id().as_str() == plugin_id
        && manifest.identity.version == version
        && super::entry_point(&resolution::module_root_for(version_dir), manifest.id()).is_some()
}

/// One package directory as inspected for doctor/rebuild purposes.
struct PackageInspection {
    version: String,
    root: String,
    complete: bool,
    manifest_hash: Option<String>,
    content_digest: Option<String>,
    requested: Vec<String>,
    detail: String,
}

/// Read every `packages/<id>/<version>/` directory.
fn read_package_dirs(
    store_root: &Path,
) -> Result<BTreeMap<String, Vec<PackageInspection>>, PackageOpError> {
    let packages = store_root.join(PACKAGES_DIR);
    let mut out: BTreeMap<String, Vec<PackageInspection>> = BTreeMap::new();
    match std::fs::read_dir(&packages) {
        Ok(listing) => {
            let mut plugin_dirs: Vec<PathBuf> = listing
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path())
                .filter(|path| !is_hidden_entry(path))
                .collect();
            plugin_dirs.sort();
            for plugin_dir in plugin_dirs.into_iter().take(ORPHAN_SCAN_MAX_PLUGINS) {
                let Some(plugin_id) = file_name(&plugin_dir) else {
                    continue;
                };
                let Ok(listing) = std::fs::read_dir(&plugin_dir) else {
                    continue;
                };
                let mut version_dirs: Vec<PathBuf> = listing
                    .flatten()
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .map(|entry| entry.path())
                    .filter(|path| !is_hidden_entry(path))
                    .collect();
                version_dirs.sort();
                for version_dir in version_dirs.into_iter().take(ORPHAN_SCAN_MAX_VERSIONS) {
                    let Some(version) = file_name(&version_dir) else {
                        continue;
                    };
                    let inspection = inspect_package_dir(&version_dir, &plugin_id, &version);
                    out.entry(plugin_id.clone()).or_default().push(inspection);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(PackageOpError::Store(format!(
                "cannot read '{}': {error}",
                packages.display()
            )));
        }
    }
    Ok(out)
}

/// Inspect one version directory; unreadable entries are typed as incomplete.
fn inspect_package_dir(version_dir: &Path, plugin_id: &str, version: &str) -> PackageInspection {
    let root = format!("{PACKAGES_DIR}/{plugin_id}/{version}");
    let mut inspection = PackageInspection {
        version: version.to_string(),
        root,
        complete: false,
        manifest_hash: None,
        content_digest: None,
        requested: Vec::new(),
        detail: String::new(),
    };
    let manifest_path = version_dir.join(MANIFEST_FILE_NAME);
    let bytes = match std::fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            inspection.detail = format!("manifest unreadable: {error}");
            return inspection;
        }
    };
    let manifest = match super::manifest_toml::parse_manifest(&bytes) {
        Ok(manifest) => manifest,
        Err(detail) => {
            inspection.detail = format!("manifest invalid: {detail}");
            return inspection;
        }
    };
    if manifest.id().as_str() != plugin_id || manifest.identity.version != version {
        inspection.detail = "manifest id/version does not match its directory".to_string();
        return inspection;
    }
    let module_root = resolution::module_root_for(version_dir);
    if super::entry_point(&module_root, manifest.id()).is_none() {
        inspection.detail = "no init.lua entry point".to_string();
        return inspection;
    }
    let digest = match resolution::scan_module_tree(plugin_id, &module_root) {
        Ok(digest) => digest,
        Err(error) => {
            inspection.detail = format!("module tree invalid: {error}");
            return inspection;
        }
    };
    let requested = match manifest.capabilities.all_ids() {
        Ok(ids) => ids.iter().map(|id| id.as_str().to_string()).collect(),
        Err(error) => {
            inspection.detail = format!("capabilities invalid: {error}");
            return inspection;
        }
    };
    inspection.complete = true;
    inspection.manifest_hash = Some(manifest.manifest_hash());
    inspection.content_digest = Some(digest);
    inspection.requested = requested;
    inspection.detail = "complete".to_string();
    inspection
}

/// Remove `current.json` (best effort) for a package with no predecessor.
fn remove_predecessor(package_dir: &Path) -> Result<(), PackageOpError> {
    match std::fs::remove_file(package_dir.join(PREDECESSOR_FILE)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(PackageOpError::Store(format!(
            "cannot remove predecessor marker: {error}"
        ))),
    }
}

/// Compare two version directory names, newest first; unparseable versions sort last.
fn compare_versions_desc(a: &str, b: &str) -> std::cmp::Ordering {
    match (Version::parse(a), Version::parse(b)) {
        (Ok(a), Ok(b)) => b.cmp(&a),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => b.cmp(a),
    }
}

/// Whether `candidate` is a strictly newer SemVer than `current`.
fn version_is_newer(candidate: &str, current: &str) -> bool {
    matches!(
        (Version::parse(candidate), Version::parse(current)),
        (Ok(candidate), Ok(current)) if candidate > current
    )
}

/// Move a path into the store quarantine, returning its new location.
fn quarantine_path(
    store_root: &Path,
    plugin_id: &str,
    name: &str,
    report: &mut StoreRepairReport,
) -> Result<PathBuf, PackageOpError> {
    let quarantine = store_root.join(QUARANTINE_DIR).join(plugin_id);
    std::fs::create_dir_all(&quarantine).map_err(|error| {
        PackageOpError::Store(format!(
            "cannot create quarantine '{}': {error} (is the store writable?)",
            quarantine.display()
        ))
    })?;
    harden_dirs(store_root, &[QUARANTINE_DIR])?;
    harden_dirs(store_root, &[QUARANTINE_DIR, plugin_id])?;
    let destination = quarantine.join(format!(
        "{}.{}-{}",
        name,
        std::process::id(),
        STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    if destination.exists() {
        report.warnings.push(format!(
            "quarantine target '{}' already exists; using a new name",
            destination.display()
        ));
        return quarantine_path(store_root, plugin_id, &format!("{name}-1"), report);
    }
    Ok(destination)
}

/// Roll back every completed quarantine move (best effort).
fn rollback_moves(moves: &[(PathBuf, PathBuf)]) {
    for (original, destination) in moves.iter().rev() {
        let _ = std::fs::rename(destination, original);
    }
}

/// Path relative to the store for display, falling back to the full path.
fn relative_to(store_root: &Path, path: &Path) -> String {
    path.strip_prefix(store_root)
        .map(|relative| relative.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.display().to_string())
}

/// One leftover staging directory.
struct StagingEntry {
    plugin_id: String,
    relative: String,
}

/// Collect `.tmp-*` directories under every package directory.
fn read_staging_dirs(store_root: &Path) -> Result<Vec<StagingEntry>, PackageOpError> {
    let packages = store_root.join(PACKAGES_DIR);
    let Ok(listing) = std::fs::read_dir(&packages) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for plugin_dir in listing.flatten() {
        if !plugin_dir.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(plugin_id) = file_name(&plugin_dir.path()) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(plugin_dir.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(STAGING_PREFIX)
                || !entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
                continue;
            }
            out.push(StagingEntry {
                plugin_id: plugin_id.clone(),
                relative: format!("{PACKAGES_DIR}/{plugin_id}/{name}"),
            });
        }
    }
    out.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(out)
}

/// Whether leftover staging is old enough to prove no install owns it.
fn is_stale_staging(path: &Path) -> bool {
    is_stale_tmp(path, DOCTOR_STAGING_MAX_AGE_SECS)
}

/// Verify `compat.bitty` and `compat.plugin-api` include this host.
fn check_compat(manifest: &PluginManifest) -> Result<(), PackageOpError> {
    check_compat_with_hosts(manifest, host_bitty_version(), bitty_lua::host::API_VERSION)
}

/// Evaluate declared compatibility ranges against the running host.
///
/// Public entry point for callers that resolve compatibility *before* a
/// package body exists (the embedded official catalog). It uses the same
/// closed requirement grammar and the same host versions as install and
/// activation, so a catalog pre-check can never disagree with the package
/// pipeline. The returned error is [`PackageOpError::Incompatible`] with the
/// exact declared range and evaluated host version.
pub fn check_declared_compat(
    plugin: &str,
    bitty_req: Option<&str>,
    api_req: Option<&str>,
) -> Result<(), PackageOpError> {
    evaluate_compat(bitty_req, api_req, host_bitty_version(), host_api_version())
        .map_err(|failure| compat_failure_to_op(plugin, failure))
}

/// Running host Bitty version used for compat evaluation.
///
/// Single source for the resolve/activate re-check (CTX-0416): install,
/// resolution, and activation must evaluate the same closed grammar against
/// the same host line. `0.1`-line floors in declared ranges stay as written;
/// dev-host (`0.0.x`) behavior is unchanged because bundled packages skip the
/// re-check while store records fail closed.
pub(crate) fn host_bitty_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Running host plugin-API version used for compat evaluation.
pub(crate) fn host_api_version() -> &'static str {
    bitty_lua::host::API_VERSION
}

/// Structured compat failure shared by install and resolve/activate.
///
/// Keeps the closed requirement grammar in one place so `resolve_record` and
/// `activate` evaluate exactly what `install_local_dir` evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompatFailure {
    /// Range text violates the closed grammar.
    Invalid {
        /// Field that failed (`compat.bitty` or `compat.plugin-api`).
        field: String,
        /// Bounded detail.
        detail: String,
    },
    /// Host version does not satisfy the declared range.
    Incompatible {
        /// Field that failed.
        field: String,
        /// Declared range.
        requested: String,
        /// Host version evaluated.
        host: String,
    },
}

/// Evaluate `compat.bitty` / `compat.plugin-api` against explicit hosts.
///
/// Pure and testable: `install` passes the running hosts, while tests pass an
/// upgraded host to simulate a host upgrade after install (CTX-0416).
pub(crate) fn evaluate_compat(
    bitty_req: Option<&str>,
    api_req: Option<&str>,
    host_bitty: &str,
    host_api: &str,
) -> Result<(), CompatFailure> {
    evaluate_range("compat.bitty", bitty_req, host_bitty)?;
    evaluate_range("compat.plugin-api", api_req, host_api)
}

/// Evaluate one compat field with the closed requirement grammar.
fn evaluate_range(field: &str, requested: Option<&str>, host: &str) -> Result<(), CompatFailure> {
    let Some(requested) = requested else {
        return Ok(());
    };
    let range = normalize_comparators(requested);
    let requirement = VersionReq::parse(&range).map_err(|error| CompatFailure::Invalid {
        field: field.to_string(),
        detail: format!("invalid {field} range '{requested}': {error}"),
    })?;
    let host_version = Version::parse(host).map_err(|error| CompatFailure::Incompatible {
        field: field.to_string(),
        requested: requested.to_string(),
        host: format!("{host} (unparseable: {error})"),
    })?;
    if !requirement.matches(&host_version) {
        return Err(CompatFailure::Incompatible {
            field: field.to_string(),
            requested: requested.to_string(),
            host: host.to_string(),
        });
    }
    Ok(())
}

/// Verify `compat.bitty` and `compat.plugin-api` against explicit hosts.
///
/// Test hook for the install-then-upgrade simulation: install uses the running
/// hosts, then resolution is re-evaluated with upgraded hosts.
pub(crate) fn check_compat_with_hosts(
    manifest: &PluginManifest,
    host_bitty: &str,
    host_api: &str,
) -> Result<(), PackageOpError> {
    evaluate_compat(
        manifest.compat.bitty.as_deref(),
        manifest.compat.plugin_api.as_deref(),
        host_bitty,
        host_api,
    )
    .map_err(|failure| compat_failure_to_op(manifest.identity.id.as_str(), failure))
}

/// Map a structured compat failure to the installer's error type.
fn compat_failure_to_op(plugin: &str, failure: CompatFailure) -> PackageOpError {
    match failure {
        CompatFailure::Invalid { field: _, detail } => PackageOpError::Manifest {
            plugin: plugin.to_string(),
            detail,
        },
        CompatFailure::Incompatible {
            field,
            requested,
            host,
        } => PackageOpError::Incompatible {
            plugin: plugin.to_string(),
            field,
            requested,
            host,
        },
    }
}

/// Pad partial comparator operands to `X.Y.Z`.
///
/// The accepted manifest examples spell ranges as `>=0.5,<1.0`, while the
/// closed requirement grammar used for evaluation anchors every comparator at
/// a full `X.Y.Z` version (caret/tilde already accept shorthand). Padding
/// keeps the accepted spelling working without loosening the grammar.
fn normalize_comparators(raw: &str) -> String {
    raw.split(',')
        .map(|segment| {
            let segment = segment.trim();
            let (prefix, rest) = if let Some(rest) = segment.strip_prefix(">=") {
                (">=", rest)
            } else if let Some(rest) = segment.strip_prefix("<=") {
                ("<=", rest)
            } else if let Some(rest) = segment.strip_prefix('>') {
                (">", rest)
            } else if let Some(rest) = segment.strip_prefix('<') {
                ("<", rest)
            } else if let Some(rest) = segment.strip_prefix('=') {
                ("=", rest)
            } else {
                ("", segment)
            };
            format!("{prefix}{}", pad_partial(rest.trim()))
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn pad_partial(version: &str) -> String {
    let core_end = version.find(['-', '+']).unwrap_or(version.len());
    let core = &version[..core_end];
    let suffix = &version[core_end..];
    match core.matches('.').count() {
        1 => format!("{core}.0{suffix}"),
        0 => format!("{core}.0.0{suffix}"),
        _ => version.to_string(),
    }
}

fn granted_ids(manifest: &PluginManifest) -> Result<BTreeSet<String>, PackageOpError> {
    let ids = manifest
        .capabilities
        .all_ids()
        .map_err(|error| PackageOpError::Manifest {
            plugin: manifest.identity.id.as_str().to_string(),
            detail: format!("invalid capability request: {error}"),
        })?;
    Ok(ids.into_iter().map(|id| id.as_str().to_string()).collect())
}

/// Copy the manifest body and module tree into a fresh staging directory.
///
/// VCS metadata directories are skipped, symlinked entries are rejected, and
/// the ratified file/byte ceilings are enforced again while copying.
fn copy_package(
    source_root: &Path,
    manifest_bytes: &[u8],
    staging: &Path,
    plugin: &str,
) -> Result<(), PackageOpError> {
    std::fs::create_dir_all(staging).map_err(|error| {
        PackageOpError::Store(format!("cannot create '{}': {error}", staging.display()))
    })?;
    let lua = source_root.join("lua");
    if lua.is_dir() {
        copy_tree(
            &lua,
            &staging.join("lua"),
            plugin,
            &mut CopyBudget::default(),
        )?;
    } else {
        copy_tree(source_root, staging, plugin, &mut CopyBudget::default())?;
    }
    let manifest_target = staging.join(MANIFEST_FILE_NAME);
    std::fs::write(&manifest_target, manifest_bytes).map_err(|error| {
        PackageOpError::Store(format!(
            "cannot write '{}': {error}",
            manifest_target.display()
        ))
    })?;
    Ok(())
}

#[derive(Default)]
struct CopyBudget {
    files: usize,
    bytes: u64,
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    plugin: &str,
    budget: &mut CopyBudget,
) -> Result<(), PackageOpError> {
    std::fs::create_dir_all(destination).map_err(|error| {
        PackageOpError::Store(format!(
            "cannot create '{}': {error}",
            destination.display()
        ))
    })?;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(source)
        .map_err(|error| PackageOpError::ModuleTree {
            plugin: plugin.to_string(),
            detail: format!("cannot read '{}': {error}", source.display()),
        })?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| PackageOpError::ModuleTree {
                    plugin: plugin.to_string(),
                    detail: format!("cannot read '{}': {error}", source.display()),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    for entry in entries {
        let name = entry
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let metadata =
            std::fs::symlink_metadata(&entry).map_err(|error| PackageOpError::ModuleTree {
                plugin: plugin.to_string(),
                detail: format!("cannot stat '{}': {error}", entry.display()),
            })?;
        if metadata.file_type().is_symlink() {
            return Err(PackageOpError::ModuleTree {
                plugin: plugin.to_string(),
                detail: format!("symlinked entries are not supported: '{}'", entry.display()),
            });
        }
        if metadata.is_dir() {
            if resolution::VCS_DIRS.contains(&name.as_str()) {
                continue;
            }
            copy_tree(&entry, &destination.join(&name), plugin, budget)?;
            continue;
        }
        budget.files += 1;
        if budget.files > PLUGIN_MODULE_MAX_FILES {
            return Err(PackageOpError::ModuleTree {
                plugin: plugin.to_string(),
                detail: "module tree exceeds the 4096-file ceiling".to_string(),
            });
        }
        budget.bytes = budget.bytes.saturating_add(metadata.len());
        if budget.bytes > PLUGIN_MODULE_TREE_MAX_BYTES as u64 {
            return Err(PackageOpError::ModuleTree {
                plugin: plugin.to_string(),
                detail: "module tree exceeds the 16 MiB ceiling".to_string(),
            });
        }
        std::fs::copy(&entry, destination.join(&name)).map_err(|error| {
            PackageOpError::Store(format!(
                "cannot copy '{}' to staging: {error}",
                entry.display()
            ))
        })?;
    }
    Ok(())
}

const PREDECESSOR_FILE: &str = ".predecessor";

fn load_predecessor(package_dir: &Path) -> Option<String> {
    let path = package_dir.join(PREDECESSOR_FILE);
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn save_predecessor(package_dir: &Path, version: &str) -> Result<(), PackageOpError> {
    let path = package_dir.join(PREDECESSOR_FILE);
    std::fs::write(&path, version.as_bytes()).map_err(|error| {
        PackageOpError::Store(format!("cannot record predecessor version: {error}"))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(STORE_FILE_MODE));
    }
    Ok(())
}

/// Remove staged versions other than the current and the immediately previous
/// one (RFC retained-generation recommendation: current plus previous).
fn prune_retained_versions(
    store_root: &Path,
    plugin_id: &str,
    current: &str,
    previous: Option<&str>,
) -> Result<(), PackageOpError> {
    let package_dir = store_root.join(PACKAGES_DIR).join(plugin_id);
    let Ok(entries) = std::fs::read_dir(&package_dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(STAGING_PREFIX)
            || name.starts_with('.')
            || name == current
            || Some(name.as_str()) == previous
        {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).map_err(|error| {
                PackageOpError::Store(format!("cannot prune '{}': {error}", path.display()))
            })?;
        }
    }
    Ok(())
}

/// Best-effort GC of stale quarantine `.tmp-*` dirs under one package dir.
///
/// Bounded by age (`STALE_TMP_MAX_AGE_SECS`), scan count
/// (`STALE_TMP_MAX_SCAN`), remove count (`STALE_TMP_MAX_REMOVE`), and bytes
/// (`STALE_TMP_MAX_BYTES`). Fresh entries (mtime unavailable, in the future,
/// or younger than the age bound) are skipped so live concurrent staging is
/// never collected. Failures are collected as a single typed warning instead
/// of failing the install.
fn gc_stale_staging_dirs(package_dir: &Path) -> (StaleTmpGcStats, Option<PruneWarning>) {
    gc_stale_staging_dirs_with_limits(
        package_dir,
        STALE_TMP_MAX_AGE_SECS,
        STALE_TMP_MAX_SCAN,
        STALE_TMP_MAX_REMOVE,
        STALE_TMP_MAX_BYTES,
    )
}

fn gc_stale_staging_dirs_with_limits(
    package_dir: &Path,
    max_age_secs: u64,
    max_scan: usize,
    max_remove: usize,
    max_bytes: u64,
) -> (StaleTmpGcStats, Option<PruneWarning>) {
    let mut stats = StaleTmpGcStats::default();
    let Ok(entries) = std::fs::read_dir(package_dir) else {
        return (stats, None);
    };
    let mut staging: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        if staging.len() >= max_scan {
            stats.skipped += 1;
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(STAGING_PREFIX) {
            continue;
        }
        stats.scanned += 1;
        let path = entry.path();
        // Only dirs are staging copies; stray files with the prefix are left
        // alone (fail-closed, never delete what the installer did not make).
        if !path.is_dir() {
            stats.skipped += 1;
            continue;
        }
        staging.push(path);
    }
    staging.sort();
    let mut first_error: Option<String> = None;
    for path in staging {
        if stats.removed >= max_remove || stats.bytes_removed >= max_bytes {
            stats.skipped += 1;
            continue;
        }
        if !is_stale_tmp(&path, max_age_secs) {
            stats.skipped += 1;
            continue;
        }
        let remaining = max_bytes.saturating_sub(stats.bytes_removed);
        let size = dir_size_bounded(&path, remaining);
        // If even the bounded size already exceeds the remaining budget,
        // skip the rest rather than blowing the byte bound.
        if size > remaining {
            stats.skipped += 1;
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                stats.removed += 1;
                stats.bytes_removed = stats.bytes_removed.saturating_add(size);
            }
            Err(error) => {
                stats.skipped += 1;
                if first_error.is_none() {
                    first_error = Some(format!("cannot GC stale '{}': {error}", path.display()));
                }
            }
        }
    }
    let warning = first_error.map(PruneWarning::new);
    (stats, warning)
}

/// Whether a staging dir is old enough to be considered crashed leftovers.
fn is_stale_tmp(path: &Path, max_age_secs: u64) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let now = std::time::SystemTime::now();
    let Ok(elapsed) = now.duration_since(modified) else {
        // Future mtime (clock skew) or same instant: treat as fresh.
        return false;
    };
    elapsed.as_secs() >= max_age_secs
}

/// Approximate dir size, stopping early once `budget` is exceeded.
///
/// Uses `symlink_metadata` so symlinks are never followed; unreadable entries
/// count as 0 rather than failing the whole GC pass.
fn dir_size_bounded(path: &Path, budget: u64) -> u64 {
    fn walk(path: &Path, acc: &mut u64, budget: u64) {
        if *acc > budget {
            return;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            if *acc > budget {
                return;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() && !meta.file_type().is_symlink() {
                walk(&path, acc, budget);
            } else if meta.is_file() {
                *acc = acc.saturating_add(meta.len());
            }
        }
    }
    let mut acc = 0u64;
    walk(path, &mut acc, budget);
    acc
}

#[cfg(unix)]
fn harden_tree(root: &Path) -> Result<(), PackageOpError> {
    use std::os::unix::fs::PermissionsExt as _;
    if root.is_dir() {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(STORE_DIR_MODE)).map_err(
            |error| PackageOpError::Store(format!("cannot set store dir mode: {error}")),
        )?;
    }
    for entry in std::fs::read_dir(root).map_err(|error| {
        PackageOpError::Store(format!("cannot read '{}': {error}", root.display()))
    })? {
        let entry =
            entry.map_err(|error| PackageOpError::Store(format!("cannot read entry: {error}")))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| PackageOpError::Store(format!("cannot stat entry: {error}")))?;
        if metadata.is_dir() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(STORE_DIR_MODE))
                .map_err(|error| {
                    PackageOpError::Store(format!("cannot set store dir mode: {error}"))
                })?;
            harden_tree(&path)?;
        } else if metadata.is_file() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(STORE_FILE_MODE))
                .map_err(|error| {
                    PackageOpError::Store(format!("cannot set store file mode: {error}"))
                })?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn harden_tree(_root: &Path) -> Result<(), PackageOpError> {
    Ok(())
}

#[cfg(unix)]
fn harden_dirs(store_root: &Path, relative: &[&str]) -> Result<(), PackageOpError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut path = store_root.to_path_buf();
    for component in relative {
        path.push(component);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(STORE_DIR_MODE)).map_err(
            |error| PackageOpError::Store(format!("cannot set store dir mode: {error}")),
        )?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn harden_dirs(_store_root: &Path, _relative: &[&str]) -> Result<(), PackageOpError> {
    Ok(())
}

#[cfg(unix)]
fn harden_index(store_root: &Path) -> Result<(), PackageOpError> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = store_root.join(resolution::CURRENT_POINTER_FILE);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(STORE_FILE_MODE))
        .map_err(|error| PackageOpError::Store(format!("cannot set index mode: {error}")))
}

#[cfg(not(unix))]
fn harden_index(_store_root: &Path) -> Result<(), PackageOpError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "bitty-package-{tag}-{}-{}",
            std::process::id(),
            STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        base
    }

    fn write_plugin(root: &Path, id: &str, version: &str, capability: Option<&str>) -> PathBuf {
        let package = root.join("plugin");
        let _ = std::fs::remove_dir_all(&package);
        std::fs::create_dir_all(package.join("lua")).expect("lua dir");
        let capability = capability
            .map(|capability| format!("{capability} = true\n"))
            .unwrap_or_default();
        let manifest = format!(
            "[plugin]\nid = \"{id}\"\nname = \"Fixture\"\nversion = \"{version}\"\n\
             description = \"fixture\"\n\n[compat]\nbitty = \">=0.0.1,<1.0\"\nplugin-api = \"^1.0\"\n\n\
             [capabilities]\n{capability}"
        );
        std::fs::write(package.join(MANIFEST_FILE_NAME), manifest).expect("manifest");
        std::fs::write(
            package.join("lua").join("init.lua"),
            "bitty.commands.register({ id = \"".to_string()
                + id
                + ":greet\", run = function() return \"hi\" end })\n",
        )
        .expect("init");
        package
    }

    /// Install with auto-approved consent (most fixtures add no capability).
    fn install(store: &Path, source: &Path) -> InstallReport {
        install_local_dir(store, source, &LocalInstallOptions::default(), &mut |_| {
            Ok(true)
        })
        .expect("install")
        .expect("approved")
    }

    #[test]
    fn install_then_records_and_resolves() {
        let scratch = scratch("install");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.fixture", "1.0.0", None);
        let report = install(&store, &source);
        assert_eq!(report.plugin_id, "xuepoo.fixture");
        assert_eq!(report.version, "1.0.0");
        assert!(!report.updated);
        assert_eq!(report.granted.len(), 0);
        assert!(report.added.is_empty());
        assert_eq!(report.root, "packages/xuepoo.fixture/1.0.0");

        let packages = store.join("packages/xuepoo.fixture/1.0.0");
        assert!(packages.join(MANIFEST_FILE_NAME).is_file());
        assert!(packages.join("lua/init.lua").is_file());

        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].source_class, SourceClass::LocalPath);
        assert!(records[0].enabled);
        let package = resolution::resolve_record(&store, &records[0]).expect("resolve");
        assert!(package.unverified, "local-path provenance stays unverified");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn install_is_idempotent_and_update_replaces_version() {
        let scratch = scratch("update");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.fixture", "1.0.0", None);
        install(&store, &source);
        let again = install(&store, &source);
        assert!(again.updated);
        assert_eq!(again.previous_version.as_deref(), Some("1.0.0"));

        let source_v2 = write_plugin(&scratch, "xuepoo.fixture", "2.0.0", None);
        let update = install(&store, &source_v2);
        assert!(update.updated);
        assert_eq!(update.previous_version.as_deref(), Some("1.0.0"));
        assert!(store.join("packages/xuepoo.fixture/2.0.0").is_dir());
        assert!(
            store.join("packages/xuepoo.fixture/1.0.0").is_dir(),
            "previous version is retained for rollback"
        );
        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records[0].version, "2.0.0");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// Write a plugin with the same Lua body but a different `compat.bitty` range
    /// so the manifest hash differs while content_digest is identical.
    /// (`description` is NOT included in `canonical_bytes`, so it does not change
    /// the manifest hash; changing `compat.bitty` does.)
    fn write_plugin_alt_compat(root: &Path, id: &str, version: &str) -> PathBuf {
        let package = root.join("plugin-alt");
        let _ = std::fs::remove_dir_all(&package);
        std::fs::create_dir_all(package.join("lua")).expect("lua dir");
        // Changed compat range (">=0.0.1,<2.0" vs the standard ">=0.0.1,<1.0")
        // so manifest_hash differs but Lua body (content_digest) is identical.
        let manifest = format!(
            "[plugin]\nid = \"{id}\"\nname = \"Fixture\"\nversion = \"{version}\"\n\
             description = \"fixture\"\n\n[compat]\nbitty = \">=0.0.1,<2.0\"\nplugin-api = \"^1.0\"\n\n\
             [capabilities]\n"
        );
        std::fs::write(package.join(MANIFEST_FILE_NAME), manifest).expect("manifest");
        // Exactly the same Lua body as write_plugin so content_digest is identical.
        std::fs::write(
            package.join("lua").join("init.lua"),
            "bitty.commands.register({ id = \"".to_string()
                + id
                + ":greet\", run = function() return \"hi\" end })\n",
        )
        .expect("init");
        package
    }

    #[test]
    fn same_version_idempotent_reinstall_preserves_manifest_hash() {
        // PLUG-REG-007: reinstalling the exact same source (same content_digest
        // AND same manifest_hash) succeeds with an `updated` report and leaves
        // the index record consistent.
        let scratch = scratch("plug-reg-007-ok");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.plug007ok", "1.0.0", None);
        let first = install(&store, &source);
        let second = install(&store, &source);
        assert!(second.updated, "reinstall reports updated");
        assert_eq!(second.manifest_hash, first.manifest_hash);
        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records[0].manifest_hash, first.manifest_hash);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn same_version_manifest_change_replaces_after_consent() {
        // Issue #2439 (was PLUG-REG-007): a reinstall that changes the manifest
        // but preserves the Lua body now replaces the whole version tree after
        // consent, so the stored body and the recorded hash stay consistent.
        // A declined prompt commits nothing.
        let scratch = scratch("plug-reg-007-replace");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.plug007rej", "1.0.0", None);
        let first = install(&store, &source);
        let original_hash = first.manifest_hash.clone();
        let original_dir = store.join("packages/xuepoo.plug007rej/1.0.0");

        // Build an alt version: same Lua body, different compat range -> different manifest_hash.
        let source_alt = write_plugin_alt_compat(&scratch, "xuepoo.plug007rej", "1.0.0");

        // Decline: nothing changes.
        let declined = install_local_dir(
            &store,
            &source_alt,
            &LocalInstallOptions::default(),
            &mut |request| {
                assert!(
                    request.replacement.is_some(),
                    "same-version change must offer a repair prompt"
                );
                Ok(false)
            },
        )
        .expect("decline is not an error");
        assert!(declined.is_none(), "decline commits nothing");
        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records[0].manifest_hash, original_hash);
        assert!(original_dir.join("bitty-plugin.toml").is_file());

        // Approve: the tree is replaced and the new hash is recorded.
        let replaced = install_local_dir(
            &store,
            &source_alt,
            &LocalInstallOptions::default(),
            &mut |request| {
                assert!(request.replacement.is_some());
                Ok(true)
            },
        )
        .expect("replace")
        .expect("approved");
        assert!(replaced.updated);
        assert!(replaced.content_replaced);
        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records[0].manifest_hash, replaced.manifest_hash);
        assert_ne!(records[0].manifest_hash, original_hash);
        resolution::resolve_record(&store, &records[0]).expect("replaced tree resolves");

        // `--force` (force_repair) replaces without a prompt.
        let source_alt2 = write_plugin_alt_compat(&scratch, "xuepoo.plug007rej", "1.0.0");
        std::fs::write(source_alt2.join("lua/forced.lua"), b"return {}\n").expect("force diff");
        let forced = install_local_dir(
            &store,
            &source_alt2,
            &LocalInstallOptions {
                force_repair: true,
                ..LocalInstallOptions::default()
            },
            &mut |_| panic!("force_repair must not prompt"),
        )
        .expect("force replace")
        .expect("approved");
        assert!(forced.content_replaced);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn unchanged_reinstall_preserves_retained_predecessor() {
        // PLUG-REG-008: installing v1, upgrading to v2, then reinstalling v2
        // must NOT advance retention and must NOT prune the retained v1 tree.
        let scratch = scratch("plug-reg-008");
        let store = scratch.join("store");

        // 1. Initial install of v1.0.0
        let source_v1 = write_plugin(&scratch, "xuepoo.reinstall", "1.0.0", None);
        install(&store, &source_v1);
        assert!(store.join("packages/xuepoo.reinstall/1.0.0").is_dir());

        // 2. Upgrade to v2.0.0: v1.0.0 is retained as predecessor.
        let source_v2 = write_plugin(&scratch, "xuepoo.reinstall", "2.0.0", None);
        let update = install(&store, &source_v2);
        assert!(update.updated);
        assert_eq!(update.previous_version.as_deref(), Some("1.0.0"));
        assert!(store.join("packages/xuepoo.reinstall/2.0.0").is_dir());
        assert!(
            store.join("packages/xuepoo.reinstall/1.0.0").is_dir(),
            "v1.0.0 must be retained as predecessor after update to v2.0.0"
        );

        // 3. Unchanged reinstall of v2.0.0: must NOT prune v1.0.0.
        let reinstall = install(&store, &source_v2);
        assert!(reinstall.updated);
        assert!(
            store.join("packages/xuepoo.reinstall/2.0.0").is_dir(),
            "current v2.0.0 tree must remain"
        );
        assert!(
            store.join("packages/xuepoo.reinstall/1.0.0").is_dir(),
            "retained predecessor v1.0.0 must remain on unchanged reinstall"
        );

        // 4. Further upgrade to v3.0.0: v2.0.0 is retained as predecessor, v1.0.0 is pruned.
        let source_v3 = write_plugin(&scratch, "xuepoo.reinstall", "3.0.0", None);
        let update3 = install(&store, &source_v3);
        assert!(update3.updated);
        assert!(store.join("packages/xuepoo.reinstall/3.0.0").is_dir());
        assert!(
            store.join("packages/xuepoo.reinstall/2.0.0").is_dir(),
            "v2.0.0 must be retained as predecessor after update to v3.0.0"
        );
        assert!(
            !store.join("packages/xuepoo.reinstall/1.0.0").exists(),
            "older v1.0.0 must be pruned after advancing to v3.0.0"
        );

        // 5. Unchanged reinstall of v3.0.0: preserves retained v2.0.0.
        let reinstall3 = install(&store, &source_v3);
        assert!(reinstall3.updated);
        assert!(store.join("packages/xuepoo.reinstall/3.0.0").is_dir());
        assert!(
            store.join("packages/xuepoo.reinstall/2.0.0").is_dir(),
            "retained predecessor v2.0.0 must remain on unchanged reinstall of v3.0.0"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn concurrent_two_writer_unrelated_updates_preserved() {
        // PLUG-REG-011: deterministic two-writer model where Writer 2 commits an
        // unrelated plugin while Writer 1 is awaiting consent. When Writer 1
        // commits, it reloads under protection and preserves Writer 2's plugin.
        let scratch = scratch("plug-reg-011-unrelated");
        let store = scratch.join("store");

        // Give source_a a capability so consent() is actually invoked.
        let source_a = write_plugin(
            &scratch.join("src_a"),
            "xuepoo.plugin_a",
            "1.0.0",
            Some("platform.notify"),
        );
        let source_b = write_plugin(&scratch.join("src_b"), "xuepoo.plugin_b", "1.0.0", None);

        // Writer 1 installs plugin A, but while awaiting consent, Writer 2
        // commits plugin B into the store.
        let report_a = install_local_dir(
            &store,
            &source_a,
            &LocalInstallOptions::default(),
            &mut |_consent_req| {
                // Interleaved Writer 2 commit:
                let report_b = install(&store, &source_b);
                assert_eq!(report_b.plugin_id, "xuepoo.plugin_b");
                Ok(true)
            },
        )
        .expect("Writer 1 should succeed")
        .expect("install report");

        assert_eq!(report_a.plugin_id, "xuepoo.plugin_a");

        // Both plugin A and plugin B must be present in current.json.
        let records = resolution::load_index(&store).expect("index");
        let ids: Vec<&str> = records.iter().map(|r| r.plugin_id.as_str()).collect();
        assert!(
            ids.contains(&"xuepoo.plugin_a") && ids.contains(&"xuepoo.plugin_b"),
            "both unrelated plugins must be preserved in index, found: {ids:?}"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn concurrent_two_writer_same_plugin_conflict_fails_closed() {
        // PLUG-REG-011: deterministic two-writer model where Writer 2 mutates
        // the SAME plugin while Writer 1 is awaiting consent. Writer 1 revalidates
        // baseline under protection and returns a retryable conflict.
        let scratch = scratch("plug-reg-011-conflict");
        let store = scratch.join("store");

        // Give source_v1 a capability so consent() is actually invoked.
        let source_v1 = write_plugin(
            &scratch.join("src_v1"),
            "xuepoo.conflict",
            "1.0.0",
            Some("platform.notify"),
        );
        let source_v2 = write_plugin(&scratch.join("src_v2"), "xuepoo.conflict", "2.0.0", None);

        // Writer 1 begins install of v1.0.0. During consent, Writer 2 commits v2.0.0.
        let err = install_local_dir(
            &store,
            &source_v1,
            &LocalInstallOptions::default(),
            &mut |_consent_req| {
                // Interleaved Writer 2 commits v2.0.0 for the same plugin:
                let report_v2 = install(&store, &source_v2);
                assert_eq!(report_v2.version, "2.0.0");
                Ok(true)
            },
        )
        .expect_err("Writer 1 must detect concurrent conflict on same plugin");

        let PackageOpError::Store(msg) = err else {
            panic!("expected Store conflict error, got: {err:?}");
        };
        assert!(
            msg.contains("concurrent transaction conflict"),
            "error should indicate concurrent conflict, got: {msg}"
        );

        // Store index must retain Writer 2's commit (v2.0.0), not overwritten by Writer 1.
        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].plugin_id, "xuepoo.conflict");
        assert_eq!(records[0].version, "2.0.0");

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn disabled_state_preserved_across_version_update_and_reinstall() {
        // PLUG-REG-009: preserve existing disabled state across version updates
        // and unchanged reinstalls unless explicitly overridden.
        let scratch = scratch("plug-reg-009-preserve");
        let store = scratch.join("store");
        let source_v1 = write_plugin(&scratch.join("src_v1"), "xuepoo.persist", "1.0.0", None);
        let source_v2 = write_plugin(&scratch.join("src_v2"), "xuepoo.persist", "2.0.0", None);

        // 1. Fresh install defaults to enabled.
        let report_v1 = install(&store, &source_v1);
        assert_eq!(report_v1.version, "1.0.0");
        let records = resolution::load_index(&store).expect("index");
        assert!(records[0].enabled, "fresh install defaults to enabled");

        // 2. Explicitly disable the plugin.
        assert!(set_enabled(&store, "xuepoo.persist", false).expect("disable"));
        let records = resolution::load_index(&store).expect("index");
        assert!(!records[0].enabled, "plugin must be disabled");

        // 3. Unchanged reinstall preserves disabled state.
        let reinstall = install(&store, &source_v1);
        assert_eq!(reinstall.version, "1.0.0");
        let records = resolution::load_index(&store).expect("index");
        assert!(
            !records[0].enabled,
            "unchanged reinstall must preserve disabled state"
        );

        // 4. Version update preserves disabled state.
        let update = install(&store, &source_v2);
        assert_eq!(update.version, "2.0.0");
        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records[0].version, "2.0.0");
        assert!(
            !records[0].enabled,
            "version update must preserve disabled state"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn disabled_state_preserved_across_capability_narrowing() {
        // PLUG-REG-009: capability narrowing preserves disabled state.
        let scratch = scratch("plug-reg-009-narrow");
        let store = scratch.join("store");
        let wider = write_plugin(
            &scratch.join("src_wide"),
            "xuepoo.narrow",
            "1.0.0",
            Some("platform.notify"),
        );
        let narrower = write_plugin(&scratch.join("src_narrow"), "xuepoo.narrow", "1.1.0", None);

        // Fresh install with consent.
        install_local_dir(&store, &wider, &LocalInstallOptions::default(), &mut |_| {
            Ok(true)
        })
        .expect("install")
        .expect("approved");

        // Disable plugin.
        assert!(set_enabled(&store, "xuepoo.narrow", false).expect("disable"));
        let records = resolution::load_index(&store).expect("index");
        assert!(!records[0].enabled);
        assert_eq!(records[0].granted, vec!["platform.notify".to_string()]);

        // Narrow capabilities to none with v1.1.0.
        install_local_dir(
            &store,
            &narrower,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect("narrowing install")
        .expect("approved");

        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records[0].version, "1.1.0");
        assert!(records[0].granted.is_empty(), "capabilities narrowed");
        assert!(
            !records[0].enabled,
            "disabled state must be preserved across capability narrowing"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn explicit_override_forces_enabled_state_and_staged_disabled_policy() {
        // PLUG-REG-009: explicit_override overrides existing state;
        // staged_and_disabled policy installs fresh plugin in disabled state.
        let scratch = scratch("plug-reg-009-explicit");
        let store = scratch.join("store");
        let source_v1 = write_plugin(&scratch.join("src_v1"), "xuepoo.override", "1.0.0", None);
        let source_v2 = write_plugin(&scratch.join("src_v2"), "xuepoo.override", "2.0.0", None);

        // 1. Fresh install using staged_and_disabled() begins disabled per onboarding rule 7.
        install_local_dir(
            &store,
            &source_v1,
            &LocalInstallOptions::staged_and_disabled(),
            &mut |_| Ok(true),
        )
        .expect("install")
        .expect("approved");

        let records = resolution::load_index(&store).expect("index");
        assert!(
            !records[0].enabled,
            "fresh install with staged_and_disabled must be disabled"
        );

        // 2. Explicit override forces enabled on update.
        let opts_force_enable = LocalInstallOptions {
            enable: false,
            explicit_override: Some(true),
            ..LocalInstallOptions::default()
        };
        install_local_dir(&store, &source_v2, &opts_force_enable, &mut |_| Ok(true))
            .expect("update")
            .expect("approved");

        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records[0].version, "2.0.0");
        assert!(
            records[0].enabled,
            "explicit_override: Some(true) must force enabled"
        );

        // 3. Explicit override forces disabled on reinstall.
        let opts_force_disable = LocalInstallOptions {
            enable: true,
            explicit_override: Some(false),
            ..LocalInstallOptions::default()
        };
        install_local_dir(&store, &source_v2, &opts_force_disable, &mut |_| Ok(true))
            .expect("reinstall")
            .expect("approved");

        let records = resolution::load_index(&store).expect("index");
        assert!(
            !records[0].enabled,
            "explicit_override: Some(false) must force disabled"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn capability_increase_requires_consent_and_narrowing_carries_forward() {
        let scratch = scratch("consent");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.caps", "1.0.0", None);
        install(&store, &source);

        let wider = write_plugin(&scratch, "xuepoo.caps", "1.1.0", Some("platform.notify"));
        // Denial commits nothing and reports `None`.
        let denied =
            install_local_dir(&store, &wider, &LocalInstallOptions::default(), &mut |_| {
                Ok(false)
            })
            .expect("denied install");
        assert!(denied.is_none());
        assert!(!store.join("packages/xuepoo.caps/1.1.0").exists());
        assert_eq!(
            resolution::load_index(&store).expect("index")[0].version,
            "1.0.0"
        );

        // Aborted consent also commits nothing.
        let aborted =
            install_local_dir(&store, &wider, &LocalInstallOptions::default(), &mut |_| {
                Err(PackageOpError::ConsentAborted("eof".to_string()))
            })
            .expect_err("aborted consent");
        assert!(matches!(aborted, PackageOpError::ConsentAborted(_)));
        assert!(!store.join("packages/xuepoo.caps/1.1.0").exists());

        let approved =
            install_local_dir(&store, &wider, &LocalInstallOptions::default(), &mut |_| {
                Ok(true)
            })
            .expect("approved install")
            .expect("approved");
        assert_eq!(approved.added, vec!["platform.notify".to_string()]);

        let narrower = write_plugin(&scratch, "xuepoo.caps", "1.2.0", None);
        let carried = install(&store, &narrower);
        assert!(carried.added.is_empty());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn consent_snapshot_ignores_source_swap() {
        let scratch = scratch("toctou");
        let store = scratch.join("store");
        let base = write_plugin(&scratch, "xuepoo.toctou", "1.0.0", None);
        install(&store, &base);

        let source = write_plugin(&scratch, "xuepoo.toctou", "2.0.0", Some("platform.notify"));
        let mut swapped = false;
        let report = install_local_dir(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |request: &ConsentRequest| {
                assert_eq!(request.added, vec!["platform.notify".to_string()]);
                // Attacker swaps the source for a manifest that also requests
                // `terminal.semantic-read` while the prompt is "open".
                let expanded = "[plugin]\nid = \"xuepoo.toctou\"\nname = \"Fixture\"\nversion = \"2.0.0\"\n\
                     description = \"swapped\"\n\n[compat]\nbitty = \">=0.0.1,<1.0\"\nplugin-api = \"^1.0\"\n\n\
                     [capabilities]\nplatform.notify = true\nterminal.semantic-read = true\n";
                std::fs::write(source.join(MANIFEST_FILE_NAME), expanded).expect("swap");
                swapped = true;
                Ok(true)
            },
        )
        .expect("install")
        .expect("approved");
        assert!(swapped, "the consent callback must run");

        // The committed snapshot is the reviewed one, not the swapped source.
        assert_eq!(report.granted, vec!["platform.notify".to_string()]);
        assert_eq!(report.added, vec!["platform.notify".to_string()]);
        let staged =
            std::fs::read_to_string(store.join("packages/xuepoo.toctou/2.0.0/bitty-plugin.toml"))
                .expect("staged manifest");
        assert!(
            !staged.contains("terminal.semantic-read"),
            "the swapped capability must never be committed: {staged}"
        );
        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records[0].granted, vec!["platform.notify".to_string()]);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn bundled_id_is_reserved() {
        let scratch = scratch("bundled");
        let store = scratch.join("store");
        // #1572 / CTX-0994: shell-integration is the remaining bundled id.
        let source = write_plugin(&scratch, "bitty-terminal.shell-integration", "1.0.0", None);
        let error = install_local_dir(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect_err("bundled ids must not be shadowed");
        assert!(matches!(error, PackageOpError::BundledIdReserved { .. }));
        assert!(!store.join("packages").exists());
        // Retired workspace id is no longer reserved: shadowing it installs.
        let ws_source = write_plugin(&scratch, "bitty-terminal.workspace", "1.0.0", None);
        install_local_dir(
            &store,
            &ws_source,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect("retired workspace id installs as external")
        .expect("approved");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn vcs_metadata_is_skipped_by_the_source_scan() {
        let scratch = scratch("vcs");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.vcs", "1.0.0", None);
        // A native artifact inside VCS metadata would fail the module scan if
        // VCS directories were counted; the installer skips them.
        std::fs::create_dir_all(source.join("lua/.git")).expect("vcs dir");
        std::fs::write(source.join("lua/.git/evil.so"), b"native").expect("native");
        install(&store, &source);
        assert!(!store.join("packages/xuepoo.vcs/1.0.0/lua/.git").exists());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_source_entry_is_rejected() {
        let scratch = scratch("symlink");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.symlink", "1.0.0", None);
        std::os::unix::fs::symlink(source.join("lua/init.lua"), source.join("lua/link.lua"))
            .expect("symlink");
        let error = install_local_dir(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect_err("symlinked source entries must be rejected");
        assert!(matches!(error, PackageOpError::ModuleTree { .. }));
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn incompatible_host_range_is_rejected() {
        let scratch = scratch("compat");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.future", "1.0.0", None);
        let manifest = std::fs::read_to_string(source.join(MANIFEST_FILE_NAME))
            .expect("manifest")
            .replace("plugin-api = \"^1.0\"", "plugin-api = \">=2.0,<3.0\"");
        std::fs::write(source.join(MANIFEST_FILE_NAME), manifest).expect("rewrite");
        let error = install_local_dir(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect_err("incompatible api range must fail");
        assert!(matches!(error, PackageOpError::Incompatible { .. }));
        let _ = std::fs::remove_dir_all(&scratch);
    }

    fn write_plugin_with_compat(
        root: &Path,
        id: &str,
        version: &str,
        bitty_req: &str,
        api_req: &str,
    ) -> PathBuf {
        let package = root.join("plugin-compat");
        let _ = std::fs::remove_dir_all(&package);
        std::fs::create_dir_all(package.join("lua")).expect("lua dir");
        let manifest = format!(
            "[plugin]\nid = \"{id}\"\nname = \"Fixture\"\nversion = \"{version}\"\n\
             description = \"fixture\"\n\n[compat]\nbitty = \"{bitty_req}\"\nplugin-api = \"{api_req}\"\n"
        );
        std::fs::write(package.join(MANIFEST_FILE_NAME), manifest).expect("manifest");
        std::fs::write(
            package.join("lua").join("init.lua"),
            "bitty.commands.register({ id = \"".to_string()
                + id
                + ":greet\", run = function() return \"hi\" end })\n",
        )
        .expect("init");
        package
    }

    #[test]
    fn install_then_upgrade_host_fails_resolve_closed() {
        // CTX-0416: install when the range includes the running host, then
        // simulate a host upgrade where the stored range no longer applies.
        // Both bitty and plugin-api upgrades must fail closed with a typed
        // incompatible state and never reach a VM.
        let scratch = scratch("upgrade");
        let store = scratch.join("store");
        // Narrow floor that includes the current dev host (0.0.20) but not
        // the simulated 0.1.0 upgrade; api ^1.0 includes 1.0.0 but not 2.0.0.
        let source = write_plugin_with_compat(
            &scratch,
            "xuepoo.upgrade",
            "1.0.0",
            ">=0.0.1,<0.0.99",
            "^1.0",
        );
        install(&store, &source);
        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records.len(), 1);
        // Running host still resolves.
        resolution::resolve_record(&store, &records[0]).expect("current host resolves");

        // Simulated bitty upgrade to 0.1.0 no longer satisfies <0.0.99.
        let upgraded_bitty =
            resolution::resolve_record_with_hosts(&store, &records[0], "0.1.0", host_api_version())
                .expect_err("upgraded bitty must fail closed");
        assert!(
            matches!(
                upgraded_bitty,
                crate::plugin_runtime::PluginRuntimeError::Incompatible { ref field, .. } if field == "compat.bitty"
            ),
            "expected typed compat.bitty incompatible, got {upgraded_bitty}"
        );

        // Simulated plugin-api upgrade to 2.0.0 no longer satisfies ^1.0.
        let upgraded_api = resolution::resolve_record_with_hosts(
            &store,
            &records[0],
            host_bitty_version(),
            "2.0.0",
        )
        .expect_err("upgraded api must fail closed");
        assert!(
            matches!(
                upgraded_api,
                crate::plugin_runtime::PluginRuntimeError::Incompatible { ref field, .. } if field == "compat.plugin-api"
            ),
            "expected typed compat.plugin-api incompatible, got {upgraded_api}"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn evaluate_compat_uses_closed_grammar() {
        // Closed grammar: wildcards and disjunctions are rejected at parse.
        assert!(evaluate_compat(Some("*"), None, "0.0.20", "1.0.0").is_err());
        assert!(evaluate_compat(Some(">=0.0.1 || <1.0"), None, "0.0.20", "1.0.0").is_err());
        // Partial comparators are padded (accepted spelling >=0.1,<1.0).
        assert!(evaluate_compat(Some(">=0.1,<1.0"), Some("^1.0"), "0.1.0", "1.0.0").is_ok());
        assert!(evaluate_compat(Some(">=0.1,<1.0"), Some("^1.0"), "0.0.20", "1.0.0").is_err());
        // CTX-0493: reported official-entry spelling zero-pads to X.Y.0 too.
        assert!(evaluate_compat(Some(">=0.5,<1.0"), Some("^1.0"), "0.6.0", "1.0.0").is_ok());
        assert!(evaluate_compat(Some(">=0.5,<1.0"), Some("^1.0"), "0.4.9", "1.0.0").is_err());
        // CTX-0493: unrepresentable caret bound is a clean grammar error, not a panic.
        assert!(evaluate_compat(Some("^4294967295"), None, "0.0.20", "1.0.0").is_err());
        // Absent fields mean no constraint.
        assert!(evaluate_compat(None, None, "0.0.20", "1.0.0").is_ok());
        let _ = host_bitty_version();
        let _ = host_api_version();
    }

    #[test]
    fn missing_entry_point_is_rejected() {
        let scratch = scratch("entry");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.empty", "1.0.0", None);
        std::fs::remove_file(source.join("lua/init.lua")).expect("remove entry");
        let error = install_local_dir(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect_err("missing init.lua must fail");
        assert!(matches!(error, PackageOpError::ModuleTree { .. }));
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn enable_disable_and_uninstall() {
        let scratch = scratch("lifecycle");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.life", "1.0.0", None);
        install(&store, &source);
        assert!(set_enabled(&store, "xuepoo.life", false).expect("disable"));
        assert!(!set_enabled(&store, "xuepoo.life", false).expect("no-op"));
        let records = resolution::load_index(&store).expect("index");
        assert!(!records[0].enabled);
        let report = uninstall(&store, "xuepoo.life").expect("uninstall");
        assert!(report.tree_removed);
        assert!(!store.join("packages/xuepoo.life").exists());
        assert!(resolution::load_index(&store).expect("index").is_empty());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn tampered_staged_content_fails_closed() {
        let scratch = scratch("tamper");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.tamper", "1.0.0", None);
        install(&store, &source);
        std::fs::write(
            store.join("packages/xuepoo.tamper/1.0.0/lua/init.lua"),
            "-- tampered",
        )
        .expect("tamper");
        let records = resolution::load_index(&store).expect("index");
        assert!(
            resolution::resolve_record(&store, &records[0]).is_err(),
            "staged digest mismatch is a store integrity failure"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn same_version_different_content_replaces_after_consent() {
        // Issue #2439: a rebuilt source checkout at the same manifest version
        // must have a recovery path, not a bare "different content" refusal.
        let scratch = scratch("collision");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.collide", "1.0.0", None);
        let first = install(&store, &source);
        std::fs::write(source.join("lua/extra.lua"), "return {}\n").expect("extra");

        let declined = install_local_dir(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |request| {
                let replacement = request
                    .replacement
                    .as_ref()
                    .expect("same-version content change prompts");
                assert_eq!(replacement.kind, ReplacementKind::SameVersionContent);
                assert!(replacement.detail.contains("different content"));
                Ok(false)
            },
        )
        .expect("decline is not an error");
        assert!(declined.is_none());
        assert!(
            !store
                .join("packages/xuepoo.collide/1.0.0/lua/extra.lua")
                .exists(),
            "declined replacement leaves the old tree untouched"
        );

        let replaced = install_local_dir(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect("replace")
        .expect("approved");
        assert!(replaced.content_replaced);
        assert_ne!(replaced.content_digest, first.content_digest);
        assert!(
            store
                .join("packages/xuepoo.collide/1.0.0/lua/extra.lua")
                .is_file(),
            "approved replacement writes the new tree"
        );
        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records[0].content_digest, replaced.content_digest);
        resolution::resolve_record(&store, &records[0]).expect("new tree resolves");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[cfg(unix)]
    #[test]
    fn staged_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = scratch("modes");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.modes", "1.0.0", None);
        install(&store, &source);
        let file = store.join("packages/xuepoo.modes/1.0.0/lua/init.lua");
        let mode = std::fs::metadata(&file).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let index = store.join(resolution::CURRENT_POINTER_FILE);
        let mode = std::fs::metadata(&index)
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    fn write_git_plugin(root: &Path, id: &str, version: &str) -> PathBuf {
        let package = root.join("git-plugin");
        let _ = std::fs::remove_dir_all(&package);
        std::fs::create_dir_all(package.join("lua")).expect("lua dir");
        let manifest = format!(
            "[plugin]\nid = \"{id}\"\nname = \"Git Fixture\"\nversion = \"{version}\"\n\
             description = \"git-panel shape\"\n\n[compat]\nbitty = \">=0.0.1,<1.0\"\nplugin-api = \"^1.0\"\n\n\
             [capabilities]\npanel.provider = true\n\"process.spawn:git\" = true\n\n\
             [[capabilities.filesystem]]\naccess = \"read\"\npaths = [\"~/projects/**\"]\n\n\
             [tools.git]\nrequired = true\nversion = \">=2.30\"\n\n\
             [lazy]\ncommands = [\"{id}:open\"]\nevents = []\n"
        );
        std::fs::write(package.join(MANIFEST_FILE_NAME), manifest).expect("manifest");
        std::fs::write(
            package.join("lua").join("init.lua"),
            "bitty.commands.register({ id = \"".to_string()
                + id
                + ":open\", run = function() return \"hi\" end })\n",
        )
        .expect("init");
        package
    }

    #[test]
    fn install_git_panel_shape_succeeds_fail_closed_before() {
        // The three CTX-0400 exit-4 layers (quoted param keys,
        // `[[capabilities.filesystem]]`, `[tools.git]`) must now install.
        let scratch = scratch("git-tools");
        let store = scratch.join("store");
        let source = write_git_plugin(&scratch, "xuepoo.gitpanel", "0.1.0");
        let report = install(&store, &source);
        assert_eq!(report.plugin_id, "xuepoo.gitpanel");
        assert!(report.granted.contains(&"process.spawn:git".to_string()));
        assert!(report.granted.contains(&"panel.provider".to_string()));
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn install_unknown_tools_section_fails_closed() {
        let scratch = scratch("unknown-tool");
        let store = scratch.join("store");
        let package = scratch.join("evil-plugin");
        let _ = std::fs::remove_dir_all(&package);
        std::fs::create_dir_all(package.join("lua")).expect("lua dir");
        let manifest = "[plugin]\nid = \"xuepoo.evil\"\nname = \"E\"\nversion = \"0.1.0\"\n\
             description = \"evil\"\n\n[capabilities]\n\"process.spawn:rg\" = true\n\n\
             [tools.rg]\nrequired = true\nversion = \">=13\"\n";
        std::fs::write(package.join(MANIFEST_FILE_NAME), manifest).expect("manifest");
        std::fs::write(package.join("lua").join("init.lua"), "return {}\n").expect("init");
        let error = install_local_dir(
            &store,
            &package,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect_err("unknown [tools.rg] must fail closed");
        assert!(
            matches!(error, PackageOpError::Manifest { .. }),
            "expected manifest denial, got: {error}"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn install_spawn_without_tool_fails_closed() {
        let scratch = scratch("spawn-no-tool");
        let store = scratch.join("store");
        let package = scratch.join("notool-plugin");
        let _ = std::fs::remove_dir_all(&package);
        std::fs::create_dir_all(package.join("lua")).expect("lua dir");
        let manifest = "[plugin]\nid = \"xuepoo.notool\"\nname = \"N\"\nversion = \"0.1.0\"\n\
             description = \"no tool\"\n\n[capabilities]\n\"process.spawn:git\" = true\n";
        std::fs::write(package.join(MANIFEST_FILE_NAME), manifest).expect("manifest");
        std::fs::write(package.join("lua").join("init.lua"), "return {}\n").expect("init");
        let error = install_local_dir(
            &store,
            &package,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect_err("spawn without [tools.git] must fail closed");
        assert!(matches!(error, PackageOpError::Manifest { .. }));
        let _ = std::fs::remove_dir_all(&scratch);
    }

    // CTX-0417: bounded GC of stale `.tmp-*` quarantine dirs.
    #[test]
    fn stale_tmp_gc_removes_only_stale_and_respects_bounds() {
        let scratch = scratch("gc");
        let package_dir = scratch.join("packages/xuepoo.gc");
        std::fs::create_dir_all(&package_dir).expect("package dir");
        // Two crashed staging dirs plus one real version (never collected).
        for name in [".tmp-111-0", ".tmp-222-1"] {
            let dir = package_dir.join(name);
            std::fs::create_dir_all(&dir).expect("staging dir");
            std::fs::write(dir.join("bitty-plugin.toml"), b"stale").expect("stale file");
        }
        let version = package_dir.join("1.0.0");
        std::fs::create_dir_all(version.join("lua")).expect("version dir");

        // Age 0: everything with a past mtime is stale, so both go.
        let (stats, warning) =
            gc_stale_staging_dirs_with_limits(&package_dir, 0, 64, 32, 64 * 1024 * 1024);
        assert!(warning.is_none(), "clean GC must not warn");
        assert_eq!(stats.removed, 2, "both stale dirs must go: {stats:?}");
        assert!(!package_dir.join(".tmp-111-0").exists());
        assert!(!package_dir.join(".tmp-222-1").exists());
        assert!(
            package_dir.join("1.0.0").is_dir(),
            "real versions never GCd"
        );

        // Fresh entries are kept when the age bound is huge.
        for name in [".tmp-333-0", ".tmp-444-1"] {
            let dir = package_dir.join(name);
            std::fs::create_dir_all(&dir).expect("staging dir");
        }
        let (stats, _) =
            gc_stale_staging_dirs_with_limits(&package_dir, u64::MAX, 64, 32, 64 * 1024 * 1024);
        assert_eq!(stats.removed, 0, "fresh staging must be kept: {stats:?}");
        assert!(package_dir.join(".tmp-333-0").is_dir());

        // Remove bound: only `max_remove` go per pass.
        let (stats, _) =
            gc_stale_staging_dirs_with_limits(&package_dir, 0, 64, 1, 64 * 1024 * 1024);
        assert_eq!(stats.removed, 1, "remove cap must hold: {stats:?}");

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn happy_path_install_has_no_prune_warning() {
        let scratch = scratch("happy-warn");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.happy", "1.0.0", None);
        let report = install(&store, &source);
        assert!(
            report.prune_warning.is_none(),
            "happy path must stay warning-free: {:?}",
            report.prune_warning
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[cfg(unix)]
    #[test]
    fn prune_failure_is_reported_not_fatal() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = scratch("prune-warn");
        let store = scratch.join("store");
        let source_v1 = write_plugin(&scratch, "xuepoo.prunewarn", "1.0.0", None);
        let first = install(&store, &source_v1);
        assert!(first.prune_warning.is_none());

        // A retained extra version that prune must drop, made undeletable by
        // revoking list permission on a nested dir. The parent stays writable
        // so the new install can still commit.
        let stale = store.join("packages/xuepoo.prunewarn/0.9.0");
        let nested = stale.join("lua/nested");
        std::fs::create_dir_all(&nested).expect("nested");
        std::fs::write(stale.join("bitty-plugin.toml"), b"stale").expect("stale manifest");
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o000))
            .expect("revoke nested");

        let source_v2 = write_plugin(&scratch, "xuepoo.prunewarn", "2.0.0", None);
        let report = install_local_dir(
            &store,
            &source_v2,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect("prune failure must not fail the install")
        .expect("approved");

        // The update itself succeeded and is active.
        assert_eq!(report.version, "2.0.0");
        assert!(store.join("packages/xuepoo.prunewarn/2.0.0").is_dir());
        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records[0].version, "2.0.0");
        // But the cleanup failure is typed and reported, not swallowed.
        let warning = report
            .prune_warning
            .as_ref()
            .expect("prune failure must be reported");
        assert!(
            warning.detail.contains("cannot prune"),
            "warning must name the prune failure: {}",
            warning.detail
        );
        assert!(stale.exists(), "failed prune target stays for retry");

        // Restore so the scratch cleanup can run.
        let _ = std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&scratch);
    }
    /// Backdate a directory so the doctor's staging-age gate accepts it.
    fn backdate_staging(path: &Path) {
        use std::time::{Duration, SystemTime};
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt as _;
            // A directory handle needs FILE_FLAG_BACKUP_SEMANTICS (custom
            // flags, not the access mask) plus FILE_WRITE_ATTRIBUTES so
            // `set_times` can update the timestamp.
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
            std::fs::OpenOptions::new()
                .access_mode(FILE_WRITE_ATTRIBUTES)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)
                .expect("open staging dir")
        };
        #[cfg(not(windows))]
        let file = std::fs::File::open(path).expect("open staging dir");
        let when = SystemTime::now() - Duration::from_secs(DOCTOR_STAGING_MAX_AGE_SECS + 60);
        file.set_times(std::fs::FileTimes::new().set_modified(when))
            .expect("backdate staging dir");
    }

    #[test]
    fn orphaned_manifest_only_dir_recovers_on_reinstall() {
        // Owner repro (issue #2439): manifest-only version dir, empty lua/,
        // no current.json. Reinstall must offer a repair prompt and produce a
        // complete tree + index instead of "already installed with different
        // content".
        let scratch = scratch("orphan-recover");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.orphan", "0.0.1", None);
        install(&store, &source);
        let version_dir = store.join("packages/xuepoo.orphan/0.0.1");
        std::fs::remove_file(version_dir.join("lua/init.lua")).expect("truncate lua");
        std::fs::remove_file(store.join(resolution::CURRENT_POINTER_FILE)).expect("drop index");

        let orphans = scan_orphans(&store).expect("scan");
        assert_eq!(orphans.len(), 1, "{orphans:?}");
        assert_eq!(orphans[0].kind, OrphanKind::NoIndexRecord);
        assert!(!orphans[0].complete, "manifest-only tree is incomplete");
        let inspection = inspect_store(&store).expect("inspect");
        assert!(
            inspection
                .issues
                .iter()
                .any(|issue| issue.kind == StoreIssueKind::IncompletePackage),
            "{:?}",
            inspection.issues
        );

        // Decline leaves the store untouched.
        let declined = install_local_dir(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |request| {
                assert_eq!(
                    request.replacement.as_ref().expect("repair prompt").kind,
                    ReplacementKind::Orphaned
                );
                Ok(false)
            },
        )
        .expect("decline is not an error");
        assert!(declined.is_none());
        assert!(!store.join(resolution::CURRENT_POINTER_FILE).exists());

        // Approve: complete tree + index, no orphan remains.
        let report = install_local_dir(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect("reinstall")
        .expect("approved");
        assert!(report.content_replaced);
        assert!(version_dir.join("lua/init.lua").is_file());
        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records.len(), 1);
        resolution::resolve_record(&store, &records[0]).expect("recovered package resolves");
        assert!(scan_orphans(&store).expect("scan").is_empty());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn identical_orphan_is_adopted_without_prompt() {
        // Crash window after the version rename but before the index write:
        // the tree is complete and identical, so publishing the index is
        // strictly a recovery, not a destructive replacement.
        let scratch = scratch("orphan-adopt");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.adopt", "1.0.0", None);
        install(&store, &source);
        let index = store.join(resolution::CURRENT_POINTER_FILE);
        let original = std::fs::read(&index).expect("index bytes");
        std::fs::remove_file(&index).expect("drop index");

        let report = install_local_dir(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |_| panic!("identical orphan must not prompt"),
        )
        .expect("reinstall")
        .expect("approved");
        assert!(!report.content_replaced);
        assert_eq!(std::fs::read(&index).expect("index restored"), original);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn same_version_orphan_requires_confirmation_without_force() {
        let scratch = scratch("orphan-decline");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.decline", "1.0.0", None);
        install(&store, &source);
        let target = store.join("packages/xuepoo.decline/1.0.0/lua/init.lua");
        std::fs::write(&target, b"return { broken = true }\n").expect("truncate");
        std::fs::remove_file(store.join(resolution::CURRENT_POINTER_FILE)).expect("drop index");

        let declined = install_local_dir(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |_| Ok(false),
        )
        .expect("decline")
        .is_none();
        assert!(declined);
        assert_eq!(
            std::fs::read(&target).expect("old tree kept"),
            b"return { broken = true }\n"
        );
        assert!(!store.join(resolution::CURRENT_POINTER_FILE).exists());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn corrupt_index_fails_closed_and_doctor_rebuilds_it() {
        let scratch = scratch("corrupt-index");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.corrupt", "1.0.0", None);
        install(&store, &source);
        std::fs::write(
            store.join(resolution::CURRENT_POINTER_FILE),
            b"{not an index",
        )
        .expect("corrupt index");

        // Never silently treated as an empty store.
        assert!(resolution::load_index(&store).is_err());
        let inspection = inspect_store(&store).expect("inspect");
        assert!(
            inspection
                .issues
                .iter()
                .any(|issue| issue.kind == StoreIssueKind::CorruptIndex),
            "{:?}",
            inspection.issues
        );

        let report = repair_store(&store).expect("repair");
        assert!(report.index_rebuilt);
        assert_eq!(report.records_written, 1);
        assert!(
            report
                .quarantined
                .iter()
                .any(|(from, _)| from == resolution::CURRENT_POINTER_FILE),
            "{:?}",
            report.quarantined
        );
        let records = resolution::load_index(&store).expect("rebuilt index");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].plugin_id, "xuepoo.corrupt");
        assert!(
            records[0].enabled,
            "zero-capability rebuild can stay enabled"
        );
        assert!(records[0].granted.is_empty());
        resolution::resolve_record(&store, &records[0]).expect("rebuilt package resolves");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn dangling_record_is_dropped_by_doctor() {
        let scratch = scratch("dangling");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.dangling", "1.0.0", None);
        install(&store, &source);
        std::fs::remove_dir_all(store.join("packages/xuepoo.dangling")).expect("drop tree");

        let inspection = inspect_store(&store).expect("inspect");
        assert!(
            inspection
                .issues
                .iter()
                .any(|issue| issue.kind == StoreIssueKind::DanglingRecord),
            "{:?}",
            inspection.issues
        );
        repair_store(&store).expect("repair");
        assert!(resolution::load_index(&store).expect("index").is_empty());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn newer_complete_version_repoints_stale_pointer_without_grants() {
        let scratch = scratch("stale-pointer");
        let store = scratch.join("store");
        let source_v1 = write_plugin(&scratch, "xuepoo.stale", "1.0.0", Some("platform.notify"));
        let first = install(&store, &source_v1);
        let old_records = resolution::load_index(&store).expect("index");
        let source_v2 = write_plugin(&scratch, "xuepoo.stale", "2.0.0", Some("platform.notify"));
        install(&store, &source_v2);
        // Simulate an interrupted upgrade: v2 tree is complete, but the index
        // still points at v1 (and v1 remains the predecessor).
        resolution::write_index(&store, &old_records).expect("stale pointer");

        let inspection = inspect_store(&store).expect("inspect");
        assert!(
            inspection
                .issues
                .iter()
                .any(|issue| issue.kind == StoreIssueKind::StaleVersion),
            "{:?}",
            inspection.issues
        );
        repair_store(&store).expect("repair");
        let records = resolution::load_index(&store).expect("index");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].version, "2.0.0");
        assert!(
            records[0].granted.is_empty(),
            "repair must not carry grants across a manifest change"
        );
        assert!(
            !records[0].enabled,
            "capability-requesting repair stays disabled"
        );
        assert!(
            store.join("packages/xuepoo.stale/1.0.0").is_dir(),
            "the previously active version stays as an inert predecessor"
        );
        assert_ne!(records[0].manifest_hash, first.manifest_hash);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn incomplete_active_package_is_quarantined_by_doctor() {
        let scratch = scratch("incomplete-active");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.partial", "1.0.0", None);
        install(&store, &source);
        std::fs::remove_dir_all(store.join("packages/xuepoo.partial/1.0.0/lua")).expect("truncate");

        let inspection = inspect_store(&store).expect("inspect");
        assert!(
            inspection
                .issues
                .iter()
                .any(|issue| issue.kind == StoreIssueKind::DanglingRecord),
            "{:?}",
            inspection.issues
        );
        let report = repair_store(&store).expect("repair");
        assert!(
            report
                .quarantined
                .iter()
                .any(|(from, _)| from == "packages/xuepoo.partial/1.0.0"),
            "{:?}",
            report.quarantined
        );
        assert!(!store.join("packages/xuepoo.partial/1.0.0").exists());
        assert!(resolution::load_index(&store).expect("index").is_empty());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn stale_staging_is_reported_and_removed_by_doctor() {
        let scratch = scratch("staging");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.staging", "1.0.0", None);
        install(&store, &source);
        let staging = store.join("packages/xuepoo.staging/.tmp-crashed");
        std::fs::create_dir_all(&staging).expect("staging dir");
        std::fs::write(staging.join("bitty-plugin.toml"), b"partial").expect("partial");
        backdate_staging(&staging);

        let inspection = inspect_store(&store).expect("inspect");
        assert!(
            inspection
                .issues
                .iter()
                .any(|issue| issue.kind == StoreIssueKind::StaleStaging),
            "{:?}",
            inspection.issues
        );
        let report = repair_store(&store).expect("repair");
        assert_eq!(report.staging_removed.len(), 1);
        assert!(!staging.exists());
        assert_eq!(
            resolution::load_index(&store).expect("index").len(),
            1,
            "staging GC does not disturb the active record"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn index_commit_failure_rolls_back_fresh_and_replacement() {
        let scratch = scratch("rollback");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.rollback", "1.0.0", None);
        let failing = |_: &Path, _: &[PluginRecord]| {
            Err(super::super::PluginRuntimeError::Io(
                "injected index failure".to_string(),
            ))
        };
        let error = install_local_dir_with_index_writer(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
            &failing,
        )
        .expect_err("fresh index failure rolls back");
        assert!(format!("{error}").contains("rolled back"), "{error}");
        assert!(
            !store.join("packages/xuepoo.rollback/1.0.0").exists(),
            "no orphan version dir after a failed first install"
        );
        assert!(!store.join(resolution::CURRENT_POINTER_FILE).exists());

        // Now commit a real package, then fail a same-version replacement.
        install(&store, &source);
        let index = std::fs::read(store.join(resolution::CURRENT_POINTER_FILE)).expect("index");
        std::fs::write(
            store.join("packages/xuepoo.rollback/1.0.0/lua/init.lua"),
            b"return { changed = true }\n",
        )
        .expect("mutate tree");
        let error = install_local_dir_with_index_writer(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
            &failing,
        )
        .expect_err("replacement index failure rolls back");
        assert!(format!("{error}").contains("rolled back"), "{error}");
        assert_eq!(
            std::fs::read(store.join(resolution::CURRENT_POINTER_FILE)).expect("index"),
            index,
            "index unchanged"
        );
        assert_eq!(
            std::fs::read(store.join("packages/xuepoo.rollback/1.0.0/lua/init.lua"))
                .expect("restored tree"),
            b"return { changed = true }\n",
            "the pre-install tree is restored instead of deleted"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn uninstall_cleans_an_unindexed_orphan_tree() {
        let scratch = scratch("uninstall-orphan");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.orphanremove", "1.0.0", None);
        install(&store, &source);
        std::fs::remove_file(store.join(resolution::CURRENT_POINTER_FILE)).expect("drop index");

        let report = uninstall(&store, "xuepoo.orphanremove").expect("remove orphan");
        assert!(!report.record_removed);
        assert!(report.tree_removed);
        assert!(!store.join("packages/xuepoo.orphanremove").exists());
        assert!(
            uninstall(&store, "xuepoo.orphanremove").is_err(),
            "second run is not silent"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn repair_reports_unwritable_quarantine_instead_of_half_deleting() {
        let scratch = scratch("readonly-repair");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.readonly", "1.0.0", None);
        install(&store, &source);
        std::fs::remove_file(store.join("packages/xuepoo.readonly/1.0.0/lua/init.lua"))
            .expect("truncate");
        std::fs::remove_file(store.join(resolution::CURRENT_POINTER_FILE)).expect("drop index");
        // A file where the quarantine directory must go makes create_dir_all
        // fail deterministically, even for a privileged user.
        std::fs::write(store.join(QUARANTINE_DIR), b"not a directory").expect("block quarantine");

        let error = repair_store(&store).expect_err("repair must fail closed");
        assert!(
            format!("{error}").contains("writable"),
            "actionable permission message, got {error}"
        );
        assert!(
            store.join("packages/xuepoo.readonly/1.0.0").is_dir(),
            "failed repair must not delete the package tree"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
