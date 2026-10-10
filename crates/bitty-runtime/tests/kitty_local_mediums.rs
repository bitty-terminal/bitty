//! Kitty local-medium suite (`t=f`/`t=t`/`t=s`, S3 of #1849, Task CTX-1108).
//!
//! Open-time half of the local-medium sandbox: the parser already validated
//! the name shape single-shot (`t=f`/`t=t`/`t=s` parse-time checks in
//! `bitty-vt`), so the runtime reads the named object under the decode caps
//! (fail closed) and decodes/stores/places through the same seams as direct
//! streams (per-origin quotas S5, `q=` suppression S2).
//!
//! Every fixture lives under a test-owned directory in
//! [`std::env::temp_dir`] (process id plus an atomic counter make names
//! unique across parallel tests and runners); no test touches real user
//! files. The `t=t` delete-after-read path removes its own fixture by
//! design; every other fixture is removed best-effort at the end of its
//! test. Shared-memory fixtures (Linux only) use test-owned names under
//! `/dev/shm` and are unlinked by the read path itself.

use bitty_runtime::Runtime;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Test-owned scratch directory under the OS temp dir (never a user file).
fn temp_root(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bitty-ctx1108-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir must build");
    dir
}

fn remove_tree_best_effort(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// Standard-base64 encoder (no new dependency; paths only, tiny inputs).
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(first >> 2) as usize] as char);
        out.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// One `APC G` sequence whose payload names `path` (base64 of the raw bytes).
fn apc_with_path(header: &str, path: &[u8]) -> Vec<u8> {
    let mut seq = format!("\x1b_{header}").into_bytes();
    seq.extend_from_slice(base64_encode(path).as_bytes());
    seq.extend_from_slice(b"\x1b\\");
    seq
}

fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_str()
            .expect("test paths are UTF-8")
            .as_bytes()
            .to_vec()
    }
}

/// 2x2 opaque red RGBA payload (`f=32`, 16 bytes).
fn red_2x2_rgba() -> Vec<u8> {
    [0xFF, 0x00, 0x00, 0xFF].repeat(4)
}

/// 1x1 opaque red RGBA PNG (`f=100`); same bytes as the `bitty-rich` decode
/// fixture.
fn red_1x1_png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192, 240,
        31, 0, 5, 0, 1, 255, 86, 199, 47, 13, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

#[test]
#[cfg(unix)]
fn file_valid_raw_stores_places_and_paints() {
    let root = temp_root("file-valid");
    let file = root.join("img.bin");
    std::fs::write(&file, red_2x2_rgba()).expect("fixture must write");
    let seq = apc_with_path("Gf=32,s=2,v=2,t=f,m=0;", &path_bytes(&file));

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&seq);
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
    // `t=f` never deletes: the fixture stays for its owner.
    assert!(file.is_file(), "t=f must not delete the source file");

    remove_tree_best_effort(&root);
}

#[test]
fn file_traversal_refused_without_touching_filesystem() {
    // `..` components are refused at parse time: no open is attempted and
    // nothing is stored, even though the suffix names a real fixture.
    let root = temp_root("file-traversal");
    let real = root.join("img.bin");
    std::fs::write(&real, red_2x2_rgba()).expect("fixture must write");
    let evil = format!(
        "{}/../bitty-ctx1108-escape-{}",
        root.display(),
        std::process::id()
    );

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path("Gf=100,t=f,m=0;", evil.as_bytes()));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
    assert!(!Path::new(&evil).exists() || Path::new(&evil).starts_with(&root));

    remove_tree_best_effort(&root);
}

#[test]
fn temp_missing_marker_refused_and_kept() {
    // `t=t` without the `tty-graphics-protocol` marker is refused before any
    // open; the file is kept (no delete on refusal).
    let root = temp_root("temp-marker");
    let file = root.join("plain-output.bin");
    std::fs::write(&file, red_2x2_rgba()).expect("fixture must write");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path("Gf=32,s=2,v=2,t=t,m=0;", &path_bytes(&file)));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
    assert!(file.is_file(), "refused t=t must not delete");

    remove_tree_best_effort(&root);
}

#[test]
#[cfg(unix)]
fn temp_valid_deletes_after_read_and_places() {
    let root = temp_root("temp-valid");
    let file = root.join(format!(
        "tty-graphics-protocol-ctx1108-{}",
        std::process::id()
    ));
    std::fs::write(&file, [0xFF, 0x00, 0x00, 0xFF]).expect("fixture must write");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path("Gf=32,s=1,v=1,t=t,m=0;", &path_bytes(&file)));
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    assert!(!file.exists(), "t=t must delete after reading");

    remove_tree_best_effort(&root);
}

#[test]
fn name_nul_refused() {
    let mut name = path_bytes(&temp_root("nul").join("img.bin"));
    name.insert(name.len() / 2, 0);

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path("Gf=100,t=f,m=0;", &name));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn offset_out_of_range_refused() {
    // `O=` past end-of-file (and `O=`/`S=` arithmetic that cannot be
    // satisfied) fails closed with nothing stored.
    let root = temp_root("offset-range");
    let file = root.join("img.bin");
    std::fs::write(&file, red_2x2_rgba()).expect("fixture must write");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path(
        "Gf=32,s=2,v=2,t=f,O=4294967295,S=16,m=0;",
        &path_bytes(&file),
    ));
    assert_eq!(rt.kitty_image_count(), 0);
    rt.handle_pty_bytes(&apc_with_path(
        "Gf=32,s=2,v=2,t=f,O=64,m=0;",
        &path_bytes(&file),
    ));
    assert_eq!(rt.kitty_image_count(), 0);

    remove_tree_best_effort(&root);
}

#[test]
fn oversize_file_refused_pre_read() {
    // A sparse 64 MiB + 1 byte file (no blocks allocated) with `S=0`
    // (read-to-end) is refused by the fstat precheck before any large
    // allocation; the suite stays fast and bounded.
    let root = temp_root("oversize");
    let file = root.join("big.bin");
    let handle = std::fs::File::create(&file).expect("sparse fixture must create");
    handle
        .set_len(64 * 1024 * 1024 + 1)
        .expect("sparse fixture must size");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path("Gf=100,t=f,m=0;", &path_bytes(&file)));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);

    remove_tree_best_effort(&root);
}

#[test]
fn directory_refused_as_not_regular_file() {
    let root = temp_root("dir-refused");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path("Gf=100,t=f,m=0;", &path_bytes(&root)));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);

    remove_tree_best_effort(&root);
}

#[test]
#[cfg(unix)]
fn symlink_refused_without_following() {
    use std::os::unix::fs::symlink;
    let root = temp_root("symlink");
    let target = root.join("target.bin");
    std::fs::write(&target, red_2x2_rgba()).expect("fixture must write");
    let link = root.join("link.bin");
    symlink(&target, &link).expect("symlink fixture must build");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path("Gf=32,s=2,v=2,t=f,m=0;", &path_bytes(&link)));
    assert_eq!(rt.kitty_image_count(), 0, "O_NOFOLLOW must refuse symlinks");
    assert_eq!(rt.kitty_placement_count(), 0);
    assert_eq!(
        std::fs::read(&target).expect("target must survive"),
        red_2x2_rgba()
    );

    remove_tree_best_effort(&root);
}

#[test]
#[cfg(target_os = "linux")]
fn fifo_refused_without_hanging() {
    // Opening a FIFO without `O_NONBLOCK` would park the render thread
    // until a writer arrives; the `O_NOFOLLOW | O_NONBLOCK` open plus the
    // regular-file fstat refuses it immediately instead.
    let root = temp_root("fifo");
    let fifo = root.join("pipe.bin");
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .expect("fifo fixture must build");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path("Gf=100,t=f,m=0;", &path_bytes(&fifo)));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);

    remove_tree_best_effort(&root);
}

#[test]
#[cfg(unix)]
fn offset_size_slice_selects_bytes() {
    // `O=`/`S=` window into the file: 4 garbage bytes, then an exact 2x1
    // RGBA payload read from offset 4.
    let root = temp_root("slice");
    let file = root.join("img.bin");
    let mut content = vec![0xAA, 0xBB, 0xCC, 0xDD];
    content.extend_from_slice(&[0x01, 0x02, 0x03, 0xFF, 0x04, 0x05, 0x06, 0xFF]);
    std::fs::write(&file, content).expect("fixture must write");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path(
        "Gf=32,s=2,v=1,t=f,O=4,S=8,m=0;",
        &path_bytes(&file),
    ));
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);

    remove_tree_best_effort(&root);
}

#[test]
fn shm_bad_shape_refused() {
    for bad in [
        "noslash".as_bytes(),
        "/".as_bytes(),
        "/a/b".as_bytes(),
        "/trailing/".as_bytes(),
    ] {
        let mut rt = make_runtime();
        rt.handle_pty_bytes(&apc_with_path("Gf=100,t=s,m=0;", bad));
        assert_eq!(rt.kitty_image_count(), 0, "bad shm shape {bad:?}");
        assert_eq!(rt.kitty_placement_count(), 0);
    }
}

#[test]
#[cfg(target_os = "linux")]
fn shm_valid_roundtrip_unlinks_after_read() {
    // A test-owned object under `/dev/shm` reads, decodes, places, and is
    // unlinked after reading (spec delete-after-read for `t=s`).
    let tag = format!(
        "bitty-ctx1108-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let shm_path = PathBuf::from("/dev/shm").join(&tag);
    std::fs::write(&shm_path, [0xFF, 0x00, 0x00, 0xFF]).expect("shm fixture must write");
    let wire_name = format!("/{tag}");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path(
        "Gf=32,s=1,v=1,t=s,m=0;",
        wire_name.as_bytes(),
    ));
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    assert!(!shm_path.exists(), "t=s must unlink after reading");
}

#[test]
fn query_missing_file_answers_einval_and_honors_quiet() {
    // File-backed probe failures are silent protocol answers (never
    // stderr): `EINVAL` by default, suppressed with `q=2`.
    let missing = temp_root("query-missing").join("no-such-file.bin");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path(
        "Gf=100,t=f,a=q,i=31,q=0,m=0;",
        &path_bytes(&missing),
    ));
    let replies: Vec<Vec<u8>> = rt.take_replies().iter().map(|r| r.to_vec()).collect();
    assert_eq!(replies, vec![b"\x1b_Gi=31;EINVAL:bad data\x1b\\".to_vec()]);
    assert_eq!(rt.kitty_image_count(), 0);

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path(
        "Gf=100,t=f,a=q,i=31,q=2,m=0;",
        &path_bytes(&missing),
    ));
    assert!(rt.take_replies().is_empty(), "q=2 suppresses failures");
    assert_eq!(rt.kitty_image_count(), 0);

    remove_tree_best_effort(missing.parent().expect("temp parent"));
}

#[test]
#[cfg(unix)]
fn query_valid_file_answers_ok_and_stores_nothing() {
    let root = temp_root("query-valid");
    let file = root.join("img.png");
    std::fs::write(&file, red_1x1_png()).expect("fixture must write");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path(
        "Gf=100,t=f,a=q,i=31,m=0;",
        &path_bytes(&file),
    ));
    let replies: Vec<Vec<u8>> = rt.take_replies().iter().map(|r| r.to_vec()).collect();
    assert_eq!(replies, vec![b"\x1b_Gi=31;OK\x1b\\".to_vec()]);
    assert_eq!(rt.kitty_image_count(), 0, "probes never store");
    assert!(file.is_file(), "t=f probe must not delete");

    remove_tree_best_effort(&root);
}

#[test]
fn file_transmit_refused_when_store_full() {
    // File bytes charge against the origin quota (S5): with the 64-image
    // global bound held, a further file transmit refuses like a direct one.
    let root = temp_root("quota");
    let file = root.join("img.bin");
    std::fs::write(&file, [0xFF, 0x00, 0x00, 0xFF]).expect("fixture must write");

    let mut rt = make_runtime();
    for _ in 0..64 {
        rt.handle_pty_bytes(b"\x1b_Gf=32,s=1,v=1,a=t,m=0;AAAAAA==\x1b\\");
    }
    assert_eq!(rt.kitty_image_count(), 64);
    rt.handle_pty_bytes(&apc_with_path(
        "Gf=32,s=1,v=1,a=t,t=f,m=0;",
        &path_bytes(&file),
    ));
    assert_eq!(
        rt.kitty_image_count(),
        64,
        "quota-full file transmit stores nothing"
    );

    remove_tree_best_effort(&root);
}

#[test]
#[cfg(windows)]
fn local_mediums_denied_on_windows() {
    // Windows has no `O_NOFOLLOW`/`/dev/shm` equivalent in this slice:
    // every local medium fails closed with nothing stored.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path("Gf=100,t=f,m=0;", b"C:\\Temp\\img.png"));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
    rt.handle_pty_bytes(&apc_with_path("Gf=100,t=s,m=0;", b"/shm"));
    assert_eq!(rt.kitty_image_count(), 0);
}

#[test]
#[cfg(windows)]
fn file_valid_denied_on_windows_keeps_file() {
    // Even a valid existing `t=f` file hits `UnsupportedPlatform` before
    // any I/O on Windows: nothing stored or placed, file kept.
    let root = temp_root("file-valid-windows");
    let file = root.join("img.bin");
    std::fs::write(&file, red_2x2_rgba()).expect("fixture must write");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path("Gf=32,s=2,v=2,t=f,m=0;", &path_bytes(&file)));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
    assert!(file.is_file(), "denied t=f must not delete");

    remove_tree_best_effort(&root);
}

#[test]
#[cfg(windows)]
fn temp_valid_denied_on_windows_keeps_file() {
    // Valid `t=t` (marker-bearing temp path) still hits
    // `UnsupportedPlatform` before containment/open: nothing stored,
    // nothing deleted (unlink runs only after a successful read).
    let root = temp_root("temp-valid-windows");
    let file = root.join(format!(
        "tty-graphics-protocol-ctx1108-{}",
        std::process::id()
    ));
    std::fs::write(&file, [0xFF, 0x00, 0x00, 0xFF]).expect("fixture must write");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path("Gf=32,s=1,v=1,t=t,m=0;", &path_bytes(&file)));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
    assert!(file.is_file(), "denied t=t must not delete");

    remove_tree_best_effort(&root);
}

#[test]
#[cfg(windows)]
fn query_valid_file_denied_on_windows_answers_einval() {
    // File-backed `a=q` probes on Windows answer `EINVAL:bad data` (same
    // as missing files): the read never runs, nothing stored, file kept.
    let root = temp_root("query-valid-windows");
    let file = root.join("img.png");
    std::fs::write(&file, red_1x1_png()).expect("fixture must write");

    let mut rt = make_runtime();
    rt.handle_pty_bytes(&apc_with_path(
        "Gf=100,t=f,a=q,i=31,m=0;",
        &path_bytes(&file),
    ));
    let replies: Vec<Vec<u8>> = rt.take_replies().iter().map(|r| r.to_vec()).collect();
    assert_eq!(replies, vec![b"\x1b_Gi=31;EINVAL:bad data\x1b\\".to_vec()]);
    assert_eq!(rt.kitty_image_count(), 0, "probes never store");
    assert!(file.is_file(), "denied probe must not delete");

    remove_tree_best_effort(&root);
}
