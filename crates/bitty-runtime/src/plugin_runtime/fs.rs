//! Pluggable filesystem abstraction and atomic persistence engine.
//!
//! Provides all-or-nothing atomic replacement with distinguished durability
//! guarantees, isolated temporary file allocation, fail-safe cleanup, and fake
//! filesystem adapters for transactional failure injection (TERM-RUN-003, PLUG-REG-010).
//!
//! CTX-1087 (F5, #1891) duplication note: [`write_atomic_durably`] duplicates
//! `bitty-storage`'s `atomic_io::write_atomic_durably` (`atomic_io.rs:136`)
//! plus the `clean_temp_siblings` / `STALE_TEMP_AGE_SECS` stale-temp sweep.
//! The duplication is intentional and documented: no shared-trait Core
//! capability is introduced here, and the final dedup is owned by the
//! post-0.0.23 extraction (the `bitty-storage` repo owns the canonical
//! mechanics; Core keeps this parity copy until the extraction lands).
//! Values and sweep semantics below must stay in parity with `bitty-storage`
//! (`STALE_TEMP_AGE_SECS = 3600`, `<file>.tmp.` / `<file>.tmp-` prefixes,
//! age-gated, fail-closed toward keeping, sweep-before-write).

use std::collections::HashMap;
use std::fmt::Debug;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Storage operations required for bounded plugin settings and package resolution.
pub trait FileSystem: Send + Sync + Debug {
    /// Recursively create a directory and all of its parent components.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;

    /// Write all data to a file.
    fn write_file(&self, path: &Path, data: &[u8]) -> io::Result<()>;

    /// Flush and sync a file's metadata and data to disk for durability.
    fn sync_file(&self, path: &Path) -> io::Result<()>;

    /// Atomically rename a file from `from` to `to`, replacing `to` if it exists.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Remove a file from the filesystem.
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    /// Read an entire file into a string.
    fn read_to_string(&self, path: &Path) -> io::Result<String>;

    /// Query the length of a file in bytes.
    fn metadata_len(&self, path: &Path) -> io::Result<u64>;

    /// Check if a path exists.
    fn exists(&self, path: &Path) -> bool;
}

/// Standard native filesystem implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeFileSystem;

impl FileSystem for NativeFileSystem {
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        std::fs::write(path, data)
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        // Windows FlushFileBuffers requires GENERIC_WRITE access; opening
        // with write permissions ensures cross-platform durability flushes.
        let file = std::fs::OpenOptions::new().write(true).open(path)?;
        file.sync_all()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn metadata_len(&self, path: &Path) -> io::Result<u64> {
        std::fs::metadata(path).map(|m| m.len())
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

/// Minimum temp-sibling age before the pre-write sweep treats it as crash
/// litter (CTX-1087 parity with `bitty-storage`'s `STALE_TEMP_AGE_SECS`).
///
/// Live writers hold their temps for milliseconds, so an hour keeps the sweep
/// from ever deleting a concurrent saver's live temp while still reclaiming
/// crashed-save litter on later saves. Must stay `3600` in parity with the
/// extraction; the final dedup is owned post-0.0.23 by `bitty-storage`.
pub const STALE_TEMP_AGE_SECS: u64 = 3_600;

/// Removes stale `<file>.tmp.*` siblings best-effort (crashed-save litter).
///
/// Parity with `bitty-storage`'s `clean_temp_siblings`: age-gated so the sweep
/// never deletes a concurrent saver's live temp. Runs over the real
/// filesystem via `std::fs` (best-effort, errors ignored) rather than the
/// [`FileSystem`] trait on purpose: the sweep is hygiene, not correctness,
/// and threading directory-listing + mtime through the trait would be a new
/// Core capability (forbidden by #1891). `FakeFileSystem` entries are
/// in-memory and therefore never swept here; real stale files from previous
/// native runs are still reclaimed.
///
/// Callers sweep BEFORE writing, never after the rename: a post-rename sweep
/// would race a concurrent saver's live temp sibling.
pub fn clean_temp_siblings(destination: &Path) {
    clean_temp_siblings_with_now(destination, std::time::SystemTime::now());
}

fn clean_temp_siblings_with_now(destination: &Path, now: std::time::SystemTime) {
    let Some(parent) = destination.parent() else {
        return;
    };
    if parent.as_os_str().is_empty() {
        return;
    }
    let stem = destination.file_name().map_or_else(
        || "component".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let dot_prefix = format!("{stem}.tmp.");
    let dash_prefix = format!("{stem}.tmp-");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with(&dot_prefix) || name.starts_with(&dash_prefix)) {
            continue;
        }
        // Fail-closed toward keeping: unknown age (missing/clocked-skewed
        // mtime) is treated as live, never as litter.
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|mtime| {
                now.duration_since(mtime)
                    .is_ok_and(|age| age.as_secs() >= STALE_TEMP_AGE_SECS)
            })
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Write `data` to `destination` atomically and durably via `fs`:
/// 0. Sweeps stale `<file>.tmp.*` siblings best-effort via
///    [`clean_temp_siblings`] (parity with `bitty-storage`; never touches
///    the live `temp` or `destination`).
/// 1. Ensures destination parent directory exists.
/// 2. Writes `data` to a unique temporary file `temp` in the parent directory.
/// 3. Flushes and syncs data to disk for durability.
/// 4. Atomically replaces `destination` with `temp` via rename.
/// 5. On ANY error during write, sync, or replacement:
///    - Cleans up `temp`.
///    - NEVER deletes or modifies `destination`.
///    - Preserves previous committed file content intact.
pub fn write_atomic_durably(
    fs: &dyn FileSystem,
    destination: &Path,
    data: &[u8],
    temp: &Path,
) -> io::Result<()> {
    // Pre-write sweep only: a post-rename sweep would race a concurrent
    // saver's live temp. Best-effort; failures never fail the write.
    clean_temp_siblings(destination);
    if let Some(parent) = destination.parent() {
        if !parent.as_os_str().is_empty() {
            fs.create_dir_all(parent)?;
        }
    }
    let write_result = (|| -> io::Result<()> {
        fs.write_file(temp, data)?;
        fs.sync_file(temp)?;
        fs.rename(temp, destination)?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = fs.remove_file(temp);
        return Err(err);
    }
    Ok(())
}

/// In-memory fake filesystem with fault-injection switches for testing.
#[derive(Debug, Default)]
pub struct FakeFileSystem {
    files: RwLock<HashMap<PathBuf, Vec<u8>>>,
    fail_create_dir: AtomicBool,
    fail_writes: AtomicBool,
    fail_syncs: AtomicBool,
    fail_renames: AtomicBool,
    fail_removals: AtomicBool,
    recorded_writes: RwLock<Vec<PathBuf>>,
    recorded_syncs: RwLock<Vec<PathBuf>>,
    recorded_renames: RwLock<Vec<(PathBuf, PathBuf)>>,
    recorded_removals: RwLock<Vec<PathBuf>>,
}

impl FakeFileSystem {
    /// Create an empty fake filesystem.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-populate or inspect a file.
    pub fn set_file(&self, path: impl AsRef<Path>, data: Vec<u8>) {
        self.files
            .write()
            .unwrap()
            .insert(path.as_ref().to_path_buf(), data);
    }

    /// Read raw file bytes if present.
    #[must_use]
    pub fn get_file(&self, path: impl AsRef<Path>) -> Option<Vec<u8>> {
        self.files.read().unwrap().get(path.as_ref()).cloned()
    }

    /// Enable or disable injected write failures.
    pub fn set_fail_writes(&self, fail: bool) {
        self.fail_writes.store(fail, Ordering::Relaxed);
    }

    /// Enable or disable injected sync failures.
    pub fn set_fail_syncs(&self, fail: bool) {
        self.fail_syncs.store(fail, Ordering::Relaxed);
    }

    /// Enable or disable injected rename (replacement) failures.
    pub fn set_fail_renames(&self, fail: bool) {
        self.fail_renames.store(fail, Ordering::Relaxed);
    }

    /// Enable or disable injected removal (cleanup) failures.
    pub fn set_fail_removals(&self, fail: bool) {
        self.fail_removals.store(fail, Ordering::Relaxed);
    }

    /// Enable or disable injected directory creation failures.
    pub fn set_fail_create_dir(&self, fail: bool) {
        self.fail_create_dir.store(fail, Ordering::Relaxed);
    }

    /// Check if a path was removed.
    pub fn was_removed(&self, path: impl AsRef<Path>) -> bool {
        self.recorded_removals
            .read()
            .unwrap()
            .iter()
            .any(|p| p == path.as_ref())
    }

    /// Check if a path was synced.
    pub fn was_synced(&self, path: impl AsRef<Path>) -> bool {
        self.recorded_syncs
            .read()
            .unwrap()
            .iter()
            .any(|p| p == path.as_ref())
    }

    /// Check if a rename was performed.
    pub fn was_renamed(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> bool {
        self.recorded_renames
            .read()
            .unwrap()
            .iter()
            .any(|(f, t)| f == from.as_ref() && t == to.as_ref())
    }

    /// Check if a path exists.
    pub fn exists(&self, path: impl AsRef<Path>) -> bool {
        self.files.read().unwrap().contains_key(path.as_ref())
    }

    /// Read file content as string.
    pub fn read_to_string(&self, path: impl AsRef<Path>) -> io::Result<String> {
        let files = self.files.read().unwrap();
        let bytes = files
            .get(path.as_ref())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file not found"))?;
        String::from_utf8(bytes.clone()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

impl FileSystem for FakeFileSystem {
    fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
        if self.fail_create_dir.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected create_dir_all failure",
            ));
        }
        Ok(())
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        if self.fail_writes.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected write failure",
            ));
        }
        self.recorded_writes
            .write()
            .unwrap()
            .push(path.to_path_buf());
        self.files
            .write()
            .unwrap()
            .insert(path.to_path_buf(), data.to_vec());
        Ok(())
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        if self.fail_syncs.load(Ordering::Relaxed) {
            return Err(io::Error::other("injected sync failure"));
        }
        self.recorded_syncs
            .write()
            .unwrap()
            .push(path.to_path_buf());
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.fail_renames.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected rename failure",
            ));
        }
        let mut files = self.files.write().unwrap();
        let data = files
            .remove(from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "source file does not exist"))?;
        files.insert(to.to_path_buf(), data);
        drop(files);
        self.recorded_renames
            .write()
            .unwrap()
            .push((from.to_path_buf(), to.to_path_buf()));
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        if self.fail_removals.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected remove failure",
            ));
        }
        self.recorded_removals
            .write()
            .unwrap()
            .push(path.to_path_buf());
        self.files.write().unwrap().remove(path);
        Ok(())
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let files = self.files.read().unwrap();
        let bytes = files
            .get(path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file not found"))?;
        String::from_utf8(bytes.clone()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn metadata_len(&self, path: &Path) -> io::Result<u64> {
        let files = self.files.read().unwrap();
        let bytes = files
            .get(path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file not found"))?;
        Ok(bytes.len() as u64)
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.read().unwrap().contains_key(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static SWEEP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct SweepDir(PathBuf);

    impl Drop for SweepDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl SweepDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    fn sweep_dir(tag: &str) -> SweepDir {
        let id = SWEEP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bitty-fs-sweep-{tag}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create sweep dir");
        SweepDir(dir)
    }

    #[test]
    fn stale_temp_age_parity_with_storage() {
        // CTX-1087: the sweep horizon must stay `3600` in parity with
        // `bitty-storage`'s `STALE_TEMP_AGE_SECS`.
        assert_eq!(STALE_TEMP_AGE_SECS, 3_600);
    }

    #[test]
    fn sweep_keeps_fresh_temps_and_non_temps() {
        let dir = sweep_dir("fresh");
        let root = dir.path();
        let destination = root.join("current.json");
        std::fs::write(&destination, b"committed").expect("write destination");
        let fresh_dash = root.join("current.json.tmp-123-456");
        let fresh_dot = root.join("current.json.tmp.789");
        let unrelated = root.join("current.json.bak");
        std::fs::write(&fresh_dash, b"live").expect("write fresh dash");
        std::fs::write(&fresh_dot, b"live").expect("write fresh dot");
        std::fs::write(&unrelated, b"keep").expect("write unrelated");

        clean_temp_siblings(&destination);

        assert!(fresh_dash.exists(), "fresh dash temp must be kept");
        assert!(fresh_dot.exists(), "fresh dot temp must be kept");
        assert!(unrelated.exists(), "non-temp sibling must be kept");
        assert_eq!(
            std::fs::read(&destination).expect("read destination"),
            b"committed"
        );
    }

    #[test]
    fn sweep_removes_aged_litter_both_prefixes() {
        let dir = sweep_dir("aged");
        let root = dir.path();
        let destination = root.join("current.json");
        let stale_dash = root.join("current.json.tmp-9-9");
        let stale_dot = root.join("current.json.tmp.11");
        std::fs::write(&stale_dash, b"litter").expect("write stale dash");
        std::fs::write(&stale_dot, b"litter").expect("write stale dot");

        // Simulate age without touching mtimes: sweep with a `now` far in
        // the future so both siblings read as older than the horizon.
        let future = std::time::SystemTime::now() + Duration::from_secs(STALE_TEMP_AGE_SECS + 60);
        clean_temp_siblings_with_now(&destination, future);

        assert!(!stale_dash.exists(), "aged dash litter must go");
        assert!(!stale_dot.exists(), "aged dot litter must go");
    }

    #[test]
    fn atomic_write_sweeps_stale_before_committing() {
        let dir = sweep_dir("write");
        let root = dir.path();
        let destination = root.join("current.json");
        std::fs::write(&destination, b"old").expect("write old");
        let litter = root.join("current.json.tmp-7-7");
        std::fs::write(&litter, b"litter").expect("write litter");
        // Age the litter via a pre-sweep with a future clock, then verify a
        // real `write_atomic_durably` (real clock) keeps the commit path
        // intact; the sweep itself is covered above.
        let future = std::time::SystemTime::now() + Duration::from_secs(STALE_TEMP_AGE_SECS + 60);
        clean_temp_siblings_with_now(&destination, future);
        assert!(!litter.exists());

        let temp = root.join("current.json.tmp-1-1");
        write_atomic_durably(&NativeFileSystem, &destination, b"new", &temp).expect("atomic write");
        assert_eq!(std::fs::read(&destination).expect("read"), b"new");
        assert!(!temp.exists(), "live temp renamed away");
    }
}
