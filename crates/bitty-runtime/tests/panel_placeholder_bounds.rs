#![forbid(unsafe_code)]
//! Bounds for the unbound-panel placeholder (OQ-084 Decision E rules 1, 5, 6;
//! bitty#2476 residual 4).
//!
//! What this file pins, and the wrong implementation each check fails:
//!
//! - [`PGO-14`] — a workspace full of unbound panels still produces one bounded
//!   placeholder per panel, one findable reason each, a counter that stops at
//!   eight, one `saturated` note per binding, and no diagnostic that grows with
//!   the number of attempts. Fails for a per-tick respawn, an unbounded counter,
//!   or a record log that appends per attempt.
//! - Rule 5 — the Core surface is a fixed v1 scene inside the accepted
//!   `SCN-1..5` limits, built by Core and carrying no style from a plugin. Fails
//!   for a placeholder that admits a plugin-supplied scene or style.
//! - Rule 6 — no recovery path reaches a terminal, a PTY, a renderer handle, or
//!   a plugin callback. This is a source gate over the two files that implement
//!   the path: an import or a call into any of those fails the test.
//!
//! [`PGO-14`]: https://github.com/bitty-terminal/bitty/issues/2476

use std::path::{Path, PathBuf};

use bitty_rich::scene::{
    RICH_BLOCK_VERSION, SCENE_MAX_BLOCKS_PER_TERMINAL, SCENE_MAX_DEPTH, SCENE_MAX_NODES_PER_BLOCK,
    SCENE_MAX_RICH_BYTES_PER_TERMINAL, SCENE_MAX_TEXT_BYTES_PER_BLOCK,
};
use bitty_runtime::registry::{
    PanelHandle, PanelPlaceholder, PanelRegistry, PanelRegistryConfig, PanelType,
    PlaceholderReason, RECOVERY_ATTEMPT_CAP,
};
use bitty_runtime::{ViewId, WorkspaceId};
use bitty_ui::panel_recovery::{
    PLACEHOLDER_SCENE_DEPTH, PLACEHOLDER_SCENE_NODES, SCN_MAX_BLOCKS_PER_TERMINAL,
    SCN_MAX_DEPTH_PER_BLOCK, SCN_MAX_NODES_PER_BLOCK, SCN_MAX_RICH_BYTES_PER_TERMINAL,
    SCN_MAX_TEXT_BYTES_PER_BLOCK,
};

fn registry() -> PanelRegistry {
    PanelRegistry::new(PanelRegistryConfig::default()).expect("default panel registry")
}

/// Fills one workspace to its panel cap with mounted panels.
fn full_workspace(reg: &mut PanelRegistry) -> Vec<PanelHandle> {
    let workspace = WorkspaceId::new(7);
    let cap = reg.config().max_panels_per_workspace;
    let mut handles = Vec::with_capacity(cap);
    for index in 0..cap {
        let handle = reg
            .create_panel(PanelType::Rich, Some(workspace))
            .expect("create panel");
        reg.mount_panel(
            handle.id,
            handle.generation,
            ViewId::new(1_000 + index as u64),
        )
        .expect("mount panel");
        handles.push(handle);
    }
    handles
}

/// PGO-14: one bounded placeholder per unbound panel, one reason each, the
/// counter stopping at eight, one `saturated` note per binding, and a retained
/// record set that does not grow with the number of attempts.
#[test]
fn a_workspace_full_of_unbound_panels_stays_bounded() {
    let mut reg = registry();
    let handles = full_workspace(&mut reg);
    let panels = handles.len();
    assert!(panels > 1, "the fixture fills a real workspace");

    // Nine attempts each: eight counted, the ninth saturated.
    for handle in &handles {
        for _ in 0..=RECOVERY_ATTEMPT_CAP {
            let presented = reg
                .present_placeholder(handle.id, handle.generation, "acme.notes", "disabled")
                .expect("placeholder");
            // The container is never disposed by a placeholder transition.
            assert!(presented.placeholder.is_some());
        }
    }

    let mut total_records = 0;
    for handle in &handles {
        assert_eq!(
            reg.placeholder_reason(handle.id),
            Some(PlaceholderReason::Disabled),
            "exactly one findable reason per panel"
        );
        assert_eq!(
            reg.recovery_attempts(handle.id, handle.generation, PlaceholderReason::Disabled),
            RECOVERY_ATTEMPT_CAP,
            "the counter stops at eight"
        );
        assert_eq!(
            reg.recovery_saturated_for(handle.id),
            Some(handle.generation),
            "one saturated note per binding"
        );
        let records = reg.recovery_records(handle.id);
        assert_eq!(
            records.len(),
            RECOVERY_ATTEMPT_CAP as usize,
            "the retained record set is capped, not per attempt"
        );
        total_records += records.len();

        // Still one container per panel, still mounted, still presentable.
        assert!(reg.panel_state(handle.id, handle.generation).is_ok());
        let block = reg
            .placeholder_block(handle.id)
            .expect("the fixed table is admitted")
            .expect("an unbound panel presents a block");
        assert_eq!(block.content().count_nodes(), PLACEHOLDER_SCENE_NODES);
        assert_eq!(block.content().depth(), PLACEHOLDER_SCENE_DEPTH);
        assert!(
            block.content().count_nodes() <= SCENE_MAX_NODES_PER_BLOCK,
            "SCN-1"
        );
        assert!(block.content().depth() <= SCENE_MAX_DEPTH, "SCN-2");
        assert!(
            block.content().text_bytes() <= SCENE_MAX_TEXT_BYTES_PER_BLOCK,
            "SCN-3"
        );
    }
    assert_eq!(
        total_records,
        panels * RECOVERY_ATTEMPT_CAP as usize,
        "the whole workspace's recovery state is bounded by panels times the cap"
    );
    // A workspace of placeholders is still one block per panel, inside SCN-5,
    // and the whole set is far inside SCN-4.
    assert!(panels <= SCENE_MAX_BLOCKS_PER_TERMINAL, "SCN-5");
    let total_text: usize = handles
        .iter()
        .filter_map(|handle| reg.placeholder_block(handle.id).ok().flatten())
        .map(|block| block.content().text_bytes())
        .sum();
    assert!(total_text <= SCENE_MAX_RICH_BYTES_PER_TERMINAL, "SCN-4");
}

/// Rule 5: the surface is a fixed v1 scene (no styled subtree, so no plugin
/// style), Core-owned, and rebuilt identically for the same reason. Fails for a
/// placeholder that admits a plugin scene, a style, or a mutable stored block.
#[test]
fn the_placeholder_scene_is_fixed_core_owned_and_v1() {
    let mut reg = registry();
    let handles = full_workspace(&mut reg);
    let handle = handles[0];

    reg.present_placeholder(handle.id, handle.generation, "acme.notes", "suspended")
        .expect("placeholder");
    let first = reg
        .placeholder_block(handle.id)
        .expect("admitted")
        .expect("presented");
    assert_eq!(
        first.content().schema_version(),
        RICH_BLOCK_VERSION,
        "a placeholder is a v1 scene: it carries no styled subtree"
    );
    assert_eq!(
        first.owner(),
        bitty_runtime::panel_placeholder::PLACEHOLDER_BLOCK_OWNER,
        "the block is Core's, not a plugin's"
    );
    assert_eq!(
        first.created_at(),
        bitty_runtime::panel_placeholder::PLACEHOLDER_BLOCK_CREATED_AT,
        "the surface is deterministic: no wall clock participates"
    );
    let second = reg
        .placeholder_block(handle.id)
        .expect("admitted")
        .expect("presented");
    assert_eq!(first, second, "rebuilt on demand, byte-identical");
}

/// The `SCN` constants duplicated in `bitty-ui` (which cannot depend on the
/// scene crate) still equal the values the scene gate enforces. Fails for a
/// drift that would let the surface claim a bound the renderer does not honour.
#[test]
fn the_duplicated_scn_bounds_match_the_enforced_values() {
    assert_eq!(SCN_MAX_NODES_PER_BLOCK, SCENE_MAX_NODES_PER_BLOCK);
    assert_eq!(SCN_MAX_DEPTH_PER_BLOCK, SCENE_MAX_DEPTH);
    assert_eq!(SCN_MAX_TEXT_BYTES_PER_BLOCK, SCENE_MAX_TEXT_BYTES_PER_BLOCK);
    assert_eq!(
        SCN_MAX_RICH_BYTES_PER_TERMINAL,
        SCENE_MAX_RICH_BYTES_PER_TERMINAL
    );
    assert_eq!(SCN_MAX_BLOCKS_PER_TERMINAL, SCENE_MAX_BLOCKS_PER_TERMINAL);
}

/// Rule 6: the recovery path reaches no terminal, PTY, renderer, or plugin
/// callback. The two files below are the whole implementation of that path, and
/// this gate refuses an import, path, or call into any of those surfaces.
///
/// Fails for an implementation that "presents" the placeholder by touching a
/// PTY, a renderer handle, or a plugin host callback instead of Core data.
#[test]
fn the_recovery_path_reaches_no_terminal_pty_renderer_or_plugin_callback() {
    const FORBIDDEN: [&str; 8] = [
        "bitty_pty",
        "bitty_render",
        "bitty_plugin_host::",
        "bitty_terminal::",
        "std::process",
        "Command::new",
        "unsafe {",
        "plugin_runtime",
    ];
    const SOURCES: [&str; 2] = [
        "src/panel_placeholder.rs",
        "../bitty-ui/src/panel_recovery.rs",
    ];

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for source in SOURCES {
        let path: PathBuf = manifest.join(source);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        assert!(
            text.contains("fn core_placeholder_block")
                || text.contains("pub struct RecoveryBudget"),
            "{} must be the file this gate thinks it is",
            path.display()
        );
        let code = strip_comments(&text);
        for forbidden in FORBIDDEN {
            assert!(
                !code.contains(forbidden),
                "{} reaches {forbidden}: the recovery path must stay Core data plus commands",
                display_path(&path)
            );
        }
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

/// Drops `//` comments so a doc sentence may name what the code may not touch.
fn strip_comments(text: &str) -> String {
    text.lines()
        .map(|line| match line.find("//") {
            Some(index) => &line[..index],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Keeping `PanelPlaceholder` in the public surface is part of the deliverable:
/// a caller must be able to read the reason and the generation it succeeded
/// without reaching into the registry internals.
#[test]
fn the_placeholder_is_readable_from_outside_the_registry() {
    let mut reg = registry();
    let handle = reg
        .create_panel(PanelType::Rich, Some(WorkspaceId::new(3)))
        .expect("create panel");
    let presented = reg
        .present_placeholder(handle.id, handle.generation, "acme.notes", "unloaded")
        .expect("placeholder");
    let placeholder: PanelPlaceholder = presented.placeholder.expect("placeholder");
    assert_eq!(placeholder.reason(), PlaceholderReason::Unloaded);
    assert_eq!(placeholder.unbound_generation(), None);
    assert_eq!(
        placeholder.scene().detail(),
        PlaceholderReason::Unloaded.detail()
    );
}
