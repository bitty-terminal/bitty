//! W-146 session seam discipline (CTX-0939): backend-absent fail-closed,
//! safe-mode no-touch, and present-configuration plumbing through the
//! Core-owned [`SessionFileBackend`] trait.
//!
//! Byte parity rides the wiring crate (`bitty-terminal`) with the real
//! `bitty-storage` backend and the golden fixtures; the stub here is
//! deliberately format-agnostic and proves gate order, intactness, and
//! fail-closed behavior only.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bitty_runtime::{
    LayoutNode, PaneAttachment, PaneRoute, PresentationMode, Runtime, SessionError,
    SessionFileBackend, SessionSnapshot, View, ViewId, WorkspaceSnapshot,
};

/// Format-agnostic memory stub: records call order, stores committed bytes,
/// and replays a canned valid snapshot on decode.
#[derive(Debug)]
struct StubBackend {
    calls: Mutex<Vec<&'static str>>,
    files: Mutex<HashMap<PathBuf, Vec<u8>>>,
    fail_commit: AtomicBool,
    fail_decode: AtomicBool,
    canned: SessionSnapshot,
}

impl StubBackend {
    fn canned_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: bitty_runtime::SESSION_FORMAT_VERSION,
            workspaces: vec![WorkspaceSnapshot {
                seq: 1,
                name: String::from("stub"),
                layout: LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
                focus: Some(ViewId::new(1)),
                panes: vec![bitty_runtime::PaneSnapshot {
                    view: ViewId::new(1),
                    cwd: None,
                    scrollback: Vec::new(),
                    attach: Some(PaneAttachment::Session),
                    route: PaneRoute::Terminal,
                    mode: PresentationMode::Tiled,
                }],
            }],
            active: 0,
            mru: vec![0],
            pinned: Vec::new(),
        }
    }

    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            files: Mutex::new(HashMap::new()),
            fail_commit: AtomicBool::new(false),
            fail_decode: AtomicBool::new(false),
            canned: Self::canned_snapshot(),
        })
    }

    fn set_fail_decode(&self, fail: bool) {
        self.fail_decode.store(fail, Ordering::SeqCst);
    }

    fn record(&self, call: &'static str) {
        self.calls.lock().expect("stub lock").push(call);
    }

    fn call_count(&self, call: &'static str) -> usize {
        self.calls
            .lock()
            .expect("stub lock")
            .iter()
            .filter(|c| ***c == *call)
            .count()
    }
}

impl SessionFileBackend for StubBackend {
    fn encode_snapshot(&self, snap: &SessionSnapshot) -> Result<Vec<u8>, SessionError> {
        self.record("encode");
        Ok(format!("stub:{}", snap.workspaces.len()).into_bytes())
    }

    fn decode_snapshot(&self, bytes: &[u8]) -> Result<SessionSnapshot, SessionError> {
        self.record("decode");
        if self.fail_decode.load(Ordering::SeqCst) {
            return Err(SessionError::Corrupt("stub decode refused"));
        }
        assert!(
            bytes.starts_with(b"stub:"),
            "stub decodes only stub encodings"
        );
        Ok(self.canned.clone())
    }

    fn commit_session_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), SessionError> {
        self.record("commit");
        if self.fail_commit.load(Ordering::SeqCst) {
            return Err(SessionError::Io(String::from("stub commit refused")));
        }
        self.files
            .lock()
            .expect("stub lock")
            .insert(path.to_path_buf(), bytes.to_vec());
        Ok(())
    }

    fn load_session_bytes(&self, path: &Path) -> Result<Vec<u8>, SessionError> {
        self.record("load");
        self.files
            .lock()
            .expect("stub lock")
            .get(path)
            .cloned()
            .ok_or(SessionError::NotFound)
    }

    fn session_file_for(
        &self,
        xdg_state_home: Option<&str>,
        home: Option<&str>,
    ) -> Option<PathBuf> {
        self.record("paths");
        // Same resolution rule as the production backend (XDG first,
        // HOME fallback, fail-closed None); kept here so hermetic tests
        // never read the live environment.
        let base = match (xdg_state_home, home) {
            (Some(xdg), _) if !xdg.trim().is_empty() => PathBuf::from(xdg),
            (_, Some(home)) if !home.trim().is_empty() => {
                PathBuf::from(home).join(".local").join("state")
            }
            _ => return None,
        };
        Some(base.join("bitty").join("sessions").join("session"))
    }

    fn session_file(&self) -> Option<PathBuf> {
        self.record("paths");
        None
    }
}

fn present_runtime(backend: &Arc<StubBackend>) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("defaults build");
    rt.set_session_backend(Some(backend.clone() as Arc<dyn SessionFileBackend>));
    rt
}

fn scratch_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "bitty-seam-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    ))
}

#[test]
fn absent_backend_save_reports_content_free_error() {
    let rt = Runtime::with_defaults().expect("defaults build");
    let err = rt
        .save_session_to_path(Path::new("/tmp/nowhere-session"))
        .expect_err("absent backend must fail");
    assert!(
        matches!(err, SessionError::Io(_)),
        "fail closed with a content-free error: {err}"
    );
    assert!(
        !format!("{err}").contains("nowhere"),
        "errors never echo paths or contents"
    );
}

#[test]
fn absent_backend_default_save_skips_quietly() {
    let rt = Runtime::with_defaults().expect("defaults build");
    // No backend means no durable store is configured: the same silent skip
    // as a missing state dir, and the exit path stays quiet too.
    assert!(matches!(
        rt.save_session_to_default_path(),
        Err(SessionError::NoStateDir)
    ));
    assert!(matches!(
        rt.save_session_on_exit(),
        bitty_runtime::SessionExitSaveOutcome::SkippedNoStateDir
    ));
}

#[test]
fn absent_backend_restore_starts_clean_and_leaves_state_intact() {
    let dir = scratch_dir("absent");
    let path = dir.join("session");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::write(&path, b"bitty-session v2\nleft-behind\n").expect("seed file");

    let mut rt = Runtime::with_defaults().expect("defaults build");
    let outcome =
        rt.restore_session_on_startup_with_env(false, Some(dir.to_str().expect("utf8")), None);
    assert!(
        matches!(outcome, bitty_runtime::SessionStartupOutcome::Fresh),
        "absent backend is a quiet clean start: {outcome:?}"
    );
    assert_eq!(
        std::fs::read(&path).expect("read back"),
        b"bitty-session v2\nleft-behind\n",
        "previous state stays intact"
    );
    assert!(!rt.session_restored());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn safe_mode_never_touches_the_backend() {
    struct PanicBackend;
    impl std::fmt::Debug for PanicBackend {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("PanicBackend")
        }
    }
    impl SessionFileBackend for PanicBackend {
        fn encode_snapshot(&self, _snap: &SessionSnapshot) -> Result<Vec<u8>, SessionError> {
            panic!("backend must not be touched in safe mode")
        }
        fn decode_snapshot(&self, _bytes: &[u8]) -> Result<SessionSnapshot, SessionError> {
            panic!("backend must not be touched in safe mode")
        }
        fn commit_session_bytes(&self, _path: &Path, _bytes: &[u8]) -> Result<(), SessionError> {
            panic!("backend must not be touched in safe mode")
        }
        fn load_session_bytes(&self, _path: &Path) -> Result<Vec<u8>, SessionError> {
            panic!("backend must not be touched in safe mode")
        }
        fn session_file_for(&self, _x: Option<&str>, _h: Option<&str>) -> Option<PathBuf> {
            panic!("backend must not be touched in safe mode")
        }
        fn session_file(&self) -> Option<PathBuf> {
            panic!("backend must not be touched in safe mode")
        }
    }

    let dir = scratch_dir("safe");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let mut rt = Runtime::with_defaults().expect("defaults build");
    rt.set_session_backend(Some(Arc::new(PanicBackend)));
    let outcome =
        rt.restore_session_on_startup_with_env(true, Some(dir.to_str().expect("utf8")), None);
    assert!(matches!(
        outcome,
        bitty_runtime::SessionStartupOutcome::SkippedSafeMode
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn present_backend_save_then_load_round_trips_through_the_seam() {
    let backend = StubBackend::new();
    let dir = scratch_dir("present");
    let path = dir.join("session");

    let rt = present_runtime(&backend);
    let summary = rt.save_session_to_path(&path).expect("save works");
    assert_eq!(backend.call_count("encode"), 1);
    assert_eq!(backend.call_count("commit"), 1);
    assert_eq!(summary.workspaces, 1);
    assert_eq!(
        summary.bytes,
        backend
            .files
            .lock()
            .expect("stub lock")
            .get(&path)
            .expect("committed")
            .len()
    );

    let mut fresh = present_runtime(&backend);
    let restore = fresh.load_session_from_path(&path).expect("load works");
    assert_eq!(backend.call_count("load"), 1);
    assert_eq!(backend.call_count("decode"), 1);
    assert_eq!(restore.workspaces, 1);
    assert!(fresh.session_restored());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_decode_leaves_the_runtime_untouched() {
    let backend = StubBackend::new();
    let dir = scratch_dir("corrupt");
    let path = dir.join("session");

    let rt = present_runtime(&backend);
    rt.save_session_to_path(&path).expect("save works");

    backend.set_fail_decode(true);
    let mut fresh = present_runtime(&backend);
    let layout_fresh = format!("{:?}", fresh.layout().leaf_ids());
    assert!(fresh.load_session_from_path(&path).is_err());
    assert_eq!(
        format!("{:?}", fresh.layout().leaf_ids()),
        layout_fresh,
        "failed load leaves the runtime untouched"
    );
    assert!(!fresh.session_restored());
    let _ = std::fs::remove_dir_all(&dir);
}
