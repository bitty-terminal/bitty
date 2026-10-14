//! Backlight plus idle-inhibit host bridges: privilege-decided control.
//!
//! Wraps the merged `bitty-platform` samplers (`CTX-1197`, bitty#2020,
//! `DEC-0115`) with the privilege paths end to end. Backlight read is
//! unprivileged sysfs text; backlight set attempts the write only when
//! permitted and reports `E_NOT_PERMITTED` otherwise (never escalates,
//! never setuids, never shells out; an unpermitted click degrades to notify
//! consumer-side). Idle inhibit is compositor-side Wayland
//! (`zwp_idle_inhibit_manager_v1`): the state shape plus the separate toggle
//! head live here, and the live compositor binding is host work behind the
//! injected backend seam (the in-memory backend below proves the toggle
//! contract headlessly; per-compositor round-trip notes belong to the host
//! follow-up).
//!
//! Live backlight I/O uses only `std::fs` (existing seam, no new
//! dependency). D-Bus-adjacent permission checks stay file-permission checks
//! (udev `video` group membership surfaces as writability); no polkit agent,
//! no new edge.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use bitty_lua::{BridgeError, LuaValue};
use bitty_platform::backlight_state::{
    BACKLIGHT_BRIGHTNESS_FILE, BACKLIGHT_MAX_FILE, BacklightBackend, BacklightControl,
    BacklightControlError, BacklightSnapshot, BacklightSource, BacklightStateService, IdleControl,
    IdleControlBackend, IdleControlError, IdleSnapshot, IdleSource, IdleStateService, PowerClock,
    parse_brightness,
};
use bitty_platform::providers::GrantSet;

/// Translates broker-granted heads into the platform [`GrantSet`].
#[must_use]
pub fn grants_from_broker(granted: &[String]) -> GrantSet {
    let mut heads = Vec::new();
    for head in granted {
        match head.as_str() {
            "platform.backlight.read" => {
                heads.push(bitty_platform::providers::BACKLIGHT_READ_HEAD.to_string());
            }
            "platform.backlight.control" => {
                heads.push(bitty_platform::providers::BACKLIGHT_CONTROL_HEAD.to_string());
            }
            "platform.idle.read" => {
                heads.push(bitty_platform::providers::IDLE_READ_HEAD.to_string());
            }
            "platform.idle.control" => {
                heads.push(bitty_platform::providers::IDLE_CONTROL_HEAD.to_string());
            }
            _ => {}
        }
    }
    GrantSet::new(heads)
}

/// Marshal one served [`BacklightSnapshot`] into the bar-facing Lua table.
///
/// Exact keys: `pct`, `max`, `stale_age_ms`. Toggle semantics plus icons
/// stay Lua-side (config-owned glyphs).
#[must_use]
pub fn backlight_snapshot_to_lua(snapshot: &BacklightSnapshot) -> LuaValue {
    LuaValue::Table(vec![
        (
            LuaValue::String("pct".to_string()),
            LuaValue::Integer(i64::from(snapshot.pct)),
        ),
        (
            LuaValue::String("max".to_string()),
            LuaValue::Integer(i64::from(snapshot.max)),
        ),
        (
            LuaValue::String("stale_age_ms".to_string()),
            LuaValue::Integer(snapshot.stale_age_ms as i64),
        ),
    ])
}

/// Marshal one served [`IdleSnapshot`] into the bar-facing Lua table.
///
/// Exact keys: `inhibited`, `stale_age_ms`. The activated/deactivated glyph
/// switch stays config-side.
#[must_use]
pub fn idle_snapshot_to_lua(snapshot: &IdleSnapshot) -> LuaValue {
    LuaValue::Table(vec![
        (
            LuaValue::String("inhibited".to_string()),
            LuaValue::Bool(snapshot.inhibited),
        ),
        (
            LuaValue::String("stale_age_ms".to_string()),
            LuaValue::Integer(snapshot.stale_age_ms as i64),
        ),
    ])
}

/// Maps a backlight control failure into [`BridgeError`], preserving
/// `E_NOT_PERMITTED` with its naming diagnostic (grant vs permission).
fn backlight_error_to_bridge(error: BacklightControlError) -> BridgeError {
    BridgeError::new("runtime", error.code(), error.to_string())
}

/// Maps an idle control failure into [`BridgeError`], preserving codes.
fn idle_error_to_bridge(error: IdleControlError) -> BridgeError {
    BridgeError::new("runtime", error.code(), error.to_string())
}

/// Live backlight read source over an injected class root (production:
/// `/sys/class/backlight`; tests inject fixtures).
///
/// Enumeration is bounded and deterministic: the first sorted device wins
/// (documented), unreadable entries disqualify per-entry, and a missing
/// device hides fail-closed (never zero-brightness).
pub struct SysfsBacklightSource {
    class_root: PathBuf,
}

/// Maximum device entries scanned per poll (bounded enumeration).
const BACKLIGHT_MAX_DEVICES: usize = 32;

impl SysfsBacklightSource {
    /// Binds the standard Linux class root.
    #[must_use]
    pub fn system() -> Self {
        Self {
            class_root: PathBuf::from("/sys/class/backlight"),
        }
    }

    /// Binds an injected class root (fixtures, tests).
    #[must_use]
    pub fn with_root(class_root: PathBuf) -> Self {
        Self { class_root }
    }

    fn primary_device_name(&self) -> Option<String> {
        let entries = std::fs::read_dir(&self.class_root).ok()?;
        let mut names: Vec<String> = entries
            .take(BACKLIGHT_MAX_DEVICES + 1)
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.is_empty()
                    || name.len() > 64
                    || name.contains('/')
                    || name.contains('\0')
                    || name.starts_with('.')
                {
                    return None;
                }
                Some(name)
            })
            .collect();
        names.sort();
        names.truncate(BACKLIGHT_MAX_DEVICES);
        names.into_iter().next()
    }

    fn read_levels(&self, device: &str) -> Option<(u32, u32)> {
        let base = Path::new(&self.class_root).join(device);
        let brightness = std::fs::read_to_string(base.join(BACKLIGHT_BRIGHTNESS_FILE)).ok()?;
        let max = std::fs::read_to_string(base.join(BACKLIGHT_MAX_FILE)).ok()?;
        let brightness = parse_brightness(&brightness)?;
        let max = parse_brightness(&max)?;
        Some((brightness, max))
    }
}

impl BacklightSource for SysfsBacklightSource {
    fn available(&mut self) -> bool {
        self.class_root.is_dir()
    }

    fn primary_device(&mut self) -> Option<String> {
        self.primary_device_name()
    }

    fn levels(&mut self, device: &str) -> Option<(u32, u32)> {
        if device.is_empty() || device.len() > 64 || device.contains('/') || device.contains('\0') {
            return None;
        }
        self.read_levels(device)
    }
}

/// Live backlight control backend over an injected class root.
///
/// `permitted` answers writability without side effects (read-only sysfs
/// fixture denies with zero backend writes). The control head never
/// escalates past a `false`: no setuid, no shell-out.
pub struct SysfsBacklightBackend {
    class_root: PathBuf,
    device: String,
}

impl SysfsBacklightBackend {
    /// Binds the standard root plus the primary device name.
    #[must_use]
    pub fn system(device: String) -> Self {
        Self {
            class_root: PathBuf::from("/sys/class/backlight"),
            device,
        }
    }

    /// Binds injected roots (fixtures, tests).
    #[must_use]
    pub fn with_root(class_root: PathBuf, device: String) -> Self {
        Self { class_root, device }
    }

    fn brightness_path(&self) -> Option<PathBuf> {
        if self.device.is_empty()
            || self.device.len() > 64
            || self.device.contains('/')
            || self.device.contains('\0')
        {
            return None;
        }
        Some(
            Path::new(&self.class_root)
                .join(&self.device)
                .join(BACKLIGHT_BRIGHTNESS_FILE),
        )
    }
}

impl BacklightBackend for SysfsBacklightBackend {
    fn permitted(&self) -> bool {
        let Some(path) = self.brightness_path() else {
            return false;
        };
        match std::fs::metadata(&path) {
            Ok(meta) => !meta.permissions().readonly(),
            Err(_) => false,
        }
    }

    fn write_brightness(&mut self, value: u32) -> Result<(), String> {
        let Some(path) = self.brightness_path() else {
            return Err("brightness path is not writable".to_string());
        };
        std::fs::write(path, value.to_string()).map_err(|e| format!("write failed: {e}"))?;
        Ok(())
    }
}

/// In-memory backlight backend for tests (records writes, injects denial).
#[derive(Debug, Default)]
pub struct MemBacklightBackend {
    /// Whether a write may be attempted right now.
    pub permitted_flag: bool,
    /// Absolute values admitted to the backend, in call order.
    pub writes: Vec<u32>,
    /// When set, admitted writes fail with this diagnostic.
    pub fail: Option<String>,
}

impl BacklightBackend for MemBacklightBackend {
    fn permitted(&self) -> bool {
        self.permitted_flag
    }

    fn write_brightness(&mut self, value: u32) -> Result<(), String> {
        self.writes.push(value);
        if let Some(detail) = &self.fail {
            return Err(detail.clone());
        }
        Ok(())
    }
}

/// In-memory idle source for tests and headless proofs.
#[derive(Debug, Clone)]
pub struct MemIdleSource {
    /// Whether the compositor binding answers at all.
    pub present: bool,
    /// Current inhibit flag (`None` = unreadable).
    pub inhibited: Option<bool>,
}

impl IdleSource for MemIdleSource {
    fn present(&mut self) -> bool {
        self.present
    }

    fn inhibited(&mut self) -> Option<bool> {
        self.inhibited
    }
}

/// Fail-closed idle stub: no compositor binding, so the provider hides.
#[derive(Debug, Default)]
pub struct NoIdleSource;

impl IdleSource for NoIdleSource {
    fn present(&mut self) -> bool {
        false
    }

    fn inhibited(&mut self) -> Option<bool> {
        None
    }
}

/// In-memory idle-inhibit backend: the compositor path behind the seam.
///
/// Records the last requested flag so the inhibit round-trip is provable
/// headlessly; the live Wayland `zwp_idle_inhibit_manager_v1` binding (plus
/// the Bitty surface it needs) is host/compositor work documented as a
/// follow-up, not a new edge here.
#[derive(Debug, Default)]
pub struct MemIdleBackend {
    /// Inhibit values admitted to the backend, in call order.
    pub calls: Vec<bool>,
    /// Current inhibit flag (mirrors the last admitted call).
    pub inhibited: bool,
    /// When set, admitted calls fail with this diagnostic.
    pub fail: Option<String>,
}

impl IdleControlBackend for MemIdleBackend {
    fn set_inhibit(&mut self, on: bool) -> Result<(), String> {
        self.calls.push(on);
        if let Some(detail) = &self.fail {
            return Err(detail.clone());
        }
        self.inhibited = on;
        Ok(())
    }
}

/// Host bridge for backlight read: owns the platform sampler.
pub struct BacklightBridge<S> {
    service: BacklightStateService,
    source: S,
}

impl<S> BacklightBridge<S>
where
    S: BacklightSource,
{
    /// Binds the sampler to its source with no grants (denies everything).
    pub fn new(source: S) -> Self {
        Self {
            service: BacklightStateService::new(),
            source,
        }
    }

    /// Replaces the granted-head snapshot from broker heads.
    pub fn set_broker_grants(&mut self, granted: &[String]) {
        self.service.set_grants(grants_from_broker(granted));
    }

    /// Replaces the grant snapshot directly (tests).
    pub fn set_grants(&mut self, grants: GrantSet) {
        self.service.set_grants(grants);
    }

    /// Reads the provider: `None` when ungranted or hidden, otherwise the
    /// floored Lua table.
    pub fn get(&mut self, clock: &dyn PowerClock) -> Option<LuaValue> {
        let snapshot = self.service.get(clock, &mut self.source)?;
        Some(backlight_snapshot_to_lua(&snapshot))
    }
}

/// Host bridge for idle-inhibit read: owns the platform sampler.
pub struct IdleBridge<S> {
    service: IdleStateService,
    source: S,
}

impl<S> IdleBridge<S>
where
    S: IdleSource,
{
    /// Binds the sampler to its source with no grants (denies everything).
    pub fn new(source: S) -> Self {
        Self {
            service: IdleStateService::new(),
            source,
        }
    }

    /// Replaces the granted-head snapshot from broker heads.
    pub fn set_broker_grants(&mut self, granted: &[String]) {
        self.service.set_grants(grants_from_broker(granted));
    }

    /// Replaces the grant snapshot directly (tests).
    pub fn set_grants(&mut self, grants: GrantSet) {
        self.service.set_grants(grants);
    }

    /// Reads the provider: `None` when ungranted or hidden, otherwise the
    /// floored Lua table.
    pub fn get(&mut self, clock: &dyn PowerClock) -> Option<LuaValue> {
        let snapshot = self.service.get(clock, &mut self.source)?;
        Some(idle_snapshot_to_lua(&snapshot))
    }
}

/// Dispatches `backlight.control.set_step` with the `DEC-0115` privilege
/// decision: missing grant or unpermitted backend fails closed with
/// `E_NOT_PERMITTED` and zero backend writes; never escalates.
pub fn backlight_set_step(
    granted: &[String],
    backend: &mut dyn BacklightBackend,
    max: u32,
    current: u32,
    delta_pct: i32,
) -> Result<u32, BridgeError> {
    let control = BacklightControl::with_grants(grants_from_broker(granted));
    control
        .set_step(backend, max, current, delta_pct)
        .map_err(backlight_error_to_bridge)
}

/// Dispatches `idle.control.set` honoring `E_NOT_PERMITTED`.
///
/// Without the control head the backend is never touched; backend failures
/// map to `E_BACKEND_UNAVAILABLE`.
pub fn idle_set(
    granted: &[String],
    backend: &mut dyn IdleControlBackend,
    on: bool,
) -> Result<(), BridgeError> {
    let control = IdleControl::with_grants(grants_from_broker(granted));
    control.set(backend, on).map_err(idle_error_to_bridge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_platform::providers::{
        BACKLIGHT_CONTROL_HEAD, BACKLIGHT_READ_HEAD, IDLE_CONTROL_HEAD, IDLE_READ_HEAD,
    };

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

    #[test]
    fn broker_grant_translation_is_deny_by_default() {
        assert!(grants_from_broker(&[]).is_empty());
        let grants = grants_from_broker(&["platform.backlight.read".to_string()]);
        assert!(grants.contains(BACKLIGHT_READ_HEAD));
        assert!(!grants.contains(BACKLIGHT_CONTROL_HEAD));
        let grants = grants_from_broker(&["platform.idle.control".to_string()]);
        assert!(grants.contains(IDLE_CONTROL_HEAD));
        assert!(!grants.contains(IDLE_READ_HEAD));
    }

    #[test]
    fn unpermitted_set_fails_closed_without_escalation() {
        let mut backend = MemBacklightBackend {
            permitted_flag: false,
            writes: Vec::new(),
            fail: None,
        };
        let error = backlight_set_step(
            &["platform.backlight.control".to_string()],
            &mut backend,
            1_200,
            300,
            10,
        )
        .expect_err("unpermitted set must fail");
        assert_eq!(error.code, "E_NOT_PERMITTED");
        assert!(backend.writes.is_empty());
    }

    #[test]
    fn ungranted_set_fails_closed_without_touching_backend() {
        let mut backend = MemBacklightBackend {
            permitted_flag: true,
            writes: Vec::new(),
            fail: None,
        };
        let error = backlight_set_step(
            &["platform.backlight.read".to_string()],
            &mut backend,
            1_200,
            300,
            10,
        )
        .expect_err("ungranted set must fail");
        assert_eq!(error.code, "E_NOT_PERMITTED");
        assert!(backend.writes.is_empty());
    }

    #[test]
    fn permitted_set_steps_and_clamps_end_to_end() {
        let mut backend = MemBacklightBackend {
            permitted_flag: true,
            writes: Vec::new(),
            fail: None,
        };
        let target = backlight_set_step(
            &["platform.backlight.control".to_string()],
            &mut backend,
            1_200,
            300,
            10,
        )
        .expect("permitted steps");
        assert_eq!(target, 420);
        assert_eq!(backend.writes, vec![420]);
    }

    #[test]
    fn backlight_read_serves_pct_with_age() {
        let mut bridge = BacklightBridge::new(FakeBacklightSource {
            available: true,
            device: Some("intel_backlight".to_string()),
            levels: Some((300, 1_200)),
        });
        bridge.set_grants(GrantSet::new(vec![BACKLIGHT_READ_HEAD.to_string()]));
        let value = bridge.get(&FakeClock { now: 0 }).expect("serves");
        assert_eq!(value.get("pct"), Some(&LuaValue::Integer(25)));
        assert_eq!(value.get("max"), Some(&LuaValue::Integer(1_200)));
        assert_eq!(value.get("stale_age_ms"), Some(&LuaValue::Integer(0)));
    }

    #[test]
    fn idle_toggle_round_trips_through_compositor_seam() {
        let mut bridge = IdleBridge::new(MemIdleSource {
            present: true,
            inhibited: Some(false),
        });
        bridge.set_grants(GrantSet::new(vec![IDLE_READ_HEAD.to_string()]));
        let value = bridge.get(&FakeClock { now: 0 }).expect("serves");
        assert_eq!(value.get("inhibited"), Some(&LuaValue::Bool(false)));

        let mut backend = MemIdleBackend::default();
        idle_set(&["platform.idle.control".to_string()], &mut backend, true)
            .expect("granted toggles");
        assert_eq!(backend.calls, vec![true]);
        assert!(backend.inhibited);

        let mut bridge = IdleBridge::new(MemIdleSource {
            present: true,
            inhibited: Some(backend.inhibited),
        });
        bridge.set_grants(GrantSet::new(vec![IDLE_READ_HEAD.to_string()]));
        let value = bridge.get(&FakeClock { now: 0 }).expect("serves");
        assert_eq!(value.get("inhibited"), Some(&LuaValue::Bool(true)));

        let error = idle_set(&["platform.idle.read".to_string()], &mut backend, false)
            .expect_err("read never implies control");
        assert_eq!(error.code, "E_NOT_PERMITTED");
        assert_eq!(
            backend.calls,
            vec![true],
            "denied toggle never reaches backend"
        );
    }

    #[test]
    fn idle_absent_compositor_hides_fail_closed() {
        let mut bridge = IdleBridge::new(NoIdleSource);
        bridge.set_grants(GrantSet::new(vec![IDLE_READ_HEAD.to_string()]));
        assert_eq!(bridge.get(&FakeClock { now: 0 }), None);
    }
}
