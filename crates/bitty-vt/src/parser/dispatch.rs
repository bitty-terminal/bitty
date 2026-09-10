//! `vte::Perform` bridge: maps `vte` callbacks onto [`TerminalAction`] values.
//!
//! Split out of `parser.rs` (CTX-0309); behavior unchanged. The `Perform`
//! implementation and the mode/OSC helpers below are inherent to the bridge;
//! SGR decoding lives in [`super::sgr`].

use super::sgr::parse_sgr;
use super::{Bridge, sub_params};
use crate::action::{
    CharsetSlot, CharsetTable, ClipboardOp, Col, ControlChar, Count, CursorStyle, Direction,
    EraseDisplayMode, EraseLineMode, GraphemeCell, Hyperlink, Mode, MouseCoordinateEncoding,
    MouseTrackingMode, Row, SequenceKind, StatusKind, TabTargets, TerminalAction,
    UnrecognizedSequence, ZoneKind,
};
use crate::bounded::{BoundedBytes, BoundedString};
use vte::{Params, Perform};

impl<F: FnMut(TerminalAction)> Bridge<'_, F> {
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
            ([], b'Z') => self.emit(TerminalAction::RequestDeviceStatus {
                kind: StatusKind::DeviceAttributes,
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
                // For D (OutputEnd) parse optional exit code: second segment after `D` may be `;<code>`.
                let exit_code = if kind == ZoneKind::OutputEnd {
                    rest.get(1)
                        .and_then(|bytes| std::str::from_utf8(bytes).ok())
                        .and_then(|s| s.trim().parse::<i32>().ok())
                } else {
                    None
                };
                self.emit(TerminalAction::OscPromptMark { kind, exit_code });
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
