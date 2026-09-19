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
//!   -> atomic rename to packages/<id>/<version>
//!   -> atomic current.json pointer write
//! ```
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
//! CLI rejects them explicitly instead of pretending to resolve them.

use std::collections::BTreeSet;
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
/// Prefix of an in-flight quarantine copy (never a valid plugin id or version).
const STAGING_PREFIX: &str = ".tmp-";
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
    /// Desired load state for the installed record.
    pub enable: bool,
}

impl Default for LocalInstallOptions {
    fn default() -> Self {
        Self { enable: true }
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
    /// Version that was active at removal time.
    pub version: String,
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
/// `consent` is asked to approve **that snapshot** (its canonical
/// `manifest_hash` and `content_digest`), and the same snapshot is committed.
/// The source is never re-read after consent, so a manifest swapped while the
/// prompt is open can never widen the installed grant; the staged body is
/// re-verified against the reviewed hash before commit as defense in depth.
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

    // Consent is bound to the staged snapshot above; the source is never read
    // again. A caller cannot approve a different manifest than the one shown.
    if !added.is_empty() {
        let request = ConsentRequest {
            plugin_id: plugin_id.clone(),
            version: version.clone(),
            manifest_hash: manifest_hash.clone(),
            content_digest: content_digest.clone(),
            added: added.clone(),
            granted: granted.iter().cloned().collect(),
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

    let target = package_dir.join(&version);
    let mut created_target = false;
    if target.exists() {
        let existing_digest =
            resolution::scan_module_tree(&plugin_id, &resolution::module_root_for(&target))
                .map_err(|error| {
                    let _ = std::fs::remove_dir_all(&staging);
                    PackageOpError::ModuleTree {
                        plugin: plugin_id.clone(),
                        detail: format!("existing package root is unreadable: {error}"),
                    }
                })?;
        if existing_digest != content_digest {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(PackageOpError::Store(format!(
                "'{plugin_id}' version {version} is already installed with different content"
            )));
        }
        // PLUG-REG-007: also check manifest identity. A same-version reinstall
        // with identical Lua but a changed manifest would otherwise write a new
        // manifest_hash to the index while leaving the on-disk manifest
        // unchanged, causing resolution to fail closed on hash mismatch.
        if let Some(ref existing_record) = existing {
            if existing_record.manifest_hash != manifest_hash {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(PackageOpError::Store(format!(
                    "'{plugin_id}' version {version} is already installed with a different \
                     manifest identity; remove the existing install or bump the version"
                )));
            }
        }
        std::fs::remove_dir_all(&staging).map_err(|error| {
            PackageOpError::Store(format!("cannot drop '{}': {error}", staging.display()))
        })?;
    } else {
        std::fs::rename(&staging, &target).map_err(|error| {
            let _ = std::fs::remove_dir_all(&staging);
            PackageOpError::Store(format!("cannot commit '{}': {error}", target.display()))
        })?;
        created_target = true;
    }
    harden_tree(&target)?;

    let root = format!("{PACKAGES_DIR}/{plugin_id}/{version}");
    let mut updated_records = current_records;
    updated_records.retain(|record| record.plugin_id != plugin_id);
    updated_records.push(PluginRecord {
        source_class: SourceClass::LocalPath,
        plugin_id: plugin_id.clone(),
        version: version.clone(),
        root: root.clone(),
        manifest_hash: manifest_hash.clone(),
        content_digest: content_digest.clone(),
        enabled: options.enable,
        granted: granted.iter().cloned().collect(),
    });
    if let Err(error) = resolution::write_index(store_root, &updated_records) {
        if created_target {
            let _ = std::fs::remove_dir_all(&target);
        }
        return Err(PackageOpError::Store(error.to_string()));
    }
    harden_index(store_root)?;

    let previous_version = existing.as_ref().map(|record| record.version.clone());
    // PLUG-REG-008: Keep predecessor/history metadata independently so that an
    // unchanged reinstall does not advance retention and prune the genuinely
    // retained predecessor.
    let retained_predecessor = if let Some(ref existing_record) = existing {
        if existing_record.version != version {
            // Version updated: record the superseded version as the new predecessor.
            save_predecessor(&package_dir, &existing_record.version)?;
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
        Ok(()) => gc_warning,
        Err(error) => {
            let prune = PruneWarning::new(error.to_string());
            Some(match gc_warning {
                Some(gc) => PruneWarning::new(format!("{}; {}", gc.detail, prune.detail)),
                None => prune,
            })
        }
    };

    Ok(Some(InstallReport {
        plugin_id,
        version,
        source_class: SourceClass::LocalPath,
        manifest_hash,
        content_digest,
        root,
        granted: granted.into_iter().collect(),
        added,
        updated: previous_version.is_some(),
        previous_version,
        prune_warning,
    }))
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
/// inert because no record points at it.
///
/// # Errors
///
/// [`PackageOpError::Store`] when the record is unknown or the index cannot be
/// rewritten.
pub fn uninstall(store_root: &Path, plugin_id: &str) -> Result<UninstallReport, PackageOpError> {
    let _lock = STORE_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut records = resolution::load_index(store_root)?;
    let index = records
        .iter()
        .position(|record| record.plugin_id == plugin_id)
        .ok_or_else(|| {
            PackageOpError::Store(format!(
                "'{plugin_id}' is not installed in the plugin store"
            ))
        })?;
    let removed = records.remove(index);
    resolution::write_index(store_root, &records)?;
    harden_index(store_root)?;
    let package_dir = store_root.join(PACKAGES_DIR).join(plugin_id);
    let tree_removed = if package_dir.exists() {
        std::fs::remove_dir_all(&package_dir).map_err(|error| {
            PackageOpError::Store(format!(
                "record removed but '{}' could not be deleted: {error}",
                package_dir.display()
            ))
        })?;
        true
    } else {
        false
    };
    Ok(UninstallReport {
        plugin_id: plugin_id.to_string(),
        version: removed.version,
        tree_removed,
    })
}

/// Verify `compat.bitty` and `compat.plugin-api` include this host.
fn check_compat(manifest: &PluginManifest) -> Result<(), PackageOpError> {
    check_compat_with_hosts(manifest, host_bitty_version(), bitty_lua::host::API_VERSION)
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
    .map_err(|failure| {
        let plugin = manifest.identity.id.as_str().to_string();
        match failure {
            CompatFailure::Invalid { field: _, detail } => {
                PackageOpError::Manifest { plugin, detail }
            }
            CompatFailure::Incompatible {
                field,
                requested,
                host,
            } => PackageOpError::Incompatible {
                plugin,
                field,
                requested,
                host,
            },
        }
    })
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
    fn same_version_manifest_only_change_rejected_before_index_mutation() {
        // PLUG-REG-007: a reinstall that changes the manifest description (so
        // the manifest_hash changes) but preserves the Lua body (so
        // content_digest is identical) must be rejected before any index write.
        let scratch = scratch("plug-reg-007-reject");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.plug007rej", "1.0.0", None);
        let first = install(&store, &source);
        let original_hash = first.manifest_hash.clone();

        // Build an alt version: same Lua body, different compat range -> different manifest_hash.
        let source_alt = write_plugin_alt_compat(&scratch, "xuepoo.plug007rej", "1.0.0");
        let err = install_local_dir(
            &store,
            &source_alt,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect_err("should reject manifest-only change on same version");
        let PackageOpError::Store(msg) = &err else {
            panic!("expected Store error, got {err:?}");
        };
        assert!(
            msg.contains("different manifest identity"),
            "error should mention manifest identity mismatch, got: {msg}"
        );
        // Index must remain unchanged after the rejection.
        let records = resolution::load_index(&store).expect("index");
        assert_eq!(
            records[0].manifest_hash, original_hash,
            "index must not be mutated on rejection"
        );
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
        let source = write_plugin(&scratch, "bitty-terminal.tabs", "1.0.0", None);
        let error = install_local_dir(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect_err("bundled ids must not be shadowed");
        assert!(matches!(error, PackageOpError::BundledIdReserved { .. }));
        assert!(!store.join("packages").exists());
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
    fn same_version_different_content_is_rejected() {
        let scratch = scratch("collision");
        let store = scratch.join("store");
        let source = write_plugin(&scratch, "xuepoo.collide", "1.0.0", None);
        install(&store, &source);
        std::fs::write(source.join("lua/extra.lua"), "return {}\n").expect("extra");
        let error = install_local_dir(
            &store,
            &source,
            &LocalInstallOptions::default(),
            &mut |_| Ok(true),
        )
        .expect_err("same version with different content must fail");
        assert!(matches!(error, PackageOpError::Store(_)));
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
}
