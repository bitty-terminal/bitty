//! Bounded sixel (`DCS ... q ... ST`) decoding (CTX-1668, bitty#2216).
//!
//! Sixel is the DEC bitmap protocol: a device-control string whose header ends
//! in `q` and whose body is a compact stream of palette definitions and
//! six-pixel columns. [`crate::Parser`] captures that body (it already owns the
//! DCS `hook`/`put`/`unhook` callbacks for the kitty font-query envelope) and
//! hands the complete payload here once per string; every other DCS stays
//! inert exactly as before. Decoding is a pure function from bytes to
//! [`SixelImage`], and the parser emits either
//! [`crate::TerminalAction::SixelGraphics`] or the counted
//! [`crate::TerminalAction::SixelRejected`].
//!
//! # Wire shape
//!
//! `ESC P <P1>;<P2>;<P3> q <body> ST` where `ST` is `ESC \` (C1 `ST` `0x9C`
//! terminates inside the scanner as well). `P1` is the pixel aspect ratio,
//! `P2` the background selector (`1` = transparent background), `P3` the grid
//! size; none of them change the wire grammar, so this module accepts any
//! header parameter value and only uses `P2`. The body grammar is:
//!
//! | Form | Meaning |
//! |---|---|
//! | `#Pc` | select colour register `Pc` |
//! | `#Pc;Pu;Px;Py;Pz` | select and define register `Pc` (`Pu=2` RGB percent, `Pu=1` HLS) |
//! | `"Pan;Pad;Ph;Pv` | raster attributes (pixel aspect, declared extent) |
//! | `!Pn` | repeat count for the next sixel data byte |
//! | `$` | carriage return (next column starts at the left of this band) |
//! | `-` | graphics newline (next band, six rows down) |
//! | `0x3F..=0x7E` | sixel data: one column of six pixels, LSB = top row |
//!
//! Undrawn pixels stay transparent (`alpha = 0`); [`SixelImage::transparent`]
//! carries `P2`, so a consumer that honours an opaque background fills them
//! with the terminal background instead. Declared raster extents (`"`) are
//! validated but never drive the allocation: the canvas is the drawn extent,
//! so an image that *claims* 8192 x 8192 and draws one pixel costs one pixel
//! (threat T-02, expansion from a header claim).
//!
//! # Bounds (threats T-01, T-02)
//!
//! - [`SIXEL_MAX_PAYLOAD_BYTES`] caps one captured body (4 MiB, the IMG-1
//!   wire cap kitty graphics uses). The capture buffer grows only after a
//!   real sixel header has been seen, and never past the cap.
//! - [`SIXEL_MAX_COLORS`] caps the colour-register index (256 registers, the
//!   protocol maximum); a larger index fails closed instead of indexing a
//!   map.
//! - [`SIXEL_MAX_WIDTH`]/[`SIXEL_MAX_HEIGHT`] cap each side (8192, the kitty
//!   decode ceiling) and [`SIXEL_MAX_PIXELS`] caps the area (4096 x 4096),
//!   both before any pixel buffer exists. A repeat count (`!n`) or a band
//!   count is checked against them with `u64` arithmetic *before* the
//!   expansion is materialized, so `!65535~` is refused from eight bytes
//!   instead of materializing 65k columns.
//! - [`SIXEL_MAX_BYTES`] caps the RGBA8 output (64 MiB), matching the kitty
//!   IMG-3 decode ceiling.
//! - The aggregate per-screen budget is **not** taken here: it is the shared
//!   image layer's byte quota (bitty-rich `KITTY_PLACE_MAX_BYTES`, 256 MiB
//!   per screen and 64 MiB per origin), which a decoded raster is charged
//!   against through the same `store`/`display` seam a decoded kitty bitmap
//!   uses.
//!
//! # Fail-closed behavior
//!
//! Every rejection returns a typed [`SixelReject`] and produces no image and
//! no allocation above the caps; the parser drops the payload and emits
//! [`crate::TerminalAction::SixelRejected`], which terminal-state counts in
//! `TelemetryCounters::sixel_refused`. A payload the parser-wide control
//! string cap aborted mid-string is refused the same way
//! ([`SixelReject::PayloadTooLarge`]) instead of being decoded as a silently
//! truncated image.

/// Maximum bytes captured for one sixel body (IMG-1 wire cap parity).
///
/// The buffer is allocated lazily when a sixel header is seen and retained
/// (bounded) by the parser afterwards, exactly like the iTerm2 OSC scanner.
pub const SIXEL_MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// Maximum colour-register index + 1 (protocol maximum: 256 registers).
pub const SIXEL_MAX_COLORS: usize = 256;

/// Maximum decoded width in pixels (kitty decode ceiling parity).
pub const SIXEL_MAX_WIDTH: u32 = 8192;

/// Maximum decoded height in pixels (kitty decode ceiling parity).
pub const SIXEL_MAX_HEIGHT: u32 = 8192;

/// Maximum decoded pixels (4096 x 4096), checked before allocation.
pub const SIXEL_MAX_PIXELS: u64 = 4096 * 4096;

/// Maximum decoded RGBA8 bytes (64 MiB), the kitty IMG-3 ceiling.
pub const SIXEL_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Rows in one sixel band.
const SIXEL_BAND_ROWS: u32 = 6;

/// `?` in the sixel data range: bits `0`, i.e. a column with no pixel set.
const SIXEL_DATA_BASE: u8 = 0x3F;

/// Header parameters of one sixel control string.
///
/// Only `P2` (background selector) changes the decoded result; `P1`/`P3` are
/// recorded for diagnostics but never gate admission (a conforming emitter is
/// free to send any value there, and none of them expands the canvas).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SixelParams {
    /// `P1`, the pixel aspect ratio (informational: pixels decode square).
    pub aspect: u16,
    /// `P2 == 1`: leave undrawn pixels transparent instead of filling them
    /// with the terminal background.
    pub transparent: bool,
}

/// One decoded sixel bitmap: owned RGBA8 pixels in row-major order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SixelImage {
    /// Decoded pixel width (the widest drawn column, at least 1).
    pub width: u32,
    /// Decoded pixel height (the lowest drawn row + 1, at least 1).
    pub height: u32,
    /// Undrawn pixels are background, not transparent (`P2 != 1`).
    pub transparent: bool,
    /// RGBA8 bytes, exactly `width * height * 4` long; undrawn pixels are
    /// all-zero (alpha 0).
    pub rgba: Vec<u8>,
}

impl SixelImage {
    /// Decoded RGBA8 byte count.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }
}

/// Why a sixel payload (or one definition inside it) was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SixelReject {
    /// The payload carried no sixel data at all.
    Empty,
    /// The captured body exceeded [`SIXEL_MAX_PAYLOAD_BYTES`], or the
    /// parser-wide control-string cap aborted the string first.
    PayloadTooLarge {
        /// Captured (or claimed) payload length in bytes.
        bytes: usize,
    },
    /// A colour-register index reached [`SIXEL_MAX_COLORS`] or beyond.
    PaletteOverflow {
        /// The refused register index.
        register: u32,
    },
    /// A drawn or declared extent exceeded [`SIXEL_MAX_WIDTH`] or
    /// [`SIXEL_MAX_HEIGHT`].
    DimensionsTooLarge {
        /// Offending width in pixels (drawn columns, or the declared extent).
        width: u32,
        /// Offending height in pixels.
        height: u32,
    },
    /// The drawn area exceeded [`SIXEL_MAX_PIXELS`].
    TooManyPixels {
        /// Drawn pixel count.
        pixels: u64,
    },
    /// The decoded bitmap exceeded [`SIXEL_MAX_BYTES`].
    ImageTooLarge {
        /// Decoded RGBA8 byte count.
        bytes: usize,
    },
    /// A byte outside the sixel grammar, a malformed definition, or a
    /// truncated numeric parameter.
    Malformed {
        /// Byte offset inside the payload where the grammar broke.
        offset: usize,
    },
}

impl SixelReject {
    /// Stable short label used in diagnostics and telemetry.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Empty => "sixel payload carries no image",
            Self::PayloadTooLarge { .. } => "sixel payload exceeds the capture cap",
            Self::PaletteOverflow { .. } => "sixel colour register exceeds the palette cap",
            Self::DimensionsTooLarge { .. } => "sixel image exceeds the dimension cap",
            Self::TooManyPixels { .. } => "sixel image exceeds the pixel cap",
            Self::ImageTooLarge { .. } => "sixel image exceeds the decoded byte cap",
            Self::Malformed { .. } => "sixel payload is malformed",
        }
    }
}

impl std::fmt::Display for SixelReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str(Self::Empty.label()),
            Self::PayloadTooLarge { bytes } => {
                write!(
                    f,
                    "{} ({bytes} > {SIXEL_MAX_PAYLOAD_BYTES} bytes)",
                    self.label()
                )
            }
            Self::PaletteOverflow { register } => write!(
                f,
                "{} ({register} >= {SIXEL_MAX_COLORS} registers)",
                self.label()
            ),
            Self::DimensionsTooLarge { width, height } => write!(
                f,
                "{} ({width} x {height}, cap {} x {})",
                self.label(),
                SIXEL_MAX_WIDTH,
                SIXEL_MAX_HEIGHT
            ),
            Self::TooManyPixels { pixels } => {
                write!(f, "{} ({pixels} > {SIXEL_MAX_PIXELS} pixels)", self.label())
            }
            Self::ImageTooLarge { bytes } => {
                write!(f, "{} ({bytes} > {SIXEL_MAX_BYTES} bytes)", self.label())
            }
            Self::Malformed { offset } => write!(f, "{} (at byte {offset})", self.label()),
        }
    }
}

impl std::error::Error for SixelReject {}

/// Colour registers: `(r, g, b)` per register, seeded with the VT340 defaults.
///
/// Registers past 15 start black, which is what a stream that selects an
/// undefined register gets on the reference terminals.
#[derive(Clone)]
struct Palette {
    colors: [(u8, u8, u8); SIXEL_MAX_COLORS],
}

impl Default for Palette {
    fn default() -> Self {
        let mut colors = [(0u8, 0u8, 0u8); SIXEL_MAX_COLORS];
        // VT340 / xterm default registers 0..=15.
        const DEFAULTS: [(u8, u8, u8); 16] = [
            (0, 0, 0),
            (0, 0, 170),
            (0, 170, 0),
            (0, 170, 170),
            (170, 0, 0),
            (170, 0, 170),
            (170, 85, 0),
            (170, 170, 170),
            (85, 85, 85),
            (85, 85, 255),
            (85, 255, 85),
            (85, 255, 255),
            (255, 85, 85),
            (255, 85, 255),
            (255, 255, 85),
            (255, 255, 255),
        ];
        let mut index = 0;
        while index < DEFAULTS.len() {
            colors[index] = DEFAULTS[index];
            index += 1;
        }
        Self { colors }
    }
}

impl Palette {
    fn lookup(&self, register: usize) -> (u8, u8, u8) {
        self.colors.get(register).copied().unwrap_or((0, 0, 0))
    }
}

/// One sixel data byte after repeat expansion is resolved.
#[derive(Clone, Copy)]
struct Cell {
    /// Left column of the strip in pixels.
    x: u32,
    /// Top row of the band in pixels.
    y: u32,
    /// Number of identical strips to draw from `x` rightwards.
    repeat: u32,
    /// Six pixel bits, bit 0 = top row.
    bits: u8,
    /// Colour register index (`< SIXEL_MAX_COLORS`).
    color: usize,
}

/// Bounds of the drawn content, collected while walking the payload.
#[derive(Default, Clone, Copy)]
struct Extent {
    width: u32,
    height: u32,
    cells: usize,
}

/// Walks the sixel body once, resolving grammar and reporting cells.
///
/// Grammar/limit violations are rejected here in both passes, so the render
/// pass can never see a cell the measure pass refused. `u64` arithmetic keeps
/// repeat expansion (the T-02 expansion vector) from overflowing before it is
/// compared against the caps. The palette is passed to the sink so the render
/// pass sees definitions in stream order without a second mutable borrow.
fn walk(
    payload: &[u8],
    palette: &mut Palette,
    extent: &mut Extent,
    sink: &mut dyn FnMut(&Palette, Cell),
) -> Result<(), SixelReject> {
    let mut x: u32 = 0;
    let mut y: u32 = 0;
    let mut color: usize = 0;
    let mut repeat: u32 = 1;
    let mut index = 0usize;
    while index < payload.len() {
        let byte = payload[index];
        match byte {
            b'#' => {
                repeat = 1;
                index += 1;
                let register = parse_number(payload, &mut index)?;
                if register as usize >= SIXEL_MAX_COLORS {
                    return Err(SixelReject::PaletteOverflow { register });
                }
                color = register as usize;
                if index < payload.len() && payload[index] == b';' {
                    let mut values = [0u32; 4];
                    let mut count = 0usize;
                    while index < payload.len() && payload[index] == b';' {
                        index += 1;
                        if count == values.len() {
                            return Err(SixelReject::Malformed { offset: index });
                        }
                        values[count] = parse_number(payload, &mut index)?;
                        count += 1;
                    }
                    if count != values.len() {
                        return Err(SixelReject::Malformed { offset: index });
                    }
                    define_color(palette, color, values);
                }
            }
            b'"' => {
                repeat = 1;
                index += 1;
                let mut values = [0u32; 4];
                let mut count = 0usize;
                loop {
                    if count == values.len() {
                        return Err(SixelReject::Malformed { offset: index });
                    }
                    values[count] = parse_number(payload, &mut index)?;
                    count += 1;
                    if index < payload.len() && payload[index] == b';' {
                        index += 1;
                    } else {
                        break;
                    }
                }
                if count != values.len() {
                    return Err(SixelReject::Malformed { offset: index });
                }
                // Declared extent: validated, never allocated (T-02).
                let declared_width = values[2];
                let declared_height = values[3];
                if declared_width > SIXEL_MAX_WIDTH || declared_height > SIXEL_MAX_HEIGHT {
                    return Err(SixelReject::DimensionsTooLarge {
                        width: declared_width,
                        height: declared_height,
                    });
                }
            }
            b'!' => {
                index += 1;
                repeat = parse_number(payload, &mut index)?;
            }
            b'$' => {
                x = 0;
                repeat = 1;
                index += 1;
            }
            b'-' => {
                let next = u64::from(y) + u64::from(SIXEL_BAND_ROWS);
                if next > u64::from(SIXEL_MAX_HEIGHT) {
                    return Err(SixelReject::DimensionsTooLarge {
                        width: 0,
                        height: u32::try_from(next).unwrap_or(u32::MAX),
                    });
                }
                y = u32::try_from(next).unwrap_or(SIXEL_MAX_HEIGHT);
                x = 0;
                repeat = 1;
                index += 1;
            }
            SIXEL_DATA_BASE..=0x7E => {
                if repeat == 0 {
                    repeat = 1;
                    index += 1;
                    continue;
                }
                let bits = byte - SIXEL_DATA_BASE;
                // Expansion checks before the strips exist (T-02): one data
                // byte may claim up to `repeat` columns and a full band.
                let columns = u64::from(x) + u64::from(repeat);
                let band_end = u64::from(y) + u64::from(SIXEL_BAND_ROWS);
                if columns > u64::from(SIXEL_MAX_WIDTH) || band_end > u64::from(SIXEL_MAX_HEIGHT) {
                    return Err(SixelReject::DimensionsTooLarge {
                        width: u32::try_from(columns).unwrap_or(u32::MAX),
                        height: u32::try_from(band_end).unwrap_or(u32::MAX),
                    });
                }
                extent.cells += 1;
                extent.width = extent
                    .width
                    .max(u32::try_from(columns).unwrap_or(SIXEL_MAX_WIDTH));
                if bits != 0 {
                    let rows = u32::from(bits).ilog2() + 1;
                    extent.height = extent.height.max(y.saturating_add(rows));
                }
                sink(
                    palette,
                    Cell {
                        x,
                        y,
                        repeat,
                        bits,
                        color,
                    },
                );
                x = u32::try_from(columns).unwrap_or(SIXEL_MAX_WIDTH);
                repeat = 1;
                index += 1;
            }
            // Tolerate the line breaks some emitters inject; every other byte
            // outside the grammar fails closed.
            b'\r' | b'\n' => index += 1,
            _ => return Err(SixelReject::Malformed { offset: index }),
        }
    }
    Ok(())
}

/// Parses one decimal parameter; a missing number is malformed.
fn parse_number(payload: &[u8], index: &mut usize) -> Result<u32, SixelReject> {
    let start = *index;
    let mut value: u32 = 0;
    while *index < payload.len() {
        let byte = payload[*index];
        if !byte.is_ascii_digit() {
            break;
        }
        value = value
            .saturating_mul(10)
            .saturating_add(u32::from(byte - b'0'));
        *index += 1;
    }
    if *index == start {
        return Err(SixelReject::Malformed { offset: start });
    }
    Ok(value)
}

/// Applies a `#Pc;Pu;Px;Py;Pz` definition to one register.
///
/// `Pu` selects the colour space: `2` (or an omitted `0`) is RGB in percent,
/// `1` is HLS (hue degrees, lightness and saturation in percent). Values are
/// clamped rather than refused: they cannot change the canvas size, only the
/// colour of pixels that already fit the caps.
fn define_color(palette: &mut Palette, register: usize, values: [u32; 4]) {
    let [space, first, second, third] = values;
    let rgb = match space {
        1 => hls_to_rgb(first, second, third),
        _ => (
            percent_to_byte(first),
            percent_to_byte(second),
            percent_to_byte(third),
        ),
    };
    if let Some(slot) = palette.colors.get_mut(register) {
        *slot = rgb;
    }
}

/// Maps a `0..=100` wire percentage to `0..=255`.
fn percent_to_byte(value: u32) -> u8 {
    let clamped = value.min(100);
    u8::try_from((clamped * 255 + 50) / 100).unwrap_or(255)
}

/// Converts HLS (hue degrees, lightness/saturation percent) to RGB.
fn hls_to_rgb(hue: u32, lightness: u32, saturation: u32) -> (u8, u8, u8) {
    let l = f64::from(lightness.min(100)) / 100.0;
    let s = f64::from(saturation.min(100)) / 100.0;
    if s == 0.0 {
        let grey = percent_to_byte(lightness);
        return (grey, grey, grey);
    }
    let h = f64::from(hue % 360) / 360.0;
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let channel = |t: f64| -> u8 {
        let t = t.rem_euclid(1.0);
        let value = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 1.0 / 2.0 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        let scaled = (value * 255.0).round().clamp(0.0, 255.0);
        u8::try_from(scaled as u32).unwrap_or(255)
    };
    (channel(h + 1.0 / 3.0), channel(h), channel(h - 1.0 / 3.0))
}

/// Decodes one complete sixel payload into an RGBA8 bitmap.
///
/// Two passes: the first resolves the grammar, palette and drawn extent and
/// rejects anything over the caps without allocating pixels; the second
/// allocates exactly `width * height * 4` bytes and replays the same cells
/// sequentially, so a register redefined mid-image colours the pixels that
/// follow it and nothing else.
///
/// # Errors
///
/// [`SixelReject`] for an empty or oversize payload, a palette index past
/// [`SIXEL_MAX_COLORS`], a drawn/declared extent past
/// [`SIXEL_MAX_WIDTH`]/[`SIXEL_MAX_HEIGHT`]/[`SIXEL_MAX_PIXELS`], or a byte
/// outside the grammar. No path allocates above the caps and no path panics
/// on untrusted bytes.
pub fn decode_sixel(payload: &[u8], params: SixelParams) -> Result<SixelImage, SixelReject> {
    if payload.len() > SIXEL_MAX_PAYLOAD_BYTES {
        return Err(SixelReject::PayloadTooLarge {
            bytes: payload.len(),
        });
    }
    if payload.is_empty() {
        return Err(SixelReject::Empty);
    }
    let mut measure_palette = Palette::default();
    let mut extent = Extent::default();
    walk(payload, &mut measure_palette, &mut extent, &mut |_, _| {})?;
    if extent.cells == 0 || extent.width == 0 || extent.height == 0 {
        return Err(SixelReject::Empty);
    }
    let pixels = u64::from(extent.width) * u64::from(extent.height);
    if pixels > SIXEL_MAX_PIXELS {
        return Err(SixelReject::TooManyPixels { pixels });
    }
    let bytes = usize::try_from(pixels)
        .ok()
        .and_then(|count| count.checked_mul(4))
        .ok_or(SixelReject::ImageTooLarge { bytes: usize::MAX })?;
    if bytes > SIXEL_MAX_BYTES {
        return Err(SixelReject::ImageTooLarge { bytes });
    }
    let width = usize::try_from(extent.width).unwrap_or(0);
    let mut rgba = vec![0u8; bytes];
    let mut palette = Palette::default();
    let mut discarded = Extent::default();
    let mut draw = |palette: &Palette, cell: Cell| {
        let (red, green, blue) = palette.lookup(cell.color);
        let mut bit = 0u8;
        while bit < 6 {
            if cell.bits & (1u8 << bit) != 0 {
                let row = usize::try_from(cell.y + u32::from(bit)).unwrap_or(usize::MAX);
                let mut step = 0u32;
                while step < cell.repeat {
                    let col = usize::try_from(cell.x + step).unwrap_or(usize::MAX);
                    let offset = row
                        .saturating_mul(width)
                        .saturating_add(col)
                        .saturating_mul(4);
                    if let Some(pixel) = rgba.get_mut(offset..offset + 4) {
                        pixel[0] = red;
                        pixel[1] = green;
                        pixel[2] = blue;
                        pixel[3] = 255;
                    }
                    step += 1;
                }
            }
            bit += 1;
        }
    };
    // The render pass repeats the same validation; a mismatch would mean the
    // two passes disagree, which the caps above make unreachable in release
    // builds and `debug_assert` pins in tests.
    walk(payload, &mut palette, &mut discarded, &mut draw)?;
    debug_assert_eq!(discarded.width, extent.width);
    debug_assert_eq!(discarded.height, extent.height);
    Ok(SixelImage {
        width: extent.width,
        height: extent.height,
        transparent: params.transparent,
        rgba,
    })
}

/// Bounded capture of one sixel DCS body (parser-owned).
///
/// The buffer is empty until [`SixelCapture::begin`] runs for a real sixel
/// header, so ordinary DCS traffic allocates nothing; once a sixel body has
/// been captured the (bounded) capacity is retained for the next one, exactly
/// like the iTerm2 OSC scanner.
#[derive(Debug, Default)]
pub struct SixelCapture {
    payload: Vec<u8>,
    params: SixelParams,
    active: bool,
    overflow: bool,
}

impl SixelCapture {
    /// An idle capture that owns no buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts capturing a sixel body; drops any previous partial payload.
    pub fn begin(&mut self, params: SixelParams) {
        self.payload.clear();
        self.params = params;
        self.active = true;
        self.overflow = false;
    }

    /// Whether a sixel body is open (the DCS `hook` ran, `unhook` has not).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Appends one payload byte, remembering an over-cap capture.
    ///
    /// Bytes past [`SIXEL_MAX_PAYLOAD_BYTES`] are dropped (never buffered);
    /// the capture still ends fail-closed at `unhook` time.
    pub fn put(&mut self, byte: u8) {
        if !self.active || self.overflow {
            return;
        }
        if self.payload.len() >= SIXEL_MAX_PAYLOAD_BYTES {
            self.overflow = true;
            return;
        }
        self.payload.push(byte);
    }

    /// Marks the capture as refused without dropping it.
    ///
    /// The parser calls this when its own control-string cap aborts the
    /// string: the body it forwarded is a prefix, so decoding it would paint
    /// a silently truncated image.
    pub fn refuse(&mut self) {
        if self.active {
            self.overflow = true;
        }
    }

    /// Ends the capture and decodes the body; `None` when none was open.
    pub fn finish(&mut self) -> Option<Result<SixelImage, SixelReject>> {
        if !self.active {
            return None;
        }
        self.active = false;
        if self.overflow {
            let bytes = self.payload.len();
            self.payload.clear();
            return Some(Err(SixelReject::PayloadTooLarge { bytes }));
        }
        Some(decode_sixel(&self.payload, self.params))
    }

    /// Drops an open capture without decoding it.
    pub fn abort(&mut self) {
        self.active = false;
        self.overflow = false;
        self.payload.clear();
    }

    /// Bytes currently captured (bounded by [`SIXEL_MAX_PAYLOAD_BYTES`]).
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        self.payload.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opaque() -> SixelParams {
        SixelParams::default()
    }

    fn pixel(image: &SixelImage, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y * image.width + x) * 4) as usize;
        [
            image.rgba[offset],
            image.rgba[offset + 1],
            image.rgba[offset + 2],
            image.rgba[offset + 3],
        ]
    }

    /// `#0;2;100;100;100` selects register 0 and defines it white.
    const WHITE: &[u8] = b"#0;2;100;100;100";

    fn with_white(body: &[u8]) -> Vec<u8> {
        let mut payload = Vec::from(WHITE);
        payload.extend_from_slice(body);
        payload
    }

    #[test]
    fn decodes_single_column_with_lsb_top() {
        // `#2` selects the default green register; `A` = 0x41 -> bits 0b10:
        // row 0 clear, row 1 set.
        let image = decode_sixel(b"#2A", opaque()).expect("single column decodes");
        assert_eq!((image.width, image.height), (1, 2));
        assert_eq!(pixel(&image, 0, 0), [0, 0, 0, 0]);
        assert_eq!(pixel(&image, 0, 1), [0, 170, 0, 255]);
    }

    #[test]
    fn palette_definition_uses_rgb_percent() {
        let image = decode_sixel(b"#1;2;100;0;0@", opaque()).expect("rgb definition decodes");
        assert_eq!(pixel(&image, 0, 0), [255, 0, 0, 255]);
        assert_eq!(image.byte_len(), 4);
    }

    #[test]
    fn palette_definition_uses_hls() {
        // HLS 120 degrees at 50% lightness / 100% saturation is pure green.
        let image = decode_sixel(b"#3;1;120;50;100@", opaque()).expect("hls definition decodes");
        assert_eq!(pixel(&image, 0, 0), [0, 255, 0, 255]);
    }

    #[test]
    fn later_definition_only_affects_later_pixels() {
        let image =
            decode_sixel(b"#1;2;100;0;0@#1;2;0;0;100@", opaque()).expect("redefinition decodes");
        assert_eq!(image.width, 2);
        assert_eq!(pixel(&image, 0, 0), [255, 0, 0, 255]);
        assert_eq!(pixel(&image, 1, 0), [0, 0, 255, 255]);
    }

    #[test]
    fn repeat_expands_columns() {
        let image = decode_sixel(&with_white(b"!3@"), opaque()).expect("repeat decodes");
        assert_eq!((image.width, image.height), (3, 1));
        for column in 0..3 {
            assert_eq!(pixel(&image, column, 0), [255, 255, 255, 255]);
        }
    }

    #[test]
    fn carriage_return_restarts_the_band() {
        let image = decode_sixel(&with_white(b"@~$@"), opaque()).expect("carriage return decodes");
        assert_eq!((image.width, image.height), (2, 6));
        assert_eq!(pixel(&image, 1, 5), [255, 255, 255, 255]);
        assert_eq!(pixel(&image, 0, 5), [0, 0, 0, 0]);
    }

    #[test]
    fn graphics_newline_starts_the_next_band() {
        let image = decode_sixel(&with_white(b"@-@"), opaque()).expect("band move decodes");
        assert_eq!((image.width, image.height), (1, 7));
        assert_eq!(pixel(&image, 0, 0), [255, 255, 255, 255]);
        assert_eq!(pixel(&image, 0, 6), [255, 255, 255, 255]);
    }

    #[test]
    fn raster_attributes_are_validated_not_allocated() {
        let image = decode_sixel(b"\"1;1;8192;8192@", opaque()).expect("declared extent decodes");
        assert_eq!((image.width, image.height), (1, 1));

        let error = decode_sixel(b"\"1;1;8193;1@", opaque()).expect_err("declared width refused");
        assert!(matches!(
            error,
            SixelReject::DimensionsTooLarge { width: 8193, .. }
        ));
    }

    #[test]
    fn palette_register_past_cap_fails_closed() {
        let error = decode_sixel(b"#256;2;100;0;0@", opaque()).expect_err("register 256 refused");
        assert_eq!(error, SixelReject::PaletteOverflow { register: 256 });
        assert!(error.to_string().contains("256"));
    }

    #[test]
    fn oversized_repeat_fails_closed_without_expansion() {
        let error = decode_sixel(b"!65535~", opaque()).expect_err("wide repeat refused");
        assert!(matches!(
            error,
            SixelReject::DimensionsTooLarge {
                width: 65535,
                height: 6
            }
        ));
    }

    #[test]
    fn band_overflow_fails_closed() {
        // 8192 rows is the cap: 1365 bands (8190 rows) fit, the next does not.
        let mut payload = vec![b'-'; 1366];
        payload.push(b'@');
        let error = decode_sixel(&payload, opaque()).expect_err("band overflow refused");
        assert!(matches!(error, SixelReject::DimensionsTooLarge { .. }));
    }

    #[test]
    fn empty_and_blank_payloads_are_refused() {
        assert_eq!(decode_sixel(b"", opaque()), Err(SixelReject::Empty));
        assert_eq!(decode_sixel(b"#3", opaque()), Err(SixelReject::Empty));
        assert_eq!(decode_sixel(b"?#3", opaque()), Err(SixelReject::Empty));
    }

    #[test]
    fn malformed_bytes_fail_closed() {
        assert_eq!(
            decode_sixel(b"@;", opaque()),
            Err(SixelReject::Malformed { offset: 1 })
        );
        assert_eq!(
            decode_sixel(b"#", opaque()),
            Err(SixelReject::Malformed { offset: 1 })
        );
        assert_eq!(
            decode_sixel(b"#1;2;100", opaque()),
            Err(SixelReject::Malformed { offset: 8 })
        );
    }

    #[test]
    fn transparent_background_flag_is_carried() {
        let params = SixelParams {
            aspect: 2,
            transparent: true,
        };
        let image = decode_sixel(b"@", params).expect("transparent image decodes");
        assert!(image.transparent);
        assert!(!image.rgba.is_empty());
    }

    #[test]
    fn capture_is_bounded_and_lazy() {
        let mut capture = SixelCapture::new();
        assert!(!capture.is_active());
        assert!(capture.finish().is_none());
        capture.begin(opaque());
        assert!(capture.is_active());
        for _ in 0..SIXEL_MAX_PAYLOAD_BYTES + 16 {
            capture.put(b'@');
        }
        assert_eq!(capture.buffered_len(), SIXEL_MAX_PAYLOAD_BYTES);
        let outcome = capture.finish().expect("capture had a body");
        assert!(matches!(
            outcome,
            Err(SixelReject::PayloadTooLarge {
                bytes: SIXEL_MAX_PAYLOAD_BYTES
            })
        ));
    }

    #[test]
    fn refused_capture_never_decodes() {
        let mut capture = SixelCapture::new();
        capture.begin(opaque());
        capture.put(b'@');
        capture.refuse();
        let outcome = capture.finish().expect("capture had a body");
        assert!(outcome.is_err(), "a truncated body must never decode");
        assert!(!capture.is_active());
    }

    #[test]
    fn area_cap_refuses_wide_band_area() {
        // 4096 columns x 4098 rows draws past the 4096 x 4096 area cap, so the
        // image is refused before a 64 MiB buffer exists.
        let mut payload = Vec::new();
        payload.push(b'!');
        payload.extend_from_slice(b"4096");
        payload.push(b'~');
        for _ in 0..682 {
            payload.push(b'-');
            payload.push(b'!');
            payload.extend_from_slice(b"4096");
            payload.push(b'~');
        }
        let error = decode_sixel(&payload, opaque()).expect_err("area cap refused");
        assert!(
            matches!(error, SixelReject::TooManyPixels { pixels } if pixels > SIXEL_MAX_PIXELS)
        );
    }

    #[test]
    fn zero_repeat_does_not_inflate_extent_or_cells() {
        // A payload whose only sixel data has repeat 0 draws no pixels and must
        // fail closed as Empty.
        assert_eq!(decode_sixel(b"!0~", opaque()), Err(SixelReject::Empty));

        // Following a drawn 1x1 pixel (`@`) with `!0~` must not inflate the
        // image height from 1 to 6 for a zero-column repeat.
        let image = decode_sixel(&with_white(b"@!0~"), opaque()).expect("1x1 pixel decodes");
        assert_eq!((image.width, image.height), (1, 1));
        assert_eq!(pixel(&image, 0, 0), [255, 255, 255, 255]);
    }

    #[test]
    fn non_data_commands_reset_pending_repeat() {
        // A stray `!5` before `#` or `"` must not leak across the colour or
        // raster-attribute command into a subsequent sixel data byte.
        let after_color =
            decode_sixel(b"!5#1;2;100;0;0@", opaque()).expect("colour command resets repeat");
        assert_eq!((after_color.width, after_color.height), (1, 1));

        let after_raster =
            decode_sixel(b"!5\"1;1;10;10@", opaque()).expect("raster command resets repeat");
        assert_eq!((after_raster.width, after_raster.height), (1, 1));
    }
}
