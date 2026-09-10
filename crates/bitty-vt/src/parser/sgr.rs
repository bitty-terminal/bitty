//! SGR (Select Graphic Rendition) and color parsing.
//!
//! Split out of `parser.rs` (CTX-0309); behavior unchanged. SGR parameter
//! sequences are decoded into [`AttributeChange`] values and wrapped in a
//! single [`TerminalAction::SetAttributes`] action.

use super::sub_params;
use crate::action::{
    Attribute, AttributeChange, AttributeDiff, Color, Rgb, TerminalAction, UnderlineStyle,
};
use vte::Params;

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

pub(super) fn parse_sgr(params: &Params) -> TerminalAction {
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
