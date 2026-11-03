//! Idle-RSS attribution over a live runtime (bitty#2859).
//!
//! These tests exercise the real headless runtime, so the rows are charged
//! from real caches and real terminal state rather than from fixtures: the
//! glyph/atlas row must move when glyphs rasterize, the scrollback row must
//! move when content is retained, and the residual must stay arithmetically
//! exact against `/proc`.

use std::time::Instant;

use bitty_runtime::LayoutNode;
use bitty_runtime::runtime::memory::{
    MEMORY_REPORT_MAX_BYTES, MEMORY_ROW_NAMES, MemoryReport, PluginMemoryUsage, ProcessMemory,
    Provenance,
};
use bitty_runtime::{Runtime, RuntimeConfig, SplitAxis, View, ViewId};

fn runtime() -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig {
        scrollback: 2_000,
        ..RuntimeConfig::default()
    })
    .expect("deterministic runtime must build")
}

fn two_pane_layout() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 60, 20)),
    )
}

/// A truecolor repaint plus enough lines to grow the retained scrollback.
fn paint(rt: &mut Runtime, frames: u64) {
    for frame in 0..frames {
        let mut payload = String::new();
        payload.push_str("\x1b[?2026h");
        for row in 0..24 {
            payload.push_str(&format!("\x1b[{};1H", row + 1));
            payload.push_str(&format!(
                "\x1b[48;2;{};{};{}m\x1b[38;2;{};{};{}m",
                (frame * 7 + row * 11) % 256,
                (row * 5) % 256,
                (frame * 3) % 256,
                (row * 13) % 256,
                (frame * 9 + 40) % 256,
                (row * 23 + 80) % 256
            ));
            payload.push_str("the quick brown fox jumps over the lazy dog 0123456789");
        }
        payload.push_str("\x1b[?2026l");
        rt.handle_pty_bytes(payload.as_bytes());
        rt.tick_at(Instant::now())
            .unwrap_or_else(|| panic!("frame {frame} must present"));
    }
    // Push the visible screen into scrollback so the retained-content row has
    // real history, not just one screen.
    let mut lines = String::new();
    for line in 0..400 {
        lines.push_str(&format!("retained scrollback line {line:04}\r\n"));
    }
    rt.handle_pty_bytes(lines.as_bytes());
}

#[test]
fn an_idle_runtime_reports_every_attribution_row() {
    let mut rt = runtime();
    rt.set_layout(two_pane_layout());
    paint(&mut rt, 4);

    let report = rt.memory_report();

    assert_eq!(report.rows.len(), MEMORY_ROW_NAMES.len());
    assert_eq!(
        report.rows.iter().map(|row| row.name).collect::<Vec<_>>(),
        MEMORY_ROW_NAMES.to_vec()
    );
    let glyphs = report.row("glyph-atlas").expect("glyph row").bytes;
    assert!(
        glyphs > 0,
        "painted glyphs must charge the glyph/atlas row, got {glyphs}"
    );
    let scrollback = report.row("scrollback").expect("scrollback row").bytes;
    assert!(
        scrollback > 0,
        "retained terminal content must charge the scrollback row, got {scrollback}"
    );
    assert_eq!(
        report.row("glyph-atlas").expect("row").provenance,
        Provenance::Measured
    );

    // The process totals exist on Linux; the rows must never claim more than
    // the process actually holds.
    #[cfg(target_os = "linux")]
    {
        assert!(report.process.available(), "VmRSS must be readable");
        assert!(report.process.threads >= 1);
        assert!(!report.over_attributed, "rows exceed RSS: {report:?}");
        assert_eq!(
            report.accounted_bytes + report.unattributed_bytes,
            report.process.rss_bytes()
        );
    }

    let text = report.render();
    assert!(text.contains("bitty memory attribution"));
    for name in MEMORY_ROW_NAMES {
        assert!(text.contains(name), "rendered report must name `{name}`");
    }
    assert!(text.contains("unattributed"));
    assert!(text.len() <= MEMORY_REPORT_MAX_BYTES);
}

#[test]
fn the_glyph_row_tracks_rasterization() {
    let mut rt = runtime();
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    let cold = rt.memory_report().bytes_of("glyph-atlas").expect("row");
    paint(&mut rt, 6);
    let warm = rt.memory_report().bytes_of("glyph-atlas").expect("row");
    assert!(
        warm > cold,
        "rasterizing glyphs must grow the glyph/atlas row (cold {cold}, warm {warm})"
    );
}

#[test]
fn a_surface_estimate_is_present_for_a_configured_headless_surface() {
    let rt = runtime();
    let report = rt.memory_report();
    // The headless surface has an extent, so the row is an estimate rather
    // than unavailable; it must never be counted as a measurement.
    match report.row("surface") {
        Some(row) => assert!(
            matches!(
                row.provenance,
                Provenance::Estimated | Provenance::Unavailable
            ),
            "surface must never claim to be measured, got {:?}",
            row.provenance
        ),
        None => panic!("surface row must exist"),
    }
}

#[test]
fn plugin_lua_bytes_are_charged_when_the_embedder_supplies_them() {
    let mut rt = runtime();
    paint(&mut rt, 2);
    let without = rt.memory_report();
    let with = rt.memory_report_with_plugins(PluginMemoryUsage {
        lua_bytes: Some(1 << 20),
        vms: 2,
        busy_vms: 0,
    });
    assert_eq!(
        with.bytes_of("lua-vm"),
        Some(1 << 20),
        "the supplied Lua total must be charged verbatim"
    );
    assert_eq!(
        without.row("lua-vm").map(|row| row.provenance),
        Some(Provenance::Unavailable),
        "without an embedder sample the row is unavailable, never zero-measured"
    );
    assert_eq!(
        with.accounted_bytes,
        without.accounted_bytes + (1 << 20),
        "only the Lua row may move when plugin bytes are supplied"
    );
}

#[test]
fn a_busy_plugin_vm_leaves_the_row_unavailable() {
    let rt = runtime();
    let report = rt.memory_report_with_plugins(PluginMemoryUsage {
        lua_bytes: None,
        vms: 1,
        busy_vms: 1,
    });
    let row = report.row("lua-vm").expect("row");
    assert_eq!(row.provenance, Provenance::Unavailable);
    assert_eq!(row.bytes, 0);
    assert!(report.render().contains("note: lua-vm unavailable"));
}

#[test]
fn the_report_writes_to_the_diagnostics_directory() {
    let mut rt = runtime();
    paint(&mut rt, 2);
    let dir = std::env::temp_dir().join(format!("bitty-memory-integration-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = rt
        .memory_report()
        .write_to_dir(&dir)
        .expect("report writes");
    let text = std::fs::read_to_string(&path).expect("report reads");
    assert!(text.contains(" pid="));
    assert!(text.contains("source="));
    assert!(text.contains("glyph-atlas"));
    assert!(text.len() <= MEMORY_REPORT_MAX_BYTES);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_degenerate_process_sample_still_renders() {
    // A zeroed sample is what a non-Linux target (or an unreadable /proc)
    // produces; the report must degrade to unavailable totals, not panic.
    let sources = bitty_runtime::runtime::memory::MemorySources {
        glyph_atlas_bytes: 4_096,
        font_file_bytes: None,
        scrollback_bytes: 2_048,
        lua_vm_bytes: None,
        plugin_vms: 0,
        plugin_busy_vms: 0,
        surface_bytes: None,
    };
    let report = MemoryReport::from_sources(&sources, ProcessMemory::default());
    assert!(!report.process.available());
    assert_eq!(report.accounted_bytes, 6_144);
    assert_eq!(report.unattributed_bytes, 0);
    let text = report.render();
    assert!(text.contains("rss=0 KiB"));
    assert!(text.contains("unattributed"));
}
