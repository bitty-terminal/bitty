#![forbid(unsafe_code)]
//! Backlight + idle-inhibit provider interfaces (CTX-1197, bitty#2020).
//!
//! Bar modules (`custom/backlight` + `idle_inhibitor` parity) consume
//! brightness state plus the compositor inhibit flag; both mutations
//! live behind separate control heads and are never smuggled into reads.
//!
//! Backlight read is unprivileged sysfs text below
//! [`BacklightSysfs::class_root`] (`brightness` + `max_brightness` per
//! device; first sorted device wins, documented). Backlight **set** is
//! privileged on most systems (udev `video` group / polkit), and this
//! module records the privilege decision (bitty#2020): the control head
//! attempts the write **only when permitted** and reports
//! `E_NOT_PERMITTED` otherwise — it never escalates, never setuids, and
//! never shells out. A click without permission degrades to notify
//! (consumer policy through the existing `platform.notify` head, owned
//! Lua-side; this module only reports the denial with a naming
//! diagnostic).
//!
//! Idle inhibit is compositor-side Wayland
//! ([`IDLE_INHIBIT_INTERFACE`]) and additionally needs a Bitty surface,
//! so the live binding is host/compositor work: this module defines the
//! `inhibited` state shape, the separate control head, the 5 s read
//! floor, and the fail-closed hide. Where inhibit state cannot survive
//! a compositor round-trip is documented per compositor by the host
//! task, not discovered here.
//!
//! Everything is injected (no filesystem, Wayland, or process access
//! here); snapshots are bounded primitives with `stale_age_ms`
//! everywhere. Control calls are individually grant-checked, so grant
//! narrowing closes them without restart.
//!
//! Bar mapping (Lua owns toggle semantics + icons): `pct` drives the
//! slider glyph, `inhibited` selects the activated/deactivated glyph,
//! and both toggles stay config-side. No new bar API is required.

use super::providers::{
    BACKLIGHT_CONTROL_HEAD, BACKLIGHT_READ_HEAD, GrantSet, IDLE_CONTROL_HEAD, IDLE_READ_HEAD,
};

// ---------------------------------------------------------------------------
// Bounds (policy, timeout, and default values live here, never inline)
// ---------------------------------------------------------------------------

/// Backlight read-path poll floor: configuration can never sample faster.
pub const BACKLIGHT_POLL_FLOOR_MS: u64 = 5000;

/// Default backlight sampling interval (equal to the floor).
pub const BACKLIGHT_DEFAULT_INTERVAL_MS: u64 = 5000;

/// Stale-then-hide bound for a lost backlight source.
pub const BACKLIGHT_STALE_HIDE_MS: u64 = 30_000;

/// Idle read-path poll floor.
pub const IDLE_POLL_FLOOR_MS: u64 = 5000;

/// Default idle sampling interval (equal to the floor).
pub const IDLE_DEFAULT_INTERVAL_MS: u64 = 5000;

/// Stale-then-hide bound for a lost inhibit source.
pub const IDLE_STALE_HIDE_MS: u64 = 30_000;

/// Maximum sysfs value-file bytes parsed (brightness files are tiny;
/// larger input is rejected fail-closed).
pub const BACKLIGHT_VALUE_MAX_BYTES: usize = 32;

/// Control-step magnitude bound in percentage points (`set_step` deltas
/// clamp into `[-100, 100]`; bar sliders send single-digit steps).
pub const BACKLIGHT_STEP_MAX_PCT: i32 = 100;

// ---------------------------------------------------------------------------
// Sysfs + Wayland identifiers (spec-defined, exactly one place)
// ---------------------------------------------------------------------------

/// Brightness value leaf below a backlight device directory.
pub const BACKLIGHT_BRIGHTNESS_FILE: &str = "brightness";

/// Brightness ceiling leaf below a backlight device directory.
pub const BACKLIGHT_MAX_FILE: &str = "max_brightness";

/// Wayland idle-inhibit manager interface (compositor-side binding).
pub const IDLE_INHIBIT_INTERFACE: &str = "zwp_idle_inhibit_manager_v1";

/// Sysfs backlight class root (injected; the live adapter uses
/// [`BacklightSysfs::system`], tests inject fixtures).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BacklightSysfs {
    /// Root enumerating backlight device directories.
    pub class_root: std::path::PathBuf,
}

impl BacklightSysfs {
    /// Standard Linux class root (portable contract path, one place).
    #[must_use]
    pub fn system() -> Self {
        Self {
            class_root: std::path::PathBuf::from("/sys/class/backlight"),
        }
    }
}

/// Parse one sysfs brightness value file (`brightness` or
/// `max_brightness`): trimmed ASCII digits, bounded length. Anything
/// else maps to `None` (fail-closed, never a guessed level).
#[must_use]
pub fn parse_brightness(text: &str) -> Option<u32> {
    if text.len() > BACKLIGHT_VALUE_MAX_BYTES {
        return None;
    }
    let trimmed = text.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<u32>().ok()
}

/// Derive the display percent from `(brightness, max)`: clamped to
/// `0..=100`, over-max clamps (never wraps), `max == 0` maps to `None`
/// (no division by zero, fail-closed).
#[must_use]
pub fn brightness_pct(brightness: u32, max: u32) -> Option<u8> {
    if max == 0 {
        return None;
    }
    let pct = u64::from(brightness) * 100 / u64::from(max);
    Some(pct.min(100) as u8)
}

// ---------------------------------------------------------------------------
// Source traits (injected; live sysfs/Wayland adapters are host work)
// ---------------------------------------------------------------------------

/// Backlight read source (sysfs device enumeration + level reads live).
pub trait BacklightSource {
    /// Whether the class root answers at all (`false` drives
    /// stale-then-hide).
    fn available(&mut self) -> bool;
    /// Primary device name (first sorted entry live; `None` = no device).
    fn primary_device(&mut self) -> Option<String>;
    /// `(brightness, max)` for `device` (`None` = unreadable).
    fn levels(&mut self, device: &str) -> Option<(u32, u32)>;
}

/// Backlight control backend (privileged brightness write live).
///
/// `permitted` answers whether the write may even be attempted (writable
/// brightness file live: udev `video` group / polkit). The control head
/// checks it before every write and never escalates past a `false`.
pub trait BacklightBackend {
    /// Whether a brightness write may be attempted right now.
    fn permitted(&self) -> bool;
    /// Writes the absolute brightness value.
    fn write_brightness(&mut self, value: u32) -> Result<(), String>;
}

/// Idle-inhibit state source (compositor query live).
pub trait IdleSource {
    /// Whether the compositor binding answers at all.
    fn present(&mut self) -> bool;
    /// Current inhibit flag (`None` = unreadable).
    fn inhibited(&mut self) -> Option<bool>;
}

/// Idle-inhibit control backend (compositor inhibit/uninhibit live).
pub trait IdleControlBackend {
    /// Sets the inhibit flag.
    fn set_inhibit(&mut self, on: bool) -> Result<(), String>;
}

/// Injected monotonic clock (milliseconds) for cadence control.
pub trait PowerClock {
    /// Current time in milliseconds.
    fn now_ms(&self) -> u64;
}

// ---------------------------------------------------------------------------
// Backlight read service
// ---------------------------------------------------------------------------

/// Backlight snapshot: bounded primitives only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BacklightSnapshot {
    /// Brightness percent, `0..=100`.
    pub pct: u8,
    /// Device brightness ceiling (raw units, for `set_step` math).
    pub max: u32,
    /// Milliseconds since the last good sample (0 on a fresh sample).
    pub stale_age_ms: u64,
}

/// Backlight-state read provider: rate-floored, grant-gated, fail-closed.
#[derive(Debug)]
pub struct BacklightStateService {
    interval_ms: u64,
    last_poll_ms: Option<u64>,
    last_sample_ms: Option<u64>,
    cached: Option<BacklightSnapshot>,
    grants: GrantSet,
}

impl BacklightStateService {
    /// Creates the service with the default interval and no grants.
    #[must_use]
    pub fn new() -> Self {
        Self {
            interval_ms: BACKLIGHT_DEFAULT_INTERVAL_MS,
            last_poll_ms: None,
            last_sample_ms: None,
            cached: None,
            grants: GrantSet::default(),
        }
    }

    /// Creates the service with a custom interval, clamped up to the
    /// floor. Configuration can never request sub-5 s polling.
    #[must_use]
    pub fn with_interval(requested_ms: u64) -> Self {
        Self {
            interval_ms: requested_ms.max(BACKLIGHT_POLL_FLOOR_MS),
            last_poll_ms: None,
            last_sample_ms: None,
            cached: None,
            grants: GrantSet::default(),
        }
    }

    /// Effective sampling interval in milliseconds.
    #[must_use]
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// Replaces the granted-head snapshot (narrowing takes effect on
    /// the next `get` with no restart).
    pub fn set_grants(&mut self, grants: GrantSet) {
        self.grants = grants;
    }

    /// Reads the provider: `None` when ungranted or hidden, otherwise
    /// the floored snapshot.
    pub fn get(
        &mut self,
        clock: &dyn PowerClock,
        source: &mut dyn BacklightSource,
    ) -> Option<BacklightSnapshot> {
        if !self.grants.contains(BACKLIGHT_READ_HEAD) {
            return None;
        }
        let now = clock.now_ms();
        let due = self
            .last_poll_ms
            .is_none_or(|last| now.wrapping_sub(last) >= self.interval_ms);
        if !due {
            return self.held(now);
        }
        self.last_poll_ms = Some(now);
        if !source.available() {
            return self.stale_or_hidden(now);
        }
        // A missing device or unreadable levels is a lost source, not a
        // zero-brightness claim: serve stale or hide, never zeros.
        let sample = match source.primary_device() {
            Some(device) => source
                .levels(&device)
                .and_then(|(brightness, max)| sample_levels(brightness, max)),
            None => None,
        };
        let Some(snapshot) = sample else {
            return self.stale_or_hidden(now);
        };
        self.last_sample_ms = Some(now);
        self.cached = Some(snapshot.clone());
        Some(snapshot)
    }

    /// Serves the held snapshot with a refreshed age (floored polls).
    fn held(&self, now: u64) -> Option<BacklightSnapshot> {
        let mut snapshot = self.cached.clone()?;
        let last = self.last_sample_ms?;
        snapshot.stale_age_ms = now.wrapping_sub(last);
        Some(snapshot)
    }

    /// Serves the last good snapshot while inside the stale bound, else
    /// hides (`None`).
    fn stale_or_hidden(&mut self, now: u64) -> Option<BacklightSnapshot> {
        let last = self.last_sample_ms?;
        let age = now.wrapping_sub(last);
        if age > BACKLIGHT_STALE_HIDE_MS {
            self.cached = None;
            self.last_sample_ms = None;
            return None;
        }
        let mut snapshot = self.cached.clone()?;
        snapshot.stale_age_ms = age;
        Some(snapshot)
    }
}

impl Default for BacklightStateService {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Backlight control head (privilege-decided, never escalating)
// ---------------------------------------------------------------------------

/// Detail carried when the control grant itself is missing.
pub const BACKLIGHT_DENIED_GRANT_DETAIL: &str =
    "backlight control requires the granted control head";

/// Detail carried when the backend reports the write is not permitted.
/// Names the remediation (udev `video` group membership or polkit
/// authorization) and the degradation contract (consumer notifies;
/// the provider never escalates, never setuids, never shells out).
pub const BACKLIGHT_DENIED_PERMISSION_DETAIL: &str = "backlight write not permitted (udev video group or polkit authorization required; unpermitted click degrades to notify, never escalates)";

/// Backlight control failure: typed, with a stable broker-facing code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BacklightControlError {
    /// Missing control grant, or the backend reports the write is not
    /// permitted. The brightness file is never touched on this path.
    NotPermitted {
        /// Which denial fired (grant vs permission; see the `*_DETAIL`
        /// constants).
        detail: &'static str,
    },
    /// The attempted write failed (carries the backend diagnostic).
    BackendUnavailable(String),
}

impl BacklightControlError {
    /// Stable error code for broker/diagnostic mapping.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotPermitted { .. } => "E_NOT_PERMITTED",
            Self::BackendUnavailable(_) => "E_BACKEND_UNAVAILABLE",
        }
    }
}

impl std::fmt::Display for BacklightControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPermitted { detail } => write!(f, "E_NOT_PERMITTED: {detail}"),
            Self::BackendUnavailable(detail) => {
                write!(f, "E_BACKEND_UNAVAILABLE: {detail}")
            }
        }
    }
}

impl std::error::Error for BacklightControlError {}

/// Builds a backlight snapshot from raw levels (`None` when the
/// ceiling is zero: no division by zero, fail-closed).
fn sample_levels(brightness: u32, max: u32) -> Option<BacklightSnapshot> {
    Some(BacklightSnapshot {
        pct: brightness_pct(brightness, max)?,
        max,
        stale_age_ms: 0,
    })
}

/// Backlight stepping control: usable only with the control grant, and
/// attempted only when the backend reports the write is permitted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BacklightControl {
    grants: GrantSet,
}

impl BacklightControl {
    /// Binds the control head to a grant snapshot.
    #[must_use]
    pub fn with_grants(grants: GrantSet) -> Self {
        Self { grants }
    }

    /// Steps brightness by `delta_pct` percentage points from (`current`,
    /// `max`), returning the absolute target the backend must write.
    ///
    /// Denials ([`BacklightControlError::NotPermitted`], missing grant or
    /// unpermitted backend) never touch the brightness file. The delta
    /// clamps into `[-100, 100]`; the target clamps into `0..=max`.
    pub fn set_step(
        &self,
        backend: &mut dyn BacklightBackend,
        max: u32,
        current: u32,
        delta_pct: i32,
    ) -> Result<u32, BacklightControlError> {
        if !self.grants.contains(BACKLIGHT_CONTROL_HEAD) {
            return Err(BacklightControlError::NotPermitted {
                detail: BACKLIGHT_DENIED_GRANT_DETAIL,
            });
        }
        // Privilege decision (bitty#2020): attempt only when permitted.
        // A `false` here ends the call: no escalation, no setuid, no
        // shell-out — the denial names the remediation and the consumer
        // degrades to notify.
        if !backend.permitted() {
            return Err(BacklightControlError::NotPermitted {
                detail: BACKLIGHT_DENIED_PERMISSION_DETAIL,
            });
        }
        if max == 0 {
            return Err(BacklightControlError::BackendUnavailable(
                "brightness ceiling is zero".to_string(),
            ));
        }
        let delta = delta_pct.clamp(-BACKLIGHT_STEP_MAX_PCT, BACKLIGHT_STEP_MAX_PCT);
        let current_pct = i64::from(brightness_pct(current, max).unwrap_or(0));
        let target_pct = (current_pct + i64::from(delta)).clamp(0, 100);
        let target = (target_pct as u64 * u64::from(max) / 100) as u32;
        backend
            .write_brightness(target)
            .map_err(BacklightControlError::BackendUnavailable)?;
        Ok(target)
    }
}

// ---------------------------------------------------------------------------
// Idle-inhibit read service + control head
// ---------------------------------------------------------------------------

/// Idle-inhibit snapshot: the compositor flag plus sample age.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdleSnapshot {
    /// Whether inhibition is currently active.
    pub inhibited: bool,
    /// Milliseconds since the last good sample (0 on a fresh sample).
    pub stale_age_ms: u64,
}

/// Idle-state read provider: rate-floored, grant-gated, fail-closed.
#[derive(Debug)]
pub struct IdleStateService {
    interval_ms: u64,
    last_poll_ms: Option<u64>,
    last_sample_ms: Option<u64>,
    cached: Option<IdleSnapshot>,
    grants: GrantSet,
}

impl IdleStateService {
    /// Creates the service with the default interval and no grants.
    #[must_use]
    pub fn new() -> Self {
        Self {
            interval_ms: IDLE_DEFAULT_INTERVAL_MS,
            last_poll_ms: None,
            last_sample_ms: None,
            cached: None,
            grants: GrantSet::default(),
        }
    }

    /// Creates the service with a custom interval, clamped up to the
    /// floor. Configuration can never request sub-5 s polling.
    #[must_use]
    pub fn with_interval(requested_ms: u64) -> Self {
        Self {
            interval_ms: requested_ms.max(IDLE_POLL_FLOOR_MS),
            last_poll_ms: None,
            last_sample_ms: None,
            cached: None,
            grants: GrantSet::default(),
        }
    }

    /// Effective sampling interval in milliseconds.
    #[must_use]
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// Replaces the granted-head snapshot (narrowing takes effect on
    /// the next `get` with no restart).
    pub fn set_grants(&mut self, grants: GrantSet) {
        self.grants = grants;
    }

    /// Reads the provider: `None` when ungranted or hidden, otherwise
    /// the floored snapshot.
    pub fn get(
        &mut self,
        clock: &dyn PowerClock,
        source: &mut dyn IdleSource,
    ) -> Option<IdleSnapshot> {
        if !self.grants.contains(IDLE_READ_HEAD) {
            return None;
        }
        let now = clock.now_ms();
        let due = self
            .last_poll_ms
            .is_none_or(|last| now.wrapping_sub(last) >= self.interval_ms);
        if !due {
            return self.held(now);
        }
        self.last_poll_ms = Some(now);
        if !source.present() {
            return self.stale_or_hidden(now);
        }
        let Some(inhibited) = source.inhibited() else {
            return self.stale_or_hidden(now);
        };
        let snapshot = IdleSnapshot {
            inhibited,
            stale_age_ms: 0,
        };
        self.last_sample_ms = Some(now);
        self.cached = Some(snapshot.clone());
        Some(snapshot)
    }

    /// Serves the held snapshot with a refreshed age (floored polls).
    fn held(&self, now: u64) -> Option<IdleSnapshot> {
        let mut snapshot = self.cached.clone()?;
        let last = self.last_sample_ms?;
        snapshot.stale_age_ms = now.wrapping_sub(last);
        Some(snapshot)
    }

    /// Serves the last good snapshot while inside the stale bound, else
    /// hides (`None`).
    fn stale_or_hidden(&mut self, now: u64) -> Option<IdleSnapshot> {
        let last = self.last_sample_ms?;
        let age = now.wrapping_sub(last);
        if age > IDLE_STALE_HIDE_MS {
            self.cached = None;
            self.last_sample_ms = None;
            return None;
        }
        let mut snapshot = self.cached.clone()?;
        snapshot.stale_age_ms = age;
        Some(snapshot)
    }
}

impl Default for IdleStateService {
    fn default() -> Self {
        Self::new()
    }
}

/// Idle-inhibit control failure: typed, with a stable broker-facing code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdleControlError {
    /// The control head is not granted. The backend is never touched.
    NotPermitted,
    /// The backend call failed (carries the backend diagnostic).
    BackendUnavailable(String),
}

impl IdleControlError {
    /// Stable error code for broker/diagnostic mapping.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotPermitted => "E_NOT_PERMITTED",
            Self::BackendUnavailable(_) => "E_BACKEND_UNAVAILABLE",
        }
    }
}

impl std::fmt::Display for IdleControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPermitted => write!(
                f,
                "E_NOT_PERMITTED: idle control requires the '{}' grant",
                IDLE_CONTROL_HEAD
            ),
            Self::BackendUnavailable(detail) => {
                write!(f, "E_BACKEND_UNAVAILABLE: {detail}")
            }
        }
    }
}

impl std::error::Error for IdleControlError {}

/// Idle-inhibit toggle control: usable only with the control grant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdleControl {
    grants: GrantSet,
}

impl IdleControl {
    /// Binds the control head to a grant snapshot.
    #[must_use]
    pub fn with_grants(grants: GrantSet) -> Self {
        Self { grants }
    }

    /// Sets the inhibit flag. Without the control head this fails
    /// closed and the backend is never invoked.
    pub fn set(
        &self,
        backend: &mut dyn IdleControlBackend,
        on: bool,
    ) -> Result<(), IdleControlError> {
        if !self.grants.contains(IDLE_CONTROL_HEAD) {
            return Err(IdleControlError::NotPermitted);
        }
        backend
            .set_inhibit(on)
            .map_err(IdleControlError::BackendUnavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeClock {
        now: u64,
    }

    impl PowerClock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now
        }
    }

    struct FakeBacklightSource {
        available: bool,
        device: Option<String>,
        levels: Option<(u32, u32)>,
    }

    impl BacklightSource for FakeBacklightSource {
        fn available(&mut self) -> bool {
            self.available
        }

        fn primary_device(&mut self) -> Option<String> {
            self.device.clone()
        }

        fn levels(&mut self, _device: &str) -> Option<(u32, u32)> {
            self.levels
        }
    }

    struct FakeBacklightBackend {
        permitted: bool,
        writes: Vec<u32>,
        fail: Option<String>,
    }

    impl BacklightBackend for FakeBacklightBackend {
        fn permitted(&self) -> bool {
            self.permitted
        }

        fn write_brightness(&mut self, value: u32) -> Result<(), String> {
            self.writes.push(value);
            if let Some(detail) = &self.fail {
                return Err(detail.clone());
            }
            Ok(())
        }
    }

    struct FakeIdleSource {
        present: bool,
        inhibited: Option<bool>,
    }

    impl IdleSource for FakeIdleSource {
        fn present(&mut self) -> bool {
            self.present
        }

        fn inhibited(&mut self) -> Option<bool> {
            self.inhibited
        }
    }

    struct FakeIdleBackend {
        calls: Vec<bool>,
        fail: Option<String>,
    }

    impl IdleControlBackend for FakeIdleBackend {
        fn set_inhibit(&mut self, on: bool) -> Result<(), String> {
            self.calls.push(on);
            if let Some(detail) = &self.fail {
                return Err(detail.clone());
            }
            Ok(())
        }
    }

    fn backlight_grants() -> GrantSet {
        GrantSet::new(vec![BACKLIGHT_READ_HEAD.to_string()])
    }

    fn backlight_control_grants() -> GrantSet {
        GrantSet::new(vec![BACKLIGHT_CONTROL_HEAD.to_string()])
    }

    fn healthy_backlight() -> FakeBacklightSource {
        FakeBacklightSource {
            available: true,
            device: Some("intel_backlight".to_string()),
            levels: Some((300, 1_200)),
        }
    }

    #[test]
    fn brightness_files_parse_strictly() {
        assert_eq!(parse_brightness("300\n"), Some(300));
        assert_eq!(parse_brightness("  1200  "), Some(1_200));
        assert_eq!(parse_brightness("0"), Some(0));
        assert_eq!(parse_brightness(""), None);
        assert_eq!(parse_brightness("-5"), None);
        assert_eq!(parse_brightness("12.5"), None);
        assert_eq!(parse_brightness("0x10"), None);
        assert_eq!(parse_brightness("300 "), Some(300));
        assert_eq!(parse_brightness("3 00"), None);
        assert_eq!(parse_brightness("99999999999"), None);
        let big = "1".repeat(BACKLIGHT_VALUE_MAX_BYTES + 1);
        assert_eq!(parse_brightness(&big), None);
    }

    #[test]
    fn brightness_pct_clamps_and_rejects_zero_max() {
        assert_eq!(brightness_pct(300, 1_200), Some(25));
        assert_eq!(brightness_pct(0, 1_200), Some(0));
        assert_eq!(brightness_pct(1_200, 1_200), Some(100));
        assert_eq!(brightness_pct(9_999, 1_200), Some(100));
        assert_eq!(brightness_pct(1, 3), Some(33));
        assert_eq!(brightness_pct(100, 0), None);
    }

    #[test]
    fn ungranted_backlight_read_is_indistinguishable_from_absent() {
        let mut service = BacklightStateService::new();
        let mut source = healthy_backlight();
        assert_eq!(service.get(&FakeClock { now: 0 }, &mut source), None);
    }

    #[test]
    fn backlight_read_serves_pct_with_age() {
        let mut service = BacklightStateService::new();
        service.set_grants(backlight_grants());
        let mut source = healthy_backlight();
        let snapshot = service
            .get(&FakeClock { now: 0 }, &mut source)
            .expect("healthy serves");
        assert_eq!(snapshot.pct, 25);
        assert_eq!(snapshot.max, 1_200);
        assert_eq!(snapshot.stale_age_ms, 0);
        // No device or unreadable levels hide fail-closed.
        let mut service = BacklightStateService::new();
        service.set_grants(backlight_grants());
        let mut no_device = FakeBacklightSource {
            available: true,
            device: None,
            levels: None,
        };
        assert_eq!(
            service.get(&FakeClock { now: 0 }, &mut no_device),
            None,
            "device-less sysfs hides on a fresh read"
        );
    }

    #[test]
    fn backlight_source_loss_hides_after_bound() {
        let mut service = BacklightStateService::new();
        service.set_grants(backlight_grants());
        let mut source = healthy_backlight();
        service
            .get(&FakeClock { now: 0 }, &mut source)
            .expect("first serves");
        source.available = false;
        let stale = service
            .get(
                &FakeClock {
                    now: BACKLIGHT_POLL_FLOOR_MS,
                },
                &mut source,
            )
            .expect("stale serves before the bound");
        assert!(stale.stale_age_ms >= BACKLIGHT_POLL_FLOOR_MS);
        let hidden = service.get(
            &FakeClock {
                now: BACKLIGHT_STALE_HIDE_MS + BACKLIGHT_POLL_FLOOR_MS,
            },
            &mut source,
        );
        assert_eq!(hidden, None, "lost source hides after the bound");
    }

    #[test]
    fn unpermitted_backlight_set_fails_closed_without_escalation() {
        // Read-only sysfs fixture: permitted is false, so the write must
        // never be attempted however the grant snapshot looks.
        let control = BacklightControl::with_grants(backlight_control_grants());
        let mut backend = FakeBacklightBackend {
            permitted: false,
            writes: Vec::new(),
            fail: None,
        };
        let error = control
            .set_step(&mut backend, 1_200, 300, 10)
            .expect_err("unpermitted set must fail");
        assert_eq!(
            error,
            BacklightControlError::NotPermitted {
                detail: BACKLIGHT_DENIED_PERMISSION_DETAIL
            }
        );
        assert_eq!(error.code(), "E_NOT_PERMITTED");
        assert!(
            backend.writes.is_empty(),
            "denied set must never touch the brightness file"
        );
    }

    #[test]
    fn ungranted_backlight_set_fails_closed_without_touching_backend() {
        let control = BacklightControl::with_grants(backlight_grants());
        let mut backend = FakeBacklightBackend {
            permitted: true,
            writes: Vec::new(),
            fail: None,
        };
        let error = control
            .set_step(&mut backend, 1_200, 300, 10)
            .expect_err("ungranted set must fail");
        assert_eq!(
            error,
            BacklightControlError::NotPermitted {
                detail: BACKLIGHT_DENIED_GRANT_DETAIL
            }
        );
        assert!(backend.writes.is_empty());
    }

    #[test]
    fn permitted_backlight_set_steps_and_clamps() {
        let control = BacklightControl::with_grants(backlight_control_grants());
        let mut backend = FakeBacklightBackend {
            permitted: true,
            writes: Vec::new(),
            fail: None,
        };
        // 300/1200 = 25% + 10 -> 35% -> 420 units.
        let target = control
            .set_step(&mut backend, 1_200, 300, 10)
            .expect("permitted steps");
        assert_eq!(target, 420);
        assert_eq!(backend.writes, vec![420]);
        // Over-range deltas clamp instead of wrapping or erroring.
        let target = control
            .set_step(&mut backend, 1_200, 300, 500)
            .expect("clamps high");
        assert_eq!(target, 1_200);
        let target = control
            .set_step(&mut backend, 1_200, 300, -500)
            .expect("clamps low");
        assert_eq!(target, 0);
    }

    #[test]
    fn idle_read_and_control_split_heads() {
        let mut service = IdleStateService::new();
        let mut source = FakeIdleSource {
            present: true,
            inhibited: Some(true),
        };
        assert_eq!(service.get(&FakeClock { now: 0 }, &mut source), None);
        service.set_grants(GrantSet::new(vec![IDLE_READ_HEAD.to_string()]));
        let snapshot = service
            .get(&FakeClock { now: 0 }, &mut source)
            .expect("granted idle serves");
        assert!(snapshot.inhibited);
        assert_eq!(snapshot.stale_age_ms, 0);
        // Read grant never implies control.
        let control = IdleControl::with_grants(GrantSet::new(vec![IDLE_READ_HEAD.to_string()]));
        let mut backend = FakeIdleBackend {
            calls: Vec::new(),
            fail: None,
        };
        let error = control
            .set(&mut backend, false)
            .expect_err("read grant must not toggle");
        assert_eq!(error, IdleControlError::NotPermitted);
        assert_eq!(error.code(), "E_NOT_PERMITTED");
        assert!(backend.calls.is_empty());
        // Granted control toggles; backend failure maps typed.
        let control = IdleControl::with_grants(GrantSet::new(vec![IDLE_CONTROL_HEAD.to_string()]));
        control.set(&mut backend, true).expect("granted toggles");
        assert_eq!(backend.calls, vec![true]);
        backend.fail = Some("compositor dropped the request".to_string());
        let error = control
            .set(&mut backend, false)
            .expect_err("backend failure surfaces");
        assert_eq!(
            error,
            IdleControlError::BackendUnavailable("compositor dropped the request".to_string())
        );
        assert_eq!(error.code(), "E_BACKEND_UNAVAILABLE");
    }

    #[test]
    fn idle_absent_compositor_hides_fail_closed() {
        let mut service = IdleStateService::new();
        service.set_grants(GrantSet::new(vec![IDLE_READ_HEAD.to_string()]));
        let mut source = FakeIdleSource {
            present: false,
            inhibited: None,
        };
        assert_eq!(service.get(&FakeClock { now: 0 }, &mut source), None);
    }

    #[test]
    fn power_intervals_clamp_up_to_floors() {
        assert_eq!(
            BacklightStateService::with_interval(100).interval_ms(),
            BACKLIGHT_POLL_FLOOR_MS
        );
        assert_eq!(
            IdleStateService::with_interval(100).interval_ms(),
            IDLE_POLL_FLOOR_MS
        );
        assert_eq!(
            BacklightStateService::new().interval_ms(),
            BACKLIGHT_DEFAULT_INTERVAL_MS
        );
        assert_eq!(
            IdleStateService::new().interval_ms(),
            IDLE_DEFAULT_INTERVAL_MS
        );
    }
}
