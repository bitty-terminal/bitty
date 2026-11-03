//! Idle-RSS attribution (bitty#2859).
//!
//! The idle baseline is measured as process totals, and a total alone cannot
//! say which subsystem to work on. This module turns the live runtime state
//! plus the kernel's own accounting for the process into one bounded,
//! allocation-light report that charges every byte it can prove and names the
//! remainder `unattributed` instead of guessing at it.
//!
//! # Rows and provenance
//!
//! Every row carries a [`Provenance`]:
//!
//! | Row | Source | Provenance |
//! | --- | --- | --- |
//! | `glyph-atlas` | `GridRenderer::retained_bytes` (glyph cache + atlas texels + block masks) | measured |
//! | `fonts` | `Rss` of font-file mappings in `/proc/self/smaps` (Linux) | measured |
//! | `scrollback` | `State::retained_heap_bytes` summed over every live grid | measured |
//! | `lua-vm` | plugin Lua heap (`LuaVm::memory_used`, refreshed by the VM) | measured |
//! | `surface` | extent × bytes per texel × resident swap-chain images | estimated |
//! | `thread-stacks` | thread count × per-thread stack reservation | reservation |
//! | `unattributed` | process RSS minus every measured/estimated row | residual |
//!
//! Two rules keep the report honest:
//!
//! - **No silent zero.** A source that cannot be read (non-Linux, an opaque
//!   upstream allocator, a plugin VM borrowed mutably at sample time) is
//!   [`Provenance::Unavailable`] with `bytes == 0`, never a measured `0`.
//! - **Reservations never enter the RSS arithmetic.** Thread stacks are
//!   virtual reservations whose resident pages land wherever the kernel put
//!   them, so they are reported for completeness but excluded from
//!   `accounted_bytes`. `unattributed` is therefore always
//!   `rss - (measured + estimated)`, and [`MemoryReport::over_attributed`]
//!   records the (impossible) case where the rows exceed RSS, which would mean
//!   a source is double-counting.
//!
//! # Cost
//!
//! [`Runtime::memory_report`] is a plain `&self` call that allocates only its
//! own row vector, and it is never called from the paint loop: it runs when a
//! caller asks for a report (an opt-in periodic dump, a soak mark, a test).
//! The only kernel reads are `smaps_rollup`, `smaps` (fonts), and the task
//! directory, each bounded in bytes and mapping count below.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::Runtime;

/// Hard cap for one rendered report (the file and the string form share it).
pub const MEMORY_REPORT_MAX_BYTES: usize = 8 * 1024;

/// Cap on the number of `/proc/self/smaps` mappings examined for font files.
pub const MAX_FONT_MAPPINGS: usize = 4096;

/// Cap on bytes read from one `/proc` file.
///
/// Only the Linux `/proc` reader consumes it, so the constant is gated with
/// that reader: an ungated definition is dead code on Windows and macOS and
/// fails the `-D warnings` workspace check there.
#[cfg(target_os = "linux")]
const MAX_PROC_BYTES: usize = 8 * 1024 * 1024;

/// Rust's standard per-thread stack reservation when `RUST_MIN_STACK` is
/// unset (the value `std::thread` uses for spawned threads).
const SPAWNED_THREAD_STACK_BYTES: u64 = 2 * 1024 * 1024;

/// Main-thread stack reservation (the value a glibc pthread program gets from
/// `getrlimit(RLIMIT_STACK)` on the supported Linux desktop targets).
const MAIN_THREAD_STACK_BYTES: u64 = 8 * 1024 * 1024;

/// Every surface format the renderer selects is a four-channel 8-bit format.
const SURFACE_BYTES_PER_TEXEL: u64 = 4;

/// Swap-chain images resident at idle for a `Fifo` surface.
///
/// CTX-1036 (#1809) picks `Fifo` over `Mailbox` precisely so idle keeps two
/// resident images instead of three; the estimate below reuses that decision.
const SURFACE_IMAGES_FIFO: u64 = 2;

/// Swap-chain images resident for a `Mailbox` surface.
const SURFACE_IMAGES_MAILBOX: u64 = 3;

/// Swap-chain images resident for the remaining present modes.
const SURFACE_IMAGES_DEFAULT: u64 = 2;

/// File extensions treated as font files when attributing file-backed
/// mappings. These are format suffixes, not host paths: the report never
/// embeds a directory layout.
const FONT_FILE_EXTENSIONS: &[&str] = &[
    ".ttf", ".otf", ".ttc", ".otc", ".pfb", ".pcf", ".bdf", ".woff", ".woff2",
];

/// Where one row's bytes came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// The owning subsystem reports exact retained bytes or capacity.
    Measured,
    /// Derived from a configuration (extent, format, image count), not read
    /// back from the allocator.
    Estimated,
    /// A virtual reservation (thread stacks). Excluded from the RSS
    /// arithmetic; reported for completeness.
    Reservation,
    /// The source could not be read on this platform or at this moment.
    /// `bytes` is `0` and means "unknown", never "measured zero".
    Unavailable,
}

impl Provenance {
    /// Stable key rendered in the report.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Estimated => "estimated",
            Self::Reservation => "reservation",
            Self::Unavailable => "unavailable",
        }
    }

    /// Whether the row's bytes participate in the RSS arithmetic.
    #[must_use]
    pub const fn counts_toward_rss(self) -> bool {
        matches!(self, Self::Measured | Self::Estimated)
    }
}

/// One attribution row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRow {
    /// Stable key (`glyph-atlas`, `fonts`, ...).
    pub name: &'static str,
    /// Bytes charged to this row (`0` when [`Provenance::Unavailable`]).
    pub bytes: u64,
    /// How the figure was obtained.
    pub provenance: Provenance,
}

/// Stable row keys, in report order.
pub const MEMORY_ROW_NAMES: [&str; 6] = [
    "glyph-atlas",
    "fonts",
    "scrollback",
    "lua-vm",
    "surface",
    "thread-stacks",
];

/// Subsystem byte figures the runtime cannot reach on its own.
///
/// The plugin Lua heap lives in the composition root (the plugin runtime is not
/// owned by [`Runtime`]), so an embedder fills it in through
/// [`Runtime::memory_report_with_plugins`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PluginMemoryUsage {
    /// Sum of the live plugin Lua heaps, when every VM could be sampled.
    pub lua_bytes: Option<u64>,
    /// Live plugin VMs sampled.
    pub vms: u32,
    /// Live plugin VMs that could not be sampled (borrowed elsewhere).
    pub busy_vms: u32,
}

impl PluginMemoryUsage {
    /// `true` when at least one VM was sampled and none was skipped.
    #[must_use]
    pub const fn complete(&self) -> bool {
        self.vms > 0 && self.busy_vms == 0
    }
}

/// Byte figures gathered from the runtime, before kernel totals are added.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemorySources {
    /// Glyph cache + atlas texture + block masks.
    pub glyph_atlas_bytes: u64,
    /// `Rss` of font-file mappings (`None` where unavailable).
    pub font_file_bytes: Option<u64>,
    /// Retained terminal content (grids, scrollback, and per-grid stores).
    pub scrollback_bytes: u64,
    /// Plugin Lua heap bytes (`None` when no VM was sampled).
    pub lua_vm_bytes: Option<u64>,
    /// Live plugin VMs sampled.
    pub plugin_vms: u32,
    /// Live plugin VMs skipped by the sample.
    pub plugin_busy_vms: u32,
    /// Estimated resident swap-chain bytes (`None` when no surface exists).
    pub surface_bytes: Option<u64>,
}

/// Process totals read from `/proc` (Linux) or reported unavailable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessMemory {
    /// `VmRSS` in KiB.
    pub rss_kib: u64,
    /// `Pss` in KiB (proportional set size) when the kernel reports it.
    pub pss_kib: Option<u64>,
    /// `Private_Dirty` in KiB when the kernel reports it.
    pub private_dirty_kib: Option<u64>,
    /// Live thread count.
    pub threads: u64,
    /// Where the figures came from (`smaps_rollup`, `status`, `unavailable`).
    pub source: &'static str,
}

impl ProcessMemory {
    /// `true` when a real process sample exists.
    #[must_use]
    pub const fn available(&self) -> bool {
        self.rss_kib != 0
    }

    /// RSS in bytes.
    #[must_use]
    pub const fn rss_bytes(&self) -> u64 {
        self.rss_kib.saturating_mul(1024)
    }
}

/// One bounded attribution report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryReport {
    /// Process totals the rows are charged against.
    pub process: ProcessMemory,
    /// Attribution rows in [`MEMORY_ROW_NAMES`] order.
    pub rows: Vec<MemoryRow>,
    /// Sum of every row whose provenance counts toward RSS.
    pub accounted_bytes: u64,
    /// `rss - accounted`, saturating at zero.
    pub unattributed_bytes: u64,
    /// `true` when the rows exceed RSS (a source double-counted).
    pub over_attributed: bool,
}

/// Per-thread stack reservation plus the main thread's, for `threads` threads.
///
/// `threads == 0` (unavailable sample) yields `None`: a reservation derived
/// from a missing count would be a guess.
#[must_use]
pub fn thread_stack_reservation(threads: u64) -> Option<u64> {
    if threads == 0 {
        return None;
    }
    let spawned = threads.saturating_sub(1);
    Some(
        spawned
            .saturating_mul(SPAWNED_THREAD_STACK_BYTES)
            .saturating_add(MAIN_THREAD_STACK_BYTES),
    )
}

/// Swap-chain images resident for one present mode.
const fn surface_images(present_mode: bitty_render::gpu::PresentMode) -> u64 {
    use bitty_render::gpu::PresentMode;
    match present_mode {
        PresentMode::Mailbox => SURFACE_IMAGES_MAILBOX,
        PresentMode::Fifo => SURFACE_IMAGES_FIFO,
        _ => SURFACE_IMAGES_DEFAULT,
    }
}

/// A row for a source that may be unavailable.
///
/// `None` is [`Provenance::Unavailable`] with zero bytes — "unknown", never a
/// measured zero. This is the one place the rule is applied, so every optional
/// row behaves identically.
fn optional_row(name: &'static str, bytes: Option<u64>, provenance: Provenance) -> MemoryRow {
    match bytes {
        Some(bytes) => MemoryRow {
            name,
            bytes,
            provenance,
        },
        None => MemoryRow {
            name,
            bytes: 0,
            provenance: Provenance::Unavailable,
        },
    }
}

impl MemoryReport {
    /// Builds a report from subsystem sources and process totals.
    ///
    /// Pure: no kernel or runtime access, so the attribution rules stay
    /// unit-testable without a live process.
    #[must_use]
    pub fn from_sources(sources: &MemorySources, process: ProcessMemory) -> Self {
        let mut rows = Vec::with_capacity(MEMORY_ROW_NAMES.len());
        rows.push(MemoryRow {
            name: "glyph-atlas",
            bytes: sources.glyph_atlas_bytes,
            provenance: Provenance::Measured,
        });
        rows.push(optional_row(
            "fonts",
            sources.font_file_bytes,
            Provenance::Measured,
        ));
        rows.push(MemoryRow {
            name: "scrollback",
            bytes: sources.scrollback_bytes,
            provenance: Provenance::Measured,
        });
        rows.push(optional_row(
            "lua-vm",
            sources.lua_vm_bytes,
            Provenance::Measured,
        ));
        rows.push(optional_row(
            "surface",
            sources.surface_bytes,
            Provenance::Estimated,
        ));
        rows.push(optional_row(
            "thread-stacks",
            thread_stack_reservation(process.threads),
            Provenance::Reservation,
        ));

        let accounted_bytes = rows
            .iter()
            .filter(|row| row.provenance.counts_toward_rss())
            .fold(0u64, |total, row| total.saturating_add(row.bytes));
        let rss = process.rss_bytes();
        let over_attributed = accounted_bytes > rss;
        let unattributed_bytes = rss.saturating_sub(accounted_bytes);
        Self {
            process,
            rows,
            accounted_bytes,
            unattributed_bytes,
            over_attributed,
        }
    }

    /// Looks one row up by its stable key.
    #[must_use]
    pub fn row(&self, name: &str) -> Option<&MemoryRow> {
        self.rows.iter().find(|row| row.name == name)
    }

    /// Bytes of one row, or `None` when the row is absent.
    #[must_use]
    pub fn bytes_of(&self, name: &str) -> Option<u64> {
        self.row(name).map(|row| row.bytes)
    }

    /// Unattributed bytes as a whole-percent share of RSS (0 when RSS is 0).
    #[must_use]
    pub fn unattributed_percent(&self) -> u64 {
        let rss = self.process.rss_bytes();
        if rss == 0 {
            return 0;
        }
        self.unattributed_bytes.saturating_mul(100) / rss
    }

    /// Renders the bounded, greppable report.
    ///
    /// One header line carries the process totals (the numbers an owner
    /// baseline quotes), one line per row, then the residual and the notes
    /// that keep a reservation from being read as residency.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);
        let process = self.process;
        let _ = writeln!(out, "bitty memory attribution (bitty#2859)");
        let _ = writeln!(
            out,
            "  pid={} source={} threads={} rss={} KiB pss={} KiB private_dirty={} KiB",
            std::process::id(),
            process.source,
            process.threads,
            process.rss_kib,
            process
                .pss_kib
                .map_or_else(|| String::from("unavailable"), |kib| kib.to_string()),
            process
                .private_dirty_kib
                .map_or_else(|| String::from("unavailable"), |kib| kib.to_string()),
        );
        let _ = writeln!(
            out,
            "  {:<14} {:>14} {:>12} {:>8} {:>7}  provenance",
            "row", "bytes", "KiB", "MiB", "%rss"
        );
        for row in &self.rows {
            let _ = writeln!(
                out,
                "  {:<14} {:>14} {:>12} {:>8.1} {:>7}  {}",
                row.name,
                row.bytes,
                row.bytes / 1024,
                mebibytes(row.bytes),
                share_cell(row, process.rss_bytes()),
                row.provenance.as_str(),
            );
        }
        let _ = writeln!(
            out,
            "  {:<14} {:>14} {:>12} {:>8.1} {:>6.1}%  residual",
            "unattributed",
            self.unattributed_bytes,
            self.unattributed_bytes / 1024,
            mebibytes(self.unattributed_bytes),
            share_percent(self.unattributed_bytes, process.rss_bytes()),
        );
        let _ = writeln!(
            out,
            "  accounted={} B ({:.1}%) unattributed={} B ({}%) over_attributed={}",
            self.accounted_bytes,
            share_percent(self.accounted_bytes, process.rss_bytes()),
            self.unattributed_bytes,
            self.unattributed_percent(),
            self.over_attributed,
        );
        for row in &self.rows {
            if row.provenance == Provenance::Unavailable {
                let _ = writeln!(out, "  note: {} unavailable on this sample", row.name);
            }
        }
        if self
            .rows
            .iter()
            .any(|row| row.provenance == Provenance::Reservation && row.bytes != 0)
        {
            let _ = writeln!(
                out,
                "  note: thread-stacks is a virtual reservation; resident stack pages are inside unattributed"
            );
        }
        if let Some(row) = self.row("surface") {
            if row.provenance == Provenance::Estimated {
                let _ = writeln!(
                    out,
                    "  note: surface = extent x {SURFACE_BYTES_PER_TEXEL} bytes/texel x resident images (Fifo keeps {SURFACE_IMAGES_FIFO})"
                );
            }
        }
        truncate(out, MEMORY_REPORT_MAX_BYTES)
    }

    /// Writes `<dir>/memory-report-<pid>.txt`, bounded and user-only.
    ///
    /// The write is atomic (temp file + rename in the same directory) and the
    /// file is `0600` on Unix, matching the crash/exit report slots: a memory
    /// report is an owner-only diagnostic, never world-readable.
    ///
    /// # Errors
    ///
    /// Propagates directory creation and write failures to the caller, which
    /// owns the policy for a failed diagnostic.
    pub fn write_to_dir(&self, dir: &Path) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("memory-report-{}.txt", std::process::id()));
        let temp = dir.join(format!(
            ".memory-report-{}.{}.tmp",
            std::process::id(),
            monotonic_suffix()
        ));
        let text = self.render();
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        if let Err(error) = std::fs::rename(&temp, &path) {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
        Ok(path)
    }
}

/// Mebibytes with one decimal; integer math keeps the renderer allocation-free
/// besides its own `String`.
fn mebibytes(bytes: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let mib = bytes as f64 / (1024.0 * 1024.0);
    mib
}

/// Whole-tenth-percent share of `total` (0 when `total` is 0).
fn share_percent(part: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let percent = part as f64 * 100.0 / total as f64;
    percent
}

/// `%rss` cell for one row.
///
/// A reservation is a virtual figure and an unavailable row has no figure, so
/// neither gets a share of RSS — printing one would invite the reader to add
/// up a column that does not add up.
fn share_cell(row: &MemoryRow, rss_bytes: u64) -> String {
    if row.provenance.counts_toward_rss() {
        format!("{:.1}%", share_percent(row.bytes, rss_bytes))
    } else {
        String::from("-")
    }
}

/// Unique suffix for the temp file name (pid plus process-local counter).
fn monotonic_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn truncate(mut text: String, max_bytes: usize) -> String {
    const SUFFIX: &str = "\n  ... report truncated";
    if text.len() <= max_bytes {
        return text;
    }
    let mut boundary = max_bytes.saturating_sub(SUFFIX.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    text.push_str(SUFFIX);
    text
}

// ---------------------------------------------------------------------------
// Process sampling
// ---------------------------------------------------------------------------

impl ProcessMemory {
    /// Samples the current process (Linux) or reports it unavailable.
    #[must_use]
    pub fn sample() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::sample_linux()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::default()
        }
    }

    #[cfg(target_os = "linux")]
    fn sample_linux() -> Self {
        let threads = std::fs::read_dir("/proc/self/task")
            .map(|entries| entries.flatten().count())
            .unwrap_or(0);
        let threads = u64::try_from(threads).unwrap_or(u64::MAX);
        if let Some(text) = read_proc_text("/proc/self/smaps_rollup", 64 * 1024) {
            let rss = proc_kib(&text, "Rss:");
            let pss = proc_kib(&text, "Pss:");
            let private_dirty = proc_kib(&text, "Private_Dirty:");
            if let Some(rss) = rss {
                return Self {
                    rss_kib: rss,
                    pss_kib: pss,
                    private_dirty_kib: private_dirty,
                    threads,
                    source: "smaps_rollup",
                };
            }
        }
        // `smaps_rollup` needs a kernel that has it; `VmRSS` is always there
        // and keeps the report useful (without PSS) on older kernels.
        match read_proc_text("/proc/self/status", 64 * 1024)
            .and_then(|text| proc_kib(&text, "VmRSS:"))
        {
            Some(rss) => Self {
                rss_kib: rss,
                pss_kib: None,
                private_dirty_kib: None,
                threads,
                source: "status",
            },
            None => Self {
                threads,
                ..Self::default()
            },
        }
    }
}

/// Reads a `Field:  1234 kB` value from a `/proc` text blob, in KiB.
fn proc_kib(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?;
        rest.split_whitespace().next()?.parse::<u64>().ok()
    })
}

#[cfg(target_os = "linux")]
fn read_proc_text(path: &str, max_bytes: usize) -> Option<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut buffer = Vec::with_capacity(max_bytes.min(4096));
    file.take(u64::try_from(max_bytes).unwrap_or(u64::MAX))
        .read_to_end(&mut buffer)
        .ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// Resident bytes of file-backed font mappings, or `None` where unavailable.
///
/// Font faces are mmap'd by the upstream rasterizer, which exposes no byte
/// accounting. Their file mappings do show up in `/proc/self/smaps`, so the
/// report sums `Rss` for every mapping whose pathname ends in a font
/// extension. That is a real resident figure; faces the rasterizer copied off
/// the mapping stay inside `unattributed` rather than being guessed at.
#[must_use]
pub fn font_file_resident_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let text = read_proc_text("/proc/self/smaps", MAX_PROC_BYTES)?;
        Some(font_file_rss_from_smaps(&text))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Sums `Rss` for font-extension mappings in a captured `smaps` blob.
///
/// Split out so the parsing rules (header line carries the pathname, `Rss:` is
/// per mapping, only file-backed mappings with a font suffix count) are
/// testable without `/proc`.
#[must_use]
pub fn font_file_rss_from_smaps(text: &str) -> u64 {
    let mut total = 0u64;
    let mut counted = 0usize;
    let mut in_font_mapping = false;
    for line in text.lines() {
        // A mapping header is `<start>-<end> <perms> ...` at column zero; a
        // detail line (`Size:`, `Rss:`, ...) has no address range. Matching on
        // the range keeps the two apart without depending on leading spaces.
        if is_mapping_header(line) {
            if counted >= MAX_FONT_MAPPINGS {
                in_font_mapping = false;
                continue;
            }
            in_font_mapping = mapping_is_font_file(line);
            if in_font_mapping {
                counted = counted.saturating_add(1);
            }
            continue;
        }
        if !in_font_mapping {
            continue;
        }
        if let Some(kib) = proc_kib(line.trim_start(), "Rss:") {
            total = total.saturating_add(kib.saturating_mul(1024));
        }
    }
    total
}

/// `true` when `line` is a mapping header (`7f00-7f01 r--p ...`).
fn is_mapping_header(line: &str) -> bool {
    let Some((range, _)) = line.split_once(' ') else {
        return false;
    };
    let Some((start, end)) = range.split_once('-') else {
        return false;
    };
    !start.is_empty()
        && !end.is_empty()
        && start.chars().all(|ch| ch.is_ascii_hexdigit())
        && end.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn mapping_is_font_file(header: &str) -> bool {
    let path = header.split_whitespace().last().unwrap_or_default();
    if !path.starts_with('/') {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    FONT_FILE_EXTENSIONS
        .iter()
        .any(|extension| lower.ends_with(extension))
}

// ---------------------------------------------------------------------------
// Runtime integration
// ---------------------------------------------------------------------------

impl Runtime {
    /// Subsystem byte figures this runtime owns.
    ///
    /// Reads the renderer caches, every live grid's retained heap, and the
    /// surface configuration. Plugin Lua bytes are left `None`: the plugin
    /// runtime is owned by the composition root, which supplies them through
    /// [`Runtime::memory_report_with_plugins`].
    #[must_use]
    pub fn memory_sources(&self) -> MemorySources {
        let scrollback = self
            .pane_sessions
            .values()
            .fold(self.state.retained_heap_bytes(), |total, session| {
                total.saturating_add(session.state.retained_heap_bytes())
            });
        MemorySources {
            glyph_atlas_bytes: self.renderer.retained_bytes(),
            font_file_bytes: font_file_resident_bytes(),
            scrollback_bytes: u64::try_from(scrollback).unwrap_or(u64::MAX),
            lua_vm_bytes: None,
            plugin_vms: 0,
            plugin_busy_vms: 0,
            surface_bytes: surface_resident_bytes(&self.surface),
        }
    }

    /// One attribution report with process totals and no plugin figures.
    #[must_use]
    pub fn memory_report(&self) -> MemoryReport {
        self.memory_report_with_plugins(PluginMemoryUsage::default())
    }

    /// One attribution report, charging the plugin Lua heap the embedder
    /// sampled from its plugin runtime.
    #[must_use]
    pub fn memory_report_with_plugins(&self, plugins: PluginMemoryUsage) -> MemoryReport {
        let mut sources = self.memory_sources();
        sources.lua_vm_bytes = plugins.lua_bytes;
        sources.plugin_vms = plugins.vms;
        sources.plugin_busy_vms = plugins.busy_vms;
        MemoryReport::from_sources(&sources, ProcessMemory::sample())
    }
}

/// Estimated resident swap-chain bytes for one surface.
fn surface_resident_bytes(surface: &bitty_render::gpu::Surface) -> Option<u64> {
    let config = surface.config()?;
    let extent = config.extent;
    let pixels = u64::from(extent.width()).checked_mul(u64::from(extent.height()))?;
    let bytes_per_image = pixels.checked_mul(SURFACE_BYTES_PER_TEXEL)?;
    bytes_per_image.checked_mul(surface_images(config.present_mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(glyph: u64, scrollback: u64) -> MemorySources {
        MemorySources {
            glyph_atlas_bytes: glyph,
            font_file_bytes: Some(1_000),
            scrollback_bytes: scrollback,
            lua_vm_bytes: Some(500),
            plugin_vms: 1,
            plugin_busy_vms: 0,
            surface_bytes: Some(2_000),
        }
    }

    fn process(rss_kib: u64, threads: u64) -> ProcessMemory {
        ProcessMemory {
            rss_kib,
            pss_kib: Some(rss_kib / 2),
            private_dirty_kib: Some(rss_kib / 3),
            threads,
            source: "test",
        }
    }

    #[test]
    fn rows_are_charged_and_the_residual_is_exact() {
        let report = MemoryReport::from_sources(&sources(10_000, 20_000), process(100, 4));
        assert_eq!(report.rows.len(), MEMORY_ROW_NAMES.len());
        assert_eq!(
            report.accounted_bytes,
            10_000 + 1_000 + 20_000 + 500 + 2_000
        );
        assert!(
            report.rows.iter().map(|row| row.name).eq(MEMORY_ROW_NAMES),
            "rows must be in MEMORY_ROW_NAMES order"
        );
        // rss = 100 KiB = 102_400 B, accounted = 33_500 B.
        assert_eq!(report.unattributed_bytes, 102_400 - 33_500);
        assert!(!report.over_attributed);
        assert!(report.render().contains("unattributed"));
    }

    #[test]
    fn reservations_never_enter_the_rss_arithmetic() {
        let report = MemoryReport::from_sources(&sources(1, 1), process(1_000, 4));
        let stacks = report.row("thread-stacks").expect("stack row");
        assert_eq!(stacks.provenance, Provenance::Reservation);
        assert!(stacks.bytes > 1_000 * 1024);
        assert_eq!(
            report.accounted_bytes,
            1 + 1_000 + 1 + 500 + 2_000,
            "the stack reservation must not be added to accounted bytes"
        );
        assert!(report.unattributed_bytes > 0);
        assert!(
            report
                .render()
                .contains("thread-stacks is a virtual reservation"),
            "the report must say the stack figure is a reservation"
        );
    }

    #[test]
    fn unknown_sources_are_unavailable_not_zero_measured() {
        let mut sources = sources(0, 0);
        sources.font_file_bytes = None;
        sources.lua_vm_bytes = None;
        sources.surface_bytes = None;
        let report = MemoryReport::from_sources(&sources, process(10, 1));
        for name in ["fonts", "lua-vm", "surface"] {
            let row = report.row(name).expect("row");
            assert_eq!(row.provenance, Provenance::Unavailable, "{name}");
            assert_eq!(row.bytes, 0);
        }
        assert_eq!(report.accounted_bytes, 0);
        assert_eq!(report.unattributed_bytes, 10 * 1024);
        let text = report.render();
        for name in ["fonts", "lua-vm", "surface"] {
            assert!(text.contains(&format!("note: {name} unavailable")));
        }
    }

    #[test]
    fn an_unavailable_thread_count_yields_no_reservation() {
        let report = MemoryReport::from_sources(&sources(1, 1), process(10, 0));
        assert_eq!(
            report.row("thread-stacks").map(|row| row.provenance),
            Some(Provenance::Unavailable)
        );
        assert!(thread_stack_reservation(0).is_none());
        assert_eq!(
            thread_stack_reservation(1),
            Some(MAIN_THREAD_STACK_BYTES),
            "one thread is the main thread only"
        );
        assert_eq!(
            thread_stack_reservation(2),
            Some(MAIN_THREAD_STACK_BYTES + SPAWNED_THREAD_STACK_BYTES)
        );
    }

    #[test]
    fn over_attribution_is_flagged_and_the_residual_does_not_underflow() {
        let report = MemoryReport::from_sources(&sources(10_000_000, 0), process(1, 1));
        assert!(report.over_attributed);
        assert_eq!(report.unattributed_bytes, 0);
        assert_eq!(report.unattributed_percent(), 0);
    }

    #[test]
    fn a_missing_process_sample_renders_unavailable_totals() {
        let report = MemoryReport::from_sources(&sources(1, 1), ProcessMemory::default());
        assert!(!report.process.available());
        let text = report.render();
        assert!(text.contains("pss=unavailable KiB"));
        assert!(text.contains("private_dirty=unavailable KiB"));
        assert_eq!(report.unattributed_bytes, 0);
    }

    #[test]
    fn smaps_font_attribution_counts_only_font_file_mappings() {
        let smaps = "\
7f00-7f01 r--p 00000000 08:01 42 /usr/share/fonts/truetype/x.ttf
Size:               1024 kB
Rss:                 256 kB
Pss:                 128 kB

7f01-7f02 r-xp 00000000 08:01 43 /usr/lib/libfont-display.so
Size:               2048 kB
Rss:                2048 kB

7f02-7f03 rw-p 00000000 00:00 0
Size:                  4 kB
Rss:                   4 kB

7f03-7f04 r--p 00000000 08:01 44 /usr/share/fonts/opentype/y.OTF
Size:                512 kB
Rss:                  64 kB
";
        assert_eq!(font_file_rss_from_smaps(smaps), (256 + 64) * 1024);
        assert_eq!(font_file_rss_from_smaps(""), 0);
        assert!(mapping_is_font_file("7f-8f r--p 00000000 08:01 1 /x/a.ttc"));
        assert!(!mapping_is_font_file("7f-8f r--p 00000000 08:01 1 [heap]"));
    }

    #[test]
    fn report_render_is_bounded() {
        let mut sources = sources(1, 1);
        sources.font_file_bytes = Some(u64::MAX / 2);
        for _ in 0..200 {
            let report = MemoryReport::from_sources(&sources, process(u64::MAX / 2048, 1_000));
            assert!(report.render().len() <= MEMORY_REPORT_MAX_BYTES);
        }
    }

    #[test]
    fn process_sample_is_available_on_linux() {
        let sample = ProcessMemory::sample();
        #[cfg(target_os = "linux")]
        {
            assert!(sample.available(), "a live process must report VmRSS");
            assert!(sample.threads >= 1);
            assert!(sample.pss_kib.is_some(), "smaps_rollup reports Pss");
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert!(!sample.available());
            assert_eq!(sample.threads, 0);
        }
    }

    #[test]
    fn writing_a_report_lands_a_user_only_bounded_file() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-memory-report-{}-{}",
            std::process::id(),
            monotonic_suffix()
        ));
        let report = MemoryReport::from_sources(&sources(1, 1), process(10, 1));
        let path = report.write_to_dir(&dir).expect("report writes");
        assert!(path.starts_with(&dir));
        let text = std::fs::read_to_string(&path).expect("report reads back");
        assert!(text.contains("bitty memory attribution"));
        assert!(text.len() <= MEMORY_REPORT_MAX_BYTES);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "report is user-only");
        }
        // The temp file must not survive a successful rename.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp file may be left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn swap_chain_estimate_prefers_two_images_for_fifo() {
        use bitty_render::gpu::PresentMode;
        assert_eq!(surface_images(PresentMode::Fifo), SURFACE_IMAGES_FIFO);
        assert_eq!(surface_images(PresentMode::Mailbox), SURFACE_IMAGES_MAILBOX);
        assert_eq!(
            surface_images(PresentMode::Immediate),
            SURFACE_IMAGES_DEFAULT
        );
    }
}
