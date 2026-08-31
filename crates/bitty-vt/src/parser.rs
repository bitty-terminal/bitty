//! Byte-stream VT parser producing [`TerminalAction`] values.
//!
//! Wraps the adopted `vte` state machine (ADR-0004) behind this crate's own
//! API: `vte` types never appear in the public surface. The [`Perform`]
//! implementation maps every callback onto the RFC action interface; UTF-8
//! collection is delegated to `vte`, whose decoder replaces invalid bytes
//! with U+FFFD, matching the single specified replacement policy of the
//! RFC's parser obligations.
//!
//! Bounded-parsing obligations are satisfied jointly: `vte` bounds parameter
//! count (extra parameters are dropped and flagged via `ignore`),
//! parameter magnitude (`u16` saturation), and OSC payload size (fixed
//! buffer); this module additionally bounds every materialized string or
//! byte payload through the [`crate::bounded`] types. Exceeding any limit
//! therefore yields a well-defined truncated action rather than unbounded
//! growth (threat T-01).
//!
//! The crate holds no terminal state: the only memory retained across input
//! chunks is the `vte` machine itself plus a pending device-control-string
//! marker needed to report terminated-but-unmapped string sequences.

use crate::action::{
    Attribute, AttributeChange, AttributeDiff, CharsetSlot, CharsetTable, ClipboardOp, Col, Color,
    ControlChar, Count, CursorStyle, Direction, EraseDisplayMode, EraseLineMode, GraphemeCell,
    Hyperlink, Mode, MouseCoordinateEncoding, MouseTrackingMode, Rgb, Row, SequenceKind,
    StatusKind, TabTargets, TerminalAction, UnderlineStyle, UnrecognizedSequence, ZoneKind,
};
use crate::bounded::{BoundedBytes, BoundedString};
use vte::{Params, Perform};

/// Stateful byte-stream parser: wraps a `vte::Parser` and translates its
/// callbacks into semantic [`TerminalAction`] values via the `emit` sink.
///
/// No terminal state lives here; see the crate-level documentation for the
/// parser/state split mandated by ADR-0003.
pub struct Parser {
    state_machine: vte::Parser,
    dcs: PendingDcs,
}

impl std::fmt::Debug for Parser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parser")
            .field("dcs", &self.dcs)
            .finish_non_exhaustive()
    }
}

/// Marker for a device-control string opened by `hook` and closed by
/// `unhook`, so unmapped strings can be reported once, deterministically.
#[derive(Debug, Default)]
struct PendingDcs {
    active: bool,
    final_byte: u8,
    intermediates: [u8; 2],
}

impl Parser {
    /// Creates a fresh parser.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state_machine: vte::Parser::new(),
            dcs: PendingDcs::default(),
        }
    }

    /// Feeds raw PTY bytes into the parser, emitting one [`TerminalAction`]
    /// per resolved semantic event into `emit`.
    ///
    /// Parsing may be resumed across arbitrary chunk boundaries; splitting
    /// the same byte stream differently does not change the emitted action
    /// sequence.
    pub fn advance<F>(&mut self, bytes: &[u8], emit: F)
    where
        F: FnMut(TerminalAction),
    {
        let Self { state_machine, dcs } = self;
        let mut bridge = Bridge { emit, dcs };
        state_machine.advance(&mut bridge, bytes);
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

struct Bridge<'a, F> {
    emit: F,
    dcs: &'a mut PendingDcs,
}

impl<F: FnMut(TerminalAction)> Bridge<'_, F> {
    fn emit(&mut self, action: TerminalAction) {
        (self.emit)(action);
    }

    fn unknown_csi(&mut self, intermediates: &[u8], final_byte: u8) {
        self.emit(TerminalAction::Unknown(UnrecognizedSequence {
            kind: SequenceKind::Csi,
            final_byte,
            intermediates: pack_intermediates(intermediates),
        }));
    }

    fn unknown_esc(&mut self, intermediates: &[u8], final_byte: u8) {
        self.emit(TerminalAction::Unknown(UnrecognizedSequence {
            kind: SequenceKind::Esc,
            final_byte,
            intermediates: pack_intermediates(intermediates),
        }));
    }

    fn kitty_flags_from_sub(sub: &[u16]) -> u32 {
        // Progressive Kitty flags: sub[0] == 7727, remaining entries are colon-
        // separated flag identifiers. Each identifier is either a 1-indexed flag
        // number (1..5 -> bit 0..4) or a direct bitmask fragment. We handle both:
        // values 1..5 map via 1 << (v-1), values 6..31 are treated as direct
        // mask fragments (masked to 0x1F). This covers `1:2:5` -> 19 and `19`
        // -> 19 deterministically, bounded to 5 bits.
        if sub.len() <= 1 {
            return 1;
        }
        let mut flags: u32 = 0;
        for &v in &sub[1..] {
            if v == 0 {
                continue;
            }
            if (1..=5).contains(&v) {
                flags |= 1u32 << (v - 1);
            } else {
                flags |= u32::from(v) & 0x1F;
            }
        }
        if flags == 0 { 1 } else { flags & 0x1F }
    }

    fn dispatch_mode(&mut self, params: &Params, index: usize, enabled: bool) {
        let sub = sub_params(params, index).unwrap_or(&[]);
        let code = sub.first().copied().unwrap_or(0);
        let mapped = match code {
            1 => Some(Mode::ApplicationCursorKeys),
            3 => Some(Mode::Column132),
            5 => Some(Mode::ReverseVideo),
            6 => Some(Mode::Origin),
            7 => Some(Mode::AutoWrap),
            9 => Some(Mode::MouseTracking(MouseTrackingMode::X10)),
            12 => Some(Mode::CursorBlinking),
            47 | 1047 => Some(Mode::AlternateScreen),
            1049 => Some(Mode::AlternateScreenClearAndRestore),
            1000 => Some(Mode::MouseTracking(MouseTrackingMode::Normal)),
            1002 => Some(Mode::MouseTracking(MouseTrackingMode::Button)),
            1003 => Some(Mode::MouseTracking(MouseTrackingMode::Any)),
            1004 => Some(Mode::FocusEvents),
            1005 => Some(Mode::MouseCoordinateEncoding(MouseCoordinateEncoding::Utf8)),
            1006 => Some(Mode::MouseCoordinateEncoding(MouseCoordinateEncoding::Sgr)),
            1015 => Some(Mode::MouseCoordinateEncoding(
                MouseCoordinateEncoding::Urxvt,
            )),
            2004 => Some(Mode::BracketedPaste),
            7727 => {
                let flags = if enabled {
                    Self::kitty_flags_from_sub(sub)
                } else if sub.len() > 1 {
                    // Progressive disable: extract flags to clear; if none, 0 means all
                    let mut f: u32 = 0;
                    for &v in &sub[1..] {
                        if v == 0 {
                            continue;
                        }
                        if (1..=5).contains(&v) {
                            f |= 1u32 << (v - 1);
                        } else {
                            f |= u32::from(v) & 0x1F;
                        }
                    }
                    f & 0x1F
                } else {
                    0
                };
                Some(Mode::KittyKeyboard(flags))
            }
            _ => None,
        };
        match mapped {
            Some(mode) => self.emit(TerminalAction::SetMode { mode, enabled }),
            None => {
                self.emit(TerminalAction::Unknown(UnrecognizedSequence {
                    kind: SequenceKind::Csi,
                    final_byte: 0,
                    intermediates: [b'?', 0],
                }));
            }
        }
    }
}

fn pack_intermediates(intermediates: &[u8]) -> [u8; 2] {
    let mut packed = [0_u8; 2];
    for (slot, byte) in packed.iter_mut().zip(intermediates) {
        *slot = *byte;
    }
    packed
}

fn sub_params(params: &Params, index: usize) -> Option<&[u16]> {
    params.iter().nth(index)
}

fn lead_value(sub: Option<&[u16]>) -> Option<u16> {
    match sub?.first().copied() {
        Some(0) | None => None,
        Some(value) => Some(value),
    }
}

fn resolved_count(params: &Params, index: usize) -> Count {
    Count(lead_value(sub_params(params, index)).unwrap_or(Count::DEFAULT.0))
}

fn resolved_coordinate(params: &Params, index: usize) -> u16 {
    lead_value(sub_params(params, index)).unwrap_or(Col::DEFAULT.0)
}

fn mode_value(params: &Params, index: usize) -> u16 {
    sub_params(params, index)
        .and_then(<[u16]>::first)
        .copied()
        .unwrap_or(0)
}

fn color_from_rgb(values: &[u16]) -> Option<Color> {
    if values.len() < 3 {
        return None;
    }
    let clamp = |v: u16| u8::try_from(v).unwrap_or(u8::MAX);
    Some(Color::Rgb(Rgb {
        r: clamp(values[0]),
        g: clamp(values[1]),
        b: clamp(values[2]),
    }))
}

fn extended_color(
    changes: &mut Vec<AttributeChange>,
    target: ColorTarget,
    params: &Params,
    current: usize,
    current_sub: &[u16],
) -> usize {
    if current_sub.len() > 1 {
        match current_sub[1] {
            5 => {
                if let Some(index) = current_sub.get(2) {
                    let color = Color::Indexed(u8::try_from(*index).unwrap_or(u8::MAX));
                    changes.push(change_for(target, color));
                }
                return 1;
            }
            2 => {
                let rest = &current_sub[2..];
                let color = if rest.len() >= 4 {
                    color_from_rgb(&rest[1..4])
                } else {
                    color_from_rgb(rest)
                };
                if let Some(color) = color {
                    changes.push(change_for(target, color));
                }
                return 1;
            }
            _ => return 1,
        }
    }
    match sub_params(params, current + 1).and_then(<[u16]>::first) {
        Some(5) => {
            let index = sub_params(params, current + 2)
                .and_then(<[u16]>::first)
                .copied()
                .unwrap_or(0);
            changes.push(change_for(
                target,
                Color::Indexed(u8::try_from(index).unwrap_or(u8::MAX)),
            ));
            3
        }
        Some(2) => {
            let rgb: Vec<u16> = (2..=4)
                .filter_map(|offset| {
                    sub_params(params, current + offset)
                        .and_then(<[u16]>::first)
                        .copied()
                })
                .collect();
            if let Some(color) = color_from_rgb(&rgb) {
                changes.push(change_for(target, color));
            }
            5
        }
        _ => 1,
    }
}

enum ColorTarget {
    Foreground,
    Background,
    UnderlineColor,
}

fn change_for(target: ColorTarget, color: Color) -> AttributeChange {
    match target {
        ColorTarget::Foreground => AttributeChange::Foreground(color),
        ColorTarget::Background => AttributeChange::Background(color),
        ColorTarget::UnderlineColor => AttributeChange::UnderlineColor(color),
    }
}

fn parse_underline_style(style: u16) -> Option<UnderlineStyle> {
    match style {
        0 => Some(UnderlineStyle::None),
        1 => Some(UnderlineStyle::Single),
        2 => Some(UnderlineStyle::Double),
        3 => Some(UnderlineStyle::Curly),
        4 => Some(UnderlineStyle::Dotted),
        5 => Some(UnderlineStyle::Dashed),
        _ => None,
    }
}

fn parse_sgr(params: &Params) -> TerminalAction {
    let mut changes = Vec::new();
    let mut index = 0;
    while let Some(sub) = sub_params(params, index) {
        let code = sub.first().copied().unwrap_or(0);
        let consumed = match code {
            0 => {
                changes.push(AttributeChange::Reset);
                1
            }
            1 => {
                changes.push(AttributeChange::Enable(Attribute::Bold));
                1
            }
            2 => {
                changes.push(AttributeChange::Enable(Attribute::Faint));
                1
            }
            3 => {
                changes.push(AttributeChange::Enable(Attribute::Italic));
                1
            }
            4 => {
                let style = sub
                    .get(1)
                    .and_then(|&style| parse_underline_style(style))
                    .unwrap_or(UnderlineStyle::Single);
                changes.push(AttributeChange::Enable(Attribute::Underline(style)));
                1
            }
            5 => {
                changes.push(AttributeChange::Enable(Attribute::Blink));
                1
            }
            7 => {
                changes.push(AttributeChange::Enable(Attribute::Inverse));
                1
            }
            8 => {
                changes.push(AttributeChange::Enable(Attribute::Invisible));
                1
            }
            9 => {
                changes.push(AttributeChange::Enable(Attribute::Strikethrough));
                1
            }
            21 => {
                changes.push(AttributeChange::Enable(Attribute::Underline(
                    UnderlineStyle::Double,
                )));
                1
            }
            22 => {
                changes.push(AttributeChange::Disable(Attribute::Bold));
                changes.push(AttributeChange::Disable(Attribute::Faint));
                1
            }
            23 => {
                changes.push(AttributeChange::Disable(Attribute::Italic));
                1
            }
            24 => {
                changes.push(AttributeChange::Disable(Attribute::Underline(
                    UnderlineStyle::None,
                )));
                1
            }
            25 => {
                changes.push(AttributeChange::Disable(Attribute::Blink));
                1
            }
            27 => {
                changes.push(AttributeChange::Disable(Attribute::Inverse));
                1
            }
            28 => {
                changes.push(AttributeChange::Disable(Attribute::Invisible));
                1
            }
            29 => {
                changes.push(AttributeChange::Disable(Attribute::Strikethrough));
                1
            }
            30..=37 => {
                changes.push(AttributeChange::Foreground(Color::Indexed(
                    (code - 30) as u8,
                )));
                1
            }
            39 => {
                changes.push(AttributeChange::Foreground(Color::Default));
                1
            }
            40..=47 => {
                changes.push(AttributeChange::Background(Color::Indexed(
                    (code - 40) as u8,
                )));
                1
            }
            49 => {
                changes.push(AttributeChange::Background(Color::Default));
                1
            }
            58 => extended_color(
                &mut changes,
                ColorTarget::UnderlineColor,
                params,
                index,
                sub,
            ),
            59 => {
                changes.push(AttributeChange::UnderlineColor(Color::Default));
                1
            }
            90..=97 => {
                changes.push(AttributeChange::Foreground(Color::Indexed(
                    (code - 90 + 8) as u8,
                )));
                1
            }
            100..=107 => {
                changes.push(AttributeChange::Background(Color::Indexed(
                    (code - 100 + 8) as u8,
                )));
                1
            }
            38 => extended_color(&mut changes, ColorTarget::Foreground, params, index, sub),
            48 => extended_color(&mut changes, ColorTarget::Background, params, index, sub),
            _ => 1,
        };
        index += consumed;
    }
    if changes.is_empty() {
        changes.push(AttributeChange::Reset);
    }
    TerminalAction::SetAttributes {
        attrs: AttributeDiff {
            changes: changes.into_boxed_slice(),
        },
    }
}

fn osc_id(params: &[&[u8]]) -> u32 {
    params
        .first()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|text| text.parse::<u32>().ok())
        .unwrap_or(u32::MAX)
}

fn join_segments(params: &[&[u8]]) -> Vec<u8> {
    let mut joined = Vec::new();
    for (position, segment) in params.iter().enumerate() {
        if position > 0 {
            joined.push(b';');
        }
        joined.extend_from_slice(segment);
    }
    joined
}

impl<F: FnMut(TerminalAction)> Perform for Bridge<'_, F> {
    fn print(&mut self, c: char) {
        self.emit(TerminalAction::Print(GraphemeCell::from(c)));
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x0E => self.emit(TerminalAction::InvokeCharset {
                slot: CharsetSlot::G1,
            }),
            0x0F => self.emit(TerminalAction::InvokeCharset {
                slot: CharsetSlot::G0,
            }),
            other => self.emit(TerminalAction::PrintControl(ControlChar(other))),
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let final_byte = u8::try_from(u32::from(action)).unwrap_or(0);
        let private = intermediates.contains(&b'?');

        if intermediates == *b" " && final_byte == b'q' {
            let style = match mode_value(params, 0) {
                0 => CursorStyle::Default,
                1 => CursorStyle::BlinkingBlock,
                2 => CursorStyle::SteadyBlock,
                3 => CursorStyle::BlinkingUnderline,
                4 => CursorStyle::SteadyUnderline,
                5 => CursorStyle::BlinkingBar,
                6 => CursorStyle::SteadyBar,
                _ => {
                    self.unknown_csi(intermediates, final_byte);
                    return;
                }
            };
            self.emit(TerminalAction::CursorStyle { style });
            return;
        }

        if intermediates == *b"!" && final_byte == b'p' {
            self.emit(TerminalAction::SoftReset);
            return;
        }

        if private && final_byte == b'n' {
            if resolved_count(params, 0).0 == 6 {
                self.emit(TerminalAction::RequestDeviceStatus {
                    kind: StatusKind::CursorPosition,
                });
            } else {
                self.unknown_csi(intermediates, final_byte);
            }
            return;
        }

        if private && matches!(final_byte, b'h' | b'l') {
            let enabled = final_byte == b'h';
            let mut idx = 0;
            while idx < params.len() {
                let sub = sub_params(params, idx).unwrap_or(&[]);
                let code = sub.first().copied().unwrap_or(0);
                if code == 25 {
                    self.emit(TerminalAction::CursorVisibility { visible: enabled });
                    idx += 1;
                } else if code == 1048 {
                    self.emit(if enabled {
                        TerminalAction::CursorSave
                    } else {
                        TerminalAction::CursorRestore
                    });
                    idx += 1;
                } else if code == 7727 {
                    // Progressive Kitty flags: colon subparams inside same entry plus
                    // semicolon-separated flag masks immediately following this entry.
                    let mut flags = if enabled {
                        Self::kitty_flags_from_sub(sub)
                    } else if sub.len() > 1 {
                        let mut f: u32 = 0;
                        for &v in &sub[1..] {
                            if v == 0 {
                                continue;
                            }
                            if (1..=5).contains(&v) {
                                f |= 1u32 << (v - 1);
                            } else {
                                f |= u32::from(v) & 0x1F;
                            }
                        }
                        f & 0x1F
                    } else {
                        0
                    };
                    let had_colon = sub.len() > 1;
                    let mut consumed = 0usize;
                    let mut agg = flags;
                    let mut look = idx + 1;
                    // Consume following `;`-separated flag values (small numbers) as part of same Kitty negotiation.
                    // This matches the `;`-separated bitmask description while keeping distinct mode numbers like 1000 separate.
                    while look < params.len() {
                        let next_sub = sub_params(params, look).unwrap_or(&[]);
                        if next_sub.is_empty() {
                            break;
                        }
                        let next_code = next_sub[0];
                        if next_code == 7727 {
                            break;
                        }
                        if next_sub.len() > 1 {
                            break;
                        }
                        if next_code > 31 {
                            break;
                        }
                        if next_code == 25 || next_code == 1048 {
                            break;
                        }
                        // Treat as Kitty flag fragment
                        if !had_colon && consumed == 0 && agg == 1 {
                            // `7727` alone defaults to 1; a following `;19` should replace it, not OR with 1
                            agg = 0;
                        }
                        let add: u32 = u32::from(next_code) & 0x1F;
                        agg |= add;
                        consumed += 1;
                        look += 1;
                    }
                    if consumed > 0 {
                        flags = agg & 0x1F;
                        if flags == 0 && enabled {
                            flags = 1;
                        }
                    }
                    let mode = Mode::KittyKeyboard(flags);
                    self.emit(TerminalAction::SetMode { mode, enabled });
                    idx += 1 + consumed;
                } else {
                    self.dispatch_mode(params, idx, enabled);
                    idx += 1;
                }
            }
            return;
        }

        if !intermediates.is_empty() {
            self.unknown_csi(intermediates, final_byte);
            return;
        }

        match final_byte {
            b'A' | b'B' | b'C' | b'D' | b'a' | b'e' => {
                let dir = match final_byte {
                    b'A' => Direction::Up,
                    b'B' | b'e' => Direction::Down,
                    b'C' | b'a' => Direction::Right,
                    _ => Direction::Left,
                };
                self.emit(TerminalAction::CursorMove {
                    dir,
                    n: resolved_count(params, 0),
                });
            }
            b'H' | b'f' => self.emit(TerminalAction::CursorPosition {
                row: Row(resolved_coordinate(params, 0)),
                col: Col(resolved_coordinate(params, 1)),
            }),
            b'd' => self.emit(TerminalAction::CursorPosition {
                row: Row(resolved_coordinate(params, 0)),
                col: Col::SENTINEL,
            }),
            b'`' => self.emit(TerminalAction::CursorPosition {
                row: Row::SENTINEL,
                col: Col(resolved_coordinate(params, 0)),
            }),
            b'J' | b'K' => {
                let mode = mode_value(params, 0);
                if final_byte == b'J' {
                    let display = match mode {
                        0 => EraseDisplayMode::Below,
                        1 => EraseDisplayMode::Above,
                        2 => EraseDisplayMode::All,
                        3 => EraseDisplayMode::Scrollback,
                        _ => {
                            self.unknown_csi(intermediates, final_byte);
                            return;
                        }
                    };
                    self.emit(TerminalAction::EraseInDisplay { mode: display });
                } else {
                    let line = match mode {
                        0 => EraseLineMode::Right,
                        1 => EraseLineMode::Left,
                        2 => EraseLineMode::All,
                        _ => {
                            self.unknown_csi(intermediates, final_byte);
                            return;
                        }
                    };
                    self.emit(TerminalAction::EraseInLine { mode: line });
                }
            }
            b'X' => self.emit(TerminalAction::EraseChars {
                n: resolved_count(params, 0),
            }),
            b'@' => self.emit(TerminalAction::InsertChars {
                n: resolved_count(params, 0),
            }),
            b'P' => self.emit(TerminalAction::DeleteChars {
                n: resolved_count(params, 0),
            }),
            b'L' => self.emit(TerminalAction::InsertLines {
                n: resolved_count(params, 0),
            }),
            b'M' => self.emit(TerminalAction::DeleteLines {
                n: resolved_count(params, 0),
            }),
            b'S' => self.emit(TerminalAction::ScrollUp {
                n: resolved_count(params, 0),
            }),
            b'T' => {
                if params.len() <= 1 {
                    self.emit(TerminalAction::ScrollDown {
                        n: resolved_count(params, 0),
                    });
                } else {
                    self.unknown_csi(intermediates, final_byte);
                }
            }
            b'r' => {
                let bottom = if params.len() > 1 {
                    Row(resolved_coordinate(params, 1))
                } else {
                    Row::SENTINEL
                };
                self.emit(TerminalAction::SetScrollRegion {
                    top: Row(resolved_coordinate(params, 0)),
                    bottom,
                });
            }
            b'm' => {
                let action = parse_sgr(params);
                self.emit(action);
            }
            b'h' | b'l' => {
                let enabled = final_byte == b'h';
                for position in 0..params.len() {
                    let code = sub_params(params, position)
                        .and_then(<[u16]>::first)
                        .copied()
                        .unwrap_or(0);
                    match code {
                        4 => self.emit(TerminalAction::SetMode {
                            mode: Mode::Insert,
                            enabled,
                        }),
                        20 => self.emit(TerminalAction::SetMode {
                            mode: Mode::LineFeedNewLine,
                            enabled,
                        }),
                        _ => self.unknown_csi(intermediates, final_byte),
                    }
                }
            }
            b'n' => {
                let kind = match resolved_count(params, 0).0 {
                    5 => StatusKind::OperatingStatus,
                    6 => StatusKind::CursorPosition,
                    _ => {
                        self.unknown_csi(intermediates, final_byte);
                        return;
                    }
                };
                self.emit(TerminalAction::RequestDeviceStatus { kind });
            }
            b'c' => self.emit(TerminalAction::RequestDeviceStatus {
                kind: StatusKind::DeviceAttributes,
            }),
            b'I' => self.emit(TerminalAction::TabForward {
                n: resolved_count(params, 0),
            }),
            b'Z' => self.emit(TerminalAction::TabBackward {
                n: resolved_count(params, 0),
            }),
            b'g' => {
                match mode_value(params, 0) {
                    0 => self.emit(TerminalAction::TabClear {
                        targets: TabTargets::Current,
                    }),
                    3 => self.emit(TerminalAction::TabClearAll),
                    _ => self.unknown_csi(intermediates, final_byte),
                };
            }
            _ => self.unknown_csi(intermediates, final_byte),
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates, byte) {
            ([], b'7') => self.emit(TerminalAction::CursorSave),
            ([], b'8') => self.emit(TerminalAction::CursorRestore),
            ([], b'c') => self.emit(TerminalAction::FullReset),
            ([], b'H') => self.emit(TerminalAction::TabSet),
            ([], b'=') => self.emit(TerminalAction::SetMode {
                mode: Mode::ApplicationKeypad,
                enabled: true,
            }),
            ([], b'>') => self.emit(TerminalAction::SetMode {
                mode: Mode::ApplicationKeypad,
                enabled: false,
            }),
            ([], b'N') => self.emit(TerminalAction::InvokeCharset {
                slot: CharsetSlot::G2,
            }),
            ([], b'O') => self.emit(TerminalAction::InvokeCharset {
                slot: CharsetSlot::G3,
            }),
            ([], b'n') => self.emit(TerminalAction::InvokeCharset {
                slot: CharsetSlot::G2,
            }),
            ([], b'o') => self.emit(TerminalAction::InvokeCharset {
                slot: CharsetSlot::G3,
            }),
            ([], b'\\') => {}
            ([designator], table @ (b'B' | b'A' | b'0')) => {
                let slot = match designator {
                    b'(' => CharsetSlot::G0,
                    b')' => CharsetSlot::G1,
                    b'*' => CharsetSlot::G2,
                    b'+' => CharsetSlot::G3,
                    _ => {
                        self.unknown_esc(intermediates, byte);
                        return;
                    }
                };
                let charset_table = match table {
                    b'B' => CharsetTable::Ascii,
                    b'A' => CharsetTable::UnitedKingdom,
                    _ => CharsetTable::DecSpecialGraphics,
                };
                self.emit(TerminalAction::SelectCharset {
                    slot,
                    table: charset_table,
                });
            }
            _ => self.unknown_esc(intermediates, byte),
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let id = osc_id(params);
        let rest = params.get(1..).unwrap_or(&[]);
        match id {
            0 | 2 => {
                let joined = join_segments(rest);
                let text = String::from_utf8_lossy(&joined).into_owned();
                self.emit(TerminalAction::OscTitle {
                    text: BoundedString::new(text),
                });
            }
            7 => {
                let joined = join_segments(rest);
                let url = String::from_utf8_lossy(&joined).into_owned();
                self.emit(TerminalAction::OscCwd {
                    url: BoundedString::new(url),
                });
            }
            8 => {
                let uri = rest
                    .get(1)
                    .map(|segment| String::from_utf8_lossy(segment).into_owned())
                    .unwrap_or_default();
                if uri.is_empty() {
                    self.emit(TerminalAction::OscHyperlink { link: None });
                } else {
                    let identity = rest.first().copied().unwrap_or(&[]);
                    let id = identity
                        .split(|&byte| byte == b':')
                        .find_map(|pair| pair.strip_prefix(b"id=".as_slice()))
                        .map(String::from_utf8_lossy)
                        .map(BoundedString::new);
                    self.emit(TerminalAction::OscHyperlink {
                        link: Some(Hyperlink {
                            id,
                            uri: BoundedString::new(uri),
                        }),
                    });
                }
            }
            52 => {
                let query = rest.iter().any(|segment| *segment == b"?");
                let data = rest.last().copied().unwrap_or(&[]);
                self.emit(TerminalAction::OscClipboard {
                    op: if query {
                        ClipboardOp::Read
                    } else {
                        ClipboardOp::Write
                    },
                    data: BoundedBytes::new(data.to_vec()),
                });
            }
            133 => {
                let kind = match rest.first().and_then(|segment| segment.first()) {
                    Some(b'A') => ZoneKind::PromptStart,
                    Some(b'B') => ZoneKind::InputStart,
                    Some(b'C') => ZoneKind::OutputStart,
                    Some(b'D') => ZoneKind::OutputEnd,
                    _ => {
                        let data = join_segments(rest);
                        self.emit(TerminalAction::OscUnknown {
                            id,
                            data: BoundedBytes::new(data),
                        });
                        return;
                    }
                };
                self.emit(TerminalAction::OscPromptMark { kind });
            }
            _ => {
                let data = join_segments(rest);
                self.emit(TerminalAction::OscUnknown {
                    id,
                    data: BoundedBytes::new(data),
                });
            }
        }
    }

    fn hook(&mut self, _params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let capture = &mut self.dcs;
        capture.active = true;
        capture.final_byte = u8::try_from(u32::from(action)).unwrap_or(0);
        capture.intermediates = pack_intermediates(intermediates);
    }

    fn unhook(&mut self) {
        if !self.dcs.active {
            return;
        }
        let reported = UnrecognizedSequence {
            kind: SequenceKind::Dcs,
            final_byte: self.dcs.final_byte,
            intermediates: self.dcs.intermediates,
        };
        self.dcs.active = false;
        self.emit(TerminalAction::Unknown(reported));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{
        Attribute, AttributeChange, AttributeDiff, CharsetSlot, CharsetTable, ClipboardOp, Col,
        Color, ControlChar, Count, CursorStyle, Direction, EraseDisplayMode, EraseLineMode,
        GraphemeCell, Mode, MouseTrackingMode, Row, SequenceKind, StatusKind, TabTargets,
        UnderlineStyle, UnrecognizedSequence,
    };

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
                vec![TerminalAction::OscPromptMark { kind }]
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
            0xFF, 0xFE, 0x80, 0x81, 0xC0, 0x80, 0xED, 0xA0, 0x80, 0xF0, 0x80, 0x80, 0x84, 0xE2,
            0x28, 0xA1, 0xC3, 0x28,
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
}
