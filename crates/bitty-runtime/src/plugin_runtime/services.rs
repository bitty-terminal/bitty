//! Host-service boundary for one plugin generation (RFC Gap C, minimal slice).
//!
//! `PluginServices` is the object-safe [`HostServices`] implementation handed
//! to a plugin VM. It owns the plugin-scoped store and defers settings and
//! terminal snapshots to injected, generation-stable sources. Capability
//! gating is evaluated from the grant snapshot taken at activation; an absent
//! grant fails closed before any side effect.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use bitty_lua::ui::{UI_MAX_AGGREGATED_TEXT_BYTES, UI_MAX_BLOCKS, UiNode};
use bitty_lua::{BridgeError, HostServices, LuaValue, SNAPSHOT_MAX_BYTES};
use bitty_plugin_host::bundled::{WORKSPACELINE_CLAIM, canonicalize_ui_claim};

use super::store::{self, PluginStore};

/// Injected spawn backend for one plugin generation.
///
/// Receives the Lua-validated argv and returns the bounded result table
/// (`output`/`stderr`/`truncated`/`exit_code`/`execution_id`/`untrusted`). The production
/// backend is the consent-gated [`SpawnService`](super::spawn::SpawnService)
/// path wired at activation; tests inject canned closures. `None` (no
/// backend) fails closed with `E_SPAWN_UNAVAILABLE`.
pub type SpawnHandler = Rc<dyn Fn(&[String]) -> Result<LuaValue, BridgeError>>;

/// Read-only typed settings source (owned by `bitty-config` in the app).
pub trait SettingsSource {
    /// Read one setting; `None` when absent.
    fn get(&self, key: &str) -> Option<LuaValue>;
}

/// Bounded committed terminal-snapshot source.
pub trait SnapshotSource {
    /// Read a bounded snapshot for `scope`.
    ///
    /// # Errors
    ///
    /// Returns a typed `E_SNAPSHOT_*`/`E_CAPABILITY_DENIED` error.
    fn snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError>;
}

/// Settings source that has no settings.
#[derive(Debug, Default)]
pub struct EmptySettings;

impl SettingsSource for EmptySettings {
    fn get(&self, _key: &str) -> Option<LuaValue> {
        None
    }
}

/// Snapshot source that fails closed with `E_SNAPSHOT_UNAVAILABLE`.
#[derive(Debug, Default)]
pub struct UnavailableSnapshot;

impl SnapshotSource for UnavailableSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            "E_SNAPSHOT_UNAVAILABLE",
            "no committed terminal state is available for snapshot",
        ))
    }
}

/// One accepted notification handed to the platform asynchronously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Notification title.
    pub title: String,
    /// Notification body (bounded by the caller's bridge limits).
    pub body: String,
    /// Urgency hint.
    pub urgency: String,
}

/// Bounded notification queue (`RC-8` rate governance; overflow drops newest).
#[derive(Debug)]
pub struct NotificationQueue {
    items: VecDeque<Notification>,
    capacity: usize,
    dropped: u64,
}

impl NotificationQueue {
    /// Create a bounded queue.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::new(),
            capacity: capacity.max(1),
            dropped: 0,
        }
    }

    /// Push a notification; returns whether it was accepted.
    pub fn push(&mut self, notification: Notification) -> bool {
        if self.items.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.items.push_back(notification);
        true
    }

    /// Drain all queued notifications.
    pub fn drain(&mut self) -> Vec<Notification> {
        self.items.drain(..).collect()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Number dropped since creation.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// UI authorization snapshot for one plugin generation (CTX-0428).
///
/// Built from the activation grant (the same intersection the other gates
/// use) plus the manifest `[lazy].claims` list; absent grants fail closed at
/// call time and are never inferred from the plugin id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiAccess {
    /// `ui.rich` granted — declarative rich content.
    pub rich: bool,
    /// `ui.overlay` granted — the `overlay` slot additionally requires it.
    pub overlay: bool,
    /// Manifest `[lazy].claims` (exclusive slot claims; `tabline` only).
    pub claims: Vec<String>,
}

/// One mounted, generation-owned declarative block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiBlock {
    slot: String,
    node: UiNode,
    version: u32,
}

impl UiBlock {
    /// Accepted slot this block was mounted into.
    #[must_use]
    pub fn slot(&self) -> &str {
        &self.slot
    }

    /// Current validated scene subtree.
    #[must_use]
    pub fn node(&self) -> &UiNode {
        &self.node
    }

    /// Monotonic version (1 at mount, incremented by update).
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }
}

/// Bounded per-generation block registry (`SCN-4`/`SCN-5` numbers).
///
/// Handles are opaque, generation-owned integers encoded as
/// `(epoch << 32) | counter`: the registry lives inside one [`PluginServices`]
/// and the runtime pins `epoch` to the activation generation, so a handle from
/// a suspended, reloaded, or disposed generation is foreign to the next one
/// and `update` reports `false` instead of touching another generation's
/// blocks. Clearing the registry on suspend/dispose keeps the monotonic
/// counter, so handles never alias across a clear either.
///
/// The registry caps are host-owned fail-closed bounds for this bridge slice:
/// the accepted `SCN-4`/`SCN-5` budgets are terminal-wide (all plugins), and
/// the terminal-wide accounting is owned by the host composer. Exceeding a
/// registry cap fails closed with `E_UI_BLOCK_BUDGET` (`budget` class), a host
/// diagnostic pending an accepted stable plugin-visible code.
#[derive(Debug)]
pub struct UiBlocks {
    blocks: Vec<(i64, UiBlock)>,
    epoch: u32,
    next_counter: u32,
    aggregated_text_bytes: usize,
}

/// Bits of the handle reserved for the generation epoch. Capped at 31 so every
/// composed `i64` handle stays positive (the surface types handles as opaque
/// integers; positivity is diagnostic only).
const UI_HANDLE_EPOCH_BITS: u32 = 31;

/// Mask selecting the generation epoch bits of a handle.
const UI_HANDLE_EPOCH_MASK: u32 = (1 << UI_HANDLE_EPOCH_BITS) - 1;

impl Default for UiBlocks {
    fn default() -> Self {
        Self::new()
    }
}

impl UiBlocks {
    /// An empty registry (epoch `0`, first handle counter `1`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            epoch: 0,
            next_counter: 1,
            aggregated_text_bytes: 0,
        }
    }

    /// Pin the generation epoch this registry mints handles for.
    ///
    /// Set once per activation/reload before the generation's `init.lua` runs;
    /// changing it after mounts would strand live handles, so the runtime only
    /// calls this on a freshly built registry.
    fn set_epoch(&mut self, epoch: u32) {
        self.epoch = epoch;
    }

    /// Drop every retained block (`suspend`/`dispose` invalidation) while
    /// keeping the epoch and the monotonic counter, so a handle minted before
    /// the clear can never alias one minted after it.
    fn clear(&mut self) {
        self.blocks.clear();
        self.aggregated_text_bytes = 0;
    }

    /// Compose the next generation-owned handle, or fail closed when this
    /// generation has exhausted its counter (unreachable in practice).
    fn next_handle(&self) -> Result<i64, BridgeError> {
        if self.next_counter == u32::MAX {
            return Err(BridgeError::new(
                "budget",
                "E_UI_BLOCK_BUDGET",
                "ui block handle counter exhausted for this generation",
            ));
        }
        let epoch = i64::from(self.epoch & UI_HANDLE_EPOCH_MASK);
        Ok((epoch << 32) | i64::from(self.next_counter))
    }

    /// Number of retained blocks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether no block is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Look up one block by handle.
    #[must_use]
    pub fn get(&self, handle: i64) -> Option<&UiBlock> {
        self.blocks
            .iter()
            .find_map(|(candidate, block)| (*candidate == handle).then_some(block))
    }

    /// Iterate blocks in mount order.
    pub fn iter(&self) -> impl Iterator<Item = (i64, &UiBlock)> {
        self.blocks.iter().map(|(handle, block)| (*handle, block))
    }

    /// Retain one validated component, returning its generation-owned handle.
    fn mount(&mut self, slot: &str, node: UiNode) -> Result<i64, BridgeError> {
        if self.blocks.len() >= UI_MAX_BLOCKS {
            return Err(BridgeError::new(
                "budget",
                "E_UI_BLOCK_BUDGET",
                format!("ui block registry is full ({UI_MAX_BLOCKS} blocks)"),
            ));
        }
        let bytes = node.text_bytes();
        let total = self.aggregated_text_bytes.saturating_add(bytes);
        if total > UI_MAX_AGGREGATED_TEXT_BYTES {
            return Err(BridgeError::new(
                "budget",
                "E_UI_BLOCK_BUDGET",
                "ui block text budget exceeded (2 MiB aggregated)",
            ));
        }
        let handle = self.next_handle()?;
        self.next_counter = self.next_counter.saturating_add(1);
        self.aggregated_text_bytes = total;
        self.blocks.push((
            handle,
            UiBlock {
                slot: slot.to_string(),
                node,
                version: 1,
            },
        ));
        Ok(handle)
    }

    /// Replace one block's subtree; `Ok(false)` for a stale/foreign handle.
    fn update(&mut self, handle: i64, node: UiNode) -> Result<bool, BridgeError> {
        let Some(position) = self
            .blocks
            .iter()
            .position(|(candidate, _)| *candidate == handle)
        else {
            return Ok(false);
        };
        let old_bytes = self.blocks[position].1.node.text_bytes();
        let total = self
            .aggregated_text_bytes
            .saturating_sub(old_bytes)
            .saturating_add(node.text_bytes());
        if total > UI_MAX_AGGREGATED_TEXT_BYTES {
            return Err(BridgeError::new(
                "budget",
                "E_UI_BLOCK_BUDGET",
                "ui block text budget exceeded (2 MiB aggregated)",
            ));
        }
        self.aggregated_text_bytes = total;
        let block = &mut self.blocks[position].1;
        block.node = node;
        block.version = block.version.saturating_add(1);
        Ok(true)
    }
}

/// Per-generation host services for one plugin.
pub struct PluginServices {
    plugin_id: String,
    store: RefCell<PluginStore>,
    settings: Rc<dyn SettingsSource>,
    snapshot: Rc<dyn SnapshotSource>,
    notifications: Rc<RefCell<NotificationQueue>>,
    terminal_read: bool,
    platform_notify: bool,
    spawn_git: Cell<bool>,
    spawn_backend: RefCell<Option<SpawnHandler>>,
    ui_access: RefCell<UiAccess>,
    ui_blocks: RefCell<UiBlocks>,
}

impl PluginServices {
    /// Create services for `plugin_id` with an explicit grant snapshot.
    #[must_use]
    pub fn new(
        plugin_id: impl Into<String>,
        store: PluginStore,
        settings: Rc<dyn SettingsSource>,
        snapshot: Rc<dyn SnapshotSource>,
        notifications: Rc<RefCell<NotificationQueue>>,
        terminal_read: bool,
        platform_notify: bool,
    ) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            store: RefCell::new(store),
            settings,
            snapshot,
            notifications,
            terminal_read,
            platform_notify,
            spawn_git: Cell::new(false),
            spawn_backend: RefCell::new(None),
            ui_access: RefCell::new(UiAccess::default()),
            ui_blocks: RefCell::new(UiBlocks::new()),
        }
    }

    /// Grant the UI surfaces for this generation from the activation snapshot.
    ///
    /// Absent grants stay denied: the default [`UiAccess`] has every gate
    /// closed, so a caller that never sets this can never mount content.
    pub fn set_ui_access(&self, access: UiAccess) {
        *self.ui_access.borrow_mut() = access;
    }

    /// UI authorization currently applied to this generation.
    #[must_use]
    pub fn ui_access(&self) -> UiAccess {
        self.ui_access.borrow().clone()
    }

    /// Pin the UI handle epoch to the activation generation.
    ///
    /// Called once per activation/reload on the freshly built services, before
    /// the generation's `init.lua` runs, so handles minted by different
    /// generations never collide.
    pub fn set_ui_epoch(&self, epoch: u32) {
        self.ui_blocks.borrow_mut().set_epoch(epoch);
    }

    /// Invalidate every block handle this generation minted (suspend/dispose).
    ///
    /// The registry is host-side state: clearing it makes `bitty.ui.update`
    /// on a pre-suspend handle fail closed (`false`) after suspend and after
    /// resume until the plugin mounts again.
    pub fn clear_ui_blocks(&self) {
        self.ui_blocks.borrow_mut().clear();
    }

    /// Read-only mounted-block view (tests, diagnostics, presentation wiring).
    pub fn with_ui_blocks<R>(&self, f: impl FnOnce(&UiBlocks) -> R) -> R {
        f(&self.ui_blocks.borrow())
    }

    /// Grant (or revoke) the `process.spawn:git` Layer-2 spawn surface.
    ///
    /// Set from the activation grant snapshot (`process.spawn:git` present);
    /// absent grants fail closed at call time. The execution backend itself
    /// is injected separately via [`Self::set_spawn_backend`].
    pub fn set_spawn_git(&self, granted: bool) {
        self.spawn_git.set(granted);
    }

    /// Whether the `process.spawn:git` spawn surface is granted.
    #[must_use]
    pub fn has_spawn_git(&self) -> bool {
        self.spawn_git.get()
    }

    /// Inject the spawn execution backend (`None` restores fail-closed
    /// `E_SPAWN_UNAVAILABLE`).
    pub fn set_spawn_backend(&self, backend: Option<SpawnHandler>) {
        *self.spawn_backend.borrow_mut() = backend;
    }

    /// Read-only store view (tests, diagnostics).
    pub fn with_store<R>(&self, f: impl FnOnce(&PluginStore) -> R) -> R {
        f(&self.store.borrow())
    }

    /// Plugin id this service set belongs to.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }
}

impl HostServices for PluginServices {
    fn store_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(self.store.borrow().get(key))
    }

    fn store_set(&self, key: &str, value: LuaValue) -> Result<(), BridgeError> {
        self.store.borrow_mut().set(key, value)
    }

    fn settings_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        if key.is_empty() || key.len() > 128 {
            return Err(BridgeError::new(
                "validation",
                "E_SETTINGS_KEY_INVALID",
                "settings key must be 1..128 bytes",
            ));
        }
        Ok(self.settings.get(key))
    }

    fn terminal_snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError> {
        if !self.terminal_read {
            return Err(BridgeError::capability_denied("terminal.semantic-read"));
        }
        if scope != "semantic" {
            return Err(BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "only the semantic scope is supported",
            ));
        }
        let snapshot = self.snapshot.snapshot(scope)?;
        if store::encode_json(&snapshot).len() > SNAPSHOT_MAX_BYTES {
            return Err(BridgeError::new(
                "budget",
                "E_SNAPSHOT_TOO_LARGE",
                "terminal snapshot exceeds the 256 KiB ceiling",
            ));
        }
        Ok(snapshot)
    }

    fn notify_show(&self, payload: &LuaValue) -> Result<bool, BridgeError> {
        if !self.platform_notify {
            return Err(BridgeError::capability_denied("platform.notify"));
        }
        let title = match payload.get("title") {
            Some(LuaValue::String(s)) => s.clone(),
            _ => {
                return Err(BridgeError::new(
                    "validation",
                    "E_DEF_INVALID",
                    "notification title must be a string",
                ));
            }
        };
        let body = match payload.get("body") {
            Some(LuaValue::String(s)) => s.clone(),
            Some(LuaValue::Nil) | None => String::new(),
            _ => {
                return Err(BridgeError::new(
                    "validation",
                    "E_DEF_INVALID",
                    "notification body must be a string",
                ));
            }
        };
        let urgency = match payload.get("urgency") {
            Some(LuaValue::String(s)) => s.clone(),
            _ => "normal".to_string(),
        };
        let accepted = self.notifications.borrow_mut().push(Notification {
            plugin_id: self.plugin_id.clone(),
            title,
            body,
            urgency,
        });
        Ok(accepted)
    }

    fn process_spawn(&self, args: &[String]) -> Result<LuaValue, BridgeError> {
        if !self.spawn_git.get() {
            return Err(BridgeError::capability_denied("process.spawn:git"));
        }
        let backend = self.spawn_backend.borrow().clone().ok_or_else(|| {
            BridgeError::new(
                "runtime",
                "E_SPAWN_UNAVAILABLE",
                "host spawn backend is not configured",
            )
        })?;
        backend(args)
    }

    fn ui_mount(&self, slot: &str, component: &UiNode) -> Result<i64, BridgeError> {
        // The bridge already rejected unknown slots; re-check at the host
        // boundary so a direct caller can never reach the registry with one.
        if !bitty_lua::ui::is_ui_slot(slot) {
            return Err(bitty_lua::ui::component_invalid(format!(
                "unknown UI slot '{slot}'"
            )));
        }
        {
            let access = self.ui_access.borrow();
            if !access.rich {
                return Err(BridgeError::capability_denied("ui.rich"));
            }
            if slot == "overlay" && !access.overlay {
                return Err(BridgeError::capability_denied("ui.overlay"));
            }
            // Accepted: `tabline` is an exclusive claim (ADR-0009 `LUA-OQ-7`
            // plus the shipped `[lazy].claims` vocabulary in
            // `bitty-plugin-host::bundled`). Unclaimed mounts fail closed;
            // the register/claim reservation is owned by the plugin host. The
            // shipped claim grammar canonicalizes the deprecated `tabline`
            // alias to `workspaceline`, so both spellings satisfy the slot.
            let tabline_claimed = access
                .claims
                .iter()
                .any(|claim| canonicalize_ui_claim(claim) == Some(WORKSPACELINE_CLAIM));
            if slot == "tabline" && !tabline_claimed {
                return Err(BridgeError::new(
                    "validation",
                    "E_UI_CLAIM_REQUIRED",
                    "slot 'tabline' requires an exclusive claim declared in [lazy].claims",
                ));
            }
        }
        self.ui_blocks.borrow_mut().mount(slot, component.clone())
    }

    fn ui_update(&self, handle: i64, component: &UiNode) -> Result<bool, BridgeError> {
        if !self.ui_access.borrow().rich {
            return Err(BridgeError::capability_denied("ui.rich"));
        }
        self.ui_blocks
            .borrow_mut()
            .update(handle, component.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_runtime::store::PluginStore;
    use bitty_lua::ui::UI_MAX_TEXT_BYTES;

    fn services() -> PluginServices {
        PluginServices::new(
            "xuepoo.test",
            PluginStore::in_memory(),
            Rc::new(EmptySettings),
            Rc::new(UnavailableSnapshot),
            Rc::new(RefCell::new(NotificationQueue::new(8))),
            false,
            false,
        )
    }

    #[test]
    fn spawn_without_grant_denies_capability() {
        let services = services();
        let error = services
            .process_spawn(&["status".to_owned()])
            .expect_err("grant absent must deny");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
    }

    #[test]
    fn spawn_without_backend_is_unavailable() {
        let services = services();
        services.set_spawn_git(true);
        assert!(services.has_spawn_git());
        let error = services
            .process_spawn(&["status".to_owned()])
            .expect_err("backend absent must be unavailable");
        assert_eq!(error.code, "E_SPAWN_UNAVAILABLE");
    }

    #[test]
    fn spawn_with_backend_passes_argv_through() {
        let services = services();
        services.set_spawn_git(true);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let seen_clone = seen.clone();
        services.set_spawn_backend(Some(Rc::new(move |args: &[String]| {
            seen_clone.borrow_mut().push(args.to_vec());
            Ok(LuaValue::table([(
                "output",
                LuaValue::String("ok".to_owned()),
            )]))
        })));
        let result = services
            .process_spawn(&["status".to_owned(), "--porcelain".to_owned()])
            .expect("backend serves");
        assert_eq!(
            result.get("output"),
            Some(&LuaValue::String("ok".to_owned()))
        );
        assert_eq!(
            seen.borrow().as_slice(),
            &[vec!["status".to_owned(), "--porcelain".to_owned()]]
        );
    }

    fn ui_services(access: UiAccess) -> PluginServices {
        let services = services();
        services.set_ui_access(access);
        services
    }

    fn rich_only() -> UiAccess {
        UiAccess {
            rich: true,
            overlay: false,
            claims: Vec::new(),
        }
    }

    #[test]
    fn ui_mount_defaults_deny() {
        let services = services();
        let error = services
            .ui_mount("statusline", &UiNode::text("x"))
            .expect_err("default access must deny");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        services.with_ui_blocks(|blocks| assert!(blocks.is_empty()));
    }

    #[test]
    fn ui_mount_rejects_unknown_slot_at_host_boundary() {
        let services = ui_services(rich_only());
        let error = services
            .ui_mount("nowhere", &UiNode::text("x"))
            .expect_err("unknown slot must fail closed");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    }

    #[test]
    fn overlay_slot_requires_ui_overlay() {
        let services = ui_services(rich_only());
        services
            .ui_mount("statusline", &UiNode::text("ok"))
            .expect("statusline mount");
        let error = services
            .ui_mount("overlay", &UiNode::text("denied"))
            .expect_err("overlay needs ui.overlay");
        assert_eq!(error.code, "E_CAPABILITY_DENIED");
        services.set_ui_access(UiAccess {
            rich: true,
            overlay: true,
            claims: Vec::new(),
        });
        services
            .ui_mount("overlay", &UiNode::text("allowed"))
            .expect("overlay mount after grant");
        services.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 2));
    }

    #[test]
    fn tabline_slot_requires_exclusive_claim() {
        let services = ui_services(rich_only());
        let error = services
            .ui_mount("tabline", &UiNode::text("x"))
            .expect_err("unclaimed tabline must fail closed");
        assert_eq!(error.code, "E_UI_CLAIM_REQUIRED");
        services.set_ui_access(UiAccess {
            rich: true,
            overlay: false,
            claims: vec!["tabline".to_string()],
        });
        services
            .ui_mount("tabline", &UiNode::text("claimed"))
            .expect("claimed tabline mount");
        services.set_ui_access(UiAccess {
            rich: true,
            overlay: false,
            claims: vec!["workspaceline".to_string()],
        });
        services
            .ui_mount("tabline", &UiNode::text("canonical claim"))
            .expect("canonical workspaceline claim");
    }

    #[test]
    fn mount_update_round_trip_and_stale_handle() {
        let services = ui_services(rich_only());
        let handle = services
            .ui_mount("statusline", &UiNode::row(vec![UiNode::text("v1")]))
            .expect("mount");
        assert!(handle > 0);
        services.with_ui_blocks(|blocks| {
            let block = blocks.get(handle).expect("block retained");
            assert_eq!(block.slot(), "statusline");
            assert_eq!(block.version(), 1);
            assert_eq!(block.node().text_bytes(), 2);
        });
        assert!(
            services
                .ui_update(handle, &UiNode::text("v2"))
                .expect("update served")
        );
        services.with_ui_blocks(|blocks| {
            let block = blocks.get(handle).expect("block retained");
            assert_eq!(block.version(), 2);
            assert_eq!(block.node(), &UiNode::text("v2"));
        });
        assert!(
            !services
                .ui_update(handle + 999, &UiNode::text("stale"))
                .expect("stale lookup is not an error"),
            "stale handle must report false"
        );
        services.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 1));
    }

    #[test]
    fn generation_handles_are_foreign_across_instances() {
        let first = ui_services(rich_only());
        first.set_ui_epoch(7);
        let handle = first
            .ui_mount("statusline", &UiNode::text("gen1"))
            .expect("mount");
        let second = ui_services(rich_only());
        second.set_ui_epoch(8);
        let own = second
            .ui_mount("statusline", &UiNode::text("gen2"))
            .expect("mount");
        assert_ne!(
            handle, own,
            "handles minted by different generations must not alias"
        );
        assert!(
            !second
                .ui_update(handle, &UiNode::text("gen2"))
                .expect("foreign lookup is not an error"),
            "a handle from another generation must report false even when the
             next generation has its own live block"
        );
        assert!(
            second
                .ui_update(own, &UiNode::text("gen2b"))
                .expect("own handle serves"),
            "the next generation's own handle must still serve"
        );
        second.with_ui_blocks(|blocks| assert_eq!(blocks.len(), 1));
    }

    #[test]
    fn clear_invalidates_handles_without_aliasing_remounts() {
        let services = ui_services(rich_only());
        services.set_ui_epoch(3);
        let first = services
            .ui_mount("statusline", &UiNode::text("gen1"))
            .expect("mount");
        services.clear_ui_blocks();
        services.with_ui_blocks(|blocks| assert!(blocks.is_empty()));
        assert!(
            !services
                .ui_update(first, &UiNode::text("stale"))
                .expect("stale lookup is not an error"),
            "a cleared handle must report false"
        );
        let second = services
            .ui_mount("statusline", &UiNode::text("gen2"))
            .expect("remount");
        assert_ne!(
            first, second,
            "a remount after a clear must mint a fresh handle"
        );
        assert!(
            services
                .ui_update(second, &UiNode::text("gen2b"))
                .expect("fresh handle serves"),
            "the remounted handle must serve"
        );
    }

    #[test]
    fn block_registry_budget_fails_closed() {
        let services = ui_services(rich_only());
        for index in 0..UI_MAX_BLOCKS {
            services
                .ui_mount("statusline", &UiNode::text(format!("{index}")))
                .expect("under the block cap");
        }
        let error = services
            .ui_mount("statusline", &UiNode::text("overflow"))
            .expect_err("block cap must fail closed");
        assert_eq!(error.code, "E_UI_BLOCK_BUDGET");
        assert_eq!(error.class, "budget");
        services.with_ui_blocks(|blocks| assert_eq!(blocks.len(), UI_MAX_BLOCKS));
    }

    #[test]
    fn aggregated_text_budget_fails_closed() {
        let services = ui_services(rich_only());
        let chunk = "x".repeat(UI_MAX_TEXT_BYTES);
        for _ in 0..(UI_MAX_AGGREGATED_TEXT_BYTES / UI_MAX_TEXT_BYTES) {
            services
                .ui_mount("statusline", &UiNode::text(chunk.clone()))
                .expect("exactly at the aggregate cap");
        }
        let error = services
            .ui_mount("statusline", &UiNode::text("one byte over"))
            .expect_err("aggregate cap must fail closed");
        assert_eq!(error.code, "E_UI_BLOCK_BUDGET");
        services.with_ui_blocks(|blocks| {
            assert_eq!(
                blocks.len(),
                UI_MAX_AGGREGATED_TEXT_BYTES / UI_MAX_TEXT_BYTES
            )
        });
    }
}
