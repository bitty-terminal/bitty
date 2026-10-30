//! Compressed opencode-shaped soak (bitty#2830).
//!
//! The owner's unresponsiveness appeared after long sessions of an ordinary
//! TUI: alternate-screen repaints, mouse tracking, bracketed paste, terminal
//! queries, a growing scrollback, and panes attached and detached repeatedly.
//! A live campaign reproduced the workload but not the freeze, so the shape it
//! proved reachable - a hidden surface parked with retained damage - is what
//! this harness guards against, on a clock that costs seconds instead of hours.
//!
//! The byte stream mirrors the campaign's `stressor.py`: a full-screen
//! truecolor repaint per frame inside one synchronized update, scattered diff
//! writes, `DECSET 1000/1002/1003/1006`, bracketed paste, `OSC 0/2` titles,
//! an `OSC 52` read, `DSR`/`DA1`/`XTWINOPS` queries, a paste-sized write, and a
//! periodic alternate-screen exit that exposes the growing main screen.
//!
//! Bounds are the ones the issue asks for: queue depths, retained content
//! bytes, presented frames, file descriptors, RSS, and a wall-clock liveness
//! budget. RSS/fd assertions are Linux-only (`/proc`); other targets run the
//! same workload and keep the queue/content/present assertions.

use std::time::{Duration, Instant};

#[cfg(unix)]
use bitty_runtime::{LayoutNode, View, ViewId};
use bitty_runtime::{Runtime, RuntimeConfig};
#[cfg(unix)]
use bitty_ui::SplitAxis;

/// Liveness budget on a shared CI shard (never a performance target).
const SOAK_BUDGET_CI: Duration = Duration::from_secs(180);
/// Liveness budget on a development host.
const SOAK_BUDGET_LOCAL: Duration = Duration::from_secs(60);
/// Frames in the compressed run (five simulated seconds each).
const FRAMES: u64 = 400;
/// Simulated seconds per frame.
const SECONDS_PER_FRAME: u64 = 5;
/// Main-screen history lines written before the alternate screen is entered.
const BOOT_LINES: usize = 2_000;
/// Alternate-screen exit/re-enter cadence (frames).
const ALT_CYCLE: u64 = 40;
/// Query cadence (frames).
const QUERY_EVERY: u64 = 20;
/// Title cadence (frames).
const TITLE_EVERY: u64 = 10;
/// Paste-sized write cadence (frames).
const PASTE_EVERY: u64 = 7;
/// Attach/detach cadence (frames).
#[cfg(unix)]
const PANE_CYCLE: u64 = 50;
/// Hidden-surface cycle cadence (frames).
const HIDDEN_CYCLE: u64 = 100;
/// Hard cap on retained content bytes across the whole run.
const MAX_CONTENT_BYTES: u64 = 128 * 1024 * 1024;
/// Hard cap on RSS growth across the run (allocator noise included).
#[cfg(target_os = "linux")]
const MAX_RSS_GROWTH_KIB: u64 = 96 * 1024;
/// Hard cap on file-descriptor growth across the run.
#[cfg(target_os = "linux")]
const MAX_FD_GROWTH: i64 = 16;

const CSI: &str = "\x1b[";
const OSC: &str = "\x1b]";

/// Deterministic per-frame seed so a failure is reproducible without logging
/// megabytes of escape sequences.
fn next_seed(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

/// One full-screen TUI repaint, close to the shape opencode emits.
fn tui_frame(cols: u16, rows: u16, frame: u64, seed: &mut u64) -> String {
    let cols = usize::from(cols.max(2));
    let rows = usize::from(rows.max(2));
    let mut out = String::with_capacity(cols * rows * 8);
    out.push_str(CSI);
    out.push_str("?2026h");
    out.push_str(CSI);
    out.push('H');
    for row in 0..rows {
        out.push_str(&format!("{CSI}{};1H", row + 1));
        // Per-row background band, like a status/selection stripe.
        out.push_str(&format!(
            "{CSI}48;2;{};{};{}m",
            (frame * 7 + row as u64 * 11) % 256,
            (row * 5) % 256,
            (frame * 3 + row as u64 * 17) % 256
        ));
        out.push_str(&format!(
            "{CSI}38;2;{};{};{}m",
            (row * 13) % 256,
            (frame * 9 + 40) % 256,
            (row * 23 + 80) % 256
        ));
        for col in 0..cols {
            let pick = next_seed(seed) % 97;
            match pick {
                0..=2 => out.push('界'),
                3..=5 => out.push('🙂'),
                6..=8 => out.push(' '),
                _ => out.push(char::from(b'a' + (pick % 26) as u8)),
            }
            let _ = col;
        }
        out.push_str(CSI);
        out.push('K');
    }
    // Scattered cursor moves and partial repaints, like a TUI diff pass.
    for _ in 0..(rows / 2).max(8) {
        let row = next_seed(seed) as usize % rows + 1;
        let col = next_seed(seed) as usize % cols.saturating_sub(8).max(1) + 1;
        out.push_str(&format!(
            "{CSI}{row};{col}H{CSI}38;2;{};{};{}m",
            next_seed(seed) % 256,
            next_seed(seed) % 256,
            next_seed(seed) % 256
        ));
        out.push_str(match next_seed(seed) % 5 {
            0 => "thinking ",
            1 => "tool ",
            2 => "read ",
            3 => "edit ",
            _ => "bash ",
        });
    }
    out.push_str(CSI);
    out.push_str("?2026l");
    out
}

/// Paste-sized single write (a 64 KiB-ish diff payload).
fn paste_payload() -> String {
    let mut out = String::with_capacity(64 * 1024);
    out.push_str(CSI);
    out.push_str("200~");
    for line in 0..800 {
        out.push_str(CSI);
        out.push_str("2K");
        out.push_str(&format!("x{line:03}\n"));
    }
    out.push_str(CSI);
    out.push_str("201~");
    out
}

fn make_runtime() -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig {
        scrollback: 4_000,
        ..RuntimeConfig::default()
    })
    .expect("deterministic runtime must build")
}

#[cfg(target_os = "linux")]
fn rss_kib() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages = statm
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let page_kib = std::fs::read_to_string("/proc/self/smaps")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.trim().strip_prefix("KernelPageSize:")?;
                rest.split_whitespace().next()?.parse::<u64>().ok()
            })
        })
        .unwrap_or(4);
    pages.saturating_mul(page_kib)
}

#[cfg(target_os = "linux")]
fn fd_count() -> i64 {
    std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count() as i64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn two_pane_layout(first: ViewId, second: ViewId) -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(first, 80, 24)),
        LayoutNode::leaf(View::new(second, 80, 24)),
    )
}

/// The modes opencode enables on the alternate screen.
fn tui_modes() -> &'static str {
    concat!(
        "\x1b[?1049h",
        "\x1b[?1000h",
        "\x1b[?1002h",
        "\x1b[?1003h",
        "\x1b[?1006h",
        "\x1b[?2004h",
        "\x1b[?25l",
        "\x1b[2J",
        "\x1b[H",
    )
}

#[test]
fn opencode_shaped_soak_stays_bounded() {
    let start = Instant::now();
    let mut rt = make_runtime();
    let cols = u16::try_from(rt.snapshot().width).expect("grid width fits u16");
    let rows = u16::try_from(rt.snapshot().height).expect("grid height fits u16");
    assert!(cols > 2 && rows > 2, "runtime has a live grid");

    let clock = Instant::now();
    let mut now = clock;
    let mut presented = 0u64;
    assert!(rt.tick_at(now).is_some(), "the first frame presents");
    presented += 1;

    #[cfg(target_os = "linux")]
    let (rss_start, fd_start) = (rss_kib(), fd_count());

    // Main-screen launch history, then the application screen: a TUI's boot
    // sequence, and the scrollback a long session accumulates.
    for line in 0..BOOT_LINES {
        rt.handle_pty_bytes(
            format!("[boot] opencode-shaped soak line {line:06} :: tool payload\r\n").as_bytes(),
        );
    }
    assert!(rt.tick_at(now).is_some(), "the history frame presents");
    presented += 1;
    rt.handle_pty_bytes(tui_modes().as_bytes());
    assert!(rt.tick_at(now).is_some(), "the alt-screen entry presents");
    presented += 1;
    assert!(rt.state().alt_screen_active(), "the alt screen is live");

    let mut seed = 0x5eed_2830_u64;
    let mut replies_seen = 0usize;
    let mut hidden_cycles = 0u64;
    for frame in 0..FRAMES {
        now = clock + Duration::from_secs(frame * SECONDS_PER_FRAME);
        let mut payload = tui_frame(cols, rows, frame, &mut seed);
        if frame % TITLE_EVERY == 0 {
            payload.push_str(OSC);
            payload.push_str(&format!("0;opencode-soak frame {frame}\x1b\\"));
            payload.push_str(OSC);
            payload.push_str(&format!("2;opencode-soak frame {frame}\x1b\\"));
        }
        if frame % QUERY_EVERY == 0 {
            // DSR, DA1, XTWINOPS text area and pixels, cursor style.
            payload.push_str(CSI);
            payload.push_str("6n");
            payload.push_str(CSI);
            payload.push('c');
            payload.push_str(CSI);
            payload.push_str("18t");
            payload.push_str(CSI);
            payload.push_str("14t");
            payload.push_str(CSI);
            payload.push_str("0 q");
        }
        if frame % (QUERY_EVERY * 5) == 0 {
            // OSC 52 read: the policy denies it, so nothing leaves the app.
            payload.push_str(OSC);
            payload.push_str("52;c;?\x1b\\");
        }
        if frame % PASTE_EVERY == 0 {
            payload.push_str(&paste_payload());
        }
        rt.handle_pty_bytes(payload.as_bytes());

        // A real TUI drains its query replies on every pass; the bounded reply
        // ring must never grow across the run.
        let replies = rt.take_replies();
        replies_seen += replies.len();
        assert!(
            replies.len() <= 64,
            "frame {frame}: reply batch {} exceeds the bounded ring",
            replies.len()
        );

        if rt.tick_at(now).is_some() {
            presented += 1;
        }

        // Alternate-screen churn: leave, print on the main screen, come back.
        if frame % ALT_CYCLE == ALT_CYCLE - 1 {
            let mut main = String::from("\x1b[?1049l\x1b[H");
            main.push_str(&format!("main screen tick {frame}\r\n"));
            rt.handle_pty_bytes(main.as_bytes());
            let _ = rt.tick_at(now + Duration::from_millis(1));
            rt.handle_pty_bytes("\x1b[?1049h\x1b[2J\x1b[H".as_bytes());
            let _ = rt.tick_at(now + Duration::from_millis(2));
        }

        // Attach/detach churn: the pane lifecycle the issue names explicitly.
        #[cfg(unix)]
        if frame % PANE_CYCLE == PANE_CYCLE - 1 {
            rt.set_layout(two_pane_layout(ViewId::new(1), ViewId::new(2)));
            let _ = rt.spawn_shell_for_view(
                ViewId::new(2),
                "/bin/sh",
                &["-c", "printf pane-attach"],
                40,
                10,
            );
            let _ = rt.poll_pty();
            let _ = rt.tick_at(now + Duration::from_millis(3));
            assert!(
                rt.close_pane_session(&ViewId::new(2)),
                "frame {frame}: the pane must close"
            );
            rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
            let _ = rt.tick_at(now + Duration::from_millis(4));
        }

        // Hidden-surface cycle: workspace switch-away with retained damage.
        // The reveal edge is dropped on purpose in every other cycle; the
        // bounded repair wake must still make the frame reach the surface
        // instead of parking the loop forever (bitty#2830 hypothesis 2).
        if frame % HIDDEN_CYCLE == HIDDEN_CYCLE - 1 {
            hidden_cycles += 1;
            let hide_at = now + Duration::from_millis(5);
            rt.set_surface_visibility_at(hide_at, false);
            rt.handle_pty_bytes(b"hidden pane output\r\n");
            let _ = rt.tick_at(hide_at + Duration::from_millis(6));
            let probed_at = hide_at + Duration::from_millis(6);
            let repair = rt
                .hidden_repair_deadline()
                .expect("retained damage must arm the repair wake");
            assert!(
                repair >= hide_at
                    && repair
                        <= probed_at
                            + Duration::from_millis(bitty_runtime::HIDDEN_REPAIR_INTERVAL_MS),
                "frame {frame}: the repair wake is bounded by the named interval (repair {repair:?}, hidden at {hide_at:?})"
            );
            // Consume the attempt while the surface is genuinely hidden: the
            // gate stays closed, so nothing may present.
            assert!(
                rt.tick_at(repair).is_none(),
                "frame {frame}: the repair attempt must respect the closed gate"
            );
            rt.note_hidden_repair_at(repair);
            // Even cycles model the dropped reveal: the gate is opened by the
            // repair probe at the deadline instead of by the lost edge. Odd
            // cycles take the ordinary reveal path first.
            let reveal_at = if hidden_cycles % 2 == 0 {
                repair
            } else {
                hide_at + Duration::from_millis(10)
            };
            rt.set_surface_visibility_at(reveal_at, true);
            assert_eq!(
                rt.hidden_repair_deadline(),
                None,
                "frame {frame}: the reveal clears the repair wake"
            );
            let healed = rt.tick_at(reveal_at);
            assert!(
                healed.is_some(),
                "frame {frame}: the retained frame must paint after the reveal"
            );
            presented += 1;
        }
    }

    // Layout and pane lifecycle must be clean after the churn.
    assert!(
        rt.pane_session_ids().is_empty(),
        "attach/detach churn must not leave pane sessions: {:?}",
        rt.pane_session_ids()
    );

    let gauges = rt.hang_gauges().snapshot();
    assert!(
        gauges.tick_count >= FRAMES,
        "every compressed frame must be counted, got {}",
        gauges.tick_count
    );
    assert!(
        gauges.content_bytes <= MAX_CONTENT_BYTES,
        "retained content bytes must stay bounded, got {}",
        gauges.content_bytes
    );
    assert_eq!(
        gauges.hidden_damage, 0,
        "the soak ends with a visible surface"
    );
    assert_eq!(gauges.pending_replies, 0, "every reply was answered");
    assert_eq!(gauges.plugin_parked_commands, 0);
    assert_eq!(gauges.plugin_active_commands, 0);
    assert!(
        rt.cold_queue_len() <= rt.cold_queue_capacity(),
        "cold queue overflow"
    );
    assert!(
        rt.plugin_side_len() <= rt.plugin_side_capacity(),
        "plugin side queue overflow"
    );
    assert!(
        presented >= FRAMES,
        "the TUI workload must present on at least every frame, got {presented}"
    );
    assert!(
        replies_seen > 0,
        "the workload must have exercised query replies"
    );
    assert!(hidden_cycles > 0, "the hidden-surface cycle must have run");

    let elapsed = start.elapsed();
    let is_ci = std::env::var("CI").is_ok() || std::env::var("GITHUB_ACTIONS").is_ok();
    let budget = if is_ci {
        SOAK_BUDGET_CI
    } else {
        SOAK_BUDGET_LOCAL
    };
    assert!(
        elapsed < budget,
        "opencode-shaped soak must finish inside its budget, took {elapsed:?} (budget {budget:?})"
    );

    #[cfg(target_os = "linux")]
    {
        let rss_growth = rss_kib().saturating_sub(rss_start);
        assert!(
            rss_growth <= MAX_RSS_GROWTH_KIB,
            "RSS grew {rss_growth} KiB across the soak (cap {MAX_RSS_GROWTH_KIB} KiB)"
        );
        let fd_growth = fd_count() - fd_start;
        assert!(
            fd_growth <= MAX_FD_GROWTH,
            "fd count grew {fd_growth} across the soak (cap {MAX_FD_GROWTH})"
        );
    }
}
