//! Live PulseAudio/PipeWire monitor binding plus Lua bridge for the audio
//! provider family (CTX-1200, provider side of bitty#2013, on top of the
//! #2058 contract in `bitty-plugin-host::audio`).
//!
//! # Scope
//!
//! This module plugs I/O behind the injectable seam: the
//! [`SpectrumGate`](bitty_plugin_host::SpectrumGate) sampler and
//! [`StateGate`](bitty_plugin_host::StateGate) reader closures. Gates,
//! argv shapes, parsers, byte caps, and capability heads stay in
//! `bitty-plugin-host::audio` (unchanged here, so the #2063 pre-init fix
//! rebases cleanly); this module owns only the production spawner, the
//! live reader composition, the gate-holding monitor, the Lua value shapes,
//! and the native publication descriptors.
//!
//! # Live sources
//!
//! - State (`get`): three `pactl` spawns per fresh read —
//!   [`read_volume_argv`](bitty_plugin_host::read_volume_argv),
//!   [`read_mute_argv`](bitty_plugin_host::read_mute_argv),
//!   [`list_sinks_argv`](bitty_plugin_host::list_sinks_argv) — composed by
//!   [`live_read`] and parsed by the bounded contract parsers. The default
//!   sink name parse exists in the contract but carries no `get()` field,
//!   so it is not spawned here.
//! - Source (microphone) `source_volume_pct` / `source_muted`: no scoped
//!   argv shape covers `@DEFAULT_SOURCE@` in this revision, so the live
//!   reader serves `None` (documented; a source argv addition is a
//!   follow-up with its own parser review, never smuggled in).
//! - Spectrum (`spectrum`): `pactl` exposes no level taps, so
//!   [`live_sample_spectrum`] is fail-closed unavailable by design. The
//!   gate plus Lua bridge still enforce the 5 Hz / 32-bucket / 1 KiB
//!   contract on the injected sampler path, and the monitor hides (never
//!   fabricates) until a PipeWire/cava monitor follow-up lands.
//! - Control (`set_volume`, `toggle_mute`): one `pactl` spawn per admitted
//!   call ([`set_volume_argv`](bitty_plugin_host::set_volume_argv) /
//!   [`toggle_mute_argv`](bitty_plugin_host::toggle_mute_argv)) behind the
//!   10 Hz [`ControlGate`](bitty_plugin_host::ControlGate).
//!
//! # Fail-closed posture
//!
//! [`live_spawn`] runs the fixed `pactl` argv only (no shell, no caller
//! text, [`check_argv`](bitty_plugin_host::check_argv) enforced before
//! every spawn, byte-capped output). A missing binary, spawn failure, or
//! non-zero exit maps to [`AudioErrorKind::MonitorUnavailable`](bitty_plugin_host::AudioErrorKind):
//! retryable, served stale inside the hide grace by the gates, hidden past
//! it. Nothing fabricates zeros.
//!
//! `wpctl` is explicitly NOT a fallback here: its CLI shapes
//! (`get-volume`, `set-volume`, `set-mute`, `inspect`) and output grammar
//! differ from `pactl`, so a fallback needs new argv discipline plus new
//! parsers under separate review. When `pactl` is absent the monitor hides
//! even if `wpctl` is present (documented follow-up, never silent
//! substitution).
//!
//! # Grants
//!
//! Every bridge entry takes an explicit `granted: bool`. An ungranted call
//! touches no spawner and fails with the broker-indistinguishable code
//! (`E_SERVICE_RESOLUTION` on resolve-shaped paths, `E_SERVICE_GONE` on
//! call-shaped paths per the #2058 negative tests), so read works
//! ungranted-control and control without its head fails closed live.
//!
//! # Time
//!
//! Ticks are caller-supplied `now_ms` (monotonic milliseconds owned by the
//! host tick), never read inside the gates: every rate assertion stays
//! hermetic. [`monotonic_ms_since`] converts a host [`Instant`] baseline
//! for production callers.
//!
//! # Lua shapes (bridge, exact keys)
//!
//! - `get()` returns `{ volume_pct, muted, port, source_volume_pct?,
//!   source_muted?, stale_age_ms }`. Absent source fields encode as Lua
//!   `nil`. Icon/port selection stays Lua-side (port names only).
//! - `spectrum({ buckets = N })` returns `{ levels = [...], stale_age_ms }`.
//!   `buckets` defaults to 8, must sit in 1..=32.
//! - `set_volume({ volume = V })` parks absolute 0..=100;
//!   `set_volume({ delta = D })` steps relative -10..=+10 (over-bound
//!   denied, never clamped). Returns `{ ok = true }`; the bar re-polls
//!   `get()` for the new level.
//! - `toggle_mute({})` returns `{ ok = true }`; the bar re-polls `get()`.
//!
//! Bar rendering lives in bitty-plugins#86 (out of scope).

use std::rc::Rc;
use std::time::Instant;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::ProvidedService;
use bitty_plugin_host::{
    AudioError, AudioErrorKind, AudioState, CONTROL_IFACE, CONTROL_METHOD_SET_VOLUME,
    CONTROL_METHOD_TOGGLE_MUTE, CONTROL_VERSION, ControlGate, MONITOR_OUTPUT_MAX_BYTES,
    SPECTRUM_HEAD, SPECTRUM_IFACE, SPECTRUM_MAX_BUCKETS, SPECTRUM_MAX_RESPONSE_BYTES,
    SPECTRUM_METHOD, SPECTRUM_VERSION, STATE_CONTROL_HEAD, STATE_IFACE, STATE_METHOD_GET,
    STATE_READ_HEAD, STATE_VERSION, SpectrumGate, SpectrumPoll, StateGate, VolumeTarget,
    argv_bytes, check_argv, list_sinks_argv, parse_active_port, parse_sink_mute, parse_sink_volume,
    read_mute_argv, read_volume_argv, set_volume_argv, spectrum_response_bytes, toggle_mute_argv,
};

/// Native audio provider id (mirrors `SENSOR_PROVIDER` for batch A).
pub const AUDIO_PROVIDER: &str = "bitty-terminal.audio";

/// Interface version served by this revision (must match the contract).
pub const AUDIO_INTERFACE_VERSION: &str = "1.0.0";

/// Default spectrum bucket count when Lua passes no `buckets`.
pub const DEFAULT_SPECTRUM_BUCKETS: usize = 8;

/// Deadline per `pactl` spawn in milliseconds.
///
/// `pactl` is a fast local query (normally single-digit milliseconds), so a
/// hung daemon past this bound is a dead monitor: the spawn is killed and
/// reaped, and the timeout maps to `MonitorUnavailable` so the gates serve
/// stale inside the hide grace, then hide. Deliberately tighter than the
/// generic tool-spawn 5 s: a state read performs up to three spawns, and the
/// bar tick must degrade to hide rather than stall behind a wedged daemon.
pub const AUDIO_SPAWN_TIMEOUT_MS: u64 = 1_000;

/// Poll interval while waiting for a spawned monitor to exit.
const AUDIO_SPAWN_POLL_MS: u64 = 5;

// The hung-monitor deadline is a tight local bound: well under the generic
// 5 s tool-spawn contract, above a single poll slice.
const _: () = {
    assert!(AUDIO_SPAWN_TIMEOUT_MS >= 100 && AUDIO_SPAWN_TIMEOUT_MS <= 2_000);
    assert!(AUDIO_SPAWN_POLL_MS >= 1 && AUDIO_SPAWN_POLL_MS <= AUDIO_SPAWN_TIMEOUT_MS);
};

/// Production spawner over one argv vector: returns bounded stdout text.
///
/// Pure function type so tests inject fixtures; [`live_spawn`] is the
/// production member. Fail-closed: every failure is a typed [`AudioError`],
/// never a panic, never shell text.
pub type SpawnFn = Rc<dyn Fn(&[String]) -> Result<String, AudioError>>;

/// Monotonic milliseconds since `start` (production clock for gate ticks).
#[must_use]
pub fn monotonic_ms_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Descriptor for one native audio interface (version, schemas, and heads
/// come from this definition, never from Lua).
#[must_use]
pub fn audio_descriptor(iface: &str) -> ProvidedService {
    let (version, head) = match iface {
        STATE_IFACE => (STATE_VERSION, STATE_READ_HEAD),
        CONTROL_IFACE => (CONTROL_VERSION, STATE_CONTROL_HEAD),
        SPECTRUM_IFACE => (SPECTRUM_VERSION, SPECTRUM_HEAD),
        _ => (AUDIO_INTERFACE_VERSION, iface),
    };
    ProvidedService {
        iface: iface.to_string(),
        version: version.to_string(),
        args_schema: None,
        result_schema: None,
        heads: vec![head.to_string()],
    }
}

/// All three native audio descriptors in interface order.
#[must_use]
pub fn audio_descriptors() -> Vec<ProvidedService> {
    vec![
        audio_descriptor(STATE_IFACE),
        audio_descriptor(CONTROL_IFACE),
        audio_descriptor(SPECTRUM_IFACE),
    ]
}

/// Run one fixed `pactl` argv and return bounded stdout text.
///
/// Discipline: [`check_argv`] first (fixed program, no metacharacters,
/// byte cap), [`std::process::Command`] with an argv array (no shell,
/// piped stdout, null stdin/stderr), execution bounded by
/// [`AUDIO_SPAWN_TIMEOUT_MS`] via `spawn` + `try_wait` polling (a hung
/// monitor is killed and reaped, never waited on forever), output capped
/// at [`MONITOR_OUTPUT_MAX_BYTES`]. Missing binary, spawn failure, timeout,
/// and non-zero exit all map to `MonitorUnavailable` (retryable; the gates
/// stale-then-hide). Non-UTF8 stdout maps to `Invalid`.
pub fn live_spawn(argv: &[String]) -> Result<String, AudioError> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    check_argv(argv)?;
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            use std::io::ErrorKind as Io;
            match error.kind() {
                Io::NotFound => AudioError::unavailable(
                    "pactl absent (fail-closed hide; wpctl is not a fallback here)",
                ),
                _ => AudioError::unavailable(format!(
                    "audio monitor spawn failed (fail-closed): {}",
                    truncate_io_error(&error.to_string())
                )),
            }
        })?;
    let deadline = Duration::from_millis(AUDIO_SPAWN_TIMEOUT_MS);
    let started = Instant::now();
    let status = loop {
        match child.try_wait().map_err(|error| {
            AudioError::unavailable(format!(
                "audio monitor wait failed (fail-closed): {}",
                truncate_io_error(&error.to_string())
            ))
        })? {
            Some(status) => break status,
            None => {
                if started.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(AudioError::unavailable(format!(
                        "pactl hung past {AUDIO_SPAWN_TIMEOUT_MS} ms (killed, fail-closed, retryable)"
                    )));
                }
                std::thread::sleep(Duration::from_millis(AUDIO_SPAWN_POLL_MS));
            }
        }
    };
    if !status.success() {
        return Err(AudioError::unavailable(format!(
            "pactl exited {} (fail-closed, retryable)",
            status.code().unwrap_or(-1)
        )));
    }
    let mut stdout = Vec::new();
    if let Some(piped) = child.stdout.take() {
        piped
            .take(
                u64::try_from(MONITOR_OUTPUT_MAX_BYTES)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
            )
            .read_to_end(&mut stdout)
            .map_err(|_| AudioError::invalid("monitor output unreadable (rejected)"))?;
    }
    if stdout.len() > MONITOR_OUTPUT_MAX_BYTES {
        return Err(AudioError::budget(format!(
            "monitor output {} bytes exceeds {MONITOR_OUTPUT_MAX_BYTES} (rejected)",
            stdout.len()
        )));
    }
    String::from_utf8(stdout)
        .map_err(|_| AudioError::invalid("monitor output is not UTF-8 (rejected)"))
}

/// Live state reader behind the [`StateGate`] seam.
///
/// Three bounded spawns (volume, mute, active port) composed into one
/// validated [`AudioState`]. Source fields serve `None`: no scoped source
/// argv exists in this revision. Any spawn or parse failure propagates as
/// the typed contract error so the gate stales-then-hides.
pub fn live_read(spawn: &SpawnFn) -> Result<AudioState, AudioError> {
    let volume_argv = read_volume_argv();
    let mute_argv = read_mute_argv();
    let sinks_argv = list_sinks_argv();
    debug_assert!(argv_bytes(&volume_argv) <= 256);
    let volume_out = spawn(&volume_argv)?;
    let volume_pct = parse_sink_volume(&volume_out)?;
    let mute_out = spawn(&mute_argv)?;
    let muted = parse_sink_mute(&mute_out)?;
    let sinks_out = spawn(&sinks_argv)?;
    let port = parse_active_port(&sinks_out)?;
    AudioState::validate(volume_pct, muted, &port, None, None)
}

/// Live spectrum sampler behind the [`SpectrumGate`] seam.
///
/// Fail-closed unavailable by design: `pactl` exposes no level taps, so
/// there is no live spectrum source until the PipeWire/cava follow-up.
/// The gate still enforces the 5 Hz / 32-bucket / 1 KiB contract and hides
/// past the grace (never fabricates). Takes no spawner and performs no I/O.
pub fn live_sample_spectrum() -> Result<Vec<f32>, AudioError> {
    Err(AudioError::unavailable(
        "spectrum monitor has no live source via pactl (cava follow-up; hide, never fabricate)",
    ))
}

/// Marshal one [`AudioState`] into the exact Lua `get()` table.
#[must_use]
pub fn audio_state_to_lua(state: &AudioState) -> LuaValue {
    let source_volume = match state.source_volume_pct {
        Some(v) => LuaValue::Integer(i64::from(v)),
        None => LuaValue::Nil,
    };
    let source_muted = match state.source_muted {
        Some(m) => LuaValue::Bool(m),
        None => LuaValue::Nil,
    };
    LuaValue::table([
        ("volume_pct", LuaValue::Integer(i64::from(state.volume_pct))),
        ("muted", LuaValue::Bool(state.muted)),
        ("port", LuaValue::String(state.port.clone())),
        ("source_volume_pct", source_volume),
        ("source_muted", source_muted),
        ("stale_age_ms", LuaValue::Integer(state.stale_age_ms as i64)),
    ])
}

/// Marshal one served [`SpectrumPoll`] into `{ levels, stale_age_ms }`.
#[must_use]
pub fn spectrum_poll_to_lua(poll: &SpectrumPoll) -> LuaValue {
    let levels = poll
        .levels()
        .iter()
        .map(|level| LuaValue::Number(f64::from(*level)))
        .collect();
    LuaValue::table([
        ("levels", LuaValue::array(levels)),
        ("stale_age_ms", LuaValue::Integer(poll.age_ms() as i64)),
    ])
}

/// Parse `spectrum({ buckets = N })` args (nil defaults to 8).
pub fn parse_spectrum_args(args: &LuaValue) -> Result<usize, BridgeError> {
    let buckets = match args.get("buckets") {
        None | Some(LuaValue::Nil) => DEFAULT_SPECTRUM_BUCKETS,
        Some(LuaValue::Integer(n)) => {
            if *n <= 0 || *n > SPECTRUM_MAX_BUCKETS as i64 {
                return Err(BridgeError::new(
                    "validation",
                    "E_SERVICE_INVALID",
                    format!("spectrum buckets {n} out of 1..={SPECTRUM_MAX_BUCKETS}"),
                ));
            }
            *n as usize
        }
        Some(LuaValue::Number(n)) => {
            if !n.is_finite() || *n < 1.0 || *n > SPECTRUM_MAX_BUCKETS as f64 || n.fract() != 0.0 {
                return Err(BridgeError::new(
                    "validation",
                    "E_SERVICE_INVALID",
                    format!("spectrum buckets {n} out of 1..={SPECTRUM_MAX_BUCKETS}"),
                ));
            }
            *n as usize
        }
        Some(_) => {
            return Err(BridgeError::new(
                "validation",
                "E_SERVICE_INVALID",
                "spectrum buckets must be an integer",
            ));
        }
    };
    if spectrum_response_bytes(buckets) > SPECTRUM_MAX_RESPONSE_BYTES {
        return Err(BridgeError::new(
            "validation",
            "E_SERVICE_INVALID",
            format!("spectrum buckets {buckets} exceeds the 1024-byte response cap"),
        ));
    }
    Ok(buckets)
}

/// Parse `set_volume({ volume = V })` / `set_volume({ delta = D })` args.
pub fn parse_set_volume_args(args: &LuaValue) -> Result<VolumeTarget, BridgeError> {
    let volume = args.get("volume").or_else(|| args.get("volume_pct"));
    let delta = args.get("delta").or_else(|| args.get("delta_pct"));
    match (volume, delta) {
        (Some(v), None) => {
            let pct: u8 = match v {
                LuaValue::Integer(n) => {
                    if *n < 0 || *n > 100 {
                        return Err(volume_invalid(format!("volume park {n} exceeds 0..=100")));
                    }
                    *n as u8
                }
                LuaValue::Number(n) => {
                    if !n.is_finite() || *n < 0.0 || *n > 100.0 || n.fract() != 0.0 {
                        return Err(volume_invalid(format!("volume park {n} exceeds 0..=100")));
                    }
                    *n as u8
                }
                _ => {
                    return Err(volume_invalid("volume park must be an integer 0..=100"));
                }
            };
            VolumeTarget::absolute(pct).map_err(|error| volume_invalid(error.message().to_string()))
        }
        (None, Some(d)) => {
            let step: i32 = match d {
                LuaValue::Integer(n) => {
                    if n.abs() > 10 {
                        return Err(volume_invalid(format!("volume step {n} exceeds +/-10")));
                    }
                    i32::try_from(*n).unwrap_or(10)
                }
                LuaValue::Number(n) => {
                    if !n.is_finite() || n.abs() > 10.0 || n.fract() != 0.0 {
                        return Err(volume_invalid(format!("volume step {n} exceeds +/-10")));
                    }
                    *n as i32
                }
                _ => {
                    return Err(volume_invalid("volume step must be an integer -10..=+10"));
                }
            };
            VolumeTarget::delta(step).map_err(|error| volume_invalid(error.message().to_string()))
        }
        (None, None) => Err(volume_invalid(
            "set_volume needs volume (0..=100) or delta (-10..=+10)",
        )),
        (Some(_), Some(_)) => Err(volume_invalid(
            "set_volume takes exactly one of volume or delta",
        )),
    }
}

/// Bridge error for a malformed `set_volume` argument (fail-closed, never clamped).
fn volume_invalid(message: impl Into<String>) -> BridgeError {
    BridgeError::new("validation", "E_SERVICE_INVALID", message)
}

/// Bridge error for an unavailable audio service (indistinguishable from a parked provider).
fn service_gone(message: impl Into<String>) -> BridgeError {
    BridgeError::new("runtime", "E_SERVICE_GONE", message)
}

/// Map a contract [`AudioError`] onto the broker-indistinguishable bridge code.
///
/// Bounds/parse failures surface as `E_SERVICE_INVALID`; absent, lost, or
/// hidden monitors surface as `E_SERVICE_GONE` (like a parked provider).
fn audio_error_to_gone(error: AudioError) -> BridgeError {
    match error.kind() {
        AudioErrorKind::Budget | AudioErrorKind::Invalid => BridgeError::new(
            "validation",
            "E_SERVICE_INVALID",
            error.message().to_string(),
        ),
        AudioErrorKind::MonitorUnavailable | AudioErrorKind::Hidden => {
            service_gone(error.message().to_string())
        }
    }
}

/// Bound one OS error string for a fail-closed diagnostic (single line, 120 chars).
fn truncate_io_error(message: &str) -> String {
    let out: String = message.chars().take(120).collect();
    out.replace(['\n', '\r'], " ")
}

/// Gate-holding live audio monitor: spectrum/state/control behind the
/// contract gates with one injected spawner.
///
/// Grants are explicit per call (`granted == false` touches no spawner and
/// fails with the broker-indistinguishable code). Ticks are explicit
/// `now_ms`. The default spawner is [`live_spawn`]; tests inject counting
/// fixtures.
pub struct AudioMonitor {
    spectrum_gate: SpectrumGate,
    state_gate: StateGate,
    control_gate: ControlGate,
    spawn: SpawnFn,
}

impl AudioMonitor {
    /// Monitor over an injected spawner (fixtures, tests).
    pub fn with_spawn(spawn: SpawnFn) -> Self {
        Self {
            spectrum_gate: SpectrumGate::new(),
            state_gate: StateGate::new(),
            control_gate: ControlGate::new(),
            spawn,
        }
    }

    /// Production monitor spawning live `pactl`.
    pub fn live() -> Self {
        Self::with_spawn(Rc::new(live_spawn))
    }

    /// Serve `spectrum({ buckets })` with the 5 Hz / 32-bucket / 1 KiB gate.
    ///
    /// The sampler seam stays injected behind the gate: production callers
    /// pass [`live_sample_spectrum`] (fail-closed hide until the cava
    /// follow-up); tests pass fixtures. Ungranted calls touch no sampler.
    pub fn spectrum(
        &mut self,
        args: &LuaValue,
        now_ms: u64,
        granted: bool,
    ) -> Result<LuaValue, BridgeError> {
        if !granted {
            return Err(BridgeError::new(
                "resolution",
                "E_SERVICE_RESOLUTION",
                "audio.spectrum is unavailable",
            ));
        }
        let buckets = parse_spectrum_args(args)?;
        let poll = self
            .spectrum_gate
            .poll(now_ms, buckets, live_sample_spectrum)
            .map_err(audio_error_to_gone)?;
        Ok(spectrum_poll_to_lua(&poll))
    }

    /// Serve `spectrum` with an injected sampler (tests, fixtures).
    ///
    /// Same gate and Lua shape as [`Self::spectrum`]; the sampler closure
    /// is caller-provided so burst/ceiling assertions count invocations.
    pub fn spectrum_with(
        &mut self,
        buckets: usize,
        now_ms: u64,
        granted: bool,
        sample: impl FnOnce() -> Result<Vec<f32>, AudioError>,
    ) -> Result<LuaValue, BridgeError> {
        if !granted {
            return Err(BridgeError::new(
                "resolution",
                "E_SERVICE_RESOLUTION",
                "audio.spectrum is unavailable",
            ));
        }
        if buckets == 0 || buckets > SPECTRUM_MAX_BUCKETS {
            return Err(BridgeError::new(
                "validation",
                "E_SERVICE_INVALID",
                format!("spectrum buckets {buckets} out of 1..={SPECTRUM_MAX_BUCKETS}"),
            ));
        }
        if spectrum_response_bytes(buckets) > SPECTRUM_MAX_RESPONSE_BYTES {
            return Err(BridgeError::new(
                "validation",
                "E_SERVICE_INVALID",
                format!("spectrum buckets {buckets} exceeds the 1024-byte response cap"),
            ));
        }
        let poll = self
            .spectrum_gate
            .poll(now_ms, buckets, sample)
            .map_err(audio_error_to_gone)?;
        Ok(spectrum_poll_to_lua(&poll))
    }

    /// Serve `get()` with the 2 s read ceiling.
    pub fn get(&mut self, now_ms: u64, granted: bool) -> Result<LuaValue, BridgeError> {
        if !granted {
            return Err(BridgeError::new(
                "resolution",
                "E_SERVICE_RESOLUTION",
                "audio.state is unavailable",
            ));
        }
        let spawn = Rc::clone(&self.spawn);
        let poll = self
            .state_gate
            .poll(now_ms, || live_read(&spawn))
            .map_err(audio_error_to_gone)?;
        Ok(audio_state_to_lua(poll.state()))
    }

    /// Serve `set_volume` with the 10 Hz control budget.
    ///
    /// Denied windows fail closed without spawning; relative overflows are
    /// denied at parse (never clamped). One admitted call performs exactly
    /// one spawn.
    pub fn set_volume(
        &mut self,
        args: &LuaValue,
        now_ms: u64,
        granted: bool,
    ) -> Result<LuaValue, BridgeError> {
        if !granted {
            return Err(service_gone("audio.control is unavailable"));
        }
        let target = parse_set_volume_args(args)?;
        if !self.control_gate.admit(now_ms) {
            return Err(BridgeError::new(
                "validation",
                "E_SERVICE_INVALID",
                "control call within 100 ms window (coalesce, retry)".to_string(),
            ));
        }
        let argv = set_volume_argv(target);
        check_argv(&argv).map_err(audio_error_to_gone)?;
        (self.spawn)(&argv).map_err(audio_error_to_gone)?;
        Ok(LuaValue::table([("ok", LuaValue::Bool(true))]))
    }

    /// Serve `toggle_mute` with the 10 Hz control budget.
    pub fn toggle_mute(&mut self, now_ms: u64, granted: bool) -> Result<LuaValue, BridgeError> {
        if !granted {
            return Err(service_gone("audio.control is unavailable"));
        }
        if !self.control_gate.admit(now_ms) {
            return Err(BridgeError::new(
                "validation",
                "E_SERVICE_INVALID",
                "control call within 100 ms window (coalesce, retry)",
            ));
        }
        let argv = toggle_mute_argv();
        check_argv(&argv).map_err(audio_error_to_gone)?;
        (self.spawn)(&argv).map_err(audio_error_to_gone)?;
        Ok(LuaValue::table([("ok", LuaValue::Bool(true))]))
    }

    /// Dispatch one native method by interface + method name (host bridge).
    ///
    /// Routes `audio.state/get`, `audio.spectrum/spectrum`,
    /// `audio.control/set_volume|toggle_mute` through the gated entries
    /// above. Unknown pairs fail closed as `E_SERVICE_GONE` (like a parked
    /// provider method), never as a new code.
    pub fn dispatch(
        &mut self,
        iface: &str,
        method: &str,
        args: &LuaValue,
        now_ms: u64,
        granted: bool,
    ) -> Result<LuaValue, BridgeError> {
        match (iface, method) {
            (STATE_IFACE, STATE_METHOD_GET) => self.get(now_ms, granted),
            (SPECTRUM_IFACE, SPECTRUM_METHOD) => self.spectrum(args, now_ms, granted),
            (CONTROL_IFACE, CONTROL_METHOD_SET_VOLUME) => self.set_volume(args, now_ms, granted),
            (CONTROL_IFACE, CONTROL_METHOD_TOGGLE_MUTE) => self.toggle_mute(now_ms, granted),
            _ => Err(service_gone(format!(
                "service '{iface}.{method}' is not published"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    use bitty_plugin_host::{
        CONTROL_MAX_ARGV_BYTES, SPECTRUM_MIN_INTERVAL_MS, STATE_MIN_INTERVAL_MS,
        parse_service_reference, service_version_satisfies,
    };

    use crate::plugin_runtime::services::{
        NotificationQueue, PluginServices, ServiceDirectory, ServiceRecord,
    };
    use crate::plugin_runtime::store::PluginStore;
    use std::collections::BTreeMap;
    use std::rc::Weak;

    const VOLUME_FIXTURE: &str = "Volume: front-left: 32768 /  50% / -18.06 dB\n";
    const MUTE_FIXTURE: &str = "Mute: no\n";
    const SINKS_FIXTURE: &str =
        "Sink #0\n\tState: RUNNING\n\tActive Port: analog-output-headphones\n";

    fn fixture_spawn(log: Rc<RefCell<Vec<Vec<String>>>>) -> SpawnFn {
        Rc::new(move |argv: &[String]| {
            log.borrow_mut().push(argv.to_vec());
            let text = argv.join(" ");
            if text.contains("get-sink-volume") {
                Ok(VOLUME_FIXTURE.to_string())
            } else if text.contains("get-sink-mute") {
                Ok(MUTE_FIXTURE.to_string())
            } else if text.contains("list") {
                Ok(SINKS_FIXTURE.to_string())
            } else if text.contains("set-sink") {
                Ok(String::new())
            } else {
                Err(AudioError::unavailable("unexpected argv in fixture"))
            }
        })
    }

    fn absent_spawn() -> SpawnFn {
        Rc::new(|_: &[String]| {
            Err(AudioError::unavailable(
                "pactl absent (fail-closed hide; wpctl is not a fallback here)",
            ))
        })
    }

    #[test]
    fn descriptors_validate_and_pin_owner_version_and_heads() {
        assert_eq!(AUDIO_INTERFACE_VERSION, "1.0.0");
        let all = audio_descriptors();
        assert_eq!(all.len(), 3);
        for service in &all {
            service.validate().expect("descriptor validates");
            let (owner, short) = parse_service_reference(&service.iface, "services.get")
                .expect("descriptor reference parses");
            assert!(owner.is_none(), "provide-side names the short form");
            assert!(!short.starts_with("bitty."), "never Core-reserved");
            assert!(!service.heads.is_empty(), "every interface is backed");
            assert!(service_version_satisfies(&service.version, "^1.0"));
        }
        let state = audio_descriptor(STATE_IFACE);
        assert_eq!(state.version, STATE_VERSION);
        assert_eq!(state.heads, vec![STATE_READ_HEAD.to_string()]);
        let control = audio_descriptor(CONTROL_IFACE);
        assert_eq!(control.version, CONTROL_VERSION);
        assert_eq!(control.heads, vec![STATE_CONTROL_HEAD.to_string()]);
        let spectrum = audio_descriptor(SPECTRUM_IFACE);
        assert_eq!(spectrum.version, SPECTRUM_VERSION);
        assert_eq!(spectrum.heads, vec![SPECTRUM_HEAD.to_string()]);
    }

    #[test]
    fn live_read_composes_three_scoped_spawns() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let spawn = fixture_spawn(Rc::clone(&log));
        let state = live_read(&spawn).expect("fixture reads");
        assert_eq!(state.volume_pct, 50);
        assert!(!state.muted);
        assert_eq!(state.port, "analog-output-headphones");
        assert_eq!(state.source_volume_pct, None);
        assert_eq!(state.source_muted, None);
        let calls = log.borrow();
        assert_eq!(calls.len(), 3, "volume + mute + port, nothing else");
        for argv in calls.iter() {
            check_argv(argv).expect("every live argv checks");
            assert_eq!(argv[0], "pactl");
            assert!(argv_bytes(argv) <= CONTROL_MAX_ARGV_BYTES);
        }
        assert_eq!(calls[0][1], "get-sink-volume");
        assert_eq!(calls[1][1], "get-sink-mute");
        assert_eq!(calls[0][2], "@DEFAULT_SINK@");
    }

    #[test]
    fn live_read_fails_closed_on_garbage_without_shell() {
        let spawn: SpawnFn = Rc::new(|argv: &[String]| {
            check_argv(argv).expect("argv checks before spawn");
            Ok("Volume: mono: mute\n".to_string())
        });
        assert!(live_read(&spawn).is_err());
        // Hostile argv never spawns: the discipline rejects before I/O.
        assert!(check_argv(&["pactl".to_string(), "list; rm -rf".to_string()]).is_err());
        assert!(
            check_argv(&["sh".to_string(), "-c".to_string(), "pactl list".to_string()]).is_err()
        );
    }

    #[test]
    fn live_sample_spectrum_hides_without_io() {
        let error = live_sample_spectrum().expect_err("no pactl source hides");
        assert_eq!(error.kind(), AudioErrorKind::MonitorUnavailable);
    }

    #[test]
    fn live_spawn_discipline_rejects_before_io() {
        // Hostile argv never reaches the OS: the discipline rejects first.
        assert!(live_spawn(&[]).is_err());
        assert!(
            live_spawn(&["sh".to_string(), "-c".to_string(), "pactl list".to_string()]).is_err()
        );
        assert!(
            live_spawn(&[
                "pactl".to_string(),
                "set-sink-volume".to_string(),
                "@DEFAULT_SINK@".to_string(),
                "$(evil)".to_string()
            ])
            .is_err()
        );
    }

    #[test]
    fn get_enforces_two_second_ceiling_on_live_path() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut monitor = AudioMonitor::with_spawn(fixture_spawn(Rc::clone(&log)));
        let first = monitor.get(1000, true).expect("fresh serves");
        assert_eq!(first.get("volume_pct"), Some(&LuaValue::Integer(50)));
        assert_eq!(
            first.get("port"),
            Some(&LuaValue::String("analog-output-headphones".to_string()))
        );
        assert_eq!(log.borrow().len(), 3);
        // 500 ms later: coalesced, no new spawns.
        let second = monitor.get(1500, true).expect("coalesces");
        assert_eq!(second.get("volume_pct"), Some(&LuaValue::Integer(50)));
        assert_eq!(log.borrow().len(), 3, "over-poll performs no monitor work");
        // Past the ceiling: fresh read runs again.
        monitor.get(4000, true).expect("window reads");
        assert_eq!(log.borrow().len(), 6);
    }

    #[test]
    fn spectrum_bridge_enforces_five_hertz_thirty_two_bucket_one_kib() {
        let calls = Rc::new(Cell::new(0_usize));
        let mut monitor = AudioMonitor::with_spawn(absent_spawn());
        let probed = Rc::clone(&calls);
        let first = monitor
            .spectrum_with(8, 1000, true, || {
                probed.set(probed.get() + 1);
                Ok(vec![0.5; 8])
            })
            .expect("fresh serves");
        assert_eq!(calls.get(), 1);
        let levels = match first.get("levels") {
            Some(LuaValue::Table(pairs)) => pairs.len(),
            other => panic!("levels must be an array, got {other:?}"),
        };
        assert_eq!(levels, 8);
        // Inside the 200 ms window the sampler does NOT run.
        let probed = Rc::clone(&calls);
        monitor
            .spectrum_with(8, 1050, true, || {
                probed.set(probed.get() + 1);
                Ok(vec![0.9; 8])
            })
            .expect("coalesces");
        assert_eq!(calls.get(), 1, "over-poll performs no monitor work");
        // Bucket overflow denied without sampling.
        let probed = Rc::clone(&calls);
        let error = monitor
            .spectrum_with(SPECTRUM_MAX_BUCKETS + 1, 2000, true, || {
                probed.set(probed.get() + 1);
                Ok(vec![0.5; 8])
            })
            .expect_err("33 buckets denied");
        assert_eq!(error.code, "E_SERVICE_INVALID");
        assert_eq!(calls.get(), 1, "rejected polls never sample");
        // Response cap holds for every admissible count.
        for buckets in 1..=SPECTRUM_MAX_BUCKETS {
            assert!(spectrum_response_bytes(buckets) <= SPECTRUM_MAX_RESPONSE_BYTES);
        }
        let _ = SPECTRUM_MIN_INTERVAL_MS;
        let _ = STATE_MIN_INTERVAL_MS;
    }

    #[test]
    fn control_enforces_ten_hertz_coalesced_budget_live() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut monitor = AudioMonitor::with_spawn(fixture_spawn(Rc::clone(&log)));
        let delta = LuaValue::table([("delta", LuaValue::Integer(5))]);
        monitor.set_volume(&delta, 0, true).expect("first admitted");
        assert_eq!(log.borrow().len(), 1);
        // Same 100 ms window: denied without spawning.
        let error = monitor
            .set_volume(&delta, 50, true)
            .expect_err("window denied");
        assert_eq!(error.code, "E_SERVICE_INVALID");
        assert_eq!(log.borrow().len(), 1, "denied control performs no spawn");
        // Next window: admitted.
        monitor
            .set_volume(&delta, 100, true)
            .expect("window admitted");
        assert_eq!(log.borrow().len(), 2);
        // Over-bound steps denied at parse, never clamped, no spawn.
        let hot = LuaValue::table([("delta", LuaValue::Integer(11))]);
        assert!(monitor.set_volume(&hot, 200, true).is_err());
        assert_eq!(log.borrow().len(), 2);
        // Toggle rides the same budget.
        monitor.toggle_mute(300, true).expect("toggle admitted");
        assert_eq!(log.borrow().len(), 3);
        assert!(monitor.toggle_mute(320, true).is_err());
        assert_eq!(log.borrow().len(), 3);
    }

    #[test]
    fn read_without_control_holds_live() {
        // bitty#2013 acceptance live: read + spectrum serve with the
        // read-only grant set; control without its head fails closed as
        // gone (call) and resolution-named (resolve), same codes as #2058.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut monitor = AudioMonitor::with_spawn(fixture_spawn(Rc::clone(&log)));
        monitor.get(0, true).expect("read serves ungranted-control");
        let error = monitor
            .set_volume(
                &LuaValue::table([("delta", LuaValue::Integer(3))]),
                1000,
                false,
            )
            .expect_err("control without head fails closed");
        assert_eq!(error.code, "E_SERVICE_GONE");
        assert!(
            !error.message.contains(STATE_CONTROL_HEAD),
            "denial names no head"
        );
        let error = monitor.toggle_mute(2000, false).expect_err("toggle denied");
        assert_eq!(error.code, "E_SERVICE_GONE");
        // Denied calls never spawn.
        let spawns_before = log.borrow().len();
        let _ = monitor.set_volume(
            &LuaValue::table([("delta", LuaValue::Integer(1))]),
            3000,
            false,
        );
        assert_eq!(
            log.borrow().len(),
            spawns_before,
            "denied control never spawns"
        );
    }

    #[test]
    fn ungranted_read_touches_no_spawner_live() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut monitor = AudioMonitor::with_spawn(fixture_spawn(Rc::clone(&log)));
        let error = monitor.get(0, false).expect_err("ungranted read fails");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        assert_eq!(log.borrow().len(), 0, "ungranted never touches the source");
        let calls = Rc::new(Cell::new(0_usize));
        let probed = Rc::clone(&calls);
        let error = monitor
            .spectrum_with(8, 0, false, || {
                probed.set(probed.get() + 1);
                Ok(vec![0.5; 8])
            })
            .expect_err("ungranted spectrum fails");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn absent_monitor_serves_stale_then_hides_live() {
        let mut monitor = AudioMonitor::with_spawn(absent_spawn());
        let error = monitor.get(0, true).expect_err("never read hides");
        assert_eq!(error.code, "E_SERVICE_GONE");
        // Seed last-good via a fixture, then lose the monitor: stale with
        // age inside the grace, hidden past it — never zeros.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut seeded = AudioMonitor::with_spawn(fixture_spawn(Rc::clone(&log)));
        seeded.get(2000, true).expect("lands");
        seeded.spawn = absent_spawn();
        let stale = seeded.get(4000, true).expect("silence stales");
        assert_eq!(stale.get("volume_pct"), Some(&LuaValue::Integer(50)));
        assert_eq!(stale.get("stale_age_ms"), Some(&LuaValue::Integer(2000)));
        let error = seeded
            .get(2000 + 10_000 + 2000, true)
            .expect_err("grace hides");
        assert_eq!(error.code, "E_SERVICE_GONE");
    }

    #[test]
    fn lua_shapes_carry_exact_keys_and_nil_sources() {
        let state = AudioState::validate(70, true, "analog-output-headphones", None, None)
            .expect("fixture");
        let value = audio_state_to_lua(&state);
        let LuaValue::Table(pairs) = &value else {
            panic!("state marshals as a table");
        };
        assert_eq!(pairs.len(), 6);
        assert_eq!(value.get("volume_pct"), Some(&LuaValue::Integer(70)));
        assert_eq!(value.get("muted"), Some(&LuaValue::Bool(true)));
        assert_eq!(
            value.get("port"),
            Some(&LuaValue::String("analog-output-headphones".to_string()))
        );
        assert_eq!(value.get("source_volume_pct"), Some(&LuaValue::Nil));
        assert_eq!(value.get("source_muted"), Some(&LuaValue::Nil));
        assert_eq!(value.get("stale_age_ms"), Some(&LuaValue::Integer(0)));

        let poll = SpectrumPoll::Fresh {
            levels: vec![0.25, 0.75],
            age_ms: 12,
        };
        let meter = spectrum_poll_to_lua(&poll);
        assert!(matches!(meter.get("levels"), Some(LuaValue::Table(_))));
        assert_eq!(meter.get("stale_age_ms"), Some(&LuaValue::Integer(12)));

        // Args: buckets default 8, volume/delta heads, toggle takes nothing.
        assert_eq!(parse_spectrum_args(&LuaValue::Nil).expect("default"), 8);
        assert_eq!(
            parse_spectrum_args(&LuaValue::table([("buckets", LuaValue::Integer(32))]))
                .expect("edge"),
            32
        );
        assert!(
            parse_spectrum_args(&LuaValue::table([("buckets", LuaValue::Integer(33))])).is_err()
        );
        assert_eq!(
            parse_set_volume_args(&LuaValue::table([("volume", LuaValue::Integer(60))]))
                .expect("park"),
            VolumeTarget::Absolute(60)
        );
        assert_eq!(
            parse_set_volume_args(&LuaValue::table([("delta", LuaValue::Integer(-5))]))
                .expect("step"),
            VolumeTarget::Delta(-5)
        );
        assert!(
            parse_set_volume_args(&LuaValue::table([("delta", LuaValue::Integer(11))])).is_err()
        );
        assert!(
            parse_set_volume_args(&LuaValue::table([
                ("volume", LuaValue::Integer(1)),
                ("delta", LuaValue::Integer(1))
            ]))
            .is_err()
        );
    }

    // -- broker-shaped gating over native records ---------------------------
    //
    // The directory holds the native audio records (provider
    // `bitty-terminal.audio`); the real `PluginServices` resolve path
    // proves read resolves ungranted-control while control stays
    // indistinguishable from absent — the same codes as the #2058
    // `services.rs` broker tests, now over the live descriptors.

    struct EmptySettings;

    impl crate::plugin_runtime::services::SettingsSource for EmptySettings {
        fn get(&self, _key: &str) -> Option<LuaValue> {
            None
        }
    }

    struct EmptySnapshot;

    impl crate::plugin_runtime::services::SnapshotSource for EmptySnapshot {
        fn snapshot(&self, _scope: &str) -> Result<LuaValue, bitty_lua::BridgeError> {
            Err(bitty_lua::BridgeError::not_implemented("snapshot"))
        }
    }

    fn bar_host(granted: Vec<String>) -> (Rc<RefCell<ServiceDirectory>>, PluginServices) {
        let directory = Rc::new(RefCell::new(ServiceDirectory::new()));
        for service in audio_descriptors() {
            directory.borrow_mut().publish(ServiceRecord {
                provider: AUDIO_PROVIDER.to_string(),
                generation: 1,
                iface: service.iface.clone(),
                version: service.version.clone(),
                methods: match service.iface.as_str() {
                    STATE_IFACE => vec![STATE_METHOD_GET.to_string()],
                    CONTROL_IFACE => vec![
                        CONTROL_METHOD_SET_VOLUME.to_string(),
                        CONTROL_METHOD_TOGGLE_MUTE.to_string(),
                    ],
                    _ => vec![SPECTRUM_METHOD.to_string()],
                },
                funcs: BTreeMap::new(),
                args_schema: None,
                result_schema: None,
                heads: service.heads.clone(),
                vm: Weak::new(),
                suspended: false,
            });
        }
        let host = PluginServices::new(
            "xuepoo.bar",
            PluginStore::in_memory(),
            Rc::new(EmptySettings),
            Rc::new(EmptySnapshot),
            Rc::new(RefCell::new(NotificationQueue::new(8))),
            false,
            false,
        );
        host.set_service_directory(Rc::clone(&directory));
        host.set_service_manifest(
            Vec::new(),
            vec![
                (STATE_IFACE.to_string(), ">=1.0".to_string()),
                (CONTROL_IFACE.to_string(), ">=1.0".to_string()),
                (SPECTRUM_IFACE.to_string(), ">=1.0".to_string()),
            ],
        );
        host.set_granted_capabilities(granted);
        (directory, host)
    }

    #[test]
    fn native_records_resolve_read_while_control_stays_absent() {
        use bitty_lua::HostServices;
        let (_, host) = bar_host(vec![STATE_READ_HEAD.to_string(), SPECTRUM_HEAD.to_string()]);
        let route = HostServices::service_resolve(&host, STATE_IFACE, None, false)
            .expect("read resolves")
            .expect("route present");
        assert_eq!(route.provider, AUDIO_PROVIDER);
        let route = HostServices::service_resolve(&host, SPECTRUM_IFACE, None, false)
            .expect("spectrum resolves")
            .expect("route present");
        assert_eq!(route.provider, AUDIO_PROVIDER);
        let error = HostServices::service_resolve(&host, CONTROL_IFACE, None, false)
            .expect_err("ungranted control must fail closed");
        assert_eq!(error.code, "E_SERVICE_RESOLUTION");
        assert!(!error.message.contains(STATE_CONTROL_HEAD));
        assert_eq!(
            HostServices::service_resolve(&host, CONTROL_IFACE, None, true)
                .expect("optional degrades to nil"),
            None
        );
    }

    #[test]
    fn native_control_call_without_head_fails_closed_as_gone() {
        use bitty_lua::HostServices;
        let (_, host) = bar_host(vec![STATE_READ_HEAD.to_string()]);
        for method in [CONTROL_METHOD_SET_VOLUME, CONTROL_METHOD_TOGGLE_MUTE] {
            let error = HostServices::service_call(
                &host,
                AUDIO_PROVIDER,
                1,
                CONTROL_IFACE,
                AUDIO_INTERFACE_VERSION,
                method,
                &LuaValue::Nil,
            )
            .expect_err("ungranted control call must fail closed");
            assert_eq!(error.code, "E_SERVICE_GONE");
        }
    }

    #[test]
    fn dispatch_routes_and_rejects_unknown_live() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut monitor = AudioMonitor::with_spawn(fixture_spawn(Rc::clone(&log)));
        let state = monitor
            .dispatch(STATE_IFACE, STATE_METHOD_GET, &LuaValue::Nil, 0, true)
            .expect("get dispatches");
        assert_eq!(state.get("volume_pct"), Some(&LuaValue::Integer(50)));
        let error = monitor
            .dispatch("audio.state", "nope", &LuaValue::Nil, 0, true)
            .expect_err("unknown method fails closed");
        assert_eq!(error.code, "E_SERVICE_GONE");
    }
}
