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
//! - Resolution is convergent under concurrency. Several callers resolve the
//!   store at once on a real install (startup discovery, `bitty list
//!   plugins`, `bitty plugin doctor`, a shell completion), and adoption is a
//!   probe followed by a rename that only one of them can win. A loser
//!   re-reads the roots instead of trusting its own probe, so it reads the
//!   store the winner installed rather than the legacy path the winner has
//!   already renamed away — see [`LegacyStoreAdoption::store_root`].
//! - Callers that must not mutate anything (`--safe`, `bitty plugin doctor`
//!   without `--repair`, shell completion) resolve through
//!   [`read_only_store_root`], which applies the same precedence without
//!   creating, moving, or copying a single entry.
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

/// Maximum bytes a fallback copy writes.
///
/// Entry and depth bounds cap the shape of the walk, not its weight: 8192
/// entries of arbitrary size are still copied synchronously on the start-up
/// path, before the first frame. A real store is a few MiB — one package is a
/// few dozen small files, mostly Lua and manifests — so this cap only rejects
/// a tree that is not a plugin store worth relocating at start-up. Exceeding
/// it abandons the copy (staging removed, nothing installed) and leaves the
/// legacy root serving discovery, which costs a slower path but never an
/// invisible store.
const LEGACY_COPY_MAX_BYTES: u64 = 256 * 1024 * 1024;

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
    /// The legacy root exists as a directory but could not be enumerated, so
    /// adoption could not decide whether it holds a population.
    ///
    /// Distinct from [`Self::NoLegacyRoot`] on purpose: a transient
    /// enumeration failure (an anti-virus scan, a permission blip, a network
    /// share) is not evidence that the store is empty, and treating it as
    /// absent would resolve to the empty new root and strand a tree that
    /// direct index reads can still open. Nothing is created or moved, so the
    /// legacy root keeps serving the store.
    Unreadable,
}

impl LegacyStoreAdoption {
    /// Store root to use after this attempt, re-read from the filesystem.
    ///
    /// Every outcome but [`Self::Failed`] and [`Self::Unreadable`] leaves the
    /// new root authoritative. Those two keep reading the legacy root so the
    /// plugins stay discoverable while the next start retries — but only
    /// after re-probing both roots, because a concurrent adopter may have
    /// completed the move between this attempt's probe and its failure. In
    /// that case the legacy root no longer exists and the winner's new root
    /// is the store; returning the legacy path here is precisely how an
    /// upgraded install used to end up with an empty store even though its
    /// packages had been moved successfully by another process. When neither
    /// root holds anything, the new root is returned as the documented empty
    /// state, never a legacy path that no longer exists.
    #[must_use]
    pub fn store_root<'a>(self, new_root: &'a Path, legacy_root: &'a Path) -> &'a Path {
        match self {
            Self::NoLegacyRoot | Self::NewRootPresent | Self::Moved | Self::Copied => new_root,
            Self::Failed => {
                if new_root_exists(new_root) {
                    new_root
                } else if probe_legacy_root(legacy_root) == LegacyRootProbe::Present {
                    legacy_root
                } else {
                    new_root
                }
            }
            // The probe could not prove the legacy root absent and adoption
            // returned before touching anything, so the legacy path is the
            // only root that can hold the store: keep reading it rather than
            // switch to an empty new root.
            Self::Unreadable => {
                if new_root_exists(new_root) {
                    new_root
                } else {
                    legacy_root
                }
            }
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

/// Root a caller that must not mutate the store has to read.
///
/// Same precedence as an adoption that cannot proceed, applied without a
/// single write: an existing new root is authoritative; otherwise a legacy
/// root that still holds entries is the store (the migration has not run
/// yet); otherwise the new root names the empty state.
///
/// `--safe`, `bitty plugin doctor` without `--repair`, and shell completion
/// resolve through this, so merely asking where the store is cannot create,
/// move, or copy it.
#[must_use]
pub fn read_only_store_root<'a>(new_root: &'a Path, legacy_root: &'a Path) -> &'a Path {
    if new_root_exists(new_root) {
        return new_root;
    }
    match probe_legacy_root(legacy_root) {
        LegacyRootProbe::Present | LegacyRootProbe::Unreadable => legacy_root,
        LegacyRootProbe::Absent => new_root,
    }
}

/// What probing the legacy root found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyRootProbe {
    /// A real directory holding at least one entry: a population to adopt.
    Present,
    /// Absent, a symlink, a non-directory, or an empty directory: no
    /// population, and nothing worth reporting.
    Absent,
    /// A directory that exists but whose entries could not be listed.
    ///
    /// Kept apart from [`Self::Absent`]: the store may still be readable by
    /// direct path, so neither a move nor a switch to the new root is safe.
    Unreadable,
}

/// Classify `path` as a legacy store root.
fn probe_legacy_root(path: &Path) -> LegacyRootProbe {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        // Only a definite "not there" is an absent population; any other
        // inspection failure is a probe failure, not evidence of an empty
        // store.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LegacyRootProbe::Absent;
        }
        Err(_) => return LegacyRootProbe::Unreadable,
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return LegacyRootProbe::Absent;
    }
    match std::fs::read_dir(path) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                LegacyRootProbe::Present
            } else {
                LegacyRootProbe::Absent
            }
        }
        Err(_) => LegacyRootProbe::Unreadable,
    }
}

/// Whether anything (even an empty directory) already occupies `path`.
///
/// This is the authority test the resolver has always used: an existing new
/// root wins outright, and discovery never reads two indexes.
fn new_root_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// [`adopt_legacy_store`] with an injectable rename (tests force the
/// cross-volume fallback without needing a second filesystem).
fn adopt_legacy_store_with(
    new_root: &Path,
    legacy_root: &Path,
    rename: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> LegacyStoreAdoption {
    adopt_legacy_store_with_probe(new_root, legacy_root, rename, &probe_legacy_root)
}

/// [`adopt_legacy_store_with`] with an injectable probe as well, so the
/// "probe failed" and "another caller won the race" interleavings are
/// reproducible without a flaky filesystem.
fn adopt_legacy_store_with_probe(
    new_root: &Path,
    legacy_root: &Path,
    rename: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
    probe: &dyn Fn(&Path) -> LegacyRootProbe,
) -> LegacyStoreAdoption {
    match probe(legacy_root) {
        LegacyRootProbe::Absent => return LegacyStoreAdoption::NoLegacyRoot,
        LegacyRootProbe::Unreadable => {
            // An existing new root is authoritative whatever the legacy tree
            // looks like: nothing would be adopted either way, so an
            // unreadable legacy root beside it is not a diagnostic.
            return if new_root_exists(new_root) {
                LegacyStoreAdoption::NewRootPresent
            } else {
                // Creating or moving anything here would build a second store
                // beside a tree that may hold every installed package and
                // that this attempt simply could not read.
                LegacyStoreAdoption::Unreadable
            };
        }
        LegacyRootProbe::Present => {}
    }
    if new_root_exists(new_root) {
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
    if rename(legacy_root, new_root).is_ok() {
        return LegacyStoreAdoption::Moved;
    }
    match copy_adoption(new_root, legacy_root) {
        LegacyStoreAdoption::Failed => {
            // Both paths failed, which is what the loser of a concurrent
            // adoption sees once the winner's rename has taken the source.
            // One more rename attempt costs nothing (it is the cheap
            // operation, and the copy was the expensive one) and settles the
            // race: it either takes the move for this caller or fails again
            // because the winner's root is already in place.
            if rename(legacy_root, new_root).is_ok() {
                LegacyStoreAdoption::Moved
            } else {
                converge_after_failure(new_root, legacy_root, probe)
            }
        }
        outcome => outcome,
    }
}

/// Convergent outcome for an attempt that failed both paths.
///
/// The attempt's own snapshot is stale by definition here, so read the answer
/// off the roots: a new root that exists is the winner's store (or this
/// caller's own staged copy, installed by a staging rename the filesystem
/// reported as an error); a legacy root that still holds entries is a genuine
/// failure and keeps serving the store; anything else is the documented empty
/// state, which must name the new root rather than a path that no longer
/// exists.
fn converge_after_failure(
    new_root: &Path,
    legacy_root: &Path,
    probe: &dyn Fn(&Path) -> LegacyRootProbe,
) -> LegacyStoreAdoption {
    if new_root_exists(new_root) {
        return LegacyStoreAdoption::NewRootPresent;
    }
    match probe(legacy_root) {
        LegacyRootProbe::Present => LegacyStoreAdoption::Failed,
        LegacyRootProbe::Unreadable => LegacyStoreAdoption::Unreadable,
        LegacyRootProbe::Absent => LegacyStoreAdoption::NoLegacyRoot,
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
///
/// Per process, so two `bitty` invocations adopting the same store never fill
/// each other's staging tree; the copy is a fallback, so within one process
/// the ordinary rename path (one file-system operation) is what serializes.
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

/// Bounded recursive copy that never follows a link.
///
/// A store holds regular files and directories. A symlink (or any special
/// file) fails the copy instead of being followed or silently dropped: a
/// partial store that still resolves would be worse than staying on the
/// legacy root.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    let mut budget = LegacyCopyBudget {
        entries: LEGACY_COPY_MAX_ENTRIES,
        bytes: LEGACY_COPY_MAX_BYTES,
    };
    copy_dir(from, to, 0, &mut budget)
}

/// Remaining allowances of one fallback copy.
struct LegacyCopyBudget {
    /// Entries the walk may still visit.
    entries: u32,
    /// Bytes the walk may still write.
    bytes: u64,
}

fn copy_dir(
    from: &Path,
    to: &Path,
    depth: u32,
    budget: &mut LegacyCopyBudget,
) -> std::io::Result<()> {
    if depth > LEGACY_COPY_MAX_DEPTH {
        return Err(std::io::Error::other(
            "legacy plugin store copy exceeded its depth bound",
        ));
    }
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        if budget.entries == 0 {
            return Err(std::io::Error::other(
                "legacy plugin store copy exceeded its entry bound",
            ));
        }
        budget.entries -= 1;
        let file_type = entry.file_type()?;
        let target = to.join(entry.file_name());
        if file_type.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_dir(&entry.path(), &target, depth + 1, budget)?;
        } else if file_type.is_file() {
            // `file_type` above does not follow links, so a regular file here
            // is a real one and its length is the bytes that get copied.
            let bytes = entry.metadata()?.len();
            if bytes > budget.bytes {
                return Err(std::io::Error::other(
                    "legacy plugin store copy exceeded its byte bound",
                ));
            }
            budget.bytes -= bytes;
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

    /// The package a store at `root` is expected to still hold.
    fn holds_package(root: &Path, id: &str) -> bool {
        root.join("packages")
            .join(id)
            .join("0.1.0")
            .join("lua/init.lua")
            .is_file()
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

    /// The loser of a concurrent adoption must not keep reading a legacy root
    /// that the winner has already renamed away.
    ///
    /// Both failures below are exactly what the loser observes: the rename
    /// fails because the source is gone, and the copy fails for the same
    /// reason. The interleaving is injected rather than raced, so the test is
    /// deterministic: the rename performs the move (the winner's rename
    /// landing) and then reports an error.
    #[test]
    fn losing_a_concurrent_adoption_resolves_to_the_winners_root() {
        let dir = scratch("race-lost");
        let new_root = dir.join("local/bitty/plugins");
        let legacy = dir.join("home/.local/share/bitty/plugins");
        plant_store(&legacy, "xuepoo.legacy");
        let winner_legacy = legacy.clone();
        let winner_new = new_root.clone();
        let moved = std::sync::atomic::AtomicBool::new(false);

        let outcome = adopt_legacy_store_with(&new_root, &legacy, &|from, to| {
            // A competing process wins between our probe and our rename. It
            // happens exactly once: the retry below must observe the move,
            // not repeat it.
            if !moved.swap(true, Ordering::SeqCst) {
                std::fs::rename(&winner_legacy, &winner_new).expect("the winning move");
            }
            let _ = (from, to);
            Err(std::io::Error::other("ENOENT: the source moved"))
        });
        assert_eq!(
            outcome,
            LegacyStoreAdoption::NewRootPresent,
            "a lost race is not a failure to report"
        );
        let root = outcome.store_root(&new_root, &legacy);
        assert_eq!(root, new_root, "never the renamed-away legacy path");
        assert!(root.is_dir(), "the returned root exists");
        assert!(holds_package(root, "xuepoo.legacy"), "{root:?}");
        assert!(!staging_root(&new_root).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A legacy root whose entries cannot be listed is not an empty one.
    ///
    /// The probe is injected because the failure that matters is transient
    /// (an anti-virus scan holding the directory, a permission blip, a
    /// network share) and cannot be synthesized portably on every host this
    /// suite runs on.
    #[test]
    fn an_unreadable_legacy_root_is_reported_and_left_alone() {
        let dir = scratch("unreadable");
        let new_root = dir.join("local/bitty/plugins");
        let legacy = dir.join("home/.local/share/bitty/plugins");
        plant_store(&legacy, "xuepoo.legacy");

        let outcome = adopt_legacy_store_with_probe(
            &new_root,
            &legacy,
            &|_, _| Err(std::io::Error::other("must not be called")),
            &|path| {
                if path == legacy {
                    LegacyRootProbe::Unreadable
                } else {
                    probe_legacy_root(path)
                }
            },
        );
        assert_eq!(outcome, LegacyStoreAdoption::Unreadable);
        // Nothing was created, moved, or copied, and the legacy tree still
        // serves the store: its index reads fine even though enumeration
        // failed.
        assert!(
            !new_root.exists(),
            "an unreadable legacy root creates nothing"
        );
        assert!(!new_root.parent().expect("parent").exists());
        assert!(legacy.join("current.json").is_file());
        assert!(holds_package(&legacy, "xuepoo.legacy"));
        assert_eq!(outcome.store_root(&new_root, &legacy), legacy);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real (uninjected) enumeration failure on a directory we may not read.
    #[cfg(unix)]
    #[test]
    fn a_permission_denied_legacy_root_is_unreadable_not_absent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("denied");
        let new_root = dir.join("local/bitty/plugins");
        let legacy = dir.join("home/.local/share/bitty/plugins");
        plant_store(&legacy, "xuepoo.legacy");
        let permissions = std::fs::metadata(&legacy)
            .expect("legacy metadata")
            .permissions();
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o000))
            .expect("revoke read");
        // A privileged test user can enumerate anything, and then the probe
        // under test legitimately sees a readable store.
        let enumeration_denied = std::fs::read_dir(&legacy).is_err();
        let probe = probe_legacy_root(&legacy);
        let outcome = adopt_legacy_store(&new_root, &legacy);
        std::fs::set_permissions(&legacy, permissions).expect("restore read");

        if !enumeration_denied {
            eprintln!(
                "skipping the unreadable-root assertions: this user can read a 0o000 directory"
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert_eq!(probe, LegacyRootProbe::Unreadable);
        // Enumeration failed, but the tree was never moved or copied over and
        // the caller keeps reading it: its index is still there.
        assert_eq!(outcome, LegacyStoreAdoption::Unreadable);
        assert!(!new_root.exists(), "nothing is created beside it");
        assert!(legacy.join("current.json").is_file());
        assert!(holds_package(&legacy, "xuepoo.legacy"));
        assert_eq!(outcome.store_root(&new_root, &legacy), legacy);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two adopters of the same store, every round: whoever loses reads the
    /// winner's root, and that root holds the package.
    #[test]
    fn every_concurrent_adopter_ends_up_on_a_root_that_exists_with_the_package() {
        for round in 0..64u32 {
            let dir = scratch(&format!("race-{round}"));
            let new_root = dir.join("local/bitty/plugins");
            let legacy = dir.join("home/.local/share/bitty/plugins");
            plant_store(&legacy, "xuepoo.legacy");
            let barrier = std::sync::Barrier::new(2);
            let mut roots = Vec::new();

            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for _ in 0..2 {
                    let new_root = new_root.clone();
                    let legacy = legacy.clone();
                    let barrier = &barrier;
                    handles.push(scope.spawn(move || {
                        barrier.wait();
                        let outcome = adopt_legacy_store(&new_root, &legacy);
                        (
                            outcome,
                            outcome.store_root(&new_root, &legacy).to_path_buf(),
                        )
                    }));
                }
                for handle in handles {
                    roots.push(handle.join().expect("adopter thread"));
                }
            });

            for (outcome, root) in &roots {
                assert!(
                    root.is_dir(),
                    "round {round}: {outcome:?} returned a root that does not exist: {root:?}"
                );
                assert!(
                    holds_package(root, "xuepoo.legacy"),
                    "round {round}: {outcome:?} returned a root without the package: {root:?}"
                );
            }
            // Single authority: both adopters converge on the same root, and
            // the store was moved exactly once.
            assert_eq!(roots[0].1, roots[1].1, "round {round}: roots diverged");
            assert_eq!(roots[0].1, new_root);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Adoption must not run at all through the read-only resolver.
    #[test]
    fn the_read_only_resolver_adopts_nothing() {
        let dir = scratch("read-only");
        let new_root = dir.join("local/bitty/plugins");
        let legacy = dir.join("home/.local/share/bitty/plugins");
        plant_store(&legacy, "xuepoo.legacy");

        assert_eq!(read_only_store_root(&new_root, &legacy), legacy);
        assert!(holds_package(&legacy, "xuepoo.legacy"));
        assert!(!new_root.exists(), "read-only resolution creates nothing");
        assert!(!new_root.parent().expect("parent").exists());

        // An existing new root is still authoritative, as for adoption.
        std::fs::create_dir_all(&new_root).expect("new root");
        assert_eq!(read_only_store_root(&new_root, &legacy), new_root);
        // Neither root: the new root names the empty state.
        let bare = dir.join("bare");
        assert_eq!(
            read_only_store_root(&bare.join("bitty/plugins"), &bare.join("absent")),
            bare.join("bitty/plugins")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The byte bound is a real limit, not a comment.
    #[test]
    fn a_copy_over_the_byte_bound_installs_nothing() {
        let dir = scratch("bytes");
        let new_root = dir.join("local/bitty/plugins");
        let legacy = dir.join("home/.local/share/bitty/plugins");
        std::fs::create_dir_all(&legacy).expect("legacy root");
        std::fs::write(legacy.join("current.json"), "{}").expect("index");
        // One sparse file larger than the cap: the walk must refuse it rather
        // than copying it at start-up.
        let huge = legacy.join("packages/huge/0.1.0/blob.bin");
        std::fs::create_dir_all(huge.parent().expect("parent")).expect("package dir");
        let file = std::fs::File::create(&huge).expect("blob");
        file.set_len(LEGACY_COPY_MAX_BYTES + 1)
            .expect("sparse length");
        drop(file);

        let outcome = adopt_legacy_store_with(&new_root, &legacy, &|_, _| {
            Err(std::io::Error::other("cross-device"))
        });
        assert_eq!(outcome, LegacyStoreAdoption::Failed);
        assert!(!new_root.exists(), "an over-budget copy installs nothing");
        assert!(!staging_root(&new_root).exists());
        // The legacy root keeps the store, so discovery is slower, not blind.
        assert_eq!(outcome.store_root(&new_root, &legacy), legacy);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
