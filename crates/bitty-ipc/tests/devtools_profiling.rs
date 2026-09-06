//! Socket + in-process profiling proof over the devtools surface (CTX-0189).
//!
//! Amendment A1 (proposed, bitty-docs CTX-0124) live-profiling scope:
//! `getProcessStats` / `getFrameStats` (`debug.inspect`) plus sampled
//! `streamProcessStats` / `streamFrameStats` (`debug.trace`) subscriptions,
//! reusing the Performance Budget RFC definitions (PB-2, PB-3, PB-4, PB-7).
//! Sampling-only posture (100 ms floor, cold-path publishes, tracing
//! deferred), zero-terminal-bytes records, explicit non-goals honored (no
//! system-wide profiling, no MCP streaming, no budget changes).
//!
//! Covered here:
//! - Method registration on `Dispatcher::with_defaults`.
//! - Scope matrix: unscoped denied for all four; getters need
//!   `debug.inspect`; streams need `debug.trace` (cross-scope denied).
//! - Empty store returns `"sample":"none"` (no fabricated zeros).
//! - Latest-wins coalescing on the getters; exact JSON contract for both.
//! - Bounded ring (32 records, drop-oldest, counted drops) with monotonic
//!   sequences across overflow.
//! - Cursor drains: `afterSeq` / `maxSamples` / `truncated` / 8 KiB budget.
//! - Interval floor (100 ms) enforced fail-closed; malformed params fail
//!   closed with `InvalidParams` and zero partial state.
//! - Backend label bound (256 chars, control bytes stripped) and
//!   `untrusted-observation` labeling on frame responses.
//! - Socket-level proof over a real Unix socket (CTX-0183 harness pattern).
//!
//! Headless and Unix-gated for the socket proofs; the in-process proofs run
//! anywhere. Profiling shares the process-global store, so every test holds
//! the file-local serial guard (CTX-0179 pattern; std-only, no extra
//! dev-dependency) and clears profiling before and after.

use std::sync::{Mutex, OnceLock};

use bitty_ipc::devtools::{
    Dispatcher, FrameStatsPublish, ProcessStatsPublish, ServeContext, ServerInfo,
    clear_profiling_for_tests, handle_envelope, publish_frame_stats, publish_process_stats,
};
use bitty_ipc::scope::{Scope, ScopeSet};

/// Serial guard for the process-global profiling store.
fn profiling_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn hold_profiling_lock() -> std::sync::MutexGuard<'static, ()> {
    profiling_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn test_server() -> ServerInfo {
    ServerInfo::new(
        "profiling-proof".to_string(),
        "/run/user/1000/bitty/profiling-proof.sock".to_string(),
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

fn context_with(granted: ScopeSet) -> ServeContext {
    ServeContext::with_granted_session(&test_server(), granted, "prof-sess")
}

fn envelope(id: u64, method: &str, params: Option<&str>) -> Vec<u8> {
    match params {
        Some(p) => {
            format!("{{\"id\":{id},\"method\":\"{method}\",\"version\":\"1.0\",\"params\":{p}}}")
        }
        None => format!("{{\"id\":{id},\"method\":\"{method}\",\"version\":\"1.0\"}}"),
    }
    .into_bytes()
}

fn call(granted: ScopeSet, method: &str, params: Option<&str>) -> (bool, String) {
    let dispatcher = Dispatcher::with_defaults();
    let ctx = context_with(granted);
    let outcome = handle_envelope(&envelope(1, method, params), &dispatcher, &ctx);
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

#[test]
fn profiling_methods_registered_on_defaults() {
    let dispatcher = Dispatcher::with_defaults();
    assert!(dispatcher.contains("bitty.debug/getProcessStats"));
    assert!(dispatcher.contains("bitty.debug/getFrameStats"));
    assert!(dispatcher.contains("bitty.debug/streamProcessStats"));
    assert!(dispatcher.contains("bitty.debug/streamFrameStats"));
}

#[test]
fn profiling_unscoped_calls_denied_with_zero_state() {
    let _guard = hold_profiling_lock();
    clear_profiling_for_tests();
    for method in [
        "bitty.debug/getProcessStats",
        "bitty.debug/getFrameStats",
        "bitty.debug/streamProcessStats",
        "bitty.debug/streamFrameStats",
    ] {
        let (was_error, text) = call(ScopeSet::new(), method, None);
        assert!(was_error, "unscoped {method} must fail: {text}");
        assert!(
            text.contains("ScopeDenied"),
            "unscoped {method} must be ScopeDenied: {text}"
        );
    }
    // Denials create no samples: an inspected empty store still reports none.
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getProcessStats", None);
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"sample\":\"none\""), "got: {text}");
    clear_profiling_for_tests();
}

#[test]
fn profiling_getters_require_inspect_streams_require_trace() {
    let _guard = hold_profiling_lock();
    clear_profiling_for_tests();
    publish_process_stats(1000, process_sample(1));
    publish_frame_stats(1000, frame_sample("wgpu-vulkan"));

    // Inspect-only: getters succeed, streams deny.
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getProcessStats", None);
    assert!(!was_error, "inspect getProcessStats: {text}");
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getFrameStats", None);
    assert!(!was_error, "inspect getFrameStats: {text}");
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/streamProcessStats", None);
    assert!(was_error, "inspect must not stream: {text}");
    assert!(text.contains("ScopeDenied"), "got: {text}");
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/streamFrameStats", None);
    assert!(was_error, "inspect must not stream: {text}");
    assert!(text.contains("ScopeDenied"), "got: {text}");

    // Trace-only: streams succeed, getters deny.
    let (was_error, text) = call(trace_scopes(), "bitty.debug/streamProcessStats", None);
    assert!(!was_error, "trace streamProcessStats: {text}");
    let (was_error, text) = call(trace_scopes(), "bitty.debug/streamFrameStats", None);
    assert!(!was_error, "trace streamFrameStats: {text}");
    let (was_error, text) = call(trace_scopes(), "bitty.debug/getProcessStats", None);
    assert!(was_error, "trace must not point-read: {text}");
    assert!(text.contains("ScopeDenied"), "got: {text}");
    let (was_error, text) = call(trace_scopes(), "bitty.debug/getFrameStats", None);
    assert!(was_error, "trace must not point-read: {text}");
    assert!(text.contains("ScopeDenied"), "got: {text}");

    // Control-only (automation lane) reaches neither profiling surface.
    let mut control = ScopeSet::new();
    control.insert(Scope::DebugControl);
    let (was_error, text) = call(control, "bitty.debug/getProcessStats", None);
    assert!(was_error, "control must not profile: {text}");
    assert!(text.contains("ScopeDenied"), "got: {text}");
    clear_profiling_for_tests();
}

#[test]
fn profiling_empty_store_reports_none_without_zeros() {
    let _guard = hold_profiling_lock();
    clear_profiling_for_tests();
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getProcessStats", None);
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"sample\":\"none\""), "got: {text}");
    assert!(
        !text.contains("rssBytes"),
        "empty store must not fabricate numbers: {text}"
    );
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getFrameStats", None);
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"sample\":\"none\""), "got: {text}");
    assert!(
        text.contains("\"trust\":\"untrusted-observation\""),
        "got: {text}"
    );
    assert!(
        !text.contains("frameP50Us"),
        "empty store must not fabricate numbers: {text}"
    );
    let (was_error, text) = call(trace_scopes(), "bitty.debug/streamProcessStats", None);
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"samples\":[]"), "got: {text}");
    assert!(text.contains("\"seq\":0"), "got: {text}");
    assert!(text.contains("\"dropped\":0"), "got: {text}");
    assert!(text.contains("\"truncated\":false"), "got: {text}");
    clear_profiling_for_tests();
}

#[test]
fn profiling_getters_return_latest_exact_contract() {
    let _guard = hold_profiling_lock();
    clear_profiling_for_tests();
    publish_process_stats(1000, process_sample(1_000));
    publish_process_stats(2000, process_sample(83_886_080));
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getProcessStats", None);
    assert!(!was_error, "unexpected error: {text}");
    let expected = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"version\":\"1.0\",\"snapshot\":\"process-stats\",\"sample\":\"latest\",\"seq\":2,\"nowMs\":2000,\"rssBytes\":83886080,\"cpuAvgPctX100\":100,\"cpuWindowMs\":600000,\"tasks\":7,\"timers\":3},\"version\":\"1.0\"}";
    assert_eq!(text, expected);

    publish_frame_stats(1000, frame_sample("old-backend"));
    publish_frame_stats(2000, frame_sample("wgpu-vulkan"));
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getFrameStats", None);
    assert!(!was_error, "unexpected error: {text}");
    let expected = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"version\":\"1.0\",\"snapshot\":\"frame-stats\",\"sample\":\"latest\",\"seq\":2,\"nowMs\":2000,\"frameP50Us\":3000,\"frameP99Us\":9000,\"presentedFps\":60,\"missedPresents\":2,\"gpuBytes\":12582912,\"backend\":\"wgpu-vulkan\",\"trust\":\"untrusted-observation\"},\"version\":\"1.0\"}";
    assert_eq!(text, expected);
    clear_profiling_for_tests();
}

#[test]
fn profiling_frame_stats_omits_gpu_when_unexposed() {
    let _guard = hold_profiling_lock();
    clear_profiling_for_tests();
    publish_frame_stats(
        1000,
        FrameStatsPublish {
            gpu_bytes: None,
            ..frame_sample("software")
        },
    );
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getFrameStats", None);
    assert!(!was_error, "unexpected error: {text}");
    assert!(
        !text.contains("gpuBytes"),
        "absent GPU memory must omit the field, not zero it: {text}"
    );
    assert!(text.contains("\"backend\":\"software\""), "got: {text}");
    clear_profiling_for_tests();
}

#[test]
fn profiling_ring_bounds_drop_oldest_with_counts() {
    let _guard = hold_profiling_lock();
    clear_profiling_for_tests();
    for i in 1..=40u64 {
        publish_process_stats(i * 100, process_sample(i));
    }
    // Full drain: 32 newest (seq 9..=40) with 8 counted historical drops.
    // `truncated` stays false: every retained record newer than the cursor
    // was returned (historical loss surfaces via `dropped` + head `seq`,
    // never via `truncated`, so no pointless re-poll loop triggers).
    let (was_error, text) = call(
        trace_scopes(),
        "bitty.debug/streamProcessStats",
        Some("{\"intervalMs\":100}"),
    );
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"seq\":40"), "got: {text}");
    assert!(text.contains("\"dropped\":8"), "got: {text}");
    assert!(text.contains("\"truncated\":false"), "got: {text}");
    assert!(
        text.contains("\"seq\":9,"),
        "oldest retained must be 9: {text}"
    );
    assert!(
        !text.contains("\"seq\":8,"),
        "seq 8 must have dropped: {text}"
    );
    // Cursor drain from the head: empty, not truncated.
    let (was_error, text) = call(
        trace_scopes(),
        "bitty.debug/streamProcessStats",
        Some("{\"intervalMs\":100,\"afterSeq\":40}"),
    );
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"samples\":[]"), "got: {text}");
    assert!(text.contains("\"truncated\":false"), "got: {text}");
    // Partial cursor with a record cap.
    let (was_error, text) = call(
        trace_scopes(),
        "bitty.debug/streamProcessStats",
        Some("{\"intervalMs\":100,\"afterSeq\":38,\"maxSamples\":1}"),
    );
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"seq\":39,"), "got: {text}");
    assert_eq!(
        text.matches("\"nowMs\":").count(),
        1,
        "cap must hold exactly one sample: {text}"
    );
    assert!(text.contains("\"truncated\":true"), "got: {text}");
    clear_profiling_for_tests();
}

#[test]
fn profiling_drain_byte_budget_truncates_long_labels() {
    let _guard = hold_profiling_lock();
    clear_profiling_for_tests();
    let long_backend: String = "b".repeat(256);
    for i in 1..=32u64 {
        publish_frame_stats(i * 100, frame_sample(&long_backend));
    }
    // 32 samples x ~400 bytes each exceeds the 8 KiB aggregate: the drain
    // must stop early with truncated=true rather than emit an oversized
    // batch.
    let (was_error, text) = call(
        trace_scopes(),
        "bitty.debug/streamFrameStats",
        Some("{\"intervalMs\":100,\"maxSamples\":32}"),
    );
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"truncated\":true"), "got: {text}");
    let body_len = text.len();
    assert!(
        body_len <= 8 * 1024 + 512,
        "drain exceeded byte budget: {body_len} bytes"
    );
    assert!(text.contains("\"seq\":32"), "got: {text}");
    assert!(text.contains("\"dropped\":0"), "got: {text}");
    clear_profiling_for_tests();
}

#[test]
fn profiling_interval_floor_enforced_fail_closed() {
    let _guard = hold_profiling_lock();
    clear_profiling_for_tests();
    publish_process_stats(100, process_sample(1));
    // Below the 100 ms floor: rejected with zero samples served.
    for params in [
        "{\"intervalMs\":99}",
        "{\"intervalMs\":50}",
        "{\"intervalMs\":0}",
        "{\"intervalMs\":\"fast\"}",
        "{\"intervalMs\":-100}",
    ] {
        let (was_error, text) = call(
            trace_scopes(),
            "bitty.debug/streamProcessStats",
            Some(params),
        );
        assert!(was_error, "floor must reject {params}: {text}");
        assert!(text.contains("InvalidParams"), "got: {text}");
        assert!(
            !text.contains("\"samples\""),
            "rejection must serve zero samples: {text}"
        );
    }
    // Floor and ceiling edges accepted.
    let (was_error, text) = call(
        trace_scopes(),
        "bitty.debug/streamProcessStats",
        Some("{\"intervalMs\":100}"),
    );
    assert!(!was_error, "floor edge: {text}");
    assert!(text.contains("\"intervalMs\":100"), "got: {text}");
    let (was_error, text) = call(
        trace_scopes(),
        "bitty.debug/streamProcessStats",
        Some("{\"intervalMs\":600000}"),
    );
    assert!(!was_error, "ceiling edge: {text}");
    let (was_error, text) = call(
        trace_scopes(),
        "bitty.debug/streamProcessStats",
        Some("{\"intervalMs\":600001}"),
    );
    assert!(was_error, "above ceiling must reject: {text}");
    assert!(text.contains("InvalidParams"), "got: {text}");
    // Absent cadence defaults to 1000 ms.
    let (was_error, text) = call(trace_scopes(), "bitty.debug/streamFrameStats", None);
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"intervalMs\":1000"), "got: {text}");
    clear_profiling_for_tests();
}

#[test]
fn profiling_malformed_cursor_and_cap_fail_closed() {
    let _guard = hold_profiling_lock();
    clear_profiling_for_tests();
    publish_process_stats(100, process_sample(1));
    for params in [
        "{\"afterSeq\":\"zero\"}",
        "{\"afterSeq\":-1}",
        "{\"maxSamples\":0}",
        "{\"maxSamples\":33}",
        "{\"maxSamples\":\"all\"}",
    ] {
        let (was_error, text) = call(
            trace_scopes(),
            "bitty.debug/streamProcessStats",
            Some(params),
        );
        assert!(was_error, "{params} must reject: {text}");
        assert!(text.contains("InvalidParams"), "got: {text}");
    }
    // afterSeq cursor is honored exactly.
    let (was_error, text) = call(
        trace_scopes(),
        "bitty.debug/streamProcessStats",
        Some("{\"afterSeq\":1}"),
    );
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"samples\":[]"), "got: {text}");
    clear_profiling_for_tests();
}

#[test]
fn profiling_backend_label_bounded_and_untrusted() {
    let _guard = hold_profiling_lock();
    clear_profiling_for_tests();
    // 300 chars truncate to the 256 candidate bound; control bytes strip.
    let hostile = format!("ab\x00cd\nef\x7Fgh{}", "x".repeat(300));
    publish_frame_stats(100, frame_sample(&hostile));
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getFrameStats", None);
    assert!(!was_error, "unexpected error: {text}");
    let backend_json = "\"backend\":\"";
    let start = text.find(backend_json).unwrap() + backend_json.len();
    let end = text[start..].find('"').unwrap() + start;
    let served = &text[start..end];
    assert!(
        served.chars().count() <= 256,
        "backend exceeded 256 chars: {} chars",
        served.chars().count()
    );
    assert!(
        !served.contains('\u{0}') && !served.contains('\n'),
        "control bytes leaked: {served:?}"
    );
    assert!(served.starts_with("abcdefgh"), "got: {served:?}");
    assert!(
        text.contains("\"trust\":\"untrusted-observation\""),
        "got: {text}"
    );
    // Zero terminal bytes: the only string field is the backend label;
    // numeric contract carries no PTY, clipboard, or environment content.
    for banned in ["clipboard", "environment", "ptyBytes", "scrollback"] {
        assert!(
            !text.contains(banned),
            "terminal-byte key leaked: {banned} in {text}"
        );
    }
    clear_profiling_for_tests();
}

#[test]
fn profiling_window_clamped_to_bounded_range() {
    let _guard = hold_profiling_lock();
    clear_profiling_for_tests();
    publish_process_stats(
        100,
        ProcessStatsPublish {
            window_ms: 0,
            ..process_sample(1)
        },
    );
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getProcessStats", None);
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"cpuWindowMs\":1"), "got: {text}");
    clear_profiling_for_tests();
    publish_process_stats(
        100,
        ProcessStatsPublish {
            window_ms: u64::MAX,
            ..process_sample(1)
        },
    );
    let (was_error, text) = call(inspect_scopes(), "bitty.debug/getProcessStats", None);
    assert!(!was_error, "unexpected error: {text}");
    assert!(text.contains("\"cpuWindowMs\":600000"), "got: {text}");
    clear_profiling_for_tests();
}

// ── socket-level proof (CTX-0183 harness pattern, Unix-only) ────────────────

#[cfg(unix)]
mod socket {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use bitty_ipc::devtools::{
        Dispatcher, ServeContext, ServerInfo, clear_profiling_for_tests, prepare_socket_dir,
        publish_frame_stats, publish_process_stats, serve_connection, transport_attested_peer,
    };
    use bitty_ipc::frame::{MAX_FRAME_BYTES, encode_frame};
    use bitty_ipc::limits::RateLimiter;
    use bitty_ipc::scope::ScopeSet;

    use super::{frame_sample, hold_profiling_lock, process_sample};

    fn temp_socket_path(tag: &str) -> String {
        let pid = std::process::id();
        let path = format!("/tmp/btp{pid}{tag}/s.sock");
        assert!(
            path.len() < 100,
            "socket path must fit macOS SUN_LEN: {path} ({} bytes)",
            path.len()
        );
        path
    }

    fn read_framed(stream: &mut UnixStream) -> String {
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).unwrap();
        let len = u32::from_be_bytes(header) as usize;
        assert!(len <= MAX_FRAME_BYTES, "response exceeds frame bound");
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).unwrap();
        String::from_utf8(body).unwrap()
    }

    struct ProfilingClient {
        stream: UnixStream,
        next_id: u64,
    }

    impl ProfilingClient {
        fn connect(socket_path: &str) -> Self {
            let stream = UnixStream::connect(socket_path).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            Self { stream, next_id: 1 }
        }

        fn call(&mut self, method: &str, params: Option<&str>) -> String {
            let id = self.next_id;
            self.next_id += 1;
            let envelope = match params {
                Some(p) => format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"params\":{p},\"version\":\"1.0\"}}"
                ),
                None => format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"version\":\"1.0\"}}"
                ),
            };
            let wire = encode_frame(envelope.as_bytes()).unwrap();
            self.stream.write_all(&wire).unwrap();
            self.stream.flush().unwrap();
            let response = read_framed(&mut self.stream);
            assert!(
                response.contains(&format!("\"id\":{id}")),
                "response lost correlation id {id}: {response}"
            );
            response
        }
    }

    #[cfg(unix)]
    fn unit_owner_uid(path: &str) -> u32 {
        use std::os::unix::fs::MetadataExt;

        std::fs::metadata(path).map(|m| m.uid()).unwrap_or(0)
    }

    fn spawn_profiling_server(
        socket_path: String,
        granted: ScopeSet,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let listener = UnixListener::bind(&socket_path).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let verified = transport_attested_peer(unit_owner_uid(&socket_path));
            let dispatcher = Dispatcher::with_defaults();
            let server =
                ServerInfo::new("profiling-proof".to_string(), socket_path.clone(), 80, 24);
            let context = ServeContext::with_granted_session(&server, granted, "prof-sock");
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
            .unwrap();
            assert!(stats.requests >= 1, "expected test requests");
            assert_eq!(stats.responses, stats.requests);
        })
    }

    #[test]
    fn profiling_process_stats_round_trip_over_socket() {
        let _guard = hold_profiling_lock();
        clear_profiling_for_tests();
        publish_process_stats(7000, process_sample(83_886_080));

        let socket_path = temp_socket_path("ps");
        prepare_socket_dir(&socket_path).unwrap();
        let mut granted = ScopeSet::new();
        granted.insert(bitty_ipc::scope::Scope::DebugInspect);
        let server = spawn_profiling_server(socket_path.clone(), granted);
        std::thread::sleep(Duration::from_millis(100));
        let mut client = ProfilingClient::connect(&socket_path);

        let stats = client.call("bitty.debug/getProcessStats", None);
        assert!(
            stats.contains("\"snapshot\":\"process-stats\""),
            "got: {stats}"
        );
        assert!(stats.contains("\"rssBytes\":83886080"), "got: {stats}");
        assert!(stats.contains("\"cpuAvgPctX100\":100"), "got: {stats}");

        drop(client);
        server.join().unwrap();
        clear_profiling_for_tests();
        std::fs::remove_file(&socket_path).ok();
    }

    #[test]
    fn profiling_frame_stream_round_trip_over_socket() {
        let _guard = hold_profiling_lock();
        clear_profiling_for_tests();
        publish_frame_stats(8000, frame_sample("wgpu-vulkan"));

        let socket_path = temp_socket_path("fs");
        prepare_socket_dir(&socket_path).unwrap();
        let mut granted = ScopeSet::new();
        granted.insert(bitty_ipc::scope::Scope::DebugInspect);
        granted.insert(bitty_ipc::scope::Scope::DebugTrace);
        let server = spawn_profiling_server(socket_path.clone(), granted);
        std::thread::sleep(Duration::from_millis(100));
        let mut client = ProfilingClient::connect(&socket_path);

        let drain = client.call("bitty.debug/streamFrameStats", Some("{\"intervalMs\":100}"));
        assert!(drain.contains("\"family\":\"frame-stats\""), "got: {drain}");
        assert!(
            drain.contains("\"backend\":\"wgpu-vulkan\""),
            "got: {drain}"
        );
        assert!(
            drain.contains("\"trust\":\"untrusted-observation\""),
            "got: {drain}"
        );
        // Cursor honors the served head: nothing newer drains twice.
        let again = client.call(
            "bitty.debug/streamFrameStats",
            Some("{\"intervalMs\":100,\"afterSeq\":1}"),
        );
        assert!(again.contains("\"samples\":[]"), "got: {again}");

        drop(client);
        server.join().unwrap();
        clear_profiling_for_tests();
        std::fs::remove_file(&socket_path).ok();
    }

    #[test]
    fn profiling_unscoped_denied_machine_checkably_over_socket() {
        let _guard = hold_profiling_lock();
        clear_profiling_for_tests();
        publish_process_stats(9000, process_sample(5));

        let socket_path = temp_socket_path("pd");
        prepare_socket_dir(&socket_path).unwrap();
        let server = spawn_profiling_server(socket_path.clone(), ScopeSet::new());
        std::thread::sleep(Duration::from_millis(100));
        let mut client = ProfilingClient::connect(&socket_path);

        for method in [
            "bitty.debug/getProcessStats",
            "bitty.debug/getFrameStats",
            "bitty.debug/streamProcessStats",
            "bitty.debug/streamFrameStats",
        ] {
            let denied = client.call(method, None);
            assert!(
                denied.contains("ScopeDenied"),
                "expected ScopeDenied for {method}, got: {denied}"
            );
        }

        drop(client);
        server.join().unwrap();
        clear_profiling_for_tests();
        std::fs::remove_file(&socket_path).ok();
    }
}
