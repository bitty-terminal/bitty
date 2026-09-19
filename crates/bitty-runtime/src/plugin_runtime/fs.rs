//! Pluggable filesystem abstraction and atomic persistence engine.
//!
//! Provides all-or-nothing atomic replacement with distinguished durability
//! guarantees, isolated temporary file allocation, fail-safe cleanup, and fake
//! filesystem adapters for transactional failure injection (TERM-RUN-003, PLUG-REG-010).

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
        let file = std::fs::File::open(path)?;
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

/// Write `data` to `destination` atomically and durably via `fs`:
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
