//! Kitty `APC G` control parsing, base64 unwrap, and `m=` reassembly (CTX-0256).
//!
//! The VT state machine (`vte` 0.15) leaves `SOS/PM/APC` strings inert, so
//! [`crate::Parser`] pre-scans `APC` (`ESC _ ... ST`) and feeds complete raw
//! buffers here. This module parses the `G` graphics command, base64-decodes
//! with a fail-closed alphabet check, reassembles chunked `m=1`/`m=0`
//! streams, and hands assembled decoded bytes to the caller for routing to
//! `kitty_transmit`/`kitty_display_image`.
//!
//! The advanced subset (CTX-0950) adds placement (`a=p`, virtual `U=1`,
//! relative `P`/`Q`), animation (`a=f` frame data, `a=a` control, `a=c`
//! compose), deletion (`a=d`), queries (`a=q`), and local mediums
//! (`t=f`/`t=t` files, `t=s` shared memory) with sandbox validation.
//!
//! # Wire shape
//!
//! `ESC _ G <control> ; <base64> ST` where `ST` is `ESC \` or `BEL` (C1 `ST`
//! `0x9C` also terminates inside the scanner). C1 `APC` (`0x9F`) is
//! intentionally not an introducer: it overlaps UTF-8 continuation bytes and
//! kitty/chafa always emit `ESC _`. `<control>` is a comma-separated
//! `key=value` list. Routing needs `f` (format), `s`/`v` (raw dimensions),
//! `a` (action), `c`/`r` (cell spans, or frame numbers for animation), and
//! `m` (more-chunks); the advanced keys (`t`, `i`, `I`, `p`, `q`, `d`,
//! `x`, `y`, `w`, `h`, `X`, `Y`, `z`, `U`, `P`, `Q`, `H`, `V`, `S`, `O`,
//! `N`) ride in [`KittyControlKeys`]. `o=` accepts only `z`; `t=` accepts
//! only `d`/`f`/`t`/`s`. Truly unknown keys stay ignored (future-proof).
//!
//! # Bounds
//!
//! - [`KITTY_APC_LEDGER_CAP`] is the accepted IMG-1 4 MiB cap for
//!   compressed payloads (`f=100` PNG) and for any stream whose decoded size
//!   is not declared up front.
//! - Raw `f=24`/`f=32` streams with both `s`/`v` present and no `o=z`
//!   compression key are not compressed:
//!   the payload *is* the bitmap, so their bound is the declared exact size
//!   `s * v * channels`, validated on the first chunk against the IMG-2/IMG-3
//!   decode caps ([`KITTY_APC_DECODE_MAX_DIMENSION`],
//!   [`KITTY_APC_DECODE_MAX_PIXELS`], [`KITTY_APC_DECODE_MAX_BYTES`]) before
//!   any payload byte is buffered. Full-screen HD/4K RGBA frames from `chafa
//!   -f kitty` therefore fit, while a raw stream can never grow past its own
//!   claim.
//! - Local-medium streams (`t=f`/`t=t`/`t=s`) name a path, not pixels:
//!   their bound is [`KITTY_APC_PATH_MAX_BYTES`], and an `S=` read-size
//!   claim above the decode cap is refused on the first chunk.
//! - Control-only actions (`a=p`/`d`/`a`/`c`/`q`) must arrive bodiless.
//! - Base64 is decoded incrementally into one bounded payload buffer, so
//!   pending chunks, current output, and decoder scratch never form a second
//!   large APC allocation. The control header has a separate 4 KiB bound.
//! - PNG dimensions remain governed by the downstream decoder contract.
//! - Every growth and decoded-length check runs before the corresponding
//!   allocation or adapter hand-off.
//!
//! # Fail-closed behavior
//!
//! Every rejection warns via rate-limited `eprintln!` (diagnostic only, no
//! state change) and yields no completed transmission: bad base64 alphabet,
//! malformed control, missing `f`, oversize claim, ledger-cap overflow,
//! decode-cap violation, unexpected control payload, or sandbox path
//! rejection all store nothing and paint nothing. The caller
//! (`Parser`) emits no action on rejection. Chunked streams drop only the
//! offending stream on oversize, mirroring `KittyGraphicsStub` semantics.
//!
//! An open stream never outlives a protocol violation or a stall:
//!
//! - A continuation chunk with a missing or malformed `m=`, with an action
//!   change (`a=d`, which the kitty specification says must abort a
//!   partial upload; frame data on an image stream or any non-`a=f` action
//!   on a frame stream), drops the open stream as well as the offending
//!   chunk.
//! - Input that arrives while a stream is open but is not continuation
//!   payload (text for the VT state machine, non-`G` or discarded `APC`
//!   bytes, and continuation headers themselves) is counted; past
//!   [`KITTY_APC_STALL_MAX_BYTES`] the stream is dropped, so a writer cannot
//!   pin its buffered payload by interleaving other output or by sending
//!   empty `m=1` chunks. Only accepted payload bytes reset the count.
//! - The bound counts bytes, not time: a writer that opens a stream and then
//!   goes silent keeps it until more output arrives or the pane's parser is
//!   dropped. The pinned amount stays within the stream's own declared size.

use crate::diag::{RejectLog, warn_rejection};

/// Accepted IMG-1 parser payload cap for compressed or undeclared-size streams.
pub const KITTY_APC_LEDGER_CAP: usize = 4 * 1024 * 1024;

pub(crate) const KITTY_APC_MAX_CONTROL_BYTES: usize = 4096;

const KITTY_APC_CODEC_SCRATCH_BYTES: usize = 4;

/// Interleaved non-continuation bytes tolerated while a chunked stream is
/// open before the stream is dropped as stalled.
///
/// The kitty protocol expects a client to send every chunk of one image
/// before any other graphics command, so well-behaved producers (chafa,
/// `kitten icat`) interleave nothing; 64 KiB leaves ample room for stray
/// output while keeping the window in which a stalled writer pins payload
/// memory bounded by its own interleaved traffic.
pub const KITTY_APC_STALL_MAX_BYTES: usize = 64 * 1024;

/// Side cap mirroring `bitty-rich::kitty_place::KITTY_DECODE_MAX_DIMENSION`
/// (W-141: the decoder moved to the `bitty-graphics` extension; the
/// Core-retained ceiling now lives in `kitty_place`).
pub const KITTY_APC_DECODE_MAX_DIMENSION: u32 = 8192;

/// Area cap mirroring `bitty-rich::kitty_place::KITTY_DECODE_MAX_PIXELS`
/// (W-141: see [`KITTY_APC_DECODE_MAX_DIMENSION`]).
pub const KITTY_APC_DECODE_MAX_PIXELS: u64 = 4096 * 4096;

/// Byte cap mirroring `bitty-rich::kitty_place::KITTY_DECODE_MAX_BYTES`
/// (W-141: see [`KITTY_APC_DECODE_MAX_DIMENSION`]).
pub const KITTY_APC_DECODE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Decoded-byte cap for file/shm path payloads (`t=f`/`t=t`/`t=s`).
///
/// Paths arrive base64-encoded in the payload section like pixel data, but
/// they name a filesystem or shm object rather than carrying bytes: 4096
/// (`PATH_MAX` parity) bounds the decoded name. Anything longer is not a
/// usable path and is rejected fail-closed before any open is attempted
/// downstream.
pub const KITTY_APC_PATH_MAX_BYTES: usize = 4096;

/// Maximum accepted POSIX shared-memory name length (`NAME_MAX` parity,
/// matching the kitty specification that shm names fit the OS limit).
pub const KITTY_APC_SHM_NAME_MAX: usize = 255;

/// Required substring of a `t=t` temporary-file path.
///
/// The kitty specification requires the terminal to delete the file after
/// reading it only when the path sits in a known temporary directory and
/// carries this marker; the parse-time half of that rule (marker presence)
/// is enforced here, the directory half where the file is opened
/// downstream (see [`validate_kitty_path`]).
pub const KITTY_APC_TMP_NAME_MARKER: &str = "tty-graphics-protocol";

/// Kitty graphics transmission medium (wire `t=` key).
///
/// Reference: kitty `graphics-protocol.rst` ("The transmission medium") and
/// ghostty `graphics_command.zig` (`Transmission.Medium`). `Direct` is the
/// wire default; local mediums ignore the `m=` chunking key (kitty and
/// ghostty both complete them single-shot, which `mpv` relies on for `t=s`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KittyMedium {
    /// `t=d` (or absent): pixel data rides base64-encoded in the escape
    /// code itself. The only medium that supports `m=` chunking.
    #[default]
    Direct,
    /// `t=f`: read pixel data from a regular file named by the payload.
    File,
    /// `t=t`: like [`Self::File`] but the terminal deletes the file after
    /// reading it (only inside a known temp dir with [`KITTY_APC_TMP_NAME_MARKER`]
    /// in the path).
    TempFile,
    /// `t=s`: read pixel data from a POSIX shared-memory object named by
    /// the payload (unlinked after reading).
    SharedMemory,
}

impl KittyMedium {
    /// Whether pixel bytes travel inside the escape code (chunkable).
    #[must_use]
    pub const fn is_direct(self) -> bool {
        matches!(self, Self::Direct)
    }
}

/// Advanced kitty control keys carried alongside the base transmit fields.
///
/// The base [`KittyApcParams`] fields (`f`/`s`/`v`/`a`/`c`/`r`/`C`/`m`/`o`)
/// already cover direct transmission; this struct carries every other key
/// the advanced subset needs: placement (`a=p`), animation (`a=f` frame
/// data, `a=a` control, `a=c` compose), deletion (`a=d`), queries (`a=q`),
/// and local transmission mediums (`t=f`/`t=t`/`t=s` with `S`/`O`).
///
/// Several wire keys are overloaded per action (kitty control-data
/// reference): `c`/`r` name cell spans for display but 1-based frame
/// numbers for animation; `s`/`v` name image width/height for transmit but
/// animation state/loop count for `a=a`; `z` names z-index for display but
/// frame gap for animation; `X` names a cell x-offset for display but the
/// compose/blend mode for animation; `Y` names a cell y-offset for display
/// but the frame background color for animation. The parser stores the raw
/// key values here (and in the base fields); the accessors below give the
/// per-action view so each consumer reads the meaning its action defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyControlKeys {
    /// Wire `t=` transmission medium (`d` when absent).
    pub medium: KittyMedium,
    /// Wire `i=` image id (`0` when absent: anonymous image).
    pub image_id: u32,
    /// Wire `I=` image number (`0` when absent). Mutually exclusive with
    /// a non-zero [`Self::image_id`] (the specification calls the
    /// combination an error).
    pub image_number: u32,
    /// Wire `p=` placement id (`0` when absent: anonymous placement).
    pub placement_id: u32,
    /// Wire `q=` reply suppression (`0` replies, `1` suppresses `OK`,
    /// `2` suppresses failures too).
    pub quiet: u8,
    /// Wire `d=` delete selector (`None` when absent, i.e. `a`).
    pub delete: Option<char>,
    /// Wire `x=`/`y=` source-rectangle left/top edge in pixels.
    pub src_x: u32,
    /// Wire `y=` source-rectangle top edge in pixels.
    pub src_y: u32,
    /// Wire `w=` source-rectangle width in pixels (`0` = full width).
    pub src_w: u32,
    /// Wire `h=` source-rectangle height in pixels (`0` = full height).
    pub src_h: u32,
    /// Wire `X=` cell x-offset in pixels (display), or compose/blend mode
    /// (`0` alpha blend, `1` replace) for `a=f`/`a=c`.
    pub cell_x_offset: u32,
    /// Wire `Y=` cell y-offset in pixels (display), or 32-bit RGBA
    /// background color for `a=f` frame creation.
    pub cell_y_offset: u32,
    /// Wire `z=` z-index for display (signed; negative draws under text,
    /// below `INT32_MIN/2` draws under non-default cell backgrounds), or
    /// frame gap in milliseconds for animation (`0` ignored, negative =
    /// gapless skip).
    pub z_index: i32,
    /// Wire `U=` virtual-placement flag (`1` creates a `U+10EEEE`
    /// prototype instead of a screen-anchored placement).
    pub unicode_placement: u8,
    /// Wire `P=` parent image id for relative placement (`0` = none).
    pub parent_id: u32,
    /// Wire `Q=` parent placement id for relative placement (`0` = none).
    pub parent_placement_id: u32,
    /// Wire `H=` horizontal cell offset from the parent placement origin.
    pub parent_dx: i32,
    /// Wire `V=` vertical cell offset from the parent placement origin.
    pub parent_dy: i32,
    /// Wire `S=` exact byte count to read from a file/shm object
    /// (`0` = read to end / derive from the image claim).
    pub data_size: u32,
    /// Wire `O=` byte offset to start reading a file/shm object at.
    pub data_offset: u32,
    /// Wire `N=` client usage-hint bitmask (`1` = transient).
    pub usage_hints: u32,
}

impl Default for KittyControlKeys {
    fn default() -> Self {
        Self {
            medium: KittyMedium::Direct,
            image_id: 0,
            image_number: 0,
            placement_id: 0,
            quiet: 0,
            delete: None,
            src_x: 0,
            src_y: 0,
            src_w: 0,
            src_h: 0,
            cell_x_offset: 0,
            cell_y_offset: 0,
            z_index: 0,
            unicode_placement: 0,
            parent_id: 0,
            parent_placement_id: 0,
            parent_dx: 0,
            parent_dy: 0,
            data_size: 0,
            data_offset: 0,
            usage_hints: 0,
        }
    }
}

impl KittyControlKeys {
    /// Whether this command creates a virtual (`U+10EEEE`) prototype
    /// placement rather than a screen-anchored one.
    #[must_use]
    pub const fn is_virtual_placement(self) -> bool {
        self.unicode_placement == 1
    }

    /// Whether this relative placement names a parent (`P=` non-zero).
    #[must_use]
    pub const fn has_parent(self) -> bool {
        self.parent_id != 0
    }
}

/// Parsed `G` control parameters needed for routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyApcParams {
    /// Wire `f=` format value (`0` when absent: control-only actions
    /// (`a=p`/`a=d`/`a=a`/`a=c`/`a=q`) omit it; data-carrying actions
    /// (absent action, `a=T`/`a=t`, frame data `a=f`) require it).
    pub format_f: u32,
    /// Wire `s=` width (`None` when absent). For `a=a` this key instead
    /// carries the animation state (`1` stop, `2` run-loading, `3` run);
    /// see [`KittyApcParams::anim_state`].
    pub width_s: Option<u32>,
    /// Wire `v=` height (`None` when absent). For `a=a` this key instead
    /// carries the loop count; see [`KittyApcParams::anim_loops`].
    pub height_v: Option<u32>,
    /// Wire `a=` action (`None` when absent means transmit-and-display).
    pub action_a: Option<char>,
    /// Wire `c=` columns (`0` when absent). For animation commands this
    /// key instead carries a 1-based frame number; see
    /// [`KittyApcParams::frame_c`].
    pub cols_c: u16,
    /// Wire `r=` rows (`0` when absent). For animation commands this key
    /// instead carries a 1-based frame number; see
    /// [`KittyApcParams::frame_r`].
    pub rows_r: u16,
    /// Wire `C=` cursor movement (`0` moves cursor, `1` keeps it, default `0`).
    pub cursor_movement_c: u8,
    /// Wire `m=` more-chunks (`false` when absent, i.e. single-shot/final).
    /// Honored only for direct (`t=d`) transmissions; local mediums
    /// (`t=f`/`t=t`/`t=s`) always complete single-shot (kitty/ghostty
    /// parity: `mpv` relies on this for `t=s`).
    pub more: bool,
    /// Whether the wire `o=z` compression key is present.
    pub compressed: bool,
    /// Advanced control keys (placement, animation, deletion, medium).
    pub keys: KittyControlKeys,
}

impl KittyApcParams {
    /// `a=a` animation state from the overloaded `s=` key (`1` stop,
    /// `2` run-loading, `3` run). `None` when `s=` is absent or does not
    /// fit the one-byte state value.
    #[must_use]
    pub fn anim_state(self) -> Option<u8> {
        self.width_s.and_then(|s| u8::try_from(s).ok())
    }

    /// `a=a` loop count from the overloaded `v=` key (`0` ignored,
    /// `1` infinite, `n > 1` plays `n - 1` loops). `0` when absent.
    #[must_use]
    pub fn anim_loops(self) -> u32 {
        self.height_v.unwrap_or(0)
    }

    /// 1-based frame number from the overloaded `c=` key (base canvas for
    /// `a=f` creation, edited frame for `a=c`, current frame for `a=a`).
    /// `0` when absent.
    #[must_use]
    pub const fn frame_c(self) -> u32 {
        self.cols_c as u32
    }

    /// 1-based frame number from the overloaded `r=` key (edited frame
    /// for `a=f`, source frame for `a=c`, affected frame for `a=a`).
    /// `0` when absent.
    #[must_use]
    pub const fn frame_r(self) -> u32 {
        self.rows_r as u32
    }

    /// Frame gap in milliseconds from the overloaded `z=` key (`0`
    /// ignored, negative = gapless skip). Defaults to the ghostty/kitty
    /// `40ms` only downstream; the parser reports the raw value.
    #[must_use]
    pub const fn frame_gap_ms(self) -> i32 {
        self.keys.z_index
    }

    /// Compose/blend mode from the overloaded `X=` key (`0` alpha blend,
    /// `1` replace).
    #[must_use]
    pub const fn compose_mode(self) -> u32 {
        self.keys.cell_x_offset
    }

    /// Frame background color from the overloaded `Y=` key (32-bit RGBA,
    /// `0` = transparent black).
    #[must_use]
    pub const fn frame_background(self) -> u32 {
        self.keys.cell_y_offset
    }

    /// Whether this command carries pixel data (`a` absent/`T`/`t`/`f`
    /// with a direct or local medium) as opposed to being control-only
    /// (`a=p`/`d`/`a`/`c`/`q`, which must arrive with an empty payload).
    #[must_use]
    pub const fn carries_data(self) -> bool {
        matches!(self.action_a, None | Some('T' | 't' | 'f'))
    }
}

impl KittyCompleted {
    /// `a=a` animation state from the overloaded `s=` key. See
    /// [`KittyApcParams::anim_state`].
    #[must_use]
    pub fn anim_state(&self) -> Option<u8> {
        self.width_s.and_then(|s| u8::try_from(s).ok())
    }

    /// `a=a` loop count from the overloaded `v=` key. See
    /// [`KittyApcParams::anim_loops`].
    #[must_use]
    pub fn anim_loops(&self) -> u32 {
        self.height_v.unwrap_or(0)
    }

    /// 1-based frame number from the overloaded `c=` key. See
    /// [`KittyApcParams::frame_c`].
    #[must_use]
    pub const fn frame_c(&self) -> u32 {
        self.cols_c as u32
    }

    /// 1-based frame number from the overloaded `r=` key. See
    /// [`KittyApcParams::frame_r`].
    #[must_use]
    pub const fn frame_r(&self) -> u32 {
        self.rows_r as u32
    }

    /// Frame gap in milliseconds from the overloaded `z=` key. See
    /// [`KittyApcParams::frame_gap_ms`].
    #[must_use]
    pub const fn frame_gap_ms(&self) -> i32 {
        self.keys.z_index
    }

    /// Compose/blend mode from the overloaded `X=` key. See
    /// [`KittyApcParams::compose_mode`].
    #[must_use]
    pub const fn compose_mode(&self) -> u32 {
        self.keys.cell_x_offset
    }

    /// Frame background color from the overloaded `Y=` key. See
    /// [`KittyApcParams::frame_background`].
    #[must_use]
    pub const fn frame_background(&self) -> u32 {
        self.keys.cell_y_offset
    }
}

/// Why an `APC G` buffer was rejected (fail-closed, warns, emits nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyApcReject {
    /// Buffer does not start with `G` (other APC command; inert, silent).
    NotGraphics,
    /// Control section malformed (bad `key=value` shape or bad numeric).
    MalformedControl,
    /// Required `f=` format value absent.
    MissingFormat,
    /// `a=` value longer than one character.
    BadAction,
    /// `m=` value other than `0`/`1`.
    BadMore,
    /// Base64 payload uses a non-alphabet byte or bad padding/length.
    BadBase64,
    /// A control-only action (`a=p`/`d`/`a`/`c`/`q`) arrived with payload
    /// bytes. Such commands carry no data; a body is either garbage or
    /// smuggling, so the stream is dropped fail-closed.
    UnexpectedPayload,
    /// A `t=f`/`t=t`/`t=s` path or shm name failed sandbox validation:
    /// empty, overlong, NUL-bearing, `..` traversal (file mediums),
    /// malformed shm name, or a `t=t` path without the required
    /// `tty-graphics-protocol` marker. No open is attempted.
    BadPath,
    /// Raw `s`/`v` claim exceeds decode side/area/byte caps.
    OversizeClaim,
    /// Growth would exceed the parser payload budget.
    Oversize,
    /// Continuation (`m=` present, no `f=`) with no open stream.
    Orphan,
    /// Open stream dropped: protocol violation mid-stream or stall bound.
    Aborted,
}

impl std::fmt::Display for KittyApcReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotGraphics => write!(f, "not a kitty graphics command"),
            Self::MalformedControl => write!(f, "malformed kitty control parameters"),
            Self::MissingFormat => write!(f, "kitty transmission missing f= format"),
            Self::BadAction => write!(f, "malformed kitty a= action"),
            Self::BadMore => write!(f, "malformed kitty m= flag"),
            Self::BadBase64 => write!(f, "invalid kitty base64 payload"),
            Self::UnexpectedPayload => write!(f, "kitty control action with payload"),
            Self::BadPath => write!(f, "kitty file/shm path rejected by sandbox"),
            Self::OversizeClaim => write!(f, "kitty s/v claim exceeds decode caps"),
            Self::Oversize => write!(f, "kitty payload exceeds parser budget"),
            Self::Orphan => write!(f, "kitty chunk without an open stream"),
            Self::Aborted => write!(f, "kitty chunked stream aborted"),
        }
    }
}

/// Completed transmission: routing params plus assembled decoded bytes.
///
/// For direct (`t=d`) transmissions `payload` holds pixel (or
/// zlib-compressed, already decompressed here) bytes. For local mediums
/// (`t=f`/`t=t`/`t=s`) it holds the validated path/shm-name bytes: the
/// terminal reads the pixels from the named object downstream (regular
/// files only, temp-dir + marker rule for `t=t`, POSIX shm rules for
/// `t=s`), applying `S=`/`O=` from [`KittyControlKeys`], still under the
/// decode caps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyCompleted {
    /// Wire `f=` format value (`0` for control-only actions, which omit it).
    pub format_f: u32,
    /// Wire `s=` width (`None` when absent).
    pub width_s: Option<u32>,
    /// Wire `v=` height (`None` when absent).
    pub height_v: Option<u32>,
    /// Wire `a=` action (`None` when absent).
    pub action_a: Option<char>,
    /// Wire `c=` columns (`0` when absent).
    pub cols_c: u16,
    /// Wire `r=` rows (`0` when absent).
    pub rows_r: u16,
    /// Wire `C=` cursor movement (`0` moves cursor, `1` keeps it).
    pub cursor_movement_c: u8,
    /// Assembled base64-decoded bytes across `m=` chunks.
    pub payload: Box<[u8]>,
    /// Advanced control keys (medium, ids, rects, animation, deletion).
    pub keys: KittyControlKeys,
}

/// Outcome of feeding one `APC G` buffer to the assembler.
#[derive(Debug)]
pub enum KittyFeedOutcome {
    /// `m=1` buffered; more chunks expected. No action emitted.
    NeedMore {
        /// Total base64-encoded bytes held in flight.
        buffered_encoded: usize,
    },
    /// `m=0` completed the stream (or lone single-shot). Emit one action.
    Completed(KittyCompleted),
    /// Rejected fail-closed (warned, stored nothing). No action emitted.
    Rejected(KittyApcReject),
}

/// In-flight `m=1` stream: first-chunk params plus one decoded payload buffer.
#[derive(Debug, Clone)]
struct PendingKitty {
    format_f: u32,
    width_s: Option<u32>,
    height_v: Option<u32>,
    action_a: Option<char>,
    cols_c: u16,
    rows_r: u16,
    cursor_movement_c: u8,
    compressed: bool,
    keys: KittyControlKeys,
    /// Control-only action (`a=p`/`d`/`a`/`c`/`q`): completes single-shot
    /// and must carry no payload bytes.
    control_only: bool,
    /// Animation frame stream (opened with `a=f`): continuations must
    /// repeat `a=f` per the specification.
    is_frame: bool,
    encoded_len: usize,
    /// Decoded-byte bound for this stream: IMG-1 for compressed or
    /// undeclared-size payloads, the exact declared size for raw claims.
    limit: usize,
    /// Non-continuation bytes seen since the last accepted chunk.
    interleaved: usize,
    decoder: Base64Stream,
    payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Base64Stream {
    carry: [u8; 4],
    carry_len: usize,
    padding: usize,
    finished: bool,
}

#[derive(Debug, Clone, Copy)]
struct IntakeBudget {
    payload_limit: usize,
    total_limit: usize,
    current: usize,
    decoded: usize,
    peak_payload: usize,
    peak_total: usize,
}

impl IntakeBudget {
    fn new(payload_limit: usize) -> Self {
        Self {
            payload_limit,
            total_limit: payload_limit
                .saturating_add(KITTY_APC_MAX_CONTROL_BYTES)
                .saturating_add(KITTY_APC_CODEC_SCRATCH_BYTES),
            current: 0,
            decoded: 0,
            peak_payload: 0,
            peak_total: KITTY_APC_CODEC_SCRATCH_BYTES,
        }
    }

    fn total(&self) -> usize {
        self.current
            .saturating_add(self.decoded)
            .saturating_add(KITTY_APC_CODEC_SCRATCH_BYTES)
    }

    fn reserve_current(&mut self, amount: usize) -> bool {
        let Some(next) = self.current.checked_add(amount) else {
            return false;
        };
        if next > KITTY_APC_MAX_CONTROL_BYTES
            || self
                .decoded
                .saturating_add(next)
                .saturating_add(KITTY_APC_CODEC_SCRATCH_BYTES)
                > self.total_limit
        {
            return false;
        }
        self.current = next;
        self.peak_total = self.peak_total.max(self.total());
        true
    }

    fn release_current(&mut self, amount: usize) {
        self.current = self.current.saturating_sub(amount);
    }

    fn reserve_retained(&mut self, amount: usize) -> bool {
        if amount > self.payload_limit
            || self
                .current
                .saturating_add(amount)
                .saturating_add(KITTY_APC_CODEC_SCRATCH_BYTES)
                > self.total_limit
        {
            return false;
        }
        self.decoded = amount;
        self.peak_payload = self.peak_payload.max(amount);
        self.peak_total = self.peak_total.max(self.total());
        true
    }

    fn clear_retained(&mut self) {
        self.decoded = 0;
    }
}

impl Base64Stream {
    fn push(
        &mut self,
        input: &[u8],
        output: &mut Vec<u8>,
        limit: usize,
    ) -> Result<(), KittyApcReject> {
        for &byte in input {
            if self.finished {
                return Err(KittyApcReject::BadBase64);
            }
            if self.padding != 0 {
                if byte != b'=' {
                    return Err(KittyApcReject::BadBase64);
                }
                self.padding += 1;
                if self.padding > 2 {
                    return Err(KittyApcReject::BadBase64);
                }
                if self.padding == 2 {
                    self.finish_padded(output, limit)?;
                    self.finished = true;
                }
                continue;
            }
            if byte == b'=' {
                if self.carry_len < 2 {
                    return Err(KittyApcReject::BadBase64);
                }
                self.padding = 1;
                if self.carry_len == 3 {
                    self.finish_padded(output, limit)?;
                    self.finished = true;
                }
                continue;
            }
            if sextet(byte).is_none() {
                return Err(KittyApcReject::BadBase64);
            }
            self.carry[self.carry_len] = byte;
            self.carry_len += 1;
            if self.carry_len == 4 {
                self.finish_full(output, limit)?;
            }
        }
        Ok(())
    }

    fn finish(&mut self, output: &mut Vec<u8>, limit: usize) -> Result<(), KittyApcReject> {
        if self.finished {
            return Ok(());
        }
        if self.padding != 0 {
            return Err(KittyApcReject::BadBase64);
        }
        match self.carry_len {
            0 => {}
            1 => return Err(KittyApcReject::BadBase64),
            2 => self.finish_tail(output, limit, 2)?,
            3 => self.finish_tail(output, limit, 3)?,
            _ => return Err(KittyApcReject::BadBase64),
        }
        self.finished = true;
        Ok(())
    }

    fn finish_full(&mut self, output: &mut Vec<u8>, limit: usize) -> Result<(), KittyApcReject> {
        let triple = (sextet(self.carry[0]).ok_or(KittyApcReject::BadBase64)? << 18)
            | (sextet(self.carry[1]).ok_or(KittyApcReject::BadBase64)? << 12)
            | (sextet(self.carry[2]).ok_or(KittyApcReject::BadBase64)? << 6)
            | sextet(self.carry[3]).ok_or(KittyApcReject::BadBase64)?;
        append_decoded(
            output,
            &[(triple >> 16) as u8, (triple >> 8) as u8, triple as u8],
            limit,
        )?;
        self.carry_len = 0;
        Ok(())
    }

    fn finish_padded(&mut self, output: &mut Vec<u8>, limit: usize) -> Result<(), KittyApcReject> {
        let expected = if self.padding == 1 { 3 } else { 2 };
        if self.carry_len != expected {
            return Err(KittyApcReject::BadBase64);
        }
        let first = sextet(self.carry[0]).ok_or(KittyApcReject::BadBase64)?;
        let second = sextet(self.carry[1]).ok_or(KittyApcReject::BadBase64)?;
        if expected == 3 {
            let third = sextet(self.carry[2]).ok_or(KittyApcReject::BadBase64)?;
            let bits = (first << 18) | (second << 12) | (third << 6);
            append_decoded(output, &[(bits >> 16) as u8, (bits >> 8) as u8], limit)?;
        } else {
            let bits = (first << 18) | (second << 12);
            append_decoded(output, &[(bits >> 16) as u8], limit)?;
        }
        self.carry_len = 0;
        Ok(())
    }

    fn finish_tail(
        &mut self,
        output: &mut Vec<u8>,
        limit: usize,
        len: usize,
    ) -> Result<(), KittyApcReject> {
        let first = sextet(self.carry[0]).ok_or(KittyApcReject::BadBase64)?;
        let second = sextet(self.carry[1]).ok_or(KittyApcReject::BadBase64)?;
        if len == 2 {
            append_decoded(output, &[((first << 18 | second << 12) >> 16) as u8], limit)
        } else {
            let third = sextet(self.carry[2]).ok_or(KittyApcReject::BadBase64)?;
            append_decoded(
                output,
                &[
                    ((first << 18 | second << 12 | third << 6) >> 16) as u8,
                    ((first << 18 | second << 12 | third << 6) >> 8) as u8,
                ],
                limit,
            )
        }
    }
}

impl PendingKitty {
    fn push(
        &mut self,
        input: &[u8],
        available: usize,
        budget: &mut IntakeBudget,
    ) -> Result<(), KittyApcReject> {
        let encoded_len = self
            .encoded_len
            .checked_add(input.len())
            .ok_or(KittyApcReject::Oversize)?;
        if encoded_len > max_encoded_len_for_decode_cap(available) {
            return Err(KittyApcReject::Oversize);
        }
        self.encoded_len = encoded_len;
        self.decoder.push(input, &mut self.payload, available)?;
        if !budget.reserve_retained(self.payload.capacity()) {
            return Err(KittyApcReject::Oversize);
        }
        Ok(())
    }
}

fn sextet(byte: u8) -> Option<u32> {
    match byte {
        b'A'..=b'Z' => Some(u32::from(byte - b'A')),
        b'a'..=b'z' => Some(u32::from(byte - b'a' + 26)),
        b'0'..=b'9' => Some(u32::from(byte - b'0' + 52)),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn append_decoded(output: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Result<(), KittyApcReject> {
    let required = output
        .len()
        .checked_add(bytes.len())
        .ok_or(KittyApcReject::Oversize)?;
    if required > limit {
        return Err(KittyApcReject::Oversize);
    }
    if output.capacity() < required {
        let quantum = if limit >= 4096 { 4096 } else { 1 };
        let target = required
            .saturating_add(quantum - 1)
            .checked_div(quantum)
            .and_then(|value| value.checked_mul(quantum))
            .unwrap_or(limit)
            .min(limit);
        if target < required {
            return Err(KittyApcReject::Oversize);
        }
        output
            .try_reserve_exact(target - output.len())
            .map_err(|_| KittyApcReject::Oversize)?;
    }
    output.extend_from_slice(bytes);
    Ok(())
}

fn max_encoded_len_for_decode_cap(decode_cap: usize) -> usize {
    let full = decode_cap / 3;
    let remainder = decode_cap % 3;
    full.saturating_mul(4)
        .saturating_add(if remainder == 0 { 0 } else { 4 })
}

#[derive(Debug, Clone)]
pub struct KittyApcAssembler {
    pending: Option<PendingKitty>,
    current_final: Option<bool>,
    ledger_cap: usize,
    decode_cap: usize,
    budget: IntakeBudget,
    log: RejectLog,
}

impl KittyApcAssembler {
    #[must_use]
    pub fn new() -> Self {
        Self::with_caps(KITTY_APC_LEDGER_CAP, KITTY_APC_DECODE_MAX_BYTES)
    }

    #[must_use]
    pub fn with_ledger_cap(ledger_cap: usize) -> Self {
        Self::with_caps(ledger_cap, KITTY_APC_DECODE_MAX_BYTES)
    }

    #[must_use]
    pub fn with_caps(ledger_cap: usize, decode_cap: usize) -> Self {
        Self {
            pending: None,
            current_final: None,
            ledger_cap,
            decode_cap,
            // The intake budget spans the largest stream any bound admits
            // (raw claims up to the decode cap); each stream is further
            // limited by its own `PendingKitty::limit`.
            budget: IntakeBudget::new(decode_cap),
            log: RejectLog::default(),
        }
    }

    #[must_use]
    pub const fn ledger_cap(&self) -> usize {
        self.ledger_cap
    }

    #[must_use]
    pub const fn decode_cap(&self) -> usize {
        self.decode_cap
    }

    /// IMG-1 bound for compressed or undeclared-size streams.
    #[must_use]
    fn compressed_cap(&self) -> usize {
        self.ledger_cap.min(self.decode_cap)
    }

    /// Decoded-byte bound for a new stream, validated before buffering.
    ///
    /// Local mediums (`t=f`/`t=t`/`t=s`) name a path rather than carrying
    /// pixels: their bound is [`KITTY_APC_PATH_MAX_BYTES`], and an `S=`
    /// read-size claim above the decode cap is refused up front so the
    /// downstream read can never be tricked into an unbounded allocation.
    /// Control-only actions carry no payload at all (bound `0`; any byte
    /// is [`KittyApcReject::UnexpectedPayload`]).
    /// Uncompressed raw formats with a non-zero `s`/`v` claim are bounded by
    /// the exact declared size (checked against the decode caps here, so
    /// oversize claims are refused on the first chunk). Compressed (`o=z`)
    /// and everything else keep the IMG-1 compressed cap.
    fn stream_limit(&self, params: &KittyApcParams) -> Result<usize, KittyApcReject> {
        if !params.carries_data() {
            return Ok(0);
        }
        if !params.keys.medium.is_direct() {
            if usize::try_from(params.keys.data_size).unwrap_or(usize::MAX) > self.decode_cap {
                return Err(KittyApcReject::OversizeClaim);
            }
            return Ok(KITTY_APC_PATH_MAX_BYTES);
        }
        if params.compressed {
            return Ok(self.compressed_cap());
        }
        match raw_claim_bytes(
            params.format_f,
            params.width_s,
            params.height_v,
            self.decode_cap,
        )? {
            Some(bytes) => Ok(bytes),
            None => Ok(self.compressed_cap()),
        }
    }

    pub(crate) fn reserve_header_byte(&mut self) -> bool {
        self.budget.reserve_current(1)
    }

    pub(crate) fn release_header(&mut self, amount: usize) {
        self.budget.release_current(amount);
    }

    #[cfg(test)]
    pub(crate) fn peak_memory(&self) -> usize {
        self.budget.peak_payload
    }

    #[cfg(test)]
    pub(crate) fn peak_total_memory(&self) -> usize {
        self.budget.peak_total
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn pending_encoded_len(&self) -> usize {
        self.pending
            .as_ref()
            .map_or(0, |pending| pending.encoded_len)
    }

    pub fn abort(&mut self) -> bool {
        self.current_final = None;
        self.budget.clear_retained();
        self.pending.take().is_some()
    }

    /// Charges `amount` interleaved non-continuation bytes against the open
    /// stream's stall bound ([`KITTY_APC_STALL_MAX_BYTES`]).
    ///
    /// Returns `true` when this call dropped the open stream. A no-op
    /// without an open stream, so callers may charge unconditionally.
    pub fn note_interleaved(&mut self, amount: usize) -> bool {
        let Some(pending) = self.pending.as_mut() else {
            return false;
        };
        pending.interleaved = pending.interleaved.saturating_add(amount);
        if pending.interleaved <= KITTY_APC_STALL_MAX_BYTES {
            return false;
        }
        self.abort();
        self.warn_reject(KittyApcReject::Aborted, "stalled stream");
        true
    }

    pub fn feed(&mut self, raw: &[u8]) -> KittyFeedOutcome {
        let (control, payload) = match raw.iter().position(|&byte| byte == b';') {
            Some(semi) => (&raw[..semi], &raw[semi + 1..]),
            None => (raw, &[][..]),
        };
        if let Err(reason) = self.begin_control(control) {
            return KittyFeedOutcome::Rejected(reason);
        }
        if let Err(reason) = self.push_payload(payload) {
            return KittyFeedOutcome::Rejected(reason);
        }
        self.finish()
    }

    pub(crate) fn begin_control(&mut self, control: &[u8]) -> Result<(), KittyApcReject> {
        if self.current_final.is_some() {
            let reason = KittyApcReject::Orphan;
            self.warn_reject(reason, "nested APC chunk");
            return Err(reason);
        }
        if control.len() > KITTY_APC_MAX_CONTROL_BYTES {
            let reason = KittyApcReject::Oversize;
            self.warn_reject(reason, "control");
            return Err(reason);
        }
        let Some(after_g) = control.strip_prefix(b"G") else {
            // Other APC commands stay inert but count against the stall
            // bound of an open stream (header plus its introducer).
            self.note_interleaved(control.len().saturating_add(1));
            return Err(KittyApcReject::NotGraphics);
        };
        if self.pending.is_some() {
            // Fail closed on a malformed continuation: the open stream is
            // dropped with the chunk, never kept around for a later tail.
            let (more, action) = match continuation_flags(after_g) {
                Ok(Continuation { delete: true, .. }) => {
                    self.abort();
                    let reason = KittyApcReject::Aborted;
                    self.warn_reject(reason, "delete with open stream");
                    return Err(reason);
                }
                Ok(Continuation {
                    more: Some(more),
                    action,
                    ..
                }) => (more, action),
                Ok(Continuation { more: None, .. }) => {
                    self.abort();
                    let reason = KittyApcReject::Aborted;
                    self.warn_reject(reason, "missing m= with open stream");
                    return Err(reason);
                }
                Err(reason) => {
                    self.abort();
                    self.warn_reject(reason, "continuation m=");
                    return Err(reason);
                }
            };
            // The action class must not change mid-stream: frame data
            // (`a=f`) chunks must repeat `a=f` (the specification requires
            // it on every chunk), and image-stream chunks must not smuggle
            // frame data (or any other action) in. Anything else is a
            // confused or hostile client; drop the stream, not just the
            // chunk. A missing `a=` on a frame chunk is accepted leniently:
            // payload bytes are indistinguishable, so stream binding alone
            // decides, and strictness here would only break lenient senders.
            let frame_stream = self.pending.as_ref().is_some_and(|p| p.is_frame);
            let action_ok = match (frame_stream, action) {
                (_, Some('d')) => false,
                (true, Some('f')) | (true, None) => true,
                (true, Some(_)) => false,
                (false, Some('f')) => false,
                (false, _) => action.is_none_or(|a| Some(a) == self.open_action()),
            };
            if !action_ok {
                self.abort();
                let reason = KittyApcReject::Aborted;
                self.warn_reject(reason, "action change with open stream");
                return Err(reason);
            }
            // The continuation header itself is not progress: only decoded
            // payload resets the stall count (see `push_payload`), so a flood
            // of empty `m=1` chunks is bounded too.
            if self.note_interleaved(control.len().saturating_add(1)) {
                return Err(KittyApcReject::Aborted);
            }
            self.current_final = Some(!more);
        } else {
            let params = match parse_control(after_g) {
                Ok(params) => params,
                Err(reason) => {
                    self.warn_reject(reason, "control");
                    return Err(reason);
                }
            };
            let limit = match self.stream_limit(&params) {
                Ok(limit) => limit,
                Err(reason) => {
                    self.warn_reject(reason, "control");
                    return Err(reason);
                }
            };
            self.pending = Some(PendingKitty {
                format_f: params.format_f,
                width_s: params.width_s,
                height_v: params.height_v,
                action_a: params.action_a,
                cols_c: params.cols_c,
                rows_r: params.rows_r,
                cursor_movement_c: params.cursor_movement_c,
                compressed: params.compressed,
                keys: params.keys,
                control_only: !params.carries_data(),
                is_frame: params.action_a == Some('f'),
                encoded_len: 0,
                limit,
                interleaved: 0,
                decoder: Base64Stream::default(),
                payload: Vec::new(),
            });
            // Control-only actions (`a=p`/`d`/`a`/`c`/`q`) and local
            // mediums (`t=f`/`t=t`/`t=s`) always complete single-shot:
            // `m=` chunking is a remote-client (`t=d`) mechanism, and
            // kitty/ghostty both ignore `m=` for local mediums (an `mpv`
            // `t=s` reliance). A stray `m=1` on such a command completes
            // rather than pinning an empty stream.
            let single_shot = !params.carries_data() || !params.keys.medium.is_direct();
            self.current_final = Some(single_shot || !params.more);
        }
        Ok(())
    }

    /// The `a=` action that opened the pending stream, if any.
    fn open_action(&self) -> Option<char> {
        self.pending.as_ref().and_then(|pending| pending.action_a)
    }

    pub(crate) fn push_payload(&mut self, payload: &[u8]) -> Result<(), KittyApcReject> {
        if self.current_final.is_none() {
            return Err(KittyApcReject::Orphan);
        }
        let result = match self.pending.as_mut() {
            Some(pending) => {
                // Control-only actions must arrive bodiless: any byte is
                // garbage or smuggling, never data. Falls into the shared
                // abort-and-warn path below like any other payload error.
                if pending.control_only && !payload.is_empty() {
                    Err(KittyApcReject::UnexpectedPayload)
                } else {
                    if !payload.is_empty() {
                        pending.interleaved = 0;
                    }
                    let available = pending.limit.min(self.budget.payload_limit);
                    pending.push(payload, available, &mut self.budget)
                }
            }
            None => Err(KittyApcReject::Orphan),
        };
        if let Err(reason) = result {
            self.abort();
            self.warn_reject(reason, "payload");
            return Err(reason);
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> KittyFeedOutcome {
        let Some(final_chunk) = self.current_final.take() else {
            return KittyFeedOutcome::Rejected(KittyApcReject::Orphan);
        };
        let Some(mut pending) = self.pending.take() else {
            return KittyFeedOutcome::Rejected(KittyApcReject::Orphan);
        };
        if !final_chunk {
            // A padded quantum may legitimately close the base64 text in an
            // `m=1` chunk when the producer (chafa) terminates the stream with
            // an empty `m=0` chunk. Any further payload byte after the padding
            // still fails closed in `Base64Stream::push`.
            self.pending = Some(pending);
            return KittyFeedOutcome::NeedMore {
                buffered_encoded: self.pending_encoded_len(),
            };
        }
        let available = pending.limit.min(self.budget.payload_limit);
        let result =
            pending
                .decoder
                .finish(&mut pending.payload, available)
                .and_then(|()| {
                    if pending.control_only {
                        // Belt and braces: `push_payload` already refuses
                        // body bytes for control-only actions, so a
                        // non-empty buffer here means an internal error —
                        // still fail closed, never emit.
                        if pending.payload.is_empty() {
                            return Ok(());
                        }
                        return Err(KittyApcReject::UnexpectedPayload);
                    }
                    if !pending.keys.medium.is_direct() {
                        // Local medium: the payload names a file/shm
                        // object. Validate the name against the sandbox
                        // before handing it downstream; the object itself
                        // is opened (TOCTOU-safe, regular-file-checked)
                        // and read under the decode caps there. Never
                        // zlib-decompressed here: `o=z` on a local medium
                        // describes the *stored* bytes, resolved after
                        // the read.
                        validate_kitty_path(pending.keys.medium, &pending.payload)?;
                        if !self.budget.reserve_retained(pending.payload.capacity()) {
                            return Err(KittyApcReject::Oversize);
                        }
                        return validate_raw_claim(
                            pending.format_f,
                            pending.width_s,
                            pending.height_v,
                            self.decode_cap,
                        );
                    }
                    if pending.compressed {
                        let out_cap = raw_claim_bytes(
                            pending.format_f,
                            pending.width_s,
                            pending.height_v,
                            self.decode_cap,
                        )?
                        .unwrap_or(self.decode_cap);
                        let decompressed =
                            match miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(
                                &pending.payload,
                                out_cap,
                            ) {
                                Ok(mut data) => {
                                    data.shrink_to_fit();
                                    data
                                }
                                Err(miniz_oxide::inflate::DecompressError {
                                    status: miniz_oxide::inflate::TINFLStatus::HasMoreOutput,
                                    ..
                                }) => {
                                    return Err(KittyApcReject::Oversize);
                                }
                                Err(_) => return Err(KittyApcReject::BadBase64),
                            };
                        pending.payload = decompressed;
                    }
                    if !self.budget.reserve_retained(pending.payload.capacity()) {
                        return Err(KittyApcReject::Oversize);
                    }
                    validate_raw_claim(
                        pending.format_f,
                        pending.width_s,
                        pending.height_v,
                        self.decode_cap,
                    )
                });
        if let Err(reason) = result {
            self.budget.clear_retained();
            self.warn_reject(reason, "final payload");
            return KittyFeedOutcome::Rejected(reason);
        }
        self.budget.clear_retained();
        KittyFeedOutcome::Completed(KittyCompleted {
            format_f: pending.format_f,
            width_s: pending.width_s,
            height_v: pending.height_v,
            action_a: pending.action_a,
            cols_c: pending.cols_c,
            rows_r: pending.rows_r,
            cursor_movement_c: pending.cursor_movement_c,
            payload: pending.payload.into_boxed_slice(),
            keys: pending.keys,
        })
    }
}

impl Default for KittyApcAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl KittyApcAssembler {
    /// Rate-limited diagnostic warn on fail-closed rejection (no state
    /// change, no paint). `NotGraphics` stays silent.
    fn warn_reject(&mut self, reason: KittyApcReject, context: &str) {
        if reason == KittyApcReject::NotGraphics {
            return;
        }
        if let Some(occurrence) = self.log.record() {
            warn_rejection(
                occurrence,
                &format!("bitty: rejecting kitty APC G ({reason} in {context}): stored nothing"),
            );
        }
    }
}

/// Parses the `G` control section (`key=value` pairs separated by `,`).
///
/// Known keys are strictly validated: any malformed known value rejects the
/// whole transmission fail-closed. Unknown keys (including any multi-letter
/// key) are ignored for future-proofing — a deliberate divergence from
/// kitty, which reports unknown keys as errors: bitty parses the full
/// control block so newer clients keep working, and unknown semantics stay
/// inert downstream.
///
/// Data-carrying actions (absent action, `a=T`/`a=t`, frame data `a=f`)
/// require `f=`; control-only actions (`a=p`/`a=d`/`a=a`/`a=c`/`a=q`) omit
/// it. `i=` and `I=` together are an error (the specification mandates
/// `EINVAL`); `o=` accepts only `z`; `t=` accepts only `d`/`f`/`t`/`s`.
fn parse_control(control: &[u8]) -> Result<KittyApcParams, KittyApcReject> {
    if control.is_empty() {
        return Err(KittyApcReject::MissingFormat);
    }
    let mut format_f: Option<u32> = None;
    let mut width_s: Option<u32> = None;
    let mut height_v: Option<u32> = None;
    let mut action_a: Option<char> = None;
    let mut cols_c: u16 = 0;
    let mut rows_r: u16 = 0;
    let mut cursor_movement_c: u8 = 0;
    let mut more = false;
    let mut compressed = false;
    let mut keys = KittyControlKeys::default();
    for piece in control.split(|&b| b == b',') {
        if piece.is_empty() {
            continue;
        }
        let eq = piece
            .iter()
            .position(|&b| b == b'=')
            .ok_or(KittyApcReject::MalformedControl)?;
        let (key, value) = (&piece[..eq], &piece[eq + 1..]);
        if key.len() != 1 {
            // Multi-letter keys are unknown future extensions: ignore.
            continue;
        }
        match key[0] {
            b'f' => {
                format_f = Some(parse_u32(value).ok_or(KittyApcReject::MalformedControl)?);
            }
            b's' => {
                width_s = Some(parse_u32(value).ok_or(KittyApcReject::MalformedControl)?);
            }
            b'v' => {
                height_v = Some(parse_u32(value).ok_or(KittyApcReject::MalformedControl)?);
            }
            b'a' => {
                action_a = match value {
                    [] => None,
                    [single] => Some(char::from(*single)),
                    _ => return Err(KittyApcReject::BadAction),
                };
            }
            b'c' => {
                cols_c = parse_u16(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'r' => {
                rows_r = parse_u16(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'C' => {
                cursor_movement_c = parse_u8(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'm' => {
                more = match value {
                    b"0" => false,
                    b"1" => true,
                    _ => return Err(KittyApcReject::BadMore),
                };
            }
            b'o' => {
                // Only `o=z` (zlib) exists. An empty value means
                // uncompressed; anything else is a malformed claim, not
                // a future codec to guess at.
                match value {
                    [] => compressed = false,
                    b"z" => compressed = true,
                    _ => return Err(KittyApcReject::MalformedControl),
                }
            }
            b't' => {
                keys.medium = match value {
                    b"d" => KittyMedium::Direct,
                    b"f" => KittyMedium::File,
                    b"t" => KittyMedium::TempFile,
                    b"s" => KittyMedium::SharedMemory,
                    _ => return Err(KittyApcReject::MalformedControl),
                };
            }
            b'i' => {
                keys.image_id = parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'I' => {
                keys.image_number = parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'p' => {
                keys.placement_id = parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'q' => {
                keys.quiet = parse_u8(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'd' => {
                keys.delete = match value {
                    [] => None,
                    [single] => Some(char::from(*single)),
                    _ => return Err(KittyApcReject::MalformedControl),
                };
            }
            b'x' => {
                keys.src_x = parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'y' => {
                keys.src_y = parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'w' => {
                keys.src_w = parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'h' => {
                keys.src_h = parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'X' => {
                keys.cell_x_offset = parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'Y' => {
                keys.cell_y_offset = parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'z' => {
                keys.z_index = parse_i32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'U' => {
                keys.unicode_placement = parse_u8(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'P' => {
                keys.parent_id = parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'Q' => {
                keys.parent_placement_id =
                    parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'H' => {
                keys.parent_dx = parse_i32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'V' => {
                keys.parent_dy = parse_i32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'S' => {
                keys.data_size = parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'O' => {
                keys.data_offset = parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            b'N' => {
                keys.usage_hints = parse_u32(value).ok_or(KittyApcReject::MalformedControl)?;
            }
            _ => {
                // Unknown single-letter keys (`e`, `R`, ...): ignored for
                // transmit/display (future-proof).
            }
        }
    }
    // `i=` and `I=` together are a specification error: fail closed
    // rather than guessing which identity the client meant.
    if keys.image_id != 0 && keys.image_number != 0 {
        return Err(KittyApcReject::MalformedControl);
    }
    let carries_data = matches!(action_a, None | Some('T' | 't' | 'f'));
    let format_f = match (format_f, carries_data) {
        (Some(format), _) => format,
        // Control-only actions omit `f=`; record `0` (absent) so the
        // completed value stays a plain `u32` like the transmit path.
        (None, false) => 0,
        (None, true) => return Err(KittyApcReject::MissingFormat),
    };
    Ok(KittyApcParams {
        format_f,
        width_s,
        height_v,
        action_a,
        cols_c,
        rows_r,
        cursor_movement_c,
        more,
        compressed,
        keys,
    })
}

/// Keys of a continuation chunk that decide its fate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Continuation {
    /// `m=` value (`None` when absent).
    more: Option<bool>,
    /// `a=d`: a delete command, which must abort a partial upload.
    delete: bool,
    /// Explicit `a=` value when present (`None` when absent). Frame
    /// streams require `a=f` here; image streams accept no action change.
    action: Option<char>,
}

/// Scans a continuation control section for `m=`, `a=d`, and an explicit
/// action.
///
/// A continuation should carry only `m` and `q`, except animation frame
/// chunks which must also repeat `a=f`. Anything else is policed by the
/// caller against the open stream's action class.
fn continuation_flags(control: &[u8]) -> Result<Continuation, KittyApcReject> {
    let mut flags = Continuation {
        more: None,
        delete: false,
        action: None,
    };
    for piece in control.split(|&b| b == b',') {
        let Some(eq) = piece.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (key, value) = (&piece[..eq], &piece[eq + 1..]);
        match key {
            // Same rule as `parse_control`: the last `m=` wins and any
            // malformed value rejects the chunk.
            b"m" => {
                flags.more = Some(match value {
                    b"0" => false,
                    b"1" => true,
                    _ => return Err(KittyApcReject::BadMore),
                });
            }
            b"a" if value == b"d" => flags.delete = true,
            b"a" => {
                flags.action = match value {
                    [] => None,
                    [single] => Some(char::from(*single)),
                    _ => return Err(KittyApcReject::BadAction),
                };
            }
            _ => {}
        }
    }
    Ok(flags)
}

/// Sandbox validation for a `t=f`/`t=t`/`t=s` name (decoded payload bytes).
///
/// This is the parse-time half of the sandbox: pure string-shape checks
/// that need no I/O and therefore belong in the headless parser. The
/// open-time half (open-before-validate against TOCTOU, regular-file
/// check, temp-dir containment, actual read under the decode caps) runs
/// where the object is opened downstream, following ghostty's
/// `readFile`/`readSharedMemory` order.
///
/// - Every medium: non-empty, at most [`KITTY_APC_PATH_MAX_BYTES`] bytes
///   (already bounded by the stream limit; re-checked for direct calls),
///   no NUL bytes.
/// - `t=s`: strict POSIX shm shape — starts with `/`, carries no other
///   `/`, and fits [`KITTY_APC_SHM_NAME_MAX`] (ghostty
///   `validSharedMemoryName` parity; the kitty specification mandates
///   the same shape).
/// - `t=t`: must contain [`KITTY_APC_TMP_NAME_MARKER`].
/// - `t=f`/`t=t`: no `..` path component (lexical traversal can never
///   resolve inside an allowed root, so reject it before any open).
///
/// Anything else — including a relative `t=f` path without `..`, which
/// the opener resolves and contains — passes to the open-time checks.
fn validate_kitty_path(medium: KittyMedium, name: &[u8]) -> Result<(), KittyApcReject> {
    if name.is_empty() || name.len() > KITTY_APC_PATH_MAX_BYTES || name.contains(&0) {
        return Err(KittyApcReject::BadPath);
    }
    match medium {
        KittyMedium::Direct => Ok(()),
        KittyMedium::SharedMemory => {
            let valid = name.len() >= 2
                && name.len() <= KITTY_APC_SHM_NAME_MAX
                && name[0] == b'/'
                && !name[1..].contains(&b'/');
            if valid {
                Ok(())
            } else {
                Err(KittyApcReject::BadPath)
            }
        }
        KittyMedium::File | KittyMedium::TempFile => {
            if medium == KittyMedium::TempFile
                && !name
                    .windows(KITTY_APC_TMP_NAME_MARKER.len())
                    .any(|w| w == KITTY_APC_TMP_NAME_MARKER.as_bytes())
            {
                return Err(KittyApcReject::BadPath);
            }
            // Lexical traversal: any `..` component escapes whatever root
            // the opener contains the path to.
            if name.split(|&b| b == b'/').any(|c| c == b"..") {
                return Err(KittyApcReject::BadPath);
            }
            Ok(())
        }
    }
}

/// Rejects oversize raw `s`/`v` claims before any pixel buffer could exist.
///
/// Only raw formats (`f=24` RGB, `f=32` RGBA) with both dimensions present
/// are checked. PNG (`f=100`) ignores `s`/`v` (`IHDR` governs); unknown
/// formats skip the check and let the decoder fail closed. Zero/missing
/// dimensions pass through to the decoder (`ZeroDimension`/`MissingDimensions`).
fn validate_raw_claim(
    format_f: u32,
    width_s: Option<u32>,
    height_v: Option<u32>,
    payload_cap: usize,
) -> Result<(), KittyApcReject> {
    raw_claim_bytes(format_f, width_s, height_v, payload_cap).map(|_| ())
}

/// Exact decoded byte size of a raw `s`/`v` claim, if one is declared.
///
/// `Ok(None)` for PNG/unknown formats and missing or zero dimensions (the
/// decoder fails those closed downstream); `Err(OversizeClaim)` when the
/// claim exceeds the side, area, or `payload_cap` byte bound.
fn raw_claim_bytes(
    format_f: u32,
    width_s: Option<u32>,
    height_v: Option<u32>,
    payload_cap: usize,
) -> Result<Option<usize>, KittyApcReject> {
    let channels: usize = match format_f {
        24 => 3,
        32 => 4,
        _ => return Ok(None),
    };
    let (Some(w), Some(h)) = (width_s, height_v) else {
        return Ok(None);
    };
    if w == 0 || h == 0 {
        return Ok(None);
    }
    if w > KITTY_APC_DECODE_MAX_DIMENSION || h > KITTY_APC_DECODE_MAX_DIMENSION {
        return Err(KittyApcReject::OversizeClaim);
    }
    let pixels = u64::from(w) * u64::from(h);
    if pixels > KITTY_APC_DECODE_MAX_PIXELS {
        return Err(KittyApcReject::OversizeClaim);
    }
    (pixels as usize)
        .checked_mul(channels)
        .filter(|&n| n <= payload_cap)
        .map(Some)
        .ok_or(KittyApcReject::OversizeClaim)
}

/// Strict ASCII decimal `u32` (no sign, no whitespace, no empty).
fn parse_u32(value: &[u8]) -> Option<u32> {
    if value.is_empty() || value.len() > 10 {
        return None;
    }
    let mut acc: u32 = 0;
    for &b in value {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    Some(acc)
}

/// Strict ASCII decimal `u16` (no sign, no whitespace, no empty).
fn parse_u16(value: &[u8]) -> Option<u16> {
    if value.is_empty() || value.len() > 5 {
        return None;
    }
    let mut acc: u16 = 0;
    for &b in value {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(u16::from(b - b'0'))?;
    }
    Some(acc)
}

fn parse_u8(value: &[u8]) -> Option<u8> {
    if value.is_empty() || value.len() > 3 {
        return None;
    }
    let mut acc: u8 = 0;
    for &b in value {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(b - b'0')?;
    }
    Some(acc)
}

/// Strict ASCII decimal `i32` (optional leading `-`, no `+`, no
/// whitespace, no empty). Used for the signed wire keys `z`, `H`, `V`.
fn parse_i32(value: &[u8]) -> Option<i32> {
    let (negative, digits) = match value.strip_prefix(b"-") {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    if digits.is_empty() || digits.len() > 10 {
        return None;
    }
    // `i32::MIN` has no positive mirror: accept its exact digits.
    if negative && digits == b"2147483648" {
        return Some(i32::MIN);
    }
    let mut acc: i32 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(i32::from(b - b'0'))?;
    }
    Some(if negative { acc.checked_neg()? } else { acc })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed(raw: &[u8]) -> KittyCompleted {
        let mut assembler = KittyApcAssembler::new();
        match assembler.feed(raw) {
            KittyFeedOutcome::Completed(done) => done,
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn control_parses_routing_fields() {
        // 2x2 RGBA red (16 bytes) -> 24-char base64.
        let raw = b"Gf=32,s=2,v=2,a=T,c=2,r=2,m=0;/wAA//8AAP//AAD//wAA/w==";
        let done = completed(raw);
        assert_eq!(done.format_f, 32);
        assert_eq!(done.width_s, Some(2));
        assert_eq!(done.height_v, Some(2));
        assert_eq!(done.action_a, Some('T'));
        assert_eq!(done.cols_c, 2);
        assert_eq!(done.rows_r, 2);
        assert_eq!(done.payload.len(), 16);
        assert_eq!(&*done.payload, &[0xFF, 0, 0, 0xFF].repeat(4));
    }

    #[test]
    fn absent_action_and_spans_default() {
        let done = completed(b"Gf=100,m=0;aGk=");
        assert_eq!(done.format_f, 100);
        assert_eq!(done.action_a, None);
        assert_eq!(done.cols_c, 0);
        assert_eq!(done.rows_r, 0);
        assert_eq!(&*done.payload, b"hi");
    }

    #[test]
    fn unknown_keys_ignored() {
        let done = completed(b"Gf=32,s=1,v=1,i=7,p=1,q=2,X=0,Y=0,z=5,m=0;/wAA/w==");
        assert_eq!(done.format_f, 32);
        assert_eq!(&*done.payload, &[0xFF, 0, 0, 0xFF]);
    }

    #[test]
    fn non_g_is_inert_not_graphics() {
        let mut assembler = KittyApcAssembler::new();
        for raw in [b"Thello".as_slice(), b"".as_slice(), b" f=32".as_slice()] {
            match assembler.feed(raw) {
                KittyFeedOutcome::Rejected(KittyApcReject::NotGraphics) => {}
                other => panic!("expected NotGraphics for {raw:?}, got {other:?}"),
            }
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn malformed_control_rejected() {
        let mut assembler = KittyApcAssembler::new();
        for raw in [
            b"Gf".as_slice(),
            b"Gf=abc".as_slice(),
            b"Gs=2".as_slice(),
            b"Gf=32,a=TT".as_slice(),
            b"Gf=32,m=2".as_slice(),
            b"G".as_slice(),
        ] {
            match assembler.feed(raw) {
                KittyFeedOutcome::Rejected(_) => {}
                other => panic!("expected Rejected for {raw:?}, got {other:?}"),
            }
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn bad_base64_rejected() {
        let mut assembler = KittyApcAssembler::new();
        // `!` is outside the standard alphabet; `=` mid-body is misplaced.
        for raw in [
            b"Gf=32,s=1,v=1,m=0;!!!!".as_slice(),
            b"Gf=32,s=1,v=1,m=0;/w=A/w==".as_slice(),
            b"Gf=32,s=1,v=1,m=0;abcde".as_slice(),
        ] {
            match assembler.feed(raw) {
                KittyFeedOutcome::Rejected(KittyApcReject::BadBase64) => {}
                other => panic!("expected BadBase64 for {raw:?}, got {other:?}"),
            }
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn oversize_claim_rejected_before_emit() {
        let mut assembler = KittyApcAssembler::new();
        // 9000px side exceeds 8192; tiny payload proves no large alloc.
        match assembler.feed(b"Gf=32,s=9000,v=1,m=0;AA==") {
            KittyFeedOutcome::Rejected(KittyApcReject::OversizeClaim) => {}
            other => panic!("expected OversizeClaim, got {other:?}"),
        }
        // 5000x5000 area exceeds 4096^2.
        match assembler.feed(b"Gf=32,s=5000,v=5000,m=0;AA==") {
            KittyFeedOutcome::Rejected(KittyApcReject::OversizeClaim) => {}
            other => panic!("expected OversizeClaim, got {other:?}"),
        }
        // PNG ignores s/v: same claim passes the claim gate (decodes empty
        // PNG file bytes, which are not a valid PNG, but the claim itself
        // must not reject; the decoder fails closed downstream).
        match assembler.feed(b"Gf=100,s=9000,v=9000,m=0;AA==") {
            KittyFeedOutcome::Completed(_) | KittyFeedOutcome::Rejected(_) => {}
            KittyFeedOutcome::NeedMore { .. } => panic!("unexpected NeedMore"),
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn raw_claim_over_decode_caps_is_rejected_on_first_chunk() {
        let mut assembler = KittyApcAssembler::new();
        // 4096 x 4097 RGBA exceeds the IMG-2 pixel area: refused before any
        // payload byte is buffered, even on an `m=1` opener.
        assert!(matches!(
            assembler.feed(b"Gf=32,s=4096,v=4097,m=1;AAAA"),
            KittyFeedOutcome::Rejected(KittyApcReject::OversizeClaim)
        ));
        assert!(!assembler.has_pending());
        assert_eq!(assembler.peak_memory(), 0);
        // A custom smaller decode cap bounds raw claims too.
        let mut assembler = KittyApcAssembler::with_caps(4096, 15);
        assert!(matches!(
            assembler.feed(b"Gf=32,s=2,v=2,m=1;AAAA"),
            KittyFeedOutcome::Rejected(KittyApcReject::OversizeClaim)
        ));
        assert!(!assembler.has_pending());
    }

    #[test]
    fn raw_claim_bounds_stream_to_declared_size() {
        // f=24, 2x1 declares exactly 6 bytes; 9 decoded bytes overrun the
        // claim and drop the stream even though IMG-1 would admit them.
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=24,s=2,v=1,m=1;AAAAAAAA"),
            KittyFeedOutcome::NeedMore { .. }
        ));
        assert!(matches!(
            assembler.feed(b"Gm=0;AAAA"),
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize)
        ));
        assert!(!assembler.has_pending());
        assert!(assembler.peak_memory() <= 6);
    }

    #[test]
    fn compressed_payload_keeps_img1_cap() {
        // PNG (f=100) and raw streams without s/v stay bound by IMG-1.
        let encoded = base64_encode(&vec![0x5a; KITTY_APC_LEDGER_CAP + 1]);
        for header in [b"Gf=100,m=0;".as_slice(), b"Gf=32,m=0;".as_slice()] {
            let mut raw = header.to_vec();
            raw.extend_from_slice(encoded.as_bytes());
            let mut assembler = KittyApcAssembler::new();
            assert!(matches!(
                assembler.feed(&raw),
                KittyFeedOutcome::Rejected(KittyApcReject::Oversize)
            ));
            assert!(assembler.peak_memory() <= KITTY_APC_LEDGER_CAP);
        }
    }

    #[test]
    fn compressed_raw_claim_keeps_img1_cap() {
        // `o=z` marks a compressed payload: a raw s/v claim must not lift
        // the stream bound above IMG-1.
        let (w, h) = (1024_u32, 1024_u32);
        let encoded = base64_encode(&vec![0x5a; KITTY_APC_LEDGER_CAP + 1]);
        let mut raw = format!("Gf=32,o=z,s={w},v={h},m=0;").into_bytes();
        raw.extend_from_slice(encoded.as_bytes());
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(&raw),
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize)
        ));
        assert!(assembler.peak_memory() <= KITTY_APC_LEDGER_CAP);
    }

    #[test]
    fn zlib_compressed_payload_decompresses_successfully() {
        // Fastfetch sends zlib-compressed raw RGBA with o=z (CTX-0945 / #1656).
        let raw_rgba = [0xFF, 0, 0, 0xFF].repeat(4); // 2x2 red
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&raw_rgba, 6);
        let encoded = base64_encode(&compressed);
        let raw = format!("Gf=32,s=2,v=2,o=z,m=0;{encoded}").into_bytes();
        let mut assembler = KittyApcAssembler::new();
        match assembler.feed(&raw) {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(done.format_f, 32);
                assert_eq!(done.width_s, Some(2));
                assert_eq!(done.height_v, Some(2));
                assert_eq!(&*done.payload, &raw_rgba);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn zlib_chunked_payload_decompresses_successfully() {
        let raw_rgba = [0x00, 0xFF, 0, 0xFF].repeat(8); // 4x2 green
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&raw_rgba, 6);
        let encoded = base64_encode(&compressed);
        let mid = encoded.len() / 2;
        let (first, second) = encoded.split_at(mid);

        let chunk1 = format!("Gf=32,s=4,v=2,o=z,m=1;{first}").into_bytes();
        let chunk2 = format!("Gm=0;{second}").into_bytes();

        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(&chunk1),
            KittyFeedOutcome::NeedMore { .. }
        ));
        match assembler.feed(&chunk2) {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(done.format_f, 32);
                assert_eq!(&*done.payload, &raw_rgba);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn hd_raw_rgba_claim_beyond_img1_completes() {
        // chafa -f kitty emits raw RGBA sized to the terminal: a 2850x1600
        // frame is ~18 MiB, far over IMG-1 but inside IMG-2/IMG-3.
        let (w, h) = (2850_u32, 1600_u32);
        let len = (w * h * 4) as usize;
        assert!(len > KITTY_APC_LEDGER_CAP);
        let encoded = base64_encode(&vec![0x7f; len]);
        let mut assembler = KittyApcAssembler::new();
        let opener = format!("Ga=T,f=32,s={w},v={h},c=285,r=80,m=1,q=2;");
        assert!(matches!(
            assembler.feed(opener.as_bytes()),
            KittyFeedOutcome::NeedMore { .. }
        ));
        // chafa chunk size: 4096 base64 chars per `m=1` chunk.
        for chunk in encoded.as_bytes().chunks(4096) {
            let mut raw = b"Gm=1;".to_vec();
            raw.extend_from_slice(chunk);
            assert!(matches!(
                assembler.feed(&raw),
                KittyFeedOutcome::NeedMore { .. }
            ));
        }
        match assembler.feed(b"Gm=0;") {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(done.payload.len(), len);
                assert_eq!((done.width_s, done.height_v), (Some(w), Some(h)));
            }
            other => panic!("expected HD completion, got {other:?}"),
        }
        assert!(assembler.peak_memory() <= KITTY_APC_DECODE_MAX_BYTES);
    }

    #[test]
    fn padded_non_final_chunk_then_empty_terminator_completes() {
        // chafa ends the base64 text with `==` inside an `m=1` chunk, then
        // closes the stream with an empty `m=0` chunk.
        let raw_payload = [0x11, 0x22, 0x33, 0x44];
        let encoded = base64_encode(&raw_payload);
        assert!(encoded.ends_with("=="));
        let mut assembler = KittyApcAssembler::new();
        let opener = format!("Gf=32,s=1,v=1,m=1;{}", &encoded[..4]);
        assert!(matches!(
            assembler.feed(opener.as_bytes()),
            KittyFeedOutcome::NeedMore { .. }
        ));
        let padded = format!("Gm=1;{}", &encoded[4..]);
        assert!(matches!(
            assembler.feed(padded.as_bytes()),
            KittyFeedOutcome::NeedMore { .. }
        ));
        match assembler.feed(b"Gm=0;") {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(&*done.payload, &raw_payload[..]);
            }
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore = "64 MiB test too slow on Windows CI")]
    fn raw_rgba_at_exact_img3_boundary_completes() {
        // 4096x4096 RGBA is exactly 64 MiB (IMG-3 KITTY_APC_DECODE_MAX_BYTES).
        // This must complete without rejection.
        let (w, h) = (4096_u32, 4096_u32);
        let len = (w * h * 4) as usize;
        assert_eq!(len, KITTY_APC_DECODE_MAX_BYTES);
        let encoded = base64_encode(&vec![0x42; len]);
        let mut assembler = KittyApcAssembler::new();
        let opener = format!("Ga=T,f=32,s={w},v={h},m=1;");
        assert!(matches!(
            assembler.feed(opener.as_bytes()),
            KittyFeedOutcome::NeedMore { .. }
        ));
        // Feed in 4096-byte chunks (chafa style).
        for chunk in encoded.as_bytes().chunks(4096) {
            let mut raw = b"Gm=1;".to_vec();
            raw.extend_from_slice(chunk);
            assert!(matches!(
                assembler.feed(&raw),
                KittyFeedOutcome::NeedMore { .. }
            ));
        }
        match assembler.feed(b"Gm=0;") {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(done.payload.len(), len);
                assert_eq!((done.width_s, done.height_v), (Some(w), Some(h)));
                assert!(done.payload.iter().all(|&b| b == 0x42));
            }
            other => panic!("expected completion at exact IMG-3 boundary, got {other:?}"),
        }
        assert_eq!(assembler.peak_memory(), KITTY_APC_DECODE_MAX_BYTES);
    }

    #[test]
    fn payload_after_padding_still_fails_closed() {
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=100,m=1;aGk="),
            KittyFeedOutcome::NeedMore { .. }
        ));
        assert!(matches!(
            assembler.feed(b"Gm=0;aGk="),
            KittyFeedOutcome::Rejected(KittyApcReject::BadBase64)
        ));
        assert!(!assembler.has_pending());
        assert!(matches!(
            assembler.feed(b"Gf=100,m=1;aGk="),
            KittyFeedOutcome::NeedMore { .. }
        ));
        assert!(matches!(
            assembler.feed(b"Gm=1;AAAA"),
            KittyFeedOutcome::Rejected(KittyApcReject::BadBase64)
        ));
        assert!(!assembler.has_pending());
    }

    #[test]
    fn chunked_reassembly_is_exact() {
        let mut assembler = KittyApcAssembler::new();
        // Split the 24-char red_2x2 base64 across three APCs.
        let full = b"/wAA//8AAP//AAD//wAA/w==";
        match assembler.feed(b"Gf=32,s=2,v=2,m=1;/wAA//8A") {
            KittyFeedOutcome::NeedMore {
                buffered_encoded: 8,
            } => {}
            other => panic!("expected NeedMore(8), got {other:?}"),
        }
        assert!(assembler.has_pending());
        match assembler.feed(b"Gm=1;AP//AAD/") {
            KittyFeedOutcome::NeedMore {
                buffered_encoded: 16,
            } => {}
            other => panic!("expected NeedMore(16), got {other:?}"),
        }
        match assembler.feed(b"Gm=0;/wAA/w==") {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(done.format_f, 32);
                assert_eq!(done.width_s, Some(2));
                assert_eq!(done.height_v, Some(2));
                assert_eq!(&*done.payload, &[0xFF, 0, 0, 0xFF].repeat(4));
                let _ = full;
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn chunked_empty_edge_chunks_assemble_chafa_shape() {
        // Real `chafa --format kitty` shape: params-only first `m=1` (empty
        // payload) and params-only final `m=0` (no `;`), data in middles.
        let mut assembler = KittyApcAssembler::new();
        match assembler.feed(b"Gf=32,s=2,v=2,a=T,c=2,r=2,m=1,q=2") {
            KittyFeedOutcome::NeedMore {
                buffered_encoded: 0,
            } => {}
            other => panic!("expected NeedMore(0), got {other:?}"),
        }
        match assembler.feed(b"Gm=1;/wAA//8AAP//AAD/") {
            KittyFeedOutcome::NeedMore {
                buffered_encoded: 16,
            } => {}
            other => panic!("expected NeedMore(16), got {other:?}"),
        }
        match assembler.feed(b"Gm=0") {
            KittyFeedOutcome::Completed(done) => {
                // `q=2` ignored; first-chunk params authoritative.
                assert_eq!(done.format_f, 32);
                assert_eq!(done.action_a, Some('T'));
                assert_eq!(done.cols_c, 2);
                assert_eq!(done.rows_r, 2);
                // 16 encoded chars -> 12 decoded bytes (3 red pixels); the
                // tail `/wAA/w==` (4B) is intentionally absent here to prove
                // empty-final assembly is exact for what was sent.
                assert_eq!(&*done.payload, &[0xFF, 0, 0, 0xFF].repeat(3));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn chunked_oversize_drops_stream_and_rejects() {
        let mut assembler = KittyApcAssembler::with_ledger_cap(16);
        match assembler.feed(b"Gf=32,s=2,v=2,m=1;/wAA//8A") {
            KittyFeedOutcome::NeedMore { .. } => {}
            other => panic!("expected NeedMore, got {other:?}"),
        }
        // 8 + 16 > 16: drops the stream, stores nothing.
        match assembler.feed(b"Gm=1;AAAAAAAAAAAAAAAA") {
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize) => {}
            other => panic!("expected Oversize, got {other:?}"),
        }
        assert!(!assembler.has_pending());
        // Reusable afterwards: lone single-shot fits.
        match assembler.feed(b"Gf=32,s=1,v=1,m=0;/wAA/w==") {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(&*done.payload, &[0xFF, 0, 0, 0xFF]);
            }
            other => panic!("expected Completed after drop, got {other:?}"),
        }
    }

    #[test]
    fn decode_cap_default_mirrors_rich_decode() {
        assert_eq!(
            KittyApcAssembler::new().decode_cap(),
            KITTY_APC_DECODE_MAX_BYTES
        );
    }

    #[test]
    fn single_shot_decode_is_capped_before_decode() {
        // A single packet may not bypass the decode cap via the larger
        // ledger: the decoded size bound applies to lone transmissions too.
        let mut assembler = KittyApcAssembler::with_caps(4096, 9);
        // 16 base64 chars decode to 12 bytes > cap 9: rejected.
        match assembler.feed(b"Gf=24,m=0;AAAAAAAAAAAAAAAA") {
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize) => {}
            other => panic!("expected Oversize, got {other:?}"),
        }
        // Encoded length beyond what the decode cap can yield is refused
        // before any decode allocation (20 chars > 16 allowed for cap 9).
        match assembler.feed(b"Gf=24,m=0;AAAAAAAAAAAAAAAAAAAA") {
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize) => {}
            other => panic!("expected Oversize, got {other:?}"),
        }
        // Exactly at the cap still completes (12 chars -> 9 bytes).
        match assembler.feed(b"Gf=24,m=0;AAAAAAAAAAAA") {
            KittyFeedOutcome::Completed(done) => assert_eq!(done.payload.len(), 9),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn chunked_assembly_respects_decode_cap() {
        let mut assembler = KittyApcAssembler::with_caps(4096, 9);
        match assembler.feed(b"Gf=24,s=3,v=1,m=1;AAAAAAAA") {
            KittyFeedOutcome::NeedMore { .. } => {}
            other => panic!("expected NeedMore, got {other:?}"),
        }
        // 8 + 8 encoded chars decode to 12 bytes > cap 9: drop, warn, no emit.
        match assembler.feed(b"Gm=0;AAAAAAAA") {
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize) => {}
            other => panic!("expected Oversize, got {other:?}"),
        }
        assert!(!assembler.has_pending());
        // Reusable afterwards: a capped lone single-shot fits (1x3 RGB
        // declares exactly the 9 capped bytes).
        match assembler.feed(b"Gf=24,s=1,v=3,m=0;AAAAAAAAAAAA") {
            KittyFeedOutcome::Completed(done) => assert_eq!(done.payload.len(), 9),
            other => panic!("expected Completed after drop, got {other:?}"),
        }
    }

    #[test]
    fn missing_m_with_open_stream_aborts_stream() {
        let mut assembler = KittyApcAssembler::new();
        match assembler.feed(b"Gf=32,s=2,v=2,m=1;/wAA//8A") {
            KittyFeedOutcome::NeedMore { .. } => {}
            other => panic!("expected NeedMore, got {other:?}"),
        }
        // No `m=` with an open stream: protocol violation, fail closed by
        // dropping the open stream together with the newcomer.
        match assembler.feed(b"Gf=32,s=1,v=1;/wAA/w==") {
            KittyFeedOutcome::Rejected(KittyApcReject::Aborted) => {}
            other => panic!("expected Aborted, got {other:?}"),
        }
        assert!(!assembler.has_pending());
        assert_eq!(assembler.budget.decoded, 0);
        // The old tail is now an orphan and paints nothing.
        // (A bare `m=0` without `f=` and no open stream is a missing-format
        // transmission.)
        match assembler.feed(b"Gm=0;AP//AAD//wAA/w==") {
            KittyFeedOutcome::Rejected(KittyApcReject::MissingFormat) => {}
            other => panic!("expected MissingFormat, got {other:?}"),
        }
        // A fresh stream still completes afterwards.
        match assembler.feed(b"Gf=32,s=1,v=1;/wAA/w==") {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(&*done.payload, &[0xFF, 0, 0, 0xFF]);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn malformed_continuation_m_aborts_stream() {
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=32,s=2,v=2,m=1;/wAA//8A"),
            KittyFeedOutcome::NeedMore { .. }
        ));
        match assembler.feed(b"Gm=2;AP//AAD/") {
            KittyFeedOutcome::Rejected(KittyApcReject::BadMore) => {}
            other => panic!("expected BadMore, got {other:?}"),
        }
        assert!(!assembler.has_pending());
        assert_eq!(assembler.budget.decoded, 0);
        assert!(matches!(
            assembler.feed(b"Gm=0;/wAA/w=="),
            KittyFeedOutcome::Rejected(KittyApcReject::MissingFormat)
        ));
    }

    #[test]
    fn delete_with_open_stream_aborts_partial_upload() {
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=32,s=2,v=2,m=1;/wAA//8A"),
            KittyFeedOutcome::NeedMore { .. }
        ));
        // Kitty spec: a delete received mid-upload aborts the partial upload.
        match assembler.feed(b"Ga=d,d=A,m=0") {
            KittyFeedOutcome::Rejected(KittyApcReject::Aborted) => {}
            other => panic!("expected Aborted, got {other:?}"),
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn interleaved_bytes_past_stall_bound_drop_stream() {
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=32,s=2,v=2,m=1;/wAA//8A"),
            KittyFeedOutcome::NeedMore { .. }
        ));
        // Close to the bound keeps the stream, with room for one header.
        assert!(!assembler.note_interleaved(KITTY_APC_STALL_MAX_BYTES - 16));
        assert!(assembler.has_pending());
        // Continuation payload resets the count (the 5-byte `Gm=1` header
        // is charged first, then cleared by the payload).
        assert!(matches!(
            assembler.feed(b"Gm=1;AP//AAD/"),
            KittyFeedOutcome::NeedMore { .. }
        ));
        assert!(!assembler.note_interleaved(KITTY_APC_STALL_MAX_BYTES));
        assert!(assembler.has_pending());
        // One byte past the bound drops it and releases the payload charge.
        assert!(assembler.note_interleaved(1));
        assert!(!assembler.has_pending());
        assert_eq!(assembler.budget.decoded, 0);
        // Idle assembler: charging is a no-op.
        assert!(!assembler.note_interleaved(usize::MAX));
    }

    #[test]
    fn empty_continuation_flood_hits_stall_bound() {
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=32,s=2,v=2,m=1;/wAA//8A"),
            KittyFeedOutcome::NeedMore { .. }
        ));
        // Each empty `Gm=1` charges its header and never resets the count.
        let rounds = KITTY_APC_STALL_MAX_BYTES / b"Gm=1".len() + 1;
        let mut aborted = false;
        for _ in 0..rounds {
            if let KittyFeedOutcome::Rejected(KittyApcReject::Aborted) = assembler.feed(b"Gm=1") {
                aborted = true;
                break;
            }
        }
        assert!(aborted, "empty continuation flood must hit the stall bound");
        assert!(!assembler.has_pending());
        assert_eq!(assembler.budget.decoded, 0);
    }

    #[test]
    fn duplicate_continuation_m_last_wins_and_malformed_rejects() {
        assert_eq!(
            continuation_flags(b"m=1,m=0"),
            Ok(Continuation {
                more: Some(false),
                delete: false,
                action: None,
            })
        );
        assert_eq!(continuation_flags(b"m=0,m=x"), Err(KittyApcReject::BadMore));
    }

    #[test]
    fn non_graphics_apc_counts_toward_stall_bound() {
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=32,s=2,v=2,m=1;/wAA//8A"),
            KittyFeedOutcome::NeedMore { .. }
        ));
        let other = vec![b'X'; KITTY_APC_MAX_CONTROL_BYTES - 1];
        let rounds = KITTY_APC_STALL_MAX_BYTES / KITTY_APC_MAX_CONTROL_BYTES + 1;
        for _ in 0..rounds {
            assert!(matches!(
                assembler.feed(&other),
                KittyFeedOutcome::Rejected(KittyApcReject::NotGraphics)
            ));
        }
        assert!(!assembler.has_pending());
    }

    fn base64_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
        for chunk in bytes.chunks(3) {
            let first = chunk[0];
            let second = chunk.get(1).copied().unwrap_or(0);
            let third = chunk.get(2).copied().unwrap_or(0);
            out.push(ALPHABET[(first >> 2) as usize] as char);
            out.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(third & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    #[test]
    fn img1_payload_below_at_and_above_boundary_is_bounded() {
        let cap = KITTY_APC_LEDGER_CAP;
        for (len, should_complete) in [(cap - 1, true), (cap, true), (cap + 1, false)] {
            let payload = vec![0x5a; len];
            let encoded = base64_encode(&payload);
            let mut raw = Vec::with_capacity(encoded.len() + 16);
            raw.extend_from_slice(b"Gf=100,m=0;");
            raw.extend_from_slice(encoded.as_bytes());
            let mut assembler = KittyApcAssembler::new();
            match assembler.feed(&raw) {
                KittyFeedOutcome::Completed(done) if should_complete => {
                    assert_eq!(done.payload.len(), len);
                }
                KittyFeedOutcome::Rejected(KittyApcReject::Oversize) if !should_complete => {}
                other => panic!("unexpected boundary outcome for {len}: {other:?}"),
            }
            assert!(assembler.peak_memory() <= cap);
            assert!(
                assembler.peak_total_memory()
                    <= cap + KITTY_APC_MAX_CONTROL_BYTES + KITTY_APC_CODEC_SCRATCH_BYTES
            );
        }
    }

    #[test]
    fn small_cap_continuation_is_exact_and_over_budget_is_rejected() {
        let cap = 9;
        let encoded = base64_encode(&vec![0x33; cap]);
        let split = encoded.len() - 4;
        let mut assembler = KittyApcAssembler::with_caps(4096, cap);
        let mut first = b"Gf=100,m=1;".to_vec();
        first.extend_from_slice(&encoded.as_bytes()[..split]);
        match assembler.feed(&first) {
            KittyFeedOutcome::NeedMore { .. } => {}
            other => panic!("expected NeedMore, got {other:?}"),
        }
        let mut final_chunk = b"Gm=0;".to_vec();
        final_chunk.extend_from_slice(&encoded.as_bytes()[split..]);
        match assembler.feed(&final_chunk) {
            KittyFeedOutcome::Completed(done) => assert_eq!(done.payload.len(), cap),
            other => panic!("expected completion, got {other:?}"),
        }
        assert!(assembler.peak_memory() <= cap);

        let first = base64_encode(&[0x33; 6]);
        let tail = base64_encode(&[0x33; 4]);
        let mut assembler = KittyApcAssembler::with_caps(4096, cap);
        let mut first_chunk = b"Gf=100,m=1;".to_vec();
        first_chunk.extend_from_slice(first.as_bytes());
        assert!(matches!(
            assembler.feed(&first_chunk),
            KittyFeedOutcome::NeedMore { .. }
        ));
        let mut final_chunk = b"Gm=0;".to_vec();
        final_chunk.extend_from_slice(tail.as_bytes());
        assert!(matches!(
            assembler.feed(&final_chunk),
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize)
        ));
        assert!(!assembler.has_pending());
        assert!(assembler.peak_memory() <= cap);
    }

    #[test]
    fn first_chunk_over_budget_is_rejected_before_decode() {
        let cap = 9;
        let encoded = base64_encode(&vec![0x33; cap + 1]);
        let mut raw = b"Gf=100,m=1;".to_vec();
        raw.extend_from_slice(encoded.as_bytes());
        let mut assembler = KittyApcAssembler::with_caps(4096, cap);
        assert!(matches!(
            assembler.feed(&raw),
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize)
        ));
        assert!(!assembler.has_pending());
        assert!(assembler.peak_memory() <= cap);
    }

    #[test]
    fn caps_mirror_canonical_crates() {
        assert_eq!(KITTY_APC_LEDGER_CAP, 4 * 1024 * 1024);
        assert_eq!(KITTY_APC_DECODE_MAX_DIMENSION, 8192);
        assert_eq!(KITTY_APC_DECODE_MAX_PIXELS, 4096 * 4096);
        assert_eq!(KITTY_APC_DECODE_MAX_BYTES, 64 * 1024 * 1024);
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore = "64 MiB test too slow on Windows CI")]
    fn raw_rgba_at_exact_decode_cap_completes() {
        // CTX-0904: 4096x4096 RGBA is exactly 64 MiB (IMG-2/IMG-3 cap).
        // The stream must complete without rejection.
        let w = 4096_u32;
        let h = 4096_u32;
        let bytes = (w as usize) * (h as usize) * 4; // 67108864 = 64 MiB
        assert_eq!(bytes, KITTY_APC_DECODE_MAX_BYTES);

        let payload = vec![0xAA_u8; bytes];
        let encoded = base64_encode(&payload);
        let chunk_size = 4096;
        let mut assembler = KittyApcAssembler::new();

        // Feed opener with s/v
        let opener = format!("Gf=32,s={w},v={h},m=1;");
        assert!(matches!(
            assembler.feed(opener.as_bytes()),
            KittyFeedOutcome::NeedMore { .. }
        ));

        // Feed payload in 4096-char chunks
        let encoded_bytes = encoded.as_bytes();
        for chunk in encoded_bytes.chunks(chunk_size) {
            let mut frame = b"Gm=1;".to_vec();
            frame.extend_from_slice(chunk);
            match assembler.feed(&frame) {
                KittyFeedOutcome::NeedMore { .. } => {}
                other => panic!("expected NeedMore mid-stream, got {other:?}"),
            }
        }

        // Terminator
        match assembler.feed(b"Gm=0;") {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(done.format_f, 32);
                assert_eq!(done.width_s, Some(w));
                assert_eq!(done.height_v, Some(h));
                assert_eq!(done.payload.len(), bytes);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn raw_rgba_one_pixel_over_decode_cap_is_rejected() {
        // CTX-0904: 4096x4096 RGBA + 1 pixel exceeds the cap and must be
        // rejected on the first chunk (before buffering the payload).
        let w = 4096_u32;
        let h = 4096_u32;
        let bytes = (w as usize) * (h as usize) * 4 + 4; // 64 MiB + 1 pixel
        assert!(bytes > KITTY_APC_DECODE_MAX_BYTES);

        let opener = format!("Gf=32,s={w},v={},m=1;", h + 1);
        let mut assembler = KittyApcAssembler::new();

        // First chunk with oversized claim is rejected immediately
        assert!(matches!(
            assembler.feed(opener.as_bytes()),
            KittyFeedOutcome::Rejected(KittyApcReject::OversizeClaim)
        ));
        assert!(!assembler.has_pending());
    }

    // -- Advanced subset (CTX-0950): placement, animation, local mediums.

    fn path_command(header: &str, path: &[u8]) -> Vec<u8> {
        let mut raw = header.as_bytes().to_vec();
        raw.extend_from_slice(base64_encode(path).as_bytes());
        raw
    }

    #[test]
    fn place_action_completes_without_format() {
        // `a=p` displays a previously transmitted image: no `f=`, no
        // payload, just ids and spans.
        let done = completed(b"Ga=p,i=42,p=7,c=20,r=10;");
        assert_eq!(done.format_f, 0);
        assert_eq!(done.action_a, Some('p'));
        assert_eq!(done.keys.image_id, 42);
        assert_eq!(done.keys.placement_id, 7);
        assert_eq!((done.cols_c, done.rows_r), (20, 10));
        assert_eq!(done.keys.medium, KittyMedium::Direct);
        assert!(done.payload.is_empty());
    }

    #[test]
    fn virtual_placement_flag_and_negative_z_index() {
        // yazi-style virtual placement: `U=1` prototype with a negative
        // z-index (under text).
        let done = completed(b"Ga=p,U=1,i=9,c=4,r=2,z=-5;");
        assert!(done.keys.is_virtual_placement());
        assert_eq!(done.keys.z_index, -5);
        // Below `INT32_MIN/2` draws under non-default cell backgrounds:
        // the extreme value must still parse.
        let done = completed(b"Ga=p,U=1,i=9,c=4,r=2,z=-1073741825;");
        assert_eq!(done.keys.z_index, -1_073_741_825);
        let done = completed(b"Ga=p,i=9,z=-2147483648;");
        assert_eq!(done.keys.z_index, i32::MIN);
    }

    #[test]
    fn signed_keys_reject_malformed_values() {
        for raw in [
            b"Ga=p,i=1,z=;".as_slice(),
            b"Ga=p,i=1,z=--1;".as_slice(),
            b"Ga=p,i=1,z=abc;".as_slice(),
            b"Ga=p,i=1,H=+2;".as_slice(),
            b"Ga=p,i=1,V=1.5;".as_slice(),
            b"Ga=p,i=1,z=2147483648;".as_slice(),
        ] {
            let mut assembler = KittyApcAssembler::new();
            assert!(
                matches!(
                    assembler.feed(raw),
                    KittyFeedOutcome::Rejected(KittyApcReject::MalformedControl)
                ),
                "expected MalformedControl for {raw:?}"
            );
            assert!(!assembler.has_pending());
        }
    }

    #[test]
    fn control_actions_complete_bodiless() {
        // Delete, animation control, compose, and query carry keys only.
        // `f=` is meaningless for them and must be omittable.
        let done = completed(b"Ga=d;");
        assert_eq!((done.format_f, done.action_a), (0, Some('d')));
        let done = completed(b"Ga=d,d=i,i=10,p=7;");
        assert_eq!(done.keys.delete, Some('i'));
        assert_eq!(done.keys.image_id, 10);
        assert_eq!(done.keys.placement_id, 7);
        let done = completed(b"Ga=d,d=Z,z=-1;");
        assert_eq!(done.keys.delete, Some('Z'));
        assert_eq!(done.keys.z_index, -1);
        let done = completed(b"Ga=a,i=7,r=3,z=48;");
        assert_eq!(done.frame_r(), 3);
        assert_eq!(done.frame_gap_ms(), 48);
        let done = completed(b"Ga=c,i=1,r=7,c=9,w=23,h=27,X=4,Y=8,x=1,y=3;");
        assert_eq!(done.keys.image_id, 1);
        assert_eq!((done.frame_r(), done.frame_c()), (7, 9));
        assert_eq!((done.keys.src_w, done.keys.src_h), (23, 27));
        assert_eq!(done.compose_mode(), 4);
        assert_eq!((done.keys.src_x, done.keys.src_y), (1, 3));
        let done = completed(b"Ga=q,i=5;");
        assert_eq!(done.action_a, Some('q'));
        assert!(done.payload.is_empty());
    }

    #[test]
    fn animation_control_state_and_loops() {
        // The overloaded `s=`/`v=` keys carry state and loop count for
        // `a=a` (ghostty `Animation.State`, kitty `s=`/`v=` semantics).
        let done = completed(b"Ga=a,i=3,c=7;");
        assert_eq!(done.frame_c(), 7);
        assert_eq!(done.anim_state(), None);
        let done = completed(b"Ga=a,i=7,s=3,v=1;");
        assert_eq!(done.anim_state(), Some(3));
        assert_eq!(done.anim_loops(), 1);
        // Gapless frame: negative gap is skipped during playback.
        let done = completed(b"Ga=a,i=7,r=2,z=-1;");
        assert_eq!(done.frame_gap_ms(), -1);
    }

    #[test]
    fn relative_placement_keys_parsed() {
        let done = completed(b"Ga=p,i=2,p=3,P=9,Q=4,H=-1,V=2;");
        assert!(done.keys.has_parent());
        assert_eq!(done.keys.parent_id, 9);
        assert_eq!(done.keys.parent_placement_id, 4);
        assert_eq!((done.keys.parent_dx, done.keys.parent_dy), (-1, 2));
        // Virtual prototypes cannot be relative (kitty `EINVAL`): the
        // parser still reports both halves; the store refuses the mix.
        let done = completed(b"Ga=p,U=1,i=2,P=9;");
        assert!(done.keys.is_virtual_placement() && done.keys.has_parent());
    }

    #[test]
    fn frame_data_requires_format_like_transmit() {
        // `a=f` carries pixels: `f=` stays mandatory.
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Ga=f,i=1,m=0;"),
            KittyFeedOutcome::Rejected(KittyApcReject::MissingFormat)
        ));
        assert!(!assembler.has_pending());
        // With `f=`, empty frame data completes (downstream validates
        // the rectangle against the image).
        let done = completed(b"Gf=32,a=f,i=1,s=2,v=2,m=0;");
        assert_eq!(done.action_a, Some('f'));
        assert!(done.payload.is_empty());
    }

    #[test]
    fn control_action_with_payload_is_rejected() {
        for raw in [
            b"Ga=d;eHh4".as_slice(),
            b"Ga=p,i=1;AA==".as_slice(),
            b"Ga=a,i=1;AA==".as_slice(),
            b"Ga=q;AA==".as_slice(),
        ] {
            let mut assembler = KittyApcAssembler::new();
            assert!(
                matches!(
                    assembler.feed(raw),
                    KittyFeedOutcome::Rejected(KittyApcReject::UnexpectedPayload)
                ),
                "expected UnexpectedPayload for {raw:?}"
            );
            assert!(!assembler.has_pending());
        }
    }

    #[test]
    fn image_id_and_number_together_rejected() {
        // The specification calls `i=` + `I=` an error (`EINVAL`).
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=32,i=1,I=2,m=0;"),
            KittyFeedOutcome::Rejected(KittyApcReject::MalformedControl)
        ));
        assert!(!assembler.has_pending());
        // Either alone is fine.
        assert_eq!(completed(b"Gf=32,I=13,m=0;").keys.image_number, 13);
        assert_eq!(completed(b"Gf=32,i=0,I=13,m=0;").keys.image_number, 13);
    }

    #[test]
    fn bad_medium_and_compression_rejected() {
        for raw in [
            b"Gf=32,t=x,m=0;".as_slice(),
            b"Gf=32,t=D,m=0;".as_slice(),
            b"Gf=32,o=x,m=0;".as_slice(),
            b"Gf=32,o=zz,m=0;".as_slice(),
        ] {
            let mut assembler = KittyApcAssembler::new();
            assert!(
                matches!(
                    assembler.feed(raw),
                    KittyFeedOutcome::Rejected(KittyApcReject::MalformedControl)
                ),
                "expected MalformedControl for {raw:?}"
            );
        }
        // `o=` empty stays uncompressed; `t=d` is the explicit default.
        let done = completed(b"Gf=32,o=,t=d,m=0;");
        assert!(done.keys.medium.is_direct());
    }

    #[test]
    fn shm_name_shape_enforced() {
        // Valid POSIX shm name: completes with the name as payload.
        let done = completed(&path_command("Gf=100,t=s,m=0;", b"/kitty-123"));
        assert_eq!(done.keys.medium, KittyMedium::SharedMemory);
        assert_eq!(&*done.payload, b"/kitty-123");
        for bad in [
            b"noslash".as_slice(),
            b"/".as_slice(),
            b"/a/b".as_slice(),
            b"/trailing/".as_slice(),
            b"".as_slice(),
        ] {
            let mut assembler = KittyApcAssembler::new();
            assert!(
                matches!(
                    assembler.feed(&path_command("Gf=100,t=s,m=0;", bad)),
                    KittyFeedOutcome::Rejected(KittyApcReject::BadPath)
                ),
                "expected BadPath for {bad:?}"
            );
            assert!(!assembler.has_pending());
        }
        // Overlong shm name (past NAME_MAX) fails closed.
        let long = vec![b'a'; KITTY_APC_SHM_NAME_MAX + 1];
        let mut name = b"/".to_vec();
        name.extend_from_slice(&long);
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(&path_command("Gf=100,t=s,m=0;", &name)),
            KittyFeedOutcome::Rejected(KittyApcReject::BadPath)
        ));
        // NUL bytes never name an object.
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(&path_command("Gf=100,t=s,m=0;", b"/a\x00b")),
            KittyFeedOutcome::Rejected(KittyApcReject::BadPath)
        ));
    }

    #[test]
    fn local_medium_ignores_chunking_flag() {
        // kitty/ghostty complete local mediums single-shot even with
        // `m=1` (`mpv` relies on this for `t=s`).
        let mut assembler = KittyApcAssembler::new();
        match assembler.feed(&path_command("Gf=100,t=s,m=1;", b"/mpv-shm")) {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(&*done.payload, b"/mpv-shm");
            }
            other => panic!("expected single-shot completion, got {other:?}"),
        }
        assert!(!assembler.has_pending());
    }

    #[test]
    fn file_medium_rejects_traversal_and_nul() {
        let done = completed(&path_command("Gf=100,t=f,m=0;", b"/tmp/tty-ok/x.png"));
        assert_eq!(done.keys.medium, KittyMedium::File);
        assert_eq!(&*done.payload, b"/tmp/tty-ok/x.png");
        for bad in [
            b"/tmp/../etc/passwd".as_slice(),
            b"..".as_slice(),
            b"a/../../b".as_slice(),
            b"/x/../y".as_slice(),
            b"/a\x00b".as_slice(),
            b"".as_slice(),
        ] {
            let mut assembler = KittyApcAssembler::new();
            assert!(
                matches!(
                    assembler.feed(&path_command("Gf=100,t=f,m=0;", bad)),
                    KittyFeedOutcome::Rejected(KittyApcReject::BadPath)
                ),
                "expected BadPath for {bad:?}"
            );
            assert!(!assembler.has_pending());
        }
        // A relative path without `..` passes parse-time (the opener
        // contains it); `..`-free absolute paths always pass here.
        let done = completed(&path_command("Gf=100,t=f,m=0;", b"relative/x.png"));
        assert_eq!(&*done.payload, b"relative/x.png");
    }

    #[test]
    fn temp_file_requires_protocol_marker() {
        let done = completed(&path_command(
            "Gf=100,t=t,m=0;",
            b"/tmp/tty-graphics-protocol-1",
        ));
        assert_eq!(done.keys.medium, KittyMedium::TempFile);
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(&path_command("Gf=100,t=t,m=0;", b"/tmp/other-file")),
            KittyFeedOutcome::Rejected(KittyApcReject::BadPath)
        ));
        assert!(!assembler.has_pending());
    }

    #[test]
    fn oversize_read_size_claim_rejected_up_front() {
        // `S=` above the decode cap can never be satisfied: refuse on the
        // first chunk, before buffering anything.
        let header = format!("Gf=100,t=f,S={},m=0;", KITTY_APC_DECODE_MAX_BYTES + 1);
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(&path_command(&header, b"/tmp/tty-ok/x.png")),
            KittyFeedOutcome::Rejected(KittyApcReject::OversizeClaim)
        ));
        assert!(!assembler.has_pending());
        assert_eq!(assembler.peak_memory(), 0);
        // A satisfiable `S=`/`O=` pair rides along for the downstream read.
        let done = completed(&path_command(
            "Gf=100,t=f,S=80,O=10,m=0;",
            b"/tmp/tty-ok/x.png",
        ));
        assert_eq!((done.keys.data_size, done.keys.data_offset), (80, 10));
    }

    #[test]
    fn path_payload_bounded_by_path_cap() {
        // A path stream can never grow past `PATH_MAX`: the ledger cap
        // does not apply (and must not be reachable through `t=s`).
        let big = vec![0x5a_u8; KITTY_APC_PATH_MAX_BYTES + 1];
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(&path_command("Gf=100,t=f,m=0;", &big)),
            KittyFeedOutcome::Rejected(KittyApcReject::Oversize)
        ));
        assert!(assembler.peak_memory() <= KITTY_APC_PATH_MAX_BYTES);
    }

    #[test]
    fn frame_stream_continuation_rules() {
        // Frame chunks repeat `a=f` (specification MUST).
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=32,s=2,v=2,a=f,i=3,m=1;/wAA//8A"),
            KittyFeedOutcome::NeedMore { .. }
        ));
        // 6 bytes down, 10 to go (2x2 RGBA = 16 declared).
        let tail10 = base64_encode(&[0x41; 10]);
        let tail = format!("Ga=f,m=0;{tail10}");
        match assembler.feed(tail.as_bytes()) {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(done.payload.len(), 16);
                assert_eq!(done.keys.image_id, 3);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        // A missing `a=` on a frame tail is accepted leniently: stream
        // binding decides, payload bytes are indistinguishable.
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=32,s=1,v=1,a=f,i=3,m=1;/wAA/w=="),
            KittyFeedOutcome::NeedMore { .. }
        ));
        assert!(matches!(
            assembler.feed(b"Gm=0;"),
            KittyFeedOutcome::Completed(_)
        ));
        // An explicit action change mid-frame-stream aborts the stream.
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=32,s=1,v=1,a=f,i=3,m=1;/wAA/w=="),
            KittyFeedOutcome::NeedMore { .. }
        ));
        assert!(matches!(
            assembler.feed(b"Ga=T,m=0;/wAA/w=="),
            KittyFeedOutcome::Rejected(KittyApcReject::Aborted)
        ));
        assert!(!assembler.has_pending());
        // Frame data can never continue an image stream.
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=32,s=1,v=1,m=1;/wAA/w=="),
            KittyFeedOutcome::NeedMore { .. }
        ));
        assert!(matches!(
            assembler.feed(b"Ga=f,m=0;/wAA/w=="),
            KittyFeedOutcome::Rejected(KittyApcReject::Aborted)
        ));
        assert!(!assembler.has_pending());
    }

    #[test]
    fn image_stream_rejects_explicit_action_change() {
        // Image-stream tails carry only `m`/`q`: an explicit `a=` is a
        // protocol violation and drops the stream (spec: finish all
        // chunks before any other graphics command).
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=32,s=1,v=1,m=1;/wAA/w=="),
            KittyFeedOutcome::NeedMore { .. }
        ));
        assert!(matches!(
            assembler.feed(b"Ga=T,m=0;/wAA/w=="),
            KittyFeedOutcome::Rejected(KittyApcReject::Aborted)
        ));
        assert!(!assembler.has_pending());
    }

    #[test]
    fn frame_data_chunks_reassemble() {
        // Multi-chunk `a=f` frame upload with the required repeats.
        let raw = [0x11_u8, 0x22, 0x33, 0x44];
        let encoded = base64_encode(&raw);
        let mid = encoded.len() / 2;
        let mut assembler = KittyApcAssembler::new();
        let opener = format!("Gf=32,s=2,v=2,a=f,i=9,m=1;{}", &encoded[..mid]);
        assert!(matches!(
            assembler.feed(opener.as_bytes()),
            KittyFeedOutcome::NeedMore { .. }
        ));
        let tail = format!("Ga=f,m=0;{}", &encoded[mid..]);
        match assembler.feed(tail.as_bytes()) {
            KittyFeedOutcome::Completed(done) => {
                assert_eq!(done.action_a, Some('f'));
                assert_eq!(done.keys.image_id, 9);
                assert_eq!(&*done.payload, &raw);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn pending_frame_stream_drop_clears_state() {
        // A malformed tail drops the frame stream like an image stream.
        let mut assembler = KittyApcAssembler::new();
        assert!(matches!(
            assembler.feed(b"Gf=32,s=1,v=1,a=f,i=3,m=1;/wAA/w=="),
            KittyFeedOutcome::NeedMore { .. }
        ));
        assert!(matches!(
            assembler.feed(b"Ga=f,m=x;"),
            KittyFeedOutcome::Rejected(KittyApcReject::BadMore)
                | KittyFeedOutcome::Rejected(KittyApcReject::Aborted)
        ));
        assert!(!assembler.has_pending());
    }

    #[test]
    fn delete_selectors_and_quiet_parsed() {
        for (raw, delete, quiet) in [
            (b"Ga=d,d=a;".as_slice(), Some('a'), 0),
            (b"Ga=d,d=A,q=1;".as_slice(), Some('A'), 1),
            (b"Ga=d,d=c,q=2;".as_slice(), Some('c'), 2),
            (b"Ga=d,d=F,i=4;".as_slice(), Some('F'), 0),
            (b"Ga=d,d=p,x=3,y=4;".as_slice(), Some('p'), 0),
            (b"Ga=d,d=q,x=3,y=4,z=2;".as_slice(), Some('q'), 0),
            (b"Ga=d,d=r,x=2,y=9;".as_slice(), Some('r'), 0),
            (b"Ga=d,d=x,x=3;".as_slice(), Some('x'), 0),
            (b"Ga=d,d=y,y=3;".as_slice(), Some('y'), 0),
            (b"Ga=d,d=z,z=0;".as_slice(), Some('z'), 0),
            (b"Ga=d,d=n,I=5;".as_slice(), Some('n'), 0),
        ] {
            let done = completed(raw);
            assert_eq!(done.keys.delete, delete, "for {raw:?}");
            assert_eq!(done.keys.quiet, quiet, "for {raw:?}");
            assert!(done.payload.is_empty());
        }
    }
}
