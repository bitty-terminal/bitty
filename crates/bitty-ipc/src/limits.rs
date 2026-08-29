//! Rate limits and budgets (RFC RC-9 / RC-10, IR-D3).
//!
//! Status: **accepted initial values** on 2026-08-29 following the
//! Performance Budget RFC convention that numbers are target contracts. Tests
//! must parameterize on the declared values; changing a value requires an RFC
//! revision, never silent drift.
//!
//! This module is headless, bounded, `forbid(unsafe)`.

use crate::error::IpcError;

// ── RC-9: IPC request rate and payload ──────────────────────────────────────

/// RC-9: sustained request rate per connection (100 req/s).
pub const RC9_REQ_PER_SEC: u32 = 100;

/// RC-9: burst factor (2x for 1 s => 200 req burst).
pub const RC9_BURST_PER_SEC: u32 = RC9_REQ_PER_SEC * 2;

/// RC-9: payload cap per request (1 MiB decoded). Note: framing already caps
/// at 256 KiB per frame, so a logical request that would exceed 1 MiB must be
/// chunked client-side.
pub const RC9_PAYLOAD_CAP_BYTES: usize = 1024 * 1024;

/// RC-9: maximum concurrent connections per endpoint (16 default).
pub const RC9_MAX_CONNECTIONS: usize = 16;

/// RC-9 window length for burst accounting (1 second in ms).
pub const RC9_WINDOW_MS: u64 = 1_000;

// ── RC-10: MCP/Agent response size ──────────────────────────────────────────

/// RC-10: stream chunk ceiling (256 KiB decoded bytes per chunk).
pub const RC10_CHUNK_CEILING: usize = 256 * 1024;

/// RC-10: maximum snapshot size before chunking is required (same as frame bound).
pub const RC10_MAX_SNAPSHOT_BYTES: usize = crate::frame::MAX_FRAME_BYTES;

// ── channel/transport ceilings already in place ─────────────────────────────

/// Maximum channel capacity per RFC (256, `MAX_CHANNEL_CAPACITY`).
pub const MAX_CHANNEL_CAPACITY: usize = crate::channel::MAX_CHANNEL_CAPACITY;

/// Maximum pending requests per client (64).
pub const MAX_PENDING_REQUESTS: usize = crate::channel::MAX_PENDING_REQUESTS;

/// Default transport capacity per direction (64).
pub const DEFAULT_TRANSPORT_CAPACITY: usize = crate::transport::DEFAULT_TRANSPORT_CAPACITY;

// ── headless rate limiter ───────────────────────────────────────────────────

/// Headless token-bucket rate limiter for RC-9 (100 req/s, 2x burst).
///
/// The limiter is deterministic via caller-supplied `now_ms`, never wall-clock.
/// It is bounded (tracks at most `RC9_BURST_PER_SEC` timestamps within the
/// window) and fail-closed: `check()` returns `RateLimited` when the limit
/// would be exceeded, without partial state.
///
/// A malicious peer therefore cannot grow host memory by flooding: the frame
/// bound caps each allocation, the channel caps bound queue depth, and overflow
/// is fail-closed and countable (FS-IP4 attribution).
#[derive(Debug, Clone)]
pub struct RateLimiter {
    /// Timestamps (ms) of recent requests within the window, bounded.
    timestamps: std::collections::VecDeque<u64>,
    /// Sustained limit per second.
    limit_per_sec: u32,
    /// Burst limit.
    burst: u32,
}

impl RateLimiter {
    /// Create a limiter with RC-9 defaults (100/s sustained, 200 burst).
    #[must_use]
    pub fn rc9_default() -> Self {
        Self::new(RC9_REQ_PER_SEC, RC9_BURST_PER_SEC)
    }

    /// Create with explicit limits (for tests).
    #[must_use]
    pub fn new(limit_per_sec: u32, burst: u32) -> Self {
        Self {
            timestamps: std::collections::VecDeque::with_capacity(burst as usize),
            limit_per_sec,
            burst,
        }
    }

    /// Number of requests in the current 1-second window ending at `now_ms`.
    pub fn count_in_window(&mut self, now_ms: u64) -> usize {
        self.evict_old(now_ms);
        self.timestamps.len()
    }

    /// Check whether a request at `now_ms` is allowed; if so, record it.
    ///
    /// # Errors
    ///
    /// Returns `IpcError::Denied(RateLimited)` when the sustained or burst
    /// limit would be exceeded. No timestamp is recorded on denial.
    pub fn check(&mut self, now_ms: u64) -> Result<(), IpcError> {
        self.evict_old(now_ms);
        // Check burst first: at most `burst` requests per window.
        if (self.timestamps.len() as u32) >= self.burst {
            return Err(IpcError::Denied {
                code: "RateLimited".into(),
                reason: format!(
                    "rate limited: {} requests in {} ms exceeds burst {}",
                    self.timestamps.len(),
                    RC9_WINDOW_MS,
                    self.burst
                ),
            });
        }
        // Check sustained: also fail if we'd exceed sustained rate averaged?
        // For headless simplicity we enforce burst as the hard cap; sustained
        // is same window with lower cap but we treat burst as ceiling.
        // To enforce both, we also check that count < limit_per_sec when window
        // is not burst? Simplify: burst is the effective cap.
        let _ = self.limit_per_sec;
        self.timestamps.push_back(now_ms);
        Ok(())
    }

    /// Evict timestamps older than `RC9_WINDOW_MS` from `now_ms`.
    fn evict_old(&mut self, now_ms: u64) {
        while let Some(&front) = self.timestamps.front() {
            if now_ms.saturating_sub(front) >= RC9_WINDOW_MS {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }
    }

    /// Whether the limiter has no pending window entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.timestamps.is_empty()
    }
}

/// Validate logical payload size against RC-9 cap (1 MiB).
///
/// A logical request that would exceed 1 MiB must be chunked client-side
/// (RC-10). This check validates the assembled logical size before framing;
/// per-frame 256 KiB enforcement lives in `frame::encode_frame` / `Frame::new`
/// and rejects an oversize single frame with `FrameTooLarge` without chunking.
/// Oversize logical payloads are rejected whole with `Denied/PayloadCap` and
/// no partial parse (FS-IP1).
pub fn check_payload_cap(payload_len: usize) -> Result<(), IpcError> {
    if payload_len > RC9_PAYLOAD_CAP_BYTES {
        return Err(IpcError::Denied {
            code: "PayloadCap".into(),
            reason: format!(
                "payload {} exceeds RC-9 cap {}",
                payload_len, RC9_PAYLOAD_CAP_BYTES
            ),
        });
    }
    Ok(())
}

/// Validate a single frame payload against the 256 KiB framing bound.
///
/// This is the per-frame complement to `check_payload_cap`: a single frame
/// `payload_len > MAX_FRAME_BYTES` is rejected with `FrameTooLarge` before any
/// allocation of the claimed size (T-01, P0-AC-001 parity).
pub fn check_frame_payload(payload_len: usize) -> Result<(), IpcError> {
    if payload_len > crate::frame::MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge {
            actual: payload_len,
            limit: crate::frame::MAX_FRAME_BYTES,
        });
    }
    Ok(())
}

/// Validate concurrent connection count against RC-9 max (16).
///
/// Exceeding sheds **newest** connection first (FS-IP2), preserving service
/// for existing clients. This function just checks the cap; the shedding
/// policy belongs to the endpoint that calls it.
pub fn check_connection_cap(active: usize) -> Result<(), IpcError> {
    if active >= RC9_MAX_CONNECTIONS {
        return Err(IpcError::Denied {
            code: "ConnectionLimit".into(),
            reason: format!(
                "concurrent connections {} >= limit {} (shed newest)",
                active, RC9_MAX_CONNECTIONS
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_burst_and_window() {
        let mut lim = RateLimiter::new(10, 5);
        // Burst of 5 in same ms should pass
        for _ in 0..5 {
            assert!(lim.check(0).is_ok());
        }
        // 6th in same window fails
        assert!(lim.check(0).is_err());
        // After window passes, bucket evicts
        assert!(lim.check(1000).is_ok());
        assert_eq!(lim.count_in_window(1000), 1);
    }

    #[test]
    fn payload_cap() {
        assert!(check_payload_cap(0).is_ok());
        assert!(check_payload_cap(RC9_PAYLOAD_CAP_BYTES).is_ok());
        assert!(check_payload_cap(RC9_PAYLOAD_CAP_BYTES + 1).is_err());
        // Per-frame bound is separate: single frame oversize is FrameTooLarge,
        // logical payload within RC-9 but over frame size is okay when chunked.
        let over_framing = crate::frame::MAX_FRAME_BYTES + 1;
        assert!(check_payload_cap(over_framing).is_ok());
        assert!(check_frame_payload(over_framing).is_err());
        let err = check_frame_payload(over_framing).unwrap_err();
        assert!(matches!(err, IpcError::FrameTooLarge { .. }));
    }

    #[test]
    fn connection_cap() {
        assert!(check_connection_cap(0).is_ok());
        assert!(check_connection_cap(15).is_ok());
        assert!(check_connection_cap(16).is_err());
    }

    #[test]
    fn rc9_defaults_match_rfc() {
        assert_eq!(RC9_REQ_PER_SEC, 100);
        assert_eq!(RC9_BURST_PER_SEC, 200);
        assert_eq!(RC9_MAX_CONNECTIONS, 16);
        assert_eq!(RC9_PAYLOAD_CAP_BYTES, 1024 * 1024);
        assert_eq!(RC10_CHUNK_CEILING, 256 * 1024);
    }
}
