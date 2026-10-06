//! Host bridge for the `bitty` Lua module (RFC `plugin-host-runtime-rfc` Gap A).
//!
//! This module is the VM seam half of Gap A: it injects the read-only `bitty`
//! namespace, implements host-mediated, source-only rooted `require`, and
//! captures `init.lua` registration results for the runtime to validate. It
//! owns no policy (capability grants, manifest validation, lifecycle) and
//! performs no network, process, or ambient filesystem work beyond reading the
//! caller-supplied module root.
//!
//! ## Contract (working identifiers, not accepted spellings)
//!
//! - [`HostServices`] is the object-safe boundary the runtime implements. The
//!   bridge only ever calls it synchronously, inside the current VM slice,
//!   under the existing RC-1/RC-2 budgets; every call is deadline-checked and
//!   re-entrancy is rejected fail-closed.
//! - [`LuaValue`] is the bounded, immutable data copy passed across the
//!   boundary; live host objects and Lua functions never cross it. Function
//!   handles (command `run`, event handlers, timer callbacks) are captured
//!   inside the VM as stashed functions and re-invoked through
//!   [`crate::LuaVm::call_function`].
//! - `require` resolves only inside the caller's module root, canonicalizes,
//!   rejects traversal/escapes and non-`.lua` artifacts, and caches per VM.
//! - The `bitty` table and every sub-table are read-only proxies: assignment
//!   and raw metatable mutation fail with a typed `runtime` diagnostic.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub use phodopus::StashedFunction;
use phodopus::{
    Callback, CallbackReturn, Closure, Context, Error, ExecutorMode, Function, Table, Value,
};

use crate::ui::{UiNode, component_invalid, is_ui_slot, read_component};
use crate::{LuaVm, SuspendReason, VmError};

/// Version of the host bridge line exposed as `bitty.api_version`.
pub const API_VERSION: &str = "1.0.0";

/// Default marshalling depth ceiling (W-131 backend value; was 8 under
/// `RC-1`/bridge contract A.3, now 16 to match the store backend).
pub const DEFAULT_MAX_DEPTH: usize = 16;
/// Default marshalling node ceiling (W-131 backend value; was 1024 under
/// bridge contract A.3, now 256 to match the store backend).
pub const DEFAULT_MAX_NODES: usize = 256;
/// Default marshalling byte ceiling for storage-shaped values (`8 KiB`).
pub const DEFAULT_MAX_VALUE_BYTES: usize = 8 * 1024;
/// Snapshot byte ceiling (`SNAPSHOT_MAX_BYTES`, RFC C.2).
pub const SNAPSHOT_MAX_BYTES: usize = 256 * 1024;
/// Maximum workspaces one `bitty.workspace.list()` result carries (CTX-0889).
///
/// Mirrors the Core slot-table bound (`bitty_runtime::MAX_WORKSPACES`, 16);
/// a runtime test pins the two together. The bridge truncates defensively,
/// so a misbehaving host source can never push an unbounded array into Lua.
pub const WORKSPACE_LIST_MAX_ITEMS: usize = 16;

/// Maximum characters of one workspace name crossing into Lua (CTX-0889).
///
/// Mirrors the Core display bound (`bitty_runtime::WORKSPACE_NAME_MAX_CHARS`,
/// 32); names are truncated at a char boundary.
pub const WORKSPACE_NAME_MAX_CHARS: usize = 32;

/// Maximum bytes of a `bitty.workspace.rename` name argument (CTX-0889).
///
/// Mirrors the IPC `workspace rename` bound (`MAX_WORKSPACE_RENAME_BYTES`,
/// 256): longer input fails closed with `E_DEF_LIMIT` before reaching the
/// host; accepted names are truncated by Core to
/// [`WORKSPACE_NAME_MAX_CHARS`].
pub const WORKSPACE_RENAME_MAX_BYTES: usize = 256;

/// Maximum parked panels one `bitty.workspace.list()` row reports (CTX-0954).
///
/// Mirrors the Core single-slot invariant (`ScratchpadSlot` holds at most one
/// parked leaf): occupancy is `0` (empty) or `1` (occupied), never more.
pub const SCRATCHPAD_COUNT_MAX: usize = 1;

/// Attention flags for one workspace in `bitty.workspace.list()` (CTX-0889).
///
/// Core has no per-workspace attention source yet (bell, activity, and
/// exit state are tracked per session/runtime, not per workspace), so hosts
/// currently report every flag `false`. The shape is fixed now so plugins
/// do not break when a source lands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceAttention {
    /// A bell rang in this workspace since it was last focused.
    pub bell: bool,
    /// Output arrived in this workspace while it was inactive.
    pub activity: bool,
    /// A panel process in this workspace exited.
    pub exited: bool,
}

/// One workspace row for `bitty.workspace.list()` (CTX-0889, ADR-0014).
///
/// Identity, order, and structure only: never terminal content.
///
/// CTX-0954: scratchpad occupancy rides the same row under the same
/// `workspace.read` grant (no panel capability is consulted): the slot is
/// window-global (one [`bitty_ui::ScratchpadSlot`] per window), so every row
/// of one `list()` result carries the same occupancy snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInfo {
    /// Stable workspace id (Core creation sequence; survives index shifts).
    pub id: u64,
    /// Display name.
    pub name: String,
    /// Whether this workspace is active.
    pub active: bool,
    /// Number of panels (layout leaves).
    pub panel_count: usize,
    /// Parked panels in the window scratchpad slot (`0` or `1`; window-global,
    /// identical on every row of one result).
    pub scratchpad_count: usize,
    /// Whether the window scratchpad slot holds a parked panel.
    pub scratchpad_occupied: bool,
    /// Attention flags (all `false` until Core grows a source).
    pub attention: WorkspaceAttention,
}

/// One validated workspace mutation request from Lua (CTX-0889).
///
/// The bridge validates argument shapes; the host gates on
/// `workspace.control`, enqueues into a bounded queue, and the application
/// applies it on its next tick through the same Core handlers the
/// keybindings use. Ids are resolved at apply time; an id that no longer
/// exists is dropped fail-closed. Spellings of the Lua entry points are
/// candidates pending OQ-056.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceRequest {
    /// Focus the workspace with this stable id.
    FocusId(u64),
    /// Focus the workspace at this 1-based position (clamped to the last
    /// workspace, like the `Alt+N` keybinding).
    FocusIndex(u64),
    /// Create a workspace and switch to it (capacity-limited).
    New,
    /// Switch to the next workspace (wraps).
    Next,
    /// Close a workspace (`None` = the active one) through the kill-confirm
    /// gate: idle closes immediately, live arms the user confirm.
    Close(Option<u64>),
    /// Rename the workspace with this stable id.
    Rename {
        /// Stable workspace id.
        id: u64,
        /// Validated, non-blank name (bounded by [`WORKSPACE_RENAME_MAX_BYTES`]).
        name: String,
    },
    /// Move the focused panel of the active workspace into the workspace
    /// with this stable id.
    MovePanel(u64),
}

/// Default host-call deadline in milliseconds (reuses `RC-1`).
pub const DEFAULT_HOST_DEADLINE_MS: u64 = crate::RC1_WALL_CLOCK_BUDGET_MS;

/// Default `process.spawn` bridge deadline in milliseconds (`5 s`).
///
/// Cites the spawn timeout contract documented on
/// [`HostServices::process_spawn`] (default 5 s, maximum 30 s, enforced by
/// killing and reaping the child, CTX-0445): the bridge timeout path for
/// spawn uses this deadline, not the 50 ms cheap-call deadline, so
/// slow-but-successful spawns within contract are delivered while spawns past
/// contract fail-closed with typed `E_TIMEOUT` and no result delivered.
pub const SPAWN_TIMEOUT_MS: u64 = 5_000;

/// Maximum `process.spawn` bridge deadline in milliseconds (`30 s`).
///
/// Upper bound for [`LuaVm::set_spawn_deadline_ms`]; mirrors the spawn
/// contract maximum (CTX-0445). Larger values are refused fail-closed with
/// [`VmError::Budget`](crate::VmError::Budget).
pub const SPAWN_TIMEOUT_MAX_MS: u64 = 30_000;

/// Maximum `process.spawn` argv entries accepted from Lua (bounded-list
/// precedent; the host allowlist enforces a tighter per-tool count).
pub const SPAWN_LUA_MAX_ARGS: usize = 64;

/// Maximum bytes of one `process.spawn` argv entry accepted from Lua (params
/// bound precedent; the host allowlist enforces a tighter per-tool count).
pub const SPAWN_LUA_MAX_ARG_BYTES: usize = 4096;
/// Maximum bytes of one module name accepted by `require`.
pub const MODULE_NAME_MAX_BYTES: usize = 128;
/// Maximum bytes of one `bitty.env` key (CTX-0330).
///
/// Mirrors the credential/secret env-name ceiling (`128`): an over-bound key
/// is rejected fail-closed with `E_DEF_LIMIT` before any grant check, so
/// oversize input never reaches the allowlist.
pub const ENV_KEY_MAX_BYTES: usize = 128;
/// Maximum bytes of one `bitty.fs` path (RFC-0005, CTX-0984).
///
/// Reuses the accepted Core bound (`bitty-plugin-host` `MAX_FS_PATH_BYTES`,
/// `4096`): over-bound paths fail closed with `E_DEF_LIMIT` before any grant
/// check, so oversize input never reaches the scope matcher.
pub const FS_PATH_MAX_BYTES: usize = 4096;
/// Maximum bytes of one `bitty.fs.write` payload crossing the Lua bridge
/// (RFC-0005, CTX-0984).
///
/// Bridge-side defensive cap (precedent: `DEFAULT_MAX_VALUE_BYTES`, `8 KiB`
/// store values; the file-manager draft's `8 KiB` listing payload). Exact
/// per-call payload caps stay parked to the Core bridge (`FsCaps`) and SDK
/// work; the host gate enforces the authoritative ceiling.
pub const FS_CONTENT_MAX_BYTES: usize = 8 * 1024;
/// Maximum entries one `bitty.fs.list` call returns (RFC-0005, CTX-0984).
///
/// Bridge-side defensive cap (precedent: `UI_TARGETS_SNAPSHOT_MAX`, `1024`
/// cold-path entries). Exact listing caps stay parked to the Core bridge
/// (`FsCaps`); the host gate enforces the authoritative ceiling.
pub const FS_LIST_MAX_ENTRIES: usize = 1024;
/// Maximum entry-name bytes one `bitty.fs.list` call returns (RFC-0005).
///
/// Precedent: `DEFAULT_MAX_VALUE_BYTES` (`8 KiB`); exact caps stay parked to
/// the Core bridge (`FsCaps`).
pub const FS_LIST_MAX_BYTES: usize = 8 * 1024;
/// Maximum bytes of one source module file accepted by `require`.
pub const MODULE_FILE_MAX_BYTES: usize = 1024 * 1024;

/// Maximum commands captured from one `init.lua` (HOST-002 admission bound).
///
/// Matches the policy-layer manifest ceiling
/// (`bitty-plugin-host` `MAX_COMMANDS = 128`): a capture can never need more
/// than the manifest can declare, so the bridge refuses the 129th
/// registration fail-closed with typed `E_DEF_LIMIT` before any unbounded
/// growth.
pub const REGISTRATION_MAX_COMMANDS: usize = 128;

/// Maximum event subscriptions captured from one `init.lua` (HOST-002).
///
/// Matches the policy-layer manifest ceiling
/// (`bitty-plugin-host` `MAX_EVENT_TYPES = 256`).
pub const REGISTRATION_MAX_EVENTS: usize = 256;

/// Maximum timers captured from one `init.lua` (HOST-002 admission bound).
///
/// Timers have no manifest declaration to mirror, so this is the tighter
/// queue-side precedent (`bitty-plugin-host` `PER_SUBSCRIPTION_QUEUE_LIMIT =
/// 64`): a generation that needs more concurrent timers than one queue can
/// drain is hostile or broken, and the bridge refuses the 65th fail-closed
/// with typed `E_DEF_LIMIT`.
pub const REGISTRATION_MAX_TIMERS: usize = 64;

/// Maximum bytes of one captured command id (`HOST-002` admission bound).
///
/// Matches the policy-layer resource-segment ceiling (manifest qualified-name
/// resource part, 128 bytes max).
pub const REGISTRATION_MAX_ID_BYTES: usize = 128;

/// Maximum bytes of one captured command title (`HOST-002`).
///
/// Matches the policy-layer display-name ceiling
/// (`bitty-plugin-host` `MAX_NAME_LEN = 128`); titles are host-rendered
/// display data, never markup.
pub const REGISTRATION_MAX_TITLE_BYTES: usize = 128;

/// Maximum bytes of one captured command description (`HOST-002`).
///
/// Matches the policy-layer description ceiling
/// (`bitty-plugin-host` `MAX_DESCRIPTION_LEN = 1024`).
pub const REGISTRATION_MAX_DESCRIPTION_BYTES: usize = 1024;

/// Maximum bytes of one captured event kind (`HOST-002` admission bound).
///
/// Matches the policy-layer lazy-event ceiling (manifest `lazy.events`
/// entries are `1..128` bytes).
pub const REGISTRATION_MAX_EVENT_KIND_BYTES: usize = 128;

/// Maximum timer delay in milliseconds (`HOST-002` admission bound).
///
/// One day: generous for any legitimate deferred callback, while an
/// unbounded `u64` delay (millennia) is almost certainly a hostile or broken
/// computation. Larger delays are refused fail-closed with typed
/// `E_DEF_INVALID`.
pub const REGISTRATION_MAX_TIMER_DELAY_MS: u64 = 86_400_000;

/// Maximum keymap suggestions captured from one `init.lua` (CTX-0707).
///
/// Activation-scoped registrations like commands (no manifest ceiling names
/// suggestions), so this mirrors `REGISTRATION_MAX_COMMANDS`: a generation
/// that suggests more bindings than the manifest can declare commands for is
/// hostile or broken. The 129th suggestion fails closed with typed
/// `E_DEF_LIMIT`. Precedence and conflict diagnostics stay host-side (the
/// runtime applies suggestions after activation); the bridge only captures.
pub const REGISTRATION_MAX_KEYMAP_SUGGESTIONS: usize = 128;

/// Maximum bytes of one suggested chord (`CTX-0707` admission bound).
///
/// Matches the policy-layer resource-segment ceiling (manifest qualified-name
/// resource part, 128 bytes max); the shipped chord grammar itself is
/// validated host-side at application time (LUA-OQ-5), the bridge checks
/// shape only.
pub const REGISTRATION_MAX_KEYMAP_CHORD_BYTES: usize = 128;

/// Maximum bytes of one suggested command name (`CTX-0707` admission bound).
///
/// Matches `REGISTRATION_MAX_ID_BYTES`: a suggestion names a command
/// registered by the same generation, so it can never legitimately exceed
/// the command-id ceiling.
pub const REGISTRATION_MAX_KEYMAP_COMMAND_BYTES: usize = 128;

/// Maximum tasks captured from one `init.lua` (CTX-0707, RC-4).
///
/// The accepted RC-4 cap is 64 live tasks per plugin (ADR 0007, LUA-OQ-9):
/// the 65th spawn fails closed with typed `E_BUDGET_TASK` (`budget` class),
/// never queues silently. Scheduling and resumption stay host-side (tasks
/// resume through the event path); the bridge only captures the entry
/// function and owns the handle, mirroring `timers.create`/`cancel`.
pub const REGISTRATION_MAX_TASKS: usize = 64;

/// Maximum provided interfaces captured from one `init.lua` (LUA-OQ-8).
///
/// Mirrors `REGISTRATION_MAX_COMMANDS` in spirit at a smaller scale: a
/// generation that provides more interfaces than it could plausibly
/// implement is hostile or broken. The 17th `provide` fails closed with
/// typed `E_DEF_LIMIT`. The bridge only captures the impl functions;
/// publication into the runtime service directory happens at activation.
pub const REGISTRATION_MAX_SERVICES: usize = 16;

/// Maximum methods captured per provided interface (LUA-OQ-8).
///
/// Bounds the impl-table scan: a 33rd function entry fails closed with
/// typed `E_DEF_LIMIT` instead of stashing an unbounded handle set.
pub const REGISTRATION_MAX_SERVICE_METHODS: usize = 32;

/// Maximum bytes of one service interface name (LUA-OQ-8 admission bound).
///
/// Matches the manifest `services.provided` ceiling (1..128 bytes); the
/// dot-separated grammar itself is enforced host-side at resolution
/// (the bridge checks shape only).
pub const SERVICE_MAX_IFACE_BYTES: usize = 128;

/// Maximum bytes of one service method name (LUA-OQ-8 admission bound).
///
/// Method names are plain Lua table keys naming the impl functions; the
/// ceiling keeps handle-table scans bounded.
pub const SERVICE_MAX_METHOD_BYTES: usize = 128;

/// One bounded, immutable data value crossing the host bridge.
///
/// This is deliberately smaller than the Lua value space: functions, threads,
/// and userdata never cross. Tables keep insertion-ordered key/value pairs so
/// the marshalling stays deterministic.
#[derive(Debug, Clone, PartialEq)]
pub enum LuaValue {
    /// Lua `nil`.
    Nil,
    /// Lua boolean.
    Bool(bool),
    /// Lua integer.
    Integer(i64),
    /// Lua number.
    Number(f64),
    /// Lua string (valid UTF-8, lossy at the boundary).
    String(String),
    /// Lua table as ordered pairs (array part uses consecutive integer keys).
    Table(Vec<(LuaValue, LuaValue)>),
}

impl LuaValue {
    /// Build a table from string-keyed pairs (deterministic order preserved).
    #[must_use]
    pub fn table<const N: usize>(pairs: [(&str, LuaValue); N]) -> Self {
        Self::Table(
            pairs
                .into_iter()
                .map(|(k, v)| (LuaValue::String(k.to_string()), v))
                .collect(),
        )
    }

    /// Build an array table (1-based integer keys).
    #[must_use]
    pub fn array(values: Vec<LuaValue>) -> Self {
        Self::Table(
            values
                .into_iter()
                .enumerate()
                .map(|(i, v)| (LuaValue::Integer(i as i64 + 1), v))
                .collect(),
        )
    }

    /// Look up a string key in a table value.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&LuaValue> {
        match self {
            Self::Table(pairs) => pairs.iter().find_map(|(k, v)| match k {
                Self::String(s) if s == key => Some(v),
                _ => None,
            }),
            _ => None,
        }
    }

    /// Convert this bounded value into a fresh Lua value inside `ctx`.
    #[must_use]
    pub fn to_lua<'gc>(&self, ctx: Context<'gc>) -> Value<'gc> {
        match self {
            Self::Nil => Value::Nil,
            Self::Bool(b) => Value::Boolean(*b),
            Self::Integer(i) => Value::Integer(*i),
            Self::Number(n) => Value::Number(*n),
            Self::String(s) => Value::String(ctx.intern(s.as_bytes())),
            Self::Table(pairs) => {
                let table = Table::new(&ctx);
                for (key, value) in pairs {
                    let _ = table.set_raw(ctx, key.to_lua(ctx), value.to_lua(ctx));
                }
                Value::Table(table)
            }
        }
    }

    /// Read a bounded Lua value into a [`LuaValue`], enforcing `limits`.
    ///
    /// # Errors
    ///
    /// Fails closed with `E_VALUE_*` when the input exceeds a depth, node, or
    /// byte ceiling, or contains a non-data type (function, thread, userdata).
    pub fn from_lua<'gc>(
        value: Value<'gc>,
        limits: MarshallingLimits,
    ) -> Result<Self, BridgeError> {
        let mut budget = MarshalBudget::new(limits);
        Self::from_lua_inner(value, limits, &mut budget, 0)
    }

    fn from_lua_inner<'gc>(
        value: Value<'gc>,
        limits: MarshallingLimits,
        budget: &mut MarshalBudget,
        depth: usize,
    ) -> Result<Self, BridgeError> {
        if depth > limits.max_depth {
            return Err(BridgeError::value(
                "E_VALUE_DEPTH",
                "value nesting exceeds depth limit",
            ));
        }
        budget.nodes += 1;
        if budget.nodes > limits.max_nodes {
            return Err(BridgeError::value(
                "E_VALUE_NODES",
                "value node count exceeds limit",
            ));
        }
        match value {
            Value::Nil => Ok(Self::Nil),
            Value::Boolean(b) => Ok(Self::Bool(b)),
            Value::Integer(i) => Ok(Self::Integer(i)),
            Value::Number(n) => Ok(Self::Number(n)),
            Value::String(s) => {
                let text = String::from_utf8_lossy(s.as_bytes()).into_owned();
                budget.bytes = budget.bytes.saturating_add(text.len());
                if budget.bytes > limits.max_bytes {
                    return Err(BridgeError::value(
                        "E_VALUE_BYTES",
                        "value serialized size exceeds limit",
                    ));
                }
                Ok(Self::String(text))
            }
            Value::Table(table) => {
                let mut pairs = Vec::new();
                for (key, child) in table.iter() {
                    let key = Self::from_lua_inner(key, limits, budget, depth + 1)?;
                    let child = Self::from_lua_inner(child, limits, budget, depth + 1)?;
                    pairs.push((key, child));
                }
                Ok(Self::Table(pairs))
            }
            other => Err(BridgeError::new(
                "validation",
                "E_VALUE_TYPE",
                format!(
                    "value type '{}' cannot cross the host bridge",
                    other.type_name()
                ),
            )),
        }
    }
}

/// Bounded marshalling limits for the host bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarshallingLimits {
    /// Maximum table nesting depth.
    pub max_depth: usize,
    /// Maximum total nodes (scalars plus table entries).
    pub max_nodes: usize,
    /// Maximum serialized bytes (sum of string bytes plus per-node overhead).
    pub max_bytes: usize,
}

impl Default for MarshallingLimits {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_nodes: DEFAULT_MAX_NODES,
            max_bytes: DEFAULT_MAX_VALUE_BYTES,
        }
    }
}

struct MarshalBudget {
    nodes: usize,
    bytes: usize,
}

impl MarshalBudget {
    fn new(_limits: MarshallingLimits) -> Self {
        Self { nodes: 0, bytes: 0 }
    }
}

/// Stable `runtime`-class code for a UI surface the host does not present
/// (Plugin API v1, OQ-056): the default host's `ui.mount`/`ui.update`, and an
/// accepted v1 slot that a concrete host does not render (CTX-0923).
pub const E_UI_UNAVAILABLE: &str = "E_UI_UNAVAILABLE";

/// Stable `runtime`-class code for a focusable-overlay capture request while
/// another capture is already active (CTX-0941, OQ-056 v2 scope). Core owns a
/// single capture switch, so only one overlay may hold transient input focus
/// at a time; a second `acquire` fails closed instead of silently stealing or
/// queueing behind the owner.
pub const E_UI_ALREADY_CAPTURED: &str = "E_UI_ALREADY_CAPTURED";

/// Stable `runtime`-class code for an overlay capture call by a generation
/// that does not own the active capture (or names a handle that is not one of
/// its own mounted overlay blocks) (CTX-0941, OQ-056 v2 scope). Release is the
/// exception: a foreign release is an idempotent no-op, never an error.
pub const E_UI_NOT_OWNER: &str = "E_UI_NOT_OWNER";

/// Maximum captured input events a single overlay capture session retains.
///
/// The Core-side capture queue is bounded fail-safe: overflow drops the oldest
/// event and counts it, so a slow or crashed plugin can never grow the queue
/// without bound or stall the input path.
pub const OVERLAY_CAPTURE_QUEUE_MAX: usize = 256;

/// Maximum UTF-8 bytes of one captured text/IME/pointer event's text.
pub const OVERLAY_CAPTURE_TEXT_MAX_BYTES: usize = 4096;

/// Maximum number of captured events one `bitty.ui.overlay.poll` call returns.
pub const OVERLAY_CAPTURE_POLL_MAX: usize = OVERLAY_CAPTURE_QUEUE_MAX;

/// Maximum bytes of the `title` hint in `bitty.ui.overlay.acquire(spec)`.
///
/// Presentation hint only; over-bound specs fail closed with `E_VALUE_BYTES`
/// and no session is created.
pub const OVERLAY_SPEC_TITLE_MAX_BYTES: usize = 1024;

/// Maximum bytes of the `placeholder` hint in `bitty.ui.overlay.acquire(spec)`.
pub const OVERLAY_SPEC_PLACEHOLDER_MAX_BYTES: usize = 1024;

/// Maximum serialized bytes of one acquire/update call envelope (spec plus
/// reason hints). Over-bound calls fail closed with the existing
/// value-shape errors and previous state is kept.
pub const OVERLAY_CALL_MAX_BYTES: usize = 4096;

/// Maximum entries one `bitty.ui.targets.snapshot` read returns (W-29, CTX-0942).
///
/// Mirrors the Core mechanism bound (`bitty-ui` `MAX_SNAPSHOT_TARGETS`, 1024):
/// the bridge truncates defensively, so a misbehaving host source can never
/// push an unbounded array into Lua. Private: the accepted ceiling lives in
/// `bitty-ui`; this is only the bridge-side defensive cap.
const UI_TARGETS_SNAPSHOT_MAX: usize = 1024;

/// Maximum target offers one `bitty.ui.targets.register` call carries (W-29).
///
/// Mirrors the Core snapshot bound above: a registration that would overflow
/// a single cold-path collection fails closed before any registry insert.
const UI_TARGETS_REGISTER_MAX: usize = 1024;

/// Maximum bytes of a `bitty.ui.targets.register` provider name (W-29).
///
/// Mirrors the Core provider-name grammar (`bitty-ui`
/// `MAX_TARGET_PROVIDER_NAME_LEN`, 32).
const UI_TARGETS_PROVIDER_NAME_MAX_BYTES: usize = 32;

/// Maximum anchors one `bitty.ui.labels.assign` call carries (W-29).
///
/// Mirrors the Core allocator cap (`bitty-ui` `MAX_HINT_TARGETS`, 1024).
const UI_LABELS_ASSIGN_MAX: usize = 1024;

/// One bounded transient input event captured for a focusable overlay
/// (CTX-0941).
///
/// The plugin observes captured input only through the `bitty.ui.overlay.poll`
/// host call: Core enqueues events on the input path and never invokes a plugin
/// in that path (`P0-AC-015`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayInput {
    /// Monotonic per-capture sequence number (diagnostics for dropped gaps).
    pub sequence: u64,
    /// Event class: `key`, `text`, `ime`, or `pointer`.
    pub kind: String,
    /// Bounded UTF-8 payload.
    pub text: String,
}

/// Detailed overlay poll result per the accepted W-01 contract (CTX-0941).
///
/// `active` selects the `status` field (`"active"` while the session holds
/// capture, `"released"` after any terminal cause). `seq` is the monotonic
/// sequence of the last event delivered in this session. `events` holds
/// drained input in order. `overflowed` is sticky once queue overflow has
/// dropped an older event. `reason` is present only with `"released"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayPoll {
    /// Whether the session still holds capture.
    pub active: bool,
    /// Last delivered sequence in this session.
    pub seq: u64,
    /// Drained events (empty for a released session).
    pub events: Vec<OverlayInput>,
    /// Sticky overflow flag for this session.
    pub overflowed: bool,
    /// Terminal reason, present only when not active.
    pub reason: Option<String>,
}

/// Typed, bounded bridge/diagnostic error.
///
/// `class` is one of the accepted diagnostic classes (`runtime`, `validation`,
/// `resolution`, `budget`); `code` is a stable `E_*` identifier. The message is
/// host-authored and bounded; it never echoes untrusted content beyond the
/// offending token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeError {
    /// Diagnostic class.
    pub class: &'static str,
    /// Stable `E_*` code.
    pub code: &'static str,
    /// Bounded host-authored message.
    pub message: String,
}

impl BridgeError {
    /// Construct a bridge error.
    #[must_use]
    pub fn new(class: &'static str, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            class,
            code,
            message: message.into(),
        }
    }

    /// Construct a `validation`-class value error.
    #[must_use]
    pub fn value(code: &'static str, message: &str) -> Self {
        Self::new("validation", code, message)
    }

    /// Construct the typed `E_TIMEOUT` budget error.
    #[must_use]
    pub fn timeout() -> Self {
        Self::new("budget", "E_TIMEOUT", "host call exceeded its deadline")
    }

    /// Construct the typed `E_CAPABILITY_DENIED` runtime error.
    #[must_use]
    pub fn capability_denied(capability: &str) -> Self {
        Self::new(
            "runtime",
            "E_CAPABILITY_DENIED",
            format!("capability '{capability}' is not granted"),
        )
    }

    /// Construct the typed `E_NOT_IMPLEMENTED` runtime error for an accepted
    /// v1 namespace the host has not wired yet (CTX-0707 parity gap).
    ///
    /// `item` is the static `bitty.<namespace>.<fn>` spelling (never
    /// untrusted content): the namespace stays present and callable so
    /// misconfiguration is observable, but every call fails closed until a
    /// follow-up wires the host backend.
    #[must_use]
    pub fn not_implemented(item: &str) -> Self {
        Self::new(
            "runtime",
            "E_NOT_IMPLEMENTED",
            format!("{item} is not implemented by this host"),
        )
    }

    /// Render this error as a catchable Lua error table (`class`/`code`/`message`).
    #[must_use]
    pub fn to_error<'gc>(&self, ctx: Context<'gc>) -> Error<'gc> {
        let table = Table::new(&ctx);
        let _ = table.set(ctx, "class", self.class);
        let _ = table.set(ctx, "code", self.code);
        let _ = table.set(ctx, "message", self.message.clone());
        Value::Table(table).into()
    }
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} ({})", self.class, self.message, self.code)
    }
}

impl std::error::Error for BridgeError {}

/// Host service boundary implemented by `bitty-runtime`.
///
/// Every method is synchronous and non-blocking, except [`HostServices::process_spawn`].
/// Implementors are expected to
/// be cheap and bounded; the bridge deadline-checks each call fail-closed
/// with `E_TIMEOUT` (check-then-act inside the budget: the bridge checks its
/// expiry before invoking and passes the expiry to mutating/spawn calls;
/// read-only calls are also checked after delivery, while mutating calls are
/// not re-checked after success because a committed effect must never be
/// reported as a timeout; mutating/spawn implementations must check the expiry
/// before committing or delivering, so post-deadline effects never commit),
/// and rejects re-entrant calls. Durable commit I/O that a disk-backed store
/// reports through [`crate::record_store_commit_io`] during a successful
/// `store_set_with_expiry` is credited out of the RC-1 hard wall limit of
/// every enclosing callback, capped at [`crate::STORE_COMMIT_CREDIT_MAX_MS`]
/// per callback (bitty #1518).
/// `process.spawn` flows through the same
/// bridge timeout path with its own spawn deadline
/// ([`SPAWN_TIMEOUT_MS`]/[`SPAWN_TIMEOUT_MAX_MS`], default 5 s, maximum 30 s,
/// CTX-0464) instead of the 50 ms cheap-call deadline. Capability gating is the
/// caller's responsibility (it decides what `services` are reachable), but
/// implementations should still fail closed.
pub trait HostServices {
    /// Read a plugin-scoped store entry; `Ok(None)` means absent.
    fn store_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError>;
    /// Atomically write a plugin-scoped store entry.
    fn store_set(&self, key: &str, value: LuaValue) -> Result<(), BridgeError>;
    /// Expiry-aware `store_set` for the pre-commit timeout path (CTX-0464).
    ///
    /// The bridge passes its call expiry (`start + deadline`); implementations
    /// must check `Instant::now() > expiry` before committing and fail-closed
    /// with [`BridgeError::timeout`] without mutating when expired, so
    /// post-deadline effects never commit. The default checks expiry before
    /// delegating to [`HostServices::store_set`] (fail-fast when already
    /// expired; slow-during-call commits remain the delegate's responsibility
    /// — real services are in-memory fast, tests override to prove no-commit).
    fn store_set_with_expiry(
        &self,
        key: &str,
        value: LuaValue,
        expiry: Instant,
    ) -> Result<(), BridgeError> {
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.store_set(key, value)
    }
    /// Read a typed setting; `Ok(None)` means absent.
    fn settings_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError>;
    /// Read one host environment variable for `bitty.env.get` (CTX-0330).
    ///
    /// Host-mediated and grant-gated: the implementation must return
    /// [`BridgeError::not_implemented`](BridgeError::not_implemented) with
    /// `bitty.env.get` until the calling generation holds an
    /// `env.read:<KEY>` grant for `key` (fail-closed, desensitized — the
    /// same code as a host without an env backend, so ungranted keys are
    /// indistinguishable from unimplemented ones). `Ok(None)` means the
    /// granted key is absent from the host environment
    /// (absent-unless-declared). Values cross as bounded strings; keys are
    /// validated by the caller shape (`[A-Za-z_][A-Za-z0-9_]*`, `1..128`
    /// bytes) with `E_DEF_INVALID`/`E_DEF_LIMIT`.
    ///
    /// The default implementation fails closed with `E_NOT_IMPLEMENTED`: a
    /// host without an env backend can never gain ambient reads from the
    /// always-present `bitty.env` namespace.
    fn env_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        let _ = key;
        Err(BridgeError::not_implemented("bitty.env.get"))
    }
    /// Whether one host environment variable is present, for `bitty.env.has`
    /// (CTX-0330).
    ///
    /// Same grant gate as [`HostServices::env_get`]: `E_NOT_IMPLEMENTED`
    /// until the calling generation holds `env.read:<KEY>`, otherwise
    /// presence of the granted key. The default implementation fails closed
    /// like [`HostServices::env_get`].
    fn env_has(&self, key: &str) -> Result<bool, BridgeError> {
        let _ = key;
        Err(BridgeError::not_implemented("bitty.env.has"))
    }
    /// Read bounded file bytes for `bitty.fs.read` (RFC-0005, CTX-0984).
    ///
    /// Host-mediated and grant-gated: the implementation must fail closed
    /// until the calling generation holds an `fs.read:PATTERN` grant covering
    /// `path` (deny-by-default, no wildcard; `list` rides the same read
    /// grant). Results are read-into-VM-only and carry the Core-attached
    /// untrusted-observation label (`untrusted = true`); combining them with
    /// clipboard, process, IPC, or network authority needs a separately
    /// granted authority with argv-first invocation and no shell-string
    /// construction. Paths are validated by the caller shape
    /// (`1..=4096` bytes, no NUL/controls) with `E_DEF_INVALID`/`E_DEF_LIMIT`;
    /// scope, sensitive-path, secret, bound, budget, safe-mode, and trust
    /// denials are typed `E_FS_*` (oracle-tight: level plus family only).
    ///
    /// The default implementation fails closed with `E_NOT_IMPLEMENTED`: a
    /// host without an fs backend can never gain ambient reads from the
    /// always-present `bitty.fs` namespace.
    fn fs_read(&self, path: &str) -> Result<LuaValue, BridgeError> {
        let _ = path;
        Err(BridgeError::not_implemented("bitty.fs.read"))
    }
    /// Write bounded file bytes for `bitty.fs.write` (RFC-0005, CTX-0984).
    ///
    /// Grant-gated on `fs.write:PATTERN` only (a read grant never implies
    /// write). The disposition (create, overwrite, append-mode) is the
    /// `append` flag candidate, not a verb: `false` creates or overwrites,
    /// `true` appends. There is no `open` verb and no retained handle.
    /// Mutating, so the bridge routes through the pre-commit expiry guard;
    /// see [`HostServices::fs_write_with_expiry`]. The default fails closed
    /// like [`HostServices::fs_read`].
    fn fs_write(&self, path: &str, content: &str, append: bool) -> Result<LuaValue, BridgeError> {
        let _ = (path, content, append);
        Err(BridgeError::not_implemented("bitty.fs.write"))
    }
    /// Expiry-aware [`HostServices::fs_write`] for the pre-commit timeout path.
    ///
    /// Same contract as [`HostServices::store_set_with_expiry`]: check expiry
    /// before committing and fail-closed with [`BridgeError::timeout`]
    /// without mutating when expired. The default checks expiry before
    /// delegating (fail-fast).
    fn fs_write_with_expiry(
        &self,
        path: &str,
        content: &str,
        append: bool,
        expiry: Instant,
    ) -> Result<LuaValue, BridgeError> {
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.fs_write(path, content, append)
    }
    /// List bounded directory entries for `bitty.fs.list` (RFC-0005, CTX-0984).
    ///
    /// Read-class under the read grant over the listed prefix (never a
    /// grant-free enumeration oracle). Returns names plus file-kind metadata,
    /// never file bytes. Denied entries are suppressed silently (silent skip)
    /// as the default; the page leaks no absent-versus-denied signal.
    /// `max_entries` bounds the returned page. The default fails closed like
    /// [`HostServices::fs_read`]. There is no watch, subscription,
    /// tail-follow, retained handle, or cross-call cursor in this family.
    fn fs_list(&self, prefix: &str, max_entries: usize) -> Result<LuaValue, BridgeError> {
        let _ = (prefix, max_entries);
        Err(BridgeError::not_implemented("bitty.fs.list"))
    }
    /// Read a bounded committed terminal snapshot for `scope`.
    fn terminal_snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError>;
    /// Hand a notification to the platform asynchronously; returns acceptance.
    fn notify_show(&self, payload: &LuaValue) -> Result<bool, BridgeError>;
    /// Expiry-aware `notify_show` for the pre-commit timeout path (CTX-0464).
    ///
    /// Same contract as [`HostServices::store_set_with_expiry`]: check expiry
    /// before enqueueing, fail-closed with timeout without side effects.
    fn notify_show_with_expiry(
        &self,
        payload: &LuaValue,
        expiry: Instant,
    ) -> Result<bool, BridgeError> {
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.notify_show(payload)
    }
    /// Spawn an allowlisted system CLI with `args` as its argv (no shell).
    ///
    /// The default implementation fails closed with `E_SPAWN_UNAVAILABLE`;
    /// the runtime overrides it with the consent-gated, bounded spawn
    /// surface (CTX-0445). The tool identity is resolved host-side from the
    /// caller's grant, so Lua supplies only the argv array. The returned
    /// table carries at least `output` (bounded string), `truncated` (bool),
    /// `exit_code` (integer), and `untrusted` (always true): child bytes are
    /// untrusted observation data, never instructions.
    ///
    /// CTX-0464: unlike every other host method this call is long-running and
    /// governed by the spawn timeout contract (default 5 s, maximum 30 s,
    /// enforced by killing and reaping the child), so the bridge enforces the
    /// spawn deadline ([`SPAWN_TIMEOUT_MS`], configurable via
    /// [`LuaVm::set_spawn_deadline_ms`](crate::LuaVm::set_spawn_deadline_ms))
    /// through the same check-then-act timeout path instead of skipping the
    /// deadline entirely: a slow-but-successful spawn within contract is
    /// delivered, a spawn past contract fails-closed with `E_TIMEOUT` and no
    /// result delivered. The old skip-timeout behavior orphaned no registry
    /// slot here (the 64-slot registry lives host-side in the spawn service),
    /// but hid unbounded waits outside any bridge budget.
    fn process_spawn(&self, _args: &[String]) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            "E_SPAWN_UNAVAILABLE",
            "host has no process.spawn surface",
        ))
    }
    /// Expiry-aware `process_spawn` for the spawn bridge timeout path
    /// (CTX-0464).
    ///
    /// The bridge passes its spawn expiry (`start + spawn_deadline`);
    /// implementations must check expiry after waiting and before delivering,
    /// returning [`BridgeError::timeout`] without a result when expired. The
    /// host remains responsible for killing and reaping the child on timeout.
    /// The default checks expiry before delegating (fail-fast); slow backends
    /// override to check after waiting (tests prove timeout honored).
    fn process_spawn_with_expiry(
        &self,
        args: &[String],
        expiry: Instant,
    ) -> Result<LuaValue, BridgeError> {
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.process_spawn(args)
    }
    /// Current monotonic host time in milliseconds (for timer scheduling).
    fn now_millis(&self) -> u64 {
        0
    }
    /// Mount one validated v1 declarative component into an accepted slot.
    ///
    /// The bridge validates the `slot`/`component` pair against the accepted
    /// v1 contract before this call; the implementation owns capability
    /// gating (`ui.rich`; `ui.overlay` for the `overlay` slot; an exclusive
    /// claim for `tabline`) and the generation-owned block registry. A host
    /// that does not present an accepted slot must fail closed with
    /// [`E_UI_UNAVAILABLE`] (naming the slot and reason) rather than store a
    /// block it never renders (CTX-0923). Returns
    /// the opaque, generation-owned `block_id` handle.
    ///
    /// The default implementation fails closed: a host without a mount path
    /// can never gain ambient authority from the always-present `bitty.ui`
    /// namespace (`LUA-OQ-2`).
    fn ui_mount(&self, _slot: &str, _component: &UiNode) -> Result<i64, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui.mount surface",
        ))
    }
    /// Expiry-aware `ui_mount` for the pre-commit timeout path (CTX-0464).
    ///
    /// Mounting commits a generation-owned block, so the bridge uses the
    /// check-then-act mutating path: implementations must check
    /// `Instant::now() > expiry` before committing and fail-closed with
    /// [`BridgeError::timeout`] without mutating when expired. The default
    /// checks expiry before delegating (fail-fast; in-memory registries
    /// commit instantly).
    fn ui_mount_with_expiry(
        &self,
        slot: &str,
        component: &UiNode,
        expiry: Instant,
    ) -> Result<i64, BridgeError> {
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.ui_mount(slot, component)
    }
    /// Replace a mounted block's scene subtree, incrementing its version.
    ///
    /// Returns `Ok(false)` for a stale or foreign handle; the accepted
    /// `E_UI_COMPONENT_INVALID` component check runs in the bridge before
    /// this call. The default implementation fails closed like
    /// [`HostServices::ui_mount`].
    fn ui_update(&self, _handle: i64, _component: &UiNode) -> Result<bool, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui.update surface",
        ))
    }
    /// Expiry-aware `ui_update` for the pre-commit timeout path (CTX-0464).
    ///
    /// Same contract as [`HostServices::ui_mount_with_expiry`]: an update
    /// commits a replacement subtree, so an expired call returns
    /// [`BridgeError::timeout`] and leaves the last-known-good block intact.
    fn ui_update_with_expiry(
        &self,
        handle: i64,
        component: &UiNode,
        expiry: Instant,
    ) -> Result<bool, BridgeError> {
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.ui_update(handle, component)
    }
    /// Claim the Core-owned exclusive focusable-overlay input capture for one
    /// transient interaction (CTX-0941, v2 scope of OQ-056).
    ///
    /// `handle` is a block handle this generation mounted into the `overlay`
    /// slot. The implementation owns capability gating (`ui.overlay`,
    /// deny-by-default), the single-owner switch (fail closed with
    /// [`E_UI_ALREADY_CAPTURED`] while another capture is active), and the
    /// bounded capture queue. The default fails closed with
    /// [`E_UI_UNAVAILABLE`], so a host without a capture surface can never
    /// gain ambient authority.
    fn ui_overlay_acquire(&self, _handle: i64) -> Result<(), BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui overlay capture surface",
        ))
    }
    /// Expiry-aware `ui_overlay_acquire` for the pre-commit timeout path
    /// (CTX-0464/CTX-0941).
    ///
    /// Acquiring grants exclusive input capture, so the bridge uses the
    /// check-then-act mutating path: implementations must check
    /// `Instant::now() > expiry` before granting and fail-closed with
    /// [`BridgeError::timeout`] without transferring authority when expired.
    /// The default checks expiry before delegating (fail-fast).
    fn ui_overlay_acquire_with_expiry(
        &self,
        handle: i64,
        expiry: Instant,
    ) -> Result<(), BridgeError> {
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.ui_overlay_acquire(handle)
    }
    /// Release the active focusable-overlay capture held by `handle`.
    ///
    /// Release is idempotent and Core-owned: `Ok(false)` means this generation
    /// owns no capture for `handle` (including a second release), never an
    /// error, so a plugin crash or a duplicate cancel can never wedge input.
    /// The default fails closed with [`E_UI_UNAVAILABLE`].
    fn ui_overlay_release(&self, _handle: i64) -> Result<bool, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui overlay capture surface",
        ))
    }
    /// Drain up to `max` captured input events for the active capture owned by
    /// `handle`, oldest first.
    ///
    /// This is the only plugin-visible observation path for captured input;
    /// Core never calls a plugin callback on the input hot path
    /// (`P0-AC-015`). A call by a generation that does not own the capture for
    /// `handle` fails closed with [`E_UI_NOT_OWNER`]; the default fails closed
    /// with [`E_UI_UNAVAILABLE`].
    fn ui_overlay_poll(&self, _handle: i64, _max: usize) -> Result<Vec<OverlayInput>, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui overlay capture surface",
        ))
    }

    /// Detailed poll per the accepted W-01 contract (CTX-0941, v2 scope).
    ///
    /// While the caller owns the session the result is active with drained
    /// events; after any terminal cause the owner's next poll reports
    /// released with the exact reason. A non-owner or stale handle fails
    /// with [`E_UI_NOT_OWNER`]. The default fails closed with
    /// [`E_UI_UNAVAILABLE`].
    fn ui_overlay_poll_detailed(
        &self,
        _handle: i64,
        _max: usize,
    ) -> Result<OverlayPoll, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui overlay capture surface",
        ))
    }

    /// Replace the overlay content for a session the caller owns (accepted
    /// W-01 `bitty.ui.overlay.update`, CTX-0941).
    ///
    /// The scene uses the accepted v1 node set under the v1 scene budgets;
    /// the bridge validates the component before this call. An update on a
    /// handle the caller does not own, or on a released handle, fails with
    /// [`E_UI_NOT_OWNER`] and keeps the previous content. Returns `Ok(true)`
    /// when content was replaced. The default fails closed with
    /// [`E_UI_UNAVAILABLE`].
    fn ui_overlay_update(&self, _handle: i64, _component: &UiNode) -> Result<bool, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui overlay capture surface",
        ))
    }

    /// Expiry-aware `ui_overlay_update` for the pre-commit timeout path.
    ///
    /// An update commits a replacement subtree, so an expired call returns
    /// [`BridgeError::timeout`] and leaves the last-known-good content
    /// intact. The default checks expiry before delegating.
    fn ui_overlay_update_with_expiry(
        &self,
        handle: i64,
        component: &UiNode,
        expiry: Instant,
    ) -> Result<bool, BridgeError> {
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.ui_overlay_update(handle, component)
    }

    /// Acquire a focusable overlay and its capture session from a
    /// presentation-hint spec (accepted W-01 `bitty.ui.overlay.acquire`,
    /// CTX-0941).
    ///
    /// `title` and `placeholder` are bounded text hints; unknown spec fields
    /// are ignored by the caller. On success Core mounts the surface and
    /// starts capture in one Core-owned switch and returns the opaque
    /// session handle bound to the calling generation. A failed call changes
    /// nothing. The default fails closed with [`E_UI_UNAVAILABLE`].
    fn ui_overlay_acquire_with_spec(
        &self,
        _title: &str,
        _placeholder: &str,
    ) -> Result<i64, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui overlay capture surface",
        ))
    }

    /// Expiry-aware spec acquire for the pre-commit timeout path.
    ///
    /// Acquiring grants exclusive input capture, so an expired call returns
    /// [`BridgeError::timeout`] without transferring authority. The default
    /// checks expiry before delegating.
    fn ui_overlay_acquire_with_spec_and_expiry(
        &self,
        title: &str,
        placeholder: &str,
        expiry: Instant,
    ) -> Result<i64, BridgeError> {
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.ui_overlay_acquire_with_spec(title, placeholder)
    }

    /// Release a session with an owner-supplied disposition (accepted W-01
    /// `bitty.ui.overlay.release`, CTX-0941).
    ///
    /// `reason` is `None` (defaults to `"released"`) or one of
    /// `"submitted"` / `"cancelled"`; any other value is a validation error
    /// and the session is unchanged. Release is idempotent: releasing an
    /// already-released handle of the owning generation succeeds. A
    /// non-owner or stale handle fails with [`E_UI_NOT_OWNER`]. Release is
    /// deliberately ungated by capability or safe mode so cleanup can never
    /// wedge. The default fails closed with [`E_UI_UNAVAILABLE`].
    fn ui_overlay_release_with_reason(
        &self,
        _handle: i64,
        _reason: Option<&str>,
    ) -> Result<bool, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui overlay capture surface",
        ))
    }

    /// Collect a bounded, read-only target snapshot (W-29, CTX-0942, DEC-0085 thin host).
    ///
    /// Returns at most `max` entries of the current cold-path collection over
    /// the Core terminal provider plus registered generic lenses, in tier
    /// priority then registration order. The engine stages every offer before
    /// any registry insert and fails closed with `E_DEF_LIMIT` on an oversized
    /// collection; the snapshot is ephemeral and never published to the Event
    /// Bus. Each entry is `(handle, kind, tier)` with `handle` an opaque
    /// 1-based token local to this read, `kind` one of
    /// `panel`/`workspace`/`block`/`node`/`link`, and `tier` one of
    /// `core`/`plugin`/`derived`. Handles carry no generation wire shape and
    /// are never transferable across sessions; dispatch revalidates against
    /// the live registry and fails closed with [`E_UI_NOT_OWNER`] on stale.
    /// The implementation owns capability gating (`ui.overlay`,
    /// deny-by-default) and the Core `ProviderMediator` bounds. Safe mode is
    /// unaffected: with zero plugins the Core provider yields an empty
    /// snapshot. The default fails closed with [`E_UI_UNAVAILABLE`], so a host
    /// without a targeting surface has no ambient authority. No new capability
    /// identifier is introduced.
    #[allow(clippy::type_complexity)]
    fn ui_targets_snapshot(&self, _max: usize) -> Result<Vec<(i64, String, String)>, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui targeting surface",
        ))
    }

    /// Register (or replace) this generation's named target lens (W-29, CTX-0942).
    ///
    /// `tier` is `plugin` or `derived`; the `core` tier stays host-owned and
    /// fails closed with `E_DEF_INVALID` from Lua. `targets` are
    /// `(kind, id)` pairs with `kind` one of
    /// `panel`/`workspace`/`block`/`node`/`link` and `id` a non-negative
    /// semantic id (never a raw compositor, PTY, or memory handle). The
    /// implementation validates against the Core provider-name grammar,
    /// rejects foreign shadowing (no duplicate names across generations),
    /// bounds the provider count, and maps both tiers onto the existing
    /// `DerivedProvider` lens (no `Plugin`-tier source exists in `bitty-ui`;
    /// reuse avoids any new targeting type). Registration grants addressability
    /// only: a target's declared actions stay metadata and a command still
    /// executes under its own owner's grants. Capability-gated on
    /// `ui.overlay` (deny-by-default); no new capability identifier. The
    /// default fails closed with [`E_UI_UNAVAILABLE`].
    #[allow(clippy::type_complexity)]
    fn ui_targets_register(
        &self,
        _name: &str,
        _tier: &str,
        _targets: &[(String, u64)],
    ) -> Result<bool, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui targeting surface",
        ))
    }

    /// Unregister this generation's named target lens (W-29, CTX-0942).
    ///
    /// Idempotent: `Ok(false)` means no lens of this generation carried
    /// `name`, never an error, so unload/cancel cleanup can never wedge.
    /// The default fails closed with [`E_UI_UNAVAILABLE`].
    fn ui_targets_unregister(&self, _name: &str) -> Result<bool, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui targeting surface",
        ))
    }

    /// Set the label-allocation charset policy from Lua-supplied sets (W-29, CTX-0942).
    ///
    /// The Core `LabelAllocator` owns the deterministic algorithm; the
    /// character sets are policy the plugin supplies. Both sets are validated
    /// by the Core `LabelPolicy` (non-empty, `<= 64` chars, `[a-z0-9]`,
    /// duplicate-free) and an invalid charset fails closed with
    /// `E_DEF_INVALID` before any session runs. Capability-gated on
    /// `ui.overlay`; no new capability. The default fails closed with
    /// [`E_UI_UNAVAILABLE`].
    fn ui_labels_set_policy(&self, _home: &str, _overflow: &str) -> Result<(), BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui labeling surface",
        ))
    }

    /// Allocate one unique label per anchor under the active policy (W-29, CTX-0942).
    ///
    /// `anchors` are viewport-local `(x, y)` cells; `width` is the viewport
    /// width for spatial left/right pooling. Deterministic for a fixed anchor
    /// set; bounded by the Core allocator caps and fail-closed with
    /// `E_DEF_LIMIT` on exhaustion. Capability-gated on `ui.overlay`; no new
    /// capability. The default fails closed with [`E_UI_UNAVAILABLE`].
    fn ui_labels_assign(
        &self,
        _anchors: &[(u16, u16)],
        _width: u16,
    ) -> Result<Vec<String>, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui labeling surface",
        ))
    }

    /// Start a bounded targeting session over the W-28 overlay capture (W-29, CTX-0942).
    ///
    /// Collects a fresh snapshot, maps `anchors`/`commands` positionally onto
    /// it, allocates labels through the Core allocator, binds each label to
    /// its typed command, and acquires the W-28 transient input capture for
    /// `overlay_handle`. `anchors`, `commands`, and the snapshot must agree in
    /// length; any mismatch, exhausted capacity, or stale handle fails closed
    /// with `E_DEF_INVALID`/`E_DEF_LIMIT`/`E_UI_NOT_OWNER` and releases
    /// capture. Returns the labels in snapshot order. No new session type is
    /// introduced: ownership is the existing overlay capture owner, and cancel
    /// is the existing overlay release plus dispatcher clear. No target or
    /// annotation internal is published to the Event Bus. Capability-gated on
    /// `ui.overlay`; no new capability. The default fails closed with
    /// [`E_UI_UNAVAILABLE`].
    fn ui_targets_session_start(
        &self,
        _overlay_handle: i64,
        _width: u16,
        _anchors: &[(u16, u16)],
        _commands: &[String],
    ) -> Result<Vec<String>, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui targeting surface",
        ))
    }

    /// Cancel the active targeting session and release capture (W-29, CTX-0942).
    ///
    /// Idempotent and Core-owned: `Ok(false)` means this generation owns no
    /// session for `overlay_handle`, never an error, so a crash or a duplicate
    /// cancel can never wedge input. Clears the label-to-command bindings and
    /// releases the W-28 capture through the existing release path. The
    /// default fails closed with [`E_UI_UNAVAILABLE`].
    fn ui_targets_session_cancel(&self, _overlay_handle: i64) -> Result<bool, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui targeting surface",
        ))
    }

    /// Resolve a selected label to a typed command id (W-29 command-dispatch bridge).
    ///
    /// Revalidates the bound target against the live registry before returning
    /// the command: a stale handle fails closed with [`E_UI_NOT_OWNER`] and an
    /// unknown/expired label with `E_DEF_INVALID`. The bridge executes
    /// nothing; the returned id is dispatched through the accepted command
    /// registry under its owner's grants. Ending the session (clearing
    /// bindings and releasing capture) is the caller's `session_cancel` or a
    /// follow-up dispatch that consumes the one-shot session; this call alone
    /// never publishes to the Event Bus. Capability-gated on `ui.overlay`; no
    /// new capability or privileged path. The default fails closed with
    /// [`E_UI_UNAVAILABLE`].
    fn ui_targets_dispatch(&self, _label: &str) -> Result<String, BridgeError> {
        Err(BridgeError::new(
            "runtime",
            E_UI_UNAVAILABLE,
            "host has no ui targeting surface",
        ))
    }

    /// Gate one `bitty.services.provide(iface)` declaration (LUA-OQ-8).
    ///
    /// The bridge validates shapes and stashes the impl functions into the
    /// generation capture; this hook lets the host reject the declaration
    /// against the caller manifest (undeclared interface). The default
    /// fails closed with `E_NOT_IMPLEMENTED` (no service backend); the
    /// runtime overrides it with the manifest-declaration check.
    fn service_provide_check(&self, iface: &str) -> Result<(), BridgeError> {
        let _ = iface;
        Err(BridgeError::not_implemented("bitty.services.provide"))
    }

    /// Resolve one `bitty.services.get(iface, opts)` call (LUA-OQ-8).
    ///
    /// `req` is the caller-supplied version requirement (`opts.version`),
    /// or `None` when the host must fall back to the caller manifest's
    /// `services.required` entry. `optional` mirrors `opts.optional`:
    /// when true, an unresolvable interface returns `Ok(None)` (Lua `nil`)
    /// instead of `E_SERVICE_RESOLUTION`. The default fails closed with
    /// `E_NOT_IMPLEMENTED` (no service backend); the runtime overrides it
    /// with manifest-checked directory resolution.
    fn service_resolve(
        &self,
        iface: &str,
        req: Option<&str>,
        optional: bool,
    ) -> Result<Option<ServiceRoute>, BridgeError> {
        let _ = (iface, req, optional);
        Err(BridgeError::not_implemented("bitty.services.get"))
    }

    /// Invoke one provided service method (LUA-OQ-8).
    ///
    /// `provider`/`generation`/`iface`/`method` pin the exact published record
    /// the consumer handle captured (no re-resolution: a republished
    /// replacement never hijacks a live handle, and a stale generation fails
    /// closed with `E_SERVICE_GONE`). The default fails closed
    /// with `E_NOT_IMPLEMENTED`; the runtime overrides it with
    /// generation-checked schema-validated cross-VM invocation
    /// (`E_SERVICE_GONE` past disappearance, `E_SERVICE_INVALID` on
    /// schema violation).
    fn service_call(
        &self,
        provider: &str,
        generation: u32,
        iface: &str,
        method: &str,
        args: &LuaValue,
    ) -> Result<LuaValue, BridgeError> {
        let _ = (provider, generation, iface, method, args);
        Err(BridgeError::not_implemented("bitty.services.get"))
    }

    /// Inspect runtime state for `bitty.debug.inspect` (CTX-0894, CTX-0897).
    ///
    /// Grant-gated (`debug.inspect`, checked first: `E_CAPABILITY_DENIED`)
    /// and read-only. Every target returns
    /// `{ target = <name>, items = <array>, truncated = <bool> }`, where
    /// `items` is capped at the host's inspect item ceiling and `truncated`
    /// reports whether rows were cut. Targets:
    /// - `"plugins"` → `{ id, version, state, generation }` sorted by id;
    ///   `state` is a stable lowercase label (`unloaded`, `loading`,
    ///   `activating`, `active`, `suspended`, `disposing`, `disposed`,
    ///   `failed`) and never carries a failure message
    /// - `"commands"` → `{ plugin, id, title }` sorted by plugin, id
    /// - `"events"` → `{ plugin, kind }` sorted by plugin, kind
    /// - `"grants"` → the calling plugin's own granted capability ids
    ///   (sorted strings); other plugins' grants are never exposed
    /// - `"panels"` → reserved; fails closed with `E_NOT_IMPLEMENTED`
    ///
    /// Any other target is `E_DEF_INVALID`. Results never include settings
    /// values, store contents, secrets, or terminal content.
    ///
    /// The default implementation always returns `E_NOT_IMPLEMENTED`.
    fn debug_inspect(&self, target: &str) -> Result<LuaValue, BridgeError> {
        let _ = target;
        Err(BridgeError::not_implemented("bitty.debug.inspect"))
    }

    /// Open or close an event trace for `bitty.debug.trace` (CTX-0894,
    /// CTX-0897).
    ///
    /// Grant-gated (`debug.trace`, checked first: `E_CAPABILITY_DENIED`).
    /// `opts` is `nil` (open with defaults) or a table; the bridge rejects any
    /// other type with `E_DEF_INVALID` before calling the host. Table keys:
    /// - `enabled` (bool, default `true`) → `true` opens a new trace,
    ///   `false` closes the trace named by `handle`
    /// - `filter` (string, optional) → exact topic, or a prefix when the
    ///   pattern ends with a single `*` (e.g. `"terminal.*"`); 1..=128
    ///   printable ASCII bytes
    /// - `max_events` (integer, optional) → ring-buffer size, drop-oldest
    ///   (default 1000, range 1..=10000)
    /// - `handle` (integer) → required with `enabled = false`, rejected
    ///   otherwise
    ///
    /// Unknown keys, wrong types, and out-of-range values are
    /// `E_DEF_INVALID`; opening beyond the per-plugin trace limit is
    /// `E_DEF_LIMIT`.
    ///
    /// Least privilege: a trace records only event kinds the calling plugin
    /// declares in its manifest `lazy.events` (the same precondition
    /// `bitty.events.subscribe` enforces), intersected with `filter`. A
    /// filter that matches no declared kind, or a plugin that declares no
    /// events, is accepted without error; the trace simply never records. Opening returns a fresh positive handle (monotonic,
    /// never reused); closing returns the closed handle, and closing an
    /// unknown or another plugin's handle is `E_DEF_INVALID` (the two cases
    /// are indistinguishable). Traces are dropped when the owning plugin is
    /// disposed, reloaded, or fails.
    ///
    /// The default implementation always returns `E_NOT_IMPLEMENTED`.
    fn debug_trace(&self, opts: &LuaValue) -> Result<i64, BridgeError> {
        let _ = opts;
        Err(BridgeError::not_implemented("bitty.debug.trace"))
    }

    /// Expiry-aware [`HostServices::debug_trace`] for the pre-commit timeout
    /// path (CTX-0897).
    ///
    /// Opening or closing a trace mutates host state, so the bridge routes
    /// `bitty.debug.trace` through its mutation guard and passes the call
    /// expiry. Same contract as [`HostServices::store_set_with_expiry`]:
    /// check expiry before committing and fail closed with
    /// [`BridgeError::timeout`] without opening or closing anything. The
    /// default checks expiry before delegating (fail-fast).
    fn debug_trace_with_expiry(
        &self,
        opts: &LuaValue,
        expiry: Instant,
    ) -> Result<i64, BridgeError> {
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.debug_trace(opts)
    }

    /// Drain buffered records for `bitty.debug.trace_get` (CTX-0894,
    /// CTX-0897).
    ///
    /// Grant-gated (`debug.trace`, checked first: `E_CAPABILITY_DENIED`).
    /// Returns `{ records = <array>, dropped = <n> }` and empties the buffer;
    /// `dropped` counts records lost to drop-oldest since the previous drain
    /// and resets after being reported. Each record is
    /// `{ topic, sequence, timestamp, payload }`: `topic` is the event kind,
    /// `sequence` the runtime event sequence, `timestamp` integer
    /// milliseconds on a monotonic host clock (no wall clock), and `payload`
    /// the event payload, replaced by `{ truncated = true, bytes = <n> }`
    /// when its encoded size exceeds the host payload ceiling.
    ///
    /// An unknown handle and a handle owned by another plugin both return
    /// `nil`, so a plugin cannot probe other plugins' traces.
    ///
    /// The default implementation fails closed with `E_NOT_IMPLEMENTED`.
    fn debug_trace_get(&self, handle: i64) -> Result<LuaValue, BridgeError> {
        let _ = handle;
        Err(BridgeError::not_implemented("bitty.debug.trace"))
    }

    /// Control runtime behavior for `bitty.debug.control` (CTX-0894).
    ///
    /// High-risk debug controls:
    /// - `reload_plugin(id: string)` → hot-reload a plugin by id
    /// - `suspend_plugin(id: string)` → suspend a plugin's execution
    /// - `resume_plugin(id: string)` → resume a suspended plugin
    /// - `clear_state(id: string)` → clear a plugin's persisted state
    ///
    /// The default implementation always returns `E_NOT_IMPLEMENTED`. Overriding
    /// implementations must enforce the `debug.control` capability grant
    /// (high-risk, requires explicit consent).
    fn debug_control(&self, action: &str, target: &str) -> Result<LuaValue, BridgeError> {
        let _ = (action, target);
        Err(BridgeError::not_implemented("bitty.debug.control"))
    }

    /// Expiry-aware [`HostServices::debug_control`] for the pre-commit
    /// timeout path (CTX-0897).
    ///
    /// Controls mutate runtime state, so the bridge routes
    /// `bitty.debug.control` through its mutation guard. Same contract as
    /// [`HostServices::store_set_with_expiry`]: check expiry before
    /// committing; the default checks before delegating (fail-fast).
    fn debug_control_with_expiry(
        &self,
        action: &str,
        target: &str,
        expiry: Instant,
    ) -> Result<LuaValue, BridgeError> {
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.debug_control(action, target)
    }

    /// Create a new panel for `bitty.panel.create` (CTX-0915, Issue #1596).
    ///
    /// Grant-gated on `panel.create`; returns `(id, generation)` tuple for the
    /// newly created panel. `panel_type` is one of `"terminal"`, `"rich"`,
    /// `"browser"`, `"helper"`, or `"canvas"`. The default denies: a host
    /// without a panel backend never grants ambient panel creation.
    fn panel_create(&self, panel_type: &str) -> Result<(u64, u64), BridgeError> {
        let _ = panel_type;
        Err(BridgeError::capability_denied("panel.create"))
    }

    /// Close a panel for `bitty.panel.close` (CTX-0915).
    ///
    /// Grant-gated on `panel.focus`; returns whether the panel was closed.
    /// The default denies.
    fn panel_close(&self, panel_id: u64) -> Result<bool, BridgeError> {
        let _ = panel_id;
        Err(BridgeError::capability_denied("panel.focus"))
    }

    /// Destroy a panel for `bitty.panel.destroy` (CTX-0915).
    ///
    /// Grant-gated on `panel.focus`; returns whether the panel was destroyed.
    /// The default denies.
    fn panel_destroy(&self, panel_id: u64) -> Result<bool, BridgeError> {
        let _ = panel_id;
        Err(BridgeError::capability_denied("panel.focus"))
    }

    /// Get panel presentation mode for `bitty.panel.get_presentation` (CTX-0915).
    ///
    /// Grant-gated on `panel.focus`; returns the presentation mode string
    /// (`"tiled"`, `"floating"`, `"fullscreen"`, `"scratchpad"`) or `None`
    /// if the panel does not exist. The default denies.
    fn panel_get_presentation(&self, panel_id: u64) -> Result<Option<String>, BridgeError> {
        let _ = panel_id;
        Err(BridgeError::capability_denied("panel.focus"))
    }

    /// Set panel presentation mode for `bitty.panel.set_presentation` (CTX-0915).
    ///
    /// Grant-gated on `panel.focus`; returns whether the mode was set.
    /// The default denies.
    fn panel_set_presentation(
        &self,
        panel_id: u64,
        presentation: &str,
    ) -> Result<bool, BridgeError> {
        let _ = (panel_id, presentation);
        Err(BridgeError::capability_denied("panel.focus"))
    }

    /// Toggle floating mode for `bitty.panel.toggle_floating` (CTX-0915).
    ///
    /// Grant-gated on `panel.focus`; returns whether the toggle succeeded.
    /// The default denies.
    fn panel_toggle_floating(&self, panel_id: u64) -> Result<bool, BridgeError> {
        let _ = panel_id;
        Err(BridgeError::capability_denied("panel.focus"))
    }

    /// Get panel state for `bitty.panel.get_state` (CTX-0915).
    ///
    /// Grant-gated on `panel.focus`; returns a table with panel state fields
    /// or `None` if the panel does not exist. The default denies.
    fn panel_get_state(&self, panel_id: u64) -> Result<Option<LuaValue>, BridgeError> {
        let _ = panel_id;
        Err(BridgeError::capability_denied("panel.focus"))
    }

    /// List workspaces for `bitty.workspace.list()` (CTX-0889, ADR-0014).
    ///
    /// Grant-gated on `workspace.read` (`E_CAPABILITY_DENIED` otherwise);
    /// returns rows in workspace order, bounded by
    /// [`WORKSPACE_LIST_MAX_ITEMS`]. The default denies: a host without a
    /// workspace backend never grants ambient read authority.
    fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, BridgeError> {
        Err(BridgeError::capability_denied("workspace.read"))
    }

    /// Enqueue one workspace mutation (CTX-0889, ADR-0014).
    ///
    /// Grant-gated on `workspace.control` only (`workspace.read` never
    /// implies it). Returns whether the bounded queue accepted the request;
    /// acceptance means queued, not applied. The default denies.
    fn workspace_request(&self, request: &WorkspaceRequest) -> Result<bool, BridgeError> {
        let _ = request;
        Err(BridgeError::capability_denied("workspace.control"))
    }

    /// Expiry-aware [`HostServices::workspace_request`] (CTX-0889).
    ///
    /// Enqueueing mutates host state, so the bridge routes it through the
    /// mutation guard. Same contract as
    /// [`HostServices::store_set_with_expiry`]: check expiry before
    /// committing; the default checks before delegating (fail-fast).
    fn workspace_request_with_expiry(
        &self,
        request: &WorkspaceRequest,
        expiry: Instant,
    ) -> Result<bool, BridgeError> {
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        self.workspace_request(request)
    }
}

/// Truncate a workspace name to [`WORKSPACE_NAME_MAX_CHARS`] at a char
/// boundary (bridge-side defense; Core applies the same bound).
fn bounded_workspace_name(name: &str) -> String {
    name.chars().take(WORKSPACE_NAME_MAX_CHARS).collect()
}

/// Marshal workspace rows into the bounded Lua array shape (CTX-0889).
///
/// CTX-0954: each row carries `scratchpad_count` (integer `0`/`1`) and
/// `scratchpad_occupied` (bool) under the same `workspace.read` grant as the
/// rest of the row; no panel capability is consulted.
fn workspace_list_value(rows: &[WorkspaceInfo]) -> LuaValue {
    LuaValue::array(
        rows.iter()
            .take(WORKSPACE_LIST_MAX_ITEMS)
            .map(|row| {
                LuaValue::table([
                    (
                        "id",
                        LuaValue::Integer(i64::try_from(row.id).unwrap_or(i64::MAX)),
                    ),
                    ("name", LuaValue::String(bounded_workspace_name(&row.name))),
                    ("active", LuaValue::Bool(row.active)),
                    (
                        "panel_count",
                        LuaValue::Integer(i64::try_from(row.panel_count).unwrap_or(i64::MAX)),
                    ),
                    (
                        "scratchpad_count",
                        LuaValue::Integer(
                            i64::try_from(row.scratchpad_count.min(SCRATCHPAD_COUNT_MAX))
                                .unwrap_or(i64::MAX),
                        ),
                    ),
                    (
                        "scratchpad_occupied",
                        LuaValue::Bool(row.scratchpad_occupied),
                    ),
                    (
                        "attention",
                        LuaValue::table([
                            ("bell", LuaValue::Bool(row.attention.bell)),
                            ("activity", LuaValue::Bool(row.attention.activity)),
                            ("exited", LuaValue::Bool(row.attention.exited)),
                        ]),
                    ),
                ])
            })
            .collect(),
    )
}

/// Marshal captured overlay input events into the bounded Lua array shape
/// (CTX-0941, accepted W-01 envelope). Each event is `{ seq, type, text }`
/// with the decided `key`, `text`, `pointer`, and `paste` tags (plus the
/// mechanism-internal `ime` class, observed as committed text). The host
/// already drained at most [`OVERLAY_CAPTURE_POLL_MAX`] events and bounded
/// each text payload.
fn overlay_events_value(events: &[OverlayInput]) -> LuaValue {
    LuaValue::array(
        events
            .iter()
            .map(|event| {
                LuaValue::table([
                    (
                        "seq",
                        LuaValue::Integer(i64::try_from(event.sequence).unwrap_or(i64::MAX)),
                    ),
                    ("type", LuaValue::String(event.kind.clone())),
                    ("text", LuaValue::String(event.text.clone())),
                ])
            })
            .collect(),
    )
}

/// Parse an overlay acquire spec into bounded `(title, placeholder)` hints.
///
/// The spec carries presentation hints only; every field is optional and
/// unknown fields are ignored. A missing or nil spec means no hints.
/// Over-bound or misshaped specs fail closed with the existing value-shape
/// errors and no session is created.
fn parse_overlay_spec<'gc>(value: Value<'gc>) -> Result<(String, String), BridgeError> {
    if matches!(value, Value::Nil) {
        return Ok((String::new(), String::new()));
    }
    let limits = MarshallingLimits {
        max_depth: 4,
        max_nodes: 32,
        max_bytes: OVERLAY_CALL_MAX_BYTES,
    };
    let parsed = LuaValue::from_lua(value, limits)?;
    let table = match parsed {
        LuaValue::Table(_) => parsed,
        _ => {
            return Err(BridgeError::value(
                "E_VALUE_TYPE",
                "ui.overlay.acquire spec must be a table",
            ));
        }
    };
    let field = |name: &str| -> Result<String, BridgeError> {
        match table.get(name) {
            None | Some(LuaValue::Nil) => Ok(String::new()),
            Some(LuaValue::String(text)) => Ok(text.clone()),
            Some(_) => Err(BridgeError::value(
                "E_VALUE_TYPE",
                "ui.overlay.acquire spec fields must be strings",
            )),
        }
    };
    let title = field("title")?;
    let placeholder = field("placeholder")?;
    if title.len() > OVERLAY_SPEC_TITLE_MAX_BYTES {
        return Err(BridgeError::value(
            "E_VALUE_BYTES",
            "ui.overlay.acquire title exceeds size limit",
        ));
    }
    if placeholder.len() > OVERLAY_SPEC_PLACEHOLDER_MAX_BYTES {
        return Err(BridgeError::value(
            "E_VALUE_BYTES",
            "ui.overlay.acquire placeholder exceeds size limit",
        ));
    }
    if title.len() + placeholder.len() > OVERLAY_CALL_MAX_BYTES {
        return Err(BridgeError::value(
            "E_VALUE_BYTES",
            "ui.overlay.acquire spec exceeds size limit",
        ));
    }
    Ok((title, placeholder))
}
/// Marshal a detailed overlay poll result into the accepted W-01 table shape
/// (CTX-0941): `{ status, seq, events, overflowed, reason? }` with exactly
/// these fields. `reason` is present only with `"released"`.
fn overlay_poll_value(poll: &OverlayPoll) -> LuaValue {
    let status = if poll.active { "active" } else { "released" };
    let mut pairs = vec![
        (
            LuaValue::String("status".to_string()),
            LuaValue::String(status.to_string()),
        ),
        (
            LuaValue::String("seq".to_string()),
            LuaValue::Integer(i64::try_from(poll.seq).unwrap_or(i64::MAX)),
        ),
        (
            LuaValue::String("events".to_string()),
            overlay_events_value(&poll.events),
        ),
        (
            LuaValue::String("overflowed".to_string()),
            LuaValue::Bool(poll.overflowed),
        ),
    ];
    if let Some(reason) = poll.reason.as_ref() {
        pairs.push((
            LuaValue::String("reason".to_string()),
            LuaValue::String(reason.clone()),
        ));
    }
    LuaValue::Table(pairs)
}

/// Validate one `bitty.ui.targets.register(def)` table (W-29, CTX-0942).
///
/// `def` is `{ name = <string>, tier = "plugin"|"derived",
/// targets = { {kind = <string>, id = <integer>}, ... } }`. Shapes fail
/// closed with `E_UI_COMPONENT_INVALID`; capacity fails with `E_DEF_LIMIT`;
/// the reserved `core` tier fails with `E_DEF_INVALID` (host-owned).
#[allow(clippy::type_complexity)]
fn parse_targets_register(
    value: &LuaValue,
) -> Result<(String, String, Vec<(String, u64)>), BridgeError> {
    if !matches!(value, LuaValue::Table(_)) {
        return Err(component_invalid(
            "ui.targets.register definition must be a table",
        ));
    }
    let name = match value.get("name") {
        Some(LuaValue::String(name)) if !name.is_empty() => name.clone(),
        _ => {
            return Err(component_invalid(
                "ui.targets.register name must be a non-empty string",
            ));
        }
    };
    if name.len() > UI_TARGETS_PROVIDER_NAME_MAX_BYTES {
        return Err(BridgeError::new(
            "budget",
            "E_DEF_LIMIT",
            format!("ui.targets.register name exceeds {UI_TARGETS_PROVIDER_NAME_MAX_BYTES} bytes"),
        ));
    }
    let tier = match value.get("tier") {
        Some(LuaValue::String(raw)) => raw.clone(),
        _ => {
            return Err(component_invalid(
                "ui.targets.register tier must be a string",
            ));
        }
    };
    if tier == "core" {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            "the 'core' provider tier is not registrable from Lua",
        ));
    }
    if tier != "plugin" && tier != "derived" {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            format!("unknown provider tier '{tier}'"),
        ));
    }
    let targets_value = value
        .get("targets")
        .ok_or_else(|| component_invalid("ui.targets.register targets must be an array"))?;
    let items = dense_targets_array(targets_value)?;
    if items.len() > UI_TARGETS_REGISTER_MAX {
        return Err(BridgeError::new(
            "budget",
            "E_DEF_LIMIT",
            format!("ui.targets.register targets exceed {UI_TARGETS_REGISTER_MAX} offers"),
        ));
    }
    let mut targets = Vec::with_capacity(items.len());
    for item in items {
        targets.push(parse_target_offer(item)?);
    }
    Ok((name, tier, targets))
}

/// Validate one `targets` entry: `{ kind = <string>, id = <integer> }`.
fn parse_target_offer(value: &LuaValue) -> Result<(String, u64), BridgeError> {
    if !matches!(value, LuaValue::Table(_)) {
        return Err(component_invalid(
            "ui.targets.register target must be a table",
        ));
    }
    let kind = match value.get("kind") {
        Some(LuaValue::String(raw)) => raw.clone(),
        _ => {
            return Err(component_invalid(
                "ui.targets.register target.kind must be a string",
            ));
        }
    };
    match kind.as_str() {
        "panel" | "workspace" | "block" | "node" | "link" => {}
        _ => {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_INVALID",
                format!("unknown target kind '{kind}'"),
            ));
        }
    }
    let id = match value.get("id") {
        Some(LuaValue::Integer(id)) if *id >= 0 => *id as u64,
        _ => {
            return Err(component_invalid(
                "ui.targets.register target.id must be a non-negative integer",
            ));
        }
    };
    Ok((kind, id))
}

/// Validate an array of `{ x = <int>, y = <int> }` anchors (W-29).
fn parse_targets_anchors(value: &LuaValue) -> Result<Vec<(u16, u16)>, BridgeError> {
    let items = dense_targets_array(value)?;
    if items.len() > UI_LABELS_ASSIGN_MAX {
        return Err(BridgeError::new(
            "budget",
            "E_DEF_LIMIT",
            format!("ui anchors exceed {UI_LABELS_ASSIGN_MAX}"),
        ));
    }
    let mut anchors = Vec::with_capacity(items.len());
    for item in items {
        if !matches!(item, LuaValue::Table(_)) {
            return Err(component_invalid("ui anchor must be a table"));
        }
        let x = match item.get("x") {
            Some(LuaValue::Integer(raw)) if (0..=i64::from(u16::MAX)).contains(raw) => *raw as u16,
            _ => {
                return Err(component_invalid(
                    "ui anchor.x must be an integer in 0..=65535",
                ));
            }
        };
        let y = match item.get("y") {
            Some(LuaValue::Integer(raw)) if (0..=i64::from(u16::MAX)).contains(raw) => *raw as u16,
            _ => {
                return Err(component_invalid(
                    "ui anchor.y must be an integer in 0..=65535",
                ));
            }
        };
        anchors.push((x, y));
    }
    Ok(anchors)
}

/// Validate an array of command-id strings (W-29).
fn parse_targets_commands(value: &LuaValue) -> Result<Vec<String>, BridgeError> {
    let items = dense_targets_array(value)?;
    if items.len() > UI_LABELS_ASSIGN_MAX {
        return Err(BridgeError::new(
            "budget",
            "E_DEF_LIMIT",
            format!("ui commands exceed {UI_LABELS_ASSIGN_MAX}"),
        ));
    }
    let mut commands = Vec::with_capacity(items.len());
    for item in items {
        match item {
            LuaValue::String(command) => {
                if command.is_empty() {
                    return Err(component_invalid(
                        "ui command binding must be a non-empty string",
                    ));
                }
                commands.push(command.clone());
            }
            _ => {
                return Err(component_invalid("ui command binding must be a string"));
            }
        }
    }
    Ok(commands)
}

/// Dense 1-based array items of `value`, failing closed on any non-integer
/// or non-consecutive key.
fn dense_targets_array(value: &LuaValue) -> Result<Vec<&LuaValue>, BridgeError> {
    let LuaValue::Table(pairs) = value else {
        return Err(component_invalid("ui array must be a dense 1-based array"));
    };
    let mut indexed: Vec<(i64, &LuaValue)> = Vec::with_capacity(pairs.len());
    for (key, item) in pairs {
        match key {
            LuaValue::Integer(index) if *index >= 1 => indexed.push((*index, item)),
            _ => {
                return Err(component_invalid("ui array must be a dense 1-based array"));
            }
        }
    }
    indexed.sort_by_key(|(index, _)| *index);
    for (position, (index, _)) in indexed.iter().enumerate() {
        if *index != position as i64 + 1 {
            return Err(component_invalid("ui array must be a dense 1-based array"));
        }
    }
    Ok(indexed.into_iter().map(|(_, item)| item).collect())
}

/// Validate a stable workspace id argument (positive integer) for `what`.
fn workspace_id_arg(value: Value<'_>, what: &str) -> Result<u64, BridgeError> {
    match value {
        Value::Integer(id) if id >= 1 => Ok(id as u64),
        _ => Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            format!("{what} must be a positive integer workspace id"),
        )),
    }
}

/// Validate a `bitty.workspace.rename` name: string, non-blank, bounded,
/// and free of control characters (names render in Core chrome).
fn workspace_name_arg(value: Value<'_>) -> Result<String, BridgeError> {
    let Value::String(raw) = value else {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            "workspace.rename name must be a string",
        ));
    };
    if raw.as_bytes().len() > WORKSPACE_RENAME_MAX_BYTES {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_LIMIT",
            format!("workspace.rename name exceeds {WORKSPACE_RENAME_MAX_BYTES} bytes"),
        ));
    }
    let Ok(name) = std::str::from_utf8(raw.as_bytes()) else {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            "workspace.rename name must be valid UTF-8",
        ));
    };
    if name.trim().is_empty() || name.chars().any(char::is_control) {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            "workspace.rename name must be non-blank without control characters",
        ));
    }
    Ok(name.to_string())
}

/// Parse a `bitty.workspace.focus` target: an integer stable id or a table
/// `{ index = n }` with a 1-based position.
fn workspace_focus_arg<'gc>(
    ctx: Context<'gc>,
    value: Value<'gc>,
) -> Result<WorkspaceRequest, BridgeError> {
    match value {
        Value::Table(table) => match table.get::<_, Value>(ctx, "index") {
            Ok(Value::Integer(index)) if index >= 1 => {
                Ok(WorkspaceRequest::FocusIndex(index as u64))
            }
            _ => Err(BridgeError::new(
                "validation",
                "E_DEF_INVALID",
                "workspace.focus { index = n } needs a positive integer index",
            )),
        },
        other => workspace_id_arg(other, "workspace.focus target").map(WorkspaceRequest::FocusId),
    }
}

/// Build one `bitty.workspace.*` mutation callback: `parse` validates the
/// Lua arguments into a request, then the bridge enqueues it through the
/// mutation guard and returns the boolean acceptance.
fn workspace_mutation<'gc>(
    ctx: Context<'gc>,
    state: &Rc<BridgeState>,
    parse: for<'a> fn(Context<'a>, [Value<'a>; 2]) -> Result<WorkspaceRequest, BridgeError>,
) -> Callback<'gc> {
    let state = state.clone();
    Callback::from_fn(&ctx, move |ctx, _exec, mut stack| {
        let request = parse(ctx, [stack.get(0), stack.get(1)]).map_err(|e| e.to_error(ctx))?;
        let accepted = state
            .bounded_mutation(|expiry| {
                state
                    .services
                    .workspace_request_with_expiry(&request, expiry)
            })
            .map_err(|e| e.to_error(ctx))?;
        stack.replace(ctx, Value::Boolean(accepted));
        Ok(CallbackReturn::Return)
    })
}

/// One captured command registration from `init.lua`.
#[derive(Debug, Clone)]
pub struct CommandRegistration {
    /// Command id as registered (unqualified).
    pub id: String,
    /// Bounded title.
    pub title: String,
    /// Bounded description.
    pub description: String,
    /// Stashed `run` function handle (generation-scoped).
    pub run: StashedFunction,
}

/// One captured event subscription from `init.lua`.
#[derive(Debug, Clone)]
pub struct EventSubscription {
    /// Event kind name (e.g. `terminal.opened`).
    pub kind: String,
    /// Stashed handler function handle.
    pub handler: StashedFunction,
}

/// One captured timer creation from `init.lua` or a callback.
#[derive(Debug, Clone)]
pub struct TimerRegistration {
    /// Numeric handle returned to Lua.
    pub handle: i64,
    /// Delay in milliseconds.
    pub delay_ms: u64,
    /// Stashed callback function handle.
    pub callback: StashedFunction,
}

/// One captured key-binding suggestion from `init.lua` (CTX-0707, LUA-OQ-5).
///
/// Suggestion only: precedence (`user > workspace > first-party/default >
/// plugin`) and conflict diagnostics are applied host-side after activation.
/// `when` is normalized to `"global"` at capture (the only v1 context).
#[derive(Debug, Clone)]
pub struct KeymapSuggestion {
    /// Suggested chord (shipped config grammar, validated at application).
    pub chord: String,
    /// Command registered by the same generation.
    pub command: String,
    /// Activation context, always `"global"` in v1.
    pub when: String,
}

/// One captured task spawn from `init.lua` or a callback (CTX-0707, LUA-OQ-9).
///
/// The bridge owns the integer handle and stashes the entry function;
/// scheduling, cooperative cancellation, and resumption through the event
/// path stay host-side.
#[derive(Debug, Clone)]
pub struct TaskRegistration {
    /// Numeric handle returned to Lua.
    pub handle: i64,
    /// Stashed entry function handle (generation-scoped).
    pub entry: StashedFunction,
}

/// One resolved service route handed to `bitty.services.get` (LUA-OQ-8).
///
/// A snapshot, not a live reference: the bridge builds one callable closure
/// per method pinning `provider` (id plus generation, exact-published
/// record — a republished replacement never hijacks the handle). The host
/// owns freshness: past disappearance the pinned record is gone and the
/// closure fails closed with `E_SERVICE_GONE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRoute {
    /// Providing plugin id.
    pub provider: String,
    /// Activation generation that published the record.
    pub generation: u32,
    /// Service interface name.
    pub iface: String,
    /// Published interface version.
    pub version: String,
    /// Provided method names (closure set).
    pub methods: Vec<String>,
}

/// One captured service method from a `provide` impl table (LUA-OQ-8).
///
/// Impl tables are plain function tables: every value must be a Lua
/// function, stashed generation-scoped at capture. Non-function entries
/// fail closed with `E_DEF_INVALID`; the handle set never crosses as
/// values (functions cannot cross the host bridge).
#[derive(Debug, Clone)]
pub struct ServiceMethod {
    /// Method name (impl-table key).
    pub name: String,
    /// Stashed impl function handle (generation-scoped).
    pub func: StashedFunction,
}

/// One captured service provision from `init.lua` (LUA-OQ-8).
///
/// The bridge captures only; the runtime publishes the record into the
/// service directory at activation after checking it against the caller
/// manifest's `services.provided` entry (version and schemas come from the
/// manifest, never from Lua).
#[derive(Debug, Clone)]
pub struct ServiceProvision {
    /// Service interface name.
    pub iface: String,
    /// Provided methods in impl-table order.
    pub methods: Vec<ServiceMethod>,
}

/// Generation-scoped capture of `init.lua` registrations, validated by the
/// runtime after `init.lua` returns and before atomic commit.
#[derive(Debug, Default)]
pub struct RegistrationCapture {
    /// Captured commands in registration order.
    pub commands: Vec<CommandRegistration>,
    /// Captured event subscriptions in registration order.
    pub events: Vec<EventSubscription>,
    /// Captured timers keyed by handle.
    pub timers: Vec<TimerRegistration>,
    /// Next timer handle.
    pub next_timer_handle: i64,
    /// Captured keymap suggestions in suggestion order (CTX-0707).
    pub keymaps: Vec<KeymapSuggestion>,
    /// Captured task spawns keyed by handle (CTX-0707).
    pub tasks: Vec<TaskRegistration>,
    /// Next task handle.
    pub next_task_handle: i64,
    /// Captured service provisions in declaration order (LUA-OQ-8).
    pub services: Vec<ServiceProvision>,
}

impl RegistrationCapture {
    /// Create an empty capture.
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
            events: Vec::new(),
            timers: Vec::new(),
            next_timer_handle: 1,
            keymaps: Vec::new(),
            tasks: Vec::new(),
            next_task_handle: 1,
            services: Vec::new(),
        }
    }

    /// Remove a timer by handle; returns whether it existed.
    pub fn cancel_timer(&mut self, handle: i64) -> bool {
        let before = self.timers.len();
        self.timers.retain(|timer| timer.handle != handle);
        self.timers.len() != before
    }

    /// Remove a task by handle; returns whether it existed (CTX-0707).
    ///
    /// Bridge-side removal releases the RC-4 cap slot; cooperative
    /// cancellation at the next host slice stays host-side.
    pub fn cancel_task(&mut self, handle: i64) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|task| task.handle != handle);
        self.tasks.len() != before
    }

    /// Allocate the next timer handle with checked arithmetic (HOST-002).
    ///
    /// Returns the handle to assign, or `None` when the counter is exhausted:
    /// the caller must fail the `timers.create` call closed with typed
    /// `E_DEF_LIMIT` instead of wrapping the handle (which would alias a
    /// live timer in release builds). `i64::MAX` itself is reserved as the
    /// exhaustion sentinel and never issued, so handles are `1..i64::MAX`;
    /// losing one value out of 2^63 is immaterial, aliasing is not.
    pub fn alloc_timer_handle(&mut self) -> Option<i64> {
        if self.next_timer_handle == i64::MAX {
            return None;
        }
        let handle = self.next_timer_handle;
        // `handle < i64::MAX` here, so the increment cannot overflow; the
        // `checked_add` documents the no-wrap invariant rather than handling
        // a reachable `None`.
        self.next_timer_handle = self
            .next_timer_handle
            .checked_add(1)
            .expect("handle below i64::MAX increments");
        Some(handle)
    }

    /// Allocate the next task handle with checked arithmetic (CTX-0707).
    ///
    /// Same no-wrap contract as [`Self::alloc_timer_handle`]: `i64::MAX`
    /// is the reserved exhaustion sentinel and is never issued, so handles
    /// are `1..i64::MAX`; the caller fails the `tasks.spawn` call closed
    /// with typed `E_BUDGET_TASK` instead of aliasing a live task.
    pub fn alloc_task_handle(&mut self) -> Option<i64> {
        if self.next_task_handle == i64::MAX {
            return None;
        }
        let handle = self.next_task_handle;
        self.next_task_handle = self
            .next_task_handle
            .checked_add(1)
            .expect("handle below i64::MAX increments");
        Some(handle)
    }
}

/// Shared `bitty` bridge state installed into one VM.
struct BridgeState {
    services: Rc<dyn HostServices>,
    capture: Rc<RefCell<RegistrationCapture>>,
    limits: MarshallingLimits,
    deadline_ms: u64,
    spawn_deadline_ms: Rc<Cell<u64>>,
    in_call: Rc<Cell<bool>>,
}

/// Re-entrancy guard for one bridge call: clears `in_call` on drop.
struct CallGuard(Rc<Cell<bool>>);

impl Drop for CallGuard {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl BridgeState {
    /// Enter one bridge call, rejecting re-entrant calls fail-closed.
    fn enter(&self) -> Result<CallGuard, BridgeError> {
        if self.in_call.get() {
            return Err(BridgeError::new(
                "runtime",
                "E_BRIDGE_REENTRANT",
                "bridge call re-entered",
            ));
        }
        self.in_call.set(true);
        Ok(CallGuard(self.in_call.clone()))
    }

    /// Check-then-act bridge guard for cheap read-only host calls (CTX-0464
    /// gap 3).
    ///
    /// Fail-closed with typed `E_TIMEOUT`: checks the call expiry before
    /// invoking `f` (no side effects when already expired), passes the expiry
    /// to `f` where relevant, and checks again after, so a slow read is never
    /// delivered past its budget. Mutating calls use
    /// [`Self::bounded_mutation`] instead: a committed effect must never be
    /// discarded as a timeout (the old applied-then-timeout bug). The new path
    /// guarantees post-deadline effects never commit when services honor the
    /// expiry (tests prove it; real read services are in-memory fast).
    ///
    /// The post-call check uses charged RC-1 time ([`crate::BudgetMark`]):
    /// durable commit I/O of committed disk-backed `bitty.store.set` writes
    /// during `f` (a cross-VM provider writing its store inside
    /// `service_call`) is not charged to this call, up to
    /// [`crate::STORE_COMMIT_CREDIT_MAX_MS`] (bitty #1518). Every other
    /// delay, including store validation/encoding, a slow read, or slow
    /// provider Lua, still times out.
    fn bounded<T>(
        &self,
        f: impl FnOnce(Instant) -> Result<T, BridgeError>,
    ) -> Result<T, BridgeError> {
        let _guard = self.enter()?;
        let mark = crate::BudgetMark::now();
        let deadline = Duration::from_millis(self.deadline_ms);
        let expiry = mark.start() + deadline;
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        let out = f(expiry)?;
        if mark.read().charged > deadline {
            return Err(BridgeError::timeout());
        }
        Ok(out)
    }

    /// [`Self::bounded_mutation`] for the plugin store write
    /// (`store_set_with_expiry`), crediting only its durable commit I/O.
    ///
    /// Opens a [`crate::StoreCommitWindow`] so the store backend can report
    /// its temp-file write, fsync, and rename time through
    /// [`crate::record_store_commit_io`]. On `Ok` the write has committed
    /// atomically and the reported I/O (bounded by this call's measured
    /// duration) is excluded from the RC-1 hard wall limit of every
    /// enclosing callback on this thread (this VM's, and a consumer VM
    /// waiting through a service call), capped per callback at
    /// [`crate::STORE_COMMIT_CREDIT_MAX_MS`]. Validation, cloning, quota
    /// checks, and encoding stay charged; in-memory stores report nothing;
    /// refused or failed writes are never credited (bitty #1518).
    fn bounded_store_commit(
        &self,
        f: impl FnOnce(Instant) -> Result<(), BridgeError>,
    ) -> Result<(), BridgeError> {
        let started = Instant::now();
        let window = crate::StoreCommitWindow::open();
        let out = self.bounded_mutation(f);
        window.close(out.is_ok(), started.elapsed());
        out
    }

    /// Check-then-act bridge guard for mutating host calls
    /// (`store_set_with_expiry`/`notify_show_with_expiry`).
    ///
    /// Identical pre-call deadline and re-entrancy guard as [`Self::bounded`],
    /// but with **no post-call deadline failure**: once `f` returns `Ok` the
    /// mutation has committed, so raising `E_TIMEOUT` afterwards would be the
    /// applied-then-timeout bug CTX-0464 removed — the effect lands while the
    /// plugin is told the call failed. The service keeps the pre-commit guard
    /// (checks the passed expiry before committing), so an already-expired
    /// call still fails closed with no effect.
    ///
    /// This matters for the real plugin store: `bitty.store.set` performs
    /// bounded but non-instant atomic temp-then-rename I/O, and on slow
    /// platforms (Windows CI filesystem scanning a freshly created state
    /// file) that write can exceed the 50 ms cheap-call budget after it has
    /// already committed. Reporting a committed write as `E_TIMEOUT` would
    /// fail the plugin callback spuriously (CTX-0477). The store path goes
    /// through [`Self::bounded_store_commit`], which also keeps the commit
    /// time out of the enclosing callbacks' RC-1 wall clocks (bitty #1518).
    fn bounded_mutation<T>(
        &self,
        f: impl FnOnce(Instant) -> Result<T, BridgeError>,
    ) -> Result<T, BridgeError> {
        let _guard = self.enter()?;
        let start = Instant::now();
        let expiry = start + Duration::from_millis(self.deadline_ms);
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        f(expiry)
    }

    /// Spawn bridge guard through the same timeout path with the spawn
    /// deadline (CTX-0464 gap 4).
    ///
    /// `process.spawn` is a supervised long-running call with its own
    /// explicit timeout contract (default 5 s, maximum 30 s, enforced by
    /// killing and reaping the child, CTX-0445): the old guard kept only the
    /// re-entrancy rejection with no timeout handle, hiding unbounded waits
    /// outside any bridge budget. The new guard enforces the spawn deadline
    /// (`SPAWN_TIMEOUT_MS` by default, configurable via
    /// `LuaVm::set_spawn_deadline_ms`) check-then-act like [`Self::bounded`]:
    /// slow-but-within-contract spawns are still delivered (the existing
    /// `slow_spawn_is_delivered_not_timed_out` pin keeps passing), spawns past
    /// contract fail-closed with `E_TIMEOUT` and no result delivered. The
    /// re-entrancy guard still applies.
    fn bounded_spawn<T>(
        &self,
        f: impl FnOnce(Instant) -> Result<T, BridgeError>,
    ) -> Result<T, BridgeError> {
        let _guard = self.enter()?;
        let start = Instant::now();
        let spawn_ms = self.spawn_deadline_ms.get();
        let expiry = start + Duration::from_millis(spawn_ms);
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        let out = f(expiry)?;
        if Instant::now() > expiry {
            return Err(BridgeError::timeout());
        }
        Ok(out)
    }
}

impl LuaVm {
    /// Set the source-only module root used by the injected `require`.
    pub fn with_module_root(&mut self, root: impl Into<PathBuf>) -> &mut Self {
        self.module_root = Some(root.into());
        self
    }

    /// Whether the `bitty` host module has been installed.
    #[must_use]
    pub fn host_installed(&self) -> bool {
        self.host_installed
    }

    /// Install the read-only `bitty` host module and (when a module root is
    /// set) the rooted source-only `require`.
    ///
    /// Must be called before executing `init.lua`. Idempotent: a second call
    /// is a no-op.
    ///
    /// # Errors
    ///
    /// [`VmError::Budget`] when a limit is zero; [`VmError::Load`] when the
    /// configured module root cannot be canonicalized.
    pub fn install_host_module(
        &mut self,
        services: Rc<dyn HostServices>,
        limits: MarshallingLimits,
        deadline_ms: u64,
    ) -> Result<(), VmError> {
        if limits.max_depth == 0 || limits.max_nodes == 0 || limits.max_bytes == 0 {
            return Err(VmError::Budget(
                "marshalling limits must be non-zero".into(),
            ));
        }
        if deadline_ms == 0 {
            return Err(VmError::Budget("host deadline must be non-zero".into()));
        }
        if self.host_installed {
            return Ok(());
        }
        self.marshalling_limits = limits;
        self.host_deadline_ms = deadline_ms;

        let state = Rc::new(BridgeState {
            services,
            capture: self.capture.clone(),
            limits,
            deadline_ms,
            spawn_deadline_ms: self.spawn_deadline_ms.clone(),
            in_call: self.in_bridge_call.clone(),
        });

        self.lua.enter(|ctx| {
            let root = build_bitty_root(ctx, &state);
            ctx.set_global("bitty", root);
        });

        if self.module_root.is_some() {
            self.install_require()?;
        }
        self.host_installed = true;
        Ok(())
    }

    /// Clone the current generation-scoped registration capture.
    #[must_use]
    pub fn take_registrations(&self) -> RegistrationCapture {
        let capture = self.capture.borrow();
        RegistrationCapture {
            commands: capture.commands.clone(),
            events: capture.events.clone(),
            timers: capture.timers.clone(),
            next_timer_handle: capture.next_timer_handle,
            keymaps: capture.keymaps.clone(),
            tasks: capture.tasks.clone(),
            next_task_handle: capture.next_task_handle,
            services: capture.services.clone(),
        }
    }

    /// Invoke a stashed registration handle (command `run`, event handler,
    /// timer callback) with bounded arguments, under the same RC-1/RC-2
    /// budgets as any other VM slice.
    ///
    /// # Errors
    ///
    /// [`VmError::Suspended`] when the shared VM is already suspended;
    /// [`VmError::Runtime`] for a Lua runtime error; [`VmError::Load`] for an
    /// argument-marshalling failure.
    pub fn call_function(
        &mut self,
        function: &StashedFunction,
        args: &[LuaValue],
    ) -> Result<LuaValue, VmError> {
        let limits = self.marshalling_limits;
        let stashed = self.lua.enter(|ctx| {
            let func: Function = ctx.fetch(function);
            let argv: Vec<Value> = args.iter().map(|v| v.to_lua(ctx)).collect();
            ctx.stash(phodopus::Executor::start(
                ctx,
                func,
                phodopus::Variadic(argv),
            ))
        });

        match self.drive_stashed(stashed, crate::BudgetMark::now())? {
            crate::DriveOutcome::Suspended { reason, .. } => Err(VmError::Suspended { reason }),
            crate::DriveOutcome::Failed { message } => Err(VmError::Load(message)),
            crate::DriveOutcome::Ready { stashed } => {
                let parked = self
                    .lua
                    .enter(|ctx| ctx.fetch(&stashed).mode() == ExecutorMode::HostSuspended);
                if parked {
                    return Err(VmError::Runtime(
                        "executor parked on a host operation after drive".to_string(),
                    ));
                }
                let outcome = self.lua.enter(|ctx| {
                    let exec = ctx.fetch(&stashed);
                    match exec.take_result::<Value>(ctx) {
                        Ok(Ok(value)) => {
                            LuaValue::from_lua(value, limits).map_err(|e| e.to_string())
                        }
                        Ok(Err(err)) => Err(format!("{err}")),
                        Err(err) => Err(format!("{err:?}")),
                    }
                });
                outcome.map_err(VmError::Runtime)
            }
        }
    }
}

/// Initialise a fresh `bitty` namespace inside the current arena.
fn build_bitty_root<'gc>(ctx: Context<'gc>, state: &Rc<BridgeState>) -> Value<'gc> {
    let commands = Table::new(&ctx);
    commands
        .set(
            ctx,
            "register",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let def = match stack.get(0) {
                        Value::Table(table) => table,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "command definition must be a table",
                            )
                            .to_error(ctx));
                        }
                    };
                    let id = required_string(ctx, def, "id")?;
                    let title = required_string(ctx, def, "title")?;
                    let description = optional_string(ctx, def, "description").unwrap_or_default();
                    let run = match def.get_value(ctx, "run") {
                        Value::Function(function) => ctx.stash(function),
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "command 'run' must be a function",
                            )
                            .to_error(ctx));
                        }
                    };
                    // HOST-002 admission: count + length caps enforced at the
                    // bridge before the push, so a hostile `init.lua` fails
                    // closed with bounded memory instead of growing the Vecs
                    // without limit. Duplicate-id rejection stays in the
                    // runtime validator, which sees the full capture plus the
                    // manifest. `E_DEF_LIMIT` marks quota-shaped rejections;
                    // `E_DEF_INVALID` marks malformed fields.
                    if id.len() > REGISTRATION_MAX_ID_BYTES {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            format!("command id exceeds {REGISTRATION_MAX_ID_BYTES} bytes"),
                        )
                        .to_error(ctx));
                    }
                    if title.len() > REGISTRATION_MAX_TITLE_BYTES {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            format!("command title exceeds {REGISTRATION_MAX_TITLE_BYTES} bytes"),
                        )
                        .to_error(ctx));
                    }
                    if description.len() > REGISTRATION_MAX_DESCRIPTION_BYTES {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            format!(
                                "command description exceeds \
                                 {REGISTRATION_MAX_DESCRIPTION_BYTES} bytes"
                            ),
                        )
                        .to_error(ctx));
                    }
                    {
                        let mut capture = state.capture.borrow_mut();
                        if capture.commands.len() >= REGISTRATION_MAX_COMMANDS {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_LIMIT",
                                format!(
                                    "command registration limit \
                                     ({REGISTRATION_MAX_COMMANDS}) exceeded"
                                ),
                            )
                            .to_error(ctx));
                        }
                        capture.commands.push(CommandRegistration {
                            id,
                            title,
                            description,
                            run,
                        });
                    }
                    stack.replace(ctx, ());
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("commands table accepts 'register'");

    let events = Table::new(&ctx);
    events
        .set(
            ctx,
            "subscribe",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let kind = match stack.get(0) {
                        Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "event subscription needs a string kind",
                            )
                            .to_error(ctx));
                        }
                    };
                    let handler = match stack.get(1) {
                        Value::Function(function) => ctx.stash(function),
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "event handler must be a function",
                            )
                            .to_error(ctx));
                        }
                    };
                    // HOST-002 admission: kind-length + subscription-count caps at
                    // the bridge, mirroring the manifest `lazy.events`
                    // bounds. The runtime validator additionally rejects
                    // undeclared kinds and duplicate subscriptions.
                    if kind.is_empty() || kind.len() > REGISTRATION_MAX_EVENT_KIND_BYTES {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            format!(
                                "event kind must be 1..={REGISTRATION_MAX_EVENT_KIND_BYTES} bytes"
                            ),
                        )
                        .to_error(ctx));
                    }
                    {
                        let mut capture = state.capture.borrow_mut();
                        if capture.events.len() >= REGISTRATION_MAX_EVENTS {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_LIMIT",
                                format!(
                                    "event subscription limit \
                                     ({REGISTRATION_MAX_EVENTS}) exceeded"
                                ),
                            )
                            .to_error(ctx));
                        }
                        capture.events.push(EventSubscription { kind, handler });
                    }
                    stack.replace(ctx, ());
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("events table accepts 'subscribe'");

    let settings = Table::new(&ctx);
    settings
        .set(
            ctx,
            "get",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let key = match stack.get(0) {
                        Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_SETTINGS_KEY_INVALID",
                                "settings key must be a string",
                            )
                            .to_error(ctx));
                        }
                    };
                    let value = state
                        .bounded(|_expiry| state.services.settings_get(&key))
                        .map_err(|e| e.to_error(ctx))?;
                    match value {
                        Some(value) => stack.replace(ctx, value.to_lua(ctx)),
                        None => stack.replace(ctx, Value::Nil),
                    }
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("settings table accepts 'get'");

    let store = Table::new(&ctx);
    store
        .set(
            ctx,
            "get",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let key = match stack.get(0) {
                        Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_STORE_KEY_INVALID",
                                "store key must be a string",
                            )
                            .to_error(ctx));
                        }
                    };
                    let value = state
                        .bounded(|_expiry| state.services.store_get(&key))
                        .map_err(|e| e.to_error(ctx))?;
                    match value {
                        Some(value) => stack.replace(ctx, value.to_lua(ctx)),
                        None => stack.replace(ctx, Value::Nil),
                    }
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("store table accepts 'get'");
    store
        .set(
            ctx,
            "set",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let key = match stack.get(0) {
                        Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_STORE_KEY_INVALID",
                                "store key must be a string",
                            )
                            .to_error(ctx));
                        }
                    };
                    let raw = stack.get(1);
                    let value =
                        LuaValue::from_lua(raw, state.limits).map_err(|e| e.to_error(ctx))?;
                    state
                        .bounded_store_commit(|expiry| {
                            state.services.store_set_with_expiry(&key, value, expiry)
                        })
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Boolean(true));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("store table accepts 'set'");

    let terminal = Table::new(&ctx);
    terminal
        .set(
            ctx,
            "snapshot",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let opts = stack.get(0);
                    let scope = match opts {
                        Value::Table(table) => match table.get_value(ctx, "scope".to_string()) {
                            Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                            _ => {
                                return Err(BridgeError::new(
                                    "validation",
                                    "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                                    "terminal.snapshot requires a string scope",
                                )
                                .to_error(ctx));
                            }
                        },
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                                "terminal.snapshot requires an options table",
                            )
                            .to_error(ctx));
                        }
                    };
                    let snapshot = state
                        .bounded(|_expiry| state.services.terminal_snapshot(&scope))
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, snapshot.to_lua(ctx));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("terminal table accepts 'snapshot'");

    let notify = Table::new(&ctx);
    notify
        .set(
            ctx,
            "show",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let payload = stack.get(0);
                    let value =
                        LuaValue::from_lua(payload, state.limits).map_err(|e| e.to_error(ctx))?;
                    let accepted = state
                        .bounded_mutation(|expiry| {
                            state.services.notify_show_with_expiry(&value, expiry)
                        })
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Boolean(accepted));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("notify table accepts 'show'");

    let process = Table::new(&ctx);
    // CTX-0707 ruling: `bitty.process.spawn` is v1-OUT. The accepted v1
    // exclusion list bars ambient process authority, so this consent-gated
    // spawn extra is NOT part of the Plugin API v1 guarantee: it ships for
    // first-party needs (CTX-0445), carries no `api_version` stability
    // promise, and may change outside minor-version rules. The bridge keeps
    // serving it with the same typed diagnostics; SDK conformance must not
    // assert it as v1 surface.
    process
        .set(
            ctx,
            "spawn",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let raw = stack.get(0);
                    let value =
                        LuaValue::from_lua(raw, state.limits).map_err(|e| e.to_error(ctx))?;
                    let args = spawn_argv(&value).map_err(|e| e.to_error(ctx))?;
                    let result = state
                        .bounded_spawn(|expiry| {
                            state.services.process_spawn_with_expiry(&args, expiry)
                        })
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, result.to_lua(ctx));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("process table accepts 'spawn'");

    let timers = Table::new(&ctx);
    timers
        .set(
            ctx,
            "create",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let delay_ms = match stack.get(0) {
                        Value::Integer(i) if i >= 0 => i as u64,
                        Value::Number(n) if n >= 0.0 && n.is_finite() => n as u64,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "timer delay must be a non-negative number",
                            )
                            .to_error(ctx));
                        }
                    };
                    let callback = match stack.get(1) {
                        Value::Function(function) => ctx.stash(function),
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "timer callback must be a function",
                            )
                            .to_error(ctx));
                        }
                    };
                    // HOST-002 admission: delay + count caps at the bridge,
                    // checked handle allocation (no wrapping increment). The
                    // runtime validator re-checks the same bounds so a
                    // hand-built capture cannot bypass them.
                    if delay_ms > REGISTRATION_MAX_TIMER_DELAY_MS {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            format!(
                                "timer delay exceeds \
                                 {REGISTRATION_MAX_TIMER_DELAY_MS} ms"
                            ),
                        )
                        .to_error(ctx));
                    }
                    let handle = {
                        let mut capture = state.capture.borrow_mut();
                        if capture.timers.len() >= REGISTRATION_MAX_TIMERS {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_LIMIT",
                                format!("timer limit ({REGISTRATION_MAX_TIMERS}) exceeded"),
                            )
                            .to_error(ctx));
                        }
                        let Some(handle) = capture.alloc_timer_handle() else {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_LIMIT",
                                "timer handle space exhausted",
                            )
                            .to_error(ctx));
                        };
                        capture.timers.push(TimerRegistration {
                            handle,
                            delay_ms,
                            callback,
                        });
                        handle
                    };
                    stack.replace(ctx, Value::Integer(handle));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("timers table accepts 'create'");
    timers
        .set(
            ctx,
            "cancel",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let handle = match stack.get(0) {
                        Value::Integer(i) => i,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "timer handle must be an integer",
                            )
                            .to_error(ctx));
                        }
                    };
                    let removed = state.capture.borrow_mut().cancel_timer(handle);
                    stack.replace(ctx, Value::Boolean(removed));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("timers table accepts 'cancel'");

    // CTX-0707 parity: `bitty.keymaps.suggest` is WIRED as a bridge capture
    // (LUA-OQ-5). Suggestion-only and activation-scoped like
    // `commands.register`: the bridge checks shape (`chord`/`command`
    // non-empty bounded strings, `when` absent-or-`"global"`) and captures;
    // chord-grammar validation, precedence, and conflict diagnostics are
    // applied host-side after activation.
    let keymaps = Table::new(&ctx);
    keymaps
        .set(
            ctx,
            "suggest",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let def = match stack.get(0) {
                        Value::Table(table) => table,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "keymap suggestion must be a table",
                            )
                            .to_error(ctx));
                        }
                    };
                    let chord = required_string(ctx, def, "chord")?;
                    let command = required_string(ctx, def, "command")?;
                    if chord.len() > REGISTRATION_MAX_KEYMAP_CHORD_BYTES {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            format!(
                                "keymap chord exceeds {REGISTRATION_MAX_KEYMAP_CHORD_BYTES} bytes"
                            ),
                        )
                        .to_error(ctx));
                    }
                    if command.len() > REGISTRATION_MAX_KEYMAP_COMMAND_BYTES {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            format!(
                                "keymap command exceeds \
                                 {REGISTRATION_MAX_KEYMAP_COMMAND_BYTES} bytes"
                            ),
                        )
                        .to_error(ctx));
                    }
                    let when = match optional_string(ctx, def, "when") {
                        None => "global".to_string(),
                        Some(when) if when == "global" => when,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "keymap suggestion 'when' must be absent or \"global\" in v1",
                            )
                            .to_error(ctx));
                        }
                    };
                    let handle = {
                        let mut capture = state.capture.borrow_mut();
                        if capture.keymaps.len() >= REGISTRATION_MAX_KEYMAP_SUGGESTIONS {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_LIMIT",
                                format!(
                                    "keymap suggestion limit \
                                     ({REGISTRATION_MAX_KEYMAP_SUGGESTIONS}) exceeded"
                                ),
                            )
                            .to_error(ctx));
                        }
                        capture.keymaps.push(KeymapSuggestion {
                            chord,
                            command,
                            when,
                        });
                        capture.keymaps.len() as i64
                    };
                    stack.replace(ctx, Value::Integer(handle));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("keymaps table accepts 'suggest'");

    // `bitty.services.get`/`provide` are WIRED to the host backend
    // (LUA-OQ-8). The bridge validates shapes and owns the generation
    // capture; policy lives host-side behind `HostServices`
    // (`service_provide_check` / `service_resolve` / `service_call`).
    // Hosts without a backend keep the typed `E_NOT_IMPLEMENTED` default,
    // so the spellings stay present and misconfiguration is observable.
    let services = Table::new(&ctx);
    services
        .set(
            ctx,
            "get",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let iface = match stack.get(0) {
                        Value::String(name) => {
                            String::from_utf8_lossy(name.as_bytes()).into_owned()
                        }
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "service interface must be a string",
                            )
                            .to_error(ctx));
                        }
                    };
                    if let Err(detail) = check_service_iface_shape(&iface) {
                        return Err(
                            BridgeError::new("validation", "E_DEF_INVALID", detail).to_error(ctx)
                        );
                    }
                    let (req, optional) = match stack.get(1) {
                        Value::Nil => (None, false),
                        Value::Table(opts) => match read_service_opts(opts) {
                            Ok(parsed) => parsed,
                            Err(detail) => {
                                return Err(BridgeError::new(
                                    "validation",
                                    "E_DEF_INVALID",
                                    detail,
                                )
                                .to_error(ctx));
                            }
                        },
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "service options must be a table",
                            )
                            .to_error(ctx));
                        }
                    };
                    let route = state
                        .bounded(|_expiry| {
                            state
                                .services
                                .service_resolve(&iface, req.as_deref(), optional)
                        })
                        .map_err(|error| error.to_error(ctx))?;
                    let Some(route) = route else {
                        stack.replace(ctx, Value::Nil);
                        return Ok(CallbackReturn::Return);
                    };
                    let table = Table::new(&ctx);
                    for method in &route.methods {
                        let state = state.clone();
                        let provider = route.provider.clone();
                        let generation = route.generation;
                        let iface = route.iface.clone();
                        let name = method.clone();
                        let method = method.clone();
                        table
                            .set(
                                ctx,
                                name,
                                Callback::from_fn(&ctx, move |ctx, _exec, mut stack| {
                                    let arg = match stack.get(0) {
                                        Value::Nil => LuaValue::Nil,
                                        value => LuaValue::from_lua(value, state.limits)
                                            .map_err(|error| error.to_error(ctx))?,
                                    };
                                    let result = state
                                        .bounded(|_expiry| {
                                            state.services.service_call(
                                                &provider, generation, &iface, &method, &arg,
                                            )
                                        })
                                        .map_err(|error| error.to_error(ctx))?;
                                    stack.replace(ctx, result.to_lua(ctx));
                                    Ok(CallbackReturn::Return)
                                }),
                            )
                            .map_err(|error| {
                                BridgeError::new(
                                    "runtime",
                                    "E_SERVICE_FAILED",
                                    format!("failed to build service handle: {error:?}"),
                                )
                                .to_error(ctx)
                            })?;
                    }
                    stack.replace(ctx, Value::Table(table));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("services table accepts 'get'");
    services
        .set(
            ctx,
            "provide",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let iface = match stack.get(0) {
                        Value::String(name) => {
                            String::from_utf8_lossy(name.as_bytes()).into_owned()
                        }
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "service interface must be a string",
                            )
                            .to_error(ctx));
                        }
                    };
                    if let Err(detail) = check_service_iface_shape(&iface) {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            detail,
                        )
                        .to_error(ctx));
                    }
                    let impl_table = match stack.get(1) {
                        Value::Table(table) => table,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "service impl must be a table",
                            )
                            .to_error(ctx));
                        }
                    };
                    // Host gate before the impl scan: hosts without a
                    // backend fail closed with `E_NOT_IMPLEMENTED` here,
                    // while the runtime checks the caller manifest.
                    state
                        .bounded(|_expiry| state.services.service_provide_check(&iface))
                        .map_err(|error| error.to_error(ctx))?;
                    let mut methods = Vec::new();
                    for (key, value) in impl_table.iter() {
                        if methods.len() >= REGISTRATION_MAX_SERVICE_METHODS {
                            return Err(BridgeError::new(
                                "budget",
                                "E_DEF_LIMIT",
                                format!(
                                    "service method limit ({REGISTRATION_MAX_SERVICE_METHODS}) exceeded"
                                ),
                            )
                            .to_error(ctx));
                        }
                        let Value::String(raw) = key else {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "service method names must be strings",
                            )
                            .to_error(ctx));
                        };
                        let name: String =
                            String::from_utf8_lossy(raw.as_bytes()).into_owned();
                        if name.is_empty()
                            || name.len() > SERVICE_MAX_METHOD_BYTES
                            || name.contains('\0')
                        {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "service method name must be 1..128 bytes without NUL",
                            )
                            .to_error(ctx));
                        }
                        if methods.iter().any(|m: &ServiceMethod| m.name == name) {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                format!("duplicate service method '{name}'"),
                            )
                            .to_error(ctx));
                        }
                        let Value::Function(function) = value else {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "service impl entries must be functions",
                            )
                            .to_error(ctx));
                        };
                        methods.push(ServiceMethod {
                            name,
                            func: ctx.stash(function),
                        });
                    }
                    if methods.is_empty() {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            "service impl must declare at least one method",
                        )
                        .to_error(ctx));
                    }
                    {
                        let mut capture = state.capture.borrow_mut();
                        if capture.services.len() >= REGISTRATION_MAX_SERVICES {
                            return Err(BridgeError::new(
                                "budget",
                                "E_DEF_LIMIT",
                                format!(
                                    "service provision limit ({REGISTRATION_MAX_SERVICES}) exceeded"
                                ),
                            )
                            .to_error(ctx));
                        }
                        if capture.services.iter().any(|p| p.iface == iface) {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                format!("duplicate service provision '{iface}'"),
                            )
                            .to_error(ctx));
                        }
                        capture.services.push(ServiceProvision { iface, methods });
                    }
                    stack.replace(ctx, Value::Boolean(true));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("services table accepts 'provide'");

    // CTX-0707 parity: `bitty.tasks.spawn`/`cancel` are WIRED as a bridge
    // capture (LUA-OQ-9, RC-4). The bridge stashes the entry function, owns
    // the integer handle under the 64-live-task cap (`E_BUDGET_TASK` past
    // it), and releases the slot on cancel; scheduling, cooperative
    // cancellation, and resumption through the event path stay host-side,
    // mirroring `timers.create`/`cancel`.
    let tasks = Table::new(&ctx);
    tasks
        .set(
            ctx,
            "spawn",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let entry = match stack.get(0) {
                        Value::Function(function) => ctx.stash(function),
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "task entry must be a function",
                            )
                            .to_error(ctx));
                        }
                    };
                    let handle = {
                        let mut capture = state.capture.borrow_mut();
                        if capture.tasks.len() >= REGISTRATION_MAX_TASKS {
                            return Err(BridgeError::new(
                                "budget",
                                "E_BUDGET_TASK",
                                format!("task limit ({REGISTRATION_MAX_TASKS}) exceeded"),
                            )
                            .to_error(ctx));
                        }
                        let Some(handle) = capture.alloc_task_handle() else {
                            return Err(BridgeError::new(
                                "budget",
                                "E_BUDGET_TASK",
                                "task handle space exhausted",
                            )
                            .to_error(ctx));
                        };
                        capture.tasks.push(TaskRegistration { handle, entry });
                        handle
                    };
                    stack.replace(ctx, Value::Integer(handle));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("tasks table accepts 'spawn'");
    tasks
        .set(
            ctx,
            "cancel",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let handle = match stack.get(0) {
                        Value::Integer(i) => i,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "task handle must be an integer",
                            )
                            .to_error(ctx));
                        }
                    };
                    let removed = state.capture.borrow_mut().cancel_task(handle);
                    stack.replace(ctx, Value::Boolean(removed));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("tasks table accepts 'cancel'");

    // CTX-0330: `bitty.env.get`/`has` are grant-gated (ADR-0006). The bridge
    // validates the key shape, then delegates to the host services through
    // the deadline-checked read path. Hosts without an env backend — and
    // generations without an `env.read:<KEY>` grant — fail closed with typed
    // `E_NOT_IMPLEMENTED`, so ungranted keys stay indistinguishable from
    // unimplemented ones.
    let env = Table::new(&ctx);
    env.set(
        ctx,
        "get",
        Callback::from_fn(&ctx, {
            let state = state.clone();
            move |ctx, _exec, mut stack| {
                let key = match stack.get(0) {
                    Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                    _ => {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            "env.get key must be a string",
                        )
                        .to_error(ctx));
                    }
                };
                validate_env_key(&key).map_err(|e| e.to_error(ctx))?;
                let value = state
                    .bounded(|_expiry| state.services.env_get(&key))
                    .map_err(|e| e.to_error(ctx))?;
                match value {
                    Some(value) => stack.replace(ctx, value.to_lua(ctx)),
                    None => stack.replace(ctx, Value::Nil),
                }
                Ok(CallbackReturn::Return)
            }
        }),
    )
    .expect("env table accepts 'get'");
    env.set(
        ctx,
        "has",
        Callback::from_fn(&ctx, {
            let state = state.clone();
            move |ctx, _exec, mut stack| {
                let key = match stack.get(0) {
                    Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                    _ => {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            "env.has key must be a string",
                        )
                        .to_error(ctx));
                    }
                };
                validate_env_key(&key).map_err(|e| e.to_error(ctx))?;
                let present = state
                    .bounded(|_expiry| state.services.env_has(&key))
                    .map_err(|e| e.to_error(ctx))?;
                stack.replace(ctx, Value::Boolean(present));
                Ok(CallbackReturn::Return)
            }
        }),
    )
    .expect("env table accepts 'has'");

    // CTX-0984 (RFC-0005): `bitty.fs.*` Core-owned filesystem bridge beside
    // `terminal.*` (never under it). Decided verbs are `read`, `write`, and
    // `list`: `open` is rejected as a verb (a retained handle is a
    // subscription by another name), `append` is a write-disposition flag
    // (`{ append = bool }`, default create-or-overwrite), and `list` is
    // read-class under the read grant over the listed prefix. Paths validate
    // `1..=4096` bytes with `E_DEF_INVALID`/`E_DEF_LIMIT` before the grant
    // gate; scope, sensitive-path, secret, bound, budget, safe-mode, and
    // trust denials are typed `E_FS_*` (oracle-tight). Results are
    // read-into-VM-only with the Core-attached untrusted label
    // (`untrusted = true`); combining them with clipboard, process, IPC, or
    // network authority needs a separately granted authority with argv-first
    // invocation and no shell-string construction. There is no watch,
    // subscription, tail-follow, retained handle, or cross-call cursor in
    // this family. Hosts without an fs backend fail closed with typed
    // `E_NOT_IMPLEMENTED`, so the spellings stay present and misconfiguration
    // is observable.
    let fs = Table::new(&ctx);
    fs.set(
        ctx,
        "read",
        Callback::from_fn(&ctx, {
            let state = state.clone();
            move |ctx, _exec, mut stack| {
                let path =
                    fs_text_arg(stack.get(0), "fs.read path").map_err(|e| e.to_error(ctx))?;
                validate_fs_path(&path).map_err(|e| e.to_error(ctx))?;
                let result = state
                    .bounded(|_expiry| state.services.fs_read(&path))
                    .map_err(|e| e.to_error(ctx))?;
                stack.replace(ctx, result.to_lua(ctx));
                Ok(CallbackReturn::Return)
            }
        }),
    )
    .expect("fs table accepts 'read'");
    fs.set(
        ctx,
        "write",
        Callback::from_fn(&ctx, {
            let state = state.clone();
            move |ctx, _exec, mut stack| {
                let path =
                    fs_text_arg(stack.get(0), "fs.write path").map_err(|e| e.to_error(ctx))?;
                let content =
                    fs_text_arg(stack.get(1), "fs.write content").map_err(|e| e.to_error(ctx))?;
                validate_fs_path(&path).map_err(|e| e.to_error(ctx))?;
                validate_fs_content(&content).map_err(|e| e.to_error(ctx))?;
                let raw_opts = stack.get(2);
                let opts =
                    LuaValue::from_lua(raw_opts, state.limits).map_err(|e| e.to_error(ctx))?;
                let append = parse_fs_write_append(&opts).map_err(|e| e.to_error(ctx))?;
                let result = state
                    .bounded_mutation(|expiry| {
                        state
                            .services
                            .fs_write_with_expiry(&path, &content, append, expiry)
                    })
                    .map_err(|e| e.to_error(ctx))?;
                stack.replace(ctx, result.to_lua(ctx));
                Ok(CallbackReturn::Return)
            }
        }),
    )
    .expect("fs table accepts 'write'");
    fs.set(
        ctx,
        "list",
        Callback::from_fn(&ctx, {
            let state = state.clone();
            move |ctx, _exec, mut stack| {
                let prefix =
                    fs_text_arg(stack.get(0), "fs.list prefix").map_err(|e| e.to_error(ctx))?;
                validate_fs_path(&prefix).map_err(|e| e.to_error(ctx))?;
                let max_entries = match stack.get(1) {
                    Value::Nil => FS_LIST_MAX_ENTRIES,
                    Value::Integer(n) if n >= 1 => usize::try_from(n).unwrap_or(usize::MAX),
                    _ => {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            "fs.list max_entries must be a positive integer",
                        )
                        .to_error(ctx));
                    }
                };
                validate_fs_list_max(max_entries).map_err(|e| e.to_error(ctx))?;
                let result = state
                    .bounded(|_expiry| state.services.fs_list(&prefix, max_entries))
                    .map_err(|e| e.to_error(ctx))?;
                stack.replace(ctx, result.to_lua(ctx));
                Ok(CallbackReturn::Return)
            }
        }),
    )
    .expect("fs table accepts 'list'");

    // CTX-0894/CTX-0897: `bitty.debug.*` namespace for devtools plugins.
    // Default host implementations return E_NOT_IMPLEMENTED; the runtime
    // gates each entry point on its own grant: `debug.inspect` (read-only
    // state inspection), `debug.trace` (`trace`/`trace_get`), `debug.control`
    // (high-risk reload/suspend, requires explicit consent). `inspect` is a
    // read (`bounded`); `trace` and `control` mutate host state and go
    // through `bounded_mutation` with the pre-commit expiry; `trace_get`
    // drains its buffer, so it also uses `bounded_mutation`.
    let debug = Table::new(&ctx);
    debug
        .set(
            ctx,
            "inspect",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let target = match stack.get(0) {
                        Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "debug.inspect target must be a string",
                            )
                            .to_error(ctx));
                        }
                    };
                    let result = state
                        .bounded(|_expiry| state.services.debug_inspect(&target))
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, result.to_lua(ctx));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("debug table accepts 'inspect'");
    debug
        .set(
            ctx,
            "trace",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    // `nil` opens a trace with defaults; any non-table value
                    // is rejected before reaching the host.
                    let raw = stack.get(0);
                    if !matches!(raw, Value::Nil | Value::Table(_)) {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            "debug.trace opts must be a table or nil",
                        )
                        .to_error(ctx));
                    }
                    let opts =
                        LuaValue::from_lua(raw, state.limits).map_err(|e| e.to_error(ctx))?;
                    let handle = state
                        .bounded_mutation(|expiry| {
                            state.services.debug_trace_with_expiry(&opts, expiry)
                        })
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Integer(handle));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("debug table accepts 'trace'");
    debug
        .set(
            ctx,
            "trace_get",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let handle = match stack.get(0) {
                        Value::Integer(h) => h,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "debug.trace_get handle must be an integer",
                            )
                            .to_error(ctx));
                        }
                    };
                    // Draining empties the buffer, so a post-call timeout
                    // would silently discard records: use the mutation guard
                    // (pre-call deadline only), like the other commits.
                    let result = state
                        .bounded_mutation(|_expiry| state.services.debug_trace_get(handle))
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, result.to_lua(ctx));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("debug table accepts 'trace_get'");
    debug
        .set(
            ctx,
            "control",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let action = match stack.get(0) {
                        Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "debug.control action must be a string",
                            )
                            .to_error(ctx));
                        }
                    };
                    let target = match stack.get(1) {
                        Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "debug.control target must be a string",
                            )
                            .to_error(ctx));
                        }
                    };
                    let result = state
                        .bounded_mutation(|expiry| {
                            state
                                .services
                                .debug_control_with_expiry(&action, &target, expiry)
                        })
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, result.to_lua(ctx));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("debug table accepts 'control'");

    // CTX-0915: `bitty.panel` API for panel lifecycle and presentation control
    // (Issue #1596). Grant-gated on `panel.create` (creation) and `panel.focus`
    // (manipulation/queries). Backend wiring to PanelRegistry is deferred.
    let panel = Table::new(&ctx);
    panel
        .set(
            ctx,
            "create",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let args = match stack.get(0) {
                        Value::Table(t) => t,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "panel.create requires a table argument { type = \"...\" }",
                            )
                            .to_error(ctx));
                        }
                    };
                    let panel_type = match args.get(ctx, "type") {
                        Ok(Value::String(s)) => {
                            let type_str = String::from_utf8_lossy(s.as_bytes()).into_owned();
                            // Validate against documented panel types
                            match type_str.as_str() {
                                "terminal" | "rich" | "browser" | "helper" | "canvas" => type_str,
                                _ => {
                                    return Err(BridgeError::new(
                                        "validation",
                                        "E_DEF_INVALID",
                                        "panel.create type must be terminal, rich, browser, helper, or canvas",
                                    )
                                    .to_error(ctx));
                                }
                            }
                        }
                        Ok(_) => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "panel.create type field must be a string",
                            )
                            .to_error(ctx));
                        }
                        Err(_) => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "panel.create requires a 'type' field in the argument table",
                            )
                            .to_error(ctx));
                        }
                    };
                    let (id, generation) = state
                        .bounded_mutation(|_expiry| state.services.panel_create(&panel_type))
                        .map_err(|e| e.to_error(ctx))?;
                    let result = Table::new(&ctx);
                    result.set(ctx, "id", id as i64).expect("result accepts id");
                    result
                        .set(ctx, "generation", generation as i64)
                        .expect("result accepts generation");
                    stack.replace(ctx, Value::Table(result));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("panel table accepts 'create'");
    panel
        .set(
            ctx,
            "close",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let panel_id = match stack.get(0) {
                        Value::Integer(id) => id as u64,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "panel.close id must be an integer",
                            )
                            .to_error(ctx));
                        }
                    };
                    let closed = state
                        .bounded_mutation(|_expiry| state.services.panel_close(panel_id))
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Boolean(closed));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("panel table accepts 'close'");
    panel
        .set(
            ctx,
            "destroy",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let panel_id = match stack.get(0) {
                        Value::Integer(id) => id as u64,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "panel.destroy id must be an integer",
                            )
                            .to_error(ctx));
                        }
                    };
                    let destroyed = state
                        .bounded_mutation(|_expiry| state.services.panel_destroy(panel_id))
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Boolean(destroyed));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("panel table accepts 'destroy'");
    panel
        .set(
            ctx,
            "get_presentation",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let panel_id = match stack.get(0) {
                        Value::Integer(id) => id as u64,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "panel.get_presentation id must be an integer",
                            )
                            .to_error(ctx));
                        }
                    };
                    let presentation = state
                        .bounded(|_expiry| state.services.panel_get_presentation(panel_id))
                        .map_err(|e| e.to_error(ctx))?;
                    match presentation {
                        Some(mode) => {
                            stack.replace(ctx, Value::String(ctx.intern(mode.as_bytes())));
                        }
                        None => stack.replace(ctx, Value::Nil),
                    }
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("panel table accepts 'get_presentation'");
    panel
        .set(
            ctx,
            "set_presentation",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let panel_id = match stack.get(0) {
                        Value::Integer(id) => id as u64,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "panel.set_presentation id must be an integer",
                            )
                            .to_error(ctx));
                        }
                    };
                    let presentation = match stack.get(1) {
                        Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "panel.set_presentation mode must be a string",
                            )
                            .to_error(ctx));
                        }
                    };
                    let set = state
                        .bounded_mutation(|_expiry| {
                            state
                                .services
                                .panel_set_presentation(panel_id, &presentation)
                        })
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Boolean(set));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("panel table accepts 'set_presentation'");
    panel
        .set(
            ctx,
            "toggle_floating",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let panel_id = match stack.get(0) {
                        Value::Integer(id) => id as u64,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "panel.toggle_floating id must be an integer",
                            )
                            .to_error(ctx));
                        }
                    };
                    let toggled = state
                        .bounded_mutation(|_expiry| state.services.panel_toggle_floating(panel_id))
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Boolean(toggled));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("panel table accepts 'toggle_floating'");
    panel
        .set(
            ctx,
            "get_state",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let panel_id = match stack.get(0) {
                        Value::Integer(id) => id as u64,
                        _ => {
                            return Err(BridgeError::new(
                                "validation",
                                "E_DEF_INVALID",
                                "panel.get_state id must be an integer",
                            )
                            .to_error(ctx));
                        }
                    };
                    let panel_state = state
                        .bounded(|_expiry| state.services.panel_get_state(panel_id))
                        .map_err(|e| e.to_error(ctx))?;
                    match panel_state {
                        Some(state_value) => stack.replace(ctx, state_value.to_lua(ctx)),
                        None => stack.replace(ctx, Value::Nil),
                    }
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("panel table accepts 'get_state'");

    // CTX-0889 (ADR-0014): `bitty.workspace.*` L1 domain. `list` is a
    // `workspace.read` read (`bounded`); every mutation is gated on
    // `workspace.control` and only enqueues a bounded request (mutation
    // guard) that the application applies on its next tick through the
    // keybinding handlers. Spellings are candidates pending OQ-056.
    let workspace = Table::new(&ctx);
    workspace
        .set(
            ctx,
            "list",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let rows = state
                        .bounded(|_expiry| state.services.workspace_list())
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, workspace_list_value(&rows).to_lua(ctx));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("workspace table accepts 'list'");
    workspace
        .set(
            ctx,
            "focus",
            workspace_mutation(ctx, state, |ctx, args| workspace_focus_arg(ctx, args[0])),
        )
        .expect("workspace table accepts 'focus'");
    workspace
        .set(
            ctx,
            "new",
            workspace_mutation(ctx, state, |_ctx, _args| Ok(WorkspaceRequest::New)),
        )
        .expect("workspace table accepts 'new'");
    workspace
        .set(
            ctx,
            "next",
            workspace_mutation(ctx, state, |_ctx, _args| Ok(WorkspaceRequest::Next)),
        )
        .expect("workspace table accepts 'next'");
    workspace
        .set(
            ctx,
            "close",
            workspace_mutation(ctx, state, |_ctx, args| match args[0] {
                Value::Nil => Ok(WorkspaceRequest::Close(None)),
                other => workspace_id_arg(other, "workspace.close id")
                    .map(|id| WorkspaceRequest::Close(Some(id))),
            }),
        )
        .expect("workspace table accepts 'close'");
    workspace
        .set(
            ctx,
            "rename",
            workspace_mutation(ctx, state, |_ctx, args| {
                let id = workspace_id_arg(args[0], "workspace.rename id")?;
                let name = workspace_name_arg(args[1])?;
                Ok(WorkspaceRequest::Rename { id, name })
            }),
        )
        .expect("workspace table accepts 'rename'");
    workspace
        .set(
            ctx,
            "move_panel",
            workspace_mutation(ctx, state, |_ctx, args| {
                workspace_id_arg(args[0], "workspace.move_panel target")
                    .map(WorkspaceRequest::MovePanel)
            }),
        )
        .expect("workspace table accepts 'move_panel'");

    let ui = Table::new(&ctx);
    ui.set(
        ctx,
        "mount",
        Callback::from_fn(&ctx, {
            let state = state.clone();
            move |ctx, _exec, mut stack| {
                let slot = match stack.get(0) {
                    Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                    _ => {
                        return Err(
                            component_invalid("ui.mount slot must be a string").to_error(ctx)
                        );
                    }
                };
                if !is_ui_slot(&slot) {
                    return Err(component_invalid(format!(
                        "unknown UI slot '{}'",
                        bounded_token(&slot)
                    ))
                    .to_error(ctx));
                }
                let component = read_component(stack.get(1)).map_err(|e| e.to_error(ctx))?;
                let handle = state
                    .bounded_mutation(|expiry| {
                        state
                            .services
                            .ui_mount_with_expiry(&slot, &component, expiry)
                    })
                    .map_err(|e| e.to_error(ctx))?;
                stack.replace(ctx, Value::Integer(handle));
                Ok(CallbackReturn::Return)
            }
        }),
    )
    .expect("ui table accepts 'mount'");
    ui.set(
        ctx,
        "update",
        Callback::from_fn(&ctx, {
            let state = state.clone();
            move |ctx, _exec, mut stack| {
                let handle = match stack.get(0) {
                    Value::Integer(handle) => handle,
                    _ => {
                        return Err(component_invalid(
                            "ui.update handle must be an integer block handle",
                        )
                        .to_error(ctx));
                    }
                };
                let component = read_component(stack.get(1)).map_err(|e| e.to_error(ctx))?;
                let updated = state
                    .bounded_mutation(|expiry| {
                        state
                            .services
                            .ui_update_with_expiry(handle, &component, expiry)
                    })
                    .map_err(|e| e.to_error(ctx))?;
                stack.replace(ctx, Value::Boolean(updated));
                Ok(CallbackReturn::Return)
            }
        }),
    )
    .expect("ui table accepts 'update'");

    // CTX-0941 (accepted W-01 host contract, v2 scope of OQ-056):
    // Core-owned focusable-overlay transient input capture under the decided
    // `bitty.ui.overlay.*` spellings. The surface gates on the v2
    // `ui.overlay.focus` capability (deny-by-default, naming the capability)
    // and fails closed with `E_UI_UNAVAILABLE` on a host without a capture
    // backend or in safe mode. No plugin callback runs on the input path:
    // Core queues captured input and the owner reads it through `poll`.
    let overlay = Table::new(&ctx);
    overlay
        .set(
            ctx,
            "acquire",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    match stack.get(0) {
                        Value::Integer(handle) => {
                            // Mechanism path: claim capture for a block this
                            // generation mounted into the `overlay` slot.
                            state
                                .bounded_mutation(|expiry| {
                                    state
                                        .services
                                        .ui_overlay_acquire_with_expiry(handle, expiry)
                                })
                                .map_err(|e| e.to_error(ctx))?;
                            stack.replace(ctx, Value::Integer(handle));
                            Ok(CallbackReturn::Return)
                        }
                        Value::Nil | Value::Table(_) => {
                            // Accepted path: presentation-hint spec; Core
                            // mounts the surface and starts capture in one
                            // Core-owned switch and returns the session
                            // handle. Unknown fields are ignored.
                            let (title, placeholder) = parse_overlay_spec(stack.get(0))
                                .map_err(|e| e.to_error(ctx))?;
                            let handle = state
                                .bounded_mutation(|expiry| {
                                    state.services.ui_overlay_acquire_with_spec_and_expiry(
                                        &title,
                                        &placeholder,
                                        expiry,
                                    )
                                })
                                .map_err(|e| e.to_error(ctx))?;
                            stack.replace(ctx, Value::Integer(handle));
                            Ok(CallbackReturn::Return)
                        }
                        _ => Err(component_invalid(
                            "ui.overlay.acquire expects a mounted overlay block handle or a spec table",
                        )
                        .to_error(ctx)),
                    }
                }
            }),
        )
        .expect("overlay table accepts 'acquire'");
    overlay
        .set(
            ctx,
            "update",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let handle = match stack.get(0) {
                        Value::Integer(handle) => handle,
                        _ => {
                            return Err(component_invalid(
                                "ui.overlay.update handle must be an integer block handle",
                            )
                            .to_error(ctx));
                        }
                    };
                    let component = read_component(stack.get(1)).map_err(|e| e.to_error(ctx))?;
                    let updated = state
                        .bounded_mutation(|expiry| {
                            state
                                .services
                                .ui_overlay_update_with_expiry(handle, &component, expiry)
                        })
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Boolean(updated));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("overlay table accepts 'update'");
    overlay
        .set(
            ctx,
            "release",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let handle = match stack.get(0) {
                        Value::Integer(handle) => handle,
                        _ => {
                            return Err(component_invalid(
                                "ui.overlay.release handle must be an integer block handle",
                            )
                            .to_error(ctx));
                        }
                    };
                    let reason = match stack.get(1) {
                        Value::Nil => None,
                        Value::String(s) => {
                            Some(String::from_utf8_lossy(s.as_bytes()).into_owned())
                        }
                        _ => {
                            return Err(component_invalid(
                                "ui.overlay.release reason must be 'submitted' or 'cancelled'",
                            )
                            .to_error(ctx));
                        }
                    };
                    let released = state
                        .bounded_mutation(|_expiry| {
                            state
                                .services
                                .ui_overlay_release_with_reason(handle, reason.as_deref())
                        })
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Boolean(released));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("overlay table accepts 'release'");
    overlay
        .set(
            ctx,
            "poll",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let handle = match stack.get(0) {
                        Value::Integer(handle) => handle,
                        _ => {
                            return Err(component_invalid(
                                "ui.overlay.poll handle must be an integer block handle",
                            )
                            .to_error(ctx));
                        }
                    };
                    let max = match stack.get(1) {
                        Value::Nil => OVERLAY_CAPTURE_POLL_MAX,
                        Value::Integer(max) if max >= 1 => usize::try_from(max)
                            .unwrap_or(usize::MAX)
                            .min(OVERLAY_CAPTURE_POLL_MAX),
                        _ => {
                            return Err(component_invalid(
                                "ui.overlay.poll max must be a positive integer",
                            )
                            .to_error(ctx));
                        }
                    };
                    let poll = state
                        .bounded_mutation(|_expiry| {
                            state.services.ui_overlay_poll_detailed(handle, max)
                        })
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, overlay_poll_value(&poll).to_lua(ctx));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("overlay table accepts 'poll'");
    ui.set(ctx, "overlay", readonly_table(ctx, overlay))
        .expect("ui table accepts 'overlay'");

    // W-29 (CTX-0942, DEC-0085 thin host): read-only targeting mechanism over
    // the existing `bitty-ui` targeting types, exposed under the existing
    // `bitty.ui` namespace (no `bitty.beacon.*` namespace, no new capability,
    // no new error codes). All calls gate on the accepted `ui.overlay`
    // identifier (deny-by-default) because a targeting session consumes the
    // W-28 focusable overlay / transient-input-capture mechanism. No target
    // or annotation internal is published to the Event Bus, and no plugin
    // callback runs on the input hot path: labels resolve through the Core
    // dispatcher only. Session ownership is the existing overlay capture
    // owner (no new session type); dispatch returns a typed command id for
    // the accepted registry (no new privileged path). Safe mode is
    // unaffected: with zero plugins the Core provider yields an empty
    // snapshot and every call fails closed typed.
    let targets = Table::new(&ctx);
    targets
        .set(
            ctx,
            "snapshot",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let max = match stack.get(0) {
                        Value::Nil => UI_TARGETS_SNAPSHOT_MAX,
                        Value::Integer(max) if max >= 0 => usize::try_from(max)
                            .unwrap_or(usize::MAX)
                            .min(UI_TARGETS_SNAPSHOT_MAX),
                        _ => {
                            return Err(component_invalid(
                                "ui.targets.snapshot max must be a non-negative integer",
                            )
                            .to_error(ctx));
                        }
                    };
                    let entries = state
                        .bounded(|_expiry| state.services.ui_targets_snapshot(max))
                        .map_err(|e| e.to_error(ctx))?;
                    let items = entries
                        .into_iter()
                        .map(|(handle, kind, tier)| {
                            LuaValue::table([
                                ("handle", LuaValue::Integer(handle)),
                                ("kind", LuaValue::String(kind)),
                                ("tier", LuaValue::String(tier)),
                            ])
                        })
                        .collect();
                    stack.replace(ctx, LuaValue::array(items).to_lua(ctx));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("targets table accepts 'snapshot'");
    targets
        .set(
            ctx,
            "register",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let raw = stack.get(0);
                    let value =
                        LuaValue::from_lua(raw, state.limits).map_err(|e| e.to_error(ctx))?;
                    let (name, tier, offers) =
                        parse_targets_register(&value).map_err(|e| e.to_error(ctx))?;
                    let registered = state
                        .bounded_mutation(|_expiry| {
                            state.services.ui_targets_register(&name, &tier, &offers)
                        })
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Boolean(registered));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("targets table accepts 'register'");
    targets
        .set(
            ctx,
            "unregister",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let name = match stack.get(0) {
                        Value::String(raw) => String::from_utf8_lossy(raw.as_bytes()).into_owned(),
                        _ => {
                            return Err(component_invalid(
                                "ui.targets.unregister name must be a string",
                            )
                            .to_error(ctx));
                        }
                    };
                    let removed = state
                        .bounded_mutation(|_expiry| state.services.ui_targets_unregister(&name))
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Boolean(removed));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("targets table accepts 'unregister'");
    targets
        .set(
            ctx,
            "session_start",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let overlay_handle = match stack.get(0) {
                        Value::Integer(handle) if handle > 0 => handle,
                        _ => {
                            return Err(component_invalid(
                                "ui.targets.session_start handle must be a positive integer block handle",
                            )
                            .to_error(ctx));
                        }
                    };
                    let width = match stack.get(1) {
                        Value::Integer(width)
                            if (0..=i64::from(u16::MAX)).contains(&width) =>
                        {
                            width as u16
                        }
                        _ => {
                            return Err(component_invalid(
                                "ui.targets.session_start width must be an integer in 0..=65535",
                            )
                            .to_error(ctx));
                        }
                    };
                    let anchors_raw = stack.get(2);
                    let anchors_value =
                        LuaValue::from_lua(anchors_raw, state.limits).map_err(|e| e.to_error(ctx))?;
                    let anchors =
                        parse_targets_anchors(&anchors_value).map_err(|e| e.to_error(ctx))?;
                    let commands_raw = stack.get(3);
                    let commands_value =
                        LuaValue::from_lua(commands_raw, state.limits).map_err(|e| e.to_error(ctx))?;
                    let commands =
                        parse_targets_commands(&commands_value).map_err(|e| e.to_error(ctx))?;
                    if anchors.len() != commands.len() {
                        return Err(BridgeError::new(
                            "validation",
                            "E_DEF_INVALID",
                            "ui.targets.session_start anchors and commands must agree in length",
                        )
                        .to_error(ctx));
                    }
                    let labels = state
                        .bounded_mutation(|_expiry| {
                            state.services.ui_targets_session_start(
                                overlay_handle,
                                width,
                                &anchors,
                                &commands,
                            )
                        })
                        .map_err(|e| e.to_error(ctx))?;
                    let items = labels.into_iter().map(LuaValue::String).collect();
                    stack.replace(ctx, LuaValue::array(items).to_lua(ctx));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("targets table accepts 'session_start'");
    targets
        .set(
            ctx,
            "session_cancel",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let overlay_handle = match stack.get(0) {
                        Value::Integer(handle) if handle > 0 => handle,
                        _ => {
                            return Err(component_invalid(
                                "ui.targets.session_cancel handle must be a positive integer block handle",
                            )
                            .to_error(ctx));
                        }
                    };
                    let cancelled = state
                        .bounded_mutation(|_expiry| {
                            state.services.ui_targets_session_cancel(overlay_handle)
                        })
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Boolean(cancelled));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("targets table accepts 'session_cancel'");
    targets
        .set(
            ctx,
            "dispatch",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let label = match stack.get(0) {
                        Value::String(raw) => String::from_utf8_lossy(raw.as_bytes()).into_owned(),
                        _ => {
                            return Err(component_invalid(
                                "ui.targets.dispatch label must be a string",
                            )
                            .to_error(ctx));
                        }
                    };
                    if label.is_empty() {
                        return Err(component_invalid(
                            "ui.targets.dispatch label must be non-empty",
                        )
                        .to_error(ctx));
                    }
                    let command = state
                        .bounded(|_expiry| state.services.ui_targets_dispatch(&label))
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::String(ctx.intern(command.as_bytes())));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("targets table accepts 'dispatch'");
    ui.set(ctx, "targets", readonly_table(ctx, targets))
        .expect("ui table accepts 'targets'");

    let labels = Table::new(&ctx);
    labels
        .set(
            ctx,
            "set_policy",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let home = match stack.get(0) {
                        Value::String(raw) => String::from_utf8_lossy(raw.as_bytes()).into_owned(),
                        _ => {
                            return Err(component_invalid(
                                "ui.labels.set_policy home must be a string",
                            )
                            .to_error(ctx));
                        }
                    };
                    let overflow = match stack.get(1) {
                        Value::String(raw) => String::from_utf8_lossy(raw.as_bytes()).into_owned(),
                        _ => {
                            return Err(component_invalid(
                                "ui.labels.set_policy overflow must be a string",
                            )
                            .to_error(ctx));
                        }
                    };
                    state
                        .bounded_mutation(|_expiry| {
                            state.services.ui_labels_set_policy(&home, &overflow)
                        })
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Boolean(true));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("labels table accepts 'set_policy'");
    labels
        .set(
            ctx,
            "assign",
            Callback::from_fn(&ctx, {
                let state = state.clone();
                move |ctx, _exec, mut stack| {
                    let anchors_raw = stack.get(0);
                    let anchors_value = LuaValue::from_lua(anchors_raw, state.limits)
                        .map_err(|e| e.to_error(ctx))?;
                    let anchors =
                        parse_targets_anchors(&anchors_value).map_err(|e| e.to_error(ctx))?;
                    let width = match stack.get(1) {
                        Value::Integer(width) if (0..=i64::from(u16::MAX)).contains(&width) => {
                            width as u16
                        }
                        _ => {
                            return Err(component_invalid(
                                "ui.labels.assign width must be an integer in 0..=65535",
                            )
                            .to_error(ctx));
                        }
                    };
                    let assigned = state
                        .bounded(|_expiry| state.services.ui_labels_assign(&anchors, width))
                        .map_err(|e| e.to_error(ctx))?;
                    let items = assigned.into_iter().map(LuaValue::String).collect();
                    stack.replace(ctx, LuaValue::array(items).to_lua(ctx));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("labels table accepts 'assign'");
    ui.set(ctx, "labels", readonly_table(ctx, labels))
        .expect("ui table accepts 'labels'");

    let root = Table::new(&ctx);
    root.set(ctx, "api_version", API_VERSION)
        .expect("root accepts api_version");
    root.set(ctx, "commands", readonly_table(ctx, commands))
        .expect("root accepts commands");
    root.set(ctx, "events", readonly_table(ctx, events))
        .expect("root accepts events");
    root.set(ctx, "settings", readonly_table(ctx, settings))
        .expect("root accepts settings");
    root.set(ctx, "store", readonly_table(ctx, store))
        .expect("root accepts store");
    root.set(ctx, "terminal", readonly_table(ctx, terminal))
        .expect("root accepts terminal");
    root.set(ctx, "notify", readonly_table(ctx, notify))
        .expect("root accepts notify");
    root.set(ctx, "process", readonly_table(ctx, process))
        .expect("root accepts process");
    root.set(ctx, "ui", readonly_table(ctx, ui))
        .expect("root accepts ui");
    root.set(ctx, "timers", readonly_table(ctx, timers))
        .expect("root accepts timers");
    // CTX-0707 parity shape: all four accepted v1 namespaces are present.
    // `keymaps`/`tasks` capture at the bridge; `services` fails closed
    // with `E_NOT_IMPLEMENTED` until its host backend lands, while `env`
    // delegates to the grant-gated backend (CTX-0330: `E_NOT_IMPLEMENTED`
    // until an `env.read:<KEY>` grant exists).
    root.set(ctx, "keymaps", readonly_table(ctx, keymaps))
        .expect("root accepts keymaps");
    root.set(ctx, "services", readonly_table(ctx, services))
        .expect("root accepts services");
    root.set(ctx, "tasks", readonly_table(ctx, tasks))
        .expect("root accepts tasks");
    root.set(ctx, "env", readonly_table(ctx, env))
        .expect("root accepts env");
    root.set(ctx, "fs", readonly_table(ctx, fs))
        .expect("root accepts fs");
    root.set(ctx, "debug", readonly_table(ctx, debug))
        .expect("root accepts debug");
    root.set(ctx, "panel", readonly_table(ctx, panel))
        .expect("root accepts panel");
    root.set(ctx, "workspace", readonly_table(ctx, workspace))
        .expect("root accepts workspace");
    Value::Table(readonly_table(ctx, root))
}

impl LuaVm {
    /// Install the rooted, source-only `require` over the configured module root.
    ///
    /// The module root acts as the single VFS-style capability root: resolution
    /// canonicalizes under it and rejects traversal, non-`.lua` artifacts, and
    /// cross-tree fallback. The runtime builder installs core with an empty
    /// module configuration, so this injection is the only searcher — a VM
    /// without a module root keeps the preload-only `require`, which resolves
    /// nothing.
    fn install_require(&mut self) -> Result<(), VmError> {
        let Some(root) = self.directory_root() else {
            return Err(VmError::Load("module root not configured".into()));
        };
        let canonical = std::fs::canonicalize(&root)
            .map_err(|error| VmError::Load(format!("module root is not readable: {error}")))?;
        let compiled: Rc<RefCell<HashMap<String, StashedFunction>>> =
            Rc::new(RefCell::new(HashMap::new()));
        self.lua.enter(|ctx| {
            let cache = Table::new(&ctx);
            let cache_stashed = ctx.stash(cache);
            ctx.set_global(
                "require",
                Callback::from_fn(&ctx, {
                    let root = canonical.clone();
                    let cache = cache_stashed.clone();
                    let compiled = compiled.clone();
                    move |ctx, _exec, mut stack| {
                        let name = match stack.get(0) {
                            Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                            _ => {
                                return Err(BridgeError::new(
                                    "validation",
                                    "E_REQUIRE_NAME",
                                    "require expects a module name string",
                                )
                                .to_error(ctx));
                            }
                        };
                        validate_module_name(&name).map_err(|e| e.to_error(ctx))?;
                        let loaded: Table = ctx.fetch(&cache);
                        let cached = loaded.get_value(ctx, name.clone());
                        if !cached.is_nil() {
                            stack.replace(ctx, cached);
                            return Ok(CallbackReturn::Return);
                        }
                        let function = {
                            let existing = compiled.borrow().get(&name).cloned();
                            match existing {
                                Some(stashed) => ctx.fetch(&stashed),
                                None => {
                                    let source = resolve_module_source(&root, &name)
                                        .map_err(|e| e.to_error(ctx))?;
                                    let closure =
                                        Closure::load(ctx, Some(name.as_str()), source.as_bytes())
                                            .map_err(|e| {
                                                BridgeError::new(
                                                    "resolution",
                                                    "E_REQUIRE_LOAD",
                                                    format!("failed to load module '{name}': {e}"),
                                                )
                                                .to_error(ctx)
                                            })?;
                                    let function: Function = closure.into();
                                    let stashed = ctx.stash(function);
                                    compiled.borrow_mut().insert(name.clone(), stashed);
                                    function
                                }
                            }
                        };
                        let cache_fn = Callback::from_fn(&ctx, {
                            let cache = cache.clone();
                            let name = name.clone();
                            move |ctx, _exec, mut stack| {
                                let loaded: Table = ctx.fetch(&cache);
                                let value = stack.get(0);
                                let _ = loaded.set(ctx, name.clone(), value);
                                stack.replace(ctx, value);
                                Ok(CallbackReturn::Return)
                            }
                        });
                        let composed = Function::compose(&ctx, vec![function, cache_fn.into()]);
                        stack.clear();
                        Ok(CallbackReturn::Call {
                            function: composed,
                            then: None,
                        })
                    }
                }),
            );
        });
        Ok(())
    }

    /// The configured module root, if any.
    fn directory_root(&self) -> Option<PathBuf> {
        self.module_root.clone()
    }
}

/// Wrap `real` in an empty read-only proxy: reads forward through `__index`,
/// any assignment hits `__newindex` and fails, and `__metatable` is hidden.
fn readonly_table<'gc>(ctx: Context<'gc>, real: Table<'gc>) -> Table<'gc> {
    let proxy = Table::new(&ctx);
    let metatable = Table::new(&ctx);
    metatable
        .set(ctx, "__index", real)
        .expect("metatable accepts __index");
    metatable
        .set(
            ctx,
            "__newindex",
            Callback::from_fn(&ctx, |ctx, _, _| {
                Err(BridgeError::new(
                    "runtime",
                    "E_BITTY_READONLY",
                    "the 'bitty' namespace is read-only",
                )
                .to_error(ctx))
            }),
        )
        .expect("metatable accepts __newindex");
    metatable
        .set(ctx, "__metatable", false)
        .expect("metatable accepts __metatable");
    proxy.set_metatable(&ctx, Some(metatable));
    proxy
}

/// Bound an untrusted token echoed into a host-authored diagnostic.
///
/// Error messages carry at most the offending token, never the full payload
/// (bridge contract): truncate on a char boundary and mark the cut.
fn bounded_token(token: &str) -> String {
    const MAX_CHARS: usize = 32;
    if token.chars().count() <= MAX_CHARS {
        return token.to_string();
    }
    let mut bounded: String = token.chars().take(MAX_CHARS).collect();
    bounded.push('…');
    bounded
}

/// Pure `bitty.env` key-shape predicate shared by every validation site
/// (CTX-0727, #1315).
///
/// Single source of truth for `[A-Za-z_][A-Za-z0-9_]*` within
/// `1..=ENV_KEY_MAX_BYTES` bytes: the bridge [`validate_env_key`], the
/// services boundary, and the `bitty-runtime` grant extractor all agree through this
/// predicate instead of re-spelling the rule. Error-typed callers map `false`
/// to their own fail-closed error; the `bitty-runtime` grant extractor uses
/// it directly.
#[must_use]
pub fn env_key_shape_ok(key: &str) -> bool {
    if key.is_empty() || key.len() > ENV_KEY_MAX_BYTES {
        return false;
    }
    let mut bytes = key.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_');
    first_ok && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Prefix-wildcard `bitty.env` grant shape (CTX-0830, #1483).
///
/// A grant suffix (the `env.read:` prefix stripped) is either an exact key
/// ([`env_key_shape_ok`]) or a prefix wildcard `PREFIX*`: a non-empty
/// well-shaped prefix followed by one literal trailing asterisk. The bare
/// star (`*`, empty prefix) is rejected — there is no allow-all grant — as
/// is any star that is not the single trailing byte.
#[must_use]
pub fn env_grant_shape_ok(grant: &str) -> bool {
    if env_key_shape_ok(grant) {
        return true;
    }
    let Some(prefix) = grant.strip_suffix('*') else {
        return false;
    };
    !prefix.is_empty() && env_key_shape_ok(prefix) && !prefix.contains('*')
}

/// Whether a retained grant suffix authorizes a concrete `bitty.env` key
/// (CTX-0830, #1483).
///
/// Exact grants authorize only their own key; `PREFIX*` grants authorize
/// every well-shaped key with that prefix. The bare star never authorizes
/// (`env_grant_shape_ok` rejects it at the boundary, and this matcher
/// treats a non-shape-conforming grant as deny).
#[must_use]
pub fn env_grant_authorizes(grant: &str, key: &str) -> bool {
    if !env_key_shape_ok(key) {
        return false;
    }
    if let Some(prefix) = grant.strip_suffix('*') {
        return !prefix.is_empty() && env_key_shape_ok(prefix) && key.starts_with(prefix);
    }
    grant == key
}

/// Validate a `bitty.env` key shape (CTX-0330).
///
/// Keys are `[A-Za-z_][A-Za-z0-9_]*` within `1..=ENV_KEY_MAX_BYTES` bytes.
/// Shape failures are `E_DEF_INVALID`/`E_DEF_LIMIT` (validation class) and
/// run before the grant gate; diagnostics quote the bounded key only, never
/// a value.
///
/// The shape rule itself lives in [`env_key_shape_ok`]; this wrapper only
/// attaches the typed errors.
pub fn validate_env_key(key: &str) -> Result<(), BridgeError> {
    if key.is_empty() {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            "env key must not be empty",
        ));
    }
    if key.len() > ENV_KEY_MAX_BYTES {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_LIMIT",
            format!("env key exceeds {ENV_KEY_MAX_BYTES} bytes"),
        ));
    }
    if !env_key_shape_ok(key) {
        let mut bytes = key.bytes();
        let first = bytes.next().unwrap_or(b'_');
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_INVALID",
                format!(
                    "env key '{}' must start with a letter or '_'",
                    bounded_token(key)
                ),
            ));
        }
        return Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            format!(
                "env key '{}' must be [A-Za-z_][A-Za-z0-9_]*",
                bounded_token(key)
            ),
        ));
    }
    Ok(())
}

/// Decode one `bitty.fs` Lua string argument for `what` (RFC-0005, CTX-0984).
///
/// Lua strings are byte strings: lossy conversion would persist U+FFFD
/// replacements and corrupt binary/Latin-1 content under a success receipt.
/// Fail closed with `E_DEF_INVALID` before any grant check or write so a
/// non-UTF-8 caller gets a typed denial and no file is touched.
fn fs_text_arg(value: Value<'_>, what: &str) -> Result<String, BridgeError> {
    let Value::String(raw) = value else {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            format!("{what} must be a string"),
        ));
    };
    let Ok(text) = std::str::from_utf8(raw.as_bytes()) else {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            format!("{what} must be valid UTF-8"),
        ));
    };
    Ok(text.to_owned())
}

/// Validate a `bitty.fs` path shape (RFC-0005, CTX-0984).
///
/// Paths are `1..=FS_PATH_MAX_BYTES` bytes with no NUL or control
/// characters. Shape failures are `E_DEF_INVALID`/`E_DEF_LIMIT` (validation
/// class) and run before the grant gate; diagnostics quote the bounded path
/// only, never file bytes. Scope, sensitive-path, secret, bound, budget,
/// safe-mode, and trust denials are typed `E_FS_*` from the host.
pub fn validate_fs_path(path: &str) -> Result<(), BridgeError> {
    if path.is_empty() {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            "fs path must not be empty",
        ));
    }
    if path.len() > FS_PATH_MAX_BYTES {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_LIMIT",
            format!("fs path exceeds {FS_PATH_MAX_BYTES} bytes"),
        ));
    }
    if path.contains('\0') || path.chars().any(|c| c.is_control()) {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            format!(
                "fs path '{}' must not contain NUL or control characters",
                bounded_token(path)
            ),
        ));
    }
    Ok(())
}

/// Validate `bitty.fs.write` content shape (RFC-0005, CTX-0984).
///
/// Content crosses as a bounded string; over-bound payloads fail closed with
/// `E_DEF_LIMIT` before any grant check. Secret-shaped refusal, sensitive
/// gating, and budget enforcement stay host-side with typed `E_FS_*`.
pub fn validate_fs_content(content: &str) -> Result<(), BridgeError> {
    if content.len() > FS_CONTENT_MAX_BYTES {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_LIMIT",
            format!("fs content exceeds {FS_CONTENT_MAX_BYTES} bytes"),
        ));
    }
    if content.contains('\0') {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            "fs content must not contain NUL",
        ));
    }
    Ok(())
}

/// Parse the `bitty.fs.write` disposition flag (RFC-0005, CTX-0984).
///
/// `append` is a write-disposition flag candidate, not a verb: `nil`
/// (absent) means create-or-overwrite, a table `{ append = bool }` selects
/// the disposition, and any other shape fails closed with `E_DEF_INVALID`.
/// Unknown table fields are ignored.
pub fn parse_fs_write_append(value: &LuaValue) -> Result<bool, BridgeError> {
    match value {
        LuaValue::Nil => Ok(false),
        LuaValue::Table(_) => match value.get("append") {
            None | Some(LuaValue::Nil) => Ok(false),
            Some(LuaValue::Bool(append)) => Ok(*append),
            Some(_) => Err(BridgeError::value(
                "E_DEF_INVALID",
                "fs.write append must be a boolean",
            )),
        },
        _ => Err(BridgeError::value(
            "E_DEF_INVALID",
            "fs.write opts must be a table or nil",
        )),
    }
}

/// Validate a `bitty.fs.list` entry bound (RFC-0005, CTX-0984).
///
/// `max_entries` is an explicit page bound within the bridge defensive cap;
/// the host gate enforces the authoritative ceiling. Failures are
/// `E_DEF_INVALID`/`E_DEF_LIMIT` before any grant check.
pub fn validate_fs_list_max(max_entries: usize) -> Result<(), BridgeError> {
    if max_entries == 0 {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            "fs.list max_entries must be positive",
        ));
    }
    if max_entries > FS_LIST_MAX_ENTRIES {
        return Err(BridgeError::new(
            "validation",
            "E_DEF_LIMIT",
            format!("fs.list max_entries exceeds {FS_LIST_MAX_ENTRIES}"),
        ));
    }
    Ok(())
}

/// Extract a 1-based argv array of strings from a marshalled Lua value.
///
/// Fails closed with `E_VALUE_*` when the value is not a dense 1-based
/// string array, is empty, exceeds [`SPAWN_LUA_MAX_ARGS`], or carries an
/// entry past [`SPAWN_LUA_MAX_ARG_BYTES`]. Tighter per-tool bounds live
/// host-side with the allowlist (CTX-0444); this is shape only.
fn spawn_argv(value: &LuaValue) -> Result<Vec<String>, BridgeError> {
    let LuaValue::Table(pairs) = value else {
        return Err(BridgeError::value(
            "E_VALUE_TYPE",
            "process.spawn expects an argv array table",
        ));
    };
    if pairs.is_empty() {
        return Err(BridgeError::value(
            "E_VALUE_TYPE",
            "process.spawn argv must not be empty",
        ));
    }
    if pairs.len() > SPAWN_LUA_MAX_ARGS {
        return Err(BridgeError::value(
            "E_VALUE_NODES",
            "process.spawn argv exceeds the entry limit",
        ));
    }
    let mut args = Vec::with_capacity(pairs.len());
    for (index, (key, item)) in pairs.iter().enumerate() {
        let want = index as i64 + 1;
        if *key != LuaValue::Integer(want) {
            return Err(BridgeError::value(
                "E_VALUE_TYPE",
                "process.spawn argv must be a dense 1-based array",
            ));
        }
        let LuaValue::String(text) = item else {
            return Err(BridgeError::value(
                "E_VALUE_TYPE",
                "process.spawn argv entries must be strings",
            ));
        };
        if text.is_empty() {
            return Err(BridgeError::value(
                "E_VALUE_TYPE",
                "process.spawn argv entries must not be empty",
            ));
        }
        if text.len() > SPAWN_LUA_MAX_ARG_BYTES {
            return Err(BridgeError::value(
                "E_VALUE_BYTES",
                "process.spawn argv entry exceeds the byte limit",
            ));
        }
        args.push(text.clone());
    }
    Ok(args)
}

fn required_string<'gc>(
    ctx: Context<'gc>,
    table: Table<'gc>,
    field: &str,
) -> Result<String, Error<'gc>> {
    match optional_string(ctx, table, field) {
        Some(value) if !value.is_empty() => Ok(value),
        _ => Err(BridgeError::new(
            "validation",
            "E_DEF_INVALID",
            format!("'{field}' must be a non-empty string"),
        )
        .to_error(ctx)),
    }
}

fn optional_string<'gc>(ctx: Context<'gc>, table: Table<'gc>, field: &str) -> Option<String> {
    match table.get_value(ctx, field.to_string()) {
        Value::String(s) => Some(String::from_utf8_lossy(s.as_bytes()).into_owned()),
        _ => None,
    }
}

/// Shape-check one service interface name (LUA-OQ-8 admission bound).
///
/// Mirrors the manifest `services.provided` ceiling (1..128 bytes,
/// dot-separated non-empty segments of at most 64 bytes, no NUL or
/// space); the full grammar stays host-side, the bridge rejects only
/// malformed shapes with a static detail for the caller to wrap.
fn check_service_iface_shape(iface: &str) -> Result<(), &'static str> {
    if iface.is_empty() || iface.len() > SERVICE_MAX_IFACE_BYTES {
        return Err("service interface must be 1..128 bytes");
    }
    if iface.contains('\0') || iface.contains(' ') {
        return Err("service interface must not contain NUL or space");
    }
    for segment in iface.split('.') {
        if segment.is_empty() || segment.len() > 64 {
            return Err("service interface segment must be 1..64 bytes");
        }
    }
    Ok(())
}

/// Read `bitty.services.get(iface, opts)` options (LUA-OQ-8).
///
/// Returns `(version_req, optional)`: `version` overrides the caller
/// manifest's `services.required` entry for this call, `optional`
/// degrades an unresolvable interface to Lua `nil`. Unknown keys and
/// mistyped values fail closed — silent options would hide typos such
/// as `optinal = true`.
fn read_service_opts(opts: Table<'_>) -> Result<(Option<String>, bool), &'static str> {
    let mut req = None;
    let mut optional = false;
    for (key, value) in opts.iter() {
        let Value::String(raw) = key else {
            return Err("service option keys must be strings");
        };
        let name: String = String::from_utf8_lossy(raw.as_bytes()).into_owned();
        match (name.as_str(), value) {
            ("version", Value::String(text)) => {
                let text: String = String::from_utf8_lossy(text.as_bytes()).into_owned();
                if text.is_empty() || text.len() > 128 {
                    return Err("service option 'version' must be 1..128 bytes");
                }
                req = Some(text);
            }
            ("version", Value::Nil) => {}
            ("optional", Value::Boolean(flag)) => optional = flag,
            ("optional", Value::Nil) => {}
            ("version", _) => return Err("service option 'version' must be a string"),
            ("optional", _) => return Err("service option 'optional' must be a boolean"),
            _ => return Err("unknown service option"),
        }
    }
    Ok((req, optional))
}

fn validate_module_name(name: &str) -> Result<(), BridgeError> {
    if name.is_empty() || name.len() > MODULE_NAME_MAX_BYTES {
        return Err(BridgeError::new(
            "validation",
            "E_REQUIRE_NAME",
            "module name must be 1..128 bytes",
        ));
    }
    if name.contains("..") || name.starts_with('.') || name.ends_with('.') {
        return Err(BridgeError::new(
            "resolution",
            "E_REQUIRE_TRAVERSAL",
            "module name must not traverse",
        ));
    }
    for byte in name.bytes() {
        let allowed = byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.');
        if !allowed {
            return Err(BridgeError::new(
                "validation",
                "E_REQUIRE_NAME",
                "module name contains an invalid character",
            ));
        }
    }
    Ok(())
}

/// Resolve `name` to bounded UTF-8 source inside `root`, fail-closed.
fn resolve_module_source(root: &Path, name: &str) -> Result<String, BridgeError> {
    let relative = name.replace('.', "/");
    let direct = root.join(format!("{relative}.lua"));
    let init = root.join(&relative).join("init.lua");
    let candidate = if direct.is_file() {
        direct
    } else if init.is_file() {
        init
    } else {
        return Err(BridgeError::new(
            "resolution",
            "E_REQUIRE_NOT_FOUND",
            format!("module '{name}' was not found under the plugin root"),
        ));
    };
    let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
        BridgeError::new(
            "resolution",
            "E_REQUIRE_NOT_FOUND",
            format!("module '{name}' could not be resolved: {error}"),
        )
    })?;
    if !canonical.starts_with(root) {
        return Err(BridgeError::new(
            "resolution",
            "E_REQUIRE_TRAVERSAL",
            format!("module '{name}' resolves outside the plugin root"),
        ));
    }
    if canonical.extension().and_then(|e| e.to_str()) != Some("lua") {
        return Err(BridgeError::new(
            "resolution",
            "E_REQUIRE_NATIVE",
            "only source `.lua` modules may be required",
        ));
    }
    let file = std::fs::File::open(&canonical).map_err(|error| {
        BridgeError::new(
            "resolution",
            "E_REQUIRE_NOT_FOUND",
            format!("module '{name}' could not be opened: {error}"),
        )
    })?;
    let metadata = file.metadata().map_err(|error| {
        BridgeError::new(
            "resolution",
            "E_REQUIRE_NOT_FOUND",
            format!("module '{name}' metadata unavailable: {error}"),
        )
    })?;
    if !metadata.is_file() {
        return Err(BridgeError::new(
            "resolution",
            "E_REQUIRE_NOT_FOUND",
            format!("module '{name}' is not a regular file"),
        ));
    }
    if metadata.len() as usize > MODULE_FILE_MAX_BYTES {
        return Err(BridgeError::new(
            "budget",
            "E_MODULE_TOO_LARGE",
            "module source exceeds the per-file byte ceiling",
        ));
    }
    let mut source = String::new();
    file.take((MODULE_FILE_MAX_BYTES + 1) as u64)
        .read_to_string(&mut source)
        .map_err(|error| {
            BridgeError::new(
                "resolution",
                "E_REQUIRE_LOAD",
                format!("module '{name}' is not valid UTF-8 source: {error}"),
            )
        })?;
    if source.len() > MODULE_FILE_MAX_BYTES {
        return Err(BridgeError::new(
            "budget",
            "E_MODULE_TOO_LARGE",
            "module source exceeds the per-file byte ceiling",
        ));
    }
    Ok(source)
}

/// Typed outcome of [`LuaVm::execute_bounded`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundedExecution {
    /// The chunk completed within budget.
    Completed,
    /// The chunk exceeded `RC-1`/`RC-2` and the VM suspended fail-closed.
    Suspended(SuspendReason),
    /// The chunk raised a Lua runtime error.
    RuntimeError(String),
}

impl LuaVm {
    /// Run a host-controlled chunk under `RC-1`/`RC-2` and return a typed
    /// outcome instead of a raw VM error.
    ///
    /// # Errors
    ///
    /// [`VmError::Suspended`] when the VM was already suspended;
    /// [`VmError::Budget`] for a pre-existing budget configuration error.
    pub fn execute_bounded(&mut self, code: &str) -> Result<BoundedExecution, VmError> {
        match self.execute(code)? {
            crate::ExecuteOutcome::Completed { .. } => Ok(BoundedExecution::Completed),
            crate::ExecuteOutcome::Suspended { reason, .. } => {
                Ok(BoundedExecution::Suspended(reason))
            }
            crate::ExecuteOutcome::RuntimeError { message } => {
                Ok(BoundedExecution::RuntimeError(message))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_module_root(tag: &str) -> TempDir {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitty-lua-host-unit-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let canonical = std::fs::canonicalize(&dir).expect("canonicalize temp dir");
        TempDir(canonical)
    }

    #[test]
    fn env_key_validator_and_shape_predicate_agree() {
        // CTX-0727 (#1315): the shared rule lives in `env_key_shape_ok`;
        // `validate_env_key` only attaches typed errors. Pin agreement so
        // the sites cannot drift.
        let long = "A".repeat(ENV_KEY_MAX_BYTES + 1);
        let cases: &[(&str, bool, Option<&str>)] = &[
            ("HOME", true, None),
            ("_x1", true, None),
            ("", false, Some("E_DEF_INVALID")),
            ("9LIVES", false, Some("E_DEF_INVALID")),
            ("has space", false, Some("E_DEF_INVALID")),
            ("lower-ok?", false, Some("E_DEF_INVALID")),
            (long.as_str(), false, Some("E_DEF_LIMIT")),
        ];
        for (key, ok, code) in cases {
            assert_eq!(env_key_shape_ok(key), *ok, "predicate for '{key}'");
            match validate_env_key(key) {
                Ok(()) => assert!(ok, "validator accepts '{key}'"),
                Err(error) => {
                    assert!(!ok, "validator rejects '{key}'");
                    assert_eq!(error.code, code.unwrap(), "code for '{key}'");
                }
            }
        }
    }

    #[test]
    fn env_grant_shape_ok_accepts_exact_and_prefix_wildcard() {
        // CTX-0830 (#1483): exact keys and `PREFIX*` are well-shaped grants;
        // the bare star and non-trailing stars are not.
        for grant in ["HOME", "_x1", "APP_*", "A_*"] {
            assert!(env_grant_shape_ok(grant), "accept '{grant}'");
        }
        for grant in [
            "",
            "*",
            "*APP",
            "AP*P",
            "APP**",
            "9LIVES",
            "9LIVES*",
            "has space",
            "lower-ok?",
        ] {
            assert!(!env_grant_shape_ok(grant), "reject '{grant}'");
        }
        let long_prefix = "A".repeat(ENV_KEY_MAX_BYTES + 1);
        let long_grant = format!("{long_prefix}*");
        assert!(
            !env_grant_shape_ok(&long_grant),
            "over-bound prefix rejected"
        );
    }

    #[test]
    fn env_grant_authorizes_matches_exact_and_prefix() {
        // CTX-0830 (#1483): exact grants match only their own key, `PREFIX*`
        // matches keys carrying that prefix, everything else fails closed.
        assert!(env_grant_authorizes("HOME", "HOME"));
        assert!(!env_grant_authorizes("HOME", "HOMELY"));
        assert!(env_grant_authorizes("APP_*", "APP_TOKEN"));
        assert!(env_grant_authorizes("APP_*", "APP_"));
        assert!(!env_grant_authorizes("APP_*", "APP"));
        assert!(!env_grant_authorizes("APP_*", "OTHER"));
        assert!(!env_grant_authorizes("*", "HOME"));
        assert!(!env_grant_authorizes("AP*P", "APXP"));
        assert!(!env_grant_authorizes("HOME", "9LIVES"));
        assert!(!env_grant_authorizes("APP_*", ""));
    }

    #[test]
    fn resolve_regular_module_direct() {
        let root = temp_module_root("direct");
        let code = "local M = {}; M.answer = 42; return M";
        std::fs::write(root.0.join("my_mod.lua"), code).expect("write module");

        let resolved = resolve_module_source(&root.0, "my_mod").expect("resolve");
        assert_eq!(resolved, code);
    }

    #[test]
    fn resolve_regular_module_init() {
        let root = temp_module_root("init");
        let pkg_dir = root.0.join("pkg");
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        let code = "return { name = 'pkg' }";
        std::fs::write(pkg_dir.join("init.lua"), code).expect("write init.lua");

        let resolved = resolve_module_source(&root.0, "pkg").expect("resolve");
        assert_eq!(resolved, code);
    }

    #[test]
    fn resolve_regular_module_nested() {
        let root = temp_module_root("nested");
        let sub_dir = root.0.join("foo").join("bar");
        std::fs::create_dir_all(&sub_dir).expect("create sub dirs");
        let code = "return 'nested'";
        std::fs::write(sub_dir.join("baz.lua"), code).expect("write baz.lua");

        let resolved = resolve_module_source(&root.0, "foo.bar.baz").expect("resolve");
        assert_eq!(resolved, code);
    }

    #[test]
    fn rejects_module_exceeding_byte_ceiling_on_handle_inspection() {
        let root = temp_module_root("oversized");
        let path = root.0.join("huge.lua");
        let file = std::fs::File::create(&path).expect("create file");
        file.set_len((MODULE_FILE_MAX_BYTES + 1) as u64)
            .expect("set_len");
        drop(file);

        let err = resolve_module_source(&root.0, "huge").expect_err("must be rejected");
        assert_eq!(err.class, "budget");
        assert_eq!(err.code, "E_MODULE_TOO_LARGE");
        assert_eq!(
            err.message,
            "module source exceeds the per-file byte ceiling"
        );
    }

    #[test]
    fn accepts_module_at_exact_byte_ceiling() {
        let root = temp_module_root("exact-ceiling");
        let path = root.0.join("exact.lua");
        let chunk = vec![b' '; MODULE_FILE_MAX_BYTES];
        std::fs::write(&path, chunk).expect("write exact bytes");

        let resolved = resolve_module_source(&root.0, "exact").expect("resolve exact size");
        assert_eq!(resolved.len(), MODULE_FILE_MAX_BYTES);
    }

    #[test]
    fn rejects_non_file_target_directory() {
        let root = temp_module_root("non-file-dir");
        std::fs::create_dir(root.0.join("somedir.lua")).expect("create dir");

        let err = resolve_module_source(&root.0, "somedir").expect_err("must fail");
        assert_eq!(err.class, "resolution");
        assert_eq!(err.code, "E_REQUIRE_NOT_FOUND");
    }

    #[test]
    fn rejects_non_existent_module() {
        let root = temp_module_root("non-existent");
        let err = resolve_module_source(&root.0, "missing").expect_err("must fail");
        assert_eq!(err.class, "resolution");
        assert_eq!(err.code, "E_REQUIRE_NOT_FOUND");
        assert!(err.message.contains("was not found under the plugin root"));
    }

    #[test]
    fn rejects_broken_invalid_utf8_file() {
        let root = temp_module_root("broken-utf8");
        std::fs::write(root.0.join("corrupt.lua"), [0xff, 0xfe, 0xfd, 0x00])
            .expect("write corrupt");

        let err = resolve_module_source(&root.0, "corrupt").expect_err("must fail");
        assert_eq!(err.class, "resolution");
        assert_eq!(err.code, "E_REQUIRE_LOAD");
        assert!(err.message.contains("is not valid UTF-8 source"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_unreadable_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_module_root("unreadable");
        let path = root.0.join("locked.lua");
        std::fs::write(&path, "return 1").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let err = resolve_module_source(&root.0, "locked").expect_err("must fail");
        assert_eq!(err.class, "resolution");
        assert_eq!(err.code, "E_REQUIRE_NOT_FOUND");
        assert!(err.message.contains("could not be opened"));

        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
    }

    #[test]
    fn installed_require_mounts_on_single_vfs_root() {
        // The host `require` injection mounts on the configured module root as
        // its only VFS-style capability root: modules resolve inside it, and
        // traversal outside it fails closed as a Lua error. The VM itself is
        // built through the fail-closed gate with the builder hard quota.
        let root = temp_module_root("mounted");
        std::fs::write(root.0.join("agg.lua"), "return { v = 7 }").expect("write module");
        let mut vm = crate::gate::build_plugin_vm(
            "require-mounted",
            Some(crate::gate::VmBudgets::default()),
        )
        .expect("gate build");
        vm.with_module_root(root.0.clone());
        vm.install_require().expect("install require");
        let outcome = vm
            .execute("agg_value = require(\"agg\").v")
            .expect("execute");
        assert!(
            matches!(outcome, crate::ExecuteOutcome::Completed { .. }),
            "{outcome:?}"
        );
        assert_eq!(vm.test_global("agg_value"), Some(7.0));
        // Escape past the root is denied fail-closed as a Lua error, and the
        // VM stays usable (no suspension from a refused resolution).
        let outcome = vm.execute("require(\"../outside\")").expect("execute");
        assert!(
            matches!(outcome, crate::ExecuteOutcome::RuntimeError { .. }),
            "{outcome:?}"
        );
        assert!(!vm.is_suspended());
    }

    #[test]
    fn workspace_list_value_carries_scratchpad_occupancy() {
        // CTX-0954: empty vs occupied rows marshal count + presence alongside
        // the existing shape; over-bound counts clamp to the single-slot
        // ceiling so a misbehaving host source stays bounded.
        let row = |id: u64, count: usize, occupied: bool| WorkspaceInfo {
            id,
            name: format!("ws{id}"),
            active: id == 1,
            panel_count: 2,
            scratchpad_count: count,
            scratchpad_occupied: occupied,
            attention: WorkspaceAttention::default(),
        };
        let value = workspace_list_value(&[row(1, 0, false), row(2, 99, true)]);
        let rows_out = match &value {
            LuaValue::Table(pairs) => pairs,
            _ => panic!("list marshals as an array table"),
        };
        assert_eq!(rows_out.len(), 2);
        let table = |index: usize| match &rows_out[index].1 {
            LuaValue::Table(pairs) => LuaValue::Table(pairs.clone()),
            _ => panic!("row {index} marshals as a table"),
        };
        let empty = table(0);
        assert_eq!(empty.get("scratchpad_count"), Some(&LuaValue::Integer(0)));
        assert_eq!(
            empty.get("scratchpad_occupied"),
            Some(&LuaValue::Bool(false))
        );
        assert_eq!(empty.get("panel_count"), Some(&LuaValue::Integer(2)));
        let occupied = table(1);
        assert_eq!(
            occupied.get("scratchpad_count"),
            Some(&LuaValue::Integer(1)),
            "clamped to the single-slot ceiling"
        );
        assert_eq!(
            occupied.get("scratchpad_occupied"),
            Some(&LuaValue::Bool(true))
        );
    }
}
