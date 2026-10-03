//! Shared W-146 stub backends for `bitty-runtime` integration tests.
//!
//! Seam-discipline only, never byte parity: the memory stub proves call
//! order, fail-closed behavior, and state intactness through the Core-owned
//! traits. Byte parity with the pre-rewire mechanics is proven in the
//! wiring crate (`bitty-terminal`) against the real `bitty-storage`
//! backend and the committed golden fixtures.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitty_runtime::plugin_runtime::{KvCommitBackend, KvCommitError, STORE_FILE_MAX_BYTES};

#[derive(Debug, Default)]
struct MemoryState {
    files: HashMap<PathBuf, Vec<u8>>,
    fail_commits: bool,
    fail_loads: bool,
    slow_commit: Option<Duration>,
    commits: u64,
    loads: u64,
}

/// In-memory [`KvCommitBackend`] stub with failure and latency injection.
///
/// Enforces the Core file ceiling on both directions, mirroring the
/// production requirement; every other behavior (ordering, intactness,
/// fail-closed) is what the tests pin.
///
/// Shared across integration test binaries; each binary uses a subset, so
/// unused methods are allowed here rather than per call site.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct MemoryKvBackend {
    state: Arc<Mutex<MemoryState>>,
}

#[allow(dead_code)]
impl MemoryKvBackend {
    /// Builds an empty stub backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds one stored image (stands in for a file written earlier).
    pub fn seed(&self, path: &Path, bytes: Vec<u8>) {
        self.state
            .lock()
            .expect("stub lock")
            .files
            .insert(path.to_path_buf(), bytes);
    }

    /// Reads back one stored image, if any.
    #[must_use]
    pub fn stored(&self, path: &Path) -> Option<Vec<u8>> {
        self.state
            .lock()
            .expect("stub lock")
            .files
            .get(path)
            .cloned()
    }

    /// Makes every commit fail (durability fault injection).
    pub fn set_fail_commits(&self, fail: bool) {
        self.state.lock().expect("stub lock").fail_commits = fail;
    }

    /// Makes every load fail (read fault injection).
    pub fn set_fail_loads(&self, fail: bool) {
        self.state.lock().expect("stub lock").fail_loads = fail;
    }

    /// Slows every commit (slow-disk regression stand-in).
    pub fn set_slow_commit(&self, delay: Option<Duration>) {
        self.state.lock().expect("stub lock").slow_commit = delay;
    }

    /// Commits attempted so far.
    #[must_use]
    pub fn commits(&self) -> u64 {
        self.state.lock().expect("stub lock").commits
    }

    /// Loads attempted so far.
    #[must_use]
    pub fn loads(&self) -> u64 {
        self.state.lock().expect("stub lock").loads
    }
}

impl KvCommitBackend for MemoryKvBackend {
    fn commit_store_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), KvCommitError> {
        let mut state = self.state.lock().expect("stub lock");
        if state.fail_commits {
            return Err(KvCommitError::new("stub commit refused"));
        }
        if bytes.len() > STORE_FILE_MAX_BYTES {
            return Err(KvCommitError::new("stub payload exceeds ceiling"));
        }
        if let Some(delay) = state.slow_commit {
            std::thread::sleep(delay);
        }
        state.commits += 1;
        state.files.insert(path.to_path_buf(), bytes.to_vec());
        Ok(())
    }

    fn load_store_bytes(&self, path: &Path) -> Result<Option<Vec<u8>>, KvCommitError> {
        let mut state = self.state.lock().expect("stub lock");
        if state.fail_loads {
            return Err(KvCommitError::new("stub load refused"));
        }
        state.loads += 1;
        let image = state.files.get(path).cloned();
        if let Some(bytes) = &image {
            if bytes.len() > STORE_FILE_MAX_BYTES {
                return Err(KvCommitError::new("stub image exceeds ceiling"));
            }
        }
        Ok(image)
    }
}

/// Installs a fresh stub backend on `rt` and returns the handle so tests
/// can seed storage and inject faults.
#[allow(dead_code)]
pub fn install_stub_backend(
    rt: &mut bitty_runtime::plugin_runtime::PluginRuntime,
) -> MemoryKvBackend {
    let backend = MemoryKvBackend::new();
    rt.set_store_backend(Some(Arc::new(backend.clone())));
    backend
}
