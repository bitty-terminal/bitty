//! Host-owned RC-11 plugin persistent store (`bitty.store` quota backend).
//!
//! This module is the quota-enforcing backend behind `bitty.store.set` /
//! `bitty.store.get` (see [`crate::host::HostServices`]): one [`PluginStore`]
//! per plugin identity, owned by the host, with no eviction path.
//!
//! # Budgets (RC-11)
//!
//! | ID    | Dimension              | Bound    | Enforcement |
//! |-------|------------------------|----------|-------------|
//! | RC-11 | persisted bytes total  | 256 KiB  | [`STORE_MAX_TOTAL_BYTES`] |
//! | RC-11 | bytes per value        | 8 KiB    | [`STORE_MAX_VALUE_BYTES`] |
//! | RC-11 | depth per value        | 8        | [`STORE_MAX_DEPTH`] |
//! | RC-11 | nodes persisted total  | 1024     | [`STORE_MAX_NODES`] |
//!
//! Every ceiling refuses fail-closed with typed [`STORE_QUOTA_CODE`]
//! (`E_STORE_QUOTA`, class `budget`); the previous persisted state is left
//! intact (no partial write) and nothing is ever evicted to make room.
//!
//! # Accounting (exact, deterministic)
//!
//! - **Value bytes** ([`value_bytes`]): sum of UTF-8 string payload bytes in
//!   the value, including nested table keys. Scalars other than strings
//!   contribute zero, mirroring [`crate::host::MarshallingLimits`] byte
//!   accounting; structural bulk is bounded by the node ceiling instead.
//! - **Entry size**: `key.len()` plus [`value_bytes`]. The top-level key is
//!   persisted data, so it counts toward the 256 KiB total but never toward
//!   the 8 KiB per-value ceiling.
//! - **Nodes** ([`value_nodes`]): one per [`LuaValue`](crate::host::LuaValue)
//!   node (scalars, tables, and every table key). Entry nodes are `1` for the
//!   top-level key plus the value nodes. Single-value trees always carry an
//!   odd node count (every non-root node belongs to exactly one key/value
//!   pair), so the even 1024 total is reachable only across entries.
//! - **Depth** ([`value_depth`]): scalars and empty tables are `0`; a
//!   non-empty table is one plus the deepest child. A chain of 8 nested
//!   tables has depth 8 and is accepted; 9 nested tables are refused.
//!
//! The bridge already marshals `store.set` arguments under the default
//! [`crate::host::MarshallingLimits`] (depth 8, 1024 nodes, 8 KiB), so this
//! module re-validates defence-in-depth and additionally owns the 256 KiB
//! persisted total the bridge cannot see.
//!
//! This module performs no I/O: durability reduces to the host keeping the
//! owning [`PluginStore`] alive across generations. There is no expiry, no
//! LRU, and no other eviction path by construction.

use std::collections::BTreeMap;

use crate::host::{BridgeError, LuaValue};

/// RC-11 persisted-bytes ceiling per plugin (`256 KiB`).
pub const STORE_MAX_TOTAL_BYTES: usize = 256 * 1024;

/// RC-11 per-value byte ceiling (`8 KiB`, string payload per [`value_bytes`]).
pub const STORE_MAX_VALUE_BYTES: usize = 8 * 1024;

/// RC-11 per-value depth ceiling (see [`value_depth`]).
pub const STORE_MAX_DEPTH: usize = 8;

/// RC-11 persisted-node ceiling across all entries (keys included).
pub const STORE_MAX_NODES: usize = 1024;

/// Typed refusal code for every RC-11 quota denial.
pub const STORE_QUOTA_CODE: &str = "E_STORE_QUOTA";

/// Build the typed `E_STORE_QUOTA` refusal (class `budget`).
///
/// Messages carry sizes only, never untrusted key or value content.
#[must_use]
pub fn store_quota_error(detail: impl Into<String>) -> BridgeError {
    BridgeError::new("budget", STORE_QUOTA_CODE, detail.into())
}

/// String-payload bytes of a value (recursive, keys included).
///
/// Scalars other than strings contribute zero; see the module accounting
/// section for why structural bulk is left to the node ceiling.
#[must_use]
pub fn value_bytes(value: &LuaValue) -> usize {
    match value {
        LuaValue::Nil | LuaValue::Bool(_) | LuaValue::Integer(_) | LuaValue::Number(_) => 0,
        LuaValue::String(text) => text.len(),
        LuaValue::Table(pairs) => {
            let mut total: usize = 0;
            for (key, child) in pairs {
                total = total
                    .saturating_add(value_bytes(key))
                    .saturating_add(value_bytes(child));
            }
            total
        }
    }
}

/// Node count of a value: one per scalar, table, and table key.
#[must_use]
pub fn value_nodes(value: &LuaValue) -> usize {
    match value {
        LuaValue::Nil
        | LuaValue::Bool(_)
        | LuaValue::Integer(_)
        | LuaValue::Number(_)
        | LuaValue::String(_) => 1,
        LuaValue::Table(pairs) => {
            let mut total: usize = 1;
            for (key, child) in pairs {
                total = total
                    .saturating_add(value_nodes(key))
                    .saturating_add(value_nodes(child));
            }
            total
        }
    }
}

/// Nesting depth of a value: scalars and empty tables are `0`, a non-empty
/// table is one plus its deepest child.
#[must_use]
pub fn value_depth(value: &LuaValue) -> usize {
    match value {
        LuaValue::Table(pairs) if !pairs.is_empty() => {
            let mut deepest: usize = 0;
            for (key, child) in pairs {
                deepest = deepest.max(value_depth(key).max(value_depth(child)));
            }
            deepest.saturating_add(1)
        }
        _ => 0,
    }
}

/// Validate one value against the per-value RC-11 ceilings.
///
/// # Errors
///
/// Returns typed [`STORE_QUOTA_CODE`] when depth, node count, or byte size
/// exceeds its ceiling. Pure: inspects without mutating.
fn validate_value(value: &LuaValue) -> Result<(), BridgeError> {
    let depth = value_depth(value);
    if depth > STORE_MAX_DEPTH {
        return Err(store_quota_error(format!(
            "store value depth {depth} exceeds limit {STORE_MAX_DEPTH}"
        )));
    }
    let nodes = value_nodes(value);
    if nodes > STORE_MAX_NODES {
        return Err(store_quota_error(format!(
            "store value nodes {nodes} exceed limit {STORE_MAX_NODES}"
        )));
    }
    let bytes = value_bytes(value);
    if bytes > STORE_MAX_VALUE_BYTES {
        return Err(store_quota_error(format!(
            "store value size {bytes} exceeds limit {STORE_MAX_VALUE_BYTES}"
        )));
    }
    Ok(())
}

/// Host-owned persistent store for one plugin identity (RC-11).
///
/// Entries are insertion-ordered by key (`BTreeMap`) so iteration and
/// persistence stay deterministic. All writes validate fully before
/// mutating, so a denied write leaves the previous state intact. Setting a
/// key to [`LuaValue::Nil`] deletes it (Lua `t[k] = nil` semantics) and
/// always succeeds, freeing quota.
#[derive(Debug, Clone, Default)]
pub struct PluginStore {
    entries: BTreeMap<String, LuaValue>,
}

impl PluginStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Read an entry; `None` means absent.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&LuaValue> {
        self.entries.get(key)
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Persisted bytes across all entries (`key.len() + value bytes`).
    #[must_use]
    pub fn persisted_bytes(&self) -> usize {
        self.totals().0
    }

    /// Persisted nodes across all entries (one per key plus value nodes).
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.totals().1
    }

    /// Current `(bytes, nodes)` totals.
    fn totals(&self) -> (usize, usize) {
        let mut bytes: usize = 0;
        let mut nodes: usize = 0;
        for (key, value) in &self.entries {
            bytes = bytes
                .saturating_add(key.len())
                .saturating_add(value_bytes(value));
            nodes = nodes.saturating_add(1).saturating_add(value_nodes(value));
        }
        (bytes, nodes)
    }

    /// Atomically write an entry, or delete it when `value` is
    /// [`LuaValue::Nil`].
    ///
    /// Validation (per-value ceilings, then persisted totals) completes
    /// before any mutation: a denial returns typed [`STORE_QUOTA_CODE`] and
    /// the previous entry — if any — is untouched. Nothing is evicted to
    /// make room; the caller must delete entries explicitly.
    ///
    /// # Errors
    ///
    /// Returns typed [`STORE_QUOTA_CODE`] when the value or the resulting
    /// persisted totals would exceed an RC-11 ceiling.
    pub fn set(&mut self, key: String, value: LuaValue) -> Result<(), BridgeError> {
        if matches!(value, LuaValue::Nil) {
            self.entries.remove(&key);
            return Ok(());
        }
        validate_value(&value)?;
        let (total_bytes, total_nodes) = self.totals();
        let replaced_bytes = self
            .entries
            .get(&key)
            .map(|old| key.len().saturating_add(value_bytes(old)))
            .unwrap_or(0);
        let replaced_nodes = self
            .entries
            .get(&key)
            .map(|old| 1_usize.saturating_add(value_nodes(old)))
            .unwrap_or(0);
        let next_bytes = total_bytes
            .saturating_sub(replaced_bytes)
            .saturating_add(key.len())
            .saturating_add(value_bytes(&value));
        let next_nodes = total_nodes
            .saturating_sub(replaced_nodes)
            .saturating_add(1)
            .saturating_add(value_nodes(&value));
        if next_bytes > STORE_MAX_TOTAL_BYTES {
            return Err(store_quota_error(format!(
                "store total {next_bytes} exceeds limit {STORE_MAX_TOTAL_BYTES}"
            )));
        }
        if next_nodes > STORE_MAX_NODES {
            return Err(store_quota_error(format!(
                "store nodes {next_nodes} exceed limit {STORE_MAX_NODES}"
            )));
        }
        self.entries.insert(key, value);
        Ok(())
    }

    /// Delete an entry, returning the previous value when present.
    pub fn remove(&mut self, key: &str) -> Option<LuaValue> {
        self.entries.remove(key)
    }
}
