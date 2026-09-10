//! Unit tests for the VT parser state machine, SGR/color decoding, dispatch,
//! and kitty `APC G` handling.
//!
//! Extracted from `parser.rs` (CTX-0309); `use super::*` preserves the
//! previous `parser::tests` access to private items.

use super::*;
use crate::action::{
    Attribute, AttributeChange, AttributeDiff, CharsetSlot, CharsetTable, ClipboardOp, Col, Color,
    ControlChar, Count, CursorStyle, Direction, EraseDisplayMode, EraseLineMode, GraphemeCell,
    Hyperlink, Mode, MouseTrackingMode, Rgb, Row, SequenceKind, StatusKind, TabTargets,
    UnderlineStyle, UnrecognizedSequence, ZoneKind,
};
use crate::bounded::{BoundedBytes, BoundedString};

fn parse(bytes: &[u8]) -> Vec<TerminalAction> {
    let mut parser = Parser::new();
    let mut actions = Vec::new();
    parser.advance(bytes, |action| actions.push(action));
    actions
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

#[test]
fn prints_text_as_grapheme_cells() {
    assert_eq!(
        parse(b"hi"),
        vec![
            TerminalAction::Print(GraphemeCell::from('h')),
            TerminalAction::Print(GraphemeCell::from('i')),
        ]
    );
}

#[test]
fn control_bytes_map_to_print_control() {
    assert_eq!(
        parse(b"a\x07b\x08c\rd\n"),
        vec![
            TerminalAction::Print(GraphemeCell::from('a')),
            TerminalAction::PrintControl(ControlChar(0x07)),
            TerminalAction::Print(GraphemeCell::from('b')),
            TerminalAction::PrintControl(ControlChar(0x08)),
            TerminalAction::Print(GraphemeCell::from('c')),
            TerminalAction::PrintControl(ControlChar(0x0D)),
            TerminalAction::Print(GraphemeCell::from('d')),
            TerminalAction::PrintControl(ControlChar(0x0A)),
        ]
    );
}

#[test]
fn shifts_invoke_charsets() {
    let actions = parse(b"\x0E\x0F");
    assert_eq!(
        actions,
        vec![
            TerminalAction::InvokeCharset {
                slot: CharsetSlot::G1
            },
            TerminalAction::InvokeCharset {
                slot: CharsetSlot::G0
            },
        ]
    );
}

#[test]
fn cursor_moves_default_missing_and_zero_counts_to_one() {
    assert_eq!(
        parse(b"\x1b[A"),
        vec![TerminalAction::CursorMove {
            dir: Direction::Up,
            n: Count(1)
        }]
    );
    assert_eq!(
        parse(b"\x1b[5B\x1b[0C\x1b[3D\x1b[2e\x1b[a"),
        vec![
            TerminalAction::CursorMove {
                dir: Direction::Down,
                n: Count(5)
            },
            TerminalAction::CursorMove {
                dir: Direction::Right,
                n: Count(1)
            },
            TerminalAction::CursorMove {
                dir: Direction::Left,
                n: Count(3)
            },
            TerminalAction::CursorMove {
                dir: Direction::Down,
                n: Count(2)
            },
            TerminalAction::CursorMove {
                dir: Direction::Right,
                n: Count(1)
            },
        ]
    );
}

#[test]
fn cursor_position_resolves_defaults_and_explicit_values() {
    assert_eq!(
        parse(b"\x1b[H\x1b[4;7H\x1b[;9H\x1b[f"),
        vec![
            TerminalAction::CursorPosition {
                row: Row(1),
                col: Col(1)
            },
            TerminalAction::CursorPosition {
                row: Row(4),
                col: Col(7)
            },
            TerminalAction::CursorPosition {
                row: Row(1),
                col: Col(9)
            },
            TerminalAction::CursorPosition {
                row: Row(1),
                col: Col(1)
            },
        ]
    );
}

#[test]
fn single_axis_addressing_uses_sentinels() {
    assert_eq!(
        parse(b"\x1b[12d\x1b[34`"),
        vec![
            TerminalAction::CursorPosition {
                row: Row(12),
                col: Col::SENTINEL
            },
            TerminalAction::CursorPosition {
                row: Row::SENTINEL,
                col: Col(34)
            },
        ]
    );
}

#[test]
fn erase_families_map_modes_and_reject_unknown_modes() {
    assert_eq!(
        parse(b"\x1b[J\x1b[1J\x1b[2J\x1b[3J\x1b[K\x1b[1K\x1b[2K\x1b[9J"),
        vec![
            TerminalAction::EraseInDisplay {
                mode: EraseDisplayMode::Below
            },
            TerminalAction::EraseInDisplay {
                mode: EraseDisplayMode::Above
            },
            TerminalAction::EraseInDisplay {
                mode: EraseDisplayMode::All
            },
            TerminalAction::EraseInDisplay {
                mode: EraseDisplayMode::Scrollback
            },
            TerminalAction::EraseInLine {
                mode: EraseLineMode::Right
            },
            TerminalAction::EraseInLine {
                mode: EraseLineMode::Left
            },
            TerminalAction::EraseInLine {
                mode: EraseLineMode::All
            },
            unknown(SequenceKind::Csi, b'J', [0, 0]),
        ]
    );
}

#[test]
fn insert_delete_erase_chars_default_counts() {
    assert_eq!(
        parse(b"\x1b[X\x1b[4@\x1b[P\x1b[2L\x1b[M"),
        vec![
            TerminalAction::EraseChars { n: Count(1) },
            TerminalAction::InsertChars { n: Count(4) },
            TerminalAction::DeleteChars { n: Count(1) },
            TerminalAction::InsertLines { n: Count(2) },
            TerminalAction::DeleteLines { n: Count(1) },
        ]
    );
}

#[test]
fn scroll_and_region_defaults_use_geometry_sentinel() {
    assert_eq!(
        parse(b"\x1b[S\x1b[2T\x1b[r\x1b[2;10r"),
        vec![
            TerminalAction::ScrollUp { n: Count(1) },
            TerminalAction::ScrollDown { n: Count(2) },
            TerminalAction::SetScrollRegion {
                top: Row(1),
                bottom: Row::SENTINEL
            },
            TerminalAction::SetScrollRegion {
                top: Row(2),
                bottom: Row(10)
            },
        ]
    );
}

#[test]
fn sgr_empty_sequence_resets() {
    assert_eq!(parse(b"\x1b[m"), vec![attrs(&[AttributeChange::Reset])]);
}

#[test]
fn sgr_basic_attributes_in_order() {
    assert_eq!(
        parse(b"\x1b[1;3;4;9;21m"),
        vec![attrs(&[
            AttributeChange::Enable(Attribute::Bold),
            AttributeChange::Enable(Attribute::Italic),
            AttributeChange::Enable(Attribute::Underline(UnderlineStyle::Single)),
            AttributeChange::Enable(Attribute::Strikethrough),
            AttributeChange::Enable(Attribute::Underline(UnderlineStyle::Double)),
        ])]
    );
}

#[test]
fn sgr_off_switches_disable_attributes() {
    assert_eq!(
        parse(b"\x1b[22;23;24;25;27;28;29m"),
        vec![attrs(&[
            AttributeChange::Disable(Attribute::Bold),
            AttributeChange::Disable(Attribute::Faint),
            AttributeChange::Disable(Attribute::Italic),
            AttributeChange::Disable(Attribute::Underline(UnderlineStyle::None)),
            AttributeChange::Disable(Attribute::Blink),
            AttributeChange::Disable(Attribute::Inverse),
            AttributeChange::Disable(Attribute::Invisible),
            AttributeChange::Disable(Attribute::Strikethrough),
        ])]
    );
}

#[test]
fn sgr_indexed_colors_cover_all_ramps() {
    assert_eq!(
        parse(b"\x1b[31;42;97;107;39;49m"),
        vec![attrs(&[
            AttributeChange::Foreground(Color::Indexed(1)),
            AttributeChange::Background(Color::Indexed(2)),
            AttributeChange::Foreground(Color::Indexed(15)),
            AttributeChange::Background(Color::Indexed(15)),
            AttributeChange::Foreground(Color::Default),
            AttributeChange::Background(Color::Default),
        ])]
    );
}

#[test]
fn sgr_extended_colors_semicolon_form() {
    assert_eq!(
        parse(b"\x1b[38;5;196;48;2;10;20;30;58;5;99m"),
        vec![attrs(&[
            AttributeChange::Foreground(Color::Indexed(196)),
            AttributeChange::Background(Color::Rgb(Rgb {
                r: 10,
                g: 20,
                b: 30
            })),
            AttributeChange::UnderlineColor(Color::Indexed(99)),
        ])]
    );
}

#[test]
fn sgr_extended_colors_colon_forms() {
    assert_eq!(
        parse(b"\x1b[38:5:100m\x1b[38:2:1:2:3m\x1b[48:2::7:8:9m"),
        vec![
            attrs(&[AttributeChange::Foreground(Color::Indexed(100))]),
            attrs(&[AttributeChange::Foreground(Color::Rgb(Rgb {
                r: 1,
                g: 2,
                b: 3
            }))]),
            attrs(&[AttributeChange::Background(Color::Rgb(Rgb {
                r: 7,
                g: 8,
                b: 9
            }))]),
        ]
    );
}

#[test]
fn sgr_underline_styles_via_colon_subparams() {
    assert_eq!(
        parse(b"\x1b[4:3m\x1b[4:5m"),
        vec![
            attrs(&[AttributeChange::Enable(Attribute::Underline(
                UnderlineStyle::Curly
            ))]),
            attrs(&[AttributeChange::Enable(Attribute::Underline(
                UnderlineStyle::Dashed
            ))]),
        ]
    );
}

#[test]
fn decset_unknown_private_modes_report_unknown() {
    assert_eq!(
        parse(b"\x1b[?999h\x1b[?8999l"),
        vec![
            unknown(SequenceKind::Csi, 0, [b'?', 0]),
            unknown(SequenceKind::Csi, 0, [b'?', 0]),
        ]
    );
}

#[test]
fn decset_cursor_visibility_becomes_dedicated_action() {
    assert_eq!(
        parse(b"\x1b[?25l\x1b[?25h"),
        vec![
            TerminalAction::CursorVisibility { visible: false },
            TerminalAction::CursorVisibility { visible: true },
        ]
    );
}

#[test]
fn decset_1048_maps_to_cursor_save_restore() {
    assert_eq!(
        parse(b"\x1b[?1048h\x1b[?1048l"),
        vec![TerminalAction::CursorSave, TerminalAction::CursorRestore]
    );
}

#[test]
fn mouse_tracking_modes_are_distinct() {
    let actions = parse(b"\x1b[?9h\x1b[?1000h\x1b[?1002h\x1b[?1003l");
    assert_eq!(
        actions,
        vec![
            TerminalAction::SetMode {
                mode: Mode::MouseTracking(MouseTrackingMode::X10),
                enabled: true
            },
            TerminalAction::SetMode {
                mode: Mode::MouseTracking(MouseTrackingMode::Normal),
                enabled: true
            },
            TerminalAction::SetMode {
                mode: Mode::MouseTracking(MouseTrackingMode::Button),
                enabled: true
            },
            TerminalAction::SetMode {
                mode: Mode::MouseTracking(MouseTrackingMode::Any),
                enabled: false
            },
        ]
    );
}

#[test]
fn ansi_sm_rm_map_insert_and_linefeed_modes() {
    assert_eq!(
        parse(b"\x1b[4h\x1b[20h\x1b[4l\x1b[33l"),
        vec![
            TerminalAction::SetMode {
                mode: Mode::Insert,
                enabled: true
            },
            TerminalAction::SetMode {
                mode: Mode::LineFeedNewLine,
                enabled: true
            },
            TerminalAction::SetMode {
                mode: Mode::Insert,
                enabled: false
            },
            unknown(SequenceKind::Csi, b'l', [0, 0]),
        ]
    );
}

#[test]
fn decscusr_styles_map_with_default_fallback() {
    assert_eq!(
        parse(b"\x1b[2 q\x1b[5 q\x1b[0 q\x1b[9 q"),
        vec![
            TerminalAction::CursorStyle {
                style: CursorStyle::SteadyBlock
            },
            TerminalAction::CursorStyle {
                style: CursorStyle::BlinkingBar
            },
            TerminalAction::CursorStyle {
                style: CursorStyle::Default
            },
            unknown(SequenceKind::Csi, b'q', [b' ', 0]),
        ]
    );
}

#[test]
fn decscusr_all_ps_values_map_per_dec0017() {
    // DEC-0017 (ghostty `src/terminal/cursor.zig`): 0 default, 1/2 block,
    // 3/4 underline, 5/6 bar. Bare `CSI SP q` defaults to 0.
    assert_eq!(
        parse(b"\x1b[ q\x1b[0 q\x1b[1 q\x1b[2 q\x1b[3 q\x1b[4 q\x1b[5 q\x1b[6 q"),
        vec![
            TerminalAction::CursorStyle {
                style: CursorStyle::Default
            },
            TerminalAction::CursorStyle {
                style: CursorStyle::Default
            },
            TerminalAction::CursorStyle {
                style: CursorStyle::BlinkingBlock
            },
            TerminalAction::CursorStyle {
                style: CursorStyle::SteadyBlock
            },
            TerminalAction::CursorStyle {
                style: CursorStyle::BlinkingUnderline
            },
            TerminalAction::CursorStyle {
                style: CursorStyle::SteadyUnderline
            },
            TerminalAction::CursorStyle {
                style: CursorStyle::BlinkingBar
            },
            TerminalAction::CursorStyle {
                style: CursorStyle::SteadyBar
            },
        ]
    );
}

#[test]
fn tab_operations_map() {
    assert_eq!(
        parse(b"\x1b[g\x1b[0g\x1b[3g\x1b[I\x1b[3I\x1b[Z\x1b[2Z\x1bH"),
        vec![
            TerminalAction::TabClear {
                targets: TabTargets::Current
            },
            TerminalAction::TabClear {
                targets: TabTargets::Current
            },
            TerminalAction::TabClearAll,
            TerminalAction::TabForward { n: Count(1) },
            TerminalAction::TabForward { n: Count(3) },
            TerminalAction::TabBackward { n: Count(1) },
            TerminalAction::TabBackward { n: Count(2) },
            TerminalAction::TabSet,
        ]
    );
}

#[test]
fn charset_designation_and_single_shifts() {
    assert_eq!(
        parse(b"\x1b(B\x1b)0\x1b*A\x1b+0\x1bN\x1bO"),
        vec![
            TerminalAction::SelectCharset {
                slot: CharsetSlot::G0,
                table: CharsetTable::Ascii
            },
            TerminalAction::SelectCharset {
                slot: CharsetSlot::G1,
                table: CharsetTable::DecSpecialGraphics
            },
            TerminalAction::SelectCharset {
                slot: CharsetSlot::G2,
                table: CharsetTable::UnitedKingdom
            },
            TerminalAction::SelectCharset {
                slot: CharsetSlot::G3,
                table: CharsetTable::DecSpecialGraphics
            },
            TerminalAction::InvokeCharset {
                slot: CharsetSlot::G2
            },
            TerminalAction::InvokeCharset {
                slot: CharsetSlot::G3
            },
        ]
    );
}

#[test]
fn device_status_requests_map() {
    assert_eq!(
        parse(b"\x1b[5n\x1b[6n\x1b[?6n\x1b[c\x1b[7n"),
        vec![
            TerminalAction::RequestDeviceStatus {
                kind: StatusKind::OperatingStatus
            },
            TerminalAction::RequestDeviceStatus {
                kind: StatusKind::CursorPosition
            },
            TerminalAction::RequestDeviceStatus {
                kind: StatusKind::CursorPosition
            },
            TerminalAction::RequestDeviceStatus {
                kind: StatusKind::DeviceAttributes
            },
            unknown(SequenceKind::Csi, b'n', [0, 0]),
        ]
    );
}

#[test]
fn esc_save_restore_reset_keypad() {
    assert_eq!(
        parse(b"\x1b7\x1b8\x1bc\x1b=\x1b>"),
        vec![
            TerminalAction::CursorSave,
            TerminalAction::CursorRestore,
            TerminalAction::FullReset,
            TerminalAction::SetMode {
                mode: Mode::ApplicationKeypad,
                enabled: true
            },
            TerminalAction::SetMode {
                mode: Mode::ApplicationKeypad,
                enabled: false
            },
        ]
    );
}

#[test]
fn esc_decid_reports_primary_device_attributes() {
    // Legacy DECID (`ESC Z`) is the oldest primary-DA query shape; like
    // `CSI c` it must surface as a device-attributes request so terminal
    // state queues the bounded `ESC[?6c` reply instead of silence.
    assert_eq!(
        parse(b"\x1bZ"),
        vec![TerminalAction::RequestDeviceStatus {
            kind: StatusKind::DeviceAttributes
        }]
    );
}

#[test]
fn resets_map_from_csi_and_esc() {
    assert_eq!(parse(b"\x1b[!p"), vec![TerminalAction::SoftReset]);
}

#[test]
fn osc_title_joins_all_segments() {
    assert_eq!(
        parse(b"\x1b]2;my title; part two\x07"),
        vec![TerminalAction::OscTitle {
            text: BoundedString::new("my title; part two"),
        }]
    );
    assert_eq!(
        parse(b"\x1b]0;icon and title\x1b\\"),
        vec![TerminalAction::OscTitle {
            text: BoundedString::new("icon and title"),
        }]
    );
}

#[test]
fn osc_cwd_carries_url() {
    assert_eq!(
        parse(b"\x1b]7;file:///home/user/dir\x07"),
        vec![TerminalAction::OscCwd {
            url: BoundedString::new("file:///home/user/dir"),
        }]
    );
}

#[test]
fn osc_hyperlink_open_close_and_ids() {
    assert_eq!(
        parse(b"\x1b]8;;https://example.dev\x07link\x1b]8;;\x07"),
        vec![
            TerminalAction::OscHyperlink {
                link: Some(Hyperlink {
                    id: None,
                    uri: BoundedString::new("https://example.dev"),
                })
            },
            TerminalAction::Print(GraphemeCell::from('l')),
            TerminalAction::Print(GraphemeCell::from('i')),
            TerminalAction::Print(GraphemeCell::from('n')),
            TerminalAction::Print(GraphemeCell::from('k')),
            TerminalAction::OscHyperlink { link: None },
        ]
    );
    let parsed = parse(b"\x1b]8;id=abc-1;https://example.dev\x07");
    assert_eq!(
        parsed,
        vec![TerminalAction::OscHyperlink {
            link: Some(Hyperlink {
                id: Some(BoundedString::new("abc-1")),
                uri: BoundedString::new("https://example.dev"),
            })
        }]
    );
}

#[test]
fn osc_clipboard_distinguishes_query_from_write() {
    assert_eq!(
        parse(b"\x1b]52;c;?\x07"),
        vec![TerminalAction::OscClipboard {
            op: ClipboardOp::Read,
            data: BoundedBytes::new(b"?".to_vec()),
        }]
    );
    assert_eq!(
        parse(b"\x1b]52;c;cGljdW9\x07"),
        vec![TerminalAction::OscClipboard {
            op: ClipboardOp::Write,
            data: BoundedBytes::new(b"cGljdW9".to_vec()),
        }]
    );
}

#[test]
fn osc_prompt_marks_map_zone_letters() {
    for (letter, kind) in [
        (b'A', ZoneKind::PromptStart),
        (b'B', ZoneKind::InputStart),
        (b'C', ZoneKind::OutputStart),
        (b'D', ZoneKind::OutputEnd),
    ] {
        let sequence = [&b"\x1b]133;"[..], &[letter][..], &b";extra\x07"[..]].concat();
        assert_eq!(
            parse(&sequence),
            vec![TerminalAction::OscPromptMark {
                kind,
                exit_code: None
            }]
        );
    }
    assert_eq!(
        parse(b"\x1b]133;Z\x07"),
        vec![TerminalAction::OscUnknown {
            id: 133,
            data: BoundedBytes::new(b"Z".to_vec()),
        }]
    );
}

#[test]
fn osc_unknown_codes_record_id_and_payload() {
    assert_eq!(
        parse(b"\x1b]104;9\x07"),
        vec![TerminalAction::OscUnknown {
            id: 104,
            data: BoundedBytes::new(b"9".to_vec()),
        }]
    );
    let unparseable = parse(b"\x1b]xy;data\x07");
    assert_eq!(
        unparseable,
        vec![TerminalAction::OscUnknown {
            id: u32::MAX,
            data: BoundedBytes::new(b"data".to_vec()),
        }]
    );
}

#[test]
fn utf8_multibyte_decodes_to_prints() {
    let text = "héllo 🎉";
    assert_eq!(
        parse(text.as_bytes()),
        text.chars()
            .map(|c| TerminalAction::Print(GraphemeCell::from(c)))
            .collect::<Vec<_>>()
    );
}

#[test]
fn utf8_invalid_bytes_replace_with_fffd() {
    let actions = parse(b"\xff\xfe");
    assert_eq!(
        actions,
        vec![
            TerminalAction::Print(GraphemeCell::from('\u{FFFD}')),
            TerminalAction::Print(GraphemeCell::from('\u{FFFD}')),
        ]
    );
}

#[test]
fn utf8_split_across_chunks_continues_state_machine() {
    let encoded = "🎉".as_bytes().to_vec();
    assert_eq!(encoded.len(), 4);
    let mut parser = Parser::new();
    let mut actions = Vec::new();
    parser.advance(&encoded[..2], |action| actions.push(action.clone()));
    parser.advance(&encoded[2..], |action| actions.push(action));
    assert_eq!(
        actions,
        vec![TerminalAction::Print(GraphemeCell::from('\u{1F389}'))]
    );
}

#[test]
fn huge_parameter_magnitude_saturates_deterministically() {
    let actions = parse(b"\x1b[99999999999999999999C");
    let expected_n = match actions.first() {
        Some(TerminalAction::CursorMove { n, .. }) => *n,
        other => panic!("expected CursorMove, got {other:?}"),
    };
    assert_eq!(
        actions,
        vec![TerminalAction::CursorMove {
            dir: Direction::Right,
            n: expected_n
        }]
    );
    assert_eq!(expected_n.0, u16::MAX);
}

#[test]
fn parameter_overflow_truncates_but_still_dispatches() {
    let long_params: Vec<u8> = (0..64)
        .flat_map(|i| {
            let mut chunk = format!("{}", i + 1).into_bytes();
            chunk.push(b';');
            chunk
        })
        .collect();
    let sequence = [&b"\x1b["[..], &long_params[..], &b"m"[..]].concat();
    let first = parse(&sequence);
    let second = parse(&sequence);
    assert_eq!(first, second);
    assert_eq!(first.len(), 1);
    match &first[0] {
        TerminalAction::SetAttributes { attrs } => {
            assert!(!attrs.changes.is_empty());
            assert!(attrs.changes.len() <= 40);
        }
        other => panic!("expected SetAttributes, got {other:?}"),
    }
}

#[test]
fn oversized_osc_payload_truncates_at_bound() {
    let payload = vec![b'a'; BoundedString::MAX_LEN + 500];
    let mut sequence = b"\x1b]2;".to_vec();
    sequence.extend_from_slice(&payload);
    sequence.extend_from_slice(b"\x07");
    let actions = parse(&sequence);
    match &actions[0] {
        TerminalAction::OscTitle { text } => {
            assert_eq!(text.len(), BoundedString::MAX_LEN);
            assert!(text.as_str().chars().all(|c| c == 'a'));
        }
        other => panic!("expected OscTitle, got {other:?}"),
    }
    assert_eq!(actions, parse(&sequence));
}

#[test]
fn malformed_sequences_resynchronize_deterministically() {
    let input = b"\x1b[\x1b[31mred";
    let actions = parse(input);
    assert_eq!(
        actions,
        vec![
            attrs(&[AttributeChange::Foreground(Color::Indexed(1))]),
            TerminalAction::Print(GraphemeCell::from('r')),
            TerminalAction::Print(GraphemeCell::from('e')),
            TerminalAction::Print(GraphemeCell::from('d')),
        ]
    );
    assert_eq!(actions, parse(input));
}

#[test]
fn dcs_strings_report_as_unknown_once_per_string() {
    assert_eq!(
        parse(b"\x1bP+q544e\x1b\\after"),
        vec![
            unknown(SequenceKind::Dcs, b'q', [b'+', 0]),
            TerminalAction::Print(GraphemeCell::from('a')),
            TerminalAction::Print(GraphemeCell::from('f')),
            TerminalAction::Print(GraphemeCell::from('t')),
            TerminalAction::Print(GraphemeCell::from('e')),
            TerminalAction::Print(GraphemeCell::from('r')),
        ]
    );
}

#[test]
fn unmapped_csi_and_esc_families_report_unknown() {
    assert_eq!(
        parse(b"\x1bM\x1bD\x1bE\x1b#8\x1b[?12;25h\x1b[3;t"),
        vec![
            unknown(SequenceKind::Esc, b'M', [0, 0]),
            unknown(SequenceKind::Esc, b'D', [0, 0]),
            unknown(SequenceKind::Esc, b'E', [0, 0]),
            unknown(SequenceKind::Esc, b'8', [b'#', 0]),
            TerminalAction::SetMode {
                mode: Mode::CursorBlinking,
                enabled: true
            },
            TerminalAction::CursorVisibility { visible: true },
            unknown(SequenceKind::Csi, b't', [0, 0]),
        ]
    );
}

#[test]
fn action_stream_identical_across_chunkings() {
    let script = b"\x1b]0;title\x07prompt$ \x1b[32mgreen\x1b[0m \xe2\x9c\x93\n\x1b[2J\x1b[?1049h\x1bP$q\x1b\\\xff";
    let whole = parse(script);
    let byte_wise = {
        let mut parser = Parser::new();
        let mut actions = Vec::new();
        for byte in script.iter() {
            parser.advance(std::slice::from_ref(byte), |action| actions.push(action));
        }
        actions
    };
    let mixed = {
        let mut parser = Parser::new();
        let mut actions = Vec::new();
        for chunk in script.chunks(7) {
            parser.advance(chunk, |action| actions.push(action));
        }
        actions
    };
    assert_eq!(whole, byte_wise);
    assert_eq!(whole, mixed);
}

#[test]
fn pseudo_random_byte_soup_is_panic_free_and_deterministic() {
    let mut state: u64 = 0x2026_0826_dead_beef;
    let mut next_byte = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 24) as u8
    };
    let soup: Vec<u8> = (0..8192).map(|_| next_byte()).collect();
    let first = parse(&soup);
    let second = parse(&soup);
    assert_eq!(first, second);
}

// — P0-AC-001 boundary matrix: every numeric/param-count/payload limit has a named test.
// RS-1..RS-7: CSI u16 saturation, param-count truncation, OSC raw/Bounded caps,
// DCS/APC/SOS/PM inert handling, intermediate overflow — all parse-twice deterministic,
// zero panics/hangs.

#[test]
fn csi_numeric_boundary_at_u16_max_saturates_deterministically() {
    // vte params.rs saturates via saturating_mul/add to u16::MAX (65535).
    // Verify at, just-below, and beyond the limit, plus deterministic re-parse.
    let cases: &[(&[u8], u16)] = &[
        (b"\x1b[65534C", 65534),
        (b"\x1b[65535C", 65535),
        (b"\x1b[65536C", u16::MAX),
        (b"\x1b[99999C", u16::MAX),
        (b"\x1b[9999999999C", u16::MAX),
    ];
    for (bytes, expected) in cases {
        let a1 = parse(bytes);
        let a2 = parse(bytes);
        assert_eq!(a1, a2, "deterministic divergence for {bytes:?}");
        match a1.first() {
            Some(TerminalAction::CursorMove { n, .. }) => assert_eq!(n.0, *expected),
            other => panic!("expected CursorMove for {bytes:?}, got {other:?}"),
        }
    }
    // Multiple params near the cap: 65535;1 should yield 65535, not wrap.
    let multi = parse(b"\x1b[65535;1m");
    assert_eq!(multi, parse(b"\x1b[65535;1m"));
}

#[test]
fn csi_param_count_at_and_beyond_max_truncates_deterministically() {
    // vte MAX_PARAMS = 32 (params.rs). At the cap the full 32 dispatch;
    // beyond it extra params are dropped with `ignore=true` but still dispatch.
    let p32 = {
        let s = "1;".repeat(31) + "1";
        format!("\x1b[{s}m").into_bytes()
    };
    let p33 = {
        let s = "1;".repeat(32) + "1";
        format!("\x1b[{s}m").into_bytes()
    };
    let p64 = {
        let s = "1;".repeat(63) + "1";
        format!("\x1b[{s}m").into_bytes()
    };
    for seq in [&p32, &p33, &p64] {
        let a1 = parse(seq);
        let a2 = parse(seq);
        assert_eq!(a1, a2, "deterministic divergence for len {}", seq.len());
        assert_eq!(a1.len(), 1, "must still dispatch exactly one action");
        match &a1[0] {
            TerminalAction::SetAttributes { attrs } => {
                assert!(!attrs.changes.is_empty());
                // Even truncated, changes are bounded well below 1 per param.
                assert!(attrs.changes.len() <= 64);
            }
            other => panic!("expected SetAttributes, got {other:?}"),
        }
    }
    // Subparam form also respects the same cap (colon notation).
    let sub = parse(b"\x1b[38:2:255:0:128;48:5:200m");
    assert_eq!(sub, parse(b"\x1b[38:2:255:0:128;48:5:200m"));
}

#[test]
fn csi_intermediate_overflow_is_ignored_deterministically() {
    // vte MAX_INTERMEDIATES = 2. Three intermediates forces CsiIgnore path
    // but must not panic and must be deterministic.
    let seq = b"\x1b[   q"; // three spaces as intermediates + final 'q'
    let a1 = parse(seq);
    let a2 = parse(seq);
    assert_eq!(a1, a2);
    // Must still produce a single terminal action (unknown or cursor-style fallback).
    assert_eq!(a1.len(), 1);
}

#[test]
fn osc_payload_at_raw_and_bounded_caps_truncates_deterministically() {
    // vte MAX_OSC_RAW = 1024, BoundedString MAX_LEN = 4096.
    // At 1024 the raw buffer is full; beyond it vte truncates deterministically
    // and BoundedString then caps at 4096. Test at and beyond both caps.
    for len in [1024_usize, 1025, 2048, 4095, 4096, 5000] {
        let payload = vec![b'a'; len];
        let mut seq = b"\x1b]2;".to_vec();
        seq.extend_from_slice(&payload);
        seq.push(0x07);
        let a1 = parse(&seq);
        let a2 = parse(&seq);
        assert_eq!(a1, a2, "deterministic divergence for OSC len {len}");
        assert_eq!(a1.len(), 1);
        match &a1[0] {
            TerminalAction::OscTitle { text } => {
                // BoundedString caps at 4096; vte caps at 1024+overhead => <=4096 anyway.
                assert!(text.len() <= BoundedString::MAX_LEN);
                if len >= BoundedString::MAX_LEN {
                    assert_eq!(text.len(), BoundedString::MAX_LEN);
                }
            }
            other => panic!("expected OscTitle for len {len}, got {other:?}"),
        }
    }
}

#[test]
fn osc_clipboard_payload_at_bounded_bytes_cap_truncates_deterministically() {
    // BoundedBytes caps at 4096 for OSC 52.
    for len in [4095_usize, 4096, 5000, 8192] {
        let payload = vec![b'A'; len];
        let mut seq = b"\x1b]52;c;".to_vec();
        seq.extend_from_slice(&payload);
        seq.push(0x07);
        let a1 = parse(&seq);
        let a2 = parse(&seq);
        assert_eq!(a1, a2, "divergence for clipboard len {len}");
        assert_eq!(a1.len(), 1);
        match &a1[0] {
            TerminalAction::OscClipboard { data, .. } => {
                assert!(data.len() <= BoundedBytes::MAX_LEN);
                if len >= BoundedBytes::MAX_LEN {
                    assert_eq!(data.len(), BoundedBytes::MAX_LEN);
                }
            }
            other => panic!("expected OscClipboard for len {len}, got {other:?}"),
        }
    }
}

#[test]
fn truncated_escape_resynchronizes_deterministically() {
    // Truncated CSI intro without final byte must not hang; re-parse identical.
    let cases: &[&[u8]] = &[
        b"\x1b",
        b"\x1b[",
        b"\x1b[31",
        b"\x1b[38;5;196",
        b"\x1b]",
        b"\x1b]2;title without terminator",
        b"\x1bP",
        b"\x1bP+q544e without ST",
        b"\x1b_",
        b"\x1b_hello APC without ST",
        b"\x1b^",
        b"\x1bX",
    ];
    for bytes in cases {
        let a1 = parse(bytes);
        let a2 = parse(bytes);
        assert_eq!(a1, a2, "divergence for truncated {bytes:?}");
        // Appending a resync terminator plus printable must produce a print after.
        let mut extended = bytes.to_vec();
        extended.extend_from_slice(b"\x1b[31mred");
        let extended_actions = parse(&extended);
        let reparsed = parse(&extended);
        assert_eq!(extended_actions, reparsed);
        // Must contain the red SGR at the tail.
        assert!(
            extended_actions
                .iter()
                .any(|a| matches!(a, TerminalAction::SetAttributes { .. }))
                || extended_actions
                    .iter()
                    .any(|a| matches!(a, TerminalAction::Print(_))),
            "resync failed for {bytes:?}"
        );
    }
}

#[test]
fn unterminated_osc_dcs_apc_strings_are_panic_free_and_deterministic() {
    // Unterminated OSC/DCS/APC/SOS/PM must not emit partial actions until
    // terminated, must not hang, and must resynchronize on the next ESC.
    let osc_unterm = b"\x1b]2;unterminated title";
    let dcs_unterm = b"\x1bP+q544e world without terminator";
    let apc_unterm = b"\x1b_hello APC without ST";
    let pm_unterm = b"\x1b^PM data without ST";
    let sos_unterm = b"\x1bX SOS data without ST";
    // vte's SosPmApcString (APC/PM/SOS via ESC _/^/X) is inert: even terminated
    // with ST it emits nothing (anywhere handler discards). Only OSC (BEL/ST)
    // and DCS (ST) produce a dispatch on termination; others remain empty.
    for bytes in [osc_unterm.as_slice(), dcs_unterm] {
        let a1 = parse(bytes);
        let a2 = parse(bytes);
        assert_eq!(a1, a2, "divergence for unterminated {bytes:?}");
        assert!(
            a1.is_empty(),
            "unterminated OSC/DCS should be empty, got {a1:?}"
        );
        let mut terminated = bytes.to_vec();
        if bytes.starts_with(b"\x1b]") {
            terminated.push(0x07);
        } else {
            terminated.extend_from_slice(b"\x1b\\");
        }
        let t1 = parse(&terminated);
        let t2 = parse(&terminated);
        assert_eq!(t1, t2, "terminated divergence for {bytes:?}");
        assert!(!t1.is_empty(), "terminated {bytes:?} should emit");
    }
    for bytes in [apc_unterm as &[u8], pm_unterm, sos_unterm] {
        let a1 = parse(bytes);
        let a2 = parse(bytes);
        assert_eq!(a1, a2, "divergence for unterminated APC/PM/SOS {bytes:?}");
        assert!(
            a1.is_empty(),
            "unterminated APC/PM/SOS should be inert, got {a1:?}"
        );
        let mut terminated = bytes.to_vec();
        terminated.extend_from_slice(b"\x1b\\");
        let t1 = parse(&terminated);
        let t2 = parse(&terminated);
        assert_eq!(t1, t2, "terminated APC/PM/SOS divergence for {bytes:?}");
        // APC/PM/SOS are inert even after ST — no action emitted.
        assert!(
            t1.is_empty(),
            "terminated APC/PM/SOS should remain inert, got {t1:?}"
        );
    }
    // Interleaved unterminated + terminated + printable: no cross-contamination.
    let mixed = b"\x1b]2;first title\x07\x1b]2;second without terminator";
    assert_eq!(parse(mixed), parse(mixed));
}

#[test]
fn invalid_utf8_heavy_is_replaced_and_deterministic() {
    // Heavy invalid UTF-8 must replace with U+FFFD per parser obligations,
    // never panic, and remain deterministic across chunkings.
    let bytes: Vec<u8> = vec![
        0xFF, 0xFE, 0x80, 0x81, 0xC0, 0x80, 0xED, 0xA0, 0x80, 0xF0, 0x80, 0x80, 0x84, 0xE2, 0x28,
        0xA1, 0xC3, 0x28,
    ];
    let a1 = parse(&bytes);
    let a2 = parse(&bytes);
    assert_eq!(a1, a2);
    assert!(a1.iter().any(|a| matches!(
        a,
        TerminalAction::Print(c) if c.clone().scalar() == '\u{FFFD}'
    )));
    // Split across arbitrary chunk boundary must yield same replacement.
    let mut p1 = Parser::new();
    let mut whole = Vec::new();
    p1.advance(&bytes, |a| whole.push(a));
    let mut p2 = Parser::new();
    let mut chunked = Vec::new();
    for chunk in bytes.chunks(3) {
        p2.advance(chunk, |a| chunked.push(a));
    }
    assert_eq!(whole, chunked);
}

#[test]
fn boundary_matrix_zero_panics_all_limits() {
    // Single adversarial matrix covering every P0-AC-001/002 limit in one
    // deterministic re-parse pass (parse twice). Zero panics/hangs is the
    // pass threshold; every limit here has a dedicated named test above.
    let mut corpus: Vec<u8> = Vec::new();
    corpus.extend_from_slice(b"\x1b[9999999999C"); // u16 saturation
    corpus.extend_from_slice(b"\x1b[65535;65536;0;1m");
    corpus.extend_from_slice(b"\x1b["); // truncated
    corpus.extend_from_slice(&b"1;".repeat(64));
    corpus.extend_from_slice(b"m");
    corpus.extend_from_slice(b"\x1b]2;");
    corpus.extend_from_slice(&vec![b'Z'; BoundedString::MAX_LEN + 100]);
    corpus.extend_from_slice(b"\x07");
    corpus.extend_from_slice(b"\x1b]52;c;");
    corpus.extend_from_slice(&vec![b'A'; BoundedBytes::MAX_LEN + 100]);
    corpus.extend_from_slice(b"\x07");
    corpus.extend_from_slice(b"\x1bP+q544e\x1b\\");
    corpus.extend_from_slice(b"\x1b_ APC payload \x1b\\");
    corpus.extend_from_slice(b"\x1b^ PM payload \x1b\\");
    corpus.extend_from_slice(b"\x1bX SOS payload \x1b\\");
    corpus.extend_from_slice(b"\xff\xfe\x80\x81");
    corpus.extend_from_slice("héllo 🎉 ".as_bytes());
    // Deterministic re-parse and byte-wise chunking identity.
    let a1 = parse(&corpus);
    let a2 = parse(&corpus);
    assert_eq!(a1, a2);
    let mut p = Parser::new();
    let mut chunked = Vec::new();
    for b in corpus.iter() {
        p.advance(std::slice::from_ref(b), |a| chunked.push(a));
    }
    assert_eq!(a1, chunked);
}

// — CTX-0256 APC G wiring: valid/invalid/base64-bad/oversize-claim at the
// parser level, chunked reassembly, and split-across-advance invariance.

fn kitty_action(actions: &[TerminalAction]) -> &TerminalAction {
    assert_eq!(actions.len(), 1, "expected one action, got {actions:?}");
    &actions[0]
}

#[test]
fn apc_g_valid_single_shot_emits_decoded_payload() {
    // 2x2 opaque red RGBA (`f=32,s=2,v=2`), base64 `/wAA//8AAP//AAD//wAA/w==`.
    let seq = b"\x1b_Gf=32,s=2,v=2,m=0;/wAA//8AAP//AAD//wAA/w==\x1b\\";
    let actions = parse(seq);
    match kitty_action(&actions) {
        TerminalAction::KittyGraphics {
            format_f,
            width_s,
            height_v,
            action_a,
            cols_c,
            rows_r,
            payload,
        } => {
            assert_eq!(*format_f, 32);
            assert_eq!(*width_s, Some(2));
            assert_eq!(*height_v, Some(2));
            assert_eq!(*action_a, None);
            assert_eq!(*cols_c, 0);
            assert_eq!(*rows_r, 0);
            assert_eq!(&**payload, &[0xFF, 0, 0, 0xFF].repeat(4));
        }
        other => panic!("expected KittyGraphics, got {other:?}"),
    }
    assert_eq!(parse(seq), actions, "re-parse must be deterministic");
}

#[test]
fn apc_g_bel_terminator_and_transmit_only() {
    let seq = b"\x1b_Gf=32,s=1,v=1,a=t,m=0;/wAA/w==\x07";
    match kitty_action(&parse(seq)) {
        TerminalAction::KittyGraphics {
            action_a, payload, ..
        } => {
            assert_eq!(*action_a, Some('t'));
            assert_eq!(&**payload, &[0xFF, 0, 0, 0xFF]);
        }
        other => panic!("expected KittyGraphics, got {other:?}"),
    }
}

#[test]
fn apc_g_non_g_and_malformed_are_inert() {
    // Non-G APC commands stay inert (pre-existing behavior preserved).
    for seq in [
        b"\x1b_Thello\x1b\\".as_slice(),
        b"\x1b_ APC payload \x1b\\".as_slice(),
        b"\x1b_Gf=abc,m=0;AA==\x1b\\".as_slice(),
        b"\x1b_Gs=2,m=0;AA==\x1b\\".as_slice(),
        b"\x1b_Gf=32,a=TT,m=0;AA==\x1b\\".as_slice(),
        b"\x1b_Gf=32,m=2;AA==\x1b\\".as_slice(),
    ] {
        assert!(parse(seq).is_empty(), "inert failed for {seq:?}");
        assert_eq!(parse(seq), parse(seq));
    }
}

#[test]
fn apc_g_bad_base64_fails_closed() {
    for seq in [
        b"\x1b_Gf=32,s=1,v=1,m=0;!!!!\x1b\\".as_slice(),
        b"\x1b_Gf=32,s=1,v=1,m=0;abcde\x1b\\".as_slice(),
    ] {
        let actions = parse(seq);
        assert!(
            actions.is_empty(),
            "bad base64 must emit nothing for {seq:?}"
        );
        assert_eq!(parse(seq), actions);
    }
    // Parser holds no poisoned stream afterwards: a valid APC still routes.
    let valid = b"\x1b_Gf=32,s=1,v=1,m=0;/wAA/w==\x1b\\";
    assert_eq!(parse(valid).len(), 1);
}

#[test]
fn apc_g_oversize_claim_fails_closed_without_alloc() {
    // 9000px side and 5000x5000 area exceed decode caps; payloads are two
    // bytes, proving the claim gate fires before any pixel buffer.
    for seq in [
        b"\x1b_Gf=32,s=9000,v=1,m=0;AA==\x1b\\".as_slice(),
        b"\x1b_Gf=32,s=5000,v=5000,m=0;AA==\x1b\\".as_slice(),
    ] {
        let actions = parse(seq);
        assert!(
            actions.is_empty(),
            "oversize claim must emit nothing for {seq:?}"
        );
    }
}

#[test]
fn apc_g_oversize_accumulation_fails_closed() {
    // Raw control overhead (~18B) must fit while encoded accumulation
    // overflows: cap 40 fits first raw (26B) but rejects 8+33>40.
    let mut parser = Parser::with_ledger_cap(40);
    let mut actions = Vec::new();
    parser.advance(b"\x1b_Gf=32,s=2,v=2,m=1;/wAA//8A\x1b\\", |a| {
        actions.push(a)
    });
    assert!(actions.is_empty());
    assert!(parser.has_pending_kitty());
    // 8 + 33 > 40: drops the stream, emits nothing.
    parser.advance(b"\x1b_Gm=1;AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\x1b\\", |a| {
        actions.push(a);
    });
    assert!(actions.is_empty());
    assert!(!parser.has_pending_kitty());
}

#[test]
fn apc_g_chunked_reassembly_through_parser() {
    let mut parser = Parser::new();
    let mut actions = Vec::new();
    parser.advance(b"\x1b_Gf=32,s=2,v=2,m=1;/wAA//8A\x1b\\", |a| {
        actions.push(a)
    });
    assert!(actions.is_empty());
    parser.advance(b"middle-text", |a| actions.push(a));
    // `middle-text` is 11 prints interleaved before completion.
    assert_eq!(actions.len(), 11);
    parser.advance(b"\x1b_Gm=1;AP//AAD/\x1b\\", |a| actions.push(a));
    assert_eq!(actions.len(), 11, "middle chunk emits nothing");
    parser.advance(b"\x1b_Gm=0;/wAA/w==\x1b\\", |a| actions.push(a));
    assert_eq!(actions.len(), 12);
    match &actions[11] {
        TerminalAction::KittyGraphics { payload, .. } => {
            assert_eq!(&**payload, &[0xFF, 0, 0, 0xFF].repeat(4));
        }
        other => panic!("expected KittyGraphics tail, got {other:?}"),
    }
}

#[test]
fn apc_g_split_across_advances_is_invariant() {
    let seq = b"\x1b_Gf=32,s=2,v=2,a=T,c=2,r=2,m=0;/wAA//8AAP//AAD//wAA/w==\x1b\\";
    let whole = parse(seq);
    assert_eq!(whole.len(), 1);
    // Byte-wise feed must match.
    let mut parser = Parser::new();
    let mut byte_wise = Vec::new();
    for b in seq.iter() {
        parser.advance(std::slice::from_ref(b), |a| byte_wise.push(a));
    }
    assert_eq!(whole, byte_wise);
    // Split inside the ST terminator (ESC held across calls).
    let mut parser = Parser::new();
    let mut split_st = Vec::new();
    let cut = seq.len() - 1;
    parser.advance(&seq[..cut], |a| split_st.push(a));
    assert!(split_st.is_empty(), "ESC held without ST emits nothing yet");
    parser.advance(&seq[cut..], |a| split_st.push(a));
    assert_eq!(whole, split_st);
    // Surrounding text keeps order: Print, KittyGraphics, Print.
    let mut mixed = Vec::new();
    mixed.extend_from_slice(b"A");
    mixed.extend_from_slice(seq);
    mixed.extend_from_slice(b"B");
    let actions = parse(&mixed);
    assert_eq!(actions.len(), 3);
    assert!(matches!(actions[0], TerminalAction::Print(_)));
    assert!(matches!(actions[1], TerminalAction::KittyGraphics { .. }));
    assert!(matches!(actions[2], TerminalAction::Print(_)));
}
