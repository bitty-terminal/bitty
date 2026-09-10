//! `registry` — Panel Runtime domain (`PanelRegistry`, event bus, overlays,
//! commands, capabilities) per CTX-0102.
//!
//! Split from `super` (`registry.rs`) as a pure move under CTX-0308:
//! byte-identical logic, only module wiring changed.

use super::*;

// ---------------------------------------------------------------------------
// Panel Runtime domain (CTX-0102) — generic Panel Runtime per 9032d1e
// ---------------------------------------------------------------------------
//
// Implements PanelId distinct newtype, generation monotonic, lifecycle
// Declared->Created->Mounted->Focused->Suspended->Disposed, command registry
// owner.name:command, overlay max 4+1, focus MRU per Window/Workspace,
// EventBus 64/1024/2MiB/8192 DropOldest, panel.* capability per
// (PanelId,generation). Single-process winit window, one registry per
// window, no bittyd/remote. Bounded, no unsafe, headless testable.
//
// Placement: `Instance -> Window -> Workspace -> LayoutTree -> View`
// stays authoritative; Panel is typed View content (`ViewContent::Panel`)
// per Option A, reusing ViewId generation, focus MRU, visibility.
// The panel runtime owns panel lifecycle, the workspace owns layout,
// the terminal registry owns PTY descriptors; no view/panel holds PTY fd,
// GPU object, or OS window handle.

// Re-export canonical Panel types from bitty-ui for single definition.
pub use bitty_ui::panel::{
    MAX_COMMANDS_PER_PANEL_TYPE, MAX_OVERLAY_TEXT_LEN, MAX_OVERLAY_TOOLTIP_LEN,
    MAX_OVERLAYS_PER_WINDOW, Overlay, OverlayError, OverlayKind, OverlayManager, PanelFocus,
    QualifiedCommand, ViewContent,
};
pub use bitty_ui::panel::{PanelId, PanelState, PanelType};

// PanelId distinctness is already enforced by `bitty_ui::PanelId` being a
// newtype with no From bridge to `ViewId`/`TerminalId`. Re-exported here so
// `registry::PanelId` is the same canonical type but still pairwise
// incompatible at type level across crates (requires explicit import).

pub const MAX_PANELS_PER_WORKSPACE: usize = 32;
pub const MAX_PANELS_PER_WINDOW: usize = 64;
pub const DEFAULT_MAX_PANELS_PER_WORKSPACE: usize = 16;
pub const DEFAULT_MAX_PANELS_PER_WINDOW: usize = 32;
pub const MAX_TOPICS_TOTAL: usize = 256;
pub const MAX_SUBSCRIPTIONS_PER_PANEL: usize = 32;
pub const MAX_PANEL_COMMANDS_PER_TYPE: usize = 32;
pub const BUS_PER_SUBSCRIPTION_LIMIT: usize = 64;
pub const BUS_PER_PANEL_LIMIT: usize = 1024;
pub const BUS_PER_PANEL_BYTES_LIMIT: usize = 256 * 1024;
pub const BUS_GLOBAL_LIMIT: usize = 8192;
pub const BUS_GLOBAL_BYTES_LIMIT: usize = 2 * 1024 * 1024;
pub const BUS_EVENT_MAX_BYTES: usize = 8 * 1024;
pub const BUS_BATCH_MAX_EVENTS: usize = 32;
pub const BUS_BATCH_MAX_BYTES: usize = 8 * 1024;

/// Panel generation is the same monotonic `Generation` type; panels bump the
/// same registry generation counter so stale `(PanelId, Generation)` handles
/// are detectable, mirroring terminal/view rules.
pub type PanelGeneration = Generation;

/// Handle for a panel instance: `(PanelId, Generation)` pair that must be
/// validated on every cross-component call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PanelHandle {
    pub id: PanelId,
    pub generation: Generation,
}

/// Panel-specific error type; leaves previous valid state intact (fail-closed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelError {
    TooManyPanels {
        max: usize,
        current: usize,
    },
    TooManyTopics {
        max: usize,
        current: usize,
    },
    TooManySubscriptions {
        max: usize,
        current: usize,
    },
    PayloadTooLarge {
        bytes: usize,
        max: usize,
    },
    UnknownPanelType {
        value: String,
    },
    UnknownTopic {
        topic: String,
    },
    UndisclosedTopic {
        topic: String,
    },
    AlreadyMounted {
        view_id: ViewId,
        existing: ViewContent,
    },
    PanelAlreadyMounted {
        panel_id: PanelId,
        current_view: ViewId,
    },
    StaleHandle {
        expected_generation: Generation,
        found_generation: Generation,
        id_raw: u64,
    },
    RegistryDisposed {
        generation: Generation,
    },
    GenerationExhausted {
        current: Generation,
    },
    OverlayBusy,
    TooManyOverlays {
        max: usize,
        current: usize,
    },
    CapabilityDenied {
        panel_id: PanelId,
        capability: String,
    },
    InvalidCommand {
        reason: String,
    },
    DuplicateCommand {
        command: String,
        owner: PanelId,
    },
    TooManyCommands {
        max: usize,
        current: usize,
    },
    NotFound {
        kind: &'static str,
        id_raw: u64,
    },
    InvalidState {
        current: PanelState,
        expected: &'static str,
    },
    ResourceExhausted {
        reason: String,
    },
}

impl std::fmt::Display for PanelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyPanels { max, current } => {
                write!(f, "too many panels: max {max}, current {current}")
            }
            Self::TooManyTopics { max, current } => {
                write!(f, "too many topics: max {max}, current {current}")
            }
            Self::TooManySubscriptions { max, current } => {
                write!(f, "too many subscriptions: max {max}, current {current}")
            }
            Self::PayloadTooLarge { bytes, max } => {
                write!(f, "payload too large: {bytes} > {max}")
            }
            Self::UnknownPanelType { value } => write!(f, "unknown panel type '{value}'"),
            Self::UnknownTopic { topic } => write!(f, "unknown topic '{topic}'"),
            Self::UndisclosedTopic { topic } => write!(f, "undisclosed topic '{topic}'"),
            Self::AlreadyMounted { view_id, existing } => {
                write!(f, "view {view_id} already hosts {existing:?}")
            }
            Self::PanelAlreadyMounted {
                panel_id,
                current_view,
            } => write!(f, "panel {panel_id} already mounted at {current_view}"),
            Self::StaleHandle {
                expected_generation,
                found_generation,
                id_raw,
            } => write!(
                f,
                "stale panel handle id {id_raw}: expected {expected_generation}, found {found_generation}"
            ),
            Self::RegistryDisposed { generation } => {
                write!(f, "panel registry disposed at {generation}")
            }
            Self::GenerationExhausted { current } => {
                write!(f, "generation exhausted at {current}")
            }
            Self::OverlayBusy => f.write_str("modal overlay already active (OverlayBusy)"),
            Self::TooManyOverlays { max, current } => {
                write!(f, "too many overlays: max {max}, current {current}")
            }
            Self::CapabilityDenied {
                panel_id,
                capability,
            } => write!(f, "panel {panel_id} missing capability '{capability}'"),
            Self::InvalidCommand { reason } => write!(f, "invalid command: {reason}"),
            Self::DuplicateCommand { command, owner } => {
                write!(f, "duplicate command '{command}' already owned by {owner}")
            }
            Self::TooManyCommands { max, current } => {
                write!(f, "too many commands: max {max}, current {current}")
            }
            Self::NotFound { kind, id_raw } => write!(f, "{kind} {id_raw} not found"),
            Self::InvalidState { current, expected } => {
                write!(f, "invalid state {current}: expected {expected}")
            }
            Self::ResourceExhausted { reason } => write!(f, "resource exhausted: {reason}"),
        }
    }
}

impl std::error::Error for PanelError {}

/// Config for `PanelRegistry`; validated before creation via `ConfigPlan`-like checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelRegistryConfig {
    pub max_panels_per_workspace: usize,
    pub max_panels_per_window: usize,
    pub max_topics_total: usize,
    pub max_subscriptions_per_panel: usize,
}

impl Default for PanelRegistryConfig {
    fn default() -> Self {
        Self {
            max_panels_per_workspace: DEFAULT_MAX_PANELS_PER_WORKSPACE,
            max_panels_per_window: DEFAULT_MAX_PANELS_PER_WINDOW,
            max_topics_total: MAX_TOPICS_TOTAL,
            max_subscriptions_per_panel: MAX_SUBSCRIPTIONS_PER_PANEL,
        }
    }
}

impl PanelRegistryConfig {
    pub fn validate(&self) -> Result<(), PanelError> {
        if !(1..=MAX_PANELS_PER_WORKSPACE).contains(&self.max_panels_per_workspace) {
            return Err(PanelError::ResourceExhausted {
                reason: "max_panels_per_workspace must be in [1, 32]".to_string(),
            });
        }
        if !(1..=MAX_PANELS_PER_WINDOW).contains(&self.max_panels_per_window) {
            return Err(PanelError::ResourceExhausted {
                reason: "max_panels_per_window must be in [1, 64]".to_string(),
            });
        }
        if self.max_topics_total == 0 || self.max_topics_total > MAX_TOPICS_TOTAL {
            return Err(PanelError::ResourceExhausted {
                reason: "max_topics_total must be in [1, 256]".to_string(),
            });
        }
        if self.max_subscriptions_per_panel == 0
            || self.max_subscriptions_per_panel > MAX_SUBSCRIPTIONS_PER_PANEL
        {
            return Err(PanelError::ResourceExhausted {
                reason: "max_subscriptions_per_panel must be in [1, 32]".to_string(),
            });
        }
        Ok(())
    }
}

/// Internal panel record.
#[allow(dead_code)]
#[derive(Debug)]
struct PanelRecord {
    id: PanelId,
    generation: Generation,
    state: PanelState,
    panel_type: PanelType,
    workspace: Option<WorkspaceId>,
    view: Option<ViewId>,
    title: Option<String>,
}

/// Validated event topic: `owner.name:topic` with `^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*:[a-z][a-z0-9_.-]*$`, `<=64` bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventTopic(String);

impl EventTopic {
    pub fn parse(raw: &str) -> Result<Self, PanelError> {
        if raw.is_empty() {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        if raw.len() > 64 {
            return Err(PanelError::ResourceExhausted {
                reason: "topic exceeds 64 bytes".to_string(),
            });
        }
        let (owner_part, topic) = raw
            .split_once(':')
            .ok_or_else(|| PanelError::UnknownTopic {
                topic: raw.to_string(),
            })?;
        if topic.is_empty() || topic.len() > 32 {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        if !topic.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' || c == '.'
        }) {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        if !topic.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        let segs: Vec<&str> = owner_part.split('.').collect();
        if segs.len() != 2 {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        for seg in &segs {
            if seg.is_empty() || seg.len() > 16 {
                return Err(PanelError::UnknownTopic {
                    topic: raw.to_string(),
                });
            }
            if !seg.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
                return Err(PanelError::UnknownTopic {
                    topic: raw.to_string(),
                });
            }
            if !seg
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            {
                return Err(PanelError::UnknownTopic {
                    topic: raw.to_string(),
                });
            }
        }
        if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        // Forbid bitty.* impersonation for non-Core topics? Core topics are bitty.panel:*
        // Allow bitty.panel:* only from runtime; other bitty.* rejected
        if owner_part == "bitty" && !raw.starts_with("bitty.panel:") {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        Ok(Self(raw.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this topic is coalescable (latest-wins).
    #[must_use]
    pub fn is_coalescable(&self) -> bool {
        let s = self.0.as_str();
        s.contains("focus") || s.contains("cwd") || s.contains("title") || s.contains("file.open")
    }
}

/// Bounded payload text `<= 8KiB`, truncated or rejected at boundary.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BoundedPayload(String);

impl BoundedPayload {
    pub fn try_new(s: impl Into<String>) -> Result<Self, PanelError> {
        let raw = s.into();
        if raw.len() > BUS_EVENT_MAX_BYTES {
            return Err(PanelError::PayloadTooLarge {
                bytes: raw.len(),
                max: BUS_EVENT_MAX_BYTES,
            });
        }
        Ok(Self(raw))
    }

    pub fn new_truncated(s: &str) -> Self {
        if s.len() <= BUS_EVENT_MAX_BYTES {
            return Self(s.to_owned());
        }
        // Truncate at char boundary
        let mut end = BUS_EVENT_MAX_BYTES;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        Self(s[..end].to_owned())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// One bus event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusEvent {
    pub topic: EventTopic,
    pub payload: BoundedPayload,
    pub generation: Generation,
}

/// Drop policy for bus queues; v1 default is `DropOldest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BusDropPolicy {
    DropOldest,
    DropNewest,
}

/// Per-subscription bounded FIFO queue (64) with DropOldest/Newest.
#[derive(Debug)]
struct BusQueue {
    inner: VecDeque<BusEvent>,
    capacity: usize,
    dropped: u64,
    drop_policy: BusDropPolicy,
}

impl BusQueue {
    fn new(capacity: usize, drop_policy: BusDropPolicy) -> Self {
        Self {
            inner: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
            drop_policy,
        }
    }

    fn push(&mut self, event: BusEvent) -> bool {
        // Coalescing: if topic is coalescable and queue holds undelivered copy, replace latest
        if event.topic.is_coalescable() {
            if let Some(pos) = self.inner.iter().position(|e| e.topic == event.topic) {
                // Remove existing coalescable entry and push latest to back (latest-wins)
                self.inner.remove(pos);
                self.inner.push_back(event);
                return true;
            }
        }
        if self.inner.len() >= self.capacity {
            match self.drop_policy {
                BusDropPolicy::DropOldest => {
                    self.inner.pop_front();
                    self.dropped = self.dropped.wrapping_add(1);
                    self.inner.push_back(event);
                    true
                }
                BusDropPolicy::DropNewest => {
                    self.dropped = self.dropped.wrapping_add(1);
                    false
                }
            }
        } else {
            self.inner.push_back(event);
            true
        }
    }

    fn drain_batch(&mut self, max_events: usize, max_bytes: usize) -> Vec<BusEvent> {
        let mut out = Vec::new();
        let mut bytes = 0usize;
        while let Some(front) = self.inner.front() {
            if out.len() >= max_events {
                break;
            }
            let payload_len = front.payload.len();
            if bytes + payload_len > max_bytes && !out.is_empty() {
                break;
            }
            // If single event exceeds max_bytes, drain it only if it's the first? But spec says strict: never exceed max_bytes even for first? However for panel bus we follow same as plugin-host drain_batch strict: never exceed max_bytes even for first event; remainder stays queued.
            // So if first event alone exceeds max_bytes, we do not drain it.
            if payload_len > max_bytes {
                break;
            }
            if bytes + payload_len > max_bytes {
                break;
            }
            let Some(ev) = self.inner.pop_front() else {
                break;
            };
            bytes += payload_len;
            out.push(ev);
        }
        out
    }

    fn len(&self) -> usize {
        self.inner.len()
    }

    fn bytes(&self) -> usize {
        self.inner.iter().map(|e| e.payload.len()).sum()
    }
}

/// Panel EventBus with three-level budgets mirroring plugin-host but for panel traffic.
#[derive(Debug)]
pub struct PanelEventBus {
    queues: HashMap<(PanelId, String), BusQueue>,
    per_panel_events: HashMap<PanelId, usize>,
    per_panel_bytes: HashMap<PanelId, usize>,
    global_events: usize,
    global_bytes: usize,
    total_dropped: u64,
    drop_policy: BusDropPolicy,
    topics: HashSet<String>,
}

impl PanelEventBus {
    #[must_use]
    pub fn new(drop_policy: BusDropPolicy) -> Self {
        Self {
            queues: HashMap::new(),
            per_panel_events: HashMap::new(),
            per_panel_bytes: HashMap::new(),
            global_events: 0,
            global_bytes: 0,
            total_dropped: 0,
            drop_policy,
            topics: HashSet::new(),
        }
    }

    pub fn declare_topic(&mut self, raw: &str) -> Result<EventTopic, PanelError> {
        let topic = EventTopic::parse(raw)?;
        if self.topics.len() >= MAX_TOPICS_TOTAL && !self.topics.contains(topic.as_str()) {
            return Err(PanelError::TooManyTopics {
                max: MAX_TOPICS_TOTAL,
                current: self.topics.len(),
            });
        }
        self.topics.insert(topic.as_str().to_string());
        Ok(topic)
    }

    pub fn subscribe(&mut self, panel_id: PanelId, topic: &EventTopic) -> Result<(), PanelError> {
        if !self.topics.contains(topic.as_str()) {
            return Err(PanelError::UnknownTopic {
                topic: topic.as_str().to_string(),
            });
        }
        // Count subscriptions per panel
        let count = self
            .queues
            .keys()
            .filter(|(pid, _)| *pid == panel_id)
            .count();
        if count >= MAX_SUBSCRIPTIONS_PER_PANEL {
            return Err(PanelError::TooManySubscriptions {
                max: MAX_SUBSCRIPTIONS_PER_PANEL,
                current: count,
            });
        }
        let key = (panel_id, topic.as_str().to_string());
        self.queues
            .entry(key)
            .or_insert_with(|| BusQueue::new(BUS_PER_SUBSCRIPTION_LIMIT, self.drop_policy));
        Ok(())
    }

    /// Publish a payload to all subscribers of `topic`. Enforces per-panel
    /// 1024/256KiB and global 8192/2MiB with DropOldest via queue eviction.
    pub fn publish(
        &mut self,
        topic: &EventTopic,
        payload: BoundedPayload,
    ) -> Result<(), PanelError> {
        if payload.len() > BUS_EVENT_MAX_BYTES {
            return Err(PanelError::PayloadTooLarge {
                bytes: payload.len(),
                max: BUS_EVENT_MAX_BYTES,
            });
        }
        // Gather subscribers for topic
        let subscribers: Vec<(PanelId, String)> = self
            .queues
            .keys()
            .filter(|(_, t)| t == topic.as_str())
            .cloned()
            .collect();
        if subscribers.is_empty() {
            return Ok(());
        }
        for (panel_id, topic_str) in subscribers {
            let generation = Generation::INITIAL; // placeholder; real generation tracked per panel elsewhere
            let event = BusEvent {
                topic: topic.clone(),
                payload: payload.clone(),
                generation,
            };
            // Enforce per-panel aggregate before push: if would exceed, evict oldest across panel's queues (DropOldest)
            let per_panel_events = self.per_panel_events.get(&panel_id).copied().unwrap_or(0);
            let per_panel_bytes = self.per_panel_bytes.get(&panel_id).copied().unwrap_or(0);
            if per_panel_events >= BUS_PER_PANEL_LIMIT
                || per_panel_bytes + payload.len() > BUS_PER_PANEL_BYTES_LIMIT
            {
                match self.drop_policy {
                    BusDropPolicy::DropOldest => {
                        // Evict oldest across panel's queues
                        self.evict_oldest_for_panel(panel_id);
                    }
                    BusDropPolicy::DropNewest => {
                        // Drop new arrival for this subscriber
                        if let Some(q) = self.queues.get_mut(&(panel_id, topic_str.clone())) {
                            q.dropped = q.dropped.wrapping_add(1);
                            self.total_dropped = self.total_dropped.wrapping_add(1);
                        }
                        continue;
                    }
                }
            }
            // Enforce global before push
            if self.global_events >= BUS_GLOBAL_LIMIT
                || self.global_bytes + payload.len() > BUS_GLOBAL_BYTES_LIMIT
            {
                match self.drop_policy {
                    BusDropPolicy::DropOldest => {
                        self.evict_oldest_globally();
                    }
                    BusDropPolicy::DropNewest => {
                        if let Some(q) = self.queues.get_mut(&(panel_id, topic_str.clone())) {
                            q.dropped = q.dropped.wrapping_add(1);
                            self.total_dropped = self.total_dropped.wrapping_add(1);
                        }
                        continue;
                    }
                }
            }
            // Push to per-subscription queue
            let key = (panel_id, topic_str);
            if let Some(queue) = self.queues.get_mut(&key) {
                let before_len = queue.len();
                let before_bytes = queue.bytes();
                let pushed = queue.push(event);
                if pushed {
                    // Update aggregates
                    let delta_events = queue.len() as isize - before_len as isize;
                    let delta_bytes = queue.bytes() as isize - before_bytes as isize;
                    *self.per_panel_events.entry(panel_id).or_insert(0) =
                        (*self.per_panel_events.get(&panel_id).unwrap_or(&0) as isize
                            + delta_events) as usize;
                    *self.per_panel_bytes.entry(panel_id).or_insert(0) =
                        (*self.per_panel_bytes.get(&panel_id).unwrap_or(&0) as isize + delta_bytes)
                            as usize;
                    self.global_events = (self.global_events as isize + delta_events) as usize;
                    self.global_bytes = (self.global_bytes as isize + delta_bytes) as usize;
                    if queue.dropped > 0 && delta_events <= 0 {
                        // DropOldest evicted one, count total dropped already in queue.dropped
                        // Recompute total_dropped as sum of all queue dropped?
                        self.total_dropped = self.queues.values().map(|q| q.dropped).sum();
                    }
                } else {
                    self.total_dropped = self.queues.values().map(|q| q.dropped).sum();
                }
            }
        }
        Ok(())
    }

    fn evict_oldest_for_panel(&mut self, panel_id: PanelId) {
        // Find oldest queue entry for panel_id (first queue with earliest front)
        let mut oldest_key: Option<(PanelId, String)> = None;
        for key in self.queues.keys() {
            if key.0 == panel_id {
                oldest_key = Some(key.clone());
                break;
            }
        }
        if let Some(key) = oldest_key {
            if let Some(q) = self.queues.get_mut(&key) {
                if let Some(ev) = q.inner.pop_front() {
                    q.dropped = q.dropped.wrapping_add(1);
                    let bytes = ev.payload.len();
                    *self.per_panel_events.entry(panel_id).or_insert(1) -= 1;
                    *self.per_panel_bytes.entry(panel_id).or_insert(bytes) -= bytes;
                    self.global_events = self.global_events.saturating_sub(1);
                    self.global_bytes = self.global_bytes.saturating_sub(bytes);
                    self.total_dropped = self.total_dropped.wrapping_add(1);
                }
            }
        }
    }

    fn evict_oldest_globally(&mut self) {
        // Evict one event from any queue (first found)
        let key_opt = self.queues.keys().next().cloned();
        if let Some(key) = key_opt {
            if let Some(q) = self.queues.get_mut(&key) {
                if let Some(ev) = q.inner.pop_front() {
                    let bytes = ev.payload.len();
                    let pid = key.0;
                    *self.per_panel_events.entry(pid).or_insert(1) -= 1;
                    *self.per_panel_bytes.entry(pid).or_insert(bytes) -= bytes;
                    q.dropped = q.dropped.wrapping_add(1);
                    self.global_events = self.global_events.saturating_sub(1);
                    self.global_bytes = self.global_bytes.saturating_sub(bytes);
                    self.total_dropped = self.total_dropped.wrapping_add(1);
                }
            }
        }
    }

    pub fn drain_batch(
        &mut self,
        panel_id: PanelId,
        topic: &str,
        max_events: usize,
        max_bytes: usize,
    ) -> Vec<BusEvent> {
        let key = (panel_id, topic.to_string());
        let (events, delta_bytes, delta_events, dropped) =
            if let Some(q) = self.queues.get_mut(&key) {
                let batch = q.drain_batch(max_events, max_bytes);
                let bytes: usize = batch.iter().map(|e| e.payload.len()).sum();
                let ev_cnt = batch.len();
                let dropped = q.dropped;
                (batch, bytes, ev_cnt, dropped)
            } else {
                return Vec::new();
            };
        // Update aggregates
        if delta_events > 0 {
            if let Some(cnt) = self.per_panel_events.get_mut(&panel_id) {
                *cnt = cnt.saturating_sub(delta_events);
            }
            if let Some(cnt) = self.per_panel_bytes.get_mut(&panel_id) {
                *cnt = cnt.saturating_sub(delta_bytes);
            }
            self.global_events = self.global_events.saturating_sub(delta_events);
            self.global_bytes = self.global_bytes.saturating_sub(delta_bytes);
        }
        let _ = dropped;
        events
    }

    #[must_use]
    pub fn total_queued_events(&self) -> usize {
        self.global_events
    }

    #[must_use]
    pub fn total_queued_bytes(&self) -> usize {
        self.global_bytes
    }

    #[must_use]
    pub fn queued_events_for_panel(&self, panel_id: PanelId) -> usize {
        self.per_panel_events.get(&panel_id).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn total_dropped(&self) -> u64 {
        self.total_dropped
    }

    pub fn clear_panel(&mut self, panel_id: PanelId) {
        let keys: Vec<(PanelId, String)> = self
            .queues
            .keys()
            .filter(|(pid, _)| *pid == panel_id)
            .cloned()
            .collect();
        for key in keys {
            if let Some(q) = self.queues.remove(&key) {
                self.global_events = self.global_events.saturating_sub(q.len());
                self.global_bytes = self.global_bytes.saturating_sub(q.bytes());
                self.total_dropped = self.total_dropped.wrapping_add(q.dropped);
            }
        }
        self.per_panel_events.remove(&panel_id);
        self.per_panel_bytes.remove(&panel_id);
    }

    pub fn topics_len(&self) -> usize {
        self.topics.len()
    }
}

/// Generic Panel Runtime per window/process. One registry per window,
/// single-process winit, holds no PTY fd, GPU object, or OS window handle
/// (those remain with `bitty-pty`, `bitty-render`, `bitty-platform`).
pub struct PanelRegistry {
    registry_generation: Generation,
    next_panel_raw: u64,
    config: PanelRegistryConfig,
    panels: HashMap<u64, PanelRecord>,
    panel_to_view: HashMap<PanelId, ViewId>,
    view_to_panel: HashMap<ViewId, PanelId>,
    workspace_panels: HashMap<WorkspaceId, Vec<PanelId>>,
    focus_per_workspace: HashMap<WorkspaceId, bitty_ui::panel::PanelFocus>,
    active_workspace: Option<WorkspaceId>,
    command_registry: UiCommandRegistry,
    overlay_manager: UiOverlayManager,
    event_bus: PanelEventBus,
    capabilities: HashMap<(PanelId, Generation), BTreeSet<String>>,
    errors: HashMap<String, u64>,
    disposed: bool,
    total_created: u64,
}

impl std::fmt::Debug for PanelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelRegistry")
            .field("generation", &self.registry_generation)
            .field("panels_active", &self.panels.len())
            .field("total_created", &self.total_created)
            .field("disposed", &self.disposed)
            .finish_non_exhaustive()
    }
}

impl PanelRegistry {
    /// Creates a panel registry after `PanelRegistryConfig` validation.
    ///
    /// # Errors
    /// `ResourceExhausted` for bad bounds, `GenerationExhausted` if reserved.
    pub fn new(config: PanelRegistryConfig) -> Result<Self, PanelError> {
        config.validate()?;
        if Generation::INITIAL.is_exhausted() {
            return Err(PanelError::GenerationExhausted {
                current: Generation::INITIAL,
            });
        }
        Ok(Self {
            registry_generation: Generation::INITIAL,
            next_panel_raw: 1,
            config,
            panels: HashMap::new(),
            panel_to_view: HashMap::new(),
            view_to_panel: HashMap::new(),
            workspace_panels: HashMap::new(),
            focus_per_workspace: HashMap::new(),
            active_workspace: None,
            command_registry: UiCommandRegistry::new(),
            overlay_manager: UiOverlayManager::new(),
            event_bus: PanelEventBus::new(BusDropPolicy::DropOldest),
            capabilities: HashMap::new(),
            errors: HashMap::new(),
            disposed: false,
            total_created: 0,
        })
    }

    fn ensure_not_disposed(&self) -> Result<(), PanelError> {
        if self.disposed {
            return Err(PanelError::RegistryDisposed {
                generation: self.registry_generation,
            });
        }
        Ok(())
    }

    fn bump_error(&mut self, variant: &str) {
        *self.errors.entry(variant.to_owned()).or_insert(0) += 1;
    }

    #[must_use]
    pub fn generation(&self) -> Generation {
        self.registry_generation
    }

    #[must_use]
    pub fn panel_count(&self) -> usize {
        self.panels.len()
    }

    #[must_use]
    pub fn config(&self) -> &PanelRegistryConfig {
        &self.config
    }

    /// Validates `(id, generation)` before returning a reference.
    fn get_panel(&self, id: PanelId, generation: Generation) -> Result<&PanelRecord, PanelError> {
        self.ensure_not_disposed()?;
        let rec = self.panels.get(&id.0).ok_or(PanelError::NotFound {
            kind: "panel",
            id_raw: id.0,
        })?;
        if rec.generation != generation {
            return Err(PanelError::StaleHandle {
                expected_generation: rec.generation,
                found_generation: generation,
                id_raw: id.0,
            });
        }
        if rec.state == UiPanelState::Disposed {
            return Err(PanelError::NotFound {
                kind: "panel",
                id_raw: id.0,
            });
        }
        Ok(rec)
    }

    fn get_panel_mut(
        &mut self,
        id: PanelId,
        generation: Generation,
    ) -> Result<&mut PanelRecord, PanelError> {
        self.ensure_not_disposed()?;
        let current_gen = {
            let rec = self.panels.get(&id.0).ok_or(PanelError::NotFound {
                kind: "panel",
                id_raw: id.0,
            })?;
            rec.generation
        };
        if current_gen != generation {
            return Err(PanelError::StaleHandle {
                expected_generation: current_gen,
                found_generation: generation,
                id_raw: id.0,
            });
        }
        let rec = self.panels.get_mut(&id.0).ok_or(PanelError::NotFound {
            kind: "panel",
            id_raw: id.0,
        })?;
        if rec.state == UiPanelState::Disposed {
            return Err(PanelError::NotFound {
                kind: "panel",
                id_raw: id.0,
            });
        }
        Ok(rec)
    }

    /// Creates a panel of `panel_type`. Validates closed type set and
    /// `max_panels_per_workspace` / `max_panels_per_window` before allocation.
    ///
    /// # Errors
    /// `TooManyPanels`, `UnknownPanelType`, `GenerationExhausted`, `RegistryDisposed`.
    pub fn create_panel(
        &mut self,
        panel_type: PanelType,
        workspace: Option<WorkspaceId>,
    ) -> Result<PanelHandle, PanelError> {
        self.ensure_not_disposed()?;
        if self.registry_generation.is_exhausted() {
            self.bump_error("GenerationExhausted");
            return Err(PanelError::GenerationExhausted {
                current: self.registry_generation,
            });
        }
        if self.panels.len() >= self.config.max_panels_per_window {
            self.bump_error("TooManyPanels");
            return Err(PanelError::TooManyPanels {
                max: self.config.max_panels_per_window,
                current: self.panels.len(),
            });
        }
        if let Some(ws) = workspace {
            let count = self.workspace_panels.get(&ws).map_or(0, |v| v.len());
            if count >= self.config.max_panels_per_workspace {
                self.bump_error("TooManyPanels");
                return Err(PanelError::TooManyPanels {
                    max: self.config.max_panels_per_workspace,
                    current: count,
                });
            }
        }
        let next_gen =
            self.registry_generation
                .next()
                .map_err(|_| PanelError::GenerationExhausted {
                    current: self.registry_generation,
                })?;
        self.registry_generation = next_gen;
        let pid = PanelId::new(self.next_panel_raw);
        self.next_panel_raw = self.next_panel_raw.wrapping_add(1).max(1);
        let gen_val = self.registry_generation;
        let rec = PanelRecord {
            id: pid,
            generation: gen_val,
            state: UiPanelState::Created,
            panel_type,
            workspace,
            view: None,
            title: None,
        };
        self.panels.insert(pid.0, rec);
        if let Some(ws) = workspace {
            self.workspace_panels.entry(ws).or_default().push(pid);
            self.focus_per_workspace.entry(ws).or_default();
            if self.active_workspace.is_none() {
                self.active_workspace = Some(ws);
            }
        }
        self.total_created += 1;
        Ok(PanelHandle {
            id: pid,
            generation: gen_val,
        })
    }

    /// Creates a panel from a string type name; validates closed set.
    pub fn create_panel_by_type_str(
        &mut self,
        type_str: &str,
        workspace: Option<WorkspaceId>,
    ) -> Result<PanelHandle, PanelError> {
        let pt = UiPanelType::parse(type_str).ok_or_else(|| PanelError::UnknownPanelType {
            value: type_str.to_string(),
        })?;
        self.create_panel(pt, workspace)
    }

    /// Returns panel state for a handle.
    pub fn panel_state(
        &self,
        id: PanelId,
        generation: Generation,
    ) -> Result<PanelState, PanelError> {
        Ok(self.get_panel(id, generation)?.state)
    }

    /// Mounts `panel` to an empty `ViewId`. Validates handles, single-owner
    /// mapping, and transitions `Created -> Mounted`.
    ///
    /// # Errors
    /// `StaleHandle`, `AlreadyMounted`, `PanelAlreadyMounted`, `InvalidState`.
    pub fn mount_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        view_id: ViewId,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        if self.panel_to_view.contains_key(&panel_id) {
            let cur = self.panel_to_view[&panel_id];
            return Err(PanelError::PanelAlreadyMounted {
                panel_id,
                current_view: cur,
            });
        }
        if self.view_to_panel.contains_key(&view_id) {
            let existing = self.view_to_panel[&view_id];
            let content = ViewContent::Panel(existing);
            return Err(PanelError::AlreadyMounted {
                view_id,
                existing: content,
            });
        }
        let rec = self.get_panel(panel_id, generation)?;
        if !matches!(rec.state, UiPanelState::Created | UiPanelState::Suspended) {
            return Err(PanelError::InvalidState {
                current: rec.state,
                expected: "Created or Suspended",
            });
        }
        // Commit mount
        self.view_to_panel.insert(view_id, panel_id);
        self.panel_to_view.insert(panel_id, view_id);
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = UiPanelState::Mounted;
        rec_mut.view = Some(view_id);
        Ok(())
    }

    /// Unmounts a panel, preserving its identity and generation, transitioning
    /// `Mounted`/`Focused`/`Suspended` -> `Suspended` (retain) or `Created`.
    pub fn unmount_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
    ) -> Result<ViewId, PanelError> {
        self.ensure_not_disposed()?;
        let workspace = {
            let rec = self.get_panel(panel_id, generation)?;
            if !matches!(
                rec.state,
                UiPanelState::Mounted | UiPanelState::Focused | UiPanelState::Suspended
            ) {
                return Err(PanelError::InvalidState {
                    current: rec.state,
                    expected: "Mounted/Focused/Suspended",
                });
            }
            rec.workspace
        };
        let view_id = self
            .panel_to_view
            .remove(&panel_id)
            .ok_or(PanelError::NotFound {
                kind: "panel view",
                id_raw: panel_id.0,
            })?;
        self.view_to_panel.remove(&view_id);
        // Focus MRU update if focused
        if let Some(ws) = workspace {
            if let Some(focus) = self.focus_per_workspace.get_mut(&ws) {
                focus.on_panel_hidden(panel_id);
            }
        }
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = UiPanelState::Suspended;
        rec_mut.view = None;
        Ok(view_id)
    }

    /// Focuses a mounted panel within its workspace, moving to MRU front and
    /// transitioning `Mounted -> Focused`. Hidden panels cannot be focused.
    pub fn focus_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        workspace: WorkspaceId,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        let rec = self.get_panel(panel_id, generation)?;
        if rec.state != UiPanelState::Mounted && rec.state != UiPanelState::Focused {
            return Err(PanelError::InvalidState {
                current: rec.state,
                expected: "Mounted or Focused",
            });
        }
        if rec.workspace != Some(workspace) && rec.workspace.is_some() {
            // Allow focus only within its workspace; if panel has workspace binding, enforce it
            return Err(PanelError::NotFound {
                kind: "workspace",
                id_raw: workspace.0,
            });
        }
        // Update MRU
        let focus = self.focus_per_workspace.entry(workspace).or_default();
        focus.set(panel_id);
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = UiPanelState::Focused;
        Ok(())
    }

    /// Suspends a focused or mounted panel (invisible without destroying attachment).
    pub fn suspend_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        let rec = self.get_panel(panel_id, generation)?;
        if rec.state != UiPanelState::Mounted && rec.state != UiPanelState::Focused {
            return Err(PanelError::InvalidState {
                current: rec.state,
                expected: "Mounted or Focused",
            });
        }
        let ws = rec.workspace;
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = UiPanelState::Suspended;
        if let Some(w) = ws {
            if let Some(focus) = self.focus_per_workspace.get_mut(&w) {
                focus.on_panel_hidden(panel_id);
            }
        }
        Ok(())
    }

    /// Resumes a suspended panel back to mounted.
    pub fn resume_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        let rec = self.get_panel(panel_id, generation)?;
        if rec.state != UiPanelState::Suspended {
            return Err(PanelError::InvalidState {
                current: rec.state,
                expected: "Suspended",
            });
        }
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = UiPanelState::Mounted;
        Ok(())
    }

    /// Disposes a panel, clearing its view attachment, MRU, capabilities,
    /// event queues, and retiring `(PanelId, Generation)`.
    pub fn dispose_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        let (stored_gen, workspace) = {
            let rec = self.panels.get(&panel_id.0).ok_or(PanelError::NotFound {
                kind: "panel",
                id_raw: panel_id.0,
            })?;
            (rec.generation, rec.workspace)
        };
        if stored_gen != generation {
            self.bump_error("StaleHandle");
            return Err(PanelError::StaleHandle {
                expected_generation: stored_gen,
                found_generation: generation,
                id_raw: panel_id.0,
            });
        }
        // Remove view attachment if any
        if let Some(view_id) = self.panel_to_view.remove(&panel_id) {
            self.view_to_panel.remove(&view_id);
        }
        // Remove from workspace list and MRU
        if let Some(ws) = workspace {
            if let Some(list) = self.workspace_panels.get_mut(&ws) {
                list.retain(|&id| id != panel_id);
            }
            if let Some(focus) = self.focus_per_workspace.get_mut(&ws) {
                focus.on_panel_hidden(panel_id);
            }
        }
        // Clear commands, bus queues, capabilities
        self.command_registry.unregister_panel(panel_id);
        self.event_bus.clear_panel(panel_id);
        self.capabilities.remove(&(panel_id, generation));
        // Retire panel with generation bump
        if let Ok(next) = self.registry_generation.next() {
            self.registry_generation = next;
        }
        self.panels.remove(&panel_id.0);
        Ok(())
    }

    /// Returns focused panel for a workspace.
    #[must_use]
    pub fn focused_panel(&self, workspace: WorkspaceId) -> Option<PanelId> {
        self.focus_per_workspace
            .get(&workspace)
            .and_then(|f| f.focused())
    }

    #[must_use]
    pub fn mru_order(&self, workspace: WorkspaceId) -> Vec<PanelId> {
        self.focus_per_workspace
            .get(&workspace)
            .map(|f| f.mru_order())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------
    // Command registry (owner.name:command)
    // ------------------------------------------------------------------

    pub fn register_command(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        raw: &str,
    ) -> Result<QualifiedCommand, PanelError> {
        self.ensure_not_disposed()?;
        self.get_panel(panel_id, generation)?;
        self.command_registry
            .register(panel_id, raw)
            .map_err(|e| match e {
                bitty_ui::panel::CommandError::Duplicate { command, owner } => {
                    PanelError::DuplicateCommand { command, owner }
                }
                bitty_ui::panel::CommandError::TooManyCommands { max, current } => {
                    PanelError::TooManyCommands { max, current }
                }
                bitty_ui::panel::CommandError::Invalid(msg) => {
                    PanelError::InvalidCommand { reason: msg }
                }
            })
    }

    #[must_use]
    pub fn command_owner(&self, command: &str) -> Option<PanelId> {
        self.command_registry.owner_of(command)
    }

    // ------------------------------------------------------------------
    // Overlay (4+1)
    // ------------------------------------------------------------------

    pub fn create_overlay(
        &mut self,
        kind: UiOverlayKind,
        bounds: UiRect,
        text: impl Into<String>,
        tooltip: Option<String>,
    ) -> Result<u64, PanelError> {
        self.ensure_not_disposed()?;
        self.overlay_manager
            .create_overlay(kind, bounds, text, tooltip, self.registry_generation.get())
            .map_err(|e| match e {
                OverlayError::OverlayBusy => PanelError::OverlayBusy,
                OverlayError::TooManyOverlays { max, current } => {
                    PanelError::TooManyOverlays { max, current }
                }
            })
    }

    pub fn dismiss_overlay(&mut self, id: u64) -> Option<Overlay> {
        self.overlay_manager.dismiss(id)
    }

    #[must_use]
    pub fn overlay_len(&self) -> usize {
        self.overlay_manager.len()
    }

    #[must_use]
    pub fn overlay_modal_active(&self) -> bool {
        self.overlay_manager.modal_active()
    }

    // ------------------------------------------------------------------
    // EventBus
    // ------------------------------------------------------------------

    pub fn declare_topic(&mut self, raw: &str) -> Result<EventTopic, PanelError> {
        self.ensure_not_disposed()?;
        self.event_bus.declare_topic(raw)
    }

    pub fn subscribe(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        topic: &EventTopic,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        self.get_panel(panel_id, generation)?;
        self.event_bus.subscribe(panel_id, topic)
    }

    pub fn publish(
        &mut self,
        topic: &EventTopic,
        payload: BoundedPayload,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        self.event_bus.publish(topic, payload)
    }

    pub fn drain_batch(
        &mut self,
        panel_id: PanelId,
        topic: &str,
        max_events: usize,
        max_bytes: usize,
    ) -> Vec<BusEvent> {
        self.event_bus
            .drain_batch(panel_id, topic, max_events, max_bytes)
    }

    #[must_use]
    pub fn bus_total_events(&self) -> usize {
        self.event_bus.total_queued_events()
    }

    #[must_use]
    pub fn bus_total_bytes(&self) -> usize {
        self.event_bus.total_queued_bytes()
    }

    #[must_use]
    pub fn bus_events_for_panel(&self, panel_id: PanelId) -> usize {
        self.event_bus.queued_events_for_panel(panel_id)
    }

    #[must_use]
    pub fn bus_total_dropped(&self) -> u64 {
        self.event_bus.total_dropped()
    }

    // ------------------------------------------------------------------
    // Capability isolation per (PanelId, generation) — panel.*
    // ------------------------------------------------------------------

    /// Grants a `panel.*` capability to a panel handle. Validates closed set.
    pub fn grant_panel_capability(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        capability: &str,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        self.get_panel(panel_id, generation)?;
        // Validate capability via plugin-host grammar (reuse)
        let cid = bitty_plugin_host::CapabilityId::parse(capability).map_err(|e| {
            PanelError::CapabilityDenied {
                panel_id,
                capability: format!("{capability}: {e}"),
            }
        })?;
        if cid.family() != bitty_plugin_host::CapabilityFamily::Panel {
            return Err(PanelError::CapabilityDenied {
                panel_id,
                capability: capability.to_string(),
            });
        }
        self.capabilities
            .entry((panel_id, generation))
            .or_default()
            .insert(capability.to_string());
        Ok(())
    }

    #[must_use]
    pub fn is_panel_capability_granted(
        &self,
        panel_id: PanelId,
        generation: Generation,
        capability: &str,
    ) -> bool {
        self.capabilities
            .get(&(panel_id, generation))
            .is_some_and(|set| set.contains(capability))
    }

    /// Checks capability and returns error if not granted (deny-by-default).
    pub fn require_panel_capability(
        &self,
        panel_id: PanelId,
        generation: Generation,
        capability: &str,
    ) -> Result<(), PanelError> {
        if self.is_panel_capability_granted(panel_id, generation, capability) {
            Ok(())
        } else {
            Err(PanelError::CapabilityDenied {
                panel_id,
                capability: capability.to_string(),
            })
        }
    }

    // ------------------------------------------------------------------
    // Disposal of whole registry
    // ------------------------------------------------------------------

    pub fn dispose(&mut self) {
        if self.disposed {
            return;
        }
        self.panels.clear();
        self.panel_to_view.clear();
        self.view_to_panel.clear();
        self.workspace_panels.clear();
        self.focus_per_workspace.clear();
        self.command_registry = UiCommandRegistry::new();
        self.overlay_manager.clear();
        // Bus cleared via dropping? Recreate
        self.event_bus = PanelEventBus::new(BusDropPolicy::DropOldest);
        self.capabilities.clear();
        if let Ok(next) = self.registry_generation.next() {
            self.registry_generation = next;
        } else {
            self.registry_generation = Generation(u64::MAX);
        }
        self.disposed = true;
    }

    #[must_use]
    pub fn is_disposed(&self) -> bool {
        self.disposed
    }

    #[must_use]
    pub fn error_count(&self, variant: &str) -> u64 {
        self.errors.get(variant).copied().unwrap_or(0)
    }

    #[cfg(test)]
    pub fn set_generation_for_test(&mut self, generation: Generation) {
        self.registry_generation = generation;
    }

    #[must_use]
    pub fn total_created(&self) -> u64 {
        self.total_created
    }
}
