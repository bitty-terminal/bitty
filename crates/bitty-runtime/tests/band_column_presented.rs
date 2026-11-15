#![forbid(unsafe_code)]
//! bitty#2191: `bitty.ui.state` must report `presented = true` for a chrome
//! band that is painted in a column slot (`left`/`right`), not only for the row
//! bands (`top`/`bottom`).
//!
//! The defect: `slot_state_rule` still listed `left`/`right` with the *unhosted*
//! slots, so a `Column` scene the vertical band host paints inside a granted
//! column range reported `presented = false` — contradicting both the paint walk
//! and the placement table that admits the mount.
//!
//! The test drives the plugin bridge end to end (`bitty.ui.mount` →
//! `bitty.ui.state`) and then feeds the very same live mounts to a real
//! [`Runtime`], so the reported flag is checked against a band the host actually
//! commits (a granted column range and a published [`bitty_runtime::BandOrigin`]),
//! not against a bare tree. The false cases stay false on both axes: contentless
//! bands are hidden, and a row-shaped root offered to `left` stays a typed mount
//! refusal (a direction mismatch never becomes a band).

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;

use bitty_lua::ui::UiSlot;
use bitty_lua::{BridgeError, LuaValue, RC1_WALL_CLOCK_BUDGET_MS};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};
use bitty_runtime::{BandEdge, BandMount, ChromeBands, Runtime, UiRect};

/// Window grid the acceptance frame is solved in, in cells.
const WINDOW: UiRect = UiRect::new(0, 0, 120, 40);

/// Columns the `left` column scene declares (`"cpu 42%"`, its widest row).
const LEFT_W: u16 = 7;
/// Columns the `right` column scene declares (`"net"`, its only row).
const RIGHT_W: u16 = 3;

const PLUGIN_ID: &str = "bitty-featured.uibandpresented";

/// Wall-clock headroom for the fixture activation VM (bitty#1744, bitty#2800):
/// activation is slice-only and cannot settle a park, and wall time is the only
/// load-sensitive dimension. Instruction and memory stay at RC defaults, so the
/// bridge policy under test still runs at production budgets.
const ACTIVATION_WALL_BUDGET_MS: u64 = 10 * RC1_WALL_CLOCK_BUDGET_MS;

#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.0.get(key).cloned()
    }
}

struct StaticSnapshot(LuaValue);

impl SnapshotSource for StaticSnapshot {
    fn snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError> {
        if scope != "semantic" {
            return Err(BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "bad scope",
            ));
        }
        Ok(self.0.clone())
    }
}

/// The fixture's temporary plugin root and data dir; removed on drop.
struct Dirs {
    root: PathBuf,
    data: PathBuf,
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("bitty-band-presented-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Writes one `ui.rich` plugin whose `init.lua` mounts the frames under test and
/// registers the `query`/`hide`/`show` probes, then activates it.
fn activate(tag: &str, init_src: &str) -> (PluginRuntime, PluginId, Dirs) {
    let dirs = Dirs {
        root: temp_dir(&format!("{tag}-root")),
        data: temp_dir(&format!("{tag}-data")),
    };
    let plugin = dirs.root.join(PLUGIN_ID);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{PLUGIN_ID}"
name = "Band Presented Test"
version = "0.1.0"
description = "ui.state column-band presentation test"

[compat]
plugin-api = "^1.0"

[capabilities]
ui.rich = true

[lazy]
commands = ["{PLUGIN_ID}:query", "{PLUGIN_ID}:hide", "{PLUGIN_ID}:show"]
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");

    let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(dirs.data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![dirs.root.clone()],
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("zones", LuaValue::array(vec![])),
        ]))),
    });
    runtime.set_vm_wall_budget_ms(Some(ACTIVATION_WALL_BUDGET_MS));
    runtime.set_host_call_deadline_ms(Some(common::HARNESS_HOST_CALL_DEADLINE_MS));
    common::install_stub_backend(&mut runtime);
    runtime.discover();
    let id = PluginId::new(PLUGIN_ID).expect("valid id");
    let report = runtime.activate(&id).expect("activate");
    assert_eq!(report.state, LifecycleState::Active);

    (runtime, id, dirs)
}

fn bool_at(state: &LuaValue, key: &str) -> bool {
    match state.get(key) {
        Some(LuaValue::Bool(value)) => *value,
        other => panic!("{key} must be a boolean, got {other:?}"),
    }
}

const INIT_SRC: &str = r#"
-- The column scene the defect under-reported: two rows stacked top-down by the
-- vertical band host, seven cells thick.
local left = bitty.ui.mount("left", {
  kind = "Column",
  children = {
    { kind = "Text", text = "cpu 42%" },
    { kind = "Text", text = "mem 30%" },
  },
})
local right = bitty.ui.mount("right", {
  kind = "Column",
  children = { { kind = "Text", text = "net" } },
})
-- Row-band control: the content gate `top`/`bottom` already had.
local top = bitty.ui.mount("top", { kind = "Text", text = "bar" })
-- Contentless bands: nothing to grant a column or a row to, so nothing paints.
local empty_column = bitty.ui.mount("left", {
  kind = "Column",
  children = { { kind = "Text", text = "" } },
})
local empty_row = bitty.ui.mount("top", { kind = "Text", text = "" })
-- Direction mismatch: a row-shaped root has no rows to stack down a column, so
-- the mount is refused instead of painted horizontally inside the band.
local row_ok, row_err = pcall(bitty.ui.mount, "left", {
  kind = "Row",
  children = { { kind = "Text", text = "a" }, { kind = "Text", text = "b" } },
})

local function presented(handle)
  local s = bitty.ui.state(handle)
  return s and s.presented or false, s and s.focused or false
end

bitty.commands.register({
  id = "query",
  title = "Query band state",
  run = function()
    local left_presented, left_focused = presented(left)
    local right_presented = presented(right)
    local top_presented = presented(top)
    local empty_column_presented, empty_column_focused = presented(empty_column)
    local empty_row_presented = presented(empty_row)
    return {
      left_presented = left_presented,
      left_focused = left_focused,
      right_presented = right_presented,
      top_presented = top_presented,
      empty_column_presented = empty_column_presented,
      empty_column_focused = empty_column_focused,
      empty_row_presented = empty_row_presented,
      row_ok = row_ok,
      row_code = row_ok and "NONE" or row_err.code,
    }
  end,
})

bitty.commands.register({
  id = "hide",
  title = "Hide the left column band",
  run = function()
    return bitty.ui.update(left, {
      kind = "Column",
      children = { { kind = "Text", text = "" } },
    })
  end,
})

bitty.commands.register({
  id = "show",
  title = "Show the left column band",
  run = function()
    return bitty.ui.update(left, {
      kind = "Column",
      children = {
        { kind = "Text", text = "cpu 42%" },
        { kind = "Text", text = "mem 30%" },
      },
    })
  end,
})

return {}
"#;

#[test]
fn ui_state_reports_presented_for_a_painted_column_band() {
    let (mut runtime, id, _dirs) = activate("column", INIT_SRC);

    // 1. The plugin-facing flag. Before the fix `left`/`right` reported
    //    `presented = false` here while the host painted them.
    let initial = common::settle_dispatch(&mut runtime, &id, "query", &[]).expect_value("query");
    assert!(
        bool_at(&initial, "left_presented"),
        "a painted left column band must report presented = true"
    );
    assert!(
        bool_at(&initial, "right_presented"),
        "a painted right column band must report presented = true"
    );
    assert!(
        bool_at(&initial, "left_focused"),
        "a presented band under a focused window reports focused = true"
    );
    assert!(
        bool_at(&initial, "top_presented"),
        "the row-band content gate is unchanged"
    );

    // 2. The false cases stay false on both axes.
    assert!(
        !bool_at(&initial, "empty_column_presented"),
        "a column band whose rows all flatten to nothing paints nothing"
    );
    assert!(
        !bool_at(&initial, "empty_column_focused"),
        "an unpainted band never holds focus"
    );
    assert!(
        !bool_at(&initial, "empty_row_presented"),
        "an empty-text row band stays hidden"
    );
    assert!(
        !bool_at(&initial, "row_ok"),
        "a Row root is refused in left"
    );
    assert_eq!(
        initial.get("row_code"),
        Some(&LuaValue::String("E_UI_COMPONENT_INVALID".to_string())),
        "the direction mismatch keeps its typed component error"
    );

    // 3. The rule follows content, not the slot: hiding the live column band
    //    flips the flag, and showing it flips it back.
    let hidden = {
        let _ = common::settle_dispatch(&mut runtime, &id, "hide", &[]).expect_value("hide");
        common::settle_dispatch(&mut runtime, &id, "query", &[]).expect_value("query hidden")
    };
    assert!(
        !bool_at(&hidden, "left_presented"),
        "an emptied column band reports presented = false"
    );
    assert!(!bool_at(&hidden, "left_focused"));
    assert!(
        bool_at(&hidden, "right_presented"),
        "the untouched right band keeps its flag"
    );
    let shown = {
        let _ = common::settle_dispatch(&mut runtime, &id, "show", &[]).expect_value("show");
        common::settle_dispatch(&mut runtime, &id, "query", &[]).expect_value("query shown")
    };
    assert!(bool_at(&shown, "left_presented"));

    // 4. The same mounts on a real runtime: the column band gets a granted
    //    column range and a published origin, i.e. it is genuinely painted in
    //    the slot `presented` now claims.
    let blocks = runtime.ui_blocks_with_handles();
    assert_eq!(
        blocks.len(),
        5,
        "four band mounts are retained and the refused Row mount stored nothing"
    );
    let (bands, unplaced) = ChromeBands::from_mounts(blocks.iter().map(
        |(plugin, slot, node, version, _generation, _handle)| {
            (plugin.to_string(), *slot, node.clone(), *version)
        },
    ));
    assert_eq!(unplaced, 0, "every retained mount is hosted");
    let mounts = blocks
        .iter()
        .map(
            |(plugin, slot, _node, version, generation, handle)| BandMount {
                plugin_id: plugin.to_string(),
                slot: *slot,
                version: *version,
                generation: *generation,
                handle: *handle,
            },
        )
        .collect::<Vec<_>>();

    let mut host = Runtime::with_defaults().expect("runtime must build");
    host.set_window_cells(WINDOW);
    host.set_chrome_bands_with_mounts(bands, mounts);
    host.tick().expect("tick presents the bands");

    assert_eq!(
        host.plugin_band_column_budget(),
        (LEFT_W, RIGHT_W),
        "both column bands reserve exactly their declared widths"
    );
    assert_eq!(
        host.visible_band_column_count(BandEdge::Left),
        1,
        "the contentless left mount takes no column range"
    );
    assert_eq!(
        host.visible_band_column(BandEdge::Left, 0),
        Some(WINDOW.x),
        "the left column band starts at the window edge"
    );
    assert_eq!(
        host.visible_band_column(BandEdge::Right, 0),
        Some(WINDOW.x + WINDOW.width - RIGHT_W),
        "the right column band is flush to the opposite edge"
    );
    assert_eq!(
        host.visible_band_count(BandEdge::Top),
        1,
        "the row-band budget still hides the empty-text mount"
    );
    assert_eq!(
        host.visible_band_row(BandEdge::Top, 0),
        Some(WINDOW.y),
        "the row band still paints the top window row"
    );

    let origins = host.published_band_origins();
    let left_origins = origins
        .iter()
        .filter(|origin| origin.edge == BandEdge::Left)
        .collect::<Vec<_>>();
    assert_eq!(
        left_origins.len(),
        1,
        "exactly one left band committed an origin"
    );
    assert_eq!(left_origins[0].slot, UiSlot::Left);
    assert_eq!(
        left_origins[0].column, WINDOW.x,
        "the committed origin carries the band's column geometry"
    );
    let right_origin = origins
        .iter()
        .find(|origin| origin.edge == BandEdge::Right)
        .expect("the right column band committed a presented origin");
    assert_eq!(right_origin.column, WINDOW.x + WINDOW.width - RIGHT_W);
    let top_origin = origins
        .iter()
        .find(|origin| origin.edge == BandEdge::Top)
        .expect("the row band committed a presented origin");
    assert_eq!(top_origin.row, WINDOW.y);
}
