//! Process-global locks for `bitty-terminal` unit tests.
//!
//! Several registries used by the IPC/devtools tests live in process-global
//! state. Tests in this crate's unit-test binary run in parallel, so a test
//! that clears a registry can wipe an entry another test is asserting on.
//! These guards make that access mutually exclusive without serializing the
//! whole suite.
use std::sync::{Mutex, MutexGuard, OnceLock};

fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Serializes tests that clear, issue into, or count the process-global
/// automation bearer registry ([`bitty_ipc::devtools`]).
pub(crate) fn automation_guard() -> MutexGuard<'static, ()> {
    lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
