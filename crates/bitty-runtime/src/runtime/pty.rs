//! `Runtime` — PTY ownership, polling, replies, and OSC52/base64 helpers.
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
use super::plugin::cold_to_observation;
use super::*;

/// Blocking forwarder: sole consumer of `reader`, pushing into `tx` and
/// waking once per batch plus once on EOF (CTX-0476 waker merge).
///
/// - Quiet child: parked in `recv`, zero wakeups, zero CPU.
/// - Backpressure: `send` blocks when `tx` is full, which fills the original
///   pump channel, which fills the kernel PTY buffer, which blocks the child.
/// - Fail-closed: a dropped consumer breaks `send` and ends the thread with
///   no loss beyond already-queued chunks and no unbounded growth.
/// - Waker merge: after the blocking `recv` yields the first chunk, up to
///   `PTY_FORWARD_CAPACITY_CHUNKS - 1` immediately-available chunks are
///   batched via `try_recv` and delivered with a single wakeup, so a burst
///   of N chunks costs one event-loop wakeup instead of N (waker storm).
pub(super) fn pty_forward_loop(
    reader: PtyReader,
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    waker: PtyWaker,
) {
    loop {
        let first = match reader.recv() {
            Ok(Some(chunk)) => {
                debug_assert!(chunk.len() <= bitty_pty::READ_CHUNK_SIZE);
                chunk
            }
            Ok(None) | Err(_) => {
                // Clean EOF, or a pump I/O failure after every queued chunk
                // was delivered: either way wake once so the consumer drains
                // final chunks promptly instead of waiting for an incidental
                // wakeup. The pump outcome is informational here and is
                // surfaced by `join` below.
                (waker)();
                break;
            }
        };
        // Batch immediately-available follow-ups without blocking: a burst
        // already queued in the pump channel merges into one wakeup.
        let mut batch = Vec::with_capacity(PTY_FORWARD_CAPACITY_CHUNKS);
        batch.push(first);
        while batch.len() < PTY_FORWARD_CAPACITY_CHUNKS {
            match reader.try_recv() {
                bitty_pty::PtyRecv::Chunk(chunk) => {
                    debug_assert!(chunk.len() <= bitty_pty::READ_CHUNK_SIZE);
                    batch.push(chunk);
                }
                // Nothing queued right now, or the stream is over
                // (EOF/failure); the blocking `recv` above reports the
                // terminal outcome on the next iteration.
                bitty_pty::PtyRecv::Empty
                | bitty_pty::PtyRecv::Eof
                | bitty_pty::PtyRecv::Error(_) => break,
            }
        }
        let mut sent_any = false;
        let mut broken = false;
        for chunk in batch {
            if tx.send(chunk).is_err() {
                broken = true;
                break;
            }
            sent_any = true;
        }
        // One wakeup per batch, and only when at least one chunk was
        // forwarded: a dropped consumer exits without a spurious wakeup,
        // matching the per-chunk send-then-wake ordering it replaces.
        if sent_any {
            (waker)();
        }
        if broken {
            break;
        }
    }
    // Reap the pump thread; outcome is informational (EOF vs I/O error).
    let _ = reader.join();
}

/// Handle to a spawned wakeup forwarder plus its consumer channel.
pub(super) struct ForwarderParts {
    pub(super) rx: std::sync::mpsc::Receiver<Vec<u8>>,
    pub(super) handle: std::thread::JoinHandle<()>,
}

/// A refused forwarder spawn: the spawn error plus the reader recovered from
/// the shared slot (`None` only when an injected test spawner already ran the
/// body and consumed it).
pub(super) struct SpawnFailure {
    pub(super) reader: Option<PtyReader>,
    pub(super) error: std::io::Error,
}

/// Take the parked payload, tolerating a poisoned lock (fail-closed, no panic).
fn lock_take<T>(slot: &std::sync::Mutex<Option<T>>) -> Option<T> {
    match slot.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    }
}

/// Run `body` on a newly spawned thread, parking `payload` so a refused spawn
/// hands it back instead of dropping it (CTX-0473).
///
/// `spawn` performs the actual thread creation (production:
/// [`std::thread::Builder`]); injecting it lets tests exercise the refusal
/// path deterministically. On `Err` the payload was never lost: it is returned
/// so the caller can keep a degraded path alive rather than panic or go dark.
pub(super) fn spawn_recovering<P: Send + 'static>(
    payload: P,
    body: impl FnOnce(P) + Send + 'static,
    spawn: impl FnOnce(
        Box<dyn FnOnce() + Send + 'static>,
    ) -> std::io::Result<std::thread::JoinHandle<()>>,
) -> Result<std::thread::JoinHandle<()>, (Option<P>, std::io::Error)> {
    let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(payload)));
    let thread_slot = std::sync::Arc::clone(&slot);
    let boxed: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
        if let Some(payload) = lock_take(&thread_slot) {
            body(payload);
        }
    });
    match spawn(boxed) {
        Ok(handle) => Ok(handle),
        Err(error) => Err((lock_take(&slot), error)),
    }
}

/// Spawn the PTY wakeup forwarder for `reader`, fail-closed (CTX-0473).
///
/// On success the forwarder owns `reader`; on spawn refusal the reader comes
/// back in [`SpawnFailure`] so the caller can restore the direct pump path.
pub(super) fn spawn_forwarder(
    reader: PtyReader,
    waker: PtyWaker,
    spawn: impl FnOnce(
        Box<dyn FnOnce() + Send + 'static>,
    ) -> std::io::Result<std::thread::JoinHandle<()>>,
) -> Result<ForwarderParts, SpawnFailure> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(PTY_FORWARD_CAPACITY_CHUNKS);
    spawn_recovering(
        reader,
        move |reader| pty_forward_loop(reader, tx, waker),
        spawn,
    )
    .map(|handle| ForwarderParts { rx, handle })
    .map_err(|(reader, error)| SpawnFailure { reader, error })
}

/// [`spawn_forwarder`] using the production named-thread builder.
pub(super) fn spawn_forwarder_default(
    reader: PtyReader,
    waker: PtyWaker,
) -> Result<ForwarderParts, SpawnFailure> {
    spawn_forwarder(reader, waker, |body| {
        std::thread::Builder::new()
            .name("bitty-pty-wakeup".to_owned())
            .spawn(body)
    })
}

/// Best-effort bounded write of `chunks` to `writer` (CTX-0473).
///
/// Writes in order and stops at the first failure (fail-closed). Returns
/// `(bytes_written, bytes_dropped)`; a chunk whose `write_all` failed is
/// counted whole as dropped, since the partial count on error is unspecified.
pub(super) fn write_chunks<W: std::io::Write>(
    writer: &mut W,
    chunks: &[Box<[u8]>],
) -> (usize, usize) {
    let mut written = 0usize;
    let mut dropped = 0usize;
    for (idx, chunk) in chunks.iter().enumerate() {
        if writer.write_all(chunk).is_ok() {
            written += chunk.len();
        } else {
            dropped += chunk.len();
            for rest in &chunks[idx + 1..] {
                dropped += rest.len();
            }
            break;
        }
    }
    (written, dropped)
}

/// OSC 52 read-reply framing overhead: `ESC ] 52 ; c ;` (7 bytes) plus the
/// `BEL` terminator (1 byte).
const OSC52_REPLY_FRAMING_BYTES: usize = 8;

/// Maximum raw clipboard bytes encoded into one OSC 52 read reply, so the
/// framed reply always fits the 4 KiB terminal-state reply cap instead of
/// being dropped whole by it (which would hang the querier again).
const OSC52_READ_MAX_RAW_BYTES: usize =
    (bitty_term_state::REPLY_CAP_BYTES - OSC52_REPLY_FRAMING_BYTES) / 4 * 3;

/// Minimal standard-base64 encoder (RFC 4648 §4, `+/` with `=` padding).
///
/// Kept dependency-free on purpose: the only consumer is the OSC 52 read
/// reply below, and a new supply-chain dependency for ~20 lines is not
/// justified. Time O(n), space O(n) in the input length.
fn base64_encode_standard(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = if chunk.len() > 1 {
            u32::from(chunk[1])
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            u32::from(chunk[2])
        } else {
            0
        };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Minimal standard-base64 decoder (RFC 4648 §4, `+/` with `=` padding),
/// mirroring [`base64_encode_standard`] without a new dependency.
///
/// Accepts padded and unpadded input; rejects non-alphabet bytes, misplaced
/// padding, and lengths congruent to 1 mod 4. Empty input decodes to empty.
/// Trailing bits of a short final quantum are masked (lenient), but garbage
/// alphabet or bad padding is always an error so callers can fail closed.
/// Time O(n), space O(n) in the input length.
fn base64_decode_standard(input: &[u8]) -> Result<Vec<u8>, &'static str> {
    fn sextet(byte: u8) -> Result<u32, &'static str> {
        match byte {
            b'A'..=b'Z' => Ok(u32::from(byte - b'A')),
            b'a'..=b'z' => Ok(u32::from(byte - b'a' + 26)),
            b'0'..=b'9' => Ok(u32::from(byte - b'0' + 52)),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err("invalid base64 character"),
        }
    }
    if input.is_empty() {
        return Ok(Vec::new());
    }
    if input.len() % 4 == 1 {
        return Err("invalid base64 length");
    }
    let mut pad = 0_usize;
    for &byte in input.iter().rev() {
        if byte == b'=' {
            pad += 1;
        } else {
            break;
        }
    }
    if pad > 2 {
        return Err("invalid base64 padding");
    }
    let body_len = input.len() - pad;
    if input[..body_len].contains(&b'=') {
        return Err("misplaced base64 padding");
    }
    if pad == 1 && body_len % 4 != 3 {
        return Err("invalid base64 padding");
    }
    if pad == 2 && body_len % 4 != 2 {
        return Err("invalid base64 padding");
    }
    let body = &input[..body_len];
    let (full, tail) = body.split_at(body.len() / 4 * 4);
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    for chunk in full.chunks_exact(4) {
        let triple = (sextet(chunk[0])? << 18)
            | (sextet(chunk[1])? << 12)
            | (sextet(chunk[2])? << 6)
            | sextet(chunk[3])?;
        out.push((triple >> 16) as u8);
        out.push((triple >> 8) as u8);
        out.push(triple as u8);
    }
    match tail.len() {
        0 => {}
        2 => {
            let bits = (sextet(tail[0])? << 18) | (sextet(tail[1])? << 12);
            out.push((bits >> 16) as u8);
        }
        3 => {
            let bits =
                (sextet(tail[0])? << 18) | (sextet(tail[1])? << 12) | (sextet(tail[2])? << 6);
            out.push((bits >> 16) as u8);
            out.push((bits >> 8) as u8);
        }
        _ => return Err("invalid base64 length"),
    }
    Ok(out)
}

/// Builds an OSC 52 clipboard-read reply for `text` (`ESC ] 52 ; c ; <base64> BEL`).
///
/// The payload is truncated on a UTF-8 boundary to
/// [`OSC52_READ_MAX_RAW_BYTES`] so the framed reply never exceeds the reply
/// cap. Mirrors Ghostty's `clipboard_response` shape: selection `c`, BEL
/// terminator. Time O(n), space O(n) in the (bounded) clipboard length.
fn osc52_read_reply(text: &str) -> Vec<u8> {
    let mut end = text.len().min(OSC52_READ_MAX_RAW_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    // `end` is a char boundary, so slicing the UTF-8 bytes is exact.
    let encoded = base64_encode_standard(&text.as_bytes()[..end]);
    let mut reply = Vec::with_capacity(OSC52_REPLY_FRAMING_BYTES + encoded.len());
    reply.extend_from_slice(b"\x1b]52;c;");
    reply.extend_from_slice(encoded.as_bytes());
    reply.push(0x07);
    reply
}

impl Runtime {
    /// Installs the cross-thread PTY readability callback and promotes the
    /// direct reader into the wakeup forwarder pump (pane sessions promote
    /// alongside; see `set_pty_waker` body, CTX-0230).
    ///
    /// The forwarder is the sole consumer of the bounded pump channel from
    /// this point: it blocks in `recv` (zero wakeups when quiet), forwards
    /// each batch into a second bounded channel
    /// ([`PTY_FORWARD_CAPACITY_CHUNKS`]), and invokes `waker` once per batch
    /// plus once on EOF (CTX-0476 waker merge). [`poll_pty`] drains the
    /// forwarding channel, so the existing bounded-drain contract is
    /// preserved end to end.
    ///
    /// Idempotent: replacing the waker re-promotes only when a direct reader
    /// is still present; an already-promoted pump keeps its original waker
    /// clone (all clones wake the same loop).
    pub fn set_pty_waker(&mut self, waker: PtyWaker) {
        self.pty_waker = Some(waker);
        self.promote_pty_reader_to_forwarder();
        // CTX-0230: pane readers promote too. A fresh split shell emits its
        // `ESC[c` (Primary DA) immediately; without a per-pane forwarder
        // nothing wakes a `ControlFlow::Wait` loop for pane-only output, so
        // the query sat unanswered until an incidental wakeup (~10 s stall).
        // Each promoted pane invokes the same waker, and `poll_pty` already
        // drains every pane session on each call.
        let ids: Vec<ViewId> = self.pane_sessions.keys().copied().collect();
        for id in ids {
            self.promote_pane_reader_to_forwarder(id);
        }
    }

    /// Whether a readability waker is installed.
    #[must_use]
    pub fn has_pty_waker(&self) -> bool {
        self.pty_waker.is_some()
    }

    /// Whether the wakeup forwarder pump is active (implies [`has_pty_reader`]).
    #[must_use]
    pub fn has_pty_forwarder(&self) -> bool {
        self.pty_forward_rx.is_some()
    }

    /// Moves the direct [`PtyReader`] into the forwarder thread when both a
    /// reader and a waker are present. No-op otherwise.
    pub(super) fn promote_pty_reader_to_forwarder(&mut self) {
        if self.pty_forward_rx.is_some() {
            return;
        }
        if self.pty_reader.is_none() || self.pty_waker.is_none() {
            return;
        }
        let Some(reader) = self.pty_reader.take() else {
            return;
        };
        let Some(waker) = self.pty_waker.clone() else {
            self.pty_reader = Some(reader);
            return;
        };
        match spawn_forwarder_default(reader, waker) {
            Ok(parts) => {
                self.pty_forward_rx = Some(parts.rx);
                self.pty_forward_handle = Some(parts.handle);
            }
            Err(failure) => {
                // Fail-closed (CTX-0473): restore the reader so the direct
                // pump path keeps the child's output flowing; never panic.
                if let Some(reader) = failure.reader {
                    self.pty_reader = Some(reader);
                }
                self.forwarder_spawn_failures = self.forwarder_spawn_failures.wrapping_add(1);
                if let Some(suppressed) = self.spawn_log.admit_now() {
                    eprintln!(
                        "bitty: PTY wakeup forwarder spawn failed ({}): using direct pump{}",
                        failure.error,
                        log_throttle::suppressed_suffix(suppressed)
                    );
                }
            }
        }
    }

    /// Whether a PTY child is currently owned.
    #[must_use]
    pub fn has_pty(&self) -> bool {
        self.pty.is_some()
    }

    /// Whether a PTY reader channel is currently owned (implies [`has_pty`]).
    ///
    /// True while either the direct pump reader or the wakeup-forwarder
    /// channel is owned.
    #[must_use]
    pub fn has_pty_reader(&self) -> bool {
        self.pty_reader.is_some() || self.pty_forward_rx.is_some()
    }

    /// Process id of the child, when available.
    #[must_use]
    pub fn pty_pid(&self) -> Option<u32> {
        self.pty.as_ref().and_then(|p| p.pid())
    }

    /// Current PTY size as known by the kernel, if a PTY is owned.
    pub fn pty_size(&self) -> Option<(u16, u16)> {
        self.pty.as_ref().and_then(|p| p.size().ok())
    }

    /// Takes exclusive ownership of the bounded PTY output reader, if present.
    ///
    /// Only available before [`set_pty_waker`](Self::set_pty_waker) promotes
    /// the reader into the wakeup forwarder: afterwards the forwarder thread
    /// owns the reader and this returns `None` (drain via [`poll_pty`]
    /// instead). When no forwarder is active the caller becomes responsible
    /// for draining the channel without blocking the runtime thread and for
    /// joining the pump on EOF. After this call [`poll_pty`] will return `0`
    /// because the channel no longer belongs to the runtime; most embedders
    /// should prefer [`poll_pty`] instead.
    pub fn take_pty_reader(&mut self) -> Option<PtyReader> {
        if self.pty_forward_rx.is_some() {
            return None;
        }
        self.pty_reader.take()
    }

    /// Non-blocking drain of the bounded PTY output channel into
    /// [`handle_pty_bytes`].
    ///
    /// Drains the wakeup-forwarder channel when [`set_pty_waker`](Self::set_pty_waker)
    /// promoted the pump, otherwise the direct pump channel. Either way the
    /// bound holds (`CHANNEL_CAPACITY_CHUNKS` × `READ_CHUNK_SIZE` = 128 KiB
    /// per stage, 256 KiB worst-case total with the forwarder active).
    ///
    /// When a consumer stalls, the bounded channel(s) fill, the pump blocks,
    /// the kernel PTY buffer fills, and the child's writes block —
    /// end-to-end backpressure with zero data loss and zero unbounded memory
    /// growth. This method is the consumer side: it drains immediately
    /// available chunks without blocking, feeding each through the VT parser
    /// and terminal state, stopping at the first of the CTX-0476 budgets
    /// (`POLL_PTY_MAX_CHUNKS` / `POLL_PTY_MAX_BYTES` / `POLL_PTY_TIME_BUDGET`)
    /// so a hostile flood can delay but never stall the render thread; the
    /// remainder stays queued for the next poll.
    ///
    /// Returns the number of chunks drained. `0` means either no PTY, no data
    /// available yet, or EOF has been reached and the queue drained (the
    /// budgets stop a *busy* poll early, never before the first available
    /// chunk). Headless
    /// tests that never called [`spawn_shell`] get `0` without error, so the
    /// same binary works headlessly (synthetic `handle_pty_bytes`) and with a
    /// real PTY (live `poll_pty`).
    pub fn poll_pty(&mut self) -> usize {
        // Collect without holding an immutable borrow across the mutable
        // `handle_pty_bytes` call (borrow checker). Bounded by chunk count,
        // byte total, and wall time (CTX-0476); at least one chunk drains
        // when data is available so a max-size chunk always makes progress.
        let start = std::time::Instant::now();
        let mut drained_bytes = 0usize;
        let chunks: Vec<Vec<u8>> = {
            if let Some(rx) = self.pty_forward_rx.as_ref() {
                let mut out = Vec::new();
                while out.len() < POLL_PTY_MAX_CHUNKS && drained_bytes < POLL_PTY_MAX_BYTES {
                    if !out.is_empty() && start.elapsed() >= POLL_PTY_TIME_BUDGET {
                        break;
                    }
                    match rx.try_recv() {
                        Ok(chunk) => {
                            debug_assert!(chunk.len() <= bitty_pty::READ_CHUNK_SIZE);
                            drained_bytes = drained_bytes.saturating_add(chunk.len());
                            out.push(chunk);
                        }
                        Err(_) => break,
                    }
                }
                out
            } else {
                let Some(reader) = self.pty_reader.as_ref() else {
                    // No primary reader: pane sessions (if any) still pump.
                    return self.pump_pane_sessions();
                };
                let mut out = Vec::new();
                while out.len() < POLL_PTY_MAX_CHUNKS && drained_bytes < POLL_PTY_MAX_BYTES {
                    if !out.is_empty() && start.elapsed() >= POLL_PTY_TIME_BUDGET {
                        break;
                    }
                    match reader.try_recv() {
                        bitty_pty::PtyRecv::Chunk(chunk) => {
                            debug_assert!(chunk.len() <= bitty_pty::READ_CHUNK_SIZE);
                            drained_bytes = drained_bytes.saturating_add(chunk.len());
                            out.push(chunk);
                        }
                        bitty_pty::PtyRecv::Empty
                        | bitty_pty::PtyRecv::Eof
                        | bitty_pty::PtyRecv::Error(_) => break,
                    }
                }
                out
            }
        };
        let drained = chunks.len();
        for chunk in chunks {
            self.handle_pty_bytes(&chunk);
        }
        // Bounded PTY reply loop: parse->state->replies->writer (4 KiB cap, fail-closed)
        // Headless (no writer) keeps replies queued for `take_replies` observation.
        let _ = self.write_replies();
        // CTX-0176: pump every pane session on the same bounded path.
        drained + self.pump_pane_sessions()
    }

    /// Blocking drain with a timeout, returning the number of chunks drained.
    ///
    /// Blocks at most `timeout` for the first chunk; once data is flowing it
    /// drains all immediately available chunks without further blocking. Useful
    /// for tests that need to wait for a shell echo. Returns `0` on timeout
    /// or EOF.
    pub fn poll_pty_timeout(&mut self, timeout: std::time::Duration) -> usize {
        let first: Option<Vec<u8>> = {
            if let Some(rx) = self.pty_forward_rx.as_ref() {
                rx.recv_timeout(timeout).ok()
            } else {
                let Some(reader) = self.pty_reader.as_ref() else {
                    // CTX-0230: no primary reader must not starve panes.
                    // (`poll_pty` already pumps panes in this case.)
                    return self.pump_pane_sessions();
                };
                match reader.recv_timeout(timeout) {
                    Ok(Some(chunk)) => Some(chunk),
                    Ok(None) | Err(_) => None,
                }
            }
        };
        match first {
            Some(chunk) => {
                self.handle_pty_bytes(&chunk);
                1 + self.poll_pty()
            }
            // CTX-0230: a quiet primary must not starve panes. Drain every
            // pane session before reporting idle so split-shell queries
            // (e.g. fish `ESC[c` at startup) are answered on this path too.
            None => self.pump_pane_sessions(),
        }
    }

    /// Feeds raw PTY bytes through the parser into terminal state, enqueuing
    /// bounded cold-path observations derived from the actions.
    ///
    /// The byte stream may be split arbitrarily; splitting the same bytes
    /// differently yields the same action sequence (deterministic replay
    /// contract). Malformed or hostile sequences are bounded and never panic.
    ///
    /// Bridging (ADR-0003 rule 4): every [`ColdEvent`] that has a direct
    /// [`HostObservation`] mapping is also pushed into the bounded
    /// [`PluginHost`] side queue without blocking the hot path. When the side
    /// queue is full the oldest observation is dropped and
    /// [`Self::plugin_side_dropped`] increments (counted for `bitty plugin doctor`).
    pub fn handle_pty_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.handle_pty_bytes_inner(bytes);
        // CTX-0532: mode changes may have landed on the focused pane's own
        // register, so re-attribute the Kitty/mouse-capture caches once per
        // drained chunk. Done at the outer boundary (not inside the loop) so
        // the pane-swap path in `handle_pane_bytes` — which temporarily moves
        // a pane's grid into `self.state` — can skip the intermediate sync
        // and re-attribute after swapping back.
        self.sync_mode_caches_to_focus();
    }

    /// Parsing/state core of [`Self::handle_pty_bytes`] (CTX-0532 split).
    ///
    /// No cache re-attribution here: callers own it at their boundary so the
    /// pane-swap pair never caches the temporarily swapped-in grid.
    pub(super) fn handle_pty_bytes_inner(&mut self, bytes: &[u8]) {
        // CTX-0146: pre-scan overlap ++ new bytes for parameterized queries
        // (DECRQM mode numbers, XTGETTCAP payloads, secondary-DA request
        // forms). Bounded scans; matches ending inside the overlap were
        // answered on the earlier call and are filtered by the scanners.
        let overlap_len = self.query_overlap.len();
        let mut combined = Vec::with_capacity(overlap_len + bytes.len());
        combined.extend_from_slice(&self.query_overlap);
        combined.extend_from_slice(bytes);
        let mut decrqm = crate::queries::find_decrqm(&combined, overlap_len);
        let mut secondary = crate::queries::find_secondary_da(&combined, overlap_len);
        let mut tcaps = crate::queries::find_xtgettcap(&combined, overlap_len);
        let mut actions: Vec<TerminalAction> = Vec::new();
        self.parser.advance(bytes, |action| actions.push(action));
        for action in actions {
            // Map terminal actions to cold-path observations before state
            // mutation where the payload lives on the action itself; state
            // is the authority for derived values like title after mutation.
            let pre_event = match &action {
                TerminalAction::OscTitle { text } => {
                    Some(ColdEvent::TitleChanged(text.as_str().to_owned()))
                }
                TerminalAction::OscCwd { url } => {
                    Some(ColdEvent::CwdChanged(url.as_str().to_owned()))
                }
                TerminalAction::OscPromptMark { kind, .. } => Some(ColdEvent::ZoneMarked(*kind)),
                TerminalAction::OscHyperlink { link } => Some(ColdEvent::HyperlinkChanged(
                    link.as_ref().map(|h| h.uri.as_str().to_owned()),
                )),
                TerminalAction::SetMode { mode, enabled } => Some(ColdEvent::ModeChanged {
                    mode: *mode,
                    enabled: *enabled,
                }),
                TerminalAction::Unknown(seq) => Some(ColdEvent::UnknownSequence(seq.kind)),
                TerminalAction::OscUnknown { .. } => {
                    Some(ColdEvent::UnknownSequence(SequenceKind::Csi))
                }
                TerminalAction::PrintControl(ctrl) if ctrl.0 == 0x07 => Some(ColdEvent::Bell),
                TerminalAction::OscNotification { notification } => {
                    // CTX-0577: the policy decides the user-visible surface;
                    // the action itself is not a cold event (notifications
                    // are not plugin observations today).
                    self.apply_notification_policy(notification, std::time::Instant::now());
                    None
                }
                _ => None,
            };
            if let Some(ev) = pre_event {
                // CTX-0577: the bell policy decides the user-visible surface
                // (bounded visual flash by default, rate-limited under RC-8).
                if matches!(ev, ColdEvent::Bell) {
                    let _ = self.apply_bell_policy(std::time::Instant::now());
                }
                // Cold queue: bounded, drop-oldest, never blocks.
                self.cold_queue.push(ev.clone());
                // Side queue bridging: bounded, same non-blocking guarantee (ADR-0003 rule 4).
                if let Some(obs) = cold_to_observation(&ev) {
                    self.plugin_host.push_observation(obs);
                }
            }
            // OSC 52 clipboard (P0-AC-007): separate read/write policy.
            // Writes are capability-gated (clipboard.write); reads are consent-gated (clipboard.read).
            // Both default to deny. Untrusted PTY bytes cannot trigger clipboard
            // I/O without the corresponding granted capability / consent flag.
            if let TerminalAction::OscClipboard { op, data } = &action {
                match op {
                    ClipboardOp::Write => {
                        if !self.osc_clipboard_write_allowed {
                            continue;
                        }
                        // CTX-0212: the OSC 52 write payload is base64
                        // (RFC 4648 §4). Decode before storing so an encoded
                        // write lands decoded and write-then-read
                        // round-trips exact instead of double-encoding.
                        // Fail-closed: invalid base64 leaves the clipboard
                        // unchanged, bumps `osc52_rejected_writes`, and warns
                        // loudly; garbage is never raw-stored.
                        let decoded = match base64_decode_standard(data.as_bytes()) {
                            Ok(bytes) => bytes,
                            Err(reason) => {
                                self.osc52_rejected_writes =
                                    self.osc52_rejected_writes.wrapping_add(1);
                                // Rate-limited (CTX-0473): hostile PTY bytes can
                                // reject at child-output rate; the counter above
                                // stays exact while stderr stays bounded.
                                if let Some(suppressed) = self.osc52_log.admit_now() {
                                    eprintln!(
                                        "bitty: rejecting invalid OSC 52 clipboard write ({reason}): no clipboard change{}",
                                        log_throttle::suppressed_suffix(suppressed)
                                    );
                                }
                                continue;
                            }
                        };
                        let text = String::from_utf8_lossy(&decoded).into_owned();
                        self.clipboard.set_text_lossy(text);
                    }
                    ClipboardOp::Read => {
                        // Denied without explicit read consent (P0-AC-007):
                        // no data leaves the clipboard, no reply queued.
                        if !self.osc_clipboard_read_allowed {
                            continue;
                        }
                        // Allowed (CR-RT-01): answer with the base64-encoded
                        // clipboard text via the bounded `Reply` queue
                        // (Ghostty `clipboard_response` pattern), so
                        // tmux/neovim queries terminate instead of hanging.
                        // The lossy read keeps the reply path total: a
                        // system-clipboard failure answers with an empty
                        // payload rather than replaying a stale value
                        // (CTX-0478), an over-limit clipboard answers with
                        // its clipped prefix instead of an empty payload
                        // (CTX-0478 review), and the reply is always sent.
                        let text = self.clipboard.get_text_lossy();
                        let reply = osc52_read_reply(&text);
                        self.state.apply(&TerminalAction::Reply {
                            bytes: reply.into_boxed_slice(),
                        });
                    }
                }
            }
            // OSC 10/11 dynamic default colors (CTX-0381): the parser already
            // classified and bounded the payload (malformed forms never reach
            // this arm). Queries are answered from the resolved palette only
            // — no reply before theme resolution — and sets are
            // capability-gated (default deny) like OSC 52 writes.
            if let TerminalAction::OscDynamicColor { target, op } = &action {
                match op {
                    DynamicColorOp::Query => {
                        if self.config.theme_resolved {
                            let color = match target {
                                DynamicColorTarget::Foreground => self.active_foreground(),
                                DynamicColorTarget::Background => self.active_background(),
                            };
                            let reply = crate::queries::osc_color_reply(*target, color);
                            self.state.apply(&TerminalAction::Reply {
                                bytes: reply.into_boxed_slice(),
                            });
                        }
                    }
                    DynamicColorOp::Set(rgb) => {
                        if self.osc_color_set_allowed {
                            self.apply_osc_color(*target, [rgb.r, rgb.g, rgb.b]);
                        }
                    }
                }
            }
            // OSC 4 palette set/query (CTX-0392): the parser already
            // classified and bounded the payload to at most MAX_OSC4_OPS
            // pairs (malformed forms never reach this arm). Queries are
            // answered from the active 256-entry palette only — no reply
            // before theme resolution — and sets share the OSC color-set
            // gate (default deny) with OSC 10/11.
            if let TerminalAction::OscPalette { ops } = &action {
                for pair in ops.iter() {
                    match pair.op {
                        PaletteColorOp::Query => {
                            if self.config.theme_resolved {
                                let rgb = self.active_palette_color(pair.index);
                                let reply = crate::queries::osc_palette_reply(pair.index, rgb);
                                self.state.apply(&TerminalAction::Reply {
                                    bytes: reply.into_boxed_slice(),
                                });
                            }
                        }
                        PaletteColorOp::Set(rgb) => {
                            if self.osc_color_set_allowed {
                                self.apply_osc_palette(pair.index, [rgb.r, rgb.g, rgb.b]);
                            }
                        }
                    }
                }
            }
            // Kitty graphics (CTX-0256): the parser already base64-unwrapped
            // and reassembled `m=` chunks under the ledger cap, so `payload`
            // is decoded bytes ready for the existing intake seam. Route to
            // `kitty_display_image`, preserving transmit-only (`a=t` stores
            // without painting) and unknown-action (stored-not-painted)
            // semantics from CTX-0248. Failures store nothing and paint
            // nothing; warn loudly (parser already warned on base64/caps).
            if let TerminalAction::KittyGraphics {
                format_f,
                width_s,
                height_v,
                action_a,
                cols_c,
                rows_r,
                payload,
            } = &action
            {
                if let Err(err) = self.kitty_display_image(
                    *format_f, *width_s, *height_v, *action_a, *cols_c, *rows_r, payload, 0,
                ) {
                    // Rate-limited (CTX-0473): a hostile child can spam rejected
                    // kitty payloads; the parser's own warnings stay bounded too.
                    if let Some(suppressed) = self.kitty_log.admit_now() {
                        eprintln!(
                            "bitty: rejecting kitty image ({err}): stored nothing{}",
                            log_throttle::suppressed_suffix(suppressed)
                        );
                    }
                }
            }
            let damage = self.state.apply(&action);
            if !damage.regions.is_empty() {
                let generation = damage.generation;
                self.cold_queue.push(ColdEvent::Damage { generation });
                // Bridge damage generation as well.
                self.plugin_host
                    .push_observation(HostObservation::Damage { generation });
            }
            // Selection invalidation (CTX-0060, extended for issue #1337):
            // FullReset erases grid and scrollback, so any live selection
            // is no longer anchored to valid content. The same holds for
            // every ED mode that erases live-grid cells (Below/Above/All):
            // the highlight overlay paints from grid coordinates on the
            // next present, so a kept selection would keep painting its
            // rects over erased cells as a persistent block after `clear`.
            // Fail closed: drop the selection on all grid-erasing actions.
            // ED 3 (EraseDisplayMode::Scrollback) clears scrollback history
            // but leaves the live grid; live-grid selections remain valid,
            // so scrollback-only clears keep live selection.
            let grid_erased = matches!(action, TerminalAction::FullReset)
                || matches!(
                    action,
                    TerminalAction::EraseInDisplay {
                        mode: bitty_vt::EraseDisplayMode::Below
                            | bitty_vt::EraseDisplayMode::Above
                            | bitty_vt::EraseDisplayMode::All,
                    }
                );
            if grid_erased {
                self.clear_selection();
            }
            // CTX-0146 (Issue #238): answer standard terminal queries with
            // true capabilities. The parser maps these shapes to `Unknown`
            // (inert for the grid); the runtime queues the bounded reply via
            // the existing `Reply` action so the 4 KiB reply cap and the
            // `poll_pty -> write_replies` flush path apply unchanged.
            // Parameterized families additionally require their raw match
            // (pre-scanned above), so bytes buried inside OSC strings can
            // never spoof a reply, and stale overlap matches (answered on an
            // earlier call) are never re-answered.
            if let TerminalAction::Unknown(seq) = &action {
                let mut pending: Vec<Vec<u8>> = Vec::new();
                if crate::queries::is_secondary_da(seq) {
                    if secondary > 0 {
                        secondary -= 1;
                        pending.push(crate::queries::secondary_da_reply());
                    }
                } else if crate::queries::is_xterm_version(seq) {
                    pending.push(crate::queries::xterm_version_reply());
                } else if crate::queries::is_legacy_decid(seq) {
                    pending.push(crate::queries::primary_da_reply());
                } else if crate::queries::is_decrqm_private(seq)
                    || crate::queries::is_decrqm_ansi(seq)
                {
                    let want_private = crate::queries::is_decrqm_private(seq);
                    if let Some(pos) = decrqm.iter().position(|m| m.private == want_private) {
                        let query = decrqm.remove(pos);
                        for mode in query.modes {
                            let value =
                                crate::queries::decrqm_value(&self.state, want_private, mode);
                            pending.push(crate::queries::decrpm_reply(want_private, mode, value));
                        }
                    }
                } else if crate::queries::is_xtgettcap(seq) && !tcaps.is_empty() {
                    let query = tcaps.remove(0);
                    pending.push(crate::queries::xtgettcap_reply(&query.payload));
                }
                for reply in pending {
                    self.state.apply(&TerminalAction::Reply {
                        bytes: reply.into_boxed_slice(),
                    });
                }
            }
        }
        // Retain a bounded tail so a query split over two PTY reads is still
        // recognized on the next call. Capacity stays near the overlap bound:
        // huge chunks shrink back instead of pinning a large buffer.
        self.query_overlap.extend_from_slice(bytes);
        if self.query_overlap.len() > crate::queries::QUERY_OVERLAP_MAX {
            let excess = self.query_overlap.len() - crate::queries::QUERY_OVERLAP_MAX;
            self.query_overlap.drain(..excess);
        }
        if self.query_overlap.capacity() > crate::queries::QUERY_OVERLAP_MAX * 2 {
            self.query_overlap
                .shrink_to(crate::queries::QUERY_OVERLAP_MAX * 2);
        }
        // Search UI integration (CTX-0061): keep bounded matches in sync after
        // state growth/scrollback pushes; headless refresh is cheap (truncated
        // pattern, capped results) and deterministic. No I/O.
        if self.search_state.is_active() {
            self.search_state.refresh(&self.state);
        }
    }

    /// Drains queued PTY replies (device-status responses) without I/O.
    ///
    /// Terminal state synthesizes replies into a bounded queue; the runtime
    /// exposes them here so the embedder can write them back to the PTY
    /// master via `PtyWriter`. No upstream type is exposed.
    pub fn take_replies(&mut self) -> Vec<Box<[u8]>> {
        self.state.take_replies()
    }

    /// Writes pending PTY replies to the PTY writer (bounded, fail-closed).
    ///
    /// Forms the `poll_pty()->parse->state->replies->bounded PtyWriter::write_all()`
    /// loop for DSR/DA/cursor queries (DSR 6, DA `CSI c`, etc.). Bounded by
    /// `REPLY_CAP_BYTES` (4 KiB) in `State` (DropNewest per RFC invariant 7
    /// and counted via `replies_overflowed`). The hot path (`handle_pty_bytes`
    /// parsing and state apply) never blocks; this method is the only
    /// producer into the PTY master for replies and is best-effort, never
    /// panics, never interpolates through a shell, and never grows without
    /// bound. No shell interpolation, no unbounded buffering.
    ///
    /// When no live `PtyWriter` is owned (headless CI), the replies remain
    /// queued for `take_replies` observation and no I/O is performed (0
    /// returned). When a writer is present the replies are drained and each
    /// chunk is `write_all` + `flush` best-effort; errors are ignored
    /// (fail-closed, reply dropped, overflow already counted). Returns the
    /// total bytes successfully written (≤ 4 KiB, bounded).
    pub fn write_replies(&mut self) -> usize {
        use std::io::Write as _;
        if self.pty_writer.is_none() {
            return 0;
        }
        let replies = self.state.take_replies();
        if replies.is_empty() {
            return 0;
        }
        let Some(writer) = self.pty_writer.as_mut() else {
            return 0;
        };
        // Each chunk is bounded; total bounded by REPLY_CAP_BYTES (4 KiB).
        // Best-effort, fail-closed (CTX-0473): the first write error stops the
        // drain and the lost remainder is accounted, never silently swallowed.
        let (total, dropped) = write_chunks(writer, &replies);
        let flush_failed = writer.flush().is_err();
        if dropped > 0 {
            self.reply_write_dropped_bytes =
                self.reply_write_dropped_bytes.wrapping_add(dropped as u64);
        }
        if flush_failed {
            self.write_flush_failures = self.write_flush_failures.wrapping_add(1);
        }
        total
    }

    /// Alias for `write_replies` for embedders that use the `flush_pty_replies`
    /// name (both drain through the same bounded, fail-closed writer path).
    pub fn flush_pty_replies(&mut self) -> usize {
        self.write_replies()
    }

    /// Whether any reply was dropped due to the cap since the last drain.
    #[must_use]
    pub fn replies_overflowed(&self) -> bool {
        self.state.replies_overflowed()
    }

    /// Reply bytes dropped because a PTY writer failed mid-write (CTX-0473).
    ///
    /// Wrapping telemetry covering the primary and per-pane reply paths:
    /// bytes that never reached the child's shell.
    #[must_use]
    pub fn reply_write_dropped_bytes(&self) -> u64 {
        self.reply_write_dropped_bytes
    }

    /// Best-effort writer `flush` failures on reply/input paths (CTX-0473).
    #[must_use]
    pub fn write_flush_failures(&self) -> u64 {
        self.write_flush_failures
    }

    /// Wakeup-forwarder thread spawns refused by the OS (CTX-0473).
    ///
    /// The direct pump path is retained instead; non-zero means degraded
    /// wakeups, never a dropped reader.
    #[must_use]
    pub fn forwarder_spawn_failures(&self) -> u64 {
        self.forwarder_spawn_failures
    }

    /// Hot-path diagnostics suppressed by the log throttle (CTX-0473).
    ///
    /// Counts OSC 52/kitty reject and spawn-failure messages dropped to keep a
    /// hostile child from flooding stderr; the underlying event counters
    /// remain exact.
    #[must_use]
    pub fn suppressed_diagnostics(&self) -> u64 {
        self.osc52_log
            .suppressed()
            .saturating_add(self.kitty_log.suppressed())
            .saturating_add(self.spawn_log.suppressed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writer that accepts `ok_chunks` writes then errors. It reports the full
    /// buffer written, so each `write_all` consumes exactly one `write` call.
    struct FailingWriter {
        ok_chunks: usize,
        chunks: usize,
    }

    impl std::io::Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.chunks += 1;
            if self.chunks > self.ok_chunks {
                return Err(std::io::Error::other("simulated write failure"));
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn boxed(items: &[&[u8]]) -> Vec<Box<[u8]>> {
        items.iter().map(|item| (*item).into()).collect()
    }

    #[test]
    fn write_chunks_reports_written_and_dropped_remainder() {
        let data = boxed(&[b"aa", b"bbb", b"c"]);
        let mut writer = FailingWriter {
            ok_chunks: 1,
            chunks: 0,
        };
        let (written, dropped) = write_chunks(&mut writer, &data);
        assert_eq!(written, 2);
        assert_eq!(dropped, 3 + 1, "failed chunk plus remainder counted");
    }

    #[test]
    fn write_chunks_all_ok_drops_nothing() {
        let data = boxed(&[b"aa", b"bbb", b"c"]);
        let mut writer = FailingWriter {
            ok_chunks: usize::MAX,
            chunks: 0,
        };
        let (written, dropped) = write_chunks(&mut writer, &data);
        assert_eq!(written, 6);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn spawn_recovering_returns_payload_when_spawn_refused() {
        let result = spawn_recovering(
            7u32,
            |_| {},
            |_body| Err(std::io::Error::other("simulated spawn refusal")),
        );
        let (payload, _error) = result.expect_err("spawn must be refused");
        assert_eq!(payload, Some(7), "payload must not be lost on refusal");
    }

    #[test]
    fn spawn_recovering_runs_body_on_success() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let body_seen = std::sync::Arc::clone(&seen);
        let handle = spawn_recovering(
            7u32,
            move |payload| *body_seen.lock().expect("lock") = Some(payload),
            |body| {
                body();
                Ok(std::thread::spawn(|| {}))
            },
        )
        .expect("spawn succeeds");
        handle.join().expect("join dummy handle");
        assert_eq!(*seen.lock().expect("lock"), Some(7));
    }
}
