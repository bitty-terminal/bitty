#[cfg(unix)]
use super::apply::drain_global_control_queue;
use super::apply::{apply_control_envelope, snapshot_text};
use super::client::extract_string_from;
#[cfg(unix)]
use super::client::parse_ctl_response;
use super::client::resolve_ctl_target;
use super::render::{
    class_for_server_error, exit_for_server_error, format_failure, format_success,
};
use super::request::{
    CtlFormat, CtlParseError, CtlRequest, CtlTargeting, ctl_help_text, parse_ctl_request,
};
use super::{EXIT_CONFLICT, EXIT_GENERIC, EXIT_PERM, EXIT_RUNTIME};
use bitty_ipc::ctl as ipc_ctl;

fn words(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn help_anywhere_is_help() {
    assert_eq!(
        parse_ctl_request(&words(&["--help"])),
        Err(CtlParseError::Help)
    );
    assert_eq!(
        parse_ctl_request(&words(&["terminal", "list", "--help"])),
        Err(CtlParseError::Help)
    );
}

#[test]
fn missing_resource_is_usage() {
    let err = parse_ctl_request(&words(&[])).expect_err("empty must fail");
    assert!(matches!(err, CtlParseError::Usage { .. }));
}

#[test]
fn unknown_resource_is_usage() {
    let err = parse_ctl_request(&words(&["frobnicate", "list"])).expect_err("must fail");
    assert!(err.message().contains("unknown"));
}

#[test]
fn stray_double_dash_is_usage() {
    let err = parse_ctl_request(&words(&["terminal", "list", "--"])).expect_err("must fail");
    assert!(err.message().contains("--"));
}

#[test]
fn terminal_list_parses() {
    let (req, targeting) = parse_ctl_request(&words(&["terminal", "list"])).expect("must parse");
    assert_eq!(req, CtlRequest::TerminalList);
    assert_eq!(targeting.format, CtlFormat::Table);
}

#[test]
fn global_format_before_and_after() {
    let (_, t1) =
        parse_ctl_request(&words(&["--format", "json", "terminal", "list"])).expect("must parse");
    assert_eq!(t1.format, CtlFormat::Json);
    let (_, t2) =
        parse_ctl_request(&words(&["terminal", "list", "--format=jsonl"])).expect("must parse");
    assert_eq!(t2.format, CtlFormat::Jsonl);
    assert!(parse_ctl_request(&words(&["terminal", "list", "--format", "bogus"])).is_err());
}

#[test]
fn socket_and_instance_overrides_parse() {
    let (_, t) = parse_ctl_request(&words(&["--socket", "/tmp/bitty.sock", "terminal", "list"]))
        .expect("must parse");
    assert_eq!(t.socket.as_deref(), Some("/tmp/bitty.sock"));
    let (_, t2) =
        parse_ctl_request(&words(&["terminal", "list", "--instance=demo_1"])).expect("must parse");
    assert_eq!(t2.instance.as_deref(), Some("demo_1"));
    assert!(parse_ctl_request(&words(&["--instance", "bad id!", "terminal", "list"])).is_err());
}

#[test]
fn terminal_send_needs_id_and_text() {
    assert!(parse_ctl_request(&words(&["terminal", "send"])).is_err());
    assert!(parse_ctl_request(&words(&["terminal", "send", "t:1"])).is_err());
    let (req, _) =
        parse_ctl_request(&words(&["terminal", "send", "t:1", "cargo test"])).expect("must parse");
    assert_eq!(
        req,
        CtlRequest::TerminalSend {
            terminal_id: String::from("t:1"),
            text: String::from("cargo test"),
        }
    );
    assert!(parse_ctl_request(&words(&["terminal", "send", "bad", "hi"])).is_err());
    assert!(parse_ctl_request(&words(&["terminal", "send", "t:007", "hi"])).is_err());
}

#[test]
fn terminal_close_and_text_validate_ids() {
    let (req, _) = parse_ctl_request(&words(&["terminal", "close", "t:3"])).expect("must parse");
    assert_eq!(
        req,
        CtlRequest::TerminalClose {
            terminal_id: String::from("t:3"),
        }
    );
    assert!(parse_ctl_request(&words(&["terminal", "close"])).is_err());
    assert!(parse_ctl_request(&words(&["terminal", "close", "t:3", "extra"])).is_err());
    let (req, _) = parse_ctl_request(&words(&["terminal", "text", "t:1"])).expect("must parse");
    assert_eq!(
        req,
        CtlRequest::TerminalText {
            terminal_id: String::from("t:1"),
        }
    );
}

#[test]
fn view_split_defaults_right_and_rejects_two_dirs() {
    let (req, _) = parse_ctl_request(&words(&["view", "split"])).expect("must parse");
    assert_eq!(
        req,
        CtlRequest::ViewSplit {
            direction: ipc_ctl::SplitDirection::Right,
        }
    );
    let (req, _) = parse_ctl_request(&words(&["view", "split", "--left"])).expect("must parse");
    assert_eq!(
        req,
        CtlRequest::ViewSplit {
            direction: ipc_ctl::SplitDirection::Left,
        }
    );
    assert!(parse_ctl_request(&words(&["view", "split", "--left", "--right"])).is_err());
}

#[test]
fn view_focus_validates_id() {
    let (req, _) = parse_ctl_request(&words(&["view", "focus", "v:3"])).expect("must parse");
    assert_eq!(
        req,
        CtlRequest::ViewFocus {
            view_id: String::from("v:3"),
        }
    );
    assert!(parse_ctl_request(&words(&["view", "focus"])).is_err());
    assert!(parse_ctl_request(&words(&["view", "focus", "t:3"])).is_err());
}

#[test]
fn workspace_verbs_parse_and_validate_ids() {
    let (req, _) = parse_ctl_request(&words(&["workspace", "list"])).expect("must parse");
    assert_eq!(req, CtlRequest::WorkspaceList);
    let (req, _) = parse_ctl_request(&words(&["workspace", "new"])).expect("must parse");
    assert_eq!(req, CtlRequest::WorkspaceNew);
    let (req, _) = parse_ctl_request(&words(&["workspace", "close", "ws:2"])).expect("must parse");
    assert_eq!(
        req,
        CtlRequest::WorkspaceClose {
            workspace_id: String::from("ws:2"),
        }
    );
    let (req, _) = parse_ctl_request(&words(&["workspace", "focus", "ws:1"])).expect("must parse");
    assert_eq!(
        req,
        CtlRequest::WorkspaceFocus {
            workspace_id: String::from("ws:1"),
        }
    );
    let (req, _) = parse_ctl_request(&words(&["workspace", "move", "ws:2"])).expect("must parse");
    assert_eq!(
        req,
        CtlRequest::WorkspaceMove {
            workspace_id: String::from("ws:2"),
        }
    );
    assert_eq!(
        req.registry_id(),
        "core.workspace.move",
        "registry id pinned"
    );
    assert_eq!(
        req.wire_method(),
        Some(ipc_ctl::METHOD_MOVE_WORKSPACE),
        "wire method pinned"
    );
    // Fail closed: missing/extra args, wrong id shapes, misplaced flags.
    assert!(parse_ctl_request(&words(&["workspace"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "close"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "focus"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "move"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "close", "ws:2", "extra"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "list", "extra"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "new", "extra"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "close", "v:2"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "focus", "t:1"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "move", "v:2"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "move", "ws:2", "extra"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "move", "ws:007"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "move", "ws:2", "--right"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "close", "ws:007"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "close", "ws:2", "--right"])).is_err());
    assert!(parse_ctl_request(&words(&["workspace", "dance"])).is_err());
}

#[test]
fn config_reload_takes_no_args() {
    let (req, _) = parse_ctl_request(&words(&["config", "reload"])).expect("must parse");
    assert_eq!(req, CtlRequest::ConfigReload);
    assert!(parse_ctl_request(&words(&["config", "reload", "extra"])).is_err());
}

#[test]
fn misplaced_options_fail_closed() {
    // --cwd belongs to spawn only.
    assert!(parse_ctl_request(&words(&["terminal", "list", "--cwd", "/tmp"])).is_err());
    // split dirs belong to split only.
    assert!(parse_ctl_request(&words(&["terminal", "list", "--right"])).is_err());
    assert!(parse_ctl_request(&words(&["terminal", "spawn", "--right"])).is_err());
}

#[test]
fn registry_ids_are_stable() {
    assert_eq!(
        CtlRequest::TerminalSend {
            terminal_id: String::from("t:1"),
            text: String::from("hi"),
        }
        .registry_id(),
        "core.terminal.send"
    );
    assert_eq!(
        CtlRequest::ViewSplit {
            direction: ipc_ctl::SplitDirection::Right
        }
        .registry_id(),
        "core.view.split"
    );
    assert_eq!(
        CtlRequest::WorkspaceList.registry_id(),
        "core.workspace.list"
    );
    assert_eq!(CtlRequest::WorkspaceNew.registry_id(), "core.workspace.new");
    assert_eq!(
        CtlRequest::WorkspaceClose {
            workspace_id: String::from("ws:2"),
        }
        .registry_id(),
        "core.workspace.close"
    );
    assert_eq!(
        CtlRequest::WorkspaceFocus {
            workspace_id: String::from("ws:1"),
        }
        .registry_id(),
        "core.workspace.focus"
    );
    assert_eq!(
        CtlRequest::WorkspaceMove {
            workspace_id: String::from("ws:2"),
        }
        .registry_id(),
        "core.workspace.move"
    );
    assert_eq!(CtlRequest::ConfigReload.registry_id(), "core.config.reload");
}

#[test]
fn target_resolution_prefers_explicit_socket() {
    let targeting = CtlTargeting {
        socket: Some(String::from("/tmp/a.sock")),
        instance: None,
        format: CtlFormat::Table,
    };
    let t = resolve_ctl_target(
        &targeting,
        Some("/tmp/b.sock"),
        Some("x"),
        Some("/run/1"),
        1,
    )
    .expect("must resolve");
    assert_eq!(t.socket_path, "/tmp/a.sock");
}

#[test]
fn exit_mapping_covers_stable_codes() {
    assert_eq!(exit_for_server_error("auth", "ScopeDenied"), EXIT_PERM);
    assert_eq!(exit_for_server_error("usage", "Conflict"), EXIT_CONFLICT);
    assert_eq!(
        exit_for_server_error("transport", "FrameTooLarge"),
        EXIT_RUNTIME
    );
    assert_eq!(
        exit_for_server_error("usage", "InvalidParams"),
        EXIT_GENERIC
    );
    assert_eq!(class_for_server_error("auth", "ScopeDenied"), "Denied");
}

#[test]
fn envelopes_are_versioned() {
    let ok = format_success("core.terminal.text", "{\"text\":\"hi\"}");
    assert!(ok.contains("\"v\":1") && ok.contains("\"ok\":true"));
    let err = format_failure("core.terminal.text", "Denied", "ScopeDenied", "no");
    assert!(err.contains("\"v\":1") && err.contains("\"ok\":false"));
}

// ── headless live-Runtime control proofs ─────────────────────────────
//
// Each control op drives a real headless `Runtime` (no display, no IPC
// socket): parsing + scope enforcement + `apply_control` in one thread.
// Unscoped callers are rejected for every op (never ambient authority).

fn headless_runtime() -> bitty_runtime::Runtime {
    bitty_runtime::Runtime::with_defaults().expect("defaults must build headless")
}

#[test]
fn control_view_list_terminal_list_window_list_headless() {
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    let views = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_VIEWS, None, &cli);
    assert!(views.ok, "view list must succeed: {views:?}");
    assert!(views.result_json.contains("v:1"));
    let terms = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_TERMINALS, None, &cli);
    assert!(terms.ok, "terminal list must succeed: {terms:?}");
    assert!(terms.result_json.contains("t:1"));
    let wins = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_WINDOWS, None, &cli);
    assert!(wins.ok, "window list must succeed: {wins:?}");
    assert!(wins.result_json.contains("w:1"));
}

#[test]
fn control_terminal_send_and_text_headless() {
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    // Focused leaf is v:1, so t:1 routes; the bytes land in pending.
    let params = ipc_ctl::params_send_input("t:1", "cargo test");
    let sent = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SEND_INPUT, Some(&params), &cli);
    assert!(sent.ok, "send to focused t:1 must succeed: {sent:?}");
    assert!(sent.result_json.contains("t:1"));
    // Text round-trips (untrusted observation data, bounded).
    let tparams = ipc_ctl::params_terminal_id("t:1");
    let text = apply_control_envelope(
        &mut rt,
        ipc_ctl::METHOD_GET_TERMINAL_TEXT,
        Some(&tparams),
        &cli,
    );
    assert!(text.ok, "terminal text must succeed: {text:?}");
    assert!(text.result_json.contains("t:1"));
    // Unknown terminal is NotFound (no partial state).
    let bad = ipc_ctl::params_terminal_id("t:999");
    let missing =
        apply_control_envelope(&mut rt, ipc_ctl::METHOD_GET_TERMINAL_TEXT, Some(&bad), &cli);
    assert!(!missing.ok);
    assert_eq!(missing.code, "NotFound");
}

#[cfg(unix)]
#[test]
fn control_terminal_spawn_creates_observable_session() {
    // CTX-0323 (D3): `terminal spawn` reported `{"spawned":true}` while the
    // `view`/`terminal` lists stayed unchanged and `has_pane_session` stayed
    // false. It must create an addressable pane session (new leaf + shell).
    use bitty_runtime::ViewId;
    bitty_test_support::require_pty!();
    let mut rt = headless_runtime();
    let all = bitty_ipc::ScopeSet::all();
    assert_eq!(rt.pane_count(), 0, "fresh runtime owns no pane session");

    let spawned = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPAWN_TERMINAL, Some("{}"), &all);
    assert!(spawned.ok, "spawn must succeed: {spawned:?}");
    assert!(spawned.result_json.contains("\"spawned\":true"));
    assert!(
        spawned.result_json.contains("\"terminal_id\":\"t:2\""),
        "spawn must report the new terminal id: {spawned:?}"
    );

    // Observable: a new leaf/terminal exists and owns a live pane session.
    let new_view = ViewId::new(2);
    assert!(
        rt.has_pane_session(&new_view),
        "spawn must create a live pane session"
    );
    let views = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_VIEWS, None, &all);
    assert!(views.ok, "view list: {views:?}");
    assert!(
        views.result_json.contains("v:2"),
        "view list must gain v:2: {views:?}"
    );
    let terms = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_TERMINALS, None, &all);
    assert!(terms.ok, "terminal list: {terms:?}");
    assert!(
        terms
            .result_json
            .contains("{\"id\":\"t:2\",\"has_pane_session\":true}"),
        "terminal list must show the live session: {terms:?}"
    );
}

#[test]
fn control_terminal_text_renders_grid_text_not_debug() {
    // CTX-0321 (D1): `terminal text` returned a `Debug` dump of the internal
    // `Snapshot` struct. It must return the rendered grid (bounded, row-wise),
    // never the struct shape.
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    rt.handle_pty_bytes(b"hello grid");
    let params = ipc_ctl::params_terminal_id("t:1");
    let reply = apply_control_envelope(
        &mut rt,
        ipc_ctl::METHOD_GET_TERMINAL_TEXT,
        Some(&params),
        &cli,
    );
    assert!(reply.ok, "terminal text must succeed: {reply:?}");
    let text = extract_string_from(&reply.result_json, "text").expect("text field");
    assert!(
        text.contains("hello grid"),
        "rendered grid text must contain the typed bytes, got {text:?}"
    );
    assert!(
        !text.contains("Snapshot {") && !text.contains("Cell {") && !text.contains("Style {"),
        "terminal text must not leak the Debug struct shape, got {text:?}"
    );
}

#[test]
fn control_send_to_unfocused_is_conflict() {
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    // Split to create v:2, stay focused on v:1; sending to t:2 must name
    // the focus verb rather than silently retargeting input.
    let split = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
    let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&split), &cli);
    assert!(done.ok, "split must succeed: {done:?}");
    let params = ipc_ctl::params_send_input("t:2", "hi");
    let conflict = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SEND_INPUT, Some(&params), &cli);
    assert!(!conflict.ok);
    assert_eq!(conflict.code, "Conflict");
    assert!(conflict.message.contains("view focus"));
}

#[test]
fn control_terminal_text_sessionless_split_focused_parity() {
    // CTX-0284: `ctl view split` is layout-only (no pane shell), so both
    // leaves are session-less. The oracle must mirror present.rs CTX-0234
    // focused-only fallback: the focused session-less leaf returns the
    // primary text, unfocused session-less leaves stay empty (never
    // duplicate one grid as text across tiles).
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    // Seed the primary grid so the mirrored text is observable (a fresh grid
    // renders as blanks only).
    rt.handle_pty_bytes(b"parity-probe");
    let split = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
    let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&split), &cli);
    assert!(done.ok, "split must succeed: {done:?}");
    assert_eq!(rt.focused_view().map(|v| v.0), Some(1));
    let expected = snapshot_text(&rt.snapshot());
    assert!(
        expected.contains("parity-probe"),
        "primary grid text must be rendered, got {expected:?}"
    );
    let t1_params = ipc_ctl::params_terminal_id("t:1");
    let t1 = apply_control_envelope(
        &mut rt,
        ipc_ctl::METHOD_GET_TERMINAL_TEXT,
        Some(&t1_params),
        &cli,
    );
    assert!(t1.ok, "t:1 text must succeed: {t1:?}");
    let t2_params = ipc_ctl::params_terminal_id("t:2");
    let t2 = apply_control_envelope(
        &mut rt,
        ipc_ctl::METHOD_GET_TERMINAL_TEXT,
        Some(&t2_params),
        &cli,
    );
    assert!(t2.ok, "t:2 text must succeed: {t2:?}");
    let t1_text = extract_string_from(&t1.result_json, "text").expect("t:1 text field");
    let t2_text = extract_string_from(&t2.result_json, "text").expect("t:2 text field");
    assert_eq!(
        t1_text, expected,
        "focused session-less leaf mirrors primary"
    );
    assert!(
        t2_text.is_empty(),
        "unfocused session-less leaf stays empty, got {t2_text:?}"
    );
    // Refocus mirrors: t:2 becomes primary, t:1 goes empty.
    let focus = ipc_ctl::params_focus("v:2");
    let moved = apply_control_envelope(&mut rt, ipc_ctl::METHOD_FOCUS_VIEW, Some(&focus), &cli);
    assert!(moved.ok, "focus v:2 must succeed: {moved:?}");
    let expected2 = snapshot_text(&rt.snapshot());
    let t1b = apply_control_envelope(
        &mut rt,
        ipc_ctl::METHOD_GET_TERMINAL_TEXT,
        Some(&t1_params),
        &cli,
    );
    let t2b = apply_control_envelope(
        &mut rt,
        ipc_ctl::METHOD_GET_TERMINAL_TEXT,
        Some(&t2_params),
        &cli,
    );
    assert!(t1b.ok && t2b.ok, "both texts must succeed: {t1b:?} {t2b:?}");
    let t1b_text = extract_string_from(&t1b.result_json, "text").expect("t:1 text field");
    let t2b_text = extract_string_from(&t2b.result_json, "text").expect("t:2 text field");
    assert!(
        t1b_text.is_empty(),
        "v:1 unfocused after refocus stays empty, got {t1b_text:?}"
    );
    assert_eq!(
        t2b_text, expected2,
        "v:2 focused after refocus mirrors primary"
    );
}

#[test]
fn control_terminal_list_reports_session_presence() {
    // CTX-0284: layout-derived t:N entries must not imply shells that may
    // not exist (ctl splits spawn no shell). Each entry reports whether a
    // live pane session backs it.
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    let split = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
    let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&split), &cli);
    assert!(done.ok, "split must succeed: {done:?}");
    let terms = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_TERMINALS, None, &cli);
    assert!(terms.ok, "terminal list must succeed: {terms:?}");
    assert!(
        terms.result_json.contains("t:1") && terms.result_json.contains("t:2"),
        "both leaves listed: {}",
        terms.result_json
    );
    assert!(
        terms.result_json.contains("\"has_pane_session\":false"),
        "session-less leaves must report no session: {}",
        terms.result_json
    );
}

#[test]
fn control_view_split_and_focus_headless() {
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    let before = rt.layout().leaf_ids().len();
    let split = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
    let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&split), &cli);
    assert!(done.ok, "split must succeed: {done:?}");
    assert_eq!(rt.layout().leaf_ids().len(), before + 1);
    assert!(done.result_json.contains("new_view"));
    // CTX-0224: split Right must tile side-by-side (canonical
    // `SplitAxis::Horizontal`), not stacked — a 90° axis rotation here
    // regresses silently under leaf-count-only assertions.
    assert_side_by_side(&rt, 1, 2, "split Right");
    // Focus the new leaf.
    let new_id = rt
        .layout()
        .leaf_ids()
        .iter()
        .map(|id| id.0)
        .max()
        .unwrap_or(1);
    let focus = ipc_ctl::params_focus(&format!("v:{new_id}"));
    let moved = apply_control_envelope(&mut rt, ipc_ctl::METHOD_FOCUS_VIEW, Some(&focus), &cli);
    assert!(moved.ok, "focus must succeed: {moved:?}");
    assert_eq!(rt.focused_view().map(|v| v.0), Some(new_id));
    // Unknown view is NotFound.
    let bad = ipc_ctl::params_focus("v:999");
    let missing = apply_control_envelope(&mut rt, ipc_ctl::METHOD_FOCUS_VIEW, Some(&bad), &cli);
    assert!(!missing.ok);
    assert_eq!(missing.code, "NotFound");
}

/// Allocation rect of leaf `id` in the runtime's live container.
fn leaf_rect(rt: &bitty_runtime::Runtime, id: u64) -> bitty_runtime::UiRect {
    rt.layout_allocations()
        .into_iter()
        .find(|(vid, _)| vid.0 == id)
        .unwrap_or_else(|| panic!("leaf v:{id} must have an allocation"))
        .1
}

/// Assert leaves `first`/`second` tile side-by-side in that x order
/// (shared y/height band, ordered x, non-overlapping).
fn assert_side_by_side(rt: &bitty_runtime::Runtime, first: u64, second: u64, ctx: &str) {
    let a = leaf_rect(rt, first);
    let b = leaf_rect(rt, second);
    assert_eq!((a.y, a.height), (b.y, b.height), "{ctx}: shared row band");
    assert!(a.x + a.width <= b.x, "{ctx}: x-ordered, no overlap");
    assert!(a.width > 0 && b.width > 0, "{ctx}: both panes visible");
}

/// Assert leaves `first`/`second` tile stacked in that y order
/// (shared x/width band, ordered y, non-overlapping).
fn assert_stacked(rt: &bitty_runtime::Runtime, first: u64, second: u64, ctx: &str) {
    let a = leaf_rect(rt, first);
    let b = leaf_rect(rt, second);
    assert_eq!((a.x, a.width), (b.x, b.width), "{ctx}: shared column band");
    assert!(a.y + a.height <= b.y, "{ctx}: y-ordered, no overlap");
    assert!(a.height > 0 && b.height > 0, "{ctx}: both panes visible");
}

#[test]
fn control_view_split_axes_match_canonical_keymap() {
    // CTX-0224: the IPC `splitView` arm must use the same axis
    // semantics as the keymap path (`split_dir_to_axis` in `main.rs`):
    // Left/Right -> Horizontal (side-by-side), Up/Down -> Vertical
    // (stacked). Verified spatially via live layout allocations so a
    // 90° rotation cannot regress (leaf-count-only assertions miss it;
    // see CTX-0220-D1).
    let cli = bitty_ipc::ScopeSet::cli_default();
    for (direction, place_new_first, stacked) in [
        (ipc_ctl::SplitDirection::Right, false, false),
        (ipc_ctl::SplitDirection::Left, true, false),
        (ipc_ctl::SplitDirection::Down, false, true),
        (ipc_ctl::SplitDirection::Up, true, true),
    ] {
        let mut rt = headless_runtime();
        let params = ipc_ctl::params_split(direction);
        let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&params), &cli);
        let name = direction.as_str();
        assert!(done.ok, "split {name} must succeed: {done:?}");
        assert_eq!(rt.layout().leaf_ids().len(), 2, "split {name}");
        // Fresh runtime splits v:1 into v:1 + v:2; placement decides order.
        let (first, second) = if place_new_first { (2, 1) } else { (1, 2) };
        if stacked {
            assert_stacked(&rt, first, second, &format!("split {name}"));
        } else {
            assert_side_by_side(&rt, first, second, &format!("split {name}"));
        }
    }
}

#[test]
fn control_elevated_ops_deny_without_elevation() {
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    // terminal.close needs terminal.manage (not in CLI default).
    let close = ipc_ctl::params_terminal_id("t:1");
    let denied =
        apply_control_envelope(&mut rt, ipc_ctl::METHOD_CLOSE_TERMINAL, Some(&close), &cli);
    assert!(!denied.ok);
    assert_eq!(denied.code, "ScopeDenied");
    // CTX-0257: workspace.close needs terminal.manage too (kill power).
    let ws_close = ipc_ctl::params_workspace("ws:1");
    let denied = apply_control_envelope(
        &mut rt,
        ipc_ctl::METHOD_CLOSE_WORKSPACE,
        Some(&ws_close),
        &cli,
    );
    assert!(!denied.ok);
    assert_eq!(denied.code, "ScopeDenied");
    // terminal.spawn needs terminal.manage.
    let spawn = ipc_ctl::params_spawn(None);
    let denied =
        apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPAWN_TERMINAL, Some(&spawn), &cli);
    assert!(!denied.ok);
    assert_eq!(denied.code, "ScopeDenied");
    // config.reload needs config.modify.
    let denied = apply_control_envelope(&mut rt, ipc_ctl::METHOD_RELOAD_CONFIG, None, &cli);
    assert!(!denied.ok);
    assert_eq!(denied.code, "ScopeDenied");
}

#[test]
fn control_unscoped_callers_rejected_for_every_op() {
    let mut rt = headless_runtime();
    let empty = bitty_ipc::ScopeSet::new();
    let cases: Vec<(&str, Option<String>)> = vec![
        (ipc_ctl::METHOD_LIST_WINDOWS, None),
        (ipc_ctl::METHOD_LIST_VIEWS, None),
        (ipc_ctl::METHOD_LIST_TERMINALS, None),
        (
            ipc_ctl::METHOD_SPAWN_TERMINAL,
            Some(ipc_ctl::params_spawn(None)),
        ),
        (
            ipc_ctl::METHOD_CLOSE_TERMINAL,
            Some(ipc_ctl::params_terminal_id("t:1")),
        ),
        (
            ipc_ctl::METHOD_SEND_INPUT,
            Some(ipc_ctl::params_send_input("t:1", "hi")),
        ),
        (
            ipc_ctl::METHOD_GET_TERMINAL_TEXT,
            Some(ipc_ctl::params_terminal_id("t:1")),
        ),
        (
            ipc_ctl::METHOD_SPLIT_VIEW,
            Some(ipc_ctl::params_split(ipc_ctl::SplitDirection::Right)),
        ),
        (
            ipc_ctl::METHOD_FOCUS_VIEW,
            Some(ipc_ctl::params_focus("v:1")),
        ),
        (ipc_ctl::METHOD_LIST_WORKSPACES, None),
        (ipc_ctl::METHOD_NEW_WORKSPACE, None),
        (
            ipc_ctl::METHOD_CLOSE_WORKSPACE,
            Some(ipc_ctl::params_workspace("ws:1")),
        ),
        (
            ipc_ctl::METHOD_FOCUS_WORKSPACE,
            Some(ipc_ctl::params_workspace("ws:1")),
        ),
        (ipc_ctl::METHOD_RELOAD_CONFIG, None),
    ];
    for (method, params) in cases {
        let reply = apply_control_envelope(&mut rt, method, params.as_deref(), &empty);
        assert!(!reply.ok, "{method} with empty scopes must fail");
        assert_eq!(
            reply.code, "ScopeDenied",
            "{method} must be ScopeDenied, got {reply:?}"
        );
    }
}

#[test]
fn control_denial_surfaces_as_permission_for_every_verb() {
    // CTX-0231: every denial must surface as a permission error (exit 7,
    // class Denied, message naming the missing scope and the elevation
    // surface) — never as a timeout/unavailable mapping. Headless and
    // queue-free: `apply_control_envelope` authorizes inline, so no
    // timing asserts are needed.
    let mut rt = headless_runtime();
    let empty = bitty_ipc::ScopeSet::new();
    let cases: Vec<(&str, Option<String>, &str)> = vec![
        (ipc_ctl::METHOD_LIST_WINDOWS, None, "view.inspect"),
        (ipc_ctl::METHOD_LIST_VIEWS, None, "view.inspect"),
        (ipc_ctl::METHOD_LIST_TERMINALS, None, "terminal.inspect"),
        (
            ipc_ctl::METHOD_SPAWN_TERMINAL,
            Some(ipc_ctl::params_spawn(None)),
            "terminal.manage",
        ),
        (
            ipc_ctl::METHOD_CLOSE_TERMINAL,
            Some(ipc_ctl::params_terminal_id("t:1")),
            "terminal.manage",
        ),
        (
            ipc_ctl::METHOD_SEND_INPUT,
            Some(ipc_ctl::params_send_input("t:1", "hi")),
            "terminal.input",
        ),
        (
            ipc_ctl::METHOD_GET_TERMINAL_TEXT,
            Some(ipc_ctl::params_terminal_id("t:1")),
            "terminal.inspect",
        ),
        (
            ipc_ctl::METHOD_SPLIT_VIEW,
            Some(ipc_ctl::params_split(ipc_ctl::SplitDirection::Right)),
            "view.manage",
        ),
        (
            ipc_ctl::METHOD_FOCUS_VIEW,
            Some(ipc_ctl::params_focus("v:1")),
            "view.manage",
        ),
        (ipc_ctl::METHOD_LIST_WORKSPACES, None, "view.inspect"),
        (ipc_ctl::METHOD_NEW_WORKSPACE, None, "view.manage"),
        (
            ipc_ctl::METHOD_CLOSE_WORKSPACE,
            Some(ipc_ctl::params_workspace("ws:1")),
            "terminal.manage",
        ),
        (
            ipc_ctl::METHOD_FOCUS_WORKSPACE,
            Some(ipc_ctl::params_workspace("ws:1")),
            "view.manage",
        ),
        (ipc_ctl::METHOD_RELOAD_CONFIG, None, "config.modify"),
    ];
    for (method, params, scope) in cases {
        let reply = apply_control_envelope(&mut rt, method, params.as_deref(), &empty);
        assert!(!reply.ok, "{method} with empty scopes must fail");
        assert_eq!(
            reply.category, "auth",
            "{method} denial must be auth, got {reply:?}"
        );
        assert_eq!(
            reply.code, "ScopeDenied",
            "{method} denial must be ScopeDenied, got {reply:?}"
        );
        assert!(
            reply.message.contains(scope),
            "{method} denial must name scope '{scope}', got {:?}",
            reply.message
        );
        assert!(
            reply.message.contains("BITTY_CTL_ELEVATE"),
            "{method} denial must name the elevation surface, got {:?}",
            reply.message
        );
        assert!(
            !reply.message.contains("timed out"),
            "{method} denial must never read as a timeout, got {:?}",
            reply.message
        );
        assert_eq!(
            exit_for_server_error(reply.category, reply.code),
            EXIT_PERM,
            "{method} denial must exit {EXIT_PERM}, got {reply:?}"
        );
        assert_eq!(
            class_for_server_error(reply.category, reply.code),
            "Denied",
            "{method} denial must classify Denied, got {reply:?}"
        );
    }
}

#[test]
#[cfg(unix)]
fn control_client_denial_envelope_maps_to_permission_exit() {
    // CTX-0231: the IPC client must parse a server ScopeDenied envelope
    // into a permission outcome (exit 7), while a genuine queue timeout
    // (transport/Unavailable) stays exit 6. Pins the boundary so a
    // denial can never be misread as a timeout client-side.
    let denied = br#"{"jsonrpc":"2.0","id":1,"error":{"category":"auth","code":"ScopeDenied","message":"permission denied: scope 'view.manage' denied for action 'bitty.debug/splitView' (needs elevation via BITTY_CTL_ELEVATE)"},"version":"1.0"}"#;
    let outcome = parse_ctl_response(denied).expect("denial envelope must parse");
    assert!(!outcome.ok, "denial must not parse as success");
    assert_eq!(outcome.category, "auth");
    assert_eq!(outcome.code, "ScopeDenied");
    assert!(outcome.message.contains("view.manage"));
    assert!(outcome.message.contains("BITTY_CTL_ELEVATE"));
    assert_eq!(
        exit_for_server_error(&outcome.category, &outcome.code),
        EXIT_PERM
    );
    assert_eq!(
        class_for_server_error(&outcome.category, &outcome.code),
        "Denied"
    );

    let timed_out = br#"{"jsonrpc":"2.0","id":1,"error":{"category":"transport","code":"Unavailable","message":"control timed out (no live runtime draining)"},"version":"1.0"}"#;
    let slow = parse_ctl_response(timed_out).expect("timeout envelope must parse");
    assert!(!slow.ok, "timeout must not parse as success");
    assert_eq!(
        exit_for_server_error(&slow.category, &slow.code),
        EXIT_RUNTIME,
        "a genuine timeout stays exit {EXIT_RUNTIME}"
    );
}

#[test]
fn control_help_names_exact_elevation_verbs() {
    // CTX-0231: help must state exactly which verbs need elevation (the
    // cli_default-excluded scopes) and exempt view.* explicitly, so a
    // future view denial reads as a behavior bug, not docs ambiguity.
    // CTX-0257: workspace close joins the elevated set (kill power).
    let help = ctl_help_text();
    assert!(
        help.contains("only terminal spawn, terminal close (terminal.manage),"),
        "help must scope elevation to the exact verbs, got {help:?}"
    );
    assert!(
        help.contains("workspace close (terminal.manage)"),
        "help must name workspace close elevation, got {help:?}"
    );
    assert!(
        help.contains("config reload (config.modify) need BITTY_CTL_ELEVATE"),
        "help must name config reload elevation, got {help:?}"
    );
    assert!(
        help.contains("those four verbs fail closed (exit 7"),
        "help must pin denial exit 7, got {help:?}"
    );
    assert!(
        help.contains("view split / view focus (view.manage)"),
        "help must exempt view verbs, got {help:?}"
    );
    assert!(
        help.contains("workspace list / new / focus / move"),
        "help must exempt non-destructive workspace verbs, got {help:?}"
    );
    assert!(
        help.contains("need no elevation"),
        "help must state the no-elevation set, got {help:?}"
    );
}

#[test]
fn control_workspace_list_new_focus_close_headless() {
    // CTX-0257 entry over the control plane: list pins the tabline,
    // new/focus ride view.manage (no elevation), close needs elevation
    // and is immediate there (the pending-confirm gate is key UX only).
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    let all = bitty_ipc::ScopeSet::all();

    let list = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_WORKSPACES, None, &cli);
    assert!(list.ok, "workspace list must succeed: {list:?}");
    assert!(list.result_json.contains("\"count\":1"));
    assert!(
        list.result_json.contains("1:ws1* (1)"),
        "tabline pinned: {list:?}"
    );

    let created = apply_control_envelope(&mut rt, ipc_ctl::METHOD_NEW_WORKSPACE, None, &cli);
    assert!(created.ok, "workspace new must succeed: {created:?}");
    assert!(created.result_json.contains("\"created\":\"ws:2\""));
    assert!(created.result_json.contains("1:ws1 2:ws2* (2)"));

    let focus = ipc_ctl::params_workspace("ws:1");
    let moved =
        apply_control_envelope(&mut rt, ipc_ctl::METHOD_FOCUS_WORKSPACE, Some(&focus), &cli);
    assert!(moved.ok, "workspace focus must succeed: {moved:?}");
    assert!(moved.result_json.contains("1:ws1* 2:ws2 (2)"));

    // Unknown workspace is NotFound (no partial state).
    let bad = ipc_ctl::params_workspace("ws:9");
    let missing =
        apply_control_envelope(&mut rt, ipc_ctl::METHOD_FOCUS_WORKSPACE, Some(&bad), &cli);
    assert!(!missing.ok);
    assert_eq!(missing.code, "NotFound");

    // Close without elevation denies (kill power); with elevation it
    // closes immediately and the tabline follows.
    let close = ipc_ctl::params_workspace("ws:2");
    let denied =
        apply_control_envelope(&mut rt, ipc_ctl::METHOD_CLOSE_WORKSPACE, Some(&close), &cli);
    assert!(!denied.ok);
    assert_eq!(denied.code, "ScopeDenied");
    let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_CLOSE_WORKSPACE, Some(&close), &all);
    assert!(done.ok, "elevated close must succeed: {done:?}");
    assert!(done.result_json.contains("\"closed\":\"ws:2\""));
    assert!(done.result_json.contains("1:ws1* (1)"));

    // Closing an unknown workspace is NotFound (auth passed).
    let gone = apply_control_envelope(&mut rt, ipc_ctl::METHOD_CLOSE_WORKSPACE, Some(&bad), &all);
    assert!(!gone.ok);
    assert_eq!(gone.code, "NotFound");
}

#[test]
fn control_workspace_ids_roundtrip_after_close_and_new() {
    // CTX-0322 (D2): `workspace new` must report the stable sequence id
    // (`ws{seq}`) that `list` names and `focus`/`close` accept, even after a
    // close opens a sequence gap. Before the fix, `new` reported the
    // positional index (`ws:3`) while the slot's name was `ws4`, so the
    // reported id could not be fed back to `close`/`focus`.
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    let all = bitty_ipc::ScopeSet::all();

    let first = apply_control_envelope(&mut rt, ipc_ctl::METHOD_NEW_WORKSPACE, None, &cli);
    assert!(first.ok, "new ws2: {first:?}");
    assert!(first.result_json.contains("\"created\":\"ws:2\""));
    let second = apply_control_envelope(&mut rt, ipc_ctl::METHOD_NEW_WORKSPACE, None, &cli);
    assert!(second.ok, "new ws3: {second:?}");
    assert!(second.result_json.contains("\"created\":\"ws:3\""));

    let close2 = ipc_ctl::params_workspace("ws:2");
    let done = apply_control_envelope(
        &mut rt,
        ipc_ctl::METHOD_CLOSE_WORKSPACE,
        Some(&close2),
        &all,
    );
    assert!(done.ok, "close ws:2 must succeed: {done:?}");
    assert!(done.result_json.contains("\"closed\":\"ws:2\""));

    // Next creation takes the next sequence id (ws4), not the freed index.
    let third = apply_control_envelope(&mut rt, ipc_ctl::METHOD_NEW_WORKSPACE, None, &cli);
    assert!(third.ok, "new ws4: {third:?}");
    assert!(
        third.result_json.contains("\"created\":\"ws:4\""),
        "created id must be the stable sequence, got {third:?}"
    );

    let list = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_WORKSPACES, None, &cli);
    assert!(list.ok, "list must succeed: {list:?}");
    assert!(
        list.result_json.contains("ws4"),
        "list must name the created workspace: {list:?}"
    );

    // The id `new` reported must round-trip through close (the D2 failure
    // was NotFound for this exact id).
    let close4 = ipc_ctl::params_workspace("ws:4");
    let closed = apply_control_envelope(
        &mut rt,
        ipc_ctl::METHOD_CLOSE_WORKSPACE,
        Some(&close4),
        &all,
    );
    assert!(
        closed.ok,
        "close by the id reported from new must succeed: {closed:?}"
    );

    // focus by stable sequence id still resolves after the gap.
    let focus3 = ipc_ctl::params_workspace("ws:3");
    let focused = apply_control_envelope(
        &mut rt,
        ipc_ctl::METHOD_FOCUS_WORKSPACE,
        Some(&focus3),
        &cli,
    );
    assert!(focused.ok, "focus ws:3 must succeed after gap: {focused:?}");
}

/// Extract a flat JSON string array by key (test-only: the control result is
/// flat and workspace ids carry no escapes). Returns an empty vector when the
/// key or array delimiters are missing so a bad shape fails the assertion
/// rather than panicking inside the helper.
fn json_string_array(result_json: &str, field: &str) -> Vec<String> {
    let key = format!("\"{field}\":");
    let Some(start) = result_json.find(&key) else {
        return Vec::new();
    };
    let rest = &result_json[start + key.len()..];
    let Some(open) = rest.find('[') else {
        return Vec::new();
    };
    let Some(close_rel) = rest[open..].find(']') else {
        return Vec::new();
    };
    let body = &rest[open + 1..open + close_rel];
    body.split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().trim_matches('"').to_string())
        .collect()
}

#[test]
fn control_workspace_list_ids_roundtrip_across_sequence_gap() {
    // CTX-0338 (D2 residual): `workspace list` must emit the canonical
    // `ws:{seq}` identity that `focus`/`close`/`move` accept, so a client can
    // feed list output straight back into the write verbs. Display labels
    // stay available separately and the human `tabline` is unchanged.
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    let all = bitty_ipc::ScopeSet::all();

    assert!(apply_control_envelope(&mut rt, ipc_ctl::METHOD_NEW_WORKSPACE, None, &cli).ok);
    assert!(apply_control_envelope(&mut rt, ipc_ctl::METHOD_NEW_WORKSPACE, None, &cli).ok);
    let close2 = ipc_ctl::params_workspace("ws:2");
    let closed = apply_control_envelope(
        &mut rt,
        ipc_ctl::METHOD_CLOSE_WORKSPACE,
        Some(&close2),
        &all,
    );
    assert!(closed.ok, "close ws:2 must succeed: {closed:?}");
    // Next creation takes ws:4, leaving a sequence gap at ws:2.
    let third = apply_control_envelope(&mut rt, ipc_ctl::METHOD_NEW_WORKSPACE, None, &cli);
    assert!(third.result_json.contains("\"created\":\"ws:4\""));

    let list = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_WORKSPACES, None, &cli);
    assert!(list.ok, "list must succeed: {list:?}");
    let ids = json_string_array(&list.result_json, "workspaces");
    assert_eq!(
        ids,
        vec!["ws:1", "ws:3", "ws:4"],
        "list must emit canonical ids, got {list:?}"
    );
    let names = json_string_array(&list.result_json, "names");
    assert_eq!(
        names,
        vec!["ws1", "ws3", "ws4"],
        "display labels stay available separately, got {list:?}"
    );
    let active_id = extract_string_from(&list.result_json, "active_id").expect("active_id field");
    assert_eq!(active_id, "ws:4", "active id must be canonical: {list:?}");

    // Every id the list named must round-trip through focus then close.
    for id in &ids {
        let focus = apply_control_envelope(
            &mut rt,
            ipc_ctl::METHOD_FOCUS_WORKSPACE,
            Some(&ipc_ctl::params_workspace(id)),
            &cli,
        );
        assert!(focus.ok, "focus {id} from list must succeed: {focus:?}");
    }
    for id in &ids {
        let close = apply_control_envelope(
            &mut rt,
            ipc_ctl::METHOD_CLOSE_WORKSPACE,
            Some(&ipc_ctl::params_workspace(id)),
            &all,
        );
        assert!(close.ok, "close {id} from list must succeed: {close:?}");
    }
    // Closing every listed workspace never strands the window: the runtime
    // respawns one fresh slot (count >= 1 by invariant).
    let final_list = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_WORKSPACES, None, &cli);
    assert!(final_list.ok);
    assert!(final_list.result_json.contains("\"count\":1"));
}

#[test]
fn control_workspace_move_headless_no_elevation() {
    // CTX-0259 parity: `workspace move ws:N` rides view.manage (no
    // elevation), reparents the focused leaf, and fails closed on
    // unknown targets with no partial state.
    use bitty_runtime::{LayoutNode, SplitAxis, View, ViewId};
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    let created = apply_control_envelope(&mut rt, ipc_ctl::METHOD_NEW_WORKSPACE, None, &cli);
    assert!(created.ok, "setup ws2: {created:?}");
    let focus_ws1 = ipc_ctl::params_workspace("ws:1");
    let back = apply_control_envelope(
        &mut rt,
        ipc_ctl::METHOD_FOCUS_WORKSPACE,
        Some(&focus_ws1),
        &cli,
    );
    assert!(back.ok, "setup focus ws1: {back:?}");
    // Split ws1 so there is a second leaf to move.
    let moved_id = ViewId::new(50);
    let mut layout = rt.layout().clone();
    let focused = rt.focused_view().expect("focus");
    let old = layout.find_leaf(focused).cloned().expect("leaf");
    let fresh_leaf = View::new(moved_id, usize::from(old.cols()), usize::from(old.rows()));
    layout = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(old),
        LayoutNode::leaf(fresh_leaf),
    );
    rt.set_layout(layout);
    assert!(rt.set_focus(moved_id));
    let target = ipc_ctl::params_workspace("ws:2");
    let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_MOVE_WORKSPACE, Some(&target), &cli);
    assert!(
        done.ok,
        "workspace move must succeed without elevation: {done:?}"
    );
    assert!(done.result_json.contains("\"to\":\"ws:2\""));
    assert!(done.result_json.contains("\"moved\":\"v:50\""));
    assert_eq!(rt.workspace_count(), 2);
    assert_eq!(rt.active_workspace_index(), 0);
    assert_eq!(rt.layout().leaf_count(), 1);
    // Unknown target is NotFound (no partial state).
    let bad = ipc_ctl::params_workspace("ws:9");
    let missing = apply_control_envelope(&mut rt, ipc_ctl::METHOD_MOVE_WORKSPACE, Some(&bad), &cli);
    assert!(!missing.ok);
    assert_eq!(missing.code, "NotFound");
    assert_eq!(rt.workspace_count(), 2);
    assert_eq!(rt.layout().leaf_count(), 1);
}

#[test]
fn control_workspace_new_fails_closed_at_capacity() {
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    for _ in 1..bitty_runtime::MAX_WORKSPACES {
        let created = apply_control_envelope(&mut rt, ipc_ctl::METHOD_NEW_WORKSPACE, None, &cli);
        assert!(created.ok, "new within capacity: {created:?}");
    }
    let full = apply_control_envelope(&mut rt, ipc_ctl::METHOD_NEW_WORKSPACE, None, &cli);
    assert!(!full.ok);
    assert_eq!(full.code, "Conflict");
    assert_eq!(
        exit_for_server_error(full.category, full.code),
        EXIT_CONFLICT,
        "capacity refusal must exit {EXIT_CONFLICT}"
    );
}

#[test]
fn control_elevated_close_reports_not_found_not_denied() {
    // With elevation, auth passes and existence resolves: closing an
    // absent terminal is NotFound (proving the scope check passed).
    let mut rt = headless_runtime();
    let all = bitty_ipc::ScopeSet::all();
    let bad = ipc_ctl::params_terminal_id("t:999");
    let missing = apply_control_envelope(&mut rt, ipc_ctl::METHOD_CLOSE_TERMINAL, Some(&bad), &all);
    assert!(!missing.ok);
    assert_eq!(missing.code, "NotFound");
    let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_RELOAD_CONFIG, None, &all);
    assert!(done.ok, "elevated reload must succeed: {done:?}");
}

#[test]
#[cfg(unix)]
fn control_socketpair_roundtrip_headless_live_instance() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    // Full stack over real framing: client bytes -> `serve_connection`
    // -> scope check -> cross-thread queue -> main-thread `Runtime`
    // apply -> reply -> client parse. `Runtime` never leaves this thread.
    // CTX-0220: hold the file-local serial guard — the control queue is
    // process-global and the `wm_*` socket tests drain it concurrently.
    let _wm_guard = hold_wm_lock();
    let mut rt = headless_runtime();
    // Ensure a clean queue (other tests may have left entries on timeout).
    while ipc_ctl::pop_pending_control().is_some() {}

    let dispatcher = bitty_ipc::devtools::Dispatcher::with_defaults();
    let server_info = bitty_ipc::devtools::ServerInfo::new(
        "ctl-proof".to_string(),
        "/tmp/bitty-ctl-proof.sock".to_string(),
        80,
        24,
    );
    let context = bitty_ipc::devtools::ServeContext::with_granted(
        &server_info,
        bitty_ipc::ScopeSet::cli_default(),
    );
    let (mut client, mut server_stream) = UnixStream::pair().expect("socketpair");
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let peer = bitty_ipc::devtools::transport_attested_peer(0);
    let handle = std::thread::spawn(move || {
        let mut limiter = bitty_ipc::RateLimiter::rc9_default();
        let clock = || 0u64;
        bitty_ipc::devtools::serve_connection(
            &mut server_stream,
            peer,
            &dispatcher,
            &context,
            &mut limiter,
            &clock,
        )
    });

    // Helper: send one devtools envelope, drain the control queue
    // against the live `Runtime`, then read one framed response.
    fn roundtrip(
        client: &mut UnixStream,
        rt: &mut bitty_runtime::Runtime,
        method: &str,
        params: Option<&str>,
    ) -> String {
        let params_part = match params {
            None => String::new(),
            Some(p) => format!(",\"params\":{p}"),
        };
        let envelope = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"version\":\"1.0\",\"method\":\"{method}\"{params_part}}}"
        );
        let wire = bitty_ipc::encode_frame(envelope.as_bytes()).unwrap();
        client.write_all(&wire).unwrap();
        // The server thread has now enqueued; drain on this thread
        // (the sole `Runtime` owner) before reading the reply.
        //
        // Poll briefly: the enqueue races the send, so retry until the
        // queue is non-empty or a bound is hit (fail-closed, no sleep
        // loops in production — this spin is test-only).
        let granted = bitty_ipc::ScopeSet::cli_default();
        for _ in 0..100 {
            let drained = drain_global_control_queue(rt, &granted);
            if drained > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut header = [0u8; 4];
        client.read_exact(&mut header).unwrap();
        let len = u32::from_be_bytes(header) as usize;
        let mut body = vec![0u8; len];
        client.read_exact(&mut body).unwrap();
        String::from_utf8(body).unwrap()
    }

    // Allowed without elevation: view list reflects live layout.
    let body = roundtrip(&mut client, &mut rt, ipc_ctl::METHOD_LIST_VIEWS, None);
    assert!(
        body.contains("\"result\""),
        "view list must succeed: {body:?}"
    );
    assert!(body.contains("v:1"));

    // Allowed: send to the focused terminal.
    let params = ipc_ctl::params_send_input("t:1", "proof-bytes");
    let body = roundtrip(
        &mut client,
        &mut rt,
        ipc_ctl::METHOD_SEND_INPUT,
        Some(&params),
    );
    assert!(body.contains("\"result\""), "send must succeed: {body:?}");
    assert!(!rt.drain_pending_input().is_empty());

    // Denied without elevation: close needs terminal.manage.
    let params = ipc_ctl::params_terminal_id("t:1");
    let body = roundtrip(
        &mut client,
        &mut rt,
        ipc_ctl::METHOD_CLOSE_TERMINAL,
        Some(&params),
    );
    assert!(body.contains("\"error\""), "close must be denied: {body:?}");
    assert!(body.contains("ScopeDenied"));

    drop(client);
    let stats = handle.join().unwrap().unwrap();
    assert!(stats.requests >= 3, "server must see all requests");
    assert!(stats.denied >= 1, "denial must be counted");
}

/// RAII hermeticity for the process-global control queue + waker slot.
#[cfg(unix)]
struct ControlWakeGuard;

#[cfg(unix)]
impl ControlWakeGuard {
    fn take() -> Self {
        while ipc_ctl::pop_pending_control().is_some() {}
        ipc_ctl::set_control_waker(None);
        Self
    }
}

#[cfg(unix)]
impl Drop for ControlWakeGuard {
    fn drop(&mut self) {
        ipc_ctl::set_control_waker(None);
        while ipc_ctl::pop_pending_control().is_some() {}
    }
}

#[test]
#[cfg(unix)]
fn control_enqueue_wakes_then_drains_without_render_tick() {
    // CTX-0235 regression (live evidence PX-1296..PX-1311): an idle
    // window drains nothing — every verb returns `control timed out (no
    // live runtime draining)` because the `Wait`-sleeping event loop is
    // never woken. This headless proof mirrors that idle window: an
    // enqueue on a worker thread must first wake the loop (observed
    // here via the waker channel), and a single
    // `drain_global_control_queue` — with no `Runtime::tick`, no
    // `drive_tick`, and no render anywhere — must then apply the verb
    // and unblock the waiter with the live result.
    let _wm_guard = hold_wm_lock();
    let _wake_guard = ControlWakeGuard::take();
    let mut rt = headless_runtime();

    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
    ipc_ctl::set_control_waker(Some(std::sync::Arc::new(move || {
        let _ = wake_tx.send(());
    })));
    let granted = bitty_ipc::ScopeSet::cli_default();
    let worker = std::thread::spawn(move || {
        ipc_ctl::enqueue_control_and_wait(ipc_ctl::METHOD_LIST_VIEWS, None, "1", &granted)
    });

    // Idle-window proof part 1: the wakeup fires promptly with no tick
    // running (pre-fix this times out — nothing wakes the loop).
    wake_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("enqueue must wake the event loop on an idle window");

    // Idle-window proof part 2: one drain, no render tick, applies the
    // verb against the live `Runtime` and unblocks the waiter. The drain
    // re-authorizes at apply (defense in depth); only the wakeup is new.
    let drained = drain_global_control_queue(&mut rt, &bitty_ipc::ScopeSet::cli_default());
    assert_eq!(drained, 1, "exactly the enqueued verb must drain");
    let reply = worker.join().expect("worker thread must finish");
    assert!(reply.ok, "drained verb must succeed: {reply:?}");
    assert!(
        reply.result_json.contains("v:1"),
        "view list must reflect live layout: {reply:?}"
    );
    assert!(ipc_ctl::pop_pending_control().is_none());
}

// ── CTX-0220: headless WM-flow coverage over the devtools IPC surface ──
//
// Seat-contested: no GUI driving, no ydotool, no screenshots. A real Unix
// socket is served by `serve_connection` on a server thread while the
// test thread owns `Runtime` and drains the global control queue — the
// same code path the live servo drives. Synchronization is
// reply-correlation plus deadline-bounded `yield_now` polls (no sleeps);
// the process-global control queue (and automation/introspection stores)
// serialize on the file-local guard (CTX-0179 pattern).
//
// Where no IPC verb exists for an assertion (directional focus-move,
// zoom, resize, layout-leaf removal), the Runtime seam is driven directly
// and the missing method is recorded in
// `recording/ctx-0220/missing-ipc-methods.md` for the CTX-0188 follow-up.
// No new IPC methods are added here (out of scope).

/// Serial guard for the process-global control queue + automation stores.
#[cfg(unix)]
fn wm_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(unix)]
fn hold_wm_lock() -> std::sync::MutexGuard<'static, ()> {
    wm_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Short temp socket path (macOS SUN_LEN: payload < 100 bytes).
#[cfg(unix)]
fn wm_socket_path(tag: &str) -> String {
    let path = format!("/tmp/btw{}{tag}/s.sock", std::process::id());
    assert!(
        path.len() < 100,
        "socket path must fit macOS SUN_LEN: {path} ({} bytes)",
        path.len()
    );
    path
}

/// Connect with a deadline via `yield_now` retries (no sleeps): the
/// server thread binds concurrently, so the first attempts may race it.
#[cfg(unix)]
fn wm_connect(path: &str) -> std::os::unix::net::UnixStream {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                return stream;
            }
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::yield_now();
            }
            Err(err) => panic!("connect {path} within deadline: {err}"),
        }
    }
}

#[cfg(unix)]
fn wm_owner_uid(path: &str) -> u32 {
    use std::os::unix::fs::MetadataExt;

    std::fs::metadata(path).map(|m| m.uid()).unwrap_or(0)
}

/// Serve one connection on a real socket with explicit granted scopes.
/// Returns the server thread; it asserts request/response parity itself.
#[cfg(unix)]
fn spawn_wm_server(
    socket_path: String,
    granted: bitty_ipc::ScopeSet,
    session: &str,
    min_requests: u64,
) -> std::thread::JoinHandle<()> {
    let session = session.to_string();
    std::thread::spawn(move || {
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let verified = bitty_ipc::devtools::transport_attested_peer(wm_owner_uid(&socket_path));
        let dispatcher = bitty_ipc::devtools::Dispatcher::with_defaults();
        let server = bitty_ipc::devtools::ServerInfo::new(
            "wm-proof".to_string(),
            socket_path.clone(),
            80,
            24,
        );
        let context =
            bitty_ipc::devtools::ServeContext::with_granted_session(&server, granted, &session);
        let mut limiter = bitty_ipc::RateLimiter::rc9_default();
        let clock = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0)
        };
        let stats = bitty_ipc::devtools::serve_connection(
            &mut stream,
            verified,
            &dispatcher,
            &context,
            &mut limiter,
            &clock,
        )
        .unwrap();
        assert!(
            stats.requests >= min_requests,
            "expected at least {min_requests} requests, saw {}",
            stats.requests
        );
        assert_eq!(stats.responses, stats.requests);
    })
}

/// Test-thread side of the WM harness: owns `Runtime` (it is `!Send`)
/// and drains the global control queue so the server thread's
/// `enqueue_control_and_wait` unblocks with a correlated reply.
#[cfg(unix)]
struct WmHarness {
    stream: std::os::unix::net::UnixStream,
    next_id: u64,
    rt: bitty_runtime::Runtime,
    granted: bitty_ipc::ScopeSet,
}

#[cfg(unix)]
impl WmHarness {
    fn new(stream: std::os::unix::net::UnixStream, granted: bitty_ipc::ScopeSet) -> Self {
        // Clean slate: other tests may have left queue entries behind.
        while bitty_ipc::ctl::pop_pending_control().is_some() {}
        Self {
            stream,
            next_id: 1,
            rt: headless_runtime(),
            granted,
        }
    }

    fn send_envelope(&mut self, method: &str, params: Option<&str>) -> u64 {
        use std::io::Write;

        let id = self.next_id;
        self.next_id += 1;
        let params_part = match params {
            None => String::new(),
            Some(p) => format!(",\"params\":{p}"),
        };
        let envelope = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"version\":\"1.0\",\"method\":\"{method}\"{params_part}}}"
        );
        let wire = bitty_ipc::encode_frame(envelope.as_bytes()).unwrap();
        self.stream.write_all(&wire).unwrap();
        self.stream.flush().unwrap();
        id
    }

    fn read_reply(&mut self, id: u64) -> String {
        use std::io::Read;

        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header).unwrap();
        let len = u32::from_be_bytes(header) as usize;
        let mut body = vec![0u8; len];
        self.stream.read_exact(&mut body).unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(
            text.contains(&format!("\"id\":{id}")),
            "response lost correlation id {id}: {text}"
        );
        text
    }

    /// Control verbs (`splitView`, `focusView`, …) enqueue and block the
    /// server thread: drain until the queue yields work (deadline-bound,
    /// well under the 5 s enqueue timeout), then read the reply the
    /// drain produced. The reply itself is the generation-wait — when it
    /// arrives, the mutation has been applied.
    fn ctl(&mut self, method: &str, params: Option<&str>) -> String {
        let id = self.send_envelope(method, params);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        loop {
            if drain_global_control_queue(&mut self.rt, &self.granted) > 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "drain deadline hit for {method} (no live runtime draining?)"
            );
            std::thread::yield_now();
        }
        self.read_reply(id)
    }

    /// Direct verbs (`synthesizeInput`, `captureFrame`, `getInputRing`)
    /// answer without the control queue: plain request/response.
    fn direct(&mut self, method: &str, params: &str) -> String {
        let id = self.send_envelope(method, Some(params));
        self.read_reply(id)
    }

    fn leaf_ids(&self) -> Vec<u64> {
        self.rt.layout().leaf_ids().iter().map(|id| id.0).collect()
    }
}

/// Remove `target` from a split tree, collapsing its parent to the
/// sibling — the same semantics as `TerminalApp::close_focused_leaf`
/// (`main.rs`). There is no Runtime/IPC leaf-removal verb today (see the
/// CTX-0188-followup note), so close→survivor asserts through this seam
/// plus `Runtime::set_layout` (whose first-leaf refocus rule is product
/// code under test).
fn wm_prune_split_leaf(
    node: &mut bitty_runtime::LayoutNode,
    target: bitty_runtime::ViewId,
) -> bool {
    use bitty_runtime::LayoutNode;

    match node {
        LayoutNode::Leaf(_) => false,
        LayoutNode::Split { first, second, .. } => {
            let first_hit = matches!(first.as_ref(), LayoutNode::Leaf(v) if v.id() == target);
            let second_hit = matches!(second.as_ref(), LayoutNode::Leaf(v) if v.id() == target);
            if first_hit {
                *node = (**second).clone();
                true
            } else if second_hit {
                *node = (**first).clone();
                true
            } else {
                wm_prune_split_leaf(first, target) || wm_prune_split_leaf(second, target)
            }
        }
        LayoutNode::Stack(children) => {
            if let Some(pos) = children
                .iter()
                .position(|c| matches!(c, LayoutNode::Leaf(v) if v.id() == target))
            {
                if children.len() <= 1 {
                    return false;
                }
                children.remove(pos);
                true
            } else {
                children.iter_mut().any(|c| wm_prune_split_leaf(c, target))
            }
        }
        LayoutNode::Overlay { base, overlay, .. } => {
            wm_prune_split_leaf(base, target) || wm_prune_split_leaf(overlay, target)
        }
    }
}

#[test]
#[cfg(unix)]
fn wm_split_routing_close_survivor_over_socket() {
    let _guard = hold_wm_lock();
    let granted = bitty_ipc::ScopeSet::all();
    let socket_path = wm_socket_path("sr");
    bitty_ipc::devtools::prepare_socket_dir(&socket_path).unwrap();
    // 8 control verbs below: list, split, list, send-denied, focus,
    // send-ok, text, close-denied = 8 requests.
    let server = spawn_wm_server(socket_path.clone(), granted.clone(), "wm-flow", 8);
    let mut h = WmHarness::new(wm_connect(&socket_path), granted);

    // Single leaf, focused.
    let body = h.ctl(ipc_ctl::METHOD_LIST_VIEWS, None);
    assert!(body.contains("\"result\""), "view list: {body}");
    assert!(
        body.contains("\"id\":\"v:1\",\"focused\":true"),
        "one focused leaf: {body}"
    );

    // Split right over IPC: new leaf appears, focus stays on v:1.
    let params = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
    let body = h.ctl(ipc_ctl::METHOD_SPLIT_VIEW, Some(&params));
    assert!(
        body.contains("\"new_view\":\"v:2\""),
        "split names v:2: {body}"
    );
    assert_eq!(h.leaf_ids(), vec![1, 2]);
    let body = h.ctl(ipc_ctl::METHOD_LIST_VIEWS, None);
    assert!(
        body.contains("\"id\":\"v:1\",\"focused\":true"),
        "focus stays: {body}"
    );
    assert!(
        body.contains("\"id\":\"v:2\",\"focused\":false"),
        "new leaf unfocused: {body}"
    );

    // Focused-only routing: sending to the unfocused leaf fails closed
    // and names the focus verb instead of retargeting input.
    let params = ipc_ctl::params_send_input("t:2", "hi");
    let body = h.ctl(ipc_ctl::METHOD_SEND_INPUT, Some(&params));
    assert!(
        body.contains("\"error\""),
        "unfocused send must fail: {body}"
    );
    assert!(body.contains("Conflict"), "must be Conflict: {body}");
    assert!(
        body.contains("view focus"),
        "must name the focus verb: {body}"
    );
    assert!(
        h.rt.drain_pending_input().is_empty(),
        "denied bytes must not queue"
    );

    // Focus the new leaf over IPC, then input routes.
    let params = ipc_ctl::params_focus("v:2");
    let body = h.ctl(ipc_ctl::METHOD_FOCUS_VIEW, Some(&params));
    assert!(body.contains("\"focused\":\"v:2\""), "focus moves: {body}");
    let params = ipc_ctl::params_send_input("t:2", "wm-proof");
    let body = h.ctl(ipc_ctl::METHOD_SEND_INPUT, Some(&params));
    assert!(body.contains("\"sent_to\":\"t:2\""), "send routes: {body}");
    assert!(body.contains("\"bytes\":8"), "byte count: {body}");
    assert!(!h.rt.drain_pending_input().is_empty(), "bytes must queue");
    let params = ipc_ctl::params_terminal_id("t:2");
    let body = h.ctl(ipc_ctl::METHOD_GET_TERMINAL_TEXT, Some(&params));
    assert!(
        body.contains("\"terminal_id\":\"t:2\""),
        "text serves: {body}"
    );

    // Close over IPC tears down the pane *session*, not the leaf: with
    // no live PTY session headlessly this fails closed (Conflict) and
    // the layout is untouched — never a half-removed leaf.
    let params = ipc_ctl::params_terminal_id("t:2");
    let body = h.ctl(ipc_ctl::METHOD_CLOSE_TERMINAL, Some(&params));
    assert!(
        body.contains("\"error\""),
        "session-less close must fail: {body}"
    );
    assert!(body.contains("Conflict"), "must be Conflict: {body}");
    assert!(
        body.contains("no live session"),
        "must name the gap: {body}"
    );
    assert_eq!(h.leaf_ids(), vec![1, 2], "layout untouched by failed close");

    drop(h);
    server.join().unwrap();
    std::fs::remove_file(&socket_path).ok();
}

#[test]
fn wm_close_survivor_via_runtime_seam() {
    // Portable close→survivor half (the socket half above proves
    // `closeTerminal` fails closed without a session): split via the IPC
    // handler, prune the focused leaf via the Runtime seam (no
    // leaf-removal IPC verb exists — see the CTX-0188-followup note),
    // and let `Runtime::set_layout` refocus the survivor.
    let mut rt = headless_runtime();
    let elevated = bitty_ipc::ScopeSet::all();
    let split = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
    let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&split), &elevated);
    assert!(done.ok, "split must succeed: {done:?}");
    assert_eq!(rt.layout().leaf_ids().len(), 2);
    let focus = ipc_ctl::params_focus("v:2");
    let moved =
        apply_control_envelope(&mut rt, ipc_ctl::METHOD_FOCUS_VIEW, Some(&focus), &elevated);
    assert!(moved.ok, "focus must succeed: {moved:?}");

    let mut layout = rt.layout().clone();
    assert!(wm_prune_split_leaf(
        &mut layout,
        bitty_runtime::ViewId::new(2)
    ));
    // Pruning a missing leaf or the last leaf refuses (no empty tree).
    assert!(!wm_prune_split_leaf(
        &mut layout.clone(),
        bitty_runtime::ViewId::new(9)
    ));
    rt.set_layout(layout);
    assert_eq!(rt.leaf_count(), 1);
    assert_eq!(
        rt.focused_view(),
        Some(bitty_runtime::ViewId::new(1)),
        "refocus to survivor"
    );
    let allocs = rt.layout_allocations();
    assert_eq!(allocs.len(), 1);
    assert_eq!(allocs[0].1, rt.container(), "survivor reflows full-bleed");
}

#[test]
#[cfg(unix)]
fn wm_focus_move_across_leaves_over_socket() {
    let _guard = hold_wm_lock();
    let granted = bitty_ipc::ScopeSet::all();
    let socket_path = wm_socket_path("fm");
    bitty_ipc::devtools::prepare_socket_dir(&socket_path).unwrap();
    // split, focus, split, then one focus+list pair per leaf (3) = 9.
    let server = spawn_wm_server(socket_path.clone(), granted.clone(), "wm-focus", 9);
    let mut h = WmHarness::new(wm_connect(&socket_path), granted);

    // Three leaves: v:1 left, v:2 top-right, v:3 bottom-right.
    let params = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
    let body = h.ctl(ipc_ctl::METHOD_SPLIT_VIEW, Some(&params));
    assert!(body.contains("\"new_view\":\"v:2\""), "first split: {body}");
    let params = ipc_ctl::params_focus("v:2");
    let body = h.ctl(ipc_ctl::METHOD_FOCUS_VIEW, Some(&params));
    assert!(body.contains("\"focused\":\"v:2\""), "focus v:2: {body}");
    let params = ipc_ctl::params_split(ipc_ctl::SplitDirection::Down);
    let body = h.ctl(ipc_ctl::METHOD_SPLIT_VIEW, Some(&params));
    assert!(
        body.contains("\"new_view\":\"v:3\""),
        "second split: {body}"
    );
    assert_eq!(h.leaf_ids(), vec![1, 2, 3]);

    // `focusView` reaches every leaf; exactly one flag is set each time.
    for target in ["v:1", "v:2", "v:3"] {
        let params = ipc_ctl::params_focus(target);
        let body = h.ctl(ipc_ctl::METHOD_FOCUS_VIEW, Some(&params));
        assert!(
            body.contains(&format!("\"focused\":\"{target}\"")),
            "focus {target}: {body}"
        );
        let body = h.ctl(ipc_ctl::METHOD_LIST_VIEWS, None);
        for leaf in ["v:1", "v:2", "v:3"] {
            let want = leaf == target;
            assert!(
                body.contains(&format!("\"id\":\"{leaf}\",\"focused\":{want}")),
                "flags after focusing {target}: {body}"
            );
        }
    }

    // Directional `move_focus` has no IPC verb (owed follow-up), so it
    // is proven via the Runtime seam on the IPC-built layout. Spatial
    // assertions stay orientation-agnostic on purpose: defect CTX-0220-D1
    // (filed, not fixed here) — the IPC split axis mapping is rotated
    // 90° vs the canonical keymap path (`split_dir_to_axis` in main.rs
    // maps Right→Horizontal/left-right, while `apply_control` maps
    // Right→Vertical/top-bottom), so exact spatial expectations would
    // enshrine the bug.
    use bitty_runtime::{FocusDirection, ViewId};

    let leaves = h.leaf_ids();
    // Depth-first cycling reaches the whole leaf set and wraps.
    assert!(h.rt.set_focus(ViewId::new(1)));
    assert_eq!(h.rt.move_focus(FocusDirection::Prev), Some(ViewId::new(3)));
    assert!(h.rt.set_focus(ViewId::new(3)));
    assert_eq!(h.rt.move_focus(FocusDirection::Next), Some(ViewId::new(1)));
    // Spatial moves from every leaf never leave the leaf set (edge
    // moves may return `None`, keeping focus) and are deterministic
    // (pure function of layout + container + focus).
    for id in 1..=3u64 {
        assert!(h.rt.set_focus(ViewId::new(id)));
        for dir in [
            FocusDirection::Up,
            FocusDirection::Down,
            FocusDirection::Left,
            FocusDirection::Right,
        ] {
            assert!(h.rt.set_focus(ViewId::new(id)));
            let first = h.rt.move_focus(dir);
            assert!(
                first.is_none_or(|v| leaves.contains(&v.0)),
                "spatial move {dir:?} from v:{id} must stay in-set, got {first:?}"
            );
            let focused = h.rt.focused_view().expect("focus must persist");
            assert!(leaves.contains(&focused.0), "focus must stay valid");
            assert!(h.rt.set_focus(ViewId::new(id)));
            let second = h.rt.move_focus(dir);
            assert_eq!(second, first, "spatial move must be deterministic");
        }
    }

    drop(h);
    server.join().unwrap();
    std::fs::remove_file(&socket_path).ok();
}

#[test]
fn wm_zoom_on_off_reflow_via_runtime_seam() {
    // No zoom IPC verb exists (canonical zoom is
    // `TerminalApp::apply_chrome_action(ToggleZoom)` in main.rs, already
    // unit-covered there); the Runtime seam replicates its exact
    // stash→single-leaf→restore steps while `listViews` keeps the IPC
    // handler in the loop.
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    let split = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
    let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&split), &cli);
    assert!(done.ok, "split must succeed: {done:?}");
    assert_eq!(rt.leaf_count(), 2);

    let container = rt.container();
    let before = rt.layout_allocations();
    assert_eq!(before.len(), 2);
    let focused = rt.focused_view().expect("focus must exist");

    // Zoom on: stash the tree, present only the focused leaf.
    let backup = rt.layout().clone();
    let view = rt
        .layout()
        .find_leaf(focused)
        .cloned()
        .expect("focused leaf");
    rt.set_layout(bitty_runtime::LayoutNode::leaf(view));
    assert_eq!(rt.leaf_count(), 1);
    assert_eq!(rt.focused_view(), Some(focused));
    let zoomed = rt.layout_allocations();
    assert_eq!(zoomed.len(), 1);
    assert_eq!(zoomed[0].0, focused);
    // Zero-gap default tiles edge-to-edge: the zoomed leaf is full-bleed.
    assert_eq!(zoomed[0].1, container);
    let views = apply_control_envelope(&mut rt, ipc_ctl::METHOD_LIST_VIEWS, None, &cli);
    assert!(views.ok, "listViews while zoomed: {views:?}");
    assert!(views.result_json.contains("\"focused\":true"));

    // Zoom off: restore the tree bit-identically, focus preserved.
    rt.set_layout(backup);
    assert_eq!(rt.leaf_count(), 2);
    assert_eq!(rt.focused_view(), Some(focused));
    assert_eq!(
        rt.layout_allocations(),
        before,
        "restore must reflow identically"
    );
}

#[test]
fn wm_resize_reflow_via_runtime_seam() {
    // No resize IPC verb exists; `set_container` + `reflow_layout` is the
    // documented headless seam (no physical surface required).
    let mut rt = headless_runtime();
    let cli = bitty_ipc::ScopeSet::cli_default();
    let split = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
    let done = apply_control_envelope(&mut rt, ipc_ctl::METHOD_SPLIT_VIEW, Some(&split), &cli);
    assert!(done.ok, "split must succeed: {done:?}");
    let focused = rt.focused_view().expect("focus must exist");

    let area = |rt: &bitty_runtime::Runtime| {
        rt.layout_allocations()
            .iter()
            .find(|(id, _)| *id == focused)
            .map(|(_, r)| u32::from(r.width) * u32::from(r.height))
            .unwrap_or(0)
    };
    let before = area(&rt);
    assert!(before > 0, "focused leaf must have area");

    // Grow: the focused leaf gains cells and stays inside the container.
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 160, 48));
    let grown_allocs = rt.reflow_layout();
    assert_eq!(rt.container(), bitty_runtime::UiRect::new(0, 0, 160, 48));
    for (_, rect) in &grown_allocs {
        assert!(
            rect.x + rect.width <= 160 && rect.y + rect.height <= 48,
            "in bounds: {rect:?}"
        );
    }
    assert!(
        area(&rt) > before,
        "grow must add cells to the focused leaf"
    );
    let view = rt.layout().find_leaf(focused).expect("focused leaf");
    let alloc = grown_allocs
        .iter()
        .find(|(id, _)| *id == focused)
        .expect("alloc");
    assert_eq!(
        (view.cols(), view.rows()),
        (alloc.1.width, alloc.1.height),
        "view tracks alloc"
    );

    // Shrink: cells are taken back, leaf count and focus untouched.
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 40, 12));
    rt.reflow_layout();
    assert!(area(&rt) <= before, "shrink must take cells back");
    assert_eq!(rt.leaf_count(), 2);
    assert_eq!(rt.focused_view(), Some(focused));
}

#[test]
#[cfg(unix)]
fn wm_automation_surface_tracks_split_layout() {
    // `synthesizeInput`/`captureFrame` (CTX-0188) against an IPC-split
    // layout: automation addressing follows the new leaf, and the frame
    // surface keeps serving (redacted) after WM mutations.
    use bitty_ipc::devtools::{
        AutomationFamily, clear_automation_for_tests, clear_introspection_for_tests,
        issue_automation_bearer, publish_grid_text,
    };

    let _guard = hold_wm_lock();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    publish_grid_text(
        vec!["$ echo wm".to_string(), "wm".to_string()],
        1,
        7,
        true,
        41,
        80,
        24,
    );

    let granted = bitty_ipc::ScopeSet::all();
    let socket_path = wm_socket_path("au");
    bitty_ipc::devtools::prepare_socket_dir(&socket_path).unwrap();
    // split, synth, ring, capture = 4 requests.
    let server = spawn_wm_server(socket_path.clone(), granted.clone(), "wm-auto", 4);
    let mut h = WmHarness::new(wm_connect(&socket_path), granted);

    let params = ipc_ctl::params_split(ipc_ctl::SplitDirection::Right);
    let body = h.ctl(ipc_ctl::METHOD_SPLIT_VIEW, Some(&params));
    assert!(body.contains("\"new_view\":\"v:2\""), "split first: {body}");

    // Bearers bind (session, terminal, family); the servo stamps uptime
    // near zero at spawn, so issue at zero like the CTX-0188 harness.
    let synth = issue_automation_bearer("wm-auto", "t:2", AutomationFamily::Synthesize, 0)
        .expect("synth bearer");
    let params = format!(
        "{{\"terminalId\":\"t:2\",\"bearer\":\"{synth}\",\"originLabel\":\"wm-harness\",\"events\":[{{\"type\":\"key\",\"key\":\"Enter\"}}]}}"
    );
    let receipt = h.direct("bitty.debug/synthesizeInput", &params);
    assert!(
        receipt.contains("\"accepted\":1"),
        "synth receipt: {receipt}"
    );
    assert!(
        receipt.contains("\"synthetic\":true"),
        "synthetic flag: {receipt}"
    );
    let ring = h.direct("bitty.debug/getInputRing", "{\"limit\":10}");
    assert!(
        ring.contains("[synthetic:wm-harness]"),
        "synthetic marker for the new leaf: {ring}"
    );

    let cap = issue_automation_bearer("wm-auto", "t:2", AutomationFamily::Capture, 0)
        .expect("capture bearer");
    let params = format!("{{\"terminalId\":\"t:2\",\"bearer\":\"{cap}\",\"format\":\"semantic\"}}");
    let frame = h.direct("bitty.debug/captureFrame", &params);
    assert!(
        frame.contains("\"snapshot\":\"frame\""),
        "frame serves post-split: {frame}"
    );
    assert!(
        frame.contains("\"trust\":\"untrusted-observation\""),
        "untrusted label: {frame}"
    );

    drop(h);
    server.join().unwrap();
    clear_automation_for_tests();
    clear_introspection_for_tests();
    std::fs::remove_file(&socket_path).ok();
}
