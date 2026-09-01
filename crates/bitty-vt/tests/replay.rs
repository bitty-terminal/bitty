//! Deterministic replay fixtures: byte input -> exact action sequence.
//!
//! Per the Terminal State RFC's replay guarantees, the action stream is a
//! pure function of the byte stream. Each fixture parses its input twice
//! with fresh parsers and asserts both runs produce the identical expected
//! sequence.

use bitty_vt::{
    Attribute, AttributeChange, AttributeDiff, BoundedBytes, BoundedString, ClipboardOp, Color,
    ControlChar, Count, CursorStyle, Direction, EraseDisplayMode, EraseLineMode, GraphemeCell,
    Hyperlink, Mode, MouseCoordinateEncoding, MouseTrackingMode, Row, SequenceKind, StatusKind,
    TerminalAction, UnderlineStyle, UnrecognizedSequence, ZoneKind,
};

fn parse_twice(bytes: &[u8]) -> Vec<TerminalAction> {
    let first = {
        let mut parser = bitty_vt::Parser::new();
        let mut actions = Vec::new();
        parser.advance(bytes, |action| actions.push(action));
        actions
    };
    let second = {
        let mut parser = bitty_vt::Parser::new();
        let mut actions = Vec::new();
        parser.advance(bytes, |action| actions.push(action));
        actions
    };
    assert_eq!(first, second, "replay diverged between identical runs");
    first
}

fn attrs(changes: &[AttributeChange]) -> TerminalAction {
    TerminalAction::SetAttributes {
        attrs: AttributeDiff {
            changes: changes.to_vec().into_boxed_slice(),
        },
    }
}

fn unknown(kind: SequenceKind, final_byte: u8, intermediates: [u8; 2]) -> TerminalAction {
    TerminalAction::Unknown(UnrecognizedSequence {
        kind,
        final_byte,
        intermediates,
    })
}

/// A realistic shell-session slice: title, prompt marks, styled prompt with
/// hyperlink, bracketed paste mode, output, then alt-screen entry.
#[test]
fn fixture_shell_session_replay() {
    let input: &[u8] = b"\x1b]0;bitty: ~/dev\x07\
\x1b]133;A\x07\
\x1b[?2004h\
\x1b]8;;https://bitty.dev\x1b\\user@host\x1b]8;;\x1b\\ \
\x1b[32m$\x1b[0m \x1b]133;B\x07cargo build\n\
\x1b]133;C\x07\
Compiling bitty-vt v0.0.0\n\
\x1b]133;D;0\x07\
\x1b[?1049h";

    let expected = vec![
        TerminalAction::OscTitle {
            text: BoundedString::new("bitty: ~/dev"),
        },
        TerminalAction::OscPromptMark {
            kind: ZoneKind::PromptStart,
            exit_code: None,
        },
        TerminalAction::SetMode {
            mode: Mode::BracketedPaste,
            enabled: true,
        },
        TerminalAction::OscHyperlink {
            link: Some(Hyperlink {
                id: None,
                uri: BoundedString::new("https://bitty.dev"),
            }),
        },
        TerminalAction::Print(GraphemeCell::from('u')),
        TerminalAction::Print(GraphemeCell::from('s')),
        TerminalAction::Print(GraphemeCell::from('e')),
        TerminalAction::Print(GraphemeCell::from('r')),
        TerminalAction::Print(GraphemeCell::from('@')),
        TerminalAction::Print(GraphemeCell::from('h')),
        TerminalAction::Print(GraphemeCell::from('o')),
        TerminalAction::Print(GraphemeCell::from('s')),
        TerminalAction::Print(GraphemeCell::from('t')),
        TerminalAction::OscHyperlink { link: None },
        TerminalAction::Print(GraphemeCell::from(' ')),
        attrs(&[AttributeChange::Foreground(Color::Indexed(2))]),
        TerminalAction::Print(GraphemeCell::from('$')),
        attrs(&[AttributeChange::Reset]),
        TerminalAction::Print(GraphemeCell::from(' ')),
        TerminalAction::OscPromptMark {
            kind: ZoneKind::InputStart,
            exit_code: None,
        },
        TerminalAction::Print(GraphemeCell::from('c')),
        TerminalAction::Print(GraphemeCell::from('a')),
        TerminalAction::Print(GraphemeCell::from('r')),
        TerminalAction::Print(GraphemeCell::from('g')),
        TerminalAction::Print(GraphemeCell::from('o')),
        TerminalAction::Print(GraphemeCell::from(' ')),
        TerminalAction::Print(GraphemeCell::from('b')),
        TerminalAction::Print(GraphemeCell::from('u')),
        TerminalAction::Print(GraphemeCell::from('i')),
        TerminalAction::Print(GraphemeCell::from('l')),
        TerminalAction::Print(GraphemeCell::from('d')),
        TerminalAction::PrintControl(ControlChar(0x0A)),
        TerminalAction::OscPromptMark {
            kind: ZoneKind::OutputStart,
            exit_code: None,
        },
        TerminalAction::Print(GraphemeCell::from('C')),
        TerminalAction::Print(GraphemeCell::from('o')),
        TerminalAction::Print(GraphemeCell::from('m')),
        TerminalAction::Print(GraphemeCell::from('p')),
        TerminalAction::Print(GraphemeCell::from('i')),
        TerminalAction::Print(GraphemeCell::from('l')),
        TerminalAction::Print(GraphemeCell::from('i')),
        TerminalAction::Print(GraphemeCell::from('n')),
        TerminalAction::Print(GraphemeCell::from('g')),
        TerminalAction::Print(GraphemeCell::from(' ')),
        TerminalAction::Print(GraphemeCell::from('b')),
        TerminalAction::Print(GraphemeCell::from('i')),
        TerminalAction::Print(GraphemeCell::from('t')),
        TerminalAction::Print(GraphemeCell::from('t')),
        TerminalAction::Print(GraphemeCell::from('y')),
        TerminalAction::Print(GraphemeCell::from('-')),
        TerminalAction::Print(GraphemeCell::from('v')),
        TerminalAction::Print(GraphemeCell::from('t')),
        TerminalAction::Print(GraphemeCell::from(' ')),
        TerminalAction::Print(GraphemeCell::from('v')),
        TerminalAction::Print(GraphemeCell::from('0')),
        TerminalAction::Print(GraphemeCell::from('.')),
        TerminalAction::Print(GraphemeCell::from('0')),
        TerminalAction::Print(GraphemeCell::from('.')),
        TerminalAction::Print(GraphemeCell::from('0')),
        TerminalAction::PrintControl(ControlChar(0x0A)),
        TerminalAction::OscPromptMark {
            kind: ZoneKind::OutputEnd,
            exit_code: Some(0),
        },
        TerminalAction::SetMode {
            mode: Mode::AlternateScreenClearAndRestore,
            enabled: true,
        },
    ];

    assert_eq!(parse_twice(input), expected);
}

/// Escape-sequence stress mix: malformed resync, truncation-prone SGR runs,
/// DCS strings, invalid UTF-8, and unknown families interleaved.
#[test]
fn fixture_escape_storm_replay() {
    let input: &[u8] = b"\x1b[\x1b[38;5;46;4:3mstorm\x1b[m\
\x1bP!|00000000\x1b\\\
\xff\x1bM\
\x1b[99999999999999999C\
\x1b[?1002;1006h\x1b#8\x1b[3;t";

    let expected = vec![
        attrs(&[
            AttributeChange::Foreground(Color::Indexed(46)),
            AttributeChange::Enable(Attribute::Underline(UnderlineStyle::Curly)),
        ]),
        TerminalAction::Print(GraphemeCell::from('s')),
        TerminalAction::Print(GraphemeCell::from('t')),
        TerminalAction::Print(GraphemeCell::from('o')),
        TerminalAction::Print(GraphemeCell::from('r')),
        TerminalAction::Print(GraphemeCell::from('m')),
        attrs(&[AttributeChange::Reset]),
        unknown(SequenceKind::Dcs, b'|', [b'!', 0]),
        TerminalAction::Print(GraphemeCell::from('\u{FFFD}')),
        unknown(SequenceKind::Esc, b'M', [0, 0]),
        TerminalAction::CursorMove {
            dir: Direction::Right,
            n: Count(u16::MAX),
        },
        TerminalAction::SetMode {
            mode: Mode::MouseTracking(MouseTrackingMode::Button),
            enabled: true,
        },
        TerminalAction::SetMode {
            mode: Mode::MouseCoordinateEncoding(MouseCoordinateEncoding::Sgr),
            enabled: true,
        },
        unknown(SequenceKind::Esc, b'8', [b'#', 0]),
        unknown(SequenceKind::Csi, b't', [0, 0]),
    ];

    assert_eq!(parse_twice(input), expected);
}

/// Full-width text, tab operations, erase modes, scroll region, and device
/// status queries as produced by full-screen applications (vttest-style).
#[test]
fn fixture_fullscreen_app_replay() {
    let input: &[u8] = "🎉 wide \u{FFFD}text".as_bytes();
    let mut bytes = input.to_vec();
    bytes.extend_from_slice(
        b"\r\n\x1b[2;10r\x1b[5S\x1b[3T\x1b[H\x1b[J\x1b[K\x1b[7X\x1b[2I\x1b[Z\
\x1b[5n\x1b[6n\x1b[c\x1b[2 q\x1b[?25l\x1b[?12l\x1b[?47h",
    );

    let mut expected: Vec<TerminalAction> = "🎉 wide \u{FFFD}text"
        .chars()
        .map(|c| TerminalAction::Print(GraphemeCell::from(c)))
        .collect();
    expected.extend([
        TerminalAction::PrintControl(ControlChar(0x0D)),
        TerminalAction::PrintControl(ControlChar(0x0A)),
        TerminalAction::SetScrollRegion {
            top: Row(2),
            bottom: Row(10),
        },
        TerminalAction::ScrollUp { n: Count(5) },
        TerminalAction::ScrollDown { n: Count(3) },
        TerminalAction::CursorPosition {
            row: Row(1),
            col: bitty_vt::Col(1),
        },
        TerminalAction::EraseInDisplay {
            mode: EraseDisplayMode::Below,
        },
        TerminalAction::EraseInLine {
            mode: EraseLineMode::Right,
        },
        TerminalAction::EraseChars { n: Count(7) },
        TerminalAction::TabForward { n: Count(2) },
        TerminalAction::TabBackward { n: Count(1) },
        TerminalAction::RequestDeviceStatus {
            kind: StatusKind::OperatingStatus,
        },
        TerminalAction::RequestDeviceStatus {
            kind: StatusKind::CursorPosition,
        },
        TerminalAction::RequestDeviceStatus {
            kind: StatusKind::DeviceAttributes,
        },
        TerminalAction::CursorStyle {
            style: CursorStyle::SteadyBlock,
        },
        TerminalAction::CursorVisibility { visible: false },
        TerminalAction::SetMode {
            mode: Mode::CursorBlinking,
            enabled: false,
        },
        TerminalAction::SetMode {
            mode: Mode::AlternateScreen,
            enabled: true,
        },
    ]);

    assert_eq!(parse_twice(&bytes), expected);
}

/// OSC coverage sweep including clipboard query/write, cwd reports,
/// unknown codes, and payload truncation at the bounded cap.
#[test]
fn fixture_osc_sweep_replay() {
    let oversized = "z".repeat(bitty_vt::BoundedString::MAX_LEN + 64);
    let mut bytes = b"\x1b]52;p;?\x07\x1b]52;c;cGFzc3dvcmQ\x07\x1b]7;file:///tmp/wd\x07\x1b]4;1;#ff0000\x07\x1b]2;".to_vec();
    bytes.extend_from_slice(oversized.as_bytes());
    bytes.push(0x07);

    let expected = vec![
        TerminalAction::OscClipboard {
            op: ClipboardOp::Read,
            data: BoundedBytes::new(b"?".to_vec()),
        },
        TerminalAction::OscClipboard {
            op: ClipboardOp::Write,
            data: BoundedBytes::new(b"cGFzc3dvcmQ".to_vec()),
        },
        TerminalAction::OscCwd {
            url: BoundedString::new("file:///tmp/wd"),
        },
        TerminalAction::OscUnknown {
            id: 4,
            data: BoundedBytes::new(b"1;#ff0000".to_vec()),
        },
        TerminalAction::OscTitle {
            text: BoundedString::new("z".repeat(BoundedString::MAX_LEN)),
        },
    ];

    assert_eq!(parse_twice(&bytes), expected);
}

/// Fuzz seed corpus smoke: every seed under `seeds/` parses panic-free and
/// replays identically twice.
#[test]
fn seeds_corpus_is_panic_free_and_deterministic() {
    let seeds_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds");
    let mut checked = 0;
    for entry in std::fs::read_dir(&seeds_dir).expect("seeds/ directory must exist") {
        let path = entry.expect("seed entry readable").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("bin") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("seed readable");
        parse_twice(&bytes);
        checked += 1;
    }
    assert!(
        checked >= 10,
        "expected a populated seed corpus, found {checked}"
    );
}
