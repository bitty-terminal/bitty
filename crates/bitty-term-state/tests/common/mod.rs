//! Shared helpers for integration tests: a grammar-guided
//! [`TerminalAction`] generator and deterministic damage serialization.
//!
//! The generator mirrors the RFC's "Randomized action-equivalent generators"
//! testing clause: every variant family is reachable, with weights biased
//! toward printing and controls so grids fill up and scroll under pressure.

#![forbid(unsafe_code)]

use proptest::prelude::*;

use bitty_term_state::{AttributeChange, AttributeDiff, Damage, DamagedRegion};
use bitty_vt::{
    Attribute, CharsetSlot, CharsetTable, ClipboardOp, Col, Color, Count, CursorStyle, Direction,
    EraseDisplayMode, EraseLineMode, GraphemeCell, Hyperlink, Mode, MouseCoordinateEncoding,
    MouseTrackingMode, Rgb, Row, SequenceKind, StatusKind, TabTargets, TerminalAction,
    UnderlineStyle, UnrecognizedSequence, ZoneKind,
};

/// Printable ASCII plus CJK/combining samples to exercise width handling.
pub fn printable_char() -> impl Strategy<Value = char> {
    prop_oneof![
        20 => prop::char::range('a', 'z'),
        10 => prop::char::range('0', '9'),
        5 => Just(' '),
        3 => Just('\u{4E2D}'),
        3 => Just('\u{AC00}'),
        2 => Just('\u{301}'),
        1 => Just('\u{1F600}'),
    ]
}

pub fn arb_count() -> impl Strategy<Value = Count> {
    (0u16..=40).prop_map(Count)
}

pub fn arb_row() -> impl Strategy<Value = Row> {
    prop_oneof![2 => Just(Row::SENTINEL), 8 => (1u16..=32).prop_map(Row)]
}

pub fn arb_col() -> impl Strategy<Value = Col> {
    prop_oneof![2 => Just(Col::SENTINEL), 8 => (1u16..=90).prop_map(Col)]
}

pub fn arb_attribute_change() -> impl Strategy<Value = AttributeChange> {
    prop_oneof![
        1 => Just(AttributeChange::Reset),
        4 => (arb_attribute(), any::<bool>())
            .prop_map(|(attr, enabled)| if enabled {
                AttributeChange::Enable(attr)
            } else {
                AttributeChange::Disable(attr)
            }),
        2 => arb_color().prop_map(AttributeChange::Foreground),
        2 => arb_color().prop_map(AttributeChange::Background),
        1 => arb_color().prop_map(AttributeChange::UnderlineColor),
    ]
}

fn arb_attribute() -> impl Strategy<Value = Attribute> {
    prop_oneof![
        Just(Attribute::Bold),
        Just(Attribute::Faint),
        Just(Attribute::Italic),
        Just(Attribute::Underline(UnderlineStyle::Single)),
        Just(Attribute::Underline(UnderlineStyle::Curly)),
        Just(Attribute::Blink),
        Just(Attribute::Inverse),
        Just(Attribute::Invisible),
        Just(Attribute::Strikethrough),
    ]
}

fn arb_color() -> impl Strategy<Value = Color> {
    prop_oneof![
        2 => Just(Color::Default),
        3 => (0u8..=255).prop_map(Color::Indexed),
        2 => (0u8..=255, 0u8..=255, 0u8..=255)
            .prop_map(|(r, g, b)| Color::Rgb(Rgb { r, g, b })),
    ]
}

pub fn arb_mode() -> impl Strategy<Value = Mode> {
    prop_oneof![
        2 => Just(Mode::Insert),
        2 => Just(Mode::LineFeedNewLine),
        2 => Just(Mode::ApplicationKeypad),
        2 => Just(Mode::ApplicationCursorKeys),
        2 => Just(Mode::Column132),
        2 => Just(Mode::ReverseVideo),
        3 => Just(Mode::Origin),
        3 => Just(Mode::AutoWrap),
        2 => Just(Mode::CursorBlinking),
        4 => Just(Mode::AlternateScreen),
        4 => Just(Mode::AlternateScreenClearAndRestore),
        2 => Just(Mode::BracketedPaste),
        2 => Just(Mode::FocusEvents),
        1 => arb_mouse_tracking().prop_map(Mode::MouseTracking),
        1 => arb_mouse_encoding().prop_map(Mode::MouseCoordinateEncoding),
    ]
}

fn arb_mouse_tracking() -> impl Strategy<Value = MouseTrackingMode> {
    prop_oneof![
        Just(MouseTrackingMode::X10),
        Just(MouseTrackingMode::Normal),
        Just(MouseTrackingMode::Button),
        Just(MouseTrackingMode::Any),
    ]
}

fn arb_mouse_encoding() -> impl Strategy<Value = MouseCoordinateEncoding> {
    prop_oneof![
        Just(MouseCoordinateEncoding::Utf8),
        Just(MouseCoordinateEncoding::Sgr),
        Just(MouseCoordinateEncoding::Urxvt),
    ]
}

/// One arbitrary terminal action; every variant family is reachable.
pub fn arb_action() -> impl Strategy<Value = TerminalAction> {
    prop_oneof![
        24 => printable_char().prop_map(|c| TerminalAction::Print(GraphemeCell::from(c))),
        6 => any::<u8>().prop_map(|b| TerminalAction::PrintControl(bitty_vt::ControlChar(b))),
        3 => (arb_direction(), arb_count())
            .prop_map(|(dir, n)| TerminalAction::CursorMove { dir, n }),
        3 => (arb_row(), arb_col())
            .prop_map(|(row, col)| TerminalAction::CursorPosition { row, col }),
        1 => Just(TerminalAction::CursorSave),
        1 => Just(TerminalAction::CursorRestore),
        1 => arb_cursor_style().prop_map(|style| TerminalAction::CursorStyle { style }),
        1 => any::<bool>().prop_map(|visible| TerminalAction::CursorVisibility { visible }),
        2 => arb_erase_display().prop_map(|mode| TerminalAction::EraseInDisplay { mode }),
        2 => arb_erase_line().prop_map(|mode| TerminalAction::EraseInLine { mode }),
        1 => arb_count().prop_map(|n| TerminalAction::EraseChars { n }),
        1 => arb_count().prop_map(|n| TerminalAction::InsertLines { n }),
        1 => arb_count().prop_map(|n| TerminalAction::DeleteLines { n }),
        1 => arb_count().prop_map(|n| TerminalAction::InsertChars { n }),
        1 => arb_count().prop_map(|n| TerminalAction::DeleteChars { n }),
        1 => arb_count().prop_map(|n| TerminalAction::ScrollUp { n }),
        1 => arb_count().prop_map(|n| TerminalAction::ScrollDown { n }),
        1 => (arb_row(), arb_row())
            .prop_map(|(top, bottom)| TerminalAction::SetScrollRegion { top, bottom }),
        3 => prop::collection::vec(arb_attribute_change(), 1..=6).prop_map(|changes| {
            TerminalAction::SetAttributes {
                attrs: AttributeDiff {
                    changes: changes.into_boxed_slice(),
                },
            }
        }),
        3 => (arb_mode(), any::<bool>())
            .prop_map(|(mode, enabled)| TerminalAction::SetMode { mode, enabled }),
        1 => Just(TerminalAction::TabSet),
        1 => Just(TabTargets::Current).prop_map(|targets| TerminalAction::TabClear { targets }),
        1 => Just(TerminalAction::TabClearAll),
        1 => arb_count().prop_map(|n| TerminalAction::TabForward { n }),
        1 => arb_count().prop_map(|n| TerminalAction::TabBackward { n }),
        1 => (arb_slot(), arb_table())
            .prop_map(|(slot, table)| TerminalAction::SelectCharset { slot, table }),
        1 => arb_slot().prop_map(|slot| TerminalAction::InvokeCharset { slot }),
        1 => arb_status_kind().prop_map(|kind| TerminalAction::RequestDeviceStatus { kind }),
        1 => prop::collection::vec(any::<u8>(), 0..=64)
            .prop_map(|bytes| TerminalAction::Reply { bytes: bytes.into_boxed_slice() }),
        1 => ".*".prop_map(|text| TerminalAction::OscTitle {
            text: bitty_vt::BoundedString::new(text),
        }),
        1 => (arb_clipboard_op(), prop::collection::vec(any::<u8>(), 0..=32))
            .prop_map(|(op, data)| TerminalAction::OscClipboard {
                op,
                data: bitty_vt::BoundedBytes::new(data),
            }),
        1 => "[a-z]+://[a-z]{3,8}".prop_map(|url| TerminalAction::OscCwd {
            url: bitty_vt::BoundedString::new(url),
        }),
        1 => prop::option::of(("[a-z0-9]{0,4}", "[a-z]+://[a-z]{3,8}")).prop_map(|link| {
            TerminalAction::OscHyperlink {
                link: link.map(|(id, uri)| Hyperlink {
                    id: Some(bitty_vt::BoundedString::new(id)),
                    uri: bitty_vt::BoundedString::new(uri),
                }),
            }
        }),
        1 => (arb_zone_kind(), prop::option::of(-128..=128))
            .prop_map(|(kind, code)| TerminalAction::OscPromptMark {
                kind,
                exit_code: if kind == ZoneKind::OutputEnd { code } else { None },
            }),
        1 => (any::<u32>(), prop::collection::vec(any::<u8>(), 0..=32)).prop_map(
            |(id, data)| TerminalAction::OscUnknown {
                id,
                data: bitty_vt::BoundedBytes::new(data),
            },
        ),
        1 => (arb_sequence_kind(), any::<u8>(), prop::collection::vec(any::<u8>(), 0..=2))
            .prop_map(|(kind, final_byte, intermediates)| {
                let mut slots = [0u8; 2];
                for (i, b) in intermediates.iter().take(2).enumerate() {
                    slots[i] = *b;
                }
                TerminalAction::Unknown(UnrecognizedSequence {
                    kind,
                    final_byte,
                    intermediates: slots,
                })
            }),
        1 => Just(TerminalAction::SoftReset),
        1 => Just(TerminalAction::FullReset),
    ]
}

fn arb_direction() -> impl Strategy<Value = Direction> {
    prop_oneof![
        Just(Direction::Up),
        Just(Direction::Down),
        Just(Direction::Right),
        Just(Direction::Left),
    ]
}

fn arb_cursor_style() -> impl Strategy<Value = CursorStyle> {
    prop_oneof![
        Just(CursorStyle::Default),
        Just(CursorStyle::BlinkingBlock),
        Just(CursorStyle::SteadyBlock),
        Just(CursorStyle::BlinkingUnderline),
        Just(CursorStyle::SteadyUnderline),
        Just(CursorStyle::BlinkingBar),
        Just(CursorStyle::SteadyBar),
    ]
}

fn arb_erase_display() -> impl Strategy<Value = EraseDisplayMode> {
    prop_oneof![
        Just(EraseDisplayMode::Below),
        Just(EraseDisplayMode::Above),
        Just(EraseDisplayMode::All),
        Just(EraseDisplayMode::Scrollback),
    ]
}

fn arb_erase_line() -> impl Strategy<Value = EraseLineMode> {
    prop_oneof![
        Just(EraseLineMode::Right),
        Just(EraseLineMode::Left),
        Just(EraseLineMode::All),
    ]
}

fn arb_slot() -> impl Strategy<Value = CharsetSlot> {
    prop_oneof![
        Just(CharsetSlot::G0),
        Just(CharsetSlot::G1),
        Just(CharsetSlot::G2),
        Just(CharsetSlot::G3),
    ]
}

fn arb_table() -> impl Strategy<Value = CharsetTable> {
    prop_oneof![
        Just(CharsetTable::Ascii),
        Just(CharsetTable::UnitedKingdom),
        Just(CharsetTable::DecSpecialGraphics),
    ]
}

fn arb_status_kind() -> impl Strategy<Value = StatusKind> {
    prop_oneof![
        Just(StatusKind::OperatingStatus),
        Just(StatusKind::CursorPosition),
        Just(StatusKind::DeviceAttributes),
    ]
}

fn arb_clipboard_op() -> impl Strategy<Value = ClipboardOp> {
    prop_oneof![Just(ClipboardOp::Read), Just(ClipboardOp::Write)]
}

fn arb_zone_kind() -> impl Strategy<Value = ZoneKind> {
    prop_oneof![
        Just(ZoneKind::PromptStart),
        Just(ZoneKind::InputStart),
        Just(ZoneKind::OutputStart),
        Just(ZoneKind::OutputEnd),
    ]
}

fn arb_sequence_kind() -> impl Strategy<Value = SequenceKind> {
    prop_oneof![
        Just(SequenceKind::Csi),
        Just(SequenceKind::Esc),
        Just(SequenceKind::Dcs),
    ]
}

/// Deterministic little-endian encoding of one damage batch, so tests can
/// assert byte-identical damage streams (RFC replay guarantees).
#[allow(dead_code)] // shared by several integration suites; not all use it
pub fn damage_bytes(damage: &Damage) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + damage.regions.len() * 12);
    out.extend_from_slice(&damage.generation.to_le_bytes());
    out.extend_from_slice(&(damage.regions.len() as u32).to_le_bytes());
    for region in &damage.regions {
        match region {
            DamagedRegion::Grid(rect) => {
                out.push(1);
                out.extend_from_slice(&rect.top.to_le_bytes());
                out.extend_from_slice(&rect.left.to_le_bytes());
                out.extend_from_slice(&rect.bottom.to_le_bytes());
                out.extend_from_slice(&rect.right.to_le_bytes());
            }
            DamagedRegion::Scrollback {
                first_line_id,
                count,
            } => {
                out.push(2);
                out.extend_from_slice(&first_line_id.to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
            }
        }
    }
    out
}
