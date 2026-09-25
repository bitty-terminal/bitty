#![cfg(unix)]

//! DT-02 observability event pipeline verification (CTX-0591, issue #1098).
//!
//! Verifies the `bitty-ipc` profiling observability lane reachable today:
//! bounded latest-wins rings with counted `DropOldest` attribution, bounded
//! batching (`drain_batch`: 32 records or 8 KiB), separate `debug.inspect`
//! getters vs `debug.trace` streams, and scope isolation with zero partial
//! state on denial. Source: `devtools-rfc.md` § "Observability event
//! pipeline" (delivery/ordering/coalescing points 1-5, publisher/drain) and
//! § "Instrumentation" (queue accounting). Cited controls: P0-AC-014
//! (budgets attributable) and P0-AC-025 (scope matrix; no control weakened).
//!
//! Companion runtime-side proofs (the bounded input-observability ring's
//! counted drop-oldest) live in `bitty-runtime/tests/devtools_observability.rs`.
//!
//! Headless. The live profiling store is process-global, so every test holds
//! the file-local serial guard for its whole publish-assert sequence
//! (CTX-0179 pattern; std-only, no extra dev-dependency) and clears before and
//! after.

use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};

use bitty_ipc::devtools::{
    Dispatcher, FrameStatsPublish, ProcessStatsPublish, ServeContext, ServerInfo,
    clear_profiling_for_tests, handle_envelope, publish_frame_stats, publish_process_stats,
};
use bitty_ipc::scope::{Scope, ScopeSet};

/// Serial guard for the process-global profiling store.
fn obs_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn hold_obs_lock() -> std::sync::MutexGuard<'static, ()> {
    obs_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn test_server() -> ServerInfo {
    ServerInfo::new(
        "obs-proof".to_string(),
        "/run/user/1000/bitty/obs-proof.sock".to_string(),
        80,
        24,
    )
}

fn inspect_scopes() -> ScopeSet {
    let mut set = ScopeSet::new();
    set.insert(Scope::DebugInspect);
    set
}

fn trace_scopes() -> ScopeSet {
    let mut set = ScopeSet::new();
    set.insert(Scope::DebugTrace);
    set
}

fn call(granted: ScopeSet, method: &str, params: Option<&str>) -> (bool, String) {
    let dispatcher = Dispatcher::with_defaults();
    let (_client, stream) = UnixStream::pair().unwrap();
    let mut ctx = ServeContext::with_granted_session(&test_server(), granted, "obs-sess");
    ctx.bind_connected_stream_current(&stream).unwrap();
    let envelope = match params {
        Some(p) => {
            format!("{{\"id\":1,\"method\":\"{method}\",\"version\":\"1.0\",\"params\":{p}}}")
        }
        None => format!("{{\"id\":1,\"method\":\"{method}\",\"version\":\"1.0\"}}"),
    };
    let outcome = handle_envelope(envelope.as_bytes(), &dispatcher, &ctx);
    let text = String::from_utf8(outcome.response).unwrap();
    (outcome.was_error, text)
}

fn process_sample(rss: u64) -> ProcessStatsPublish {
    ProcessStatsPublish {
        rss_bytes: rss,
        cpu_avg_pct_x100: 100,
        tasks: 7,
        timers: 3,
        window_ms: 600_000,
    }
}

fn frame_sample(backend: &str) -> FrameStatsPublish {
    FrameStatsPublish {
        frame_p50_us: 3_000,
        frame_p99_us: 9_000,
        presented_fps: 60,
        missed_presents: 2,
        gpu_bytes: Some(12_582_912),
        backend: backend.to_string(),
    }
}

fn numeric_field(text: &str, key: &str) -> u64 {
    let needle = format!("\"{key}\":");
    let at = text
        .find(&needle)
        .unwrap_or_else(|| panic!("missing field {key} in {text}"))
        + needle.len();
    let rest = &text[at..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    assert!(!digits.is_empty(), "field {key} is not numeric in {text}");
    digits.parse().unwrap()
}

#[test]
fn obs_profiling_ring_bounds_drop_oldest_with_counted_attribution() {
    let _guard = hold_obs_lock();
    clear_profiling_for_tests();
    for i in 1..=40u64 {
        publish_process_stats(i * 100, process_sample(i));
    }
    let (was_error, text) = call(
        trace_scopes(),
        "bitty.debug/streamProcessStats",
        Some("{\"intervalMs\":100}"),
    );
    assert!(!was_error, "unexpected error: {text}");
    // 40 published, ring of 32: newest 32 (seq 9..=40), 8 counted drops.
    // The stream header carries the head sequence after the samples array.
    let head = text
        .rfind("\"seq\":")
        .map(|at| &text[at + "\"seq\":".len()..])
        .map(|rest| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .unwrap_or_default();
    assert_eq!(
        head, "40",
        "stream header must report the head sequence: {text}"
    );
    assert_eq!(numeric_field(&text, "dropped"), 8, "got: {text}");
    assert!(text.contains("\"truncated\":false"), "got: {text}");
    assert!(
        text.contains("\"seq\":9,"),
        "oldest retained must be 9: {text}"
    );
    assert!(!text.contains("\"seq\":8,"), "seq 8 must drop: {text}");
    clear_profiling_for_tests();
}

#[test]
fn obs_profiling_drain_batch_is_bounded_by_count_and_bytes() {
    let _guard = hold_obs_lock();
    clear_profiling_for_tests();
    let long_backend = "b".repeat(256);
    for i in 1..=32u64 {
        publish_frame_stats(i * 100, frame_sample(&long_backend));
    }
    // The 8 KiB aggregate drain bound must stop the batch early rather than
    // emit one oversized callback (RFC batching point 5).
    let (was_error, text) = call(
        trace_scopes(),
        "bitty.debug/streamFrameStats",
        Some("{\"intervalMs\":100,\"maxSamples\":32}"),
    );
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"truncated\":true"), "got: {text}");
    assert!(
        text.len() <= 8 * 1024 + 512,
        "drain exceeded the 8 KiB batch bound: {} bytes",
        text.len()
    );
    assert_eq!(numeric_field(&text, "dropped"), 0, "got: {text}");
    clear_profiling_for_tests();
}

#[test]
fn obs_profiling_empty_store_reports_none_without_fabricated_zeros() {
    let _guard = hold_obs_lock();
    clear_profiling_for_tests();
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getProcessStats", None);
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"sample\":\"none\""), "got: {text}");
    assert!(
        !text.contains("rssBytes"),
        "empty store must not fabricate numbers: {text}"
    );
    clear_profiling_for_tests();
}

#[test]
fn obs_profiling_isolated_scope_lanes_getters_vs_streams() {
    let _guard = hold_obs_lock();
    clear_profiling_for_tests();
    publish_process_stats(1000, process_sample(1));
    // `debug.inspect` point-reads but cannot stream.
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getProcessStats", None);
    assert!(!was_error, "inspect get: {text}");
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/streamProcessStats", None);
    assert!(was_error, "inspect must not stream: {text}");
    assert!(text.contains("ScopeDenied"), "got: {text}");
    // `debug.trace` streams but cannot point-read.
    let (was_error, text) = call(trace_scopes(), "bitty.debug/streamProcessStats", None);
    assert!(!was_error, "trace stream: {text}");
    let (was_error, text) = call(trace_scopes(), "bitty.debug/getProcessStats", None);
    assert!(was_error, "trace must not point-read: {text}");
    assert!(text.contains("ScopeDenied"), "got: {text}");
    clear_profiling_for_tests();
}

#[test]
fn obs_profiling_unscoped_denied_with_zero_state() {
    let _guard = hold_obs_lock();
    clear_profiling_for_tests();
    publish_process_stats(1000, process_sample(1));
    for method in [
        "bitty.debug/getProcessStats",
        "bitty.debug/getFrameStats",
        "bitty.debug/streamProcessStats",
        "bitty.debug/streamFrameStats",
    ] {
        let (was_error, text) = call(ScopeSet::new(), method, None);
        assert!(was_error, "unscoped {method} must fail: {text}");
        assert!(text.contains("ScopeDenied"), "got: {text}");
        assert!(
            !text.contains("rssBytes") && !text.contains("samples"),
            "denial must carry zero partial state: {text}"
        );
    }
    // The denials created no new samples: the one seeded sample is still the
    // store head (seq 1), so no partial state crossed the denial boundary.
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getProcessStats", None);
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"sample\":\"latest\""), "got: {text}");
    assert_eq!(
        numeric_field(&text, "seq"),
        1,
        "denials added state: {text}"
    );
    clear_profiling_for_tests();
}
