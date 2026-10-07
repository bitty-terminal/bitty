//! The typed action interface between the VT parser and terminal state.
//!
//! This module implements the "Typed Action interface" section of the
//! Terminal State RFC (`docs/specifications/terminal-state-rfc.md`).
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
use crate::kitty_apc::KittyControlKeys;

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
    /// Scroll the visible screen into the scrollback, then clear it
    /// (`ED 22`, the kitty scroll-and-clear extension adopted by ghostty).
    /// Retained scrollback content is preserved (no `ED 3` semantics).
    ScrollAndClear,
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
    /// Alternate scroll (`?1007`): wheel events in the alternate screen
    /// translate to cursor up/down keys instead of mouse reports or
    /// viewport scrolling (xterm `alternateScroll`).
    AlternateScroll,
    /// Synchronized updates (`?2026`, CTX-0380): the application brackets a
    /// redraw between set and reset; presentation defers committing frames
    /// until reset, bounded by the runtime's deferral timeout.
    SynchronizedUpdate,
    /// Kitty keyboard protocol (`?7727` progressive flags, bitmask).
    KittyKeyboard(u32),
    /// Mouse press/release/release-drag/all-motion reporting.
    MouseTracking(MouseTrackingMode),
    /// Extended mouse coordinate encoding.
    MouseCoordinateEncoding(MouseCoordinateEncoding),
}

/// How a `CSI = flags ; mode u` assignment combines with the live flags.
///
/// Mirrors the Kitty keyboard-protocol `mode` parameter
/// (`sw.kovidgoyal.net/kitty/keyboard-protocol`): `1` assigns (all set bits
/// set, all unset bits reset), `2` sets only the named bits, `3` resets only
/// the named bits. Unknown modes fail closed (no state change).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnhancedKeyboardSetMode {
    /// `mode 1`: replace the whole flag register with `flags`.
    Assign,
    /// `mode 2`: OR `flags` into the register.
    Set,
    /// `mode 3`: clear `flags` from the register.
    Reset,
}

/// One Kitty keyboard progressive-enhancement operation (CTX-0575).
///
/// The authoritative negotiation is `CSI = flags ; mode u` (set),
/// `CSI > flags u` (push), `CSI < n u` (pop) and `CSI ? u` (query); the
/// historical `CSI ? 7727 h/l` alias stays supported separately. The parser
/// only classifies the wire form — the bounded flag register and push/pop
/// stack live in terminal state (RFC invariant 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnhancedKeyboardOp {
    /// `CSI = flags ; mode u`: combine `flags` into the register by `mode`.
    Set {
        /// Enhancement bits named by the sequence (masked to the five
        /// defined bits by terminal state).
        flags: u32,
        /// How the bits combine with the live register.
        mode: EnhancedKeyboardSetMode,
    },
    /// `CSI > flags u`: push the current flags and set `flags` (default 0).
    Push {
        /// Enhancement bits for the new top of the stack.
        flags: u32,
    },
    /// `CSI < n u`: pop `n` entries (default 1); popping past the bottom
    /// resets all flags.
    Pop {
        /// Number of entries to pop (bounded by terminal state).
        n: u16,
    },
    /// `CSI ? u`: report the live flags as `CSI ? flags u`.
    Query,
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
    /// SGR-Pixels decimal pixel coordinates (`?1016`).
    SgrPixels,
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

/// Which default color an `OSC 10`/`OSC 11`/`OSC 12` operation addresses (CTX-0381, CTX-0820).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DynamicColorTarget {
    /// `OSC 10`: the default foreground color.
    Foreground,
    /// `OSC 11`: the default background color.
    Background,
    /// `OSC 12`: the cursor color (CTX-0820).
    Cursor,
}

/// Operation carried by an `OSC 10`/`OSC 11`/`OSC 12` sequence (CTX-0381, CTX-0820).
///
/// The parser only classifies and bounds the payload. Answering queries and
/// gating sets belong to the runtime, which owns the active theme palette
/// and the set capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DynamicColorOp {
    /// `OSC 10;?` / `OSC 11;?` / `OSC 12;?`: report the active color.
    Query,
    /// `OSC 10;<color>` / `OSC 11;<color>` / `OSC 12;<color>`: parsed set value.
    Set(Rgb),
}

/// Terminal-originated notification form (CTX-0577, M1-16; CTX-1008 adds
/// Kitty `OSC 99` for issue #1763).
///
/// The VT parser only classifies and bounds the payload; whether a
/// notification is shown (and how) is a runtime policy decision
/// (default deny, see `specifications/bell-notification-policy.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationSource {
    /// `OSC 9;<message>`: the xterm-style notification form (bare text; the
    /// ConEmu `OSC 9;<n>` sub-commands are not notifications).
    Osc9,
    /// `OSC 777;notify;<title>;<body>`: the rxvt-unicode notification form.
    Osc777,
    /// `OSC 99;metadata;payload`: Kitty desktop notifications (title/body
    /// chunks assembled by the runtime; capability queries and close/icon
    /// payloads never produce a notification).
    Osc99,
}

/// Which Kitty `OSC 99` payload a chunk carries (CTX-1008, issue #1763).
///
/// Only title and body chunks assemble into a notification. All other
/// `p=` values (`close`, `icon`, `?`, `alive`, `buttons`, unknown) are
/// filtered by the parser and never reach this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KittyPayloadType {
    /// `p=title` (or absent `p`, which defaults to title).
    Title,
    /// `p=body`.
    Body,
}

/// One Kitty `OSC 99` title/body chunk (CTX-1008, issue #1763).
///
/// The parser emits one chunk per `OSC 99` sequence; the runtime assembles
/// chunks sharing an `i=` identifier into a single [`Notification`] with
/// source [`NotificationSource::Osc99`]. Payloads are already base64-decoded
/// when `e=1` was present and length-bounded by [`BoundedString`]; they are
/// still untrusted display data and never executed or expanded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KittyNotificationChunk {
    /// Chunk group identifier from `i=` (empty when absent).
    pub id: BoundedString,
    /// Whether this chunk carries title or body text.
    pub payload_type: KittyPayloadType,
    /// Decoded chunk text (possibly empty for an intermediate chunk).
    pub payload: BoundedString,
    /// `d=` flag: false means more chunks follow, true completes the group.
    pub is_done: bool,
}

/// A bounded terminal-originated desktop-notification request (CTX-0577).
///
/// Emitted for the recognized notification OSC forms (`OSC 9`, `OSC 777`,
/// Kitty `OSC 99` assembled by the runtime).
/// Every field is length-bounded by the parser's OSC collector; the runtime
/// treats the strings as untrusted display data and never executes or
/// expands them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Notification {
    /// Which wire form produced this request.
    pub source: NotificationSource,
    /// Optional title; absent for `OSC 9`.
    pub title: Option<BoundedString>,
    /// Message body (always present).
    pub body: BoundedString,
}

/// Operation carried by one `OSC 4` palette pair (CTX-0392, issue #648).
///
/// The parser only classifies and bounds the payload. Answering queries and
/// gating sets belong to the runtime, which owns the active 256-entry
/// palette and the set capability (shared with OSC 10/11 gating).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaletteColorOp {
    /// `OSC 4;<index>;?`: report the active color for `index`.
    Query,
    /// `OSC 4;<index>;<color>`: parsed set value for `index`.
    Set(Rgb),
}

/// One bounded `OSC 4` palette operation: an index plus its query/set op.
///
/// Indices are `0..=255` (fixed 256-entry shape, no growth). Malformed
/// indices, colors, or pair structures never produce this type; the caller
/// records the whole sequence as inert instead (fail-closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaletteOp {
    /// Palette index `0..=255`.
    pub index: u8,
    /// Query or bounded set value.
    pub op: PaletteColorOp,
}

/// Maximum `OSC 4` pairs honored per sequence (CTX-0392).
///
/// Bounds one escape sequence's work: xterm allows many `;<index>;<spec>`
/// pairs per `OSC 4`, but each pair is a query reply or a gated set, so an
/// unbounded pair count would let untrusted PTY bytes force unbounded reply
/// growth. 16 pairs cover real-world uses (single-index sets/queries and
/// small batch recolors) while keeping replies bounded (< 1 KiB); longer
/// sequences fail closed as inert.
pub const MAX_OSC4_OPS: usize = 16;

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

/// Pointer shape name for `OSC 22` (kitty pointer-shapes, CSS cursor keywords).
///
/// Mirrors the `cursor-icon`/`winit` `CursorIcon` name set (kebab-case) so the
/// platform layer maps 1:1 without re-parsing. `from_name` also accepts the
/// legacy X11 cursor-font aliases (`left_ptr`, `hand2`, `watch`, `xterm`, …)
/// for xterm compatibility; unknown names fail open to [`PointerShape::Default`]
/// at the call site (presentation-only, never terminal truth).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerShape {
    /// Platform default (usually an arrow).
    Default,
    /// Context menu available.
    ContextMenu,
    /// Help available.
    Help,
    /// Link pointer (hand).
    Pointer,
    /// Progress (busy but interactive).
    Progress,
    /// Busy, user should wait.
    Wait,
    /// Cell selection.
    Cell,
    /// Crosshair.
    Crosshair,
    /// Text selection (I-beam).
    Text,
    /// Vertical text selection.
    VerticalText,
    /// Alias/shortcut to be created.
    Alias,
    /// Something to be copied.
    Copy,
    /// Something to be moved.
    Move,
    /// Dragged item cannot be dropped here.
    NoDrop,
    /// Requested action will not be carried out.
    NotAllowed,
    /// Something can be grabbed.
    Grab,
    /// Something is being grabbed.
    Grabbing,
    /// East border resize.
    EResize,
    /// North border resize.
    NResize,
    /// North-east corner resize.
    NeResize,
    /// North-west corner resize.
    NwResize,
    /// South border resize.
    SResize,
    /// South-east corner resize.
    SeResize,
    /// South-west corner resize.
    SwResize,
    /// West border resize.
    WResize,
    /// East-west resize.
    EwResize,
    /// North-south resize.
    NsResize,
    /// North-east/south-west diagonal resize.
    NeswResize,
    /// North-west/south-east diagonal resize.
    NwseResize,
    /// Column resize (often `ew-resize` on platforms without a distinct icon).
    ColResize,
    /// Row resize (often `ns-resize` on platforms without a distinct icon).
    RowResize,
    /// Scroll in any direction.
    AllScroll,
    /// Zoom in.
    ZoomIn,
    /// Zoom out.
    ZoomOut,
}

impl PointerShape {
    /// Kebab-case name matching `winit`/`cursor-icon` `CursorIcon::name()`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::ContextMenu => "context-menu",
            Self::Help => "help",
            Self::Pointer => "pointer",
            Self::Progress => "progress",
            Self::Wait => "wait",
            Self::Cell => "cell",
            Self::Crosshair => "crosshair",
            Self::Text => "text",
            Self::VerticalText => "vertical-text",
            Self::Alias => "alias",
            Self::Copy => "copy",
            Self::Move => "move",
            Self::NoDrop => "no-drop",
            Self::NotAllowed => "not-allowed",
            Self::Grab => "grab",
            Self::Grabbing => "grabbing",
            Self::EResize => "e-resize",
            Self::NResize => "n-resize",
            Self::NeResize => "ne-resize",
            Self::NwResize => "nw-resize",
            Self::SResize => "s-resize",
            Self::SeResize => "se-resize",
            Self::SwResize => "sw-resize",
            Self::WResize => "w-resize",
            Self::EwResize => "ew-resize",
            Self::NsResize => "ns-resize",
            Self::NeswResize => "nesw-resize",
            Self::NwseResize => "nwse-resize",
            Self::ColResize => "col-resize",
            Self::RowResize => "row-resize",
            Self::AllScroll => "all-scroll",
            Self::ZoomIn => "zoom-in",
            Self::ZoomOut => "zoom-out",
        }
    }

    /// Parses a CSS kebab-case name or legacy X11 cursor-font alias.
    ///
    /// Returns `None` for unknown names (callers fail open to
    /// [`PointerShape::Default`]) and for empty input. Matching is exact
    /// (case-sensitive, no whitespace trimming here; callers trim first).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "default" => Some(Self::Default),
            "context-menu" => Some(Self::ContextMenu),
            "help" => Some(Self::Help),
            "pointer" => Some(Self::Pointer),
            "progress" => Some(Self::Progress),
            "wait" => Some(Self::Wait),
            "cell" => Some(Self::Cell),
            "crosshair" => Some(Self::Crosshair),
            "text" => Some(Self::Text),
            "vertical-text" => Some(Self::VerticalText),
            "alias" => Some(Self::Alias),
            "copy" => Some(Self::Copy),
            "move" => Some(Self::Move),
            "no-drop" => Some(Self::NoDrop),
            "not-allowed" => Some(Self::NotAllowed),
            "grab" => Some(Self::Grab),
            "grabbing" => Some(Self::Grabbing),
            "e-resize" => Some(Self::EResize),
            "n-resize" => Some(Self::NResize),
            "ne-resize" => Some(Self::NeResize),
            "nw-resize" => Some(Self::NwResize),
            "s-resize" => Some(Self::SResize),
            "se-resize" => Some(Self::SeResize),
            "sw-resize" => Some(Self::SwResize),
            "w-resize" => Some(Self::WResize),
            "ew-resize" => Some(Self::EwResize),
            "ns-resize" => Some(Self::NsResize),
            "nesw-resize" => Some(Self::NeswResize),
            "nwse-resize" => Some(Self::NwseResize),
            "col-resize" => Some(Self::ColResize),
            "row-resize" => Some(Self::RowResize),
            "all-scroll" => Some(Self::AllScroll),
            "zoom-in" => Some(Self::ZoomIn),
            "zoom-out" => Some(Self::ZoomOut),
            // Legacy X11 cursor-font aliases (xterm `pointerShape` compat).
            // Sourced from `cursor-icon` `alt_names` plus the classic
            // `xtermSetupPointer` names (`left_ptr`, `hand2`, `watch`,
            // `xterm`); each maps to its CSS equivalent.
            "left_ptr" | "arrow" | "top_left_arrow" | "left_arrow" => Some(Self::Default),
            "question_arrow" | "whats_this" => Some(Self::Help),
            "hand2" | "hand1" | "hand" | "pointing_hand" => Some(Self::Pointer),
            "left_ptr_watch" | "half-busy" => Some(Self::Progress),
            "watch" => Some(Self::Wait),
            "plus" => Some(Self::Cell),
            "cross" => Some(Self::Crosshair),
            "xterm" | "ibeam" => Some(Self::Text),
            "link" => Some(Self::Alias),
            "circle" => Some(Self::NoDrop),
            "crossed_circle" | "forbidden" => Some(Self::NotAllowed),
            "openhand" | "fleur" => Some(Self::Grab),
            "closedhand" => Some(Self::Grabbing),
            "right_side" => Some(Self::EResize),
            "top_side" => Some(Self::NResize),
            "top_right_corner" => Some(Self::NeResize),
            "top_left_corner" => Some(Self::NwResize),
            "bottom_side" => Some(Self::SResize),
            "bottom_right_corner" => Some(Self::SeResize),
            "bottom_left_corner" => Some(Self::SwResize),
            "left_side" => Some(Self::WResize),
            "h_double_arrow" | "size_hor" => Some(Self::EwResize),
            "v_double_arrow" | "size_ver" => Some(Self::NsResize),
            "fd_double_arrow" | "size_bdiag" => Some(Self::NeswResize),
            "bd_double_arrow" | "size_fdiag" => Some(Self::NwseResize),
            "split_h" | "sb_h_double_arrow" => Some(Self::ColResize),
            "split_v" | "sb_v_double_arrow" => Some(Self::RowResize),
            "size_all" => Some(Self::AllScroll),
            _ => None,
        }
    }
}

/// Operation carried by an `OSC 22` pointer-shape sequence (issue #1762).
///
/// The parser only classifies and bounds the payload; terminal state stays
/// inert and the runtime owns the per-pane shape stack plus the focused-pane
/// platform dispatch. Queries (`?`-prefixed) are deferred: the parser emits
/// [`TerminalAction::OscUnknown`] for them so no reply is synthesized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerShapeOp {
    /// `OSC 22 ; [<name>|=<name>|<empty>]`: set (`None` = empty reset to default).
    ///
    /// Unknown names fail open to [`PointerShape::Default`] at parse time,
    /// so this arm never carries an invalid shape.
    Set {
        /// New shape, or `None` for an empty-payload reset.
        shape: Option<PointerShape>,
    },
    /// `OSC 22 ; >name[,name...]`: push all names (last is the top/current).
    Push {
        /// Pushed shapes in wire order (at least 1, at most [`MAX_OSC22_SHAPES`]).
        shapes: Box<[PointerShape]>,
    },
    /// `OSC 22 ; <[ignored>]`: pop the top of the stack (no-op when empty).
    Pop,
}

/// Maximum `OSC 22` shape names honored per push sequence.
///
/// Bounds one escape sequence's work: the kitty spec requires a minimum stack
/// of 16, so 16 names cover every conforming push while keeping the parsed
/// payload bounded; longer pushes fail closed as inert.
pub const MAX_OSC22_SHAPES: usize = 16;

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
    /// Lock a charset slot as GL for normal printing (`SO`/`SI` for
    /// `G1`/`G0`, locking shifts `LS2`/`LS3` for `G2`/`G3`).
    InvokeCharset {
        /// Slot to lock.
        slot: CharsetSlot,
    },
    /// Arm a single shift for exactly the next printed scalar
    /// (`SS2`/`SS3`; `G2`/`G3` only, the locking shift is untouched).
    SingleShiftCharset {
        /// Slot to arm for one printed scalar.
        slot: CharsetSlot,
    },

    // Keyboard protocol
    /// Kitty keyboard progressive-enhancement negotiation (CTX-0575).
    ///
    /// Emitted for `CSI = flags ; mode u`, `CSI > flags u`, `CSI < n u`, and
    /// `CSI ? u`. Terminal state owns the bounded flag register and the
    /// per-screen push/pop stack and synthesizes the `CSI ? flags u` reply
    /// for [`EnhancedKeyboardOp::Query`].
    EnhancedKeyboard {
        /// Classified wire operation.
        op: EnhancedKeyboardOp,
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
    /// Dynamic default-color operation (`OSC 10`/`OSC 11`, CTX-0381).
    ///
    /// The parser resolves the payload to a bounded query or set value;
    /// terminal state treats this as inert and the runtime answers queries
    /// from the resolved theme palette and applies authorized sets.
    OscDynamicColor {
        /// Which default color the operation addresses.
        target: DynamicColorTarget,
        /// Query or bounded set value.
        op: DynamicColorOp,
    },
    /// Palette operation (`OSC 4`, CTX-0392).
    ///
    /// The parser resolves the payload to at most [`MAX_OSC4_OPS`] bounded
    /// index/query-or-set pairs; terminal state treats this as inert and
    /// the runtime answers queries from the active 256-entry palette and
    /// applies authorized sets under the shared OSC color-set gate.
    /// Malformed sequences never reach this variant (fail-closed inert).
    OscPalette {
        /// Bounded index/query-or-set pairs in wire order.
        ops: Box<[PaletteOp]>,
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
    /// Terminal-originated notification request (`OSC 9`, `OSC 777`, Kitty
    /// `OSC 99` assembled, CTX-0577/CTX-1008).
    ///
    /// Terminal state treats this as inert; the runtime applies the
    /// bell/notification policy (default deny) and the capability/consent and
    /// rate gates before anything is surfaced.
    OscNotification {
        /// Recognized notification form and its bounded payload.
        notification: Notification,
    },
    /// One Kitty `OSC 99` title/body chunk (CTX-1008, issue #1763).
    ///
    /// Terminal state treats this as inert; the runtime assembles chunks by
    /// `id` into an [`Notification`] with source [`NotificationSource::Osc99`]
    /// and then applies the same consent and RC-8 gates as `OSC 9`/`OSC 777`.
    KittyNotificationChunk {
        /// Parsed chunk (identifier, title/body selector, bounded text).
        chunk: KittyNotificationChunk,
    },
    /// Hyperlink span begin/end (`OSC 8`); `None` ends the current span.
    OscHyperlink {
        /// Link identity and target, if any.
        link: Option<Hyperlink>,
    },
    /// Semantic prompt zone marker (`OSC 133`).
    ///
    /// For `D` (command output end) an optional `;exit_code` payload carries
    /// the exit status as a bounded signed integer; other kinds ignore the
    /// trailing payload. Absent or malformed exit codes are `None`.
    OscPromptMark {
        /// Which zone boundary was marked.
        kind: ZoneKind,
        /// Exit status for `D` (OutputEnd), `None` for other kinds or on parse failure.
        exit_code: Option<i32>,
    },
    /// Pointer-shape operation (`OSC 22`, issue #1762).
    ///
    /// The parser resolves the payload to a bounded set/push/pop op;
    /// terminal state treats this as inert and the runtime owns the per-pane
    /// shape stack plus the focused-pane platform dispatch. Queries
    /// (`?`-prefixed) never reach this variant (deferred as inert
    /// [`TerminalAction::OscUnknown`]).
    OscPointerShape {
        /// Classified set/push/pop operation.
        op: PointerShapeOp,
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
    /// Completed Kitty graphics transmission (`APC G ... ST`, CTX-0256).
    ///
    /// The parser pre-scans `APC` (which `vte` 0.15 leaves inert), parses the
    /// `G` control parameters, base64-decodes the payload with a fail-closed
    /// alphabet check, reassembles chunked `m=1`/`m=0` streams under the
    /// ledger cap, and emits exactly one action per completed transmission.
    /// Terminal state treats this as inert (images live in the runtime
    /// `KittyImageLayer`); the runtime routes it to
    /// `kitty_transmit`/`kitty_display_image`, preserving transmit-only
    /// (`a=t` stores without painting) and unknown-action
    /// (stored-not-painted) semantics from CTX-0248.
    ///
    /// The advanced subset (CTX-0950) rides in `control`: placement
    /// (`a=p`, ids, rects, z-index, virtual/relative flags), animation
    /// (`a=f`/`a=a`/`a=c` frame keys), deletion (`a=d` selector),
    /// suppression (`q=`), and local mediums (`t=f`/`t=t`/`t=s`, for which
    /// `payload` carries the validated path/shm-name bytes and `S=`/`O`
    /// bound the downstream read).
    KittyGraphics {
        /// Wire `f=` format value (`100` PNG, `24` RGB, `32` RGBA;
        /// `0` for control-only actions, which omit it).
        format_f: u32,
        /// Wire `s=` width for raw formats (`None` when absent; ignored for PNG).
        width_s: Option<u32>,
        /// Wire `v=` height for raw formats (`None` when absent; ignored for PNG).
        height_v: Option<u32>,
        /// Wire `a=` display action (`None` when absent means transmit-and-display).
        action_a: Option<char>,
        /// Wire `c=` explicit cell columns (`0` derives from pixels).
        cols_c: u16,
        /// Wire `r=` explicit cell rows (`0` derives from pixels).
        rows_r: u16,
        /// Wire `C=` cursor movement flag (`0` moves cursor, `1` keeps it).
        cursor_movement_c: u8,
        /// Base64-decoded payload bytes (assembled across `m=` chunks).
        payload: Box<[u8]>,
        /// Advanced control keys (CTX-0950; default = direct transmission).
        control: KittyControlKeys,
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
