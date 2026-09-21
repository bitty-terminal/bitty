//! RC-11 plugin persistent store quota tests (RUN-25, CTX-0596).
//!
//! Exact-boundary coverage for all four ceilings (256 KiB total, 8 KiB per
//! value, depth 8, 1024 nodes) plus denial atomicity: every over-quota write
//! fails with typed `E_STORE_QUOTA` and leaves the previous persisted state
//! intact, with no eviction.

use bitty_lua::host::LuaValue;
use bitty_lua::store::{
    PluginStore, STORE_MAX_DEPTH, STORE_MAX_NODES, STORE_MAX_TOTAL_BYTES, STORE_MAX_VALUE_BYTES,
    STORE_QUOTA_CODE, value_bytes, value_depth, value_nodes,
};

fn assert_quota_denied(result: Result<(), bitty_lua::host::BridgeError>) {
    match result {
        Ok(()) => panic!("over-quota write must be refused"),
        Err(error) => {
            assert_eq!(error.code, STORE_QUOTA_CODE, "typed refusal");
            assert_eq!(error.class, "budget", "quota is a budget diagnostic");
        }
    }
}

fn string_value(n: usize) -> LuaValue {
    LuaValue::String("x".repeat(n))
}

fn int_array(n: usize) -> LuaValue {
    LuaValue::array((1..=n as i64).map(LuaValue::Integer).collect())
}

/// Chain of `tables` nested single-key tables around an integer leaf.
fn nested(tables: usize) -> LuaValue {
    let mut value = LuaValue::Integer(1);
    for _ in 0..tables {
        value = LuaValue::Table(vec![(LuaValue::String("k".to_string()), value)]);
    }
    value
}

#[test]
fn per_value_exactly_8kib_accepted() {
    assert_eq!(STORE_MAX_VALUE_BYTES, 8 * 1024);
    let value = string_value(STORE_MAX_VALUE_BYTES);
    assert_eq!(value_bytes(&value), STORE_MAX_VALUE_BYTES);
    let mut store = PluginStore::new();
    store
        .set("a".to_string(), value)
        .expect("exact ceiling fits");
    assert_eq!(store.persisted_bytes(), 1 + STORE_MAX_VALUE_BYTES);
}

#[test]
fn per_value_one_byte_over_denied_without_state_change() {
    let mut store = PluginStore::new();
    store
        .set("keep".to_string(), LuaValue::Integer(7))
        .expect("setup");
    let before_bytes = store.persisted_bytes();
    let result = store.set("big".to_string(), string_value(STORE_MAX_VALUE_BYTES + 1));
    assert_quota_denied(result);
    assert_eq!(store.len(), 1, "no entry added on denial");
    assert_eq!(store.get("keep"), Some(&LuaValue::Integer(7)));
    assert_eq!(store.persisted_bytes(), before_bytes);
    assert!(store.get("big").is_none());
}

#[test]
fn total_exactly_256kib_accepted() {
    assert_eq!(STORE_MAX_TOTAL_BYTES, 256 * 1024);
    // 32 entries x (1-byte key + 8191-byte value) = exactly 256 KiB.
    let mut store = PluginStore::new();
    let keys: Vec<String> = ('a'..='z')
        .map(|c| c.to_string())
        .chain(('A'..='F').map(|c| c.to_string()))
        .collect();
    assert_eq!(keys.len(), 32);
    for key in &keys {
        store
            .set(key.clone(), string_value(8191))
            .expect("fill to exact ceiling");
    }
    assert_eq!(store.len(), 32);
    assert_eq!(store.persisted_bytes(), STORE_MAX_TOTAL_BYTES);
}

#[test]
fn total_one_entry_over_denied_without_eviction() {
    let mut store = PluginStore::new();
    let keys: Vec<String> = ('a'..='z')
        .map(|c| c.to_string())
        .chain(('A'..='F').map(|c| c.to_string()))
        .collect();
    for key in &keys {
        store.set(key.clone(), string_value(8191)).expect("fill");
    }
    let result = store.set("overflow".to_string(), LuaValue::Bool(true));
    assert_quota_denied(result);
    assert_eq!(store.len(), 32, "denial must not evict");
    assert_eq!(store.persisted_bytes(), STORE_MAX_TOTAL_BYTES);
    for key in &keys {
        assert!(store.get(key).is_some(), "entry {key} survives denial");
    }
}

#[test]
fn overwrite_denied_leaves_previous_value_intact() {
    let mut store = PluginStore::new();
    store
        .set("k".to_string(), string_value(8191))
        .expect("setup");
    // Fill the remaining budget exactly: 31 entries x (3-byte key +
    // 8189-byte value); the store then holds exactly 256 KiB.
    for i in 0..31 {
        store
            .set(format!("e{i:02}"), string_value(8189))
            .expect("fill to exact ceiling");
    }
    assert_eq!(store.persisted_bytes(), STORE_MAX_TOTAL_BYTES);
    // Growing the existing entry by one byte no longer fits.
    let result = store.set("k".to_string(), string_value(8192));
    assert_quota_denied(result);
    assert_eq!(store.get("k"), Some(&string_value(8191)));
    assert_eq!(store.persisted_bytes(), STORE_MAX_TOTAL_BYTES);
}

#[test]
fn depth_exactly_8_accepted_9_denied() {
    assert_eq!(STORE_MAX_DEPTH, 8);
    assert_eq!(value_depth(&nested(8)), 8);
    assert_eq!(value_depth(&nested(9)), 9);
    let mut store = PluginStore::new();
    store
        .set("d8".to_string(), nested(8))
        .expect("depth 8 fits");
    let before = store.persisted_bytes();
    assert_quota_denied(store.set("d9".to_string(), nested(9)));
    assert!(store.get("d9").is_none());
    assert_eq!(store.persisted_bytes(), before);
}

#[test]
fn nodes_total_exactly_1024_accepted() {
    assert_eq!(STORE_MAX_NODES, 1024);
    // Array of 255 integers: 1 table + 255 keys + 255 values = 511 nodes;
    // plus the entry key = 512 nodes per entry; two entries = exactly 1024.
    let value = int_array(255);
    assert_eq!(value_nodes(&value), 511);
    let mut store = PluginStore::new();
    store
        .set("a".to_string(), value.clone())
        .expect("first half");
    store
        .set("b".to_string(), value)
        .expect("exact node ceiling");
    assert_eq!(store.node_count(), 1024);
}

#[test]
fn nodes_one_entry_over_denied_without_state_change() {
    let value = int_array(255);
    let mut store = PluginStore::new();
    store.set("a".to_string(), value.clone()).expect("setup");
    store.set("b".to_string(), value).expect("setup");
    assert_eq!(store.node_count(), 1024);
    assert_quota_denied(store.set("c".to_string(), LuaValue::Bool(true)));
    assert_eq!(store.node_count(), 1024);
    assert_eq!(store.len(), 2);
    // A single 1023-node value alone still fits its per-value ceiling.
    let mut solo = PluginStore::new();
    solo.set("solo".to_string(), int_array(511))
        .expect("1023 nodes fit");
    assert_eq!(solo.node_count(), 1 + 1023);
}

#[test]
fn nil_set_deletes_and_frees_quota() {
    let mut store = PluginStore::new();
    store
        .set("gone".to_string(), string_value(100))
        .expect("setup");
    assert!(!store.is_empty());
    store
        .set("gone".to_string(), LuaValue::Nil)
        .expect("delete always succeeds");
    assert!(store.get("gone").is_none());
    assert_eq!(store.persisted_bytes(), 0);
    assert_eq!(store.node_count(), 0);
    assert!(store.is_empty());
    // Freed quota is reusable.
    store
        .set("back".to_string(), string_value(STORE_MAX_VALUE_BYTES))
        .expect("freed quota reusable");
}

#[test]
fn empty_store_and_missing_key() {
    let store = PluginStore::new();
    assert!(store.is_empty());
    assert_eq!(store.len(), 0);
    assert_eq!(store.persisted_bytes(), 0);
    assert_eq!(store.node_count(), 0);
    assert!(store.get("absent").is_none());
}

#[test]
fn remove_returns_previous_value() {
    let mut store = PluginStore::new();
    store
        .set("k".to_string(), LuaValue::Integer(3))
        .expect("setup");
    assert_eq!(store.remove("k"), Some(LuaValue::Integer(3)));
    assert!(store.remove("k").is_none());
    assert!(store.is_empty());
}
