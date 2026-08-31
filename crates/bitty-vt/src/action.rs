//! The typed action interface between the VT parser and terminal state.
//!
//! This module implements the "Typed Action interface" section of the
//! Terminal State RFC (`bitty-docs/docs/specifications/terminal-state-rfc.md`).
//! The RFC's illustrative `Action` enum shape is the accepted contract; names
//! are adapted only where Rust idioms require (the parser-facing enum is
//! named [`TerminalAction`] so the crate can also expose a plain-language
//! `Parser` type without shadowing `vte` concepts).
//!
//! Design rules honored here (RFC):
//!
//! 1. Actions are typed and side-effect free; terminal state is the sole
//!    interpreter.
//! 2. Actions are total over the byte stream: every parsed byte maps to
//!    exactly one action or is consumed as part of a multi-byte sequence.
//! 3. Parameters arrive fully resolved: numerics are parsed and defaulted,
//!    color and attribute sub-parameters are decoded — state never re-parses
//!    strings.
//! 4. Every variant names the invariants it may affect; exhaustive `match`
//!    coverage is compile-checked downstream.
//!
//! Coverage rule: sequences with no family in the accepted RFC enum are
//! reported through the semantically inert [`TerminalAction::Unknown`]
//! variant, carrying enough identification for telemetry and replay. Adding
//! a variant for such a sequence requires an RFC revision first.

use crate::bounded::{BoundedBytes, BoundedString};

/// One printed cell candidate: the leading Unicode scalar of a grapheme
/// cluster as delivered by the UTF-8 decoder.
///
/// The parser emits one `GraphemeCell` per decoded scalar; cluster
/// composition and cell-width resolution (wide chars, combining marks) are
/// Terminal Truth concerns owned by terminal state per the text-domain RFC
/// still pending under OQ-007 open items.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GraphemeCell(char);

impl GraphemeCell {
    /// The leading scalar of the cluster.
    #[must_use]
    pub fn scalar(self) -> char {
        self.0
    }
}

impl From<char> for GraphemeCell {
    fn from(value: char) -> Self {
        Self(value)
    }
}

/// A C0 (or C1, where the underlying state machine reports one) control byte
/// delivered by the parser, e.g. BS, HT, LF, CR, BEL.
///
/// The raw byte is preserved verbatim; interpretation belongs to terminal
/// state. Bytes consumed by the state machine itself (ESC, CAN, SUB and the
/// sequence-termination controls) never surface here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ControlChar(pub u8);

/// Movement count for actions that take a repeatable magnitude.
///
/// The parser resolves missing or zero parameters to [`Count::DEFAULT`] per
/// ECMA-48 ("default value 1"); magnitudes saturate at `u16::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Count(pub u16);

impl Count {
    /// Value applied when a parameter is missing or zero.
    pub const DEFAULT: Self = Self(1);
}

/// 1-based grid row coordinate.
///
/// The parser does not know the screen height, so two resolved values have
/// documented per-action meanings:
///
/// - In [`TerminalAction::CursorPosition`] and
///   [`TerminalAction::SetScrollRegion`], [`Row::SENTINEL`] means "resolve
///   against current geometry" (leave the axis unchanged / use the screen
///   bottom respectively).
/// - Otherwise rows are ordinary 1-based coordinates clamped by state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Row(pub u16);

impl Row {
    /// Default row for cursor addressing (`CUP`/`HVP`).
    pub const DEFAULT: Self = Self(1);

    /// Sentinel meaning "resolved by terminal state against current
    /// geometry"; the exact meaning is documented on each action variant.
    pub const SENTINEL: Self = Self(u16::MAX);
}

/// 1-based grid column coordinate; see [`Row`] for sentinel semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Col(pub u16);

impl Col {
    /// Default column for cursor addressing (`CUP`/`HVP`).
    pub const DEFAULT: Self = Self(1);

    /// Sentinel meaning "resolved by terminal state"; see [`Row::SENTINEL`].
    pub const SENTINEL: Self = Self(u16::MAX);
}

/// Cardinal cursor movement direction (`CUU`/`CUD`/`CUF`/`CUB`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Move up (`CUU`).
    Up,
    /// Move down (`CUD`, `VPR`).
    Down,
    /// Move right (`CUF`, `HPR`).
    Right,
    /// Move left (`CUB`).
    Left,
}

/// Cursor rendering style (`DECSCUSR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorStyle {
    /// Restore the default configured style.
    Default,
    /// Blinking block.
    BlinkingBlock,
    /// Steady block.
    SteadyBlock,
    /// Blinking underline.
    BlinkingUnderline,
    /// Steady underline.
    SteadyUnderline,
    /// Blinking bar.
    BlinkingBar,
    /// Steady bar.
    SteadyBar,
}

/// Extent selector for erase-in-display (`ED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EraseDisplayMode {
    /// From the cursor to the end of the screen (`ED 0`).
    Below,
    /// From the start of the screen to the cursor (`ED 1`).
    Above,
    /// The entire visible screen without scrollback (`ED 2`).
    All,
    /// The scrollback buffer (`ED 3`); visible cells are untouched.
    Scrollback,
}

/// Extent selector for erase-in-line (`EL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EraseLineMode {
    /// From the cursor to the end of the line (`EL 0`).
    Right,
    /// From the start of the line to the cursor (`EL 1`).
    Left,
    /// The entire line (`EL 2`).
    All,
}

/// A single sRGB color component triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

/// A fully resolved color reference.
///
/// Palette resolution (which RGB values indexed colors map to) is owned by
/// render/state configuration, not the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    /// Reset to the default foreground/background.
    Default,
    /// An index into the configured palette (0-255; bright variants are
    /// indices 8-15).
    Indexed(u8),
    /// A direct-color RGB value (`SGR 38;2;r;g;b` and friends).
    Rgb(Rgb),
}

/// Text emphasis style selected by SGR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Attribute {
    /// Bold intensity (`SGR 1`).
    Bold,
    /// Faint/dimmed intensity (`SGR 2`).
    Faint,
    /// Italic (`SGR 3`).
    Italic,
    /// Underline with its style (`SGR 4`, `4:x`).
    Underline(UnderlineStyle),
    /// Blink (`SGR 5`).
    Blink,
    /// Inverse video (`SGR 7`).
    Inverse,
    /// Concealed text (`SGR 8`).
    Invisible,
    /// Strikethrough (`SGR 9`).
    Strikethrough,
}

/// Underline shape (`SGR 4:0` through `4:5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnderlineStyle {
    /// No underline (also produced by `SGR 24`).
    None,
    /// Single straight underline (`SGR 4`).
    Single,
    /// Double straight underline (`SGR 21`).
    Double,
    /// Curly underline (`SGR 4:3`), typically used for spell-check.
    Curly,
    /// Dotted underline (`SGR 4:4`).
    Dotted,
    /// Dashed underline (`SGR 4:5`).
    Dashed,
}

/// One ordered change in an SGR attribute run.
///
/// SGR is inherently a sequence of operations applied in order; the diff
/// preserves that order so terminal state replays exactly what was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttributeChange {
    /// `SGR 0`: reset all attributes to their defaults.
    Reset,
    /// Enable an attribute.
    Enable(Attribute),
    /// Disable an attribute (e.g. `SGR 22`, `24`, `25`, `27`, `28`, `29`).
    Disable(Attribute),
    /// Set/reset the foreground color (`SGR 30-39`, `90-97`, `38`, `39`).
    Foreground(Color),
    /// Set/reset the background color (`SGR 40-49`, `100-107`, `48`, `49`).
    Background(Color),
    /// Set/reset the underline color (`SGR 58`, `59`).
    UnderlineColor(Color),
}

/// Fully resolved SGR payload: the ordered list of changes requested.
///
/// An empty change list never occurs: bare `CSI m` resolves to
/// `[AttributeChange::Reset]` per ECMA-48.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AttributeDiff {
    /// Ordered changes as they appeared in the sequence.
    pub changes: Box<[AttributeChange]>,
}

/// A supported terminal mode for [`TerminalAction::SetMode`].
///
/// The closed set below covers the modes this parser maps; DEC private codes
/// without an entry produce [`TerminalAction::Unknown`] instead, so support
/// grows only by extending this enum (RFC coverage rule). Enabling/disabling
/// side effects that depend on geometry (e.g. `DECCOLM` clearing) are
/// enforced by terminal state, not the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// `IRM` (ANSI `4`): insert characters at the cursor.
    Insert,
    /// `LNM` (ANSI `20`): linefeed implies carriage return.
    LineFeedNewLine,
    /// `DECKPAM`/`DECKPNM` (`ESC =`, `ESC >`): application keypad keys.
    ApplicationKeypad,
    /// `DECCKM` (`?1`): application cursor keys.
    ApplicationCursorKeys,
    /// `DECCOLM` (`?3`): 132-column mode switch.
    Column132,
    /// `DECSCNM` (`?5`): reverse video.
    ReverseVideo,
    /// `DECOM` (`?6`): origin mode for cursor addressing.
    Origin,
    /// `DECAWM` (`?7`): automatic wrapping.
    AutoWrap,
    /// `ATT610` (`?12`): cursor blinking.
    CursorBlinking,
    /// `DECSCUS`-adjacent alt-screen selection (`?47`).
    AlternateScreen,
    /// Alt-screen with saved cursor and clear-on-switch (`?1049`).
    AlternateScreenClearAndRestore,
    /// Bracketed paste (`?2004`).
    BracketedPaste,
    /// Focus reporting (`?1004`).
    FocusEvents,
    /// Kitty keyboard protocol (`?7727` progressive flags, bitmask).
    KittyKeyboard(u32),
    /// Mouse press/release/release-drag/all-motion reporting.
    MouseTracking(MouseTrackingMode),
    /// Extended mouse coordinate encoding.
    MouseCoordinateEncoding(MouseCoordinateEncoding),
}

/// XTerm mouse-tracking protocol level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseTrackingMode {
    /// X10: button press only (`?9`).
    X10,
    /// Normal: press and release (`?1000`).
    Normal,
    /// Button-event tracking incl. drag (`?1002`).
    Button,
    /// Any-event tracking incl. motion without buttons (`?1003`).
    Any,
}

/// Encoding used for extended mouse coordinate reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseCoordinateEncoding {
    /// UTF-8 legacy encoding (`?1005`).
    Utf8,
    /// SGR decimal encoding (`?1006`).
    Sgr,
    /// Urxvt decimal encoding (`?1015`).
    Urxvt,
}

/// Tab-clear target selector (`TBC`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TabTargets {
    /// Clear the stop at the current column (`TBC 0`).
    Current,
}

/// Charset slot (G0-G3) selected or invoked by SCS/locking-shift/single-shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CharsetSlot {
    /// Slot G0 (SCS `ESC (`, invoked by `SI`).
    G0,
    /// Slot G1 (SCS `ESC )`, invoked by `SO`).
    G1,
    /// Slot G2 (SCS `ESC *`, single shift `ESC N`).
    G2,
    /// Slot G3 (SCS `ESC +`, single shift `ESC O`).
    G3,
}

/// Translation table designated into a charset slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CharsetTable {
    /// ASCII (`B`).
    Ascii,
    /// UK national (`A`): `#` becomes pound sign.
    UnitedKingdom,
    /// DEC Special Graphics line drawing (`0`).
    DecSpecialGraphics,
}

/// Device status report kind requested via `DSR`/`DA`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusKind {
    /// `DSR 5`: operating status report.
    OperatingStatus,
    /// `DSR 6`: active cursor position report.
    CursorPosition,
    /// Primary `DA` (`CSI c`); reply generation belongs to terminal state.
    DeviceAttributes,
}

/// Clipboard operation implied by an `OSC 52` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClipboardOp {
    /// Query the clipboard (`data` segment equal to `?`).
    Read,
    /// Store the given data on the clipboard.
    Write,
}

/// A hyperlink identity and target from `OSC 8`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hyperlink {
    /// Opaque identifier used to group hyperlink spans; absent when the
    /// emitting program did not assign one.
    pub id: Option<BoundedString>,
    /// The hyperlink target URI.
    pub uri: BoundedString,
}

/// Semantic prompt/command zone marker carried by `OSC 133`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZoneKind {
    /// `A`: prompt start.
    PromptStart,
    /// `B`: command input start.
    InputStart,
    /// `C`: command output start (post-execution).
    OutputStart,
    /// `D`: command output end.
    OutputEnd,
}

/// Kind of sequence reported by [`TerminalAction::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SequenceKind {
    /// Control Sequence Introducer dispatch (`CSI ... final`) with no mapped
    /// action family.
    Csi,
    /// Escape-sequence dispatch (`ESC intermediates final`) with no mapped
    /// action family.
    Esc,
    /// Device Control String (and the indistinguishable SOS/PM/APC string
    /// states of the underlying state machine) terminated without a mapped
    /// handler.
    Dcs,
}

/// An unmapped sequence, recorded for telemetry and replay.
///
/// Semantically inert by definition (RFC coverage rule): applying this action
/// must leave terminal state unchanged. Payload bytes themselves live in the
/// session recording, which stores raw PTY bytes, not actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UnrecognizedSequence {
    /// Which dispatcher produced the report.
    pub kind: SequenceKind,
    /// Final byte of the sequence (DCS reports the hook final byte; unused
    /// bits are zero).
    pub final_byte: u8,
    /// Intermediate bytes (private markers, designators) up to the state
    /// machine cap of two.
    pub intermediates: [u8; 2],
}

/// The semantic action stream emitted by [`crate::Parser`].
///
/// Shape follows the illustrative enum in the Terminal State RFC "Typed
/// Action interface" section; see module docs for adaptation notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalAction {
    // Text and glyphs
    /// Print one cell-width-resolvable grapheme cluster lead scalar.
    Print(GraphemeCell),
    /// Deliver a control function (BS, HT, LF, CR, BEL, other C0/C1).
    PrintControl(ControlChar),

    // Cursor positioning
    /// Relative cursor movement (`CUU`/`CUD`/`CUF`/`CUB`/`VPR`/`HPR`).
    CursorMove {
        /// Direction of travel.
        dir: Direction,
        /// How far to move; missing/zero parameters resolve to 1.
        n: Count,
    },
    /// Absolute cursor addressing (`CUP`/`HVP`; also `VPA`/`CHA` with the
    /// untouched axis set to the [`Row::SENTINEL`]/[`Col::SENTINEL`]).
    ///
    /// The parser carries resolved 1-based numerics only; origin-mode
    /// remapping is applied by terminal state, which owns the mode.
    CursorPosition {
        /// Target row (sentinel: keep current row).
        row: Row,
        /// Target column (sentinel: keep current column).
        col: Col,
    },
    /// Save cursor attributes and position (`DECSC`, `SCOSC`, `?1048 h`).
    CursorSave,
    /// Restore cursor attributes and position (`DECRC`, `SCORC`, `?1048 l`).
    CursorRestore,
    /// Select the cursor rendering style (`DECSCUSR`).
    CursorStyle {
        /// Requested style.
        style: CursorStyle,
    },
    /// Show or hide the cursor (`DECTCEM`, `?25`).
    CursorVisibility {
        /// Whether the cursor should be visible.
        visible: bool,
    },

    // Erase
    /// Erase in display (`ED`).
    EraseInDisplay {
        /// Affected extent.
        mode: EraseDisplayMode,
    },
    /// Erase in line (`EL`).
    EraseInLine {
        /// Affected extent.
        mode: EraseLineMode,
    },
    /// Erase `n` characters from the cursor onward (`ECH`).
    EraseChars {
        /// Character count; missing/zero resolves to 1.
        n: Count,
    },

    // Insert/delete
    /// Insert `n` blank lines at the cursor (`IL`).
    InsertLines {
        /// Line count; missing/zero resolves to 1.
        n: Count,
    },
    /// Delete `n` lines at the cursor (`DL`).
    DeleteLines {
        /// Line count; missing/zero resolves to 1.
        n: Count,
    },
    /// Insert `n` blank characters at the cursor (`ICH`).
    InsertChars {
        /// Character count; missing/zero resolves to 1.
        n: Count,
    },
    /// Delete `n` characters at the cursor (`DCH`).
    DeleteChars {
        /// Character count; missing/zero resolves to 1.
        n: Count,
    },

    // Scroll
    /// Scroll the region contents up `n` lines (`SU`).
    ScrollUp {
        /// Line count; missing/zero resolves to 1.
        n: Count,
    },
    /// Scroll the region contents down `n` lines (`SD`).
    ScrollDown {
        /// Line count; missing/zero resolves to 1.
        n: Count,
    },
    /// Set the scrolling region (`DECSTBM`).
    ///
    /// Rows are 1-based resolved numerics; a bottom value of
    /// [`Row::SENTINEL`] means "current screen bottom" because the parser has
    /// no geometry. Margin-effect cursor relocation is applied by terminal
    /// state.
    SetScrollRegion {
        /// Top margin row.
        top: Row,
        /// Bottom margin row (sentinel: screen bottom).
        bottom: Row,
    },

    // Attributes and colors
    /// Apply an ordered SGR attribute diff.
    SetAttributes {
        /// Resolved ordered changes.
        attrs: AttributeDiff,
    },

    // Modes
    /// Enable or disable a terminal mode (`SM`/`RM`/`DECSET`/`DECRST`).
    SetMode {
        /// Which mode changed.
        mode: Mode,
        /// New state.
        enabled: bool,
    },

    // Tabulation
    /// Set a tab stop at the cursor (`HTS`, `ESC H`).
    TabSet,
    /// Clear tab stops (`TBC`).
    TabClear {
        /// Which stops to clear.
        targets: TabTargets,
    },
    /// Clear all tab stops (`TBC 3`).
    TabClearAll,
    /// Move forward `n` tab stops (`CHT`).
    TabForward {
        /// Stop count; missing/zero resolves to 1.
        n: Count,
    },
    /// Move backward `n` tab stops (`CBT`).
    TabBackward {
        /// Stop count; missing/zero resolves to 1.
        n: Count,
    },

    // Charsets and encoding
    /// Designate a translation table into a charset slot (`SCS`).
    SelectCharset {
        /// Target slot.
        slot: CharsetSlot,
        /// Table being designated.
        table: CharsetTable,
    },
    /// Invoke a charset slot for the next printed characters (`SO`/`SI`,
    /// locking shifts `LS2`/`LS3`, single shifts `SS2`/`SS3`).
    InvokeCharset {
        /// Slot to invoke.
        slot: CharsetSlot,
    },

    // Device status and replies
    /// Request a status report (`DSR`, primary `DA`).
    ///
    /// Reply synthesis belongs to terminal state; the parser never fabricates
    /// responses.
    RequestDeviceStatus {
        /// What was requested.
        kind: StatusKind,
    },
    /// A bounded response destined for the PTY input side.
    ///
    /// Reserved for higher layers synthesizing replies; the parser itself
    /// emits no `Reply` values today. Kept in the public shape so downstream
    /// matches stay exhaustive when reply synthesis lands behind the same
    /// stream.
    Reply {
        /// Bounded response bytes.
        bytes: Box<[u8]>,
    },

    // OSC handling
    /// Window/icon title update (`OSC 0`/`OSC 2`).
    OscTitle {
        /// Title payload, length-bounded.
        text: BoundedString,
    },
    /// Clipboard read/write request (`OSC 52`); effects flow through the
    /// recorded policy decision, not this action (RFC replay guarantees).
    OscClipboard {
        /// Implied operation.
        op: ClipboardOp,
        /// Base64 payload segment as received, length-bounded.
        data: BoundedBytes,
    },
    /// Working-directory report (`OSC 7`).
    OscCwd {
        /// File URL payload, length-bounded.
        url: BoundedString,
    },
    /// Hyperlink span begin/end (`OSC 8`); `None` ends the current span.
    OscHyperlink {
        /// Link identity and target, if any.
        link: Option<Hyperlink>,
    },
    /// Semantic prompt zone marker (`OSC 133`).
    ///
    /// Optional trailing option segments are currently dropped after the
    /// zone letter; extending the shape requires an RFC revision.
    OscPromptMark {
        /// Which zone boundary was marked.
        kind: ZoneKind,
    },
    /// An OSC code with no mapped semantic family, recorded for replay.
    ///
    /// Semantically inert. `id` is the numeric OSC code, or `u32::MAX` when
    /// the code field did not parse as a number.
    OscUnknown {
        /// Numeric OSC code.
        id: u32,
        /// Remaining payload segments re-joined with `;`, length-bounded.
        data: BoundedBytes,
    },

    // Unknown escape families
    /// A CSI/ESC/DCS sequence with no mapped action family (coverage-rule
    /// catch-all). Semantically inert.
    Unknown(UnrecognizedSequence),

    // Reset and misc
    /// `DECSTR` (`CSI ! p`): soft reset of the defined attribute subset.
    SoftReset,
    /// `RIS` (`ESC c`): full state re-initialization.
    FullReset,
}
