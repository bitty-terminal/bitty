//! Canonical state-hash serialization for [`State`].

use bitty_vt::{CursorStyle, ZoneKind};

use crate::canonical::{CANONICAL_HASH_VERSION, CanonicalHasher, write_cell, write_style};
use crate::charsets::Charsets;
use crate::cursor::SavedCursor;
use crate::modes::{AltScreen, Modes};

use super::{ScreenSave, State};

impl State {
    /// Platform-stable state hash (RFC replay guarantee 2).
    ///
    /// FNV-1a over the canonical serialization: fixed field ordering,
    /// little-endian integers, UTF-32 scalars, length-prefixed strings.
    /// Same input conditions produce the identical digest everywhere.
    #[must_use]
    pub fn state_hash(&self) -> u64 {
        let mut h = CanonicalHasher::new();
        h.u32(CANONICAL_HASH_VERSION);
        // Deliberately excluded: `generation` (damage bookkeeping, not
        // truth; RFC replay guarantee 2 enumerates grid, scrollback,
        // cursor, modes, tab stops, charset slots, and pending replies).
        h.u16(self.width as u16);
        h.u16(self.height as u16);

        h.u16(self.cursor.position.row);
        h.u16(self.cursor.position.col);
        h.boolean(self.cursor.pending_wrap);
        h.boolean(self.cursor.visible);
        h.u8(cursor_style_discriminant(self.cursor.cursor_style));
        write_style(&mut h, &self.cursor.style);

        h.boolean(self.modes.insert);
        h.boolean(self.modes.line_feed_new_line);
        h.boolean(self.modes.application_keypad);
        h.boolean(self.modes.application_cursor_keys);
        h.boolean(self.modes.column_132_requested);
        h.boolean(self.modes.reverse_video);
        h.boolean(self.modes.origin);
        h.boolean(self.modes.auto_wrap);
        h.boolean(self.modes.cursor_blinking);
        h.boolean(self.modes.bracketed_paste);
        h.boolean(self.modes.focus_events);
        h.u32(self.modes.kitty_keyboard);
        h.option_tag(self.modes.mouse_tracking.is_some());
        if let Some(mode) = self.modes.mouse_tracking {
            h.u8(mouse_tracking_discriminant(mode));
        }
        h.option_tag(self.modes.mouse_coordinate_encoding.is_some());
        if let Some(encoding) = self.modes.mouse_coordinate_encoding {
            h.u8(mouse_encoding_discriminant(encoding));
        }

        h.u8(alt_screen_discriminant(self.alt_screen));
        h.option_tag(self.primary_save.is_some());
        if let Some(save) = &self.primary_save {
            out_save_cursor_fields(&mut h, save);
            write_modes(&mut h, &save.modes);
        }
        for saved in &self.saved_cursors {
            h.option_tag(saved.is_some());
            if let Some(saved) = saved {
                write_saved_cursor(&mut h, saved);
            }
        }

        h.u16(self.scroll_region_top);
        h.u16(self.scroll_region_bottom);

        h.u16(self.tabs.len() as u16);
        for col in 0..self.tabs.len() {
            h.boolean(self.tabs.contains(col));
        }

        write_charsets(&mut h, &self.charsets);

        h.str(self.title.as_str());
        h.option_tag(self.cwd_report.is_some());
        if let Some(cwd) = &self.cwd_report {
            h.str(cwd.as_str());
        }

        h.u32(self.hyperlink_table.len() as u32);
        for (id, uri) in &self.hyperlink_table {
            h.option_tag(id.is_some());
            if let Some(id) = id {
                h.str(id.as_str());
            }
            h.str(uri.as_str());
        }
        h.option_tag(self.current_hyperlink.is_some());
        if let Some(link) = self.current_hyperlink {
            h.u32(link.as_u32());
        }

        h.u64(self.zone_counter);
        h.u32(self.zones.len() as u32);
        for record in &self.zones {
            h.u64(record.ordinal);
            h.u8(zone_discriminant(record.kind));
            h.option_tag(record.exit_code.is_some());
            if let Some(code) = record.exit_code {
                h.u32(code as u32);
            }
        }

        h.u64(self.scrollback.next_line_id());
        h.u64(self.scrollback.total_written());
        h.u32(self.scrollback.len() as u32);
        for line in self.scrollback.iter() {
            h.u64(line.id);
            h.boolean(line.wrapped);
            for cell in &line.cells {
                write_cell(&mut h, cell);
            }
        }

        h.u32(self.replies.total_bytes() as u32);
        h.boolean(self.replies.overflowed());
        let pending = self.peek_replies();
        h.u32(pending.len() as u32);
        for reply in pending {
            h.u32(reply.len() as u32);
            h.u8_slice(reply);
        }

        for cell in self.screens.main.all_cells() {
            write_cell(&mut h, cell);
        }
        for w in self.screens.main.wraps_slice() {
            h.boolean(*w);
        }
        for cell in self.screens.alt.all_cells() {
            write_cell(&mut h, cell);
        }
        for w in self.screens.alt.wraps_slice() {
            h.boolean(*w);
        }

        h.finish()
    }
}

// ----------------------------------------------------------------------
// Discriminant helpers (fixed values are part of the hash contract)
// ----------------------------------------------------------------------

fn cursor_style_discriminant(style: CursorStyle) -> u8 {
    match style {
        CursorStyle::Default => 0,
        CursorStyle::BlinkingBlock => 1,
        CursorStyle::SteadyBlock => 2,
        CursorStyle::BlinkingUnderline => 3,
        CursorStyle::SteadyUnderline => 4,
        CursorStyle::BlinkingBar => 5,
        CursorStyle::SteadyBar => 6,
    }
}

fn mouse_tracking_discriminant(mode: bitty_vt::MouseTrackingMode) -> u8 {
    match mode {
        bitty_vt::MouseTrackingMode::X10 => 1,
        bitty_vt::MouseTrackingMode::Normal => 2,
        bitty_vt::MouseTrackingMode::Button => 3,
        bitty_vt::MouseTrackingMode::Any => 4,
    }
}

fn mouse_encoding_discriminant(encoding: bitty_vt::MouseCoordinateEncoding) -> u8 {
    match encoding {
        bitty_vt::MouseCoordinateEncoding::Utf8 => 1,
        bitty_vt::MouseCoordinateEncoding::Sgr => 2,
        bitty_vt::MouseCoordinateEncoding::Urxvt => 3,
    }
}

fn alt_screen_discriminant(screen: AltScreen) -> u8 {
    match screen {
        AltScreen::Off => 0,
        AltScreen::Via47 => 1,
        AltScreen::Via1049 => 2,
    }
}

fn charset_discriminant(slot: bitty_vt::CharsetSlot) -> u8 {
    match slot {
        bitty_vt::CharsetSlot::G0 => 0,
        bitty_vt::CharsetSlot::G1 => 1,
        bitty_vt::CharsetSlot::G2 => 2,
        bitty_vt::CharsetSlot::G3 => 3,
    }
}

fn table_discriminant(table: bitty_vt::CharsetTable) -> u8 {
    match table {
        bitty_vt::CharsetTable::Ascii => 0,
        bitty_vt::CharsetTable::UnitedKingdom => 1,
        bitty_vt::CharsetTable::DecSpecialGraphics => 2,
    }
}

fn zone_discriminant(kind: ZoneKind) -> u8 {
    match kind {
        ZoneKind::PromptStart => 0,
        ZoneKind::InputStart => 1,
        ZoneKind::OutputStart => 2,
        ZoneKind::OutputEnd => 3,
    }
}

fn write_saved_cursor(out: &mut CanonicalHasher, saved: &SavedCursor) {
    out.u16(saved.position.row);
    out.u16(saved.position.col);
    out.boolean(saved.pending_wrap);
    write_style(out, &saved.style);
    out.boolean(saved.origin_mode);
    out.boolean(saved.auto_wrap);
    write_charsets(out, &saved.charsets);
}

fn out_save_cursor_fields(out: &mut CanonicalHasher, save: &ScreenSave) {
    out.u16(save.cursor_position.row);
    out.u16(save.cursor_position.col);
    out.boolean(save.pending_wrap);
    write_style(out, &save.style);
    out.u8(cursor_style_discriminant(save.cursor_style));
    out.boolean(save.cursor_visible);
    out.boolean(save.origin_mode);
    out.boolean(save.auto_wrap);
    write_charsets(out, &save.charsets);
}

fn write_charsets(out: &mut CanonicalHasher, charsets: &Charsets) {
    out.u8(charset_discriminant(charsets.locking));
    for table in &charsets.slots {
        out.u8(table_discriminant(*table));
    }
    out.option_tag(charsets.single.is_some());
    if let Some(slot) = charsets.single {
        out.u8(charset_discriminant(slot));
    }
}

fn write_modes(out: &mut CanonicalHasher, modes: &Modes) {
    out.boolean(modes.insert);
    out.boolean(modes.line_feed_new_line);
    out.boolean(modes.application_keypad);
    out.boolean(modes.application_cursor_keys);
    out.boolean(modes.column_132_requested);
    out.boolean(modes.reverse_video);
    out.boolean(modes.origin);
    out.boolean(modes.auto_wrap);
    out.boolean(modes.cursor_blinking);
    out.boolean(modes.bracketed_paste);
    out.boolean(modes.focus_events);
}
