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
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use piccolo::{
    Callback, CallbackReturn, Closure, Context, Error, Function, StashedFunction, Table, Value,
};

use crate::{LuaVm, SuspendReason, VmError};

/// Version of the host bridge line exposed as `bitty.api_version`.
pub const API_VERSION: &str = "1.0.0";

/// Default marshalling depth ceiling (`RC-1`/bridge contract A.3).
pub const DEFAULT_MAX_DEPTH: usize = 8;
/// Default marshalling node ceiling (bridge contract A.3).
pub const DEFAULT_MAX_NODES: usize = 1024;
/// Default marshalling byte ceiling for storage-shaped values (`8 KiB`).
pub const DEFAULT_MAX_VALUE_BYTES: usize = 8 * 1024;
/// Snapshot byte ceiling (`SNAPSHOT_MAX_BYTES`, RFC C.2).
pub const SNAPSHOT_MAX_BYTES: usize = 256 * 1024;
/// Default host-call deadline in milliseconds (reuses `RC-1`).
pub const DEFAULT_HOST_DEADLINE_MS: u64 = crate::RC1_WALL_CLOCK_BUDGET_MS;

/// Maximum bytes of one module name accepted by `require`.
pub const MODULE_NAME_MAX_BYTES: usize = 128;
/// Maximum bytes of one source module file accepted by `require`.
pub const MODULE_FILE_MAX_BYTES: usize = 1024 * 1024;

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
                    let _ = table.set_value(&ctx, key.to_lua(ctx), value.to_lua(ctx));
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
/// Every method is synchronous and non-blocking. Implementors are expected to
/// be cheap and bounded; the bridge deadline-checks each call and fails closed
/// with `E_TIMEOUT`, and rejects re-entrant calls. Capability gating is the
/// caller's responsibility (it decides what `services` are reachable), but
/// implementations should still fail closed.
pub trait HostServices {
    /// Read a plugin-scoped store entry; `Ok(None)` means absent.
    fn store_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError>;
    /// Atomically write a plugin-scoped store entry.
    fn store_set(&self, key: &str, value: LuaValue) -> Result<(), BridgeError>;
    /// Read a typed setting; `Ok(None)` means absent.
    fn settings_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError>;
    /// Read a bounded committed terminal snapshot for `scope`.
    fn terminal_snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError>;
    /// Hand a notification to the platform asynchronously; returns acceptance.
    fn notify_show(&self, payload: &LuaValue) -> Result<bool, BridgeError>;
    /// Current monotonic host time in milliseconds (for timer scheduling).
    fn now_millis(&self) -> u64 {
        0
    }
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
        }
    }

    /// Remove a timer by handle; returns whether it existed.
    pub fn cancel_timer(&mut self, handle: i64) -> bool {
        let before = self.timers.len();
        self.timers.retain(|timer| timer.handle != handle);
        self.timers.len() != before
    }
}

/// Shared `bitty` bridge state installed into one VM.
struct BridgeState {
    services: Rc<dyn HostServices>,
    capture: Rc<RefCell<RegistrationCapture>>,
    limits: MarshallingLimits,
    deadline_ms: u64,
    in_call: Rc<Cell<bool>>,
}

impl BridgeState {
    fn bounded<T>(&self, f: impl FnOnce() -> Result<T, BridgeError>) -> Result<T, BridgeError> {
        if self.in_call.get() {
            return Err(BridgeError::new(
                "runtime",
                "E_BRIDGE_REENTRANT",
                "bridge call re-entered",
            ));
        }
        self.in_call.set(true);
        struct Reset(Rc<Cell<bool>>);
        impl Drop for Reset {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _reset = Reset(self.in_call.clone());
        let start = Instant::now();
        let out = f()?;
        if start.elapsed() > Duration::from_millis(self.deadline_ms) {
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
            in_call: self.in_bridge_call.clone(),
        });

        self.lua.enter(|ctx| {
            let root = build_bitty_root(ctx, &state);
            ctx.set_global("bitty", root)
                .expect("globals accept 'bitty'");
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
            ctx.stash(piccolo::Executor::start(ctx, func, piccolo::Variadic(argv)))
        });

        match self.drive_stashed(stashed)? {
            crate::DriveOutcome::Suspended { reason, .. } => Err(VmError::Suspended { reason }),
            crate::DriveOutcome::Failed { message } => Err(VmError::Load(message)),
            crate::DriveOutcome::Ready { stashed } => {
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
                    let run = match def.get(ctx, "run") {
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
                    state
                        .capture
                        .borrow_mut()
                        .commands
                        .push(CommandRegistration {
                            id,
                            title,
                            description,
                            run,
                        });
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
                    state
                        .capture
                        .borrow_mut()
                        .events
                        .push(EventSubscription { kind, handler });
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
                        .bounded(|| state.services.settings_get(&key))
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
                        .bounded(|| state.services.store_get(&key))
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
                        .bounded(|| state.services.store_set(&key, value))
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
                        Value::Table(table) => match table.get(ctx, "scope".to_string()) {
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
                        .bounded(|| state.services.terminal_snapshot(&scope))
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
                        .bounded(|| state.services.notify_show(&value))
                        .map_err(|e| e.to_error(ctx))?;
                    stack.replace(ctx, Value::Boolean(accepted));
                    Ok(CallbackReturn::Return)
                }
            }),
        )
        .expect("notify table accepts 'show'");

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
                    let handle = {
                        let mut capture = state.capture.borrow_mut();
                        let handle = capture.next_timer_handle;
                        capture.next_timer_handle += 1;
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
    root.set(ctx, "timers", readonly_table(ctx, timers))
        .expect("root accepts timers");
    Value::Table(readonly_table(ctx, root))
}

impl LuaVm {
    /// Install the rooted, source-only `require` over the configured module root.
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
                        let cached = loaded.get(ctx, name.clone());
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
            )
            .expect("globals accept 'require'");
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
    match table.get(ctx, field.to_string()) {
        Value::String(s) => Some(String::from_utf8_lossy(s.as_bytes()).into_owned()),
        _ => None,
    }
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
    let metadata = std::fs::metadata(&canonical).map_err(|error| {
        BridgeError::new(
            "resolution",
            "E_REQUIRE_NOT_FOUND",
            format!("module '{name}' metadata unavailable: {error}"),
        )
    })?;
    if metadata.len() as usize > MODULE_FILE_MAX_BYTES {
        return Err(BridgeError::new(
            "budget",
            "E_MODULE_TOO_LARGE",
            "module source exceeds the per-file byte ceiling",
        ));
    }
    std::fs::read_to_string(&canonical).map_err(|error| {
        BridgeError::new(
            "resolution",
            "E_REQUIRE_LOAD",
            format!("module '{name}' is not valid UTF-8 source: {error}"),
        )
    })
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
