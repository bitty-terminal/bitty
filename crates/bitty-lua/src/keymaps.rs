//! Bounded read-only keymap/action registry page for `bitty.keymaps.list`.
//!
//! Issue #2405, accepted design
//! `bitty-terminal-docs` `specifications/help-overlay-ownership-design.md`
//! (bitty#2397). The surface answers one question for a presentation plugin:
//! *which chords are bound right now, and to what?* It carries registry data
//! only — never a configuration path, argv, environment value, URL, terminal
//! content, or host diagnostic.
//!
//! | Bound                    | Value | Enforcement |
//! | ------------------------ | ----- | ----------- |
//! | rows per call            | 256   | [`KeymapPage::assemble`] clamps `limit` |
//! | addressable rows         | 1024  | rows past the cap are not addressable; `truncated` reports them |
//! | serialized page          | 64 KiB | whole-row tail drop, `truncated = true` |
//! | `offset`                 | `0..=1024` | [`KeymapQuery::new`] |
//! | `limit`                  | `1..=256` | [`KeymapQuery::new`] |
//! | `chord`                  | 1..=64 bytes | [`KeymapRow::new`] |
//! | `action`                 | 1..=320 bytes | [`KeymapRow::new`] |
//! | `title`                  | 0..=128 characters | [`KeymapRow::new`] |
//! | `command`                | 1..=257 bytes | [`KeymapRow::new`] |
//! | calls per generation     | 16 / s | host call budget |
//!
//! Rows are emitted in resolved-table order. `total` counts the addressable
//! rows (capped at [`KEYMAP_LIST_MAX_TOTAL`]), not the returned page. An
//! `offset` at or past `total` yields an empty `entries` array and
//! `truncated = false`. Truncation always drops whole rows from the tail, so
//! the prefix of a page is stable across the byte cap.
//!
//! Validation is defensive: the host derives every row from a typed
//! [`KeymapRow`] built from the host's validated chord/action types, so a
//! malformed row is dropped instead of echoed. `chord`, `action`, and
//! `command` are canonical single-line spellings; a control character, an
//! embedded newline, or an over-bound value drops the whole row (truncating
//! one would advertise a different binding). `title` is display-only: it is
//! stripped to one bounded line and never drops the binding.

use crate::host::{BridgeError, LuaValue};

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Maximum rows returned by one `bitty.keymaps.list()` call.
pub const KEYMAP_LIST_MAX_ROWS: usize = 256;

/// Maximum addressable rows per full scan.
///
/// This is a read cap, not the registry's theoretical maximum: rows past the
/// cap are not addressable through this API and are reported by
/// `truncated = true`.
pub const KEYMAP_LIST_MAX_TOTAL: usize = 1024;

/// Maximum accepted `offset` value.
pub const KEYMAP_LIST_MAX_OFFSET: usize = KEYMAP_LIST_MAX_TOTAL;

/// Maximum canonical encoded page size (bytes).
pub const KEYMAP_LIST_MAX_BYTES: usize = 64 * 1024;

/// Maximum bytes of one canonical chord.
pub const KEYMAP_LIST_CHORD_MAX_BYTES: usize = 64;

/// Maximum bytes of one canonical action.
pub const KEYMAP_LIST_ACTION_MAX_BYTES: usize = 320;

/// Maximum characters of one static action title.
pub const KEYMAP_LIST_TITLE_MAX_CHARS: usize = 128;

/// Maximum bytes of one qualified command target.
pub const KEYMAP_LIST_COMMAND_MAX_BYTES: usize = 257;

/// Maximum accepted calls per second per plugin generation.
pub const KEYMAP_LIST_CALLS_PER_SECOND: u32 = 16;

/// `source` value for a binding that came from the shipped defaults.
pub const KEYMAP_SOURCE_DEFAULT: &str = "default";

/// `source` value for a binding that came from a user override.
pub const KEYMAP_SOURCE_USER: &str = "user";

/// The only accepted `context` value today.
pub const KEYMAP_CONTEXT_GLOBAL: &str = "global";

// ---------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------

/// One validated `bitty.keymaps.list()` query.
///
/// `offset` and `limit` are the only accepted arguments. The bridge parses
/// them from the optional options table and rejects every other shape before
/// the host is reached, so the call can never carry a path, command, URL,
/// argv entry, environment key, or pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeymapQuery {
    /// Zero-based start of the page in resolved-table order.
    pub offset: usize,
    /// Requested rows for this page (`1..=`[`KEYMAP_LIST_MAX_ROWS`]).
    pub limit: usize,
}

impl Default for KeymapQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: KEYMAP_LIST_MAX_ROWS,
        }
    }
}

impl KeymapQuery {
    /// Validate one query.
    ///
    /// # Errors
    ///
    /// `E_DEF_LIMIT` for an out-of-range `offset` (`0..=1024`) or `limit`
    /// (`1..=256`); the caller rejects wrong types with `E_VALUE_TYPE`
    /// before reaching this constructor.
    pub fn new(offset: usize, limit: usize) -> Result<Self, BridgeError> {
        if offset > KEYMAP_LIST_MAX_OFFSET {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_LIMIT",
                format!("keymaps.list offset must be <= {KEYMAP_LIST_MAX_OFFSET} (got {offset})"),
            ));
        }
        if limit == 0 || limit > KEYMAP_LIST_MAX_ROWS {
            return Err(BridgeError::new(
                "validation",
                "E_DEF_LIMIT",
                format!("keymaps.list limit must be 1..={KEYMAP_LIST_MAX_ROWS} (got {limit})"),
            ));
        }
        Ok(Self { offset, limit })
    }
}

// ---------------------------------------------------------------------------
// Row
// ---------------------------------------------------------------------------

/// One binding row returned by `bitty.keymaps.list()`.
///
/// `title` is the static Core description for a Core action and is absent for
/// a `command:` row; `command` carries only the validated qualified target
/// (`owner:command`, never arguments or argv) and is absent for a Core
/// action. `source` is `"default"` or `"user"`; `context` is `"global"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapRow {
    /// Canonical chord spelling (`alt+h`, `ctrl+shift+v`, ...).
    pub chord: String,
    /// Canonical action spelling (`goto_split:left`, `command:owner:cmd`).
    pub action: String,
    /// `"default"` or `"user"`.
    pub source: String,
    /// Binding context (`"global"` today).
    pub context: String,
    /// Static Core action description; absent for a `command:` row.
    pub title: Option<String>,
    /// Qualified command target; present only for a `command:` row.
    pub command: Option<String>,
}

impl KeymapRow {
    /// Validate and build one row.
    ///
    /// Returns `None` — the row is dropped, never echoed — when the canonical
    /// `chord`, `action`, or `command` is empty, over its byte bound, or
    /// carries a control character or whitespace, when `source`/`context` is
    /// outside the closed set, or when the `title`/`command` shape does not
    /// match the action. `title` is display-only and optional: an over-long or
    /// multi-line value is stripped and truncated to one bounded line instead
    /// of dropping the binding. The checks are defensive: the host builds rows
    /// from its validated chord, action, and context types.
    #[must_use]
    pub fn new(
        chord: impl Into<String>,
        action: impl Into<String>,
        source: impl Into<String>,
        context: impl Into<String>,
        title: Option<String>,
        command: Option<String>,
    ) -> Option<Self> {
        let chord = chord.into();
        let action = action.into();
        let source = source.into();
        let context = context.into();
        if !valid_token(&chord, KEYMAP_LIST_CHORD_MAX_BYTES) {
            return None;
        }
        if !valid_token(&action, KEYMAP_LIST_ACTION_MAX_BYTES) {
            return None;
        }
        if source != KEYMAP_SOURCE_DEFAULT && source != KEYMAP_SOURCE_USER {
            return None;
        }
        if context != KEYMAP_CONTEXT_GLOBAL {
            return None;
        }
        // The title is display-only and optional, so an over-long or
        // multi-line description is truncated to one bounded line instead of
        // dropping a real binding. Chord, action, and command stay strict:
        // truncating any of those would advertise a different binding.
        let title = title.as_deref().and_then(sanitize_title);
        match (action.strip_prefix("command:"), command.as_deref()) {
            (Some(target), Some(command)) if target == command => {
                if !valid_token(command, KEYMAP_LIST_COMMAND_MAX_BYTES) {
                    return None;
                }
            }
            (None, None) => {}
            // `command:` without a target, a target without `command:`, a
            // mismatched target, or a command field on a Core action all fail
            // closed: the row is dropped.
            _ => return None,
        }
        Some(Self {
            chord,
            action,
            source,
            context,
            title,
            command,
        })
    }

    /// Marshal one row as a Lua table.
    ///
    /// `title` and `command` are present only when set, so an absent optional
    /// field never appears as `nil` in the table.
    #[must_use]
    pub fn to_lua(&self) -> LuaValue {
        let mut pairs = vec![
            ("chord".to_string(), LuaValue::String(self.chord.clone())),
            ("action".to_string(), LuaValue::String(self.action.clone())),
            ("source".to_string(), LuaValue::String(self.source.clone())),
            (
                "context".to_string(),
                LuaValue::String(self.context.clone()),
            ),
        ];
        if let Some(title) = &self.title {
            pairs.push(("title".to_string(), LuaValue::String(title.clone())));
        }
        if let Some(command) = &self.command {
            pairs.push(("command".to_string(), LuaValue::String(command.clone())));
        }
        LuaValue::Table(
            pairs
                .into_iter()
                .map(|(key, value)| (LuaValue::String(key), value))
                .collect(),
        )
    }

    fn write_json(&self, out: &mut String) {
        out.push_str("{\"chord\":");
        write_json_string(out, &self.chord);
        out.push_str(",\"action\":");
        write_json_string(out, &self.action);
        out.push_str(",\"source\":");
        write_json_string(out, &self.source);
        out.push_str(",\"context\":");
        write_json_string(out, &self.context);
        if let Some(title) = &self.title {
            out.push_str(",\"title\":");
            write_json_string(out, title);
        }
        if let Some(command) = &self.command {
            out.push_str(",\"command\":");
            write_json_string(out, command);
        }
        out.push('}');
    }
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

/// One bounded page of the live keymap registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapPage {
    /// Rows for this page, in resolved-table order.
    pub entries: Vec<KeymapRow>,
    /// Number of addressable rows (capped at [`KEYMAP_LIST_MAX_TOTAL`]).
    pub total: usize,
    /// Requested (validated) offset, echoed for paging.
    pub offset: usize,
    /// Whether an unreturned tail exists (page cap, addressable cap, or byte
    /// cap).
    pub truncated: bool,
}

impl KeymapPage {
    /// Assemble a bounded page from the complete, already-validated row set.
    ///
    /// `rows` is the live registry snapshot cloned by the host at call time;
    /// this function performs no host access. Order is preserved. Rows past
    /// [`KEYMAP_LIST_MAX_TOTAL`] are not addressable. The page is trimmed from
    /// the tail until the canonical encoding fits [`KEYMAP_LIST_MAX_BYTES`];
    /// `truncated` is `true` whenever a row is left out.
    #[must_use]
    pub fn assemble(rows: Vec<KeymapRow>, query: KeymapQuery) -> Self {
        let raw_total = rows.len();
        let total = raw_total.min(KEYMAP_LIST_MAX_TOTAL);
        let start = query.offset.min(total);
        let limit = query.limit.min(KEYMAP_LIST_MAX_ROWS);
        let end = start.saturating_add(limit).min(total);
        let mut entries: Vec<KeymapRow> = rows.into_iter().skip(start).take(end - start).collect();
        // The documented rule: an `offset` at or past `total` yields an empty
        // page with `truncated = false`; otherwise any unreturned row (page
        // cap, addressable cap, or byte cap) flags the tail.
        let mut truncated = start < total && end < raw_total;
        if !fits(&entries, total, query.offset, truncated) {
            truncated = true;
            // Largest prefix whose canonical encoding fits the byte cap. The
            // encoding grows monotonically with the row count, so a binary
            // search matches sequential tail dropping without re-encoding the
            // page once per dropped row.
            let mut low = 0usize;
            let mut high = entries.len();
            while low < high {
                let mid = (low + high).div_ceil(2);
                if fits(&entries[..mid], total, query.offset, true) {
                    low = mid;
                } else {
                    high = mid - 1;
                }
            }
            entries.truncate(low);
        }
        Self {
            entries,
            total,
            offset: query.offset,
            truncated,
        }
    }

    /// Canonical encoded page length in bytes.
    ///
    /// Field order and escaping are deterministic, so the value is stable
    /// across calls; this is the bound [`KEYMAP_LIST_MAX_BYTES`] measures.
    #[must_use]
    pub fn canonical_len(&self) -> usize {
        let mut out = String::new();
        self.write_json(&mut out);
        out.len()
    }

    /// Whether the page is inside the serialized response bound.
    #[must_use]
    pub fn fits_budget(&self) -> bool {
        self.canonical_len() <= KEYMAP_LIST_MAX_BYTES
    }

    /// Marshal the page as the documented Lua result table.
    #[must_use]
    pub fn to_lua(&self) -> LuaValue {
        LuaValue::table([
            (
                "entries",
                LuaValue::array(self.entries.iter().map(KeymapRow::to_lua).collect()),
            ),
            ("total", LuaValue::Integer(saturating_i64(self.total))),
            ("offset", LuaValue::Integer(saturating_i64(self.offset))),
            ("truncated", LuaValue::Bool(self.truncated)),
        ])
    }

    fn write_json(&self, out: &mut String) {
        out.push_str("{\"entries\":[");
        for (index, row) in self.entries.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            row.write_json(out);
        }
        out.push_str("],\"total\":");
        out.push_str(&self.total.to_string());
        out.push_str(",\"offset\":");
        out.push_str(&self.offset.to_string());
        out.push_str(",\"truncated\":");
        out.push_str(if self.truncated { "true" } else { "false" });
        out.push('}');
    }
}

fn fits(entries: &[KeymapRow], total: usize, offset: usize, truncated: bool) -> bool {
    let page = KeymapPage {
        entries: entries.to_vec(),
        total,
        offset,
        truncated,
    };
    page.fits_budget()
}

fn saturating_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Canonical single-line token: non-empty, inside `max` bytes, ASCII
/// printable, no whitespace and no control character.
fn valid_token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !byte.is_ascii_whitespace())
}

/// Canonical single-line title: control characters become spaces, the result
/// is trimmed, and it is truncated to [`KEYMAP_LIST_TITLE_MAX_CHARS`]
/// characters. An all-blank title becomes `None` (the optional field stays
/// absent) rather than an empty string.
fn sanitize_title(value: &str) -> Option<String> {
    let cleaned: String = value
        .chars()
        .map(|ch| {
            if ch.is_control() || ch == '\u{2028}' || ch == '\u{2029}' {
                ' '
            } else {
                ch
            }
        })
        .take(KEYMAP_LIST_TITLE_MAX_CHARS)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Deterministic JSON string escaping for the canonical length measurement.
fn write_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if u32::from(ch) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", u32::from(ch)));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(chord: &str, action: &str, source: &str, title: Option<&str>) -> KeymapRow {
        KeymapRow::new(
            chord,
            action,
            source,
            KEYMAP_CONTEXT_GLOBAL,
            title.map(str::to_string),
            None,
        )
        .expect("fixture row is valid")
    }

    fn command_row(chord: &str, target: &str) -> KeymapRow {
        KeymapRow::new(
            chord,
            format!("command:{target}"),
            KEYMAP_SOURCE_USER,
            KEYMAP_CONTEXT_GLOBAL,
            None,
            Some(target.to_string()),
        )
        .expect("fixture command row is valid")
    }

    fn rows(count: usize) -> Vec<KeymapRow> {
        (0..count)
            .map(|index| {
                row(
                    &format!("alt+{index}"),
                    "close_view",
                    "default",
                    Some("Close."),
                )
            })
            .collect()
    }

    #[test]
    fn query_defaults_and_bounds_are_the_documented_range() {
        assert_eq!(
            KeymapQuery::default(),
            KeymapQuery {
                offset: 0,
                limit: KEYMAP_LIST_MAX_ROWS
            }
        );
        assert!(KeymapQuery::new(0, 1).is_ok());
        assert!(KeymapQuery::new(KEYMAP_LIST_MAX_OFFSET, KEYMAP_LIST_MAX_ROWS).is_ok());
        assert_eq!(
            KeymapQuery::new(KEYMAP_LIST_MAX_OFFSET + 1, 1)
                .expect_err("offset over cap")
                .code,
            "E_DEF_LIMIT"
        );
        assert_eq!(
            KeymapQuery::new(0, 0).expect_err("limit zero").code,
            "E_DEF_LIMIT"
        );
        assert_eq!(
            KeymapQuery::new(0, KEYMAP_LIST_MAX_ROWS + 1)
                .expect_err("limit over cap")
                .code,
            "E_DEF_LIMIT"
        );
    }

    #[test]
    fn page_respects_the_row_cap_and_reports_the_tail() {
        let page = KeymapPage::assemble(rows(300), KeymapQuery::default());
        assert_eq!(page.entries.len(), KEYMAP_LIST_MAX_ROWS);
        assert_eq!(page.total, 300);
        assert!(page.truncated);
        assert!(page.fits_budget());
        assert_eq!(page.entries[0].chord, "alt+0");
        assert_eq!(
            page.entries[KEYMAP_LIST_MAX_ROWS - 1].chord,
            format!("alt+{}", KEYMAP_LIST_MAX_ROWS - 1)
        );
    }

    #[test]
    fn page_is_addressable_only_up_to_the_total_cap() {
        let page = KeymapPage::assemble(
            rows(KEYMAP_LIST_MAX_TOTAL + 50),
            KeymapQuery::new(KEYMAP_LIST_MAX_TOTAL - 2, 256).expect("valid query"),
        );
        assert_eq!(page.total, KEYMAP_LIST_MAX_TOTAL);
        assert_eq!(page.entries.len(), 2);
        assert!(page.truncated, "rows past the addressable cap exist");
    }

    #[test]
    fn offset_at_or_past_total_returns_an_empty_untruncated_page() {
        let page = KeymapPage::assemble(rows(10), KeymapQuery::new(10, 256).expect("valid"));
        assert!(page.entries.is_empty());
        assert_eq!(page.total, 10);
        assert_eq!(page.offset, 10);
        assert!(!page.truncated);

        let page = KeymapPage::assemble(rows(10), KeymapQuery::new(1000, 256).expect("valid"));
        assert!(page.entries.is_empty());
        assert_eq!(page.total, 10);
        assert_eq!(page.offset, 1000);
        assert!(!page.truncated);
    }

    #[test]
    fn last_page_is_a_complete_untruncated_tail() {
        let page = KeymapPage::assemble(rows(300), KeymapQuery::new(256, 256).expect("valid"));
        assert_eq!(page.entries.len(), 44);
        assert_eq!(page.total, 300);
        assert!(!page.truncated, "the last page returns the whole tail");
    }

    #[test]
    fn byte_cap_drops_whole_tail_rows_and_flags_truncation() {
        // Wide rows so 256 of them exceed 64 KiB.
        let long_chord = "c".repeat(KEYMAP_LIST_CHORD_MAX_BYTES);
        let long_action = "a".repeat(KEYMAP_LIST_ACTION_MAX_BYTES);
        let long_title = "t".repeat(KEYMAP_LIST_TITLE_MAX_CHARS);
        let wide: Vec<KeymapRow> = (0..KEYMAP_LIST_MAX_ROWS)
            .map(|_| row(&long_chord, &long_action, "default", Some(&long_title)))
            .collect();
        let full = KeymapPage {
            entries: wide.clone(),
            total: wide.len(),
            offset: 0,
            truncated: false,
        };
        assert!(
            full.canonical_len() > KEYMAP_LIST_MAX_BYTES,
            "fixture must exceed the byte cap: {}",
            full.canonical_len()
        );
        let page = KeymapPage::assemble(wide, KeymapQuery::default());
        assert!(!page.entries.is_empty());
        assert!(page.entries.len() < KEYMAP_LIST_MAX_ROWS);
        assert!(page.truncated);
        assert!(page.fits_budget(), "assembled page must fit the cap");
        assert_eq!(page.entries[0].chord, long_chord);
    }

    #[test]
    fn canonical_length_is_deterministic_and_counts_every_field() {
        let page = KeymapPage::assemble(
            vec![row("alt+h", "goto_split:left", "default", Some("Move."))],
            KeymapQuery::default(),
        );
        assert_eq!(page.canonical_len(), page.canonical_len());
        assert_eq!(
            page.canonical_len(),
            r#"{"entries":[{"chord":"alt+h","action":"goto_split:left","source":"default","context":"global","title":"Move."}],"total":1,"offset":0,"truncated":false}"#
                .len()
        );
    }

    #[test]
    fn command_rows_carry_the_qualified_target_and_no_title() {
        let page = KeymapPage::assemble(
            vec![command_row(
                "alt+shift+p",
                "bitty-terminal.help:toggle_help",
            )],
            KeymapQuery::default(),
        );
        let LuaValue::Table(pairs) = page.to_lua() else {
            panic!("page must marshal as a table");
        };
        let entries = pairs
            .iter()
            .find_map(|(key, value)| match (key, value) {
                (LuaValue::String(key), value) if key == "entries" => Some(value),
                _ => None,
            })
            .expect("entries key");
        let LuaValue::Table(entry_rows) = entries else {
            panic!("entries must be an array");
        };
        let LuaValue::Table(fields) = &entry_rows[0].1 else {
            panic!("row must be a table");
        };
        let field = |name: &str| {
            fields.iter().find_map(|(key, value)| match (key, value) {
                (LuaValue::String(key), value) if key == name => Some(value.clone()),
                _ => None,
            })
        };
        assert_eq!(
            field("command"),
            Some(LuaValue::String(
                "bitty-terminal.help:toggle_help".to_string()
            ))
        );
        assert_eq!(field("title"), None);
        assert_eq!(
            field("action"),
            Some(LuaValue::String(
                "command:bitty-terminal.help:toggle_help".to_string()
            ))
        );
    }

    #[test]
    fn title_is_stripped_and_truncated_but_never_drops_the_binding() {
        // Control characters and newlines become spaces; the single line is
        // bounded, so a long static description never hides a real binding.
        let long = format!(
            "line one\nline two {}",
            "t".repeat(KEYMAP_LIST_TITLE_MAX_CHARS + 32)
        );
        let row = KeymapRow::new(
            "alt+h",
            "close_view",
            KEYMAP_SOURCE_DEFAULT,
            KEYMAP_CONTEXT_GLOBAL,
            Some(long),
            None,
        )
        .expect("the row is kept");
        let title = row.title.expect("title present");
        assert_eq!(title.chars().count(), KEYMAP_LIST_TITLE_MAX_CHARS);
        assert!(!title.contains('\n'));

        // Exactly at the bound and already single-line: unchanged.
        let exact = "t".repeat(KEYMAP_LIST_TITLE_MAX_CHARS);
        let row = KeymapRow::new(
            "alt+h",
            "close_view",
            KEYMAP_SOURCE_DEFAULT,
            KEYMAP_CONTEXT_GLOBAL,
            Some(exact.clone()),
            None,
        )
        .expect("the row is kept");
        assert_eq!(row.title.as_deref(), Some(exact.as_str()));

        // An all-blank title stays absent instead of becoming an empty string.
        let row = KeymapRow::new(
            "alt+h",
            "close_view",
            KEYMAP_SOURCE_DEFAULT,
            KEYMAP_CONTEXT_GLOBAL,
            Some(" \n\t ".to_string()),
            None,
        )
        .expect("the row is kept");
        assert!(row.title.is_none());
    }

    #[test]
    fn malformed_rows_are_rejected_instead_of_echoed() {
        let base = |chord: &str, action: &str| {
            KeymapRow::new(
                chord,
                action,
                KEYMAP_SOURCE_DEFAULT,
                KEYMAP_CONTEXT_GLOBAL,
                Some("Close.".to_string()),
                None,
            )
        };
        assert!(base("", "close_view").is_none(), "empty chord");
        assert!(base("alt h", "close_view").is_none(), "embedded space");
        assert!(base("alt+\nh", "close_view").is_none(), "embedded newline");
        assert!(
            base(&"x".repeat(KEYMAP_LIST_CHORD_MAX_BYTES + 1), "close_view").is_none(),
            "chord over bound"
        );
        assert!(
            base("alt+h", &"a".repeat(KEYMAP_LIST_ACTION_MAX_BYTES + 1)).is_none(),
            "action over bound"
        );
        assert!(
            KeymapRow::new(
                "alt+h",
                "close_view",
                "config",
                KEYMAP_CONTEXT_GLOBAL,
                None,
                None,
            )
            .is_none(),
            "unknown source"
        );
        assert!(
            KeymapRow::new(
                "alt+h",
                "close_view",
                KEYMAP_SOURCE_DEFAULT,
                "panel",
                None,
                None
            )
            .is_none(),
            "unknown context"
        );
        assert!(
            KeymapRow::new(
                "alt+h",
                "close_view",
                KEYMAP_SOURCE_DEFAULT,
                KEYMAP_CONTEXT_GLOBAL,
                None,
                Some("owner:cmd".to_string()),
            )
            .is_none(),
            "command on a Core action"
        );
        assert!(
            KeymapRow::new(
                "alt+h",
                "command:owner:cmd",
                KEYMAP_SOURCE_DEFAULT,
                KEYMAP_CONTEXT_GLOBAL,
                None,
                None,
            )
            .is_none(),
            "command action without a target"
        );
        assert!(
            KeymapRow::new(
                "alt+h",
                "command:owner:cmd",
                KEYMAP_SOURCE_DEFAULT,
                KEYMAP_CONTEXT_GLOBAL,
                None,
                Some("other:cmd".to_string()),
            )
            .is_none(),
            "mismatched target"
        );
        assert!(
            KeymapRow::new(
                "alt+h",
                "command:owner:cmd",
                KEYMAP_SOURCE_DEFAULT,
                KEYMAP_CONTEXT_GLOBAL,
                None,
                Some("o".repeat(KEYMAP_LIST_COMMAND_MAX_BYTES + 1)),
            )
            .is_none(),
            "command over bound"
        );
    }
}
