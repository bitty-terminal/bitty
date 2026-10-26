//! Scoped audio-monitor contract: spectrum rate gate plus audio-state
//! read/control provider logic (CTX-1196, bitty#2009/#2013).
//!
//! Core-side contract for the NEW `audio` capability family beside every
//! existing family. One invariant: **a granted plugin may poll a bounded
//! level meter and bounded volume/mute/port snapshots through the
//! PulseAudio/PipeWire monitor, or issue bounded volume steps through a
//! separate control head; it may never open a stream, widen a scope,
//! drive an unbounded poll rate, smuggle shell text, or read what the
//! monitor never reported.**
//!
//! # What this module owns
//!
//! - Interface identities: `audio.spectrum` v1.0.0 (`spectrum(buckets)`),
//!   `audio.state` v1.0.0 (`get()`), `audio.control` v1.0.0
//!   (`set_volume`, `toggle_mute`). Short names; broker resolution applies
//!   the dynapi rules unchanged (owner-qualified name, version range,
//!   capability-backed, grant-gated consume per bitty#2003/#2005/#2006).
//! - [`SpectrumGate`]: the provider-side fast-poll rate contract. The
//!   sampler closure runs at most once per ceiling window (pinned by
//!   test); over-poll coalesces to last-good; monitor loss serves
//!   last-good with age inside [`SPECTRUM_STALE_HIDE_MS`], then hides.
//! - [`StateGate`]: the 2 s read poll ceiling plus stale-then-hide for
//!   `get()` snapshots, same closure discipline.
//! - [`ControlGate`]: the per-call control budget (min interval) plus
//!   saturating wheel-burst coalescing (composes with the `on_scroll`
//!   delta from bitty#2054: bursts collapse to one bounded step).
//! - Fixed-argv constructors for the scoped monitor (`pactl` only, six
//!   fixed shapes, no shell) and bounded `pactl` output parsers.
//! - [`AudioState`]: the `get()` shape
//!   (`volume_pct`, `muted`, `port`, `source_*`, `stale_age_ms`).
//!
//! # Rate contract (the rate answer; the manifest allowlist is permission only)
//!
//! Manifest declaration (`audio.spectrum = true` + consent) solves
//! permission. It does not solve rate: spectrum at display rates through
//! the plugin VM fights the RC-1 wall-clock/instruction budgets per
//! `execute` (10^7 VM instructions / 50 ms wall / 8 ms warning, measured
//! via `bitty-lua` Fuel + wall deadline). This contract is the rate
//! answer:
//!
//! | Surface | Ceiling | Bound |
//! |---|---|---|
//! | `spectrum` poll | 5 Hz ([`SPECTRUM_MIN_INTERVAL_MS`]) | <= 32 buckets, <= 1024 response bytes |
//! | `get` poll | 0.5 Hz ([`STATE_MIN_INTERVAL_MS`]) | one 6-field snapshot |
//! | control call | 10 Hz ([`CONTROL_MIN_INTERVAL_MS`]) | one argv spawn, step <= 10 |
//!
//! Rendering needs no new vehicle: block glyphs (U+2581-U+2588) in a Text
//! node updated per tick is how terminal visualizers draw. Glyph mapping,
//! pause state, falloff smoothing, and peak hold stay pure Lua policy.
//! Full-rate 60 fps cava is explicitly out of scope: that is not a
//! plugin-VM workload. The cava-lite target is 4-5 fps.
//!
//! # Over-poll policy (documented per bitty#2009 acceptance)
//!
//! Over-poll **coalesces**, it never fails the call and never floods the
//! VM: a poll inside the ceiling window runs no monitor work and serves
//! the last-good sample with its age ([`SpectrumPoll::Coalesced`],
//! [`StatePoll::Coalesced`]). Rationale: a bar ticking at 5 Hz against a
//! 5 Hz ceiling must render steadily, not blink on phase drift. The
//! ceiling still holds because a coalesced poll performs no monitor work
//! and returns bounded bytes.
//!
//! # What this module does NOT own (parked owners)
//!
//! - Process spawning and wall-clock time: `now_ms` is a monotonic host
//!   tick advanced explicitly by the caller; sampling closures and the
//!   spawner are caller-provided. No wall-clock, no randomness, no I/O.
//! - The production monitor binding and sandbox decision (separate
//!   follow-up per bitty#2009/#2013): this module defines the argv shapes
//!   and parsers so that follow-up only plugs I/O behind the seam.
//! - Exact Lua spellings and the manifest grammar that declares them
//!   (SDK); the host capability heads (`audio.spectrum`,
//!   `audio.state.read`, `audio.state.control`) are the closed grammar in
//!   [`crate::capability`], enforced there.
//! - Broker wiring: dynapi gating, negotiation, and the observability
//!   ledger already cover these heads generically; audio-specific
//!   participation is pinned by tests in `bitty-runtime`.
//!
//! # Non-goals
//!
//! No `unsafe`, no I/O, no new dependency (`std` plus sibling host types
//! only). Pure data plus validation, headlessly testable.

use std::fmt;

// ── interface identities ──────────────────────────────────────────────────

/// Capability family label for this surface (never under another family).
pub const AUDIO_FAMILY: &str = "audio";

/// Host contract version of the audio surface.
///
/// Capability-registry stable from acceptance: identifier choice is a
/// compatibility decision, so a version mismatch disables the surface with
/// a diagnostic instead of serving reads under unknown semantics.
pub const AUDIO_CONTRACT_VERSION: u32 = 1;

/// Provided-interface name for the level meter (bitty#2009).
pub const SPECTRUM_IFACE: &str = "audio.spectrum";
/// Provided-interface version for the level meter.
pub const SPECTRUM_VERSION: &str = "1.0.0";
/// Method name serving the level meter.
pub const SPECTRUM_METHOD: &str = "spectrum";

/// Provided-interface name for the state snapshot (bitty#2013, read head).
pub const STATE_IFACE: &str = "audio.state";
/// Provided-interface version for the state snapshot.
pub const STATE_VERSION: &str = "1.0.0";
/// Method name serving the state snapshot.
pub const STATE_METHOD_GET: &str = "get";

/// Provided-interface name for volume/mute control (bitty#2013, control head).
///
/// A different interface from [`STATE_IFACE`] on purpose: read grants never
/// imply control, and control without its head fails closed (negative
/// test in `bitty-runtime`).
pub const CONTROL_IFACE: &str = "audio.control";
/// Provided-interface version for volume/mute control.
pub const CONTROL_VERSION: &str = "1.0.0";
/// Method name stepping or parking the volume.
pub const CONTROL_METHOD_SET_VOLUME: &str = "set_volume";
/// Method name toggling the mute flag.
pub const CONTROL_METHOD_TOGGLE_MUTE: &str = "toggle_mute";

/// Backing Core capability head for [`SPECTRUM_IFACE`].
pub const SPECTRUM_HEAD: &str = "audio.spectrum";
/// Backing Core capability head for [`STATE_IFACE`].
pub const STATE_READ_HEAD: &str = "audio.state.read";
/// Backing Core capability head for [`CONTROL_IFACE`].
///
/// Defaults to deny like every head; needs explicit consent per the
/// capability-gating rules (bitty#2005).
pub const STATE_CONTROL_HEAD: &str = "audio.state.control";

// ── spectrum rate contract ────────────────────────────────────────────────

/// Maximum spectrum buckets per poll (cava-lite bar count).
///
/// Bounds the per-tick Lua work to 33 values (one table plus levels), a
/// stated fraction of the RC-1 per-execute budget (see the tick-budget
/// proof test).
pub const SPECTRUM_MAX_BUCKETS: usize = 32;

/// Minimum milliseconds between monitor samples (5 Hz poll ceiling).
pub const SPECTRUM_MIN_INTERVAL_MS: u64 = 200;

/// Maximum native monitor taps accepted per sample.
///
/// The monitor's native resolution stays above the served resolution;
/// [`SpectrumGate::poll`] resamples down to the requested bucket count.
pub const SPECTRUM_MAX_TAPS: usize = 64;

/// Maximum bytes of one served spectrum response.
///
/// 32 buckets formatted at 8 chars each plus table overhead fit under a
/// quarter of this cap; the tick-budget proof test pins the worst case.
pub const SPECTRUM_MAX_RESPONSE_BYTES: usize = 1024;

/// Stale grace after monitor loss: last-good is served with age inside
/// this window, then the surface hides (never fabricates).
pub const SPECTRUM_STALE_HIDE_MS: u64 = 2000;

// ── state read contract ───────────────────────────────────────────────────

/// Minimum milliseconds between monitor reads (0.5 Hz poll ceiling).
pub const STATE_MIN_INTERVAL_MS: u64 = 2000;

/// Stale grace for `get()` after the monitor goes silent.
pub const STATE_STALE_HIDE_MS: u64 = 10_000;

/// Maximum bytes of one port label (`get().port`).
pub const AUDIO_MAX_PORT_BYTES: usize = 64;

/// Maximum bytes of one sink name parsed from monitor output.
pub const AUDIO_MAX_SINK_BYTES: usize = 128;

/// Maximum bytes of monitor output parsed per read (fail-closed above).
pub const MONITOR_OUTPUT_MAX_BYTES: usize = 16 * 1024;

// ── control contract ──────────────────────────────────────────────────────

/// Minimum milliseconds between control calls (10 Hz individual budget).
pub const CONTROL_MIN_INTERVAL_MS: u64 = 100;

/// Minimum absolute volume percent served by `set_volume`.
pub const VOLUME_MIN_PCT: u8 = 0;

/// Maximum absolute volume percent served by `set_volume`.
///
/// Monitor over-amplification above this saturates at the ceiling on
/// read; control never parks above it.
pub const VOLUME_MAX_PCT: u8 = 100;

/// Maximum single-call relative step in percent (bounded steps).
///
/// Over-bound steps are denied, never clamped silently. Wheel-burst
/// accumulation saturates at this same bound so every drain fits one
/// admitted call.
pub const VOLUME_STEP_MAX_PCT: i32 = 10;

/// Maximum bytes of one emitted control argv (fixed shapes stay far below).
pub const CONTROL_MAX_ARGV_BYTES: usize = 256;

// ── errors ────────────────────────────────────────────────────────────────

/// Audio contract failure kind.
///
/// Broker surfacing reuses the existing dynapi codes (`E_SERVICE_*`,
/// `E_CAPABILITY_DENIED`); this taxonomy stays inside the contract for
/// typed fail-closed handling before the broker is ever involved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioErrorKind {
    /// Request exceeds a contract bound (bucket count, step, byte cap).
    Budget,
    /// Monitor output or caller input is malformed.
    Invalid,
    /// Monitor backend absent or unreachable (fail-closed, retryable).
    MonitorUnavailable,
    /// Stale past the hide grace, or never sampled: hide, never fabricate.
    Hidden,
}

/// Typed audio contract failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioError {
    kind: AudioErrorKind,
    message: String,
}

impl AudioError {
    /// Budget failure naming the bound.
    pub fn budget(what: impl Into<String>) -> Self {
        Self {
            kind: AudioErrorKind::Budget,
            message: what.into(),
        }
    }

    /// Malformed-input failure.
    pub fn invalid(what: impl Into<String>) -> Self {
        Self {
            kind: AudioErrorKind::Invalid,
            message: what.into(),
        }
    }

    /// Backend-absent failure (fail-closed, retryable).
    pub fn unavailable(what: impl Into<String>) -> Self {
        Self {
            kind: AudioErrorKind::MonitorUnavailable,
            message: what.into(),
        }
    }

    /// Hide-grace-exhausted failure (hide, never fabricate).
    pub fn hidden(what: impl Into<String>) -> Self {
        Self {
            kind: AudioErrorKind::Hidden,
            message: what.into(),
        }
    }

    /// Failure kind.
    #[must_use]
    pub const fn kind(&self) -> AudioErrorKind {
        self.kind
    }

    /// Human-readable detail (bounded, caller-supplied at construction).
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "audio {:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for AudioError {}

// ── spectrum gate ─────────────────────────────────────────────────────────

/// Served spectrum sample (bitty#2009 stale policy).
#[derive(Debug, Clone, PartialEq)]
pub enum SpectrumPoll {
    /// Fresh sample taken inside this call.
    Fresh {
        /// Resampled levels in `[0.0, 1.0]`, length equals the request.
        levels: Vec<f32>,
        /// Milliseconds since the previous backing monitor sample.
        age_ms: u64,
    },
    /// Over-poll inside the ceiling window: the sampler did NOT run;
    /// last-good served, no monitor work performed (documented
    /// coalescing policy; the provider never floods the VM).
    Coalesced {
        /// Resampled last-good levels, length equals the request.
        levels: Vec<f32>,
        /// Milliseconds since the backing monitor sample.
        age_ms: u64,
    },
    /// Sampler failed inside the hide grace: last-good served with age so
    /// the bar can dim before it hides.
    Stale {
        /// Resampled last-good levels, length equals the request.
        levels: Vec<f32>,
        /// Milliseconds since the backing monitor sample.
        age_ms: u64,
    },
}

impl SpectrumPoll {
    /// Served levels regardless of freshness.
    #[must_use]
    pub fn levels(&self) -> &[f32] {
        match self {
            Self::Fresh { levels, .. }
            | Self::Coalesced { levels, .. }
            | Self::Stale { levels, .. } => levels,
        }
    }

    /// Milliseconds since the backing monitor sample.
    #[must_use]
    pub const fn age_ms(&self) -> u64 {
        match self {
            Self::Fresh { age_ms, .. }
            | Self::Coalesced { age_ms, .. }
            | Self::Stale { age_ms, .. } => *age_ms,
        }
    }
}

/// Provider-side fast-poll rate gate for `audio.spectrum`.
///
/// Holds the last-good monitor taps plus explicit host ticks (no
/// wall-clock). The sampler closure runs at most once per
/// [`SPECTRUM_MIN_INTERVAL_MS`] window: over-poll serves last-good
/// without invoking it, so the VM budget holds no matter how fast the
/// bar ticks.
#[derive(Debug, Clone, Default)]
pub struct SpectrumGate {
    taps: Vec<f32>,
    last_sample_ms: u64,
    last_poll_ms: u64,
    initialized: bool,
    pre_init_last_ms: Option<u64>,
}

impl SpectrumGate {
    /// Empty gate (never sampled: polls hide until the first sample lands).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Serve `buckets` levels at tick `now_ms`.
    ///
    /// `buckets` must sit in 1..=[`SPECTRUM_MAX_BUCKETS`]. Inside the
    /// ceiling window `sample` is NOT invoked and last-good is served
    /// coalesced. Outside the window `sample` runs once: a valid sample
    /// (1..=[`SPECTRUM_MAX_TAPS`] finite taps; out-of-range values clamp
    /// to `[0.0, 1.0]`) stores and serves fresh, while a failed sample
    /// serves last-good stale inside the hide grace and hides past it.
    /// Before the first landed sample a failed sample hides (never
    /// fabricates).
    pub fn poll(
        &mut self,
        now_ms: u64,
        buckets: usize,
        sample: impl FnOnce() -> Result<Vec<f32>, AudioError>,
    ) -> Result<SpectrumPoll, AudioError> {
        if buckets == 0 || buckets > SPECTRUM_MAX_BUCKETS {
            return Err(AudioError::budget(format!(
                "spectrum buckets {buckets} out of 1..={} (sampler never ran)",
                SPECTRUM_MAX_BUCKETS
            )));
        }
        if self.initialized && now_ms.saturating_sub(self.last_poll_ms) < SPECTRUM_MIN_INTERVAL_MS {
            // Coalesced polls never advance the window: under sustained
            // over-poll the next window still samples instead of sliding
            // forever.
            let levels = resample_levels(&self.taps, buckets);
            let age_ms = now_ms.saturating_sub(self.last_sample_ms);
            return Ok(SpectrumPoll::Coalesced { levels, age_ms });
        }
        if !self.initialized {
            // Pre-init ceiling (bitty#2062): failed attempts advance the
            // gate so a dead monitor is re-probed at the ceiling rate,
            // never hammered per tick. The sampler stays uninvoked inside
            // the window; the poll hides without monitor work.
            if let Some(last) = self.pre_init_last_ms {
                if now_ms.saturating_sub(last) < SPECTRUM_MIN_INTERVAL_MS {
                    return Err(AudioError::unavailable(
                        "spectrum monitor polling too fast (pre-init ceiling)",
                    ));
                }
            }
        }
        let age_ms = now_ms.saturating_sub(self.last_sample_ms);
        match sample() {
            Ok(taps) => match Self::check_taps(&taps) {
                Ok(()) => {
                    let clamped: Vec<f32> = taps.iter().map(|tap| tap.clamp(0.0, 1.0)).collect();
                    self.taps = clamped;
                    self.last_sample_ms = now_ms;
                    self.last_poll_ms = now_ms;
                    self.initialized = true;
                    self.pre_init_last_ms = None;
                    Ok(SpectrumPoll::Fresh {
                        levels: resample_levels(&self.taps, buckets),
                        age_ms,
                    })
                }
                // A corrupt sample never displaces last-good: with history
                // it serves stale inside the grace, without history the
                // check error surfaces (hide, never an empty render).
                Err(check) => {
                    if self.initialized {
                        self.stale_or_hidden(now_ms, buckets, age_ms)
                    } else {
                        self.pre_init_last_ms = Some(now_ms);
                        self.last_poll_ms = now_ms;
                        Err(check)
                    }
                }
            },
            Err(error) => {
                if self.initialized {
                    return self.stale_or_hidden(now_ms, buckets, age_ms);
                }
                self.pre_init_last_ms = Some(now_ms);
                self.last_poll_ms = now_ms;
                Err(error)
            }
        }
    }

    /// Serve last-good stale inside the hide grace, hide past it.
    ///
    /// Attempted outcomes advance the window so a dead monitor is
    /// re-probed at the ceiling rate, never hammered per tick.
    fn stale_or_hidden(
        &mut self,
        now_ms: u64,
        buckets: usize,
        age_ms: u64,
    ) -> Result<SpectrumPoll, AudioError> {
        self.last_poll_ms = now_ms;
        if age_ms < SPECTRUM_STALE_HIDE_MS {
            let levels = resample_levels(&self.taps, buckets);
            return Ok(SpectrumPoll::Stale { levels, age_ms });
        }
        Err(AudioError::hidden(format!(
            "spectrum monitor lost for {age_ms} ms (hide, never fabricate)"
        )))
    }

    /// Validate one raw monitor sample (fail-closed: bad samples never
    /// displace last-good).
    fn check_taps(taps: &[f32]) -> Result<(), AudioError> {
        if taps.is_empty() || taps.len() > SPECTRUM_MAX_TAPS {
            return Err(AudioError::budget(format!(
                "spectrum taps {} out of 1..={} (sample rejected)",
                taps.len(),
                SPECTRUM_MAX_TAPS
            )));
        }
        if taps.iter().any(|tap| !tap.is_finite()) {
            return Err(AudioError::invalid(
                "spectrum sample holds a non-finite tap (sample rejected)",
            ));
        }
        Ok(())
    }
}

/// Resample stored taps to `buckets` levels (nearest neighbor).
///
/// Pure helper shared by every poll outcome so fresh, coalesced, and
/// stale renders cost the same bounded O(buckets) work.
#[must_use]
pub fn resample_levels(taps: &[f32], buckets: usize) -> Vec<f32> {
    if taps.is_empty() || buckets == 0 {
        return Vec::new();
    }
    (0..buckets)
        .map(|index| {
            let tap = (index * taps.len()) / buckets;
            taps[tap.min(taps.len() - 1)]
        })
        .collect()
}

/// Worst-case response bytes for a `buckets` poll.
///
/// Canonical serialization cost: 8 chars per level plus 64 bytes of
/// table overhead. Stays under [`SPECTRUM_MAX_RESPONSE_BYTES`] for every
/// admissible bucket count (pinned by test).
#[must_use]
pub const fn spectrum_response_bytes(buckets: usize) -> usize {
    buckets.saturating_mul(8).saturating_add(64)
}

/// Lua values crossed per spectrum poll (one table plus levels).
#[must_use]
pub const fn spectrum_poll_value_count(buckets: usize) -> usize {
    buckets.saturating_add(1)
}

// ── audio state ───────────────────────────────────────────────────────────

/// `get()` snapshot shape (bitty#2013 read head).
///
/// Icon and port selection stay Lua-side: the provider returns port names
/// only. `stale_age_ms` carries the monitor age so the bar can dim stale
/// renders before it hides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioState {
    /// Sink volume percent, 0..=100 (monitor over-amplification saturates).
    pub volume_pct: u8,
    /// Sink mute flag.
    pub muted: bool,
    /// Active port name (bounded, e.g. `analog-output-headphones`).
    pub port: String,
    /// Source (microphone) volume percent, when the monitor reports one.
    pub source_volume_pct: Option<u8>,
    /// Source mute flag, when the monitor reports one.
    pub source_muted: Option<bool>,
    /// Milliseconds since the backing monitor read.
    pub stale_age_ms: u64,
}

impl AudioState {
    /// Validate a monitor snapshot (fail-closed: malformed snapshots never
    /// reach the bar).
    pub fn validate(
        volume_pct: u8,
        muted: bool,
        port: &str,
        source_volume_pct: Option<u8>,
        source_muted: Option<bool>,
    ) -> Result<Self, AudioError> {
        if volume_pct > VOLUME_MAX_PCT {
            return Err(AudioError::budget(format!(
                "volume {volume_pct} exceeds {VOLUME_MAX_PCT} (snapshot rejected)"
            )));
        }
        if let Some(source) = source_volume_pct {
            if source > VOLUME_MAX_PCT {
                return Err(AudioError::budget(format!(
                    "source volume {source} exceeds {VOLUME_MAX_PCT} (snapshot rejected)"
                )));
            }
        }
        validate_port(port)?;
        Ok(Self {
            volume_pct,
            muted,
            port: port.to_string(),
            source_volume_pct,
            source_muted,
            stale_age_ms: 0,
        })
    }
}

/// Validate one port label: non-empty, bounded, charset `[a-z0-9_.-]`
/// (covers `pactl` port names; anything else fails closed, never
/// sanitized into a different name).
fn validate_port(port: &str) -> Result<(), AudioError> {
    if port.is_empty() || port.len() > AUDIO_MAX_PORT_BYTES {
        return Err(AudioError::budget(format!(
            "port label {} bytes out of 1..={} (snapshot rejected)",
            port.len(),
            AUDIO_MAX_PORT_BYTES
        )));
    }
    let ok = port.bytes().all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || byte == b'-'
            || byte == b'_'
            || byte == b'.'
    });
    if !ok {
        return Err(AudioError::invalid(
            "port label carries outside [a-z0-9_.-] (snapshot rejected)",
        ));
    }
    Ok(())
}

/// Served state snapshot (bitty#2013 stale policy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatePoll {
    /// Fresh monitor read taken inside this call.
    Fresh(AudioState),
    /// Over-poll inside the 2 s ceiling: the reader did NOT run; cached
    /// snapshot served, no monitor work.
    Coalesced(AudioState),
    /// Reader failed inside the hide grace: last-good with age.
    Stale(AudioState),
}

impl StatePoll {
    /// Served snapshot regardless of freshness.
    #[must_use]
    pub fn state(&self) -> &AudioState {
        match self {
            Self::Fresh(state) | Self::Coalesced(state) | Self::Stale(state) => state,
        }
    }
}

/// Provider-side read gate for `audio.state.get()`.
///
/// Holds the last-good snapshot plus explicit host ticks (no
/// wall-clock). The reader closure runs at most once per
/// [`STATE_MIN_INTERVAL_MS`] window; over-poll serves cache, silence
/// serves stale inside the grace, and anything older hides.
#[derive(Debug, Clone, Default)]
pub struct StateGate {
    last: Option<AudioState>,
    last_ok_ms: u64,
    last_poll_ms: u64,
    pre_init_last_ms: Option<u64>,
}

impl StateGate {
    /// Empty gate (never read: polls hide until the first snapshot lands).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Serve one `get()` at tick `now_ms`.
    pub fn poll(
        &mut self,
        now_ms: u64,
        read: impl FnOnce() -> Result<AudioState, AudioError>,
    ) -> Result<StatePoll, AudioError> {
        if let Some(cached) = self.last.clone() {
            if now_ms.saturating_sub(self.last_poll_ms) < STATE_MIN_INTERVAL_MS {
                // Coalesced polls never advance the window (same
                // anti-slide rationale as the spectrum gate).
                let mut served = cached;
                served.stale_age_ms = now_ms.saturating_sub(self.last_ok_ms);
                return Ok(StatePoll::Coalesced(served));
            }
        }
        if self.last.is_none() {
            // Pre-init ceiling (bitty#2062): failed reads advance the
            // gate so a silent monitor is re-probed at the ceiling rate,
            // never hammered per tick. The reader stays uninvoked inside
            // the window; the poll hides without monitor work.
            if let Some(last) = self.pre_init_last_ms {
                if now_ms.saturating_sub(last) < STATE_MIN_INTERVAL_MS {
                    return Err(AudioError::unavailable(
                        "audio state polling too fast (pre-init ceiling)",
                    ));
                }
            }
        }
        match read() {
            Ok(snapshot) => self.accept_at(now_ms, now_ms, snapshot),
            Err(error) => self.failed_read(now_ms, Some(error)),
        }
    }

    /// Serve one `get()` from a snapshot produced outside this call.
    ///
    /// The live runtime reads `pactl` off the executor thread (bitty#2425:
    /// three blocking spawns inside `init.lua` exceeded the RC-1 50 ms
    /// activation slice) and hands each completed attempt here. `sampled_at_ms`
    /// is the host-clock instant when the reader **completed**, so the
    /// reported `stale_age_ms` and the [`STATE_STALE_HIDE_MS`] grace are
    /// measured from the sample, not from this call: a sample that landed
    /// 1.9 s before the poll serves with `stale_age_ms = 1900`, and a sample
    /// that landed more than the grace ago hides even if the poller was
    /// silent in between.
    ///
    /// Freshness, validation, and the hide decision are otherwise exactly
    /// [`Self::poll`]'s: a validated snapshot becomes last-good and serves
    /// [`StatePoll::Fresh`], a failed attempt ages the previous snapshot or
    /// hides. The 0.5 Hz ceiling stays with the caller, which schedules the
    /// next attempt at [`STATE_MIN_INTERVAL_MS`]; this method never reads.
    pub fn observe(
        &mut self,
        now_ms: u64,
        sampled_at_ms: u64,
        sample: Result<AudioState, AudioError>,
    ) -> Result<StatePoll, AudioError> {
        match sample {
            Ok(snapshot) => self.accept_at(now_ms, sampled_at_ms, snapshot),
            Err(error) => self.failed_read(now_ms, Some(error)),
        }
    }

    /// Serve one `get()` from last-good without running a reader.
    ///
    /// Used while an off-thread sample is in flight (bitty#2425): the
    /// snapshot ages exactly as it does after a failed read — stale inside
    /// [`STATE_STALE_HIDE_MS`], hidden past it — measured from the instant
    /// the snapshot landed, not from this call. A gate that never landed a
    /// snapshot hides. No poll window, pre-init ceiling, or last-good state
    /// is advanced here, so an in-flight sample still owns the next read
    /// slot.
    pub fn serve_cached(&mut self, now_ms: u64) -> Result<StatePoll, AudioError> {
        if let Some(cached) = self.last.clone() {
            let age_ms = now_ms.saturating_sub(self.last_ok_ms);
            if age_ms < STATE_STALE_HIDE_MS {
                let mut served = cached;
                served.stale_age_ms = age_ms;
                return Ok(StatePoll::Stale(served));
            }
            return Err(AudioError::hidden(format!(
                "audio state stale for {age_ms} ms (hide, never fabricate)"
            )));
        }
        Err(AudioError::hidden(
            "audio state not sampled yet (hide, never fabricate)",
        ))
    }

    /// Store one validated snapshot as last-good and serve it.
    ///
    /// Shared by the inline reader path (where the read completed inside the
    /// call, so both instants match) and [`Self::observe`] (where the read
    /// completed off-thread earlier). The snapshot is re-checked here so a
    /// malformed value never reaches the bar even when a future reader
    /// bypasses [`AudioState::validate`]. `last_ok_ms` is the landing instant,
    /// so every later age/grace computation reflects the sample.
    ///
    /// A sample that is already past [`STATE_STALE_HIDE_MS`] when it is first
    /// observed still becomes last-good (the value is real), but this poll
    /// hides: "hide, never fabricate" applies to a slow poller exactly as it
    /// does to a silent monitor.
    fn accept_at(
        &mut self,
        now_ms: u64,
        sampled_at_ms: u64,
        snapshot: AudioState,
    ) -> Result<StatePoll, AudioError> {
        if let Err(check) = Self::check_snapshot(&snapshot) {
            return self.failed_read(now_ms, Some(check));
        }
        let landed_ms = sampled_at_ms.min(now_ms);
        let age_ms = now_ms.saturating_sub(landed_ms);
        let mut stored = snapshot;
        stored.stale_age_ms = age_ms;
        self.last = Some(stored.clone());
        self.last_ok_ms = landed_ms;
        self.last_poll_ms = now_ms;
        self.pre_init_last_ms = None;
        if age_ms >= STATE_STALE_HIDE_MS {
            return Err(AudioError::hidden(format!(
                "audio state stale for {age_ms} ms (hide, never fabricate)"
            )));
        }
        Ok(StatePoll::Fresh(stored))
    }

    /// Re-validate one reader snapshot (shape the bar may rely on).
    fn check_snapshot(snapshot: &AudioState) -> Result<(), AudioError> {
        if snapshot.volume_pct > VOLUME_MAX_PCT {
            return Err(AudioError::budget(format!(
                "volume {} exceeds {VOLUME_MAX_PCT} (snapshot rejected)",
                snapshot.volume_pct
            )));
        }
        if let Some(source) = snapshot.source_volume_pct {
            if source > VOLUME_MAX_PCT {
                return Err(AudioError::budget(format!(
                    "source volume {source} exceeds {VOLUME_MAX_PCT} (snapshot rejected)"
                )));
            }
        }
        validate_port(&snapshot.port)
    }

    /// Failed-read path: stale inside the grace, hide past it, raw error
    /// before the first landed snapshot. Attempted outcomes advance the
    /// window so a silent monitor is re-probed at the ceiling rate.
    fn failed_read(
        &mut self,
        now_ms: u64,
        error: Option<AudioError>,
    ) -> Result<StatePoll, AudioError> {
        if let Some(cached) = self.last.clone() {
            let age_ms = now_ms.saturating_sub(self.last_ok_ms);
            self.last_poll_ms = now_ms;
            if age_ms < STATE_STALE_HIDE_MS {
                let mut served = cached;
                served.stale_age_ms = age_ms;
                return Ok(StatePoll::Stale(served));
            }
            return Err(AudioError::hidden(format!(
                "audio state stale for {age_ms} ms (hide, never fabricate)"
            )));
        }
        self.pre_init_last_ms = Some(now_ms);
        self.last_poll_ms = now_ms;
        Err(error.unwrap_or_else(|| AudioError::hidden("audio state never read (hide)")))
    }
}

// ── control ───────────────────────────────────────────────────────────────

/// Volume target for `set_volume` (bitty#2013 control head).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeTarget {
    /// Relative step in percent (-10..=+10; over-bound denied).
    Delta(i32),
    /// Absolute park in percent (0..=100).
    Absolute(u8),
}

impl VolumeTarget {
    /// Bounded relative step (over-bound steps denied, never clamped).
    pub fn delta(steps_pct: i32) -> Result<Self, AudioError> {
        if steps_pct.abs() > VOLUME_STEP_MAX_PCT {
            return Err(AudioError::budget(format!(
                "volume step {steps_pct} exceeds +/-{VOLUME_STEP_MAX_PCT} (denied, never clamped)"
            )));
        }
        Ok(Self::Delta(steps_pct))
    }

    /// Absolute park (over-ceiling parks denied, never clamped).
    pub fn absolute(volume_pct: u8) -> Result<Self, AudioError> {
        if volume_pct > VOLUME_MAX_PCT {
            return Err(AudioError::budget(format!(
                "volume park {volume_pct} exceeds {VOLUME_MAX_PCT} (denied, never clamped)"
            )));
        }
        Ok(Self::Absolute(volume_pct))
    }

    /// `pactl` volume operand (`+10%`, `-5%`, `50%`): integer-formatted,
    /// never caller text, so no shell metacharacter can cross.
    #[must_use]
    pub fn operand(&self) -> String {
        match *self {
            Self::Delta(steps) if steps >= 0 => format!("+{steps}%"),
            Self::Delta(steps) => format!("{steps}%"),
            Self::Absolute(volume) => format!("{volume}%"),
        }
    }
}

/// Scoped monitor binary (the only spawned program; `argv[0]` is fixed).
pub const MONITOR_PROGRAM: &str = "pactl";

/// Logical default-sink selector (PulseAudio/PipeWire name, never a path).
pub const DEFAULT_SINK: &str = "@DEFAULT_SINK@";

/// Fixed argv reading the sink volume (no shell, no caller text).
#[must_use]
pub fn read_volume_argv() -> Vec<String> {
    vec![
        MONITOR_PROGRAM.to_string(),
        "get-sink-volume".to_string(),
        DEFAULT_SINK.to_string(),
    ]
}

/// Fixed argv reading the sink mute flag.
#[must_use]
pub fn read_mute_argv() -> Vec<String> {
    vec![
        MONITOR_PROGRAM.to_string(),
        "get-sink-mute".to_string(),
        DEFAULT_SINK.to_string(),
    ]
}

/// Fixed argv reading the default sink name.
#[must_use]
pub fn read_default_sink_argv() -> Vec<String> {
    vec![MONITOR_PROGRAM.to_string(), "get-default-sink".to_string()]
}

/// Fixed argv listing sinks (bounded scan for the active port).
#[must_use]
pub fn list_sinks_argv() -> Vec<String> {
    vec![
        MONITOR_PROGRAM.to_string(),
        "list".to_string(),
        "sinks".to_string(),
    ]
}

/// Argv stepping or parking the sink volume (operand is integer-shaped).
#[must_use]
pub fn set_volume_argv(target: VolumeTarget) -> Vec<String> {
    vec![
        MONITOR_PROGRAM.to_string(),
        "set-sink-volume".to_string(),
        DEFAULT_SINK.to_string(),
        target.operand(),
    ]
}

/// Argv toggling the sink mute flag (operand is a fixed literal).
#[must_use]
pub fn toggle_mute_argv() -> Vec<String> {
    vec![
        MONITOR_PROGRAM.to_string(),
        "set-sink-mute".to_string(),
        DEFAULT_SINK.to_string(),
        "toggle".to_string(),
    ]
}

/// Byte length of one argv vector (control budget accounting).
#[must_use]
pub fn argv_bytes(argv: &[String]) -> usize {
    argv.iter().map(|arg| arg.len()).sum()
}

/// Assert one argv vector obeys the argv discipline: fixed program, no
/// shell metacharacters, under the byte cap.
pub fn check_argv(argv: &[String]) -> Result<(), AudioError> {
    let Some(program) = argv.first() else {
        return Err(AudioError::invalid("argv is empty (rejected)"));
    };
    if program != MONITOR_PROGRAM {
        return Err(AudioError::invalid(format!(
            "argv program '{program}' outside the scoped monitor (rejected)"
        )));
    }
    if argv_bytes(argv) > CONTROL_MAX_ARGV_BYTES {
        return Err(AudioError::budget(format!(
            "argv {} bytes exceeds {CONTROL_MAX_ARGV_BYTES} (rejected)",
            argv_bytes(argv)
        )));
    }
    let hostile = argv.iter().any(|arg| {
        arg.bytes().any(|byte| {
            matches!(
                byte,
                b';' | b'|' | b'&' | b'$' | b'`' | b'\\' | b'\'' | b'"' | b'\n' | b'\r' | b'\0'
            )
        })
    });
    if hostile {
        return Err(AudioError::invalid(
            "argv carries shell metacharacters (rejected)",
        ));
    }
    Ok(())
}

/// Parse `pactl get-sink-volume` output to a percent.
///
/// Accepts the first `<n>%` token (`Volume: front-left: 32768 /  50% / ...`);
/// values above [`VOLUME_MAX_PCT`] saturate (documented); over-cap or
/// token-less output fails closed.
pub fn parse_sink_volume(output: &str) -> Result<u8, AudioError> {
    if output.len() > MONITOR_OUTPUT_MAX_BYTES {
        return Err(AudioError::budget(format!(
            "monitor output {} bytes exceeds {MONITOR_OUTPUT_MAX_BYTES} (rejected)",
            output.len()
        )));
    }
    let token = output
        .split_whitespace()
        .find(|word| word.ends_with('%'))
        .ok_or_else(|| AudioError::invalid("sink volume has no percent token (rejected)"))?;
    let digits = token.trim_end_matches('%');
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(AudioError::invalid(
            "sink volume percent token is not digits (rejected)",
        ));
    }
    let value: u32 = digits
        .parse()
        .map_err(|_| AudioError::invalid("sink volume percent overflows (rejected)"))?;
    Ok(value.min(u32::from(VOLUME_MAX_PCT)) as u8)
}

/// Parse `pactl get-sink-mute` output (`Mute: yes` / `Mute: no`).
pub fn parse_sink_mute(output: &str) -> Result<bool, AudioError> {
    if output.len() > MONITOR_OUTPUT_MAX_BYTES {
        return Err(AudioError::budget(format!(
            "monitor output {} bytes exceeds {MONITOR_OUTPUT_MAX_BYTES} (rejected)",
            output.len()
        )));
    }
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(flag) = trimmed.strip_prefix("Mute:") {
            return match flag.trim() {
                "yes" => Ok(true),
                "no" => Ok(false),
                _ => Err(AudioError::invalid(
                    "sink mute flag is neither yes nor no (rejected)",
                )),
            };
        }
    }
    Err(AudioError::invalid(
        "sink mute output names no Mute line (rejected)",
    ))
}

/// Parse `pactl get-default-sink` output to a bounded sink name.
pub fn parse_default_sink(output: &str) -> Result<String, AudioError> {
    if output.len() > MONITOR_OUTPUT_MAX_BYTES {
        return Err(AudioError::budget(format!(
            "monitor output {} bytes exceeds {MONITOR_OUTPUT_MAX_BYTES} (rejected)",
            output.len()
        )));
    }
    let name = output.trim();
    if name.is_empty() || name.len() > AUDIO_MAX_SINK_BYTES {
        return Err(AudioError::budget(format!(
            "sink name {} bytes out of 1..={AUDIO_MAX_SINK_BYTES} (rejected)",
            name.len()
        )));
    }
    let ok = name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'));
    if !ok {
        return Err(AudioError::invalid(
            "sink name carries outside [A-Za-z0-9.\\-_+] (rejected)",
        ));
    }
    Ok(name.to_string())
}

/// Parse the active port from `pactl list sinks` output.
///
/// Scans the bounded output for the first `Active Port:` line and returns
/// its validated label; anything else fails closed (the provider returns
/// port names only; icon selection stays Lua-side).
pub fn parse_active_port(output: &str) -> Result<String, AudioError> {
    if output.len() > MONITOR_OUTPUT_MAX_BYTES {
        return Err(AudioError::budget(format!(
            "monitor output {} bytes exceeds {MONITOR_OUTPUT_MAX_BYTES} (rejected)",
            output.len()
        )));
    }
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(port) = trimmed.strip_prefix("Active Port:") {
            let label = port.trim();
            validate_port(label).map_err(|_| {
                AudioError::invalid("active port label fails validation (rejected)")
            })?;
            return Ok(label.to_string());
        }
    }
    Err(AudioError::invalid(
        "sink list names no Active Port line (rejected)",
    ))
}

/// Per-call control budget plus wheel-burst coalescing.
///
/// Admits at most one control call per [`CONTROL_MIN_INTERVAL_MS`];
/// between admissions, relative deltas accumulate with a saturating sum
/// capped at one step (mirroring the `on_scroll` burst discipline from
/// bitty#2054), so a 100-tick wheel burst drains as one bounded step and
/// never overruns the VM budget.
#[derive(Debug, Clone, Default)]
pub struct ControlGate {
    last_ms: u64,
    admitted: bool,
    pending_delta: i32,
}

impl ControlGate {
    /// Empty gate (first call admitted immediately).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a control call is admitted at tick `now_ms`.
    pub fn admit(&mut self, now_ms: u64) -> bool {
        if !self.admitted || now_ms.saturating_sub(self.last_ms) >= CONTROL_MIN_INTERVAL_MS {
            self.last_ms = now_ms;
            self.admitted = true;
            return true;
        }
        false
    }

    /// Fold one wheel tick into the pending step (saturating at one step).
    pub fn push_delta(&mut self, delta_pct: i32) {
        self.pending_delta = self
            .pending_delta
            .saturating_add(delta_pct)
            .clamp(-VOLUME_STEP_MAX_PCT, VOLUME_STEP_MAX_PCT);
    }

    /// Pending coalesced step (zero when no burst accumulated).
    #[must_use]
    pub const fn pending_delta(&self) -> i32 {
        self.pending_delta
    }

    /// Drain the pending step into one bounded [`VolumeTarget`].
    ///
    /// Returns `None` when nothing accumulated. The saturating
    /// accumulation keeps every drain inside [`VOLUME_STEP_MAX_PCT`], so
    /// the drain always fits one admitted call.
    pub fn take_pending(&mut self) -> Option<VolumeTarget> {
        if self.pending_delta == 0 {
            return None;
        }
        let delta = std::mem::replace(&mut self.pending_delta, 0);
        VolumeTarget::delta(delta).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Count sampler invocations across polls.
    struct Counter {
        calls: usize,
        taps: Vec<f32>,
        fail: bool,
    }

    impl Counter {
        fn sample(&mut self) -> Result<Vec<f32>, AudioError> {
            self.calls += 1;
            if self.fail {
                return Err(AudioError::unavailable("monitor gone"));
            }
            Ok(self.taps.clone())
        }
    }

    // ── identities ────────────────────────────────────────────────────

    #[test]
    fn interface_identities_are_stable() {
        assert_eq!(AUDIO_FAMILY, "audio");
        assert_eq!(SPECTRUM_IFACE, "audio.spectrum");
        assert_eq!(SPECTRUM_VERSION, "1.0.0");
        assert_eq!(SPECTRUM_METHOD, "spectrum");
        assert_eq!(STATE_IFACE, "audio.state");
        assert_eq!(STATE_VERSION, "1.0.0");
        assert_eq!(STATE_METHOD_GET, "get");
        assert_eq!(CONTROL_IFACE, "audio.control");
        assert_eq!(CONTROL_VERSION, "1.0.0");
        assert_eq!(CONTROL_METHOD_SET_VOLUME, "set_volume");
        assert_eq!(CONTROL_METHOD_TOGGLE_MUTE, "toggle_mute");
        assert_eq!(SPECTRUM_HEAD, "audio.spectrum");
        assert_eq!(STATE_READ_HEAD, "audio.state.read");
        assert_eq!(STATE_CONTROL_HEAD, "audio.state.control");
    }

    #[test]
    fn heads_parse_as_closed_capabilities() {
        use crate::capability::CapabilityId;
        for head in [SPECTRUM_HEAD, STATE_READ_HEAD, STATE_CONTROL_HEAD] {
            let granted = CapabilityId::parse(head).expect("audio head parses");
            assert_eq!(granted.family().as_str(), AUDIO_FAMILY);
            assert!(!granted.has_param());
        }
    }

    // ── spectrum gate ─────────────────────────────────────────────────

    fn taps(count: usize, value: f32) -> Vec<f32> {
        vec![value; count]
    }

    #[test]
    fn spectrum_poll_runs_sampler_once_per_window() {
        let mut gate = SpectrumGate::new();
        let mut sampler = Counter {
            calls: 0,
            taps: taps(8, 0.5),
            fail: false,
        };
        let first = gate.poll(1000, 4, || sampler.sample()).expect("serves");
        assert!(matches!(first, SpectrumPoll::Fresh { .. }));
        assert_eq!(first.levels(), &[0.5, 0.5, 0.5, 0.5]);
        assert_eq!(sampler.calls, 1);
        // 50 ms later: inside the 200 ms ceiling the sampler does NOT run.
        let second = gate.poll(1050, 4, || sampler.sample()).expect("coalesces");
        assert!(matches!(second, SpectrumPoll::Coalesced { .. }));
        assert_eq!(second.levels(), first.levels());
        assert_eq!(sampler.calls, 1, "over-poll performs no monitor work");
        // Past the ceiling: the sampler runs exactly once more.
        let third = gate
            .poll(1300, 4, || sampler.sample())
            .expect("serves fresh");
        assert!(matches!(third, SpectrumPoll::Fresh { .. }));
        assert_eq!(sampler.calls, 2);
    }

    #[test]
    fn spectrum_coalesced_polls_never_slide_the_window() {
        // Sustained 60 fps over-poll must still sample at the next
        // window: coalesced outcomes never advance the sampler clock.
        let mut gate = SpectrumGate::new();
        let mut sampler = Counter {
            calls: 0,
            taps: taps(4, 0.5),
            fail: false,
        };
        gate.poll(0, 4, || sampler.sample()).expect("lands");
        // Ticks 1..=12 (16..192 ms) stay inside the 200 ms window.
        for tick in 1..13_u64 {
            let poll = gate
                .poll(tick.saturating_mul(16), 4, || sampler.sample())
                .expect("coalesces");
            assert!(
                matches!(poll, SpectrumPoll::Coalesced { .. }),
                "tick {tick}"
            );
        }
        assert_eq!(sampler.calls, 1);
        let fresh = gate
            .poll(13 * 16, 4, || sampler.sample())
            .expect("window samples");
        assert!(matches!(fresh, SpectrumPoll::Fresh { .. }));
        assert!(matches!(fresh, SpectrumPoll::Fresh { .. }));
        assert_eq!(sampler.calls, 2);
    }

    #[test]
    fn spectrum_poll_rejects_bucket_overflow_without_sampling() {
        let mut gate = SpectrumGate::new();
        let mut calls = 0_usize;
        let error = gate
            .poll(0, SPECTRUM_MAX_BUCKETS + 1, || {
                calls += 1;
                Ok(taps(4, 0.5))
            })
            .expect_err("33 buckets denied");
        assert_eq!(error.kind(), AudioErrorKind::Budget);
        assert_eq!(calls, 0, "rejected polls never sample");
        let error = gate
            .poll(0, 0, || {
                calls += 1;
                Ok(taps(4, 0.5))
            })
            .expect_err("0 buckets denied");
        assert_eq!(error.kind(), AudioErrorKind::Budget);
        assert_eq!(calls, 0);
    }

    #[test]
    fn spectrum_hides_before_first_sample_and_after_grace() {
        let mut gate = SpectrumGate::new();
        let error = gate
            .poll(0, 4, || Err(AudioError::unavailable("no backend")))
            .expect_err("never sampled hides");
        assert_eq!(error.kind(), AudioErrorKind::MonitorUnavailable);
        // First good sample lands.
        let fresh = gate
            .poll(1000, 4, || Ok(taps(4, 0.25)))
            .expect("sample lands");
        assert!(matches!(fresh, SpectrumPoll::Fresh { .. }));
        // Sampler fails inside the grace: last-good with age.
        let stale = gate
            .poll(1600, 4, || Err(AudioError::unavailable("gone")))
            .expect("grace serves stale");
        assert!(matches!(stale, SpectrumPoll::Stale { age_ms: 600, .. }));
        assert_eq!(stale.levels(), &[0.25; 4]);
        // Past the grace: hide, never fabricate.
        let error = gate
            .poll(
                1000 + SPECTRUM_STALE_HIDE_MS + SPECTRUM_MIN_INTERVAL_MS,
                4,
                || Err(AudioError::unavailable("gone")),
            )
            .expect_err("grace exhausted hides");
        assert_eq!(error.kind(), AudioErrorKind::Hidden);
        // Recovery: a new sample clears loss.
        let fresh = gate
            .poll(5000, 4, || Ok(taps(4, 0.75)))
            .expect("recovery serves");
        assert!(matches!(fresh, SpectrumPoll::Fresh { .. }));
        assert_eq!(fresh.levels(), &[0.75; 4]);
    }

    #[test]
    fn spectrum_pre_init_failures_stay_within_ceiling() {
        // Regression for bitty#2062: repeated sub-window pre-init
        // failures must not hammer the sampler; the ceiling bounds
        // attempts even before the first sample lands.
        let mut gate = SpectrumGate::new();
        let mut calls = 0_usize;
        for tick in 0..50_u64 {
            let now_ms = tick.saturating_mul(10);
            let _ = gate.poll(now_ms, 4, || {
                calls += 1;
                Err(AudioError::unavailable("monitor gone"))
            });
        }
        assert!(
            calls <= 4,
            "pre-init failures hammer the sampler: {calls} runs for 50 sub-window polls"
        );
        let fresh = gate
            .poll(1000, 4, || Ok(taps(4, 0.5)))
            .expect("recovers after window");
        assert!(matches!(fresh, SpectrumPoll::Fresh { .. }));
    }

    #[test]
    fn spectrum_pre_init_bad_samples_stay_within_ceiling() {
        // Same ceiling for corrupt pre-init samples: the check failure
        // path advances the gate instead of retrying every tick.
        let mut gate = SpectrumGate::new();
        let mut calls = 0_usize;
        for tick in 0..50_u64 {
            let now_ms = tick.saturating_mul(10);
            let _ = gate.poll(now_ms, 4, || {
                calls += 1;
                Ok(Vec::new())
            });
        }
        assert!(
            calls <= 4,
            "pre-init corrupt samples hammer the sampler: {calls} runs for 50 sub-window polls"
        );
    }

    #[test]
    fn spectrum_rejects_bad_samples_without_displacing_last_good() {
        // Bootstrap with a corrupt sample and no history: the check error
        // surfaces (hide), never an empty render.
        let mut bootstrap = SpectrumGate::new();
        assert!(bootstrap.poll(0, 2, || Ok(Vec::new())).is_err());
        assert!(bootstrap.poll(0, 2, || Ok(vec![f32::NAN])).is_err());

        let mut gate = SpectrumGate::new();
        gate.poll(0, 2, || Ok(vec![0.0, 1.0])).expect("lands");
        // Empty, over-count, and non-finite samples reject into stale
        // last-good inside the grace (contained, not fatal).
        let stale = gate
            .poll(500, 2, || Ok(Vec::new()))
            .expect("empty sample keeps last-good");
        assert!(matches!(stale, SpectrumPoll::Stale { .. }));
        assert_eq!(stale.levels(), &[0.0, 1.0]);
        let stale = gate
            .poll(1000, 2, || Ok(taps(SPECTRUM_MAX_TAPS + 1, 0.5)))
            .expect("over-count keeps last-good");
        assert!(matches!(stale, SpectrumPoll::Stale { .. }));
        // Before the grace lapses a rejected sample serves stale
        // last-good (the failure is contained, not fatal).
        let stale = gate
            .poll(1500, 2, || Ok(vec![f32::NAN, 0.5]))
            .expect("bad sample keeps last-good");
        assert!(matches!(stale, SpectrumPoll::Stale { .. }));
        assert_eq!(stale.levels(), &[0.0, 1.0]);
        // Out-of-range clamps without rejecting the sample.
        let fresh = gate
            .poll(2000, 2, || Ok(vec![-1.0, 2.0]))
            .expect("clamp lands");
        assert_eq!(fresh.levels(), &[0.0, 1.0]);
    }

    #[test]
    fn resample_covers_edges() {
        assert_eq!(resample_levels(&[0.1, 0.2, 0.3, 0.4], 2), vec![0.1, 0.3]);
        assert_eq!(resample_levels(&[0.5], 3), vec![0.5, 0.5, 0.5]);
        assert_eq!(resample_levels(&[], 4), Vec::<f32>::new());
        assert_eq!(resample_levels(&[0.5], 0), Vec::<f32>::new());
        let full: Vec<f32> = (0..SPECTRUM_MAX_TAPS).map(|tap| tap as f32).collect();
        let down = resample_levels(&full, SPECTRUM_MAX_BUCKETS);
        assert_eq!(down.len(), SPECTRUM_MAX_BUCKETS);
        assert!(down.iter().all(|level| level.is_finite()));
    }

    #[test]
    fn spectrum_response_stays_under_byte_cap() {
        for buckets in 1..=SPECTRUM_MAX_BUCKETS {
            assert!(
                spectrum_response_bytes(buckets) <= SPECTRUM_MAX_RESPONSE_BYTES,
                "buckets={buckets}"
            );
            assert!(spectrum_poll_value_count(buckets) <= SPECTRUM_MAX_BUCKETS + 1);
        }
    }

    #[test]
    fn spectrum_poll_stays_within_stated_fraction_of_rc1() {
        // Tick-budget proof (bitty#2009 acceptance: measured, not
        // asserted). Worst case per poll: 64-tap resample to 32 buckets
        // plus one table. RC-1 allows 50 ms wall per execute; 10 000
        // staggered worst-case polls must fit in 1 000 ms (100 us mean:
        // 0.2% of one RC-1 budget even if a tick polled every frame).
        use crate::event::RC1_WALL_CLOCK_BUDGET_MS;
        let wall_budget_ms: u64 = RC1_WALL_CLOCK_BUDGET_MS;
        assert!(wall_budget_ms >= 50);
        let full: Vec<f32> = (0..SPECTRUM_MAX_TAPS)
            .map(|tap| (tap as f32) / (SPECTRUM_MAX_TAPS as f32))
            .collect();
        let mut gate = SpectrumGate::new();
        let start = std::time::Instant::now();
        let mut served = 0_usize;
        for round in 0..10_000_u64 {
            let buckets = 1 + (round % (SPECTRUM_MAX_BUCKETS as u64)) as usize;
            // Stagger ticks past the ceiling so every poll samples.
            let tick = round.saturating_mul(SPECTRUM_MIN_INTERVAL_MS);
            let poll = gate
                .poll(tick, buckets, || Ok(full.clone()))
                .expect("staggered polls serve");
            served += poll.levels().len();
        }
        let elapsed_ms = start.elapsed().as_millis();
        // Deterministic lattice: 312 full 1..=32 cycles (528 each) plus
        // 1..=16 (136).
        assert_eq!(served, 312 * 528 + 136);
        assert!(
            elapsed_ms < 1_000,
            "10 000 worst-case polls took {elapsed_ms} ms (budget 1 000 ms)"
        );
    }

    // ── state gate ────────────────────────────────────────────────────

    fn state(volume: u8) -> AudioState {
        AudioState::validate(volume, false, "analog-output-speaker", None, None)
            .expect("fixture state")
    }

    #[test]
    fn state_poll_runs_reader_once_per_window() {
        let mut gate = StateGate::new();
        let mut reads = 0_usize;
        let fresh = gate
            .poll(1000, || {
                reads += 1;
                Ok(state(50))
            })
            .expect("read serves");
        assert!(matches!(fresh, StatePoll::Fresh(_)));
        assert_eq!(reads, 1);
        // 500 ms later: inside the 2 s ceiling the reader does NOT run.
        let coalesced = gate
            .poll(1500, || {
                reads += 1;
                Ok(state(99))
            })
            .expect("over-poll coalesces");
        assert!(matches!(coalesced, StatePoll::Coalesced(_)));
        assert_eq!(coalesced.state().volume_pct, 50);
        assert_eq!(coalesced.state().stale_age_ms, 500);
        assert_eq!(reads, 1, "over-poll performs no monitor work");
        // Past the ceiling: the reader runs again.
        let fresh = gate
            .poll(4000, || {
                reads += 1;
                Ok(state(60))
            })
            .expect("window elapsed reads");
        assert!(matches!(fresh, StatePoll::Fresh(_)));
        assert_eq!(reads, 2);
    }

    #[test]
    fn state_poll_stales_then_hides_on_silence() {
        let mut gate = StateGate::new();
        assert!(
            gate.poll(0, || Err(AudioError::unavailable("none")))
                .is_err(),
            "never read hides"
        );
        // Pre-init ceiling (bitty#2062): the landing read waits a full
        // window after the pre-init failure so the test honors the same
        // bound the regression test pins.
        gate.poll(STATE_MIN_INTERVAL_MS, || Ok(state(50)))
            .expect("lands");
        let stale = gate
            .poll(4000, || Err(AudioError::unavailable("gone")))
            .expect("silence stales");
        assert!(matches!(stale, StatePoll::Stale(_)));
        assert_eq!(stale.state().stale_age_ms, 4000 - STATE_MIN_INTERVAL_MS);
        assert_eq!(
            gate.poll(
                STATE_MIN_INTERVAL_MS + STATE_STALE_HIDE_MS + STATE_MIN_INTERVAL_MS,
                || Err(AudioError::unavailable("gone"))
            )
            .expect_err("grace exhausted hides")
            .kind(),
            AudioErrorKind::Hidden
        );
    }

    #[test]
    fn state_pre_init_failures_stay_within_ceiling() {
        // Regression for bitty#2062: repeated sub-window pre-init
        // failures must not hammer the reader; the ceiling bounds
        // attempts even before the first snapshot lands.
        let mut gate = StateGate::new();
        let mut reads = 0_usize;
        for tick in 0..20_u64 {
            let now_ms = tick.saturating_mul(100);
            let _ = gate.poll(now_ms, || {
                reads += 1;
                Err(AudioError::unavailable("monitor gone"))
            });
        }
        assert!(
            reads <= 2,
            "pre-init failures hammer the reader: {reads} runs for 20 sub-window polls"
        );
        let fresh = gate
            .poll(4000, || Ok(state(50)))
            .expect("recovers after window");
        assert!(matches!(fresh, StatePoll::Fresh(_)));
    }

    #[test]
    fn state_observe_serves_fresh_and_ages_through_serve_cached() {
        // bitty#2425: the live runtime reads pactl off the executor thread
        // and hands completed attempts to `observe`. Freshness, the grace,
        // and the hide decision must match the inline `poll` path.
        let mut gate = StateGate::new();
        assert!(
            gate.serve_cached(0).is_err(),
            "a gate that never observed a sample hides"
        );
        let fresh = gate
            .observe(1000, 1000, Ok(state(42)))
            .expect("sample serves");
        assert!(matches!(fresh, StatePoll::Fresh(_)));
        assert_eq!(fresh.state().volume_pct, 42);
        assert_eq!(fresh.state().stale_age_ms, 0);
        // No new attempt: the last-good snapshot ages without a reader.
        let aged = gate.serve_cached(3000).expect("serves inside the grace");
        assert!(matches!(aged, StatePoll::Stale(_)));
        assert_eq!(aged.state().volume_pct, 42);
        assert_eq!(aged.state().stale_age_ms, 2000);
        // A failed attempt ages the same snapshot, never fabricates.
        let stale = gate
            .observe(5000, 5000, Err(AudioError::unavailable("monitor gone")))
            .expect("failure inside the grace serves stale");
        assert!(matches!(stale, StatePoll::Stale(_)));
        assert_eq!(stale.state().volume_pct, 42);
        assert_eq!(stale.state().stale_age_ms, 4000);
    }

    #[test]
    fn state_observe_hides_past_the_grace_and_rejects_malformed_samples() {
        let mut gate = StateGate::new();
        gate.observe(0, 0, Ok(state(50))).expect("lands");
        assert_eq!(
            gate.serve_cached(STATE_STALE_HIDE_MS)
                .expect_err("grace exhausted hides")
                .kind(),
            AudioErrorKind::Hidden
        );
        // A malformed off-thread sample takes the failed-read path.
        let mut gate = StateGate::new();
        let mut bad = state(50);
        bad.volume_pct = 101;
        assert!(
            gate.observe(0, 0, Ok(bad)).is_err(),
            "malformed sample never becomes last-good"
        );
        assert!(
            gate.serve_cached(1).is_err(),
            "nothing landed, so nothing serves"
        );
    }

    #[test]
    fn state_observe_measures_age_and_grace_from_the_landing_instant() {
        // bitty#2425 review: an off-thread sample that landed 1.9 s before
        // the poll must report that age, and the 10 s grace must run from the
        // landing instant, not from whenever the plugin happened to poll.
        let mut gate = StateGate::new();
        let fresh = gate
            .observe(1900, 0, Ok(state(42)))
            .expect("landed sample serves");
        assert_eq!(
            fresh.state().stale_age_ms,
            1900,
            "age is measured from the reader completion"
        );
        assert_eq!(fresh.state().volume_pct, 42);
        // A poll inside the grace from landing still serves the sample.
        let aged = gate
            .serve_cached(STATE_STALE_HIDE_MS - 1)
            .expect("inside the grace from landing");
        assert_eq!(aged.state().stale_age_ms, STATE_STALE_HIDE_MS - 1);
        // One millisecond later the grace is exhausted even though the gate
        // only just observed the sample for the first time.
        assert_eq!(
            gate.serve_cached(STATE_STALE_HIDE_MS)
                .expect_err("grace is measured from landing")
                .kind(),
            AudioErrorKind::Hidden
        );
        // A sample first observed at or past the grace stores as last-good
        // but this poll hides (CodeRabbit review: "hide, never fabricate"
        // applies to a slow poller too).
        let mut gate = StateGate::new();
        let hidden = gate
            .observe(STATE_STALE_HIDE_MS, 0, Ok(state(70)))
            .expect_err("an already-expired sample hides");
        assert_eq!(hidden.kind(), AudioErrorKind::Hidden);
        // It is not fabricated later either: the stored sample is past the
        // grace, so cached serves hide from the landing instant.
        assert!(gate.serve_cached(STATE_STALE_HIDE_MS).is_err());
        // A failed attempt after the grace hides as well.
        assert!(
            gate.observe(
                STATE_STALE_HIDE_MS + 1,
                STATE_STALE_HIDE_MS,
                Err(AudioError::unavailable("monitor gone")),
            )
            .is_err(),
            "a late failure cannot resurrect an expired sample"
        );
    }

    #[test]
    fn state_inline_poll_reports_zero_age() {
        // The inline reader completes inside the call, so both instants are
        // the poll instant and the served age stays zero.
        let mut gate = StateGate::new();
        let fresh = gate.poll(700, || Ok(state(50))).expect("inline read");
        assert_eq!(fresh.state().stale_age_ms, 0);
    }

    #[test]
    fn state_serve_cached_never_advances_the_read_window() {
        // bitty#2425: `serve_cached` is a serve-only path. It must not
        // consume the poll window, or an in-flight sampler would lose its
        // slot and the gate could be starved by over-polls.
        let mut gate = StateGate::new();
        gate.observe(0, 0, Ok(state(50))).expect("lands");
        for tick in 1..=100_u64 {
            let _ = gate.serve_cached(tick.saturating_mul(10));
        }
        // An inline poll at the ceiling still runs its reader: the serve
        // path left `last_poll_ms` at the observation time.
        let mut reads = 0_usize;
        let fresh = gate
            .poll(STATE_MIN_INTERVAL_MS, || {
                reads += 1;
                Ok(state(60))
            })
            .expect("ceiling elapsed");
        assert!(matches!(fresh, StatePoll::Fresh(_)));
        assert_eq!(reads, 1);
    }

    #[test]
    fn state_validate_rejects_malformed_snapshots() {
        assert!(AudioState::validate(101, false, "analog-output-speaker", None, None).is_err());
        assert!(AudioState::validate(50, false, "", None, None).is_err());
        assert!(AudioState::validate(50, false, "Analog Output!", None, None).is_err());
        assert!(AudioState::validate(50, false, "analog-output-speaker", Some(101), None).is_err());
        let ok = AudioState::validate(50, true, "analog-output-headphones", Some(70), Some(true))
            .expect("valid snapshot");
        assert_eq!(ok.volume_pct, 50);
        assert!(ok.muted);
        assert_eq!(ok.source_volume_pct, Some(70));
    }

    #[test]
    fn state_snapshot_payload_stays_bounded() {
        // `get()` crosses six fields; the port label is the only heap
        // string and sits under its byte cap.
        let snapshot = state(80);
        assert!(snapshot.port.len() <= AUDIO_MAX_PORT_BYTES);
    }

    // ── control ───────────────────────────────────────────────────────

    #[test]
    fn volume_targets_stay_integer_shaped() {
        assert_eq!(
            VolumeTarget::delta(10).expect("edge step").operand(),
            "+10%"
        );
        assert_eq!(VolumeTarget::delta(-3).expect("step").operand(), "-3%");
        assert_eq!(VolumeTarget::delta(0).expect("zero step").operand(), "+0%");
        assert_eq!(VolumeTarget::absolute(50).expect("park").operand(), "50%");
        assert_eq!(VolumeTarget::absolute(0).expect("floor").operand(), "0%");
        assert_eq!(
            VolumeTarget::absolute(100).expect("ceiling").operand(),
            "100%"
        );
        assert!(VolumeTarget::delta(11).is_err());
        assert!(VolumeTarget::delta(-11).is_err());
        assert!(VolumeTarget::absolute(101).is_err());
    }

    #[test]
    fn control_argv_is_fixed_shape_and_checked() {
        for argv in [
            read_volume_argv(),
            read_mute_argv(),
            read_default_sink_argv(),
            list_sinks_argv(),
            set_volume_argv(VolumeTarget::delta(5).expect("step")),
            set_volume_argv(VolumeTarget::absolute(42).expect("park")),
            toggle_mute_argv(),
        ] {
            check_argv(&argv).expect("fixed argv checks");
            assert!(argv_bytes(&argv) <= CONTROL_MAX_ARGV_BYTES);
        }
        assert_eq!(
            set_volume_argv(VolumeTarget::delta(-5).expect("step")),
            vec!["pactl", "set-sink-volume", "@DEFAULT_SINK@", "-5%"]
        );
        assert_eq!(
            toggle_mute_argv(),
            vec!["pactl", "set-sink-mute", "@DEFAULT_SINK@", "toggle"]
        );
        // No shell, no foreign program, no metacharacters, ever.
        assert!(check_argv(&[]).is_err());
        assert!(check_argv(&["sh".to_string(), "-c".to_string(), "x".to_string()]).is_err());
        assert!(
            check_argv(&[
                "pactl".to_string(),
                "set-sink-volume".to_string(),
                "@DEFAULT_SINK@".to_string(),
                "$(evil)".to_string()
            ])
            .is_err()
        );
    }

    // ── parsers ───────────────────────────────────────────────────────

    #[test]
    fn parsers_accept_fixtures_and_reject_garbage() {
        assert_eq!(
            parse_sink_volume("Volume: front-left: 32768 /  50% / -18.06 dB\n").expect("volume"),
            50
        );
        assert_eq!(
            parse_sink_volume("Volume: mono: 57344 /  88% / -3.28 dB\n").expect("mono"),
            88
        );
        // Over-amplification saturates at the ceiling (documented).
        assert_eq!(
            parse_sink_volume("Volume: mono: 98304 / 150% / 10.54 dB\n").expect("hot"),
            100
        );
        assert!(parse_sink_volume("Volume: mono: mute\n").is_err());
        assert!(parse_sink_volume("").is_err());
        assert!(parse_sink_volume(&"x".repeat(MONITOR_OUTPUT_MAX_BYTES + 1)).is_err());

        assert!(parse_sink_mute("Mute: yes\n").expect("muted"));
        assert!(!parse_sink_mute("Mute: no\n").expect("unmuted"));
        assert!(parse_sink_mute("Mute: maybe\n").is_err());
        assert!(parse_sink_mute("Volume: 50%\n").is_err());

        assert_eq!(
            parse_default_sink("alsa_output.pci-0000_00_1f.3.analog-stereo\n").expect("sink"),
            "alsa_output.pci-0000_00_1f.3.analog-stereo"
        );
        assert!(parse_default_sink("\n").is_err());
        assert!(parse_default_sink("sink; rm -rf\n").is_err());

        let list = "Sink #0\n\tState: RUNNING\n\tActive Port: analog-output-headphones\n";
        assert_eq!(
            parse_active_port(list).expect("port"),
            "analog-output-headphones"
        );
        assert!(parse_active_port("Sink #0\n\tState: RUNNING\n").is_err());
        assert!(parse_active_port("Active Port: Analog Output!\n").is_err());
    }

    // ── control gate ──────────────────────────────────────────────────

    #[test]
    fn control_gate_admits_and_coalesces_bursts() {
        let mut gate = ControlGate::new();
        assert!(gate.admit(0), "first call admitted");
        assert!(!gate.admit(50), "same window denied");
        assert!(gate.admit(CONTROL_MIN_INTERVAL_MS), "next window admitted");
        // 100-tick wheel burst collapses to one bounded step.
        for _ in 0..100 {
            gate.push_delta(1);
        }
        assert_eq!(gate.pending_delta(), VOLUME_STEP_MAX_PCT);
        let target = gate.take_pending().expect("burst drains");
        assert_eq!(target, VolumeTarget::Delta(VOLUME_STEP_MAX_PCT));
        assert_eq!(gate.take_pending(), None);
        // Bounded bursts drain to one argv-shaped step.
        gate.push_delta(4);
        gate.push_delta(3);
        let target = gate.take_pending().expect("drain");
        assert_eq!(target, VolumeTarget::Delta(7));
        assert_eq!(gate.take_pending(), None);
        // Opposite ticks cancel.
        gate.push_delta(5);
        gate.push_delta(-5);
        assert_eq!(gate.take_pending(), None);
    }
}
