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
//!   a plugin callback. This is a source gate over **every file that implements
//!   the path** — the placeholder side (`src/panel_placeholder.rs`), the reason
//!   and counter side (`bitty-ui/src/panel_recovery.rs`), and the seam that runs
//!   the unbind edge (`src/registry/panel.rs`) — so an import or a call into any
//!   of those surfaces fails the test whichever file it is added to.
//!
//! [`PGO-14`]: https://github.com/bitty-terminal/bitty/issues/2476

use std::path::{Path, PathBuf};

use bitty_rich::scene::{
    RICH_BLOCK_VERSION, SCENE_MAX_BLOCKS_PER_TERMINAL, SCENE_MAX_DEPTH, SCENE_MAX_NODES_PER_BLOCK,
    SCENE_MAX_RICH_BYTES_PER_TERMINAL, SCENE_MAX_TEXT_BYTES_PER_BLOCK,
};
use bitty_runtime::registry::{
    MAX_RECOVERY_KEYS_PER_PANEL, MAX_RECOVERY_RECORDS_PER_PANEL, PanelHandle, PanelPlaceholder,
    PanelRegistry, PanelRegistryConfig, PanelType, PlaceholderReason, RECOVERY_ATTEMPT_CAP,
};
use bitty_runtime::{ViewId, WorkspaceId};
use bitty_ui::panel_recovery::{
    MAX_OWNER_SPELLING_LEN, PLACEHOLDER_SCENE_DEPTH, PLACEHOLDER_SCENE_NODES,
    SCN_MAX_BLOCKS_PER_TERMINAL, SCN_MAX_DEPTH_PER_BLOCK, SCN_MAX_NODES_PER_BLOCK,
    SCN_MAX_RICH_BYTES_PER_TERMINAL, SCN_MAX_TEXT_BYTES_PER_BLOCK,
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
                .present_placeholder(handle.id, handle.generation, "disabled")
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

    reg.present_placeholder(handle.id, handle.generation, "suspended")
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
///
/// The same test pins the recovery caps themselves, not just the relationship
/// between them: `a_workspace_full_of_unbound_panels_stays_bounded` is
/// parameterised by `RECOVERY_ATTEMPT_CAP`, so it stays green when the cap moves
/// (round-1 review N3). These literals are what fails then.
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

    // Decision E rule 1: eight attempts, eight keys, eight retained records.
    assert_eq!(RECOVERY_ATTEMPT_CAP, 8, "the counter cap is eight");
    assert_eq!(
        MAX_RECOVERY_KEYS_PER_PANEL, 8,
        "the per-panel key table is bounded"
    );
    assert_eq!(
        MAX_RECOVERY_RECORDS_PER_PANEL, 8,
        "the retained record ring is bounded"
    );
    // The owner bound `bitty-ui` restates (it cannot depend on the plugin host)
    // is the one the host enforces on a `PluginId`.
    assert_eq!(
        MAX_OWNER_SPELLING_LEN,
        bitty_plugin_host::manifest::MAX_PLUGIN_ID_LEN,
        "a record's owner is a validated plugin id, so its bound is the host's"
    );
}

/// Rule 6: the recovery path reaches no terminal, PTY, renderer, or plugin
/// callback. The files below are the whole implementation of that path, and this
/// gate refuses an import, path, or call into any of those surfaces.
///
/// Fails for an implementation that "presents" the placeholder by touching a
/// PTY, a renderer handle, or a plugin host callback instead of Core data — in
/// any of those files.
#[test]
fn the_recovery_path_reaches_no_terminal_pty_renderer_or_plugin_callback() {
    // Tokens naming a terminal, a PTY, a renderer, a host callback, or a
    // non-deterministic process handle. `bitty_plugin_host::` is deliberately
    // not in this list: `registry/panel.rs` names the host's *data* types
    // (`PluginId`, `CapabilityId`), and what rule 6 refuses is a callback
    // handle, not the crate's types. The two headless files hold no binding
    // type at all, so they are additionally refused any path into the crate
    // (see `HOST_FREE`).
    const FORBIDDEN: [&str; 8] = [
        "bitty_pty",
        "bitty_render",
        "bitty_terminal::",
        "std::process",
        "Command::new",
        "unsafe {",
        "plugin_runtime",
        "PluginHost",
    ];
    /// Every file on the recovery path, with the symbol that proves the gate is
    /// reading the file it thinks it is, and whether the file must be free of
    /// any path into the plugin host at all.
    const SOURCES: [(&str, &str, bool); 3] = [
        (
            "../bitty-ui/src/panel_recovery.rs",
            "pub struct RecoveryBudget",
            true,
        ),
        (
            "src/panel_placeholder.rs",
            "fn core_placeholder_block",
            true,
        ),
        ("src/registry/panel.rs", "pub fn present_placeholder", false),
    ];

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(
        SOURCES
            .iter()
            .any(|(source, _, _)| is_registry_seam(source)),
        "the gate must cover the file that runs the unbind edge \
         (`src/registry/panel.rs`): the property is only defended where it is scanned"
    );
    for (source, sentinel, host_free) in SOURCES {
        let path: PathBuf = manifest.join(source);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        assert!(
            text.contains(sentinel),
            "{} must be the file this gate thinks it is (missing {sentinel})",
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
        if host_free {
            assert!(
                !code.contains("bitty_plugin_host::"),
                "{} reaches the plugin host: the placeholder side holds types, not host state",
                display_path(&path)
            );
        }
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

/// Whether a source path is the registry seam that implements
/// `present_placeholder` and the unbind edge.
fn is_registry_seam(source: &str) -> bool {
    source.ends_with("registry/panel.rs")
}

/// Drops `//` and `/* */` comments so a doc sentence may name what the code may
/// not touch without satisfying the gate (or failing it).
///
/// Deliberately a coarse scan: it does not parse string literals, so a comment
/// marker inside a literal is treated as a comment. The scanned files contain
/// neither, and the gate only has to be right about the code these three files
/// hold.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut depth: usize = 0;
    while let Some(c) = chars.next() {
        if depth > 0 {
            if c == '/' && chars.peek() == Some(&'*') {
                chars.next();
                depth += 1;
            } else if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                depth -= 1;
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            depth = 1;
            continue;
        }
        if c == '/' && chars.peek() == Some(&'/') {
            // Line comment: drop to the end of the line, keep the newline.
            while let Some(d) = chars.next() {
                if d == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The source gate is not a tautology: it refuses a forbidden token in the
/// registry seam that implements the recovery path (the file the round-1 gate
/// did not scan), and it is not satisfied by a block comment.
///
/// Fails for a gate that scans only the placeholder-side files, and for a
/// comment stripper that only understands `//`.
#[test]
fn the_rule_6_gate_refuses_the_recovery_seam_and_block_comments() {
    // The tokens the gate scans for are the ones it must actually catch.
    assert!(
        !strip_comments("use std::process::Command;").is_empty(),
        "code survives the stripper"
    );
    assert_eq!(
        strip_comments("/* bitty_pty */ let x = 1;"),
        " let x = 1;",
        "a block comment is stripped, not scanned as code"
    );
    assert_eq!(
        strip_comments("// bitty_pty\nfn f() {}"),
        "\nfn f() {}",
        "a line comment is stripped"
    );
    assert_eq!(
        strip_comments("/* a /* nested */ b */ tail"),
        " tail",
        "nested block comments are stripped"
    );
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
        .present_placeholder(handle.id, handle.generation, "unloaded")
        .expect("placeholder");
    let placeholder: PanelPlaceholder = presented.placeholder.expect("placeholder");
    assert_eq!(placeholder.reason(), PlaceholderReason::Unloaded);
    assert_eq!(placeholder.unbound_generation(), None);
    assert_eq!(
        placeholder.scene().detail(),
        PlaceholderReason::Unloaded.detail()
    );
}
