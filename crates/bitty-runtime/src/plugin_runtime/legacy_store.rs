//! One-time adoption of a pre-`%LOCALAPPDATA%` Windows plugin store
//! (CTX-1876, residual of bitty#3226).
//!
//! Before the platform-root change the store resolved
//! `$HOME/.local/share/bitty/plugins` whenever `$XDG_DATA_HOME` was unset. It
//! now resolves `%LOCALAPPDATA%\bitty\plugins` first, so an upgraded install
//! that kept its packages in the old root would silently discover none of
//! them: `load_index` on the new root finds nothing, so already-installed
//! plugins go invisible instead of erroring. Adoption runs once at root
//! resolution and moves that tree into the new root.
//!
//! The rules are deliberately narrow, because plugin discovery stays a
//! single-authority path:
//!
//! - An existing new root always wins. Nothing is merged, so discovery never
//!   reads two index files and a store that already lives under
//!   `%LOCALAPPDATA%` is never touched.
//! - The move is a rename of the whole tree. When the two roots sit on
//!   different volumes (a `%LOCALAPPDATA%` on `C:` with a `HOME` on a network
//!   share or another drive) the rename fails, and a bounded copy into a
//!   hidden sibling staging directory is renamed into place instead; the
//!   legacy tree is then left untouched rather than deleted.
//! - A failed adoption reports [`LegacyStoreAdoption::Failed`], and the
//!   caller keeps the legacy root as the store root: the population this
//!   exists for must never end up with invisible plugins.
//!
//! Packages are content-addressed and every record root is relative to the
//! store root, so a store that moves wholesale re-verifies exactly as it did
//! before the move.

use std::path::{Path, PathBuf};

/// Maximum directory depth a fallback copy walks below the legacy root.
///
/// A store tree is `<root>/packages/<plugin>/<version>/…`, so four levels
/// cover every layout this tree has ever written; the bound only keeps a
/// pathological tree from turning one start-up into an unbounded walk.
const LEGACY_COPY_MAX_DEPTH: u32 = 8;

/// Maximum entries a fallback copy visits.
///
/// One plugin package is a few dozen files; the bound exists so a corrupt or
/// hostile tree cannot make start-up copy an unbounded number of them.
const LEGACY_COPY_MAX_ENTRIES: u32 = 8192;

/// Suffix of the staging directory the copy fallback fills before the swap.
const LEGACY_STAGING_SUFFIX: &str = ".adopting";

/// How one adoption attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyStoreAdoption {
    /// The legacy root is absent, empty, or not a directory: no population.
    NoLegacyRoot,
    /// The new root already exists and stays authoritative (no merge).
    NewRootPresent,
    /// The legacy tree was renamed into the new root.
    Moved,
    /// The rename failed; the tree was copied and the legacy root left in
    /// place.
    Copied,
    /// Neither the rename nor the copy worked; nothing was written.
    Failed,
}

impl LegacyStoreAdoption {
    /// Store root to use after this attempt.
    ///
    /// Every outcome but [`Self::Failed`] leaves the new root authoritative;
    /// a failed adoption falls back to the legacy root so the plugins keep
    /// being discovered while the next start retries.
    #[must_use]
    pub fn store_root<'a>(self, new_root: &'a Path, legacy_root: &'a Path) -> &'a Path {
        match self {
            Self::Failed => legacy_root,
            Self::NoLegacyRoot | Self::NewRootPresent | Self::Moved | Self::Copied => new_root,
        }
    }
}

/// Adopt `legacy_root` into `new_root` by moving the tree once.
///
/// Idempotent: after a successful adoption the new root exists, so a second
/// call reports [`LegacyStoreAdoption::NewRootPresent`] and writes nothing.
#[must_use]
pub fn adopt_legacy_store(new_root: &Path, legacy_root: &Path) -> LegacyStoreAdoption {
    adopt_legacy_store_with(new_root, legacy_root, &|from, to| std::fs::rename(from, to))
}

/// [`adopt_legacy_store`] with an injectable rename (tests force the
/// cross-volume fallback without needing a second filesystem).
fn adopt_legacy_store_with(
    new_root: &Path,
    legacy_root: &Path,
    rename: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> LegacyStoreAdoption {
    if !is_non_empty_dir(legacy_root) {
        return LegacyStoreAdoption::NoLegacyRoot;
    }
    if std::fs::symlink_metadata(new_root).is_ok() {
        return LegacyStoreAdoption::NewRootPresent;
    }
    // The new root's parent (`<data-home>/bitty`) may not exist yet; creating
    // it is what makes the rename below possible in the first place. The new
    // root itself is never created: the rename or the staging rename must be
    // the operation that installs it.
    let Some(parent) = new_root.parent() else {
        return LegacyStoreAdoption::Failed;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return LegacyStoreAdoption::Failed;
    }
    match rename(legacy_root, new_root) {
        Ok(()) => LegacyStoreAdoption::Moved,
        Err(_) => copy_adoption(new_root, legacy_root),
    }
}

/// Fallback for a rename the filesystem refused (cross-volume roots).
///
/// The copy lands in a staging sibling and is renamed into place, so a partial
/// copy is never readable as a store: either the new root appears complete or
/// it does not appear at all.
fn copy_adoption(new_root: &Path, legacy_root: &Path) -> LegacyStoreAdoption {
    let staging = staging_root(new_root);
    if std::fs::symlink_metadata(&staging).is_ok() {
        // Leftover from a killed run; it is this module's own naming scheme.
        let _ = std::fs::remove_dir_all(&staging);
    }
    if copy_tree(legacy_root, &staging).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
        return LegacyStoreAdoption::Failed;
    }
    if std::fs::rename(&staging, new_root).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
        return LegacyStoreAdoption::Failed;
    }
    // The legacy tree stays: deleting it is not needed for discovery (the new
    // root now exists and wins) and a half-finished cleanup would be worse
    // than the duplicate.
    LegacyStoreAdoption::Copied
}

/// Sibling staging directory for the copy fallback.
fn staging_root(new_root: &Path) -> PathBuf {
    let name = new_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plugins".to_owned());
    new_root.with_file_name(format!(
        "{name}{LEGACY_STAGING_SUFFIX}-{}",
        std::process::id()
    ))
}

/// Whether `path` is a real directory holding at least one entry.
fn is_non_empty_dir(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return false;
    }
    match std::fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}

/// Bounded recursive copy that never follows a link.
///
/// A store holds regular files and directories. A symlink (or any special
/// file) fails the copy instead of being followed or silently dropped: a
/// partial store that still resolves would be worse than staying on the
/// legacy root.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    let mut budget = LEGACY_COPY_MAX_ENTRIES;
    copy_dir(from, to, 0, &mut budget)
}

fn copy_dir(from: &Path, to: &Path, depth: u32, budget: &mut u32) -> std::io::Result<()> {
    if depth > LEGACY_COPY_MAX_DEPTH {
        return Err(std::io::Error::other(
            "legacy plugin store copy exceeded its depth bound",
        ));
    }
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        if *budget == 0 {
            return Err(std::io::Error::other(
                "legacy plugin store copy exceeded its entry bound",
            ));
        }
        *budget -= 1;
        let file_type = entry.file_type()?;
        let target = to.join(entry.file_name());
        if file_type.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_dir(&entry.path(), &target, depth + 1, budget)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &target)?;
        } else {
            return Err(std::io::Error::other(format!(
                "legacy plugin store copy refused a non-regular entry: {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bitty-ctx1876-adopt-{tag}-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// The smallest tree that still looks like an installed store.
    fn plant_store(root: &Path, id: &str) {
        let package = root.join("packages").join(id).join("0.1.0");
        std::fs::create_dir_all(package.join("lua")).expect("package dirs");
        std::fs::write(package.join("bitty-plugin.toml"), "[plugin]\n").expect("manifest");
        std::fs::write(package.join("lua/init.lua"), "return {}\n").expect("init");
        std::fs::write(root.join("current.json"), "{}").expect("index");
    }

    #[test]
    fn an_existing_new_root_is_authoritative_and_nothing_is_merged() {
        let dir = scratch("present");
        let new_root = dir.join("local/bitty/plugins");
        let legacy = dir.join("home/.local/share/bitty/plugins");
        plant_store(&legacy, "xuepoo.legacy");
        std::fs::create_dir_all(&new_root).expect("new root");
        std::fs::write(new_root.join("current.json"), "{}").expect("new index");

        assert_eq!(
            adopt_legacy_store(&new_root, &legacy),
            LegacyStoreAdoption::NewRootPresent
        );
        // Neither side moved: the legacy package is still where it was and the
        // new root never gained a second package tree.
        assert!(
            legacy
                .join("packages/xuepoo.legacy/0.1.0/lua/init.lua")
                .is_file()
        );
        assert!(!new_root.join("packages").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absent_or_empty_legacy_root_adopts_nothing() {
        let dir = scratch("absent");
        let new_root = dir.join("local/bitty/plugins");
        let legacy = dir.join("home/.local/share/bitty/plugins");

        assert_eq!(
            adopt_legacy_store(&new_root, &legacy),
            LegacyStoreAdoption::NoLegacyRoot
        );
        // An empty directory carries no population: adopting it would only
        // relocate an empty tree.
        std::fs::create_dir_all(&legacy).expect("empty legacy");
        assert_eq!(
            adopt_legacy_store(&new_root, &legacy),
            LegacyStoreAdoption::NoLegacyRoot
        );
        assert!(!new_root.exists(), "no empty store is created");
        assert!(!new_root.parent().expect("parent").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_legacy_store_is_moved_into_the_new_root_wholesale() {
        let dir = scratch("moved");
        let new_root = dir.join("local/bitty/plugins");
        let legacy = dir.join("home/.local/share/bitty/plugins");
        plant_store(&legacy, "xuepoo.legacy");

        let outcome = adopt_legacy_store(&new_root, &legacy);
        assert_eq!(outcome, LegacyStoreAdoption::Moved);
        assert_eq!(outcome.store_root(&new_root, &legacy), new_root);
        // The whole tree arrived, packages included, and the legacy root is
        // gone so a later run cannot adopt a stale duplicate.
        assert!(new_root.join("current.json").is_file());
        assert!(
            new_root
                .join("packages/xuepoo.legacy/0.1.0/lua/init.lua")
                .is_file()
        );
        assert!(!legacy.exists());
        // Idempotent: the second call sees the new root and writes nothing.
        assert_eq!(
            adopt_legacy_store(&new_root, &legacy),
            LegacyStoreAdoption::NoLegacyRoot
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_rename_falls_back_to_a_staged_copy_and_keeps_the_legacy_tree() {
        let dir = scratch("copied");
        let new_root = dir.join("local/bitty/plugins");
        let legacy = dir.join("home/.local/share/bitty/plugins");
        plant_store(&legacy, "xuepoo.legacy");

        // Cross-volume roots are the real trigger; a rename that always fails
        // reproduces it without a second filesystem.
        let outcome = adopt_legacy_store_with(&new_root, &legacy, &|_, _| {
            Err(std::io::Error::other("cross-device"))
        });
        assert_eq!(outcome, LegacyStoreAdoption::Copied);
        assert!(new_root.join("current.json").is_file());
        assert!(
            new_root
                .join("packages/xuepoo.legacy/0.1.0/lua/init.lua")
                .is_file()
        );
        // The legacy tree is deliberately left in place, and the staging
        // directory did not survive the swap.
        assert!(legacy.join("current.json").is_file());
        let staging = staging_root(&new_root);
        assert!(!staging.exists(), "staging dir must not linger");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_copy_reports_failure_and_writes_no_new_root() {
        let dir = scratch("failed");
        let new_root = dir.join("local/bitty/plugins");
        let legacy = dir.join("home/.local/share/bitty/plugins");
        std::fs::create_dir_all(&legacy).expect("legacy root");
        std::fs::write(legacy.join("current.json"), "{}").expect("index");
        // A nested directory the bounded copy refuses: depth beyond the cap.
        let mut deep = legacy.clone();
        for _ in 0..(LEGACY_COPY_MAX_DEPTH + 1) {
            deep = deep.join("d");
        }
        std::fs::create_dir_all(&deep).expect("deep dir");

        let outcome = adopt_legacy_store_with(&new_root, &legacy, &|_, _| {
            Err(std::io::Error::other("cross-device"))
        });
        assert_eq!(outcome, LegacyStoreAdoption::Failed);
        // A failed adoption keeps the legacy root usable instead of pointing
        // discovery at a tree that does not exist.
        assert_eq!(outcome.store_root(&new_root, &legacy), legacy);
        assert!(!new_root.exists(), "a refused copy installs nothing");
        assert!(!staging_root(&new_root).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
