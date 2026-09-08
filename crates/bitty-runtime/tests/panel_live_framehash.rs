#![forbid(unsafe_code)]
#![allow(deprecated)]
//! Panel live V1–V3 gates on `frameHash` equality (CTX-0242, CTX-0244 transport).
//!
//! The lossless question is equality, not transport: does the frame the IPC
//! path observed equal the frame the present path produced? `frameHash`
//! (`bitty.debug/frameHash`, CTX-0244) answers it with a SHA-256 digest over
//! canonical `BFH1` header + `headless_rgba` — 32 bytes, zero pixel movement,
//! uninvertible (P0-AC-026 parity: no clipboard/env bytes can leak).
//!
//! What each gate pins (all headless, deterministic, synthetic-only):
//! - V1 gaps band paint: gapped digest differs from the no-gap baseline for
//!   identical content, and a fresh re-run reproduces the gapped digest
//!   exactly. One gap-band pixel is also asserted == theme bg directly
//!   (`panel_gaps.rs` parity: CTX-0151 stale-bg / CTX-0228 no-repaint class).
//! - V2 focus-switch diff: moving focus moves the primary fallback between
//!   tiles (CTX-0234 rule) so the digest changes; switching back restores
//!   the exact digest. Headless `Runtime` renders no tabline strip, so the
//!   stack-focus pixel delta lives in the workspaceline widget (plugin UI,
//!   live-only); the split-focus analogue below pins the same mechanism
//!   (focus change forces a full present per CTX-0228, primary follows
//!   focus per CTX-0234). Live ws4 V2 covers the tabline region diff
//!   (see the ignored live-grant test at the bottom).
//! - V3 rename parity: `WorkspaceIntegration::stack_for_workspace` and the
//!   deprecated `TabsIntegration::stack_for_tabs` shim produce identical
//!   layouts and identical frame digests for identical content.
//! - Socket round-trip: a real `Runtime` headless frame published via
//!   `publish_frame_rgba` digests identically over a real Unix socket
//!   through `bitty.debug/frameHash` (CTX-0188 harness pattern: real
//!   socket, file-local serial guard, reply correlation). This is the
//!   lossless proof the design note (§d) demands before any live suite.
//! - Live-grant ceremony stays local-manual: the ignored test at the bottom
//!   never runs in CI (no human consent available there).
//!
//! Digest/sequence discipline: `frame_digest_hex` binds `(width, height,
//! frameSeq, rgba)`. Pixel-equality assertions hash with a fixed seq `0` to
//! isolate pixels from the transport frame counter; the socket test uses the
//! real `PresentStats::frame` on both sides. Fresh `Runtime`s must also start
//! their frame counters identically (asserted in V1) or the real-seq
//! comparisons would be meaningless.

use bitty_ipc::frame_digest::{FRAME_DIGEST_ALGO, frame_digest_hex};
use bitty_runtime::{
    LayoutNode, PresentStats, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId,
    tabs::TabsIntegration, workspace::WorkspaceIntegration,
};

/// Serial guard for the process-global automation + introspection stores
/// (CTX-0179 pattern). EVERY test in this file takes it for its whole body:
/// `Runtime::tick` auto-publishes the presented frame into the global RGBA
/// digest store whenever a `FrameDigest` bearer is live
/// (`frame_digest_publish_wanted`), so a parallel V1/V2/V3 tick landing
/// inside the socket test's bearer window would overwrite the published
/// frame (served `frameSeq`/digest diverge from the local expectation —
/// the macOS-only CI flake). The headless digest gates are pure only while
/// no bearer exists; the guard makes that unconditional.
fn live_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

fn hold_live_lock() -> std::sync::MutexGuard<'static, ()> {
    live_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn two_pane_split() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    )
}

/// Writes a full-width synthetic marker row into the primary grid (cursor
/// parks on the next row, so row content and cursor ink never share a row).
/// Same helper as `split_zoom_repaint.rs`; content is synthetic fixture
/// bytes only (P0-AC-026 harness rule — never secrets).
fn write_marker(rt: &mut Runtime, row_1based: u8, glyph: u8) {
    assert!((1..=9).contains(&row_1based), "single-digit rows only");
    let mut seq = vec![0x1b, b'[', b'0' + row_1based, b';', b'1', b'H'];
    seq.extend(std::iter::repeat_n(glyph, 80));
    rt.handle_pty_bytes(&seq);
    let snap = rt.snapshot();
    let row = usize::from(row_1based - 1);
    assert!(
        snap.cells
            .chunks(snap.width)
            .nth(row)
            .is_some_and(|cells| cells.iter().all(|c| c.glyph as u8 == glyph)),
        "marker row {row_1based} must read back full-width"
    );
}

/// Present one synthetic frame and return `(width_px, height_px,
/// frame_seq, rgba)`. Asserts the surface matches the config extent.
fn present_frame(rt: &mut Runtime, what: &str) -> (u32, u32, u64, Vec<u8>) {
    let stats: PresentStats = rt.tick().unwrap_or_else(|| panic!("{what} must present"));
    assert!(stats.headless, "{what} must present headlessly");
    let extent = rt.config().window_extent();
    let (w, h) = (extent.width(), extent.height());
    let rgba = rt.headless_rgba().expect("rgba after present");
    assert_eq!(
        rgba.len(),
        w as usize * h as usize * 4,
        "{what} surface must match the config extent"
    );
    (w, h, stats.frame, rgba)
}

/// Pixel-isolating digest: fixed seq `0` so equality means pixel equality
/// (the transport counter is covered separately by the socket test with the
/// real `PresentStats::frame` on both sides).
fn digest_pixels(width: u32, height: u32, rgba: &[u8]) -> String {
    frame_digest_hex(width, height, 0, rgba)
}

fn rgba_at(rgba: &[u8], stride_px: usize, x: usize, y: usize) -> [u8; 4] {
    let i = (y * stride_px + x) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

#[test]
fn v1_gap_digest_differs_from_no_gap_and_matches_rerun() {
    // Serialized: ticks auto-publish into the global digest store while a
    // digest bearer is live (see `live_lock`), so this must not interleave
    // with the socket round-trip's bearer window.
    let _guard = hold_live_lock();
    // Gapped run: identical content, gaps_in=2/gaps_out=1.
    let mut gapped = Runtime::new(RuntimeConfig {
        gaps_in: 2,
        gaps_out: 1,
        ..RuntimeConfig::default()
    })
    .expect("gapped config must build");
    gapped.set_layout(two_pane_split());
    gapped.set_container(UiRect::new(0, 0, 80, 24));
    write_marker(&mut gapped, 3, b'M');
    let (w, h, seq_g, rgba_g) = present_frame(&mut gapped, "gapped split");
    assert_eq!(gapped.tick(), None, "must idle after gap present");

    // V1 paint contract parity (`panel_gaps.rs`): the inner gap band between
    // the two allocations is theme bg, not stale content.
    let bg = bitty_render::grid::DEFAULT_BG;
    let allocs = gapped.layout_allocations();
    assert_eq!(allocs.len(), 2);
    let gap_col = allocs[0].1.x + allocs[0].1.width;
    let gap_row = allocs[0].1.y + 2;
    let cfg = gapped.config();
    let cw = usize::try_from(cfg.cell_width).expect("cell width fits");
    let ch = usize::try_from(cfg.cell_height).expect("cell height fits");
    let pad = usize::try_from(gapped.window_padding_physical()).expect("pad fits");
    let gx = usize::from(gap_col) * cw + pad + cw / 2;
    let gy = usize::from(gap_row) * ch + pad + ch / 2;
    assert_eq!(
        rgba_at(&rgba_g, w as usize, gx, gy),
        bg,
        "inner gap band must be theme bg"
    );

    // No-gap baseline: identical layout + content, ZERO gaps.
    let mut plain = Runtime::new(RuntimeConfig::default()).expect("plain config must build");
    plain.set_layout(two_pane_split());
    plain.set_container(UiRect::new(0, 0, 80, 24));
    write_marker(&mut plain, 3, b'M');
    let (w2, h2, seq_p, rgba_p) = present_frame(&mut plain, "plain split");
    assert_eq!((w, h), (w2, h2), "extents must match (gaps do not resize)");
    // Fresh runtimes start their frame counters identically, so the real-seq
    // digest comparison below is a pixel comparison, not a counter artefact.
    assert_eq!(
        seq_g, seq_p,
        "fresh runtimes must start frame counters identically"
    );
    assert_ne!(
        rgba_g, rgba_p,
        "gap layout must move pixels vs the no-gap baseline"
    );
    assert_ne!(
        digest_pixels(w, h, &rgba_g),
        digest_pixels(w, h, &rgba_p),
        "V1: gapped digest must differ from the no-gap baseline"
    );
    assert_ne!(
        frame_digest_hex(w, h, seq_g, &rgba_g),
        frame_digest_hex(w, h, seq_p, &rgba_p),
        "V1: real-seq digests must differ too (same seq, different pixels)"
    );

    // Re-run: a fresh identical runtime reproduces the gapped frame exactly.
    let mut rerun = Runtime::new(RuntimeConfig {
        gaps_in: 2,
        gaps_out: 1,
        ..RuntimeConfig::default()
    })
    .expect("rerun config must build");
    rerun.set_layout(two_pane_split());
    rerun.set_container(UiRect::new(0, 0, 80, 24));
    write_marker(&mut rerun, 3, b'M');
    let (w3, h3, seq_r, rgba_r) = present_frame(&mut rerun, "gapped re-run");
    assert_eq!((w, h), (w3, h3));
    assert_eq!(seq_g, seq_r, "re-run must reach the same frame seq");
    assert_eq!(rgba_r, rgba_g, "re-run must reproduce pixels exactly");
    assert_eq!(
        digest_pixels(w, h, &rgba_r),
        digest_pixels(w, h, &rgba_g),
        "V1: re-run digest must match"
    );
    assert_eq!(
        frame_digest_hex(w3, h3, seq_r, &rgba_r),
        frame_digest_hex(w, h, seq_g, &rgba_g),
        "V1: real-seq re-run digest must match"
    );
}

#[test]
fn v2_focus_switch_changes_digest_and_switchback_restores() {
    // Serialized: see V1 — this test ticks one runtime three times
    // (frames 1, 2, 3), so an interleaved tick would overwrite the socket
    // test's published frame mid-window.
    let _guard = hold_live_lock();
    let mut rt = Runtime::with_defaults().expect("build");
    rt.set_layout(two_pane_split());
    rt.set_container(UiRect::new(0, 0, 80, 24));
    write_marker(&mut rt, 3, b'M');
    let (w, h, _, rgba_a) = present_frame(&mut rt, "baseline");
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    assert_eq!(rt.tick(), None, "must idle before focus switch");
    let digest_a = digest_pixels(w, h, &rgba_a);

    // Focus switch: the primary fallback follows focus (CTX-0234), so the
    // marker moves tiles and the digest must change (full present, CTX-0228).
    assert!(rt.set_focus(ViewId::new(2)));
    let stats_b: PresentStats = rt.tick().expect("focus switch must present");
    assert!(stats_b.headless);
    assert!(stats_b.fills > 0, "focus switch must carry fills");
    let rgba_b = rt.headless_rgba().expect("rgba after focus switch");
    assert_ne!(
        rgba_b, rgba_a,
        "focus switch must move pixels (primary follows focus)"
    );
    assert_ne!(
        digest_pixels(w, h, &rgba_b),
        digest_a,
        "V2: tab-switch digest must change"
    );
    assert_eq!(rt.tick(), None);

    // Switch back: the exact baseline frame must be restored.
    assert!(rt.set_focus(ViewId::new(1)));
    rt.tick().expect("switch-back must present");
    let rgba_c = rt.headless_rgba().expect("rgba after switch-back");
    assert_eq!(
        rgba_c, rgba_a,
        "switch-back must restore baseline pixels exactly"
    );
    assert_eq!(
        digest_pixels(w, h, &rgba_c),
        digest_a,
        "V2: switch-back digest must restore"
    );
    assert_eq!(rt.tick(), None, "must idle after restore");
}

#[test]
fn v3_workspace_alias_and_tabs_shim_digests_equal() {
    // Serialized: see V1.
    let _guard = hold_live_lock();
    let views = vec![
        View::new(ViewId::new(1), 80, 24),
        View::new(ViewId::new(2), 80, 24),
    ];
    let via_workspace = WorkspaceIntegration::stack_for_workspace(views.clone());
    let via_tabs = TabsIntegration::stack_for_tabs(views);
    // Layout parity first (the `tabs_compat.rs` guard, frame consequence below).
    let bounds = bitty_ui::Rect::new(0, 0, 80, 24);
    assert_eq!(
        via_workspace.layout(bounds),
        via_tabs.layout(bounds),
        "alias and shim must allocate identically"
    );

    let mut rt_new = Runtime::with_defaults().expect("new-path runtime must build");
    rt_new.set_layout(via_workspace);
    rt_new.set_container(UiRect::new(0, 0, 80, 24));
    write_marker(&mut rt_new, 3, b'M');
    let (w, h, seq_new, rgba_new) = present_frame(&mut rt_new, "workspace path");

    let mut rt_old = Runtime::with_defaults().expect("old-path runtime must build");
    rt_old.set_layout(via_tabs);
    rt_old.set_container(UiRect::new(0, 0, 80, 24));
    write_marker(&mut rt_old, 3, b'M');
    let (w2, h2, seq_old, rgba_old) = present_frame(&mut rt_old, "tabs-shim path");

    assert_eq!((w, h), (w2, h2));
    assert_eq!(seq_new, seq_old, "identical runs must reach the same seq");
    assert_eq!(
        rgba_old, rgba_new,
        "alias vs shim must render identical pixels for identical content"
    );
    assert_eq!(
        digest_pixels(w, h, &rgba_old),
        digest_pixels(w, h, &rgba_new),
        "V3: workspace alias vs tabs shim digests must be equal"
    );
    assert_eq!(
        frame_digest_hex(w2, h2, seq_old, &rgba_old),
        frame_digest_hex(w, h, seq_new, &rgba_new),
        "V3: real-seq digests must be equal too"
    );
}

/// Socket-level lossless proof: a real `Runtime` headless frame published
/// into the live stores digests identically through `bitty.debug/frameHash`
/// over a real Unix socket (CTX-0188 pattern: real socket, file-local serial
/// guard, reply correlation). No pixel bytes may reach the wire.
#[cfg(unix)]
#[test]
fn framehash_socket_roundtrip_matches_runtime_frame() {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use bitty_ipc::devtools::{
        AutomationFamily, Dispatcher, ServeContext, ServerInfo, clear_automation_for_tests,
        clear_introspection_for_tests, issue_automation_bearer_with_ttl, prepare_socket_dir,
        publish_frame_rgba, publish_grid_text, serve_connection, transport_attested_peer,
    };
    use bitty_ipc::frame::{MAX_FRAME_BYTES, encode_frame};
    use bitty_ipc::limits::RateLimiter;
    use bitty_ipc::scope::{Scope, ScopeSet};

    let _guard = hold_live_lock();
    clear_introspection_for_tests();
    clear_automation_for_tests();

    // A real present-path frame (gapped split + synthetic marker): the exact
    // bytes the V1 gate pins above.
    let mut rt = Runtime::new(RuntimeConfig {
        gaps_in: 2,
        gaps_out: 1,
        ..RuntimeConfig::default()
    })
    .expect("gapped config must build");
    rt.set_layout(two_pane_split());
    rt.set_container(UiRect::new(0, 0, 80, 24));
    write_marker(&mut rt, 3, b'M');
    let (w, h, seq, rgba) = present_frame(&mut rt, "socket-proof frame");
    // Quiesce: exactly one present must be pending-published. A second tick
    // here would mean the published frame is already stale before serving.
    assert_eq!(rt.tick(), None, "must idle after socket-proof present");

    publish_frame_rgba(w, h, seq, rgba.clone());
    publish_grid_text(
        vec!["synthetic harness line".to_string()],
        0,
        0,
        true,
        seq,
        80,
        24,
    );
    let bearer = issue_automation_bearer_with_ttl(
        "panel-live-proof",
        "t:1",
        AutomationFamily::FrameDigest,
        0,
        60_000,
    )
    .expect("digest bearer must mint");
    let mut scopes = ScopeSet::new();
    scopes.insert(Scope::DebugTrace);
    scopes.insert(Scope::TerminalInspect);

    let socket_path = format!("/tmp/btplp{}/s.sock", std::process::id());
    assert!(
        socket_path.len() < 100,
        "socket path must fit macOS SUN_LEN"
    );
    prepare_socket_dir(&socket_path).expect("socket dir");
    let server_scopes = scopes.clone();
    let server = std::thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let verified = {
            use std::os::unix::fs::MetadataExt;
            let uid = std::fs::metadata(&socket_path)
                .map(|m| m.uid())
                .unwrap_or(0);
            transport_attested_peer(uid)
        };
        let dispatcher = Dispatcher::with_defaults();
        let info = ServerInfo::new("panel-live-proof".to_string(), socket_path.clone(), 80, 24);
        let mut context =
            ServeContext::with_granted_session(&info, server_scopes, "panel-live-proof");
        context.attest_local_peer();
        let mut limiter = RateLimiter::rc9_default();
        let clock = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0)
        };
        let stats = serve_connection(
            &mut stream,
            verified,
            &dispatcher,
            &context,
            &mut limiter,
            &clock,
        )
        .expect("serve");
        assert!(stats.requests >= 1, "expected test requests");
        assert_eq!(stats.responses, stats.requests);
    });
    std::thread::sleep(Duration::from_millis(100));
    let mut stream =
        UnixStream::connect(format!("/tmp/btplp{}/s.sock", std::process::id()).as_str())
            .expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");

    let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{bearer}\"}}");
    let envelope = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"bitty.debug/frameHash\",\"params\":{params},\"version\":\"1.0\"}}"
    );
    let wire = encode_frame(envelope.as_bytes()).expect("frame");
    stream.write_all(&wire).expect("write");
    stream.flush().expect("flush");
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).expect("header");
    let len = u32::from_be_bytes(header) as usize;
    assert!(len <= MAX_FRAME_BYTES, "response exceeds frame bound");
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).expect("body");
    let proof = String::from_utf8(body).expect("utf8");
    assert!(
        proof.contains("\"id\":1"),
        "response lost correlation id: {proof}"
    );

    let expect = frame_digest_hex(w, h, seq, &rgba);
    assert!(
        proof.contains("\"snapshot\":\"frameHash\""),
        "unexpected proof: {proof}"
    );
    assert!(
        proof.contains(&format!("\"algo\":\"{FRAME_DIGEST_ALGO}\"")),
        "unexpected proof: {proof}"
    );
    assert!(
        proof.contains(&format!("\"digest\":\"{expect}\"")),
        "socket digest must equal the local present-path digest: {proof}"
    );
    // Pin the sequence too: digest equality is only meaningful for the exact
    // frame the local present produced (single present → single publish →
    // single serve, no ticks in between).
    assert!(
        proof.contains(&format!("\"frameSeq\":{seq}")),
        "served frameSeq must equal the locally hashed seq: {proof}"
    );
    assert!(
        proof.contains("\"trust\":\"untrusted-observation\""),
        "unexpected proof: {proof}"
    );
    assert!(proof.len() < 512, "digest response must stay tiny");

    drop(stream);
    server.join().expect("server");
    clear_automation_for_tests();
    clear_introspection_for_tests();
    std::fs::remove_file(format!("/tmp/btplp{}/s.sock", std::process::id())).ok();
}

/// Local-manual only: the live-grant ceremony (a human confirming
/// synthetic-only content at the DevTools/`bitty dev` consent prompt)
/// cannot run in CI. This test never runs by default (`#[ignore]`); run it
/// explicitly on ws4 with `BITTY_LIVE_GRANT_TESTS=1` against a live instance.
#[test]
#[ignore = "manual-only: needs a live ws4 instance plus a human consent ceremony; CI must never mint real grants"]
fn panel_live_v1_v2_v3_manual_only() {
    if std::env::var("BITTY_LIVE_GRANT_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set BITTY_LIVE_GRANT_TESTS=1 to run against a live instance");
        return;
    }
    // Manual procedure on ws4 (prebuilt binary only, synthetic-only fixture
    // content — never passwords, tokens, clipboard, or env bytes):
    // V1: split 2 panes, set layout.gaps_in/out, frameHash before/after —
    //     digests differ; re-capture without changes reproduces the digest.
    // V2: 3 tabs via workspace shim, focus each with frameHash per step —
    //     each digest differs, refocusing the first restores its digest.
    // V3: activate the same content via bitty-terminal.tabs:new and
    //     bitty-terminal.workspace:new — frameHash digests must be equal.
    // Then confirm ScopeDenied after grant expiry (<= 120 s) and a digest
    // audit trail on the serving side.
    panic!("manual-only: point this at a live BITTY_SOCKET before enabling");
}
