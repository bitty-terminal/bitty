//! Persistent system font index for the shaped text stack (CTX-1607).
//!
//! Font discovery is the shaped stack's only cold-start cost that scales with
//! the host's font collection: a full `fontdb::Database::load_system_fonts`
//! pass parses every face on every process launch (hundreds of milliseconds
//! on a font-rich seat), which blows the accepted PB-1 startup budget
//! (`p50 <= 100 ms`). This module keeps the parsed face metadata in a small
//! persistent index under `$XDG_CACHE_HOME/bitty/font-index-v2` and
//! reconciles it against a fresh discovery pass on every start.
//!
//! # Design
//!
//! The index is a **parse cache**, not a discovery cache: discovery runs on
//! every start, so new fonts, removed fonts, fontconfig changes, and
//! remove/re-add cycles are visible without deleting the cache.
//!
//! - **Cold build (cache miss):** discover the font file paths, parse them in
//!   parallel with bounded `std::thread::scope` workers, and write the index
//!   atomically (unique temp file + `rename`). The cold build is a one-time
//!   cost; the next process reads the index in a few milliseconds.
//! - **Warm start:** discover the current path set + stats, read the
//!   versioned index (`v2`, with a content hash), keep only cached faces discovery still produces
//!   with an unchanged stat, reparse changed/new paths (and drop foreign,
//!   missing, or stale entries), take generic aliases from the fresh
//!   discovery, and build a metadata-only [`fontdb::Database`]. Face bytes
//!   are not read by the index at startup. The runtime still loads the
//!   primary face to construct its grid renderer; [`super::shaped::SwashSingle`]
//!   loads only the faces the first frames need and keeps dynamic
//!   (missing-glyph) fallback on demand.
//! - **Correctness:** entries carry path, face index, family names, style,
//!   weight, stretch, monospace flag, and the file size/mtime they were
//!   parsed from. Selection metadata is **query-compatible** with fontdb's
//!   own `FaceInfo` for the fields queries read (family name strings, style,
//!   weight, stretch, monospace); it is not byte-identical: PostScript names
//!   and MacRoman-only/localized family entries are reduced, and duplicate
//!   family names are dropped. A corrupt/truncated/version-mismatched index
//!   is discarded and rebuilt; a single changed font only reparses that file.
//! - **Concurrency:** builds run in-process only; writers use a unique temp
//!   file plus an atomic rename, so a reader sees either the previous or the
//!   next complete index. Concurrent processes are last-writer-wins.
//! - **Fail soft:** a missing cache root, read-only cache directory, failed
//!   write, or malformed entry never fails startup; the in-memory index is
//!   used and the full scan remains the final fallback.
//!
//! # Freshness and trust
//!
//! Every start walks the current font directories (measured ~5.5 ms for
//! 1,552 paths) and reconciles the cache against that set. A path the
//! discovery pass does not produce is dropped, even if the cache names it
//! (so a foreign or `/etc/passwd` entry can never reach the shaper). Newly
//! installed and remove/re-added fonts appear on the next start; changed
//! files are detected by size + mtime, the same staleness rule the cache
//! format records. The cache is untrusted for **robustness**, not
//! authenticity: the v2 version, FNV-1a content hash, end marker/count,
//! line/field caps, and bounded read reject accidental or partial
//! corruption (including syntactically valid line mutations), and any
//! failure rebuilds from discovery. It is not a MAC: a same-user attacker
//! who can rewrite both the cache and the discovered font files is outside
//! this check.
//!
//! # PATCHES: `BITTY_FONT_INDEX=0`
//!
//! `fontdb` upstream deliberately rescans system fonts (it has no persistent
//! discovery cache). This module is Bitty's additive cache layer; the
//! `BITTY_FONT_INDEX=0` environment switch restores the upstream full-scan
//! behavior for diagnosis and bypasses both cache reads and writes. Any other
//! value (or unset) enables the cache. This is an in-code escape hatch, not a
//! user-facing configuration key.
//!
//! Discovery scope matches [`fontdb::Database::load_system_fonts`]: fontconfig
//! directories (plus aliases) on Linux/BSD, the platform font directories on
//! macOS/Windows. The fallback for an empty discovery result or an all-reject
//! parse is the upstream full scan, so a cache miss never reduces coverage.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use fontdb::{Database, FaceInfo, Family, ID, Language, Source, Stretch, Style, Weight};

/// Environment switch that restores the upstream full system scan.
///
/// `BITTY_FONT_INDEX=0` disables both index reads and writes for diagnosis.
/// Any other value (or unset) keeps the persistent index enabled.
pub(crate) const INDEX_ENV: &str = "BITTY_FONT_INDEX";

/// Cache file name; the version is repeated in the header.
const INDEX_FILE_NAME: &str = "font-index-v2";

/// On-disk format version.
const INDEX_VERSION: &str = "v2";

/// Refuse to read an index larger than this (hostile/truncated files).
const MAX_INDEX_BYTES: u64 = 32 * 1024 * 1024;

/// Refuse to parse an individual line longer than this.
const MAX_INDEX_LINE_BYTES: usize = 64 * 1024;

/// Bound on retained faces (the reference host has ~1.8k).
const MAX_INDEX_FACES: usize = 16_384;

/// Space reserved for the trailing `end\t<count>\t<hash>` marker.
const END_MARKER_RESERVE: usize = 64;

/// FNV-1a 64-bit offset basis (index integrity hash).
const FNV_OFFSET: u64 = 0xcbf29ce484222325;

/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x100000001b3;

/// Reader/writer bound on one stored font path.
const MAX_INDEX_PATH_BYTES: usize = 4096;

/// Reader/writer bound on one family name (writer truncates to this).
const MAX_INDEX_FAMILY_BYTES: usize = 256;

/// Reader/writer bound on family names per face (writer truncates to this).
const MAX_INDEX_FAMILIES: usize = 32;

/// Reader/writer bound on the PostScript name (writer truncates).
const MAX_INDEX_POSTSCRIPT_BYTES: usize = 256;

/// Bound on files one discovery pass may enqueue.
const MAX_FONT_FILES: usize = 16_384;

/// Refuse to parse a font file larger than this for metadata.
///
/// Face bytes are still bounded by [`super::shaped::MAX_FACE_BYTES`] when a
/// face is actually loaded; the index bound only keeps a hostile multi-GB
/// file from ballooning the cold scan.
const MAX_INDEX_FONT_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum faces accepted from one collection file.
const MAX_FACES_PER_FILE: usize = 256;

/// Maximum table records accepted from one face.
const MAX_TABLES_PER_FACE: usize = 4096;

/// Bound on one metadata table read (`name`/`OS/2`/`post` are tiny).
const MAX_METADATA_TABLE_BYTES: u64 = 16 * 1024 * 1024;

/// Maximum recursion depth while walking font directories.
const MAX_SCAN_DEPTH: usize = 32;

/// Maximum parallel parser workers (bounded host cost).
const MAX_SCAN_WORKERS: usize = 8;

/// Monotonic suffix for temp-file names (in addition to pid + nanos).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One parsed face, as persisted in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexFace {
    /// Font file path (UTF-8; non-UTF-8 paths are skipped by the index).
    pub path: PathBuf,
    /// Face index inside a collection.
    pub face_index: u32,
    /// Family names in fontdb's preference order (English US first).
    pub families: Vec<String>,
    /// PostScript name.
    pub post_script_name: String,
    /// Fontdb style code (`0` normal, `1` italic, `2` oblique).
    pub style: u8,
    /// OS/2 weight class.
    pub weight: u16,
    /// OS/2 width class (`1..=9`).
    pub stretch: u8,
    /// `post` monospace flag.
    pub monospaced: bool,
    /// File size at parse time.
    pub file_size: u64,
    /// File mtime seconds since the Unix epoch (`-1` when unavailable).
    pub mtime_secs: i64,
    /// File mtime nanoseconds (valid when `mtime_secs >= 0`).
    pub mtime_nanos: u32,
}

/// Generic family aliases resolved by fontconfig (or platform defaults).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GenericFamilies {
    /// `Family::Serif` target.
    pub serif: String,
    /// `Family::SansSerif` target.
    pub sans_serif: String,
    /// `Family::Cursive` target.
    pub cursive: String,
    /// `Family::Fantasy` target.
    pub fantasy: String,
    /// `Family::Monospace` target.
    pub monospace: String,
}

impl Default for GenericFamilies {
    fn default() -> Self {
        // `Database::new` carries fontdb's platform-neutral defaults; Linux
        // discovery overlays fontconfig aliases on top exactly like
        // `load_system_fonts`.
        let db = Database::new();
        Self {
            serif: db.family_name(&Family::Serif).to_string(),
            sans_serif: db.family_name(&Family::SansSerif).to_string(),
            cursive: db.family_name(&Family::Cursive).to_string(),
            fantasy: db.family_name(&Family::Fantasy).to_string(),
            monospace: db.family_name(&Family::Monospace).to_string(),
        }
    }
}

impl GenericFamilies {
    /// Applies the aliases to a database.
    fn apply(&self, db: &mut Database) {
        db.set_serif_family(self.serif.clone());
        db.set_sans_serif_family(self.sans_serif.clone());
        db.set_cursive_family(self.cursive.clone());
        db.set_fantasy_family(self.fantasy.clone());
        db.set_monospace_family(self.monospace.clone());
    }

    /// Truncates each alias to the reader/writer family cap.
    fn sanitize(&mut self) {
        for name in [
            &mut self.serif,
            &mut self.sans_serif,
            &mut self.cursive,
            &mut self.fantasy,
            &mut self.monospace,
        ] {
            truncate_utf8(name, MAX_INDEX_FAMILY_BYTES);
        }
    }

    /// Iterates `(kind, name)` pairs in a stable order.
    fn entries(&self) -> [(&'static str, &str); 5] {
        [
            ("serif", &self.serif),
            ("sans-serif", &self.sans_serif),
            ("cursive", &self.cursive),
            ("fantasy", &self.fantasy),
            ("monospace", &self.monospace),
        ]
    }
}

/// A complete parsed index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FontIndex {
    /// Parsed faces, sorted by `(path, face_index)`.
    pub faces: Vec<IndexFace>,
    /// Generic family aliases captured with the face set.
    pub generic: GenericFamilies,
}

/// Paths, stats, and aliases produced by one discovery pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredFonts {
    /// Candidate font files (deduplicated, sorted).
    pub paths: Vec<PathBuf>,
    /// Generic family aliases discovered alongside the paths.
    pub generic: GenericFamilies,
    /// Stat snapshot captured during the walk (one per path).
    stats: HashMap<PathBuf, Stat>,
}

impl DiscoveredFonts {
    fn stat(&self, path: &Path) -> Option<Stat> {
        self.stats.get(path).copied()
    }

    /// Test constructor: stat `paths` directly (no directory walk).
    #[cfg(test)]
    pub(crate) fn from_paths(paths: Vec<PathBuf>, generic: GenericFamilies) -> Self {
        let mut paths = paths;
        paths.sort();
        paths.dedup();
        let stats = paths
            .iter()
            .filter_map(|path| Stat::read(path).map(|stat| (path.clone(), stat)))
            .collect();
        Self {
            paths,
            generic,
            stats,
        }
    }
}

/// Result of one index load + reconciliation.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct LoadOutcome {
    /// Reconciled index.
    pub index: FontIndex,
    /// Number of paths parsed during this load (new + changed).
    pub parsed_paths: usize,
    /// True when the on-disk index was rewritten.
    pub changed: bool,
}

/// One file stat snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stat {
    size: u64,
    mtime_secs: i64,
    mtime_nanos: u32,
}

impl Stat {
    fn read(path: &Path) -> Option<Self> {
        Self::from_metadata(&std::fs::metadata(path).ok()?)
    }

    fn from_metadata(meta: &std::fs::Metadata) -> Option<Self> {
        if !meta.is_file() {
            return None;
        }
        let (mtime_secs, mtime_nanos) = match meta.modified() {
            Ok(modified) => match modified.duration_since(UNIX_EPOCH) {
                Ok(duration) => (
                    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
                    duration.subsec_nanos(),
                ),
                Err(_) => (-1, 0),
            },
            Err(_) => (-1, 0),
        };
        Some(Self {
            size: meta.len(),
            mtime_secs,
            mtime_nanos,
        })
    }

    fn matches(&self, face: &IndexFace) -> bool {
        if self.size != face.file_size {
            return false;
        }
        // `-1` records "mtime unavailable at build time"; only size can be
        // compared then. A real mtime must match exactly.
        face.mtime_secs < 0
            || (self.mtime_secs == face.mtime_secs && self.mtime_nanos == face.mtime_nanos)
    }
}

/// Builds the process-wide database: discovery + index reconciliation.
///
/// `BITTY_FONT_INDEX=0` restores the upstream full scan and bypasses the
/// cache entirely. A cache root that is unavailable also falls back to the
/// full scan; this function never panics on filesystem or metadata errors.
pub(crate) fn system_database() -> Database {
    let index_env = std::env::var_os(INDEX_ENV);
    if cache_disabled(index_env.as_deref()) {
        return full_scan_database();
    }
    if let Some(root) = cache_root() {
        if let Some(index) = load_or_build(&root) {
            return database_from_index_owned(index);
        }
    }
    full_scan_database()
}

/// Upstream behavior: parse every system face (no cache).
pub(crate) fn full_scan_database() -> Database {
    let mut db = Database::new();
    db.load_system_fonts();
    db
}

/// Reads a cache root from the live environment.
fn cache_root() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CACHE_HOME");
    let home = std::env::var_os("HOME");
    #[cfg(windows)]
    let local_app_data = std::env::var_os("LOCALAPPDATA");
    #[cfg(not(windows))]
    let local_app_data: Option<std::ffi::OsString> = None;
    cache_root_with_values(xdg.as_deref(), home.as_deref(), local_app_data.as_deref())
}

/// Pure cache-root resolution (unit-testable; no env reads).
///
/// Precedence: `$XDG_CACHE_HOME/bitty`, `%LOCALAPPDATA%\bitty\cache` on
/// Windows, `$HOME/.cache/bitty`. Empty values are ignored.
fn cache_root_with_values(
    xdg_cache_home: Option<&OsStr>,
    home: Option<&OsStr>,
    local_app_data: Option<&OsStr>,
) -> Option<PathBuf> {
    fn non_empty(value: Option<&OsStr>) -> Option<&Path> {
        value.filter(|value| !value.is_empty()).map(Path::new)
    }
    if let Some(xdg) = non_empty(xdg_cache_home) {
        return Some(xdg.join("bitty"));
    }
    #[cfg(windows)]
    if let Some(local) = non_empty(local_app_data) {
        return Some(local.join("bitty").join("cache"));
    }
    #[cfg(not(windows))]
    let _ = local_app_data;
    non_empty(home).map(|home| home.join(".cache").join("bitty"))
}

/// True when the environment value disables the index (`0`).
fn cache_disabled(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| value == OsStr::new("0"))
}

/// Loads the reconciled index (or rebuilds it).
fn load_or_build(root: &Path) -> Option<FontIndex> {
    load_or_build_with(root, discover_system_fonts).map(|outcome| outcome.index)
}

/// Testable form of [`load_or_build`] with injected discovery.
///
/// Discovery always runs, even on a warm cache: the index caches **parsing**,
/// not the host's font set. New fonts, removed fonts, fontconfig changes, and
/// transient stat failures must be visible on the next start.
fn load_or_build_with<F>(root: &Path, discover: F) -> Option<LoadOutcome>
where
    F: FnOnce() -> DiscoveredFonts,
{
    let discovered = discover();
    let path = root.join(INDEX_FILE_NAME);
    let (mut index, parsed_paths, mut changed) = if discovered.paths.is_empty() {
        // Discovery found nothing (bare host or unsupported target): let the
        // upstream scan decide, and serialize its metadata on success.
        (
            index_from_database(&full_scan_database(), discovered.generic.clone()),
            0,
            true,
        )
    } else {
        match read_index(&path) {
            Some(cached) => {
                let (index, parsed, changed) = reconcile(cached, &discovered);
                (index, parsed, changed)
            }
            None => {
                let (faces, parsed) = build_index_counted(&discovered.paths, &discovered.stats);
                (
                    FontIndex {
                        faces,
                        generic: discovered.generic.clone(),
                    },
                    parsed,
                    true,
                )
            }
        }
    };
    let generic_before = index.generic.clone();
    index.generic.sanitize();
    if index.generic != generic_before {
        changed = true;
    }
    if index.faces.is_empty() {
        // Every candidate was rejected by the metadata parser; the upstream
        // scan is the coverage backstop.
        index = index_from_database(&full_scan_database(), index.generic);
        changed = true;
    }
    if index.faces.is_empty() {
        return None;
    }
    if changed {
        let _ = write_index(&path, &index);
    }
    Some(LoadOutcome {
        index,
        parsed_paths,
        changed,
    })
}

/// Reconciles a cached index with the current discovery set.
///
/// - Faces whose path discovery no longer produces are dropped (this is the
///   foreign-cache boundary: a path the host does not advertise can never
///   survive).
/// - Faces whose stat changed, or whose file disappeared, are dropped and
///   the path is reparsed (when it still exists).
/// - Paths present in discovery but absent from the cache are parsed.
/// - Generic aliases always come from the fresh discovery.
fn reconcile(cached: FontIndex, discovered: &DiscoveredFonts) -> (FontIndex, usize, bool) {
    let discovered_paths: HashSet<&Path> = discovered.paths.iter().map(PathBuf::as_path).collect();
    // Group cached faces by file so a stat mismatch drops the whole file's
    // faces (a TTC reparses as one unit) instead of leaving a partial set.
    let mut by_path: BTreeMap<PathBuf, Vec<IndexFace>> = BTreeMap::new();
    for face in cached.faces {
        by_path.entry(face.path.clone()).or_default().push(face);
    }
    let mut faces = Vec::new();
    let mut kept_paths: HashSet<PathBuf> = HashSet::new();
    let mut changed = false;
    for (path, group) in by_path {
        if !discovered_paths.contains(path.as_path()) {
            changed = true;
            continue;
        }
        let Some(stat) = discovered.stat(&path) else {
            // Discovered but unreadable/missing right now: drop the stale
            // entry; the next start reparses it once the file is back.
            changed = true;
            continue;
        };
        if !group.iter().all(|face| stat.matches(face)) {
            changed = true;
            continue;
        }
        kept_paths.insert(path);
        faces.extend(group);
    }
    let parse_paths: Vec<PathBuf> = discovered
        .paths
        .iter()
        .filter(|path| !kept_paths.contains(*path) && discovered.stat(path).is_some())
        .cloned()
        .collect();
    let (new_faces, parsed_paths) = build_index_counted(&parse_paths, &discovered.stats);
    if !new_faces.is_empty() {
        changed = true;
        faces.extend(new_faces);
    }
    if cached.generic != discovered.generic {
        changed = true;
    }
    // Path order from the BTreeMap and face order from the validated index
    // are already deterministic; sort/dedup defensively after appends.
    sort_and_dedup(&mut faces);
    (
        FontIndex {
            faces,
            generic: discovered.generic.clone(),
        },
        parsed_paths,
        changed,
    )
}

/// Reads and validates an index file.
///
/// Returns `None` when the file is missing, too large, unreadable, has the
/// wrong version, is boundary-truncated (no `end` marker), or is malformed.
fn read_index(path: &Path) -> Option<FontIndex> {
    use std::io::Read as _;
    // Check regular-file status before opening: `File::open` on a FIFO blocks
    // until a writer appears (a hanging startup). `metadata` never opens the
    // node. The bounded read below still guards a racing/size-changing file.
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_INDEX_BYTES {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    // Bounded read: one byte over the limit detects an oversized file without
    // a metadata/read TOCTOU window.
    file.take(MAX_INDEX_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() as u64 > MAX_INDEX_BYTES {
        return None;
    }
    parse_index(&text)
}

/// Converts an index into the metadata-only fontdb database the shaper uses.
///
/// No face bytes are read here; `Source::File` keeps the path so
/// [`super::shaped::SwashSingle::load_font`] can load the selected face and
/// dynamic fallback can load a missing-glyph face on demand.
#[cfg(test)]
pub(crate) fn database_from_index(index: &FontIndex) -> Database {
    database_from_index_owned(index.clone())
}

/// Consuming form used at startup: moves family/path strings into the
/// database instead of cloning every face.
fn database_from_index_owned(index: FontIndex) -> Database {
    let mut db = Database::new();
    index.generic.apply(&mut db);
    for face in index.faces {
        db.push_face_info(FaceInfo {
            id: ID::dummy(),
            source: Source::File(face.path),
            index: face.face_index,
            families: face
                .families
                .into_iter()
                .map(|family| (family, Language::English_UnitedStates))
                .collect(),
            post_script_name: face.post_script_name,
            style: map_style(face.style),
            weight: Weight(face.weight),
            stretch: map_stretch(face.stretch),
            monospaced: face.monospaced,
        });
    }
    db
}

/// Serializes the index to the versioned line format.
fn serialize_index(index: &FontIndex) -> String {
    let mut out = String::new();
    out.push_str(INDEX_VERSION);
    out.push('\n');
    let mut hash = FNV_OFFSET;
    // The writer applies the same caps as the reader, including generic
    // aliases, so a valid discovered name cannot produce a rejected line.
    let mut generic = index.generic.clone();
    generic.sanitize();
    for (kind, name) in generic.entries() {
        let line = format!("generic\t{kind}\t{}", escape_field(name));
        out.push_str(&line);
        out.push('\n');
        hash = fnv_line(hash, &line);
    }
    let mut face_count = 0usize;
    for face in index.faces.iter().take(MAX_INDEX_FACES) {
        let mut face = face.clone();
        if !sanitize_face(&mut face) {
            continue;
        }
        let line = serialize_face(&face);
        if line.len() > MAX_INDEX_LINE_BYTES {
            continue;
        }
        // Keep the whole file under the reader's byte bound, reserving room
        // for the trailing end marker. A truncated-at-the-limit index still
        // carries a valid end marker and is accepted/read as-is.
        let projected = out
            .len()
            .saturating_add(line.len())
            .saturating_add(1)
            .saturating_add(END_MARKER_RESERVE);
        if projected as u64 > MAX_INDEX_BYTES {
            break;
        }
        out.push_str(&line);
        out.push('\n');
        hash = fnv_line(hash, &line);
        face_count += 1;
    }
    // Trailing end marker: count + content hash. A cache truncated at any
    // line boundary, or mutated on a syntactically valid line, is rejected.
    out.push_str(&format!("end\t{face_count}\t{hash}"));
    out.push('\n');
    out
}

/// FNV-1a update over one line plus its newline separator.
fn fnv_line(mut hash: u64, line: &str) -> u64 {
    for byte in line.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash ^= u64::from(b'\n');
    hash.wrapping_mul(FNV_PRIME)
}

/// Serializes one already-sanitized face line (no trailing newline).
fn serialize_face(face: &IndexFace) -> String {
    let mut out = String::new();
    out.push_str("face\t");
    out.push_str(&escape_field(&face.path.to_string_lossy()));
    out.push('\t');
    out.push_str(&face.face_index.to_string());
    out.push('\t');
    out.push_str(&face.file_size.to_string());
    out.push('\t');
    out.push_str(&face.mtime_secs.to_string());
    out.push('\t');
    out.push_str(&face.mtime_nanos.to_string());
    out.push('\t');
    out.push_str(&face.style.to_string());
    out.push('\t');
    out.push_str(&face.weight.to_string());
    out.push('\t');
    out.push_str(&face.stretch.to_string());
    out.push('\t');
    out.push(if face.monospaced { '1' } else { '0' });
    out.push('\t');
    for (index, family) in face.families.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&escape_field(family));
    }
    out.push('\t');
    out.push_str(&escape_field(&face.post_script_name));
    out
}

/// Applies the reader's caps to one face, returning false when it cannot be
/// represented at all (non-UTF-8 or over-long path, empty family list).
fn sanitize_face(face: &mut IndexFace) -> bool {
    let Some(path) = face.path.to_str() else {
        return false;
    };
    if path.is_empty() || path.len() > MAX_INDEX_PATH_BYTES {
        return false;
    }
    // Move (not clone) each family into a small ordered unique list; the
    // per-face name count is tiny, so an O(n^2) scan avoids a HashSet
    // allocation on the warm path.
    let mut unique = Vec::with_capacity(face.families.len().min(MAX_INDEX_FAMILIES));
    for mut family in face.families.drain(..) {
        if family.is_empty() {
            continue;
        }
        truncate_utf8(&mut family, MAX_INDEX_FAMILY_BYTES);
        if !unique.iter().any(|existing| existing == &family) {
            unique.push(family);
            if unique.len() >= MAX_INDEX_FAMILIES {
                break;
            }
        }
    }
    if unique.is_empty() {
        return false;
    }
    face.families = unique;
    truncate_utf8(&mut face.post_script_name, MAX_INDEX_POSTSCRIPT_BYTES);
    true
}

/// Truncates a string to at most `max` bytes on a character boundary.
fn truncate_utf8(value: &mut String, max: usize) {
    if value.len() <= max {
        return;
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

/// Parses the versioned line format.
///
/// Strict on v2: any malformed line, missing end marker, count mismatch, or
/// content-hash mismatch rejects the whole file so the caller rebuilds from
/// discovery rather than trusting a partially corrupted cache.
fn parse_index(text: &str) -> Option<FontIndex> {
    let mut lines = text.lines();
    if lines.next()? != INDEX_VERSION {
        return None;
    }
    let mut generic = GenericFamilies::default();
    let mut faces = Vec::new();
    let mut ended = false;
    let mut hash = FNV_OFFSET;
    for line in lines {
        if line.is_empty() || ended {
            return None;
        }
        if line.len() > MAX_INDEX_LINE_BYTES {
            return None;
        }
        let mut fields = line.split('\t');
        match fields.next()? {
            "generic" => {
                let kind = fields.next()?;
                let name = unescape_field(fields.next()?)?;
                if fields.next().is_some() || name.len() > MAX_INDEX_FAMILY_BYTES {
                    return None;
                }
                match kind {
                    "serif" => generic.serif = name,
                    "sans-serif" => generic.sans_serif = name,
                    "cursive" => generic.cursive = name,
                    "fantasy" => generic.fantasy = name,
                    "monospace" => generic.monospace = name,
                    _ => return None,
                }
            }
            "face" => {
                if faces.len() >= MAX_INDEX_FACES {
                    return None;
                }
                faces.push(parse_face_line(fields)?);
            }
            "end" => {
                let count = fields.next()?.parse::<usize>().ok()?;
                let expected = fields.next()?.parse::<u64>().ok()?;
                if fields.next().is_some()
                    || count != faces.len()
                    || count > MAX_INDEX_FACES
                    || expected != hash
                {
                    return None;
                }
                ended = true;
                continue;
            }
            _ => return None,
        }
        hash = fnv_line(hash, line);
    }
    if !ended {
        return None;
    }
    // Every field was validated against the same caps the writer enforces,
    // so parsed faces are already sanitized; only ordering/dedup remains.
    sort_and_dedup(&mut faces);
    Some(FontIndex { faces, generic })
}

/// Parses one `face` line (the `face` tag is already consumed).
fn parse_face_line<'a>(mut fields: impl Iterator<Item = &'a str>) -> Option<IndexFace> {
    let path = unescape_field(fields.next()?)?;
    if path.is_empty() || path.len() > MAX_INDEX_PATH_BYTES {
        return None;
    }
    let face_index = fields.next()?.parse::<u32>().ok()?;
    let file_size = fields.next()?.parse::<u64>().ok()?;
    let mtime_secs = fields.next()?.parse::<i64>().ok()?;
    let mtime_nanos = fields.next()?.parse::<u32>().ok()?;
    if mtime_nanos >= 1_000_000_000 {
        return None;
    }
    let style = fields.next()?.parse::<u8>().ok()?;
    if style > 2 {
        return None;
    }
    let weight = fields.next()?.parse::<u16>().ok()?;
    let stretch = fields.next()?.parse::<u8>().ok()?;
    if !(1..=9).contains(&stretch) {
        return None;
    }
    let monospaced = match fields.next()? {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    let families_field = fields.next()?;
    let families = families_field
        .split(',')
        .map(unescape_field)
        .collect::<Option<Vec<_>>>()?;
    if families.is_empty()
        || families.len() > MAX_INDEX_FAMILIES
        || families
            .iter()
            .any(|family| family.len() > MAX_INDEX_FAMILY_BYTES)
    {
        return None;
    }
    let post_script_name = unescape_field(fields.next()?)?;
    if post_script_name.len() > MAX_INDEX_POSTSCRIPT_BYTES || fields.next().is_some() {
        return None;
    }
    Some(IndexFace {
        path: PathBuf::from(path),
        face_index,
        families,
        post_script_name,
        style,
        weight,
        stretch,
        monospaced,
        file_size,
        mtime_secs,
        mtime_nanos,
    })
}

/// Atomic index write (temp file in the same directory + rename).
///
/// A missing or read-only cache directory returns an error; callers treat
/// that as "cache unavailable" and keep the in-memory index.
fn write_index(path: &Path, index: &FontIndex) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "index has no parent")
    })?;
    std::fs::create_dir_all(dir)?;
    let suffix = format!(
        "{}.{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let temp = dir.join(format!(".{INDEX_FILE_NAME}.tmp.{suffix}"));
    let text = serialize_index(index);
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        // `std::fs::rename` replaces an existing destination on Unix and on
        // Windows (MoveFileEx with MOVEFILE_REPLACE_EXISTING), so a
        // concurrent reader sees either the old or the new complete file.
        // Concurrent writers are last-writer-wins.
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Sorts and deduplicates by `(path, index)`, then applies the face bound.
fn sort_and_dedup(faces: &mut Vec<IndexFace>) {
    faces.sort_by(|a, b| a.path.cmp(&b.path).then(a.face_index.cmp(&b.face_index)));
    faces.dedup_by(|a, b| a.path == b.path && a.face_index == b.face_index);
    // Keep the cache self-consistent with `parse_index`'s read bound; a host
    // beyond this bound keeps the lexicographically first 16k faces.
    faces.truncate(MAX_INDEX_FACES);
}

/// Applies the writer caps, then sorts and deduplicates by `(path, index)`.
fn normalize_faces(faces: &mut Vec<IndexFace>) {
    faces.retain_mut(sanitize_face);
    sort_and_dedup(faces);
}

/// Builds index entries for `paths` using bounded parallel workers.
///
/// Test convenience wrapper: stats each path and discards the parse count.
#[cfg(test)]
fn build_index(paths: &[PathBuf]) -> Vec<IndexFace> {
    let mut stats = HashMap::new();
    for path in paths {
        if let Some(stat) = Stat::read(path) {
            stats.insert(path.clone(), stat);
        }
    }
    build_index_counted(paths, &stats).0
}

/// Builds index entries and reports how many paths were parsed.
///
/// Paths are sorted before chunking so the result is deterministic regardless
/// of worker scheduling or `read_dir` order. Stats come from the discovery
/// walk, so reconciliation does not stat the same file twice.
fn build_index_counted(
    paths: &[PathBuf],
    stats: &HashMap<PathBuf, Stat>,
) -> (Vec<IndexFace>, usize) {
    let mut paths = paths.to_vec();
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return (Vec::new(), 0);
    }
    let workers = scan_workers(paths.len());
    let (mut faces, parsed_paths) = if workers <= 1 {
        let mut faces = Vec::new();
        for path in &paths {
            faces.extend(parse_font(path, stats.get(path).copied()));
        }
        (faces, paths.len())
    } else {
        let chunk = paths.len().div_ceil(workers);
        let parts = std::thread::scope(|scope| {
            let handles: Vec<_> = paths
                .chunks(chunk)
                .map(|chunk| {
                    scope.spawn(move || {
                        let mut faces = Vec::new();
                        for path in chunk {
                            faces.extend(parse_font(path, stats.get(path).copied()));
                        }
                        (faces, chunk.len())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap_or_default())
                .collect::<Vec<(Vec<IndexFace>, usize)>>()
        });
        let mut faces = Vec::new();
        let mut parsed = 0usize;
        for (part, count) in parts {
            faces.extend(part);
            parsed = parsed.saturating_add(count);
        }
        (faces, parsed)
    };
    normalize_faces(&mut faces);
    (faces, parsed_paths)
}

/// Worker count for one scan (bounded, and no threads for tiny inputs).
fn scan_workers(path_count: usize) -> usize {
    if path_count < 4 {
        return 1;
    }
    std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .clamp(1, MAX_SCAN_WORKERS)
}

/// Parses every face in one font file.
///
/// Reads only the SFNT table directory and the `name`, `OS/2`, and `post`
/// tables instead of loading the whole font: outline/GSUB/GPOS/variation
/// data dominates font size and is irrelevant to face selection. This keeps
/// the cold scan both fast and memory-bounded (a few hundred KiB per worker
/// instead of hundreds of MiB for a full-file read).
fn parse_font(path: &Path, stat: Option<Stat>) -> Vec<IndexFace> {
    // The line format is UTF-8; a non-UTF-8 path would not survive the
    // round trip and would look stale on every launch. Such paths are rare
    // and remain reachable through `BITTY_FONT_INDEX=0`/the full scan.
    if path.to_str().is_none() {
        return Vec::new();
    }
    let Some(stat) = stat.or_else(|| Stat::read(path)) else {
        return Vec::new();
    };
    if stat.size > MAX_INDEX_FONT_BYTES {
        return Vec::new();
    }
    let faces = read_faces(path, &stat);
    if faces.is_empty() {
        // Unusual encodings (MacRoman-only names, synthetic table layouts)
        // fall back to fontdb's parser for this file so coverage matches the
        // full scan.
        parse_with_fontdb(path, stat)
    } else {
        faces
    }
}

/// Reads all faces' selection metadata with bounded table-range reads.
fn read_faces(path: &Path, stat: &Stat) -> Vec<IndexFace> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(header) = read_at(&mut file, 0, 12, stat.size) else {
        return Vec::new();
    };
    let face_offsets = if header.get(0..4) == Some(b"ttcf") {
        let Some(count) = header
            .get(8..12)
            .and_then(be_u32)
            .map(|value| value as usize)
        else {
            return Vec::new();
        };
        if count == 0 || count > MAX_FACES_PER_FILE {
            return Vec::new();
        }
        let Some(offset_bytes) = count
            .checked_mul(4)
            .and_then(|len| read_at(&mut file, 12, len, stat.size).ok())
        else {
            return Vec::new();
        };
        let mut offsets = Vec::with_capacity(count);
        for chunk in offset_bytes.chunks_exact(4) {
            let Some(offset) = be_u32(chunk) else {
                return Vec::new();
            };
            offsets.push(u64::from(offset));
        }
        offsets
    } else {
        vec![0u64]
    };
    face_offsets
        .iter()
        .enumerate()
        .filter_map(|(face_index, offset)| {
            read_face(&mut file, *offset, path, face_index as u32, stat)
        })
        .collect()
}

/// Reads one face's table directory and required tables.
fn read_face(
    file: &mut std::fs::File,
    face_offset: u64,
    path: &Path,
    face_index: u32,
    stat: &Stat,
) -> Option<IndexFace> {
    let header = read_at(file, face_offset, 12, stat.size).ok()?;
    let table_count = usize::from(be_u16(header.get(4..6)?)?);
    if table_count == 0 || table_count > MAX_TABLES_PER_FACE {
        return None;
    }
    let directory_len = table_count.checked_mul(16)?;
    let directory = read_at(file, face_offset.checked_add(12)?, directory_len, stat.size).ok()?;
    let mut name = None;
    let mut os2 = None;
    let mut post = None;
    for record in directory.chunks_exact(16) {
        let tag = record.get(0..4)?;
        let offset = be_u32(record.get(8..12)?)?;
        let length = be_u32(record.get(12..16)?)?;
        let table = match tag {
            b"name" => &mut name,
            b"OS/2" => &mut os2,
            b"post" => &mut post,
            _ => continue,
        };
        if table.is_some() || length == 0 || u64::from(length) > MAX_METADATA_TABLE_BYTES {
            continue;
        }
        let bytes = read_at(
            file,
            u64::from(offset),
            usize::try_from(length).ok()?,
            stat.size,
        )
        .ok()?;
        *table = Some(bytes);
    }
    let name_table = ttf_parser::name::Table::parse(name.as_deref()?)?;
    let families = family_names(name_table.names)?;
    let post_script_name = post_script_name(name_table.names)?;
    let os2 = os2.as_deref().and_then(ttf_parser::os2::Table::parse);
    let post = post.as_deref().and_then(ttf_parser::post::Table::parse);
    let style = match os2.map(|table| table.style()) {
        Some(ttf_parser::Style::Italic) => 1,
        Some(ttf_parser::Style::Oblique) => 2,
        _ => {
            let italic = post.is_some_and(|table| table.italic_angle != 0.0);
            u8::from(italic)
        }
    };
    let weight = os2.map(|table| table.weight().to_number()).unwrap_or(400);
    let stretch = os2
        .map(|table| stretch_code(table.width().to_number()))
        .unwrap_or(5);
    let monospaced = post.is_some_and(|table| table.is_monospaced);
    Some(IndexFace {
        path: path.to_path_buf(),
        face_index,
        families,
        post_script_name,
        style,
        weight,
        stretch,
        monospaced,
        file_size: stat.size,
        mtime_secs: stat.mtime_secs,
        mtime_nanos: stat.mtime_nanos,
    })
}

/// Bounded positional read (`offset + len` must stay inside the file).
fn read_at(
    file: &mut std::fs::File,
    offset: u64,
    len: usize,
    file_size: u64,
) -> std::io::Result<Vec<u8>> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let end = offset
        .checked_add(u64::try_from(len).unwrap_or(u64::MAX))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "range overflow"))?;
    if end > file_size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "font table range past file end",
        ));
    }
    let mut buffer = vec![0u8; len];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

/// Big-endian u16 from a two-byte slice.
fn be_u16(bytes: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(..2)?.try_into().ok()?))
}

/// Big-endian u32 from a four-byte slice.
fn be_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?))
}

/// Fontdb-parser fallback for files ttf-parser cannot name.
fn parse_with_fontdb(path: &Path, stat: Stat) -> Vec<IndexFace> {
    let mut db = Database::new();
    if db.load_font_file(path).is_err() {
        return Vec::new();
    }
    db.faces()
        .filter_map(|face| {
            let Source::File(source_path) = &face.source else {
                return None;
            };
            Some(IndexFace {
                path: source_path.clone(),
                face_index: face.index,
                families: face
                    .families
                    .iter()
                    .map(|(family, _)| family.clone())
                    .collect(),
                post_script_name: face.post_script_name.clone(),
                style: fontdb_style_code(face.style),
                weight: face.weight.0,
                stretch: stretch_code(face.stretch.to_number()),
                monospaced: face.monospaced,
                file_size: stat.size,
                mtime_secs: stat.mtime_secs,
                mtime_nanos: stat.mtime_nanos,
            })
        })
        .collect()
}

/// Family names in fontdb's order: typographic family, then family, English
/// US first, deduplicated.
fn family_names(names: ttf_parser::name::Names<'_>) -> Option<Vec<String>> {
    for name_id in [
        ttf_parser::name_id::TYPOGRAPHIC_FAMILY,
        ttf_parser::name_id::FAMILY,
    ] {
        let mut collected: Vec<(String, bool)> = Vec::new();
        for name in names {
            if name.name_id != name_id {
                continue;
            }
            let Some(text) = name.to_string() else {
                continue;
            };
            let english = name.language() == ttf_parser::Language::English_UnitedStates;
            collected.push((text, english));
        }
        if !collected.is_empty() {
            // Stable sort keeps the original order within each language
            // group; English US moves to the front like fontdb.
            collected.sort_by_key(|(_, english)| !*english);
            let mut out: Vec<String> = Vec::new();
            for (name, _) in collected {
                if !out.contains(&name) {
                    out.push(name);
                }
            }
            return Some(out);
        }
    }
    None
}

/// PostScript name from the `name` table.
fn post_script_name(names: ttf_parser::name::Names<'_>) -> Option<String> {
    names
        .into_iter()
        .find(|name| name.name_id == ttf_parser::name_id::POST_SCRIPT_NAME && name.is_unicode())
        .and_then(|name| name.to_string())
}

/// Clamps the OS/2 width class to the index's `1..=9` range.
fn stretch_code(width: u16) -> u8 {
    u8::try_from(width.clamp(1, 9)).unwrap_or(5)
}

/// Maps a fontdb style to the index code.
fn fontdb_style_code(style: Style) -> u8 {
    match style {
        Style::Normal => 0,
        Style::Italic => 1,
        Style::Oblique => 2,
    }
}

/// Maps an index style code to fontdb's style.
fn map_style(code: u8) -> Style {
    match code {
        1 => Style::Italic,
        2 => Style::Oblique,
        _ => Style::Normal,
    }
}

/// Maps an index width class to fontdb's stretch enum.
fn map_stretch(code: u8) -> Stretch {
    match code {
        1 => Stretch::UltraCondensed,
        2 => Stretch::ExtraCondensed,
        3 => Stretch::Condensed,
        4 => Stretch::SemiCondensed,
        6 => Stretch::SemiExpanded,
        7 => Stretch::Expanded,
        8 => Stretch::ExtraExpanded,
        9 => Stretch::UltraExpanded,
        _ => Stretch::Normal,
    }
}

/// Serializes metadata from a full-scan database (coverage fallback).
fn index_from_database(db: &Database, generic: GenericFamilies) -> FontIndex {
    let mut faces = Vec::new();
    for face in db.faces() {
        let Source::File(path) = &face.source else {
            continue;
        };
        if path.to_str().is_none() {
            continue;
        }
        let Some(stat) = Stat::read(path) else {
            continue;
        };
        faces.push(IndexFace {
            path: path.clone(),
            face_index: face.index,
            families: face
                .families
                .iter()
                .map(|(family, _)| family.clone())
                .collect(),
            post_script_name: face.post_script_name.clone(),
            style: fontdb_style_code(face.style),
            weight: face.weight.0,
            stretch: stretch_code(face.stretch.to_number()),
            monospaced: face.monospaced,
            file_size: stat.size,
            mtime_secs: stat.mtime_secs,
            mtime_nanos: stat.mtime_nanos,
        });
    }
    normalize_faces(&mut faces);
    FontIndex { faces, generic }
}

/// Discovers system font files and generic aliases.
pub(crate) fn discover_system_fonts() -> DiscoveredFonts {
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    ))]
    {
        let home = std::env::var("HOME").ok();
        let (dirs, generic) = fontconfig_discovery().unwrap_or_else(|| {
            (
                fontconfig_fallback_dirs(home.as_deref()),
                GenericFamilies::default(),
            )
        });
        let (paths, stats) = collect_font_files(&dirs);
        return DiscoveredFonts {
            paths,
            generic,
            stats,
        };
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "windows"))]
    {
        let (paths, stats) = collect_font_files(&platform_font_dirs());
        return DiscoveredFonts {
            paths,
            generic: GenericFamilies::default(),
            stats,
        };
    }

    #[allow(unreachable_code)]
    DiscoveredFonts {
        paths: Vec::new(),
        generic: GenericFamilies::default(),
        stats: HashMap::new(),
    }
}

/// Linux/BSD discovery through fontconfig, matching fontdb's parser setup.
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
))]
fn fontconfig_discovery() -> Option<(Vec<PathBuf>, GenericFamilies)> {
    let config_file = std::env::var_os("FONTCONFIG_FILE").map(PathBuf::from);
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var("HOME").ok();
    fontconfig_discovery_from(
        config_file.as_deref(),
        xdg_config_home.as_deref(),
        home.as_deref(),
    )
}

/// Pure fontconfig discovery (testable without mutating process env).
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
))]
fn fontconfig_discovery_from(
    config_file: Option<&Path>,
    xdg_config_home: Option<&Path>,
    home: Option<&str>,
) -> Option<(Vec<PathBuf>, GenericFamilies)> {
    let mut fontconfig = fontconfig_parser::FontConfig::default();
    if let Some(config_file) = config_file {
        let _ = fontconfig.merge_config(config_file);
    } else {
        let xdg_config_home = xdg_config_home
            .map(Path::to_path_buf)
            .or_else(|| home.map(|home| Path::new(home).join(".config")));
        let read_global = match xdg_config_home {
            Some(path) => fontconfig
                .merge_config(&path.join("fontconfig/fonts.conf"))
                .is_err(),
            None => true,
        };
        if read_global {
            let _ = fontconfig.merge_config(Path::new("/etc/fonts/local.conf"));
        }
        let _ = fontconfig.merge_config(Path::new("/etc/fonts/fonts.conf"));
    }

    let generic = generic_from_aliases(fontconfig.aliases, GenericFamilies::default());
    let dirs = fontconfig
        .dirs
        .into_iter()
        .filter_map(|dir| expand_home(dir.path, home))
        .collect::<Vec<_>>();
    if dirs.is_empty() {
        // Keep the parsed aliases even when no <dir> entry was configured;
        // fontdb's full scan sets aliases before falling back to known dirs.
        return Some((fontconfig_fallback_dirs(home), generic));
    }
    Some((dirs, generic))
}

/// Expands a leading `~`/`$HOME` in a discovered directory.
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
))]
fn expand_home(path: PathBuf, home: Option<&str>) -> Option<PathBuf> {
    if let Ok(stripped) = path.strip_prefix("~") {
        let home = home?;
        return Some(Path::new(home).join(stripped));
    }
    Some(path)
}

/// fontdb's fallback directory list when fontconfig yields no directories.
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
))]
fn fontconfig_fallback_dirs(home: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
    ];
    if let Some(home) = home {
        let home = Path::new(home);
        dirs.push(home.join(".fonts"));
        dirs.push(home.join(".local/share/fonts"));
    }
    dirs
}

/// Applies fontconfig aliases to the generic family set.
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
))]
fn generic_from_aliases(
    aliases: Vec<fontconfig_parser::Alias>,
    mut generic: GenericFamilies,
) -> GenericFamilies {
    for alias in aliases {
        let name = alias
            .prefer
            .first()
            .or_else(|| alias.accept.first())
            .or_else(|| alias.default.first());
        let Some(name) = name else {
            continue;
        };
        match alias.alias.to_lowercase().as_str() {
            "serif" => generic.serif = name.clone(),
            "sans-serif" | "sans serif" => generic.sans_serif = name.clone(),
            "monospace" => generic.monospace = name.clone(),
            "cursive" => generic.cursive = name.clone(),
            "fantasy" => generic.fantasy = name.clone(),
            _ => {}
        }
    }
    generic
}

/// Platform font directories, matching fontdb's `load_system_fonts`.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "windows"))]
fn platform_font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Some(system_root) = std::env::var_os("SYSTEMROOT") {
            dirs.push(PathBuf::from(system_root).join("Fonts"));
        } else {
            dirs.push(PathBuf::from("C:\\Windows\\Fonts"));
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            let home = PathBuf::from(home);
            dirs.push(home.join("AppData/Local/Microsoft/Windows/Fonts"));
            dirs.push(home.join("AppData/Roaming/Microsoft/Windows/Fonts"));
        }
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        dirs.push(PathBuf::from("/Library/Fonts"));
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        if let Ok(entries) = std::fs::read_dir("/System/Library/AssetsV2") {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("com_apple_MobileAsset_Font")
                {
                    dirs.push(entry.path());
                }
            }
        }
        dirs.push(PathBuf::from("/Network/Library/Fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Fonts"));
        }
    }
    dirs
}

/// Recursively collects supported font files from `dirs`.
///
/// Mirrors fontdb's walk: recursive, follows symlinks, skips duplicate
/// canonical paths, and accepts `ttf`/`ttc`/`otf`/`otc` (either case).
fn collect_font_files(dirs: &[PathBuf]) -> (Vec<PathBuf>, HashMap<PathBuf, Stat>) {
    let mut files = Vec::new();
    let mut stats = HashMap::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    let mut seen_files: HashSet<PathBuf> = HashSet::new();
    for dir in dirs {
        walk_font_dir(
            dir,
            0,
            &mut seen_dirs,
            &mut seen_files,
            &mut files,
            &mut stats,
        );
        if files.len() >= MAX_FONT_FILES {
            break;
        }
    }
    files.sort();
    files.dedup();
    files.truncate(MAX_FONT_FILES);
    (files, stats)
}

/// One recursive directory step.
fn walk_font_dir(
    dir: &Path,
    depth: usize,
    seen_dirs: &mut HashSet<PathBuf>,
    seen_files: &mut HashSet<PathBuf>,
    files: &mut Vec<PathBuf>,
    stats: &mut HashMap<PathBuf, Stat>,
) {
    if depth > MAX_SCAN_DEPTH || files.len() >= MAX_FONT_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= MAX_FONT_FILES {
            return;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if seen_dirs.insert(key) {
                walk_font_dir(&path, depth + 1, seen_dirs, seen_files, files, stats);
            }
            continue;
        }
        if !meta.is_file() || !is_font_extension(&path) {
            continue;
        }
        // Keep the walked path for regular files (fontdb does the same) and
        // the resolved path for symlinks (fontdb canonicalizes those).
        // Canonicalizing every regular file would add ~1.6k syscalls to the
        // warm discovery pass for no coverage gain.
        let stored = if file_type.is_symlink() {
            std::fs::canonicalize(&path).unwrap_or(path)
        } else {
            path
        };
        if !seen_files.insert(stored.clone()) {
            continue;
        }
        let Some(stat) = Stat::from_metadata(&meta) else {
            continue;
        };
        stats.insert(stored.clone(), stat);
        files.push(stored);
    }
}

/// True for the extensions fontdb loads.
fn is_font_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("ttf" | "ttc" | "TTF" | "TTC" | "otf" | "otc" | "OTF" | "OTC")
    )
}

/// Percent-encodes a field for the tab-separated index.
///
/// `%`, tab, carriage return, newline, comma, and every byte outside printable
/// ASCII are encoded; spaces stay readable. This keeps family lists
/// comma-separated and the line/field split unambiguous.
fn escape_field(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let encode = byte == b'%'
            || byte == b'\t'
            || byte == b'\r'
            || byte == b'\n'
            || byte == b','
            || !(0x20..=0x7E).contains(&byte);
        if encode {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        } else {
            out.push(char::from(byte));
        }
    }
    out
}

/// Decodes a percent-encoded field.
fn unescape_field(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut iter = value.bytes();
    while let Some(byte) = iter.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let high = hex_value(iter.next()?)?;
        let low = hex_value(iter.next()?)?;
        bytes.push((high << 4) | low);
    }
    String::from_utf8(bytes).ok()
}

/// Decodes one ASCII hex digit.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Normalizes a face handle to `(file path, face index)` for parity checks.
#[cfg(test)]
fn file_source(db: &Database, id: Option<ID>) -> Option<(PathBuf, u32)> {
    match db.face_source(id?)? {
        (Source::File(path), index) => Some((path, index)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::GlyphRasterizer as _;
    use std::cell::Cell;

    // Minimal TrueType fixture with a real name/OS2/post table: family
    // "Bitty Index Test", PostScript "BittyIndexTest-Regular", monospaced.
    const TEST_TTF_HEX: &str = "00010000000a0080000300204f532f32452144350000012800000060636d6170000c00940000019000000034676c79665bd156ce000001cc0000001a686561642fda7856000000ac0000003668686561051601f6000000e400000024686d74780258000000000188000000066c6f6361000d0000000001c4000000066d6178700004000600000108000000206e616d65aad4e986000001e80000018c706f737400280001000003740000002600010000000100009eee9ba15f0f3cf5000103e800000000e7051a5d00000000e7051a5d00640000025802bc000000030002000000000000000100000320ff38000002580000006401f40001000000000000000000000000000000010001000000020004000100000000000200000000000000000000000000000000000302580190000500040000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000003f3f3f3f0000004100410320ff380000032000c80000000000000000000000000000002000000258000000000000000000020000000300000014000300010000001400040020000000040004000100000041ffff00000041ffffffc000010000000000000000000d0000000100640000025802bc00030000331121116401f402bcfd440000000000000c009600010000000000010010000000010000000000020007001000010000000000030016001700010000000000040018002d0001000000000005000d00450001000000000006001600170003000104090001002000520003000104090002000e00720003000104090003002c00800003000104090004003000ac0003000104090005001a00dc0003000104090006002c0080426974747920496e6465782054657374526567756c61724269747479496e646578546573742d526567756c6172426974747920496e646578205465737420526567756c617256657273696f6e20312e3030300042006900740074007900200049006e006400650078002000540065007300740052006500670075006c00610072004200690074007400790049006e0064006500780054006500730074002d0052006500670075006c006100720042006900740074007900200049006e0064006500780020005400650073007400200052006500670075006c0061007200560065007200730069006f006e00200031002e00300030003000020000000000000000000000000001000000000000000000000000000000000002000000240000";

    fn test_font_bytes() -> Vec<u8> {
        TEST_TTF_HEX
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bitty-font-index-{tag}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    fn write_font(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, test_font_bytes()).expect("write test font");
        path
    }

    fn fixture_faces(paths: &[PathBuf]) -> Vec<IndexFace> {
        build_index(paths)
    }

    fn discovered(paths: Vec<PathBuf>) -> DiscoveredFonts {
        DiscoveredFonts::from_paths(paths, GenericFamilies::default())
    }

    fn scan_dir(dir: &Path) -> DiscoveredFonts {
        let (paths, stats) = collect_font_files(std::slice::from_ref(&dir.to_path_buf()));
        DiscoveredFonts {
            paths,
            generic: GenericFamilies::default(),
            stats,
        }
    }

    fn face_paths(outcome: &LoadOutcome) -> Vec<PathBuf> {
        outcome
            .index
            .faces
            .iter()
            .map(|face| face.path.clone())
            .collect()
    }

    #[test]
    fn parses_fixture_metadata_like_fontdb() {
        let dir = temp_dir("fixture");
        let path = write_font(&dir, "bitty-index-test.ttf");
        let faces = fixture_faces(std::slice::from_ref(&path));
        assert_eq!(faces.len(), 1, "{faces:?}");
        let face = &faces[0];
        assert_eq!(face.families, vec!["Bitty Index Test".to_string()]);
        assert_eq!(face.post_script_name, "BittyIndexTest-Regular");
        assert!(face.monospaced);
        assert_eq!(face.style, 0);
        assert_eq!(face.weight, 400);
        assert_eq!(face.stretch, 5);
        assert_eq!(face.file_size, test_font_bytes().len() as u64);
        let mut db = Database::new();
        db.load_font_file(&path).expect("fontdb parses fixture");
        let expected = db.faces().next().expect("fontdb face");
        assert_eq!(face.families[0], expected.families[0].0);
        assert_eq!(face.post_script_name, expected.post_script_name);
        assert_eq!(face.monospaced, expected.monospaced);
        assert_eq!(face.weight, expected.weight.0);
        assert_eq!(face.style, fontdb_style_code(expected.style));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_round_trip_preserves_fields() {
        let dir = temp_dir("roundtrip");
        let path = write_font(&dir, "a.ttf");
        let faces = fixture_faces(&[path]);
        let index = FontIndex {
            faces,
            generic: GenericFamilies {
                monospace: "Bitty Index Test".to_string(),
                ..GenericFamilies::default()
            },
        };
        let text = serialize_index(&index);
        assert!(text.starts_with("v2\n"));
        assert!(text.contains("end\t1\t"), "end marker: {text}");
        let parsed = parse_index(&text).expect("round trip");
        assert_eq!(parsed, index);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn escapes_tabs_commas_percent_and_unicode() {
        let family = "Fam\tily,100% \u{2713}";
        let escaped = escape_field(family);
        assert_eq!(unescape_field(&escaped).as_deref(), Some(family));
        assert!(!escaped.contains('\t'));
        assert!(!escaped.contains(','));
        let index = FontIndex {
            faces: vec![IndexFace {
                path: PathBuf::from("/tmp/font with space.ttf"),
                face_index: 0,
                families: vec![family.to_string()],
                post_script_name: "PS-Name".to_string(),
                style: 1,
                weight: 700,
                stretch: 3,
                monospaced: false,
                file_size: 42,
                mtime_secs: 1_700_000_000,
                mtime_nanos: 123,
            }],
            generic: GenericFamilies::default(),
        };
        let parsed = parse_index(&serialize_index(&index)).expect("escaped round trip");
        assert_eq!(parsed, index);
    }

    #[test]
    fn cache_path_honours_xdg_then_home() {
        let xdg = cache_root_with_values(
            Some(OsStr::new("/tmp/xdg-cache")),
            Some(OsStr::new("/home/user")),
            None,
        );
        assert_eq!(xdg, Some(PathBuf::from("/tmp/xdg-cache/bitty")));
        let home = cache_root_with_values(None, Some(OsStr::new("/home/user")), None);
        assert_eq!(home, Some(PathBuf::from("/home/user/.cache/bitty")));
        assert_eq!(cache_root_with_values(None, None, None), None);
        assert_eq!(
            cache_root_with_values(Some(OsStr::new("")), Some(OsStr::new("/home/u")), None),
            Some(PathBuf::from("/home/u/.cache/bitty")),
            "empty XDG value falls back"
        );
    }

    #[test]
    fn bitty_font_index_zero_disables_the_cache() {
        assert!(cache_disabled(Some(OsStr::new("0"))));
        assert!(!cache_disabled(None));
        assert!(!cache_disabled(Some(OsStr::new("1"))));
        assert!(!cache_disabled(Some(OsStr::new(""))));
    }

    #[test]
    fn corrupt_or_version_mismatched_index_is_rejected() {
        assert!(parse_index("garbage\n").is_none());
        assert!(
            parse_index("v1\nend\t0\t0\n").is_none(),
            "old format rejects"
        );
        assert!(parse_index("v2\n").is_none());
        let good = serialize_index(&FontIndex {
            faces: Vec::new(),
            generic: GenericFamilies::default(),
        });
        assert!(parse_index(&good).is_some());
        assert!(
            parse_index("v2\nface\tbroken\n").is_none(),
            "truncated face line must reject the cache"
        );
        assert!(
            parse_index("v2\nend\t1\t0\n").is_none(),
            "end count must match the face count"
        );
        assert!(
            parse_index("v2\nend\t0\t0\nextra\n").is_none(),
            "lines after the end marker must reject the cache"
        );
    }

    #[test]
    fn boundary_truncated_index_is_rejected() {
        let dir = temp_dir("boundary");
        let path = write_font(&dir, "a.ttf");
        let index = FontIndex {
            faces: fixture_faces(&[path]),
            generic: GenericFamilies::default(),
        };
        let text = serialize_index(&index);
        let mut cut = 0;
        for line in text.split_inclusive('\n') {
            cut += line.len();
            if cut < text.len() {
                assert!(
                    parse_index(&text[..cut]).is_none(),
                    "truncation after {cut} bytes must reject the cache"
                );
            }
        }
        // Mid-line cuts too (the escaped format is ASCII, so slicing is safe).
        for cut in (1..text.len()).step_by(5) {
            assert!(
                parse_index(&text[..cut]).is_none(),
                "mid-line truncation at {cut} must reject the cache"
            );
        }
        assert!(parse_index(&text).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_entry_rebuilds_only_the_changed_path() {
        let dir = temp_dir("stale");
        let changed = write_font(&dir, "changed.ttf");
        let stable = write_font(&dir, "stable.ttf");
        let root = dir.join("cache");
        let discovery_calls = Cell::new(0);
        let first = load_or_build_with(&root, || {
            discovery_calls.set(discovery_calls.get() + 1);
            discovered(vec![changed.clone(), stable.clone()])
        })
        .expect("cold build");
        assert_eq!(first.index.faces.len(), 2);
        assert_eq!(first.parsed_paths, 2);
        assert_eq!(discovery_calls.get(), 1);

        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::open(&changed)
            .expect("open changed")
            .set_modified(future)
            .expect("set mtime");
        let second = load_or_build_with(&root, || {
            discovery_calls.set(discovery_calls.get() + 1);
            discovered(vec![changed.clone(), stable.clone()])
        })
        .expect("warm read");
        assert_eq!(discovery_calls.get(), 2, "discovery must run every start");
        assert_eq!(second.index.faces.len(), 2);
        assert_eq!(second.parsed_paths, 1, "only the changed path reparses");
        let changed_face = second
            .index
            .faces
            .iter()
            .find(|face| face.path == changed)
            .expect("changed entry");
        assert_eq!(
            changed_face.mtime_secs,
            future.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
        );
        let stable_face = second
            .index
            .faces
            .iter()
            .find(|face| face.path == stable)
            .expect("stable entry");
        assert_eq!(
            stable_face.mtime_secs,
            first
                .index
                .faces
                .iter()
                .find(|face| face.path == stable)
                .unwrap()
                .mtime_secs
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_dropped_and_restored_on_the_next_start() {
        let dir = temp_dir("missing");
        let kept = write_font(&dir, "kept.ttf");
        let removed = write_font(&dir, "removed.ttf");
        let root = dir.join("cache");
        let discovery_calls = Cell::new(0);
        let first = load_or_build_with(&root, || {
            discovery_calls.set(discovery_calls.get() + 1);
            discovered(vec![kept.clone(), removed.clone()])
        })
        .expect("cold build");
        assert_eq!(first.index.faces.len(), 2);

        std::fs::remove_file(&removed).expect("remove font");
        let second = load_or_build_with(&root, || {
            discovery_calls.set(discovery_calls.get() + 1);
            discovered(vec![kept.clone(), removed.clone()])
        })
        .expect("warm read");
        assert_eq!(discovery_calls.get(), 2);
        assert_eq!(second.index.faces.len(), 1);
        assert_eq!(second.index.faces[0].path, kept);
        assert_eq!(second.parsed_paths, 0, "a missing file is not parsed");

        // Restore it: the next start discovers it again and parses it.
        std::fs::write(&removed, test_font_bytes()).expect("restore font");
        let third = load_or_build_with(&root, || {
            discovery_calls.set(discovery_calls.get() + 1);
            discovered(vec![kept.clone(), removed.clone()])
        })
        .expect("restored read");
        assert_eq!(discovery_calls.get(), 3);
        assert_eq!(third.index.faces.len(), 2);
        assert_eq!(third.parsed_paths, 1, "the restored path parses");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_font_in_scanned_directory_is_parsed() {
        let dir = temp_dir("new-font");
        let a = write_font(&dir, "a.ttf");
        let root = dir.join("cache");
        let first = load_or_build_with(&root, || scan_dir(&dir)).expect("cold build");
        assert_eq!(face_paths(&first), vec![a.clone()]);
        assert_eq!(first.parsed_paths, 1);

        let b = write_font(&dir, "b.ttf");
        let second = load_or_build_with(&root, || scan_dir(&dir)).expect("new font read");
        assert_eq!(face_paths(&second), vec![a, b]);
        assert_eq!(second.parsed_paths, 1, "only the new font parses");
        assert!(second.changed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removed_and_readded_font_is_discovered_again() {
        let dir = temp_dir("readd");
        let font = write_font(&dir, "a.ttf");
        let stable = write_font(&dir, "stable.ttf");
        let root = dir.join("cache");
        let first = load_or_build_with(&root, || scan_dir(&dir)).expect("cold build");
        assert_eq!(first.index.faces.len(), 2);

        std::fs::remove_file(&font).expect("remove");
        let second = load_or_build_with(&root, || scan_dir(&dir)).expect("removed read");
        assert_eq!(second.index.faces.len(), 1);
        assert_eq!(second.index.faces[0].path, stable);
        assert_eq!(second.parsed_paths, 0);

        std::fs::write(&font, test_font_bytes()).expect("re-add");
        let third = load_or_build_with(&root, || scan_dir(&dir)).expect("readd read");
        assert_eq!(third.index.faces.len(), 2);
        assert_eq!(third.parsed_paths, 1, "re-added font parses again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_set_change_drops_old_paths_and_adds_new_ones() {
        let dir = temp_dir("set-change");
        let a = write_font(&dir, "a.ttf");
        let b = write_font(&dir, "b.ttf");
        let root = dir.join("cache");
        let first = load_or_build_with(&root, || discovered(vec![a.clone()])).expect("cold build");
        assert_eq!(face_paths(&first), vec![a.clone()]);

        // Models a fontconfig directory/FONTCONFIG_FILE change: discovery no
        // longer produces `a` and now produces `b`.
        let second =
            load_or_build_with(&root, || discovered(vec![b.clone()])).expect("changed discovery");
        assert_eq!(face_paths(&second), vec![b]);
        assert_eq!(second.parsed_paths, 1);
        assert!(second.changed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn foreign_cache_path_is_dropped() {
        let dir = temp_dir("foreign");
        let font = write_font(&dir, "a.ttf");
        let root = dir.join("cache");
        std::fs::create_dir_all(&root).expect("cache dir");
        let foreign = IndexFace {
            path: PathBuf::from("/etc/passwd"),
            face_index: 0,
            families: vec!["passwd".to_string()],
            post_script_name: "passwd".to_string(),
            style: 0,
            weight: 400,
            stretch: 5,
            monospaced: false,
            file_size: 1,
            mtime_secs: 0,
            mtime_nanos: 0,
        };
        write_index(
            &root.join(INDEX_FILE_NAME),
            &FontIndex {
                faces: vec![foreign],
                generic: GenericFamilies::default(),
            },
        )
        .expect("write foreign index");
        let outcome =
            load_or_build_with(&root, || discovered(vec![font.clone()])).expect("reconciled load");
        assert_eq!(face_paths(&outcome), vec![font]);
        assert!(
            !outcome
                .index
                .faces
                .iter()
                .any(|face| face.path == Path::new("/etc/passwd")),
            "a foreign cache path must never survive discovery"
        );
        assert_eq!(outcome.parsed_paths, 1);
        let text = std::fs::read_to_string(root.join(INDEX_FILE_NAME)).expect("rewritten index");
        assert!(
            !text.contains("passwd"),
            "foreign entry must be rewritten out"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_outside_discovered_fonts_is_dropped() {
        let dir = temp_dir("outside");
        let discovered_font = write_font(&dir, "a.ttf");
        let outside = write_font(&dir, "outside.ttf");
        let root = dir.join("cache");
        std::fs::create_dir_all(&root).expect("cache dir");
        let faces = fixture_faces(std::slice::from_ref(&outside));
        write_index(
            &root.join(INDEX_FILE_NAME),
            &FontIndex {
                faces,
                generic: GenericFamilies::default(),
            },
        )
        .expect("write outside index");
        let outcome = load_or_build_with(&root, || discovered(vec![discovered_font.clone()]))
            .expect("reconciled load");
        assert_eq!(face_paths(&outcome), vec![discovered_font]);
        assert_eq!(outcome.parsed_paths, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_font_is_indexed_from_its_canonical_path() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("symlink");
        let real_dir = dir.join("real");
        let font_dir = dir.join("fonts");
        std::fs::create_dir_all(&real_dir).expect("real dir");
        std::fs::create_dir_all(&font_dir).expect("font dir");
        let real = write_font(&real_dir, "real.ttf");
        let link = font_dir.join("link.ttf");
        symlink(&real, &link).expect("symlink");
        let discovered = scan_dir(&font_dir);
        let canonical = real.canonicalize().expect("canonicalize the real font");
        assert_eq!(discovered.paths, vec![canonical.clone()]);
        assert!(discovered.stat(&canonical).is_some());
        assert!(
            !discovered.paths.iter().any(|path| path == &link),
            "the symlink path must not enter the index"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_caps_match_reader_caps() {
        let over_cap = IndexFace {
            path: PathBuf::from("/tmp/capped.ttf"),
            face_index: 0,
            families: (0..100)
                .map(|index| format!("{index:03}{}", "f".repeat(300)))
                .collect(),
            post_script_name: "p".repeat(1000),
            style: 0,
            weight: 400,
            stretch: 5,
            monospaced: false,
            file_size: 1,
            mtime_secs: 0,
            mtime_nanos: 0,
        };
        let mut faces = vec![over_cap];
        normalize_faces(&mut faces);
        assert_eq!(faces.len(), 1);
        assert_eq!(faces[0].families.len(), MAX_INDEX_FAMILIES);
        assert!(
            faces[0]
                .families
                .iter()
                .all(|family| family.len() <= MAX_INDEX_FAMILY_BYTES)
        );
        assert!(faces[0].post_script_name.len() <= MAX_INDEX_POSTSCRIPT_BYTES);
        let parsed = parse_index(&serialize_index(&FontIndex {
            faces,
            generic: GenericFamilies::default(),
        }))
        .expect("capped face must round-trip");
        assert_eq!(parsed.faces.len(), 1);
    }

    #[test]
    fn over_cap_path_is_dropped() {
        let face = IndexFace {
            path: PathBuf::from(format!("/tmp/{}.ttf", "x".repeat(MAX_INDEX_PATH_BYTES))),
            face_index: 0,
            families: vec!["Capped".to_string()],
            post_script_name: "Capped".to_string(),
            style: 0,
            weight: 400,
            stretch: 5,
            monospaced: false,
            file_size: 1,
            mtime_secs: 0,
            mtime_nanos: 0,
        };
        let mut faces = vec![face];
        normalize_faces(&mut faces);
        assert!(faces.is_empty(), "over-cap paths cannot be represented");
    }

    #[test]
    fn truncated_index_is_repaired_from_discovery() {
        let dir = temp_dir("truncated");
        let font = write_font(&dir, "a.ttf");
        let root = dir.join("cache");
        std::fs::create_dir_all(&root).expect("cache dir");
        let valid = serialize_index(&FontIndex {
            faces: fixture_faces(std::slice::from_ref(&font)),
            generic: GenericFamilies::default(),
        });
        let cut = valid.rfind("end\t").expect("end marker");
        std::fs::write(root.join(INDEX_FILE_NAME), &valid[..cut]).expect("truncated index");
        assert!(read_index(&root.join(INDEX_FILE_NAME)).is_none());
        let outcome = load_or_build_with(&root, || discovered(vec![font])).expect("repaired");
        assert_eq!(outcome.index.faces.len(), 1);
        assert_eq!(outcome.parsed_paths, 1);
        let text = std::fs::read_to_string(root.join(INDEX_FILE_NAME)).expect("rewritten");
        assert!(text.contains("end\t1\t"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unchanged_warm_load_is_read_only() {
        let dir = temp_dir("warm");
        let font = write_font(&dir, "a.ttf");
        let root = dir.join("cache");
        let first = load_or_build_with(&root, || discovered(vec![font.clone()])).expect("cold");
        assert!(first.changed);
        let index_path = root.join(INDEX_FILE_NAME);
        let before = std::fs::metadata(&index_path).expect("index stat");
        let second = load_or_build_with(&root, || discovered(vec![font])).expect("warm");
        assert_eq!(second.parsed_paths, 0);
        assert!(!second.changed, "an unchanged warm load must not rewrite");
        let after = std::fs::metadata(&index_path).expect("index stat");
        assert_eq!(before.modified().ok(), after.modified().ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generic_aliases_come_from_the_fresh_discovery() {
        let dir = temp_dir("generic");
        let font = write_font(&dir, "a.ttf");
        let root = dir.join("cache");
        let generic_a = GenericFamilies {
            monospace: "First Mono".to_string(),
            ..GenericFamilies::default()
        };
        let generic_b = GenericFamilies {
            monospace: "Second Mono".to_string(),
            ..GenericFamilies::default()
        };
        let first = load_or_build_with(&root, || {
            DiscoveredFonts::from_paths(vec![font.clone()], generic_a.clone())
        })
        .expect("cold");
        assert_eq!(first.index.generic, generic_a);
        let second = load_or_build_with(&root, || {
            DiscoveredFonts::from_paths(vec![font], generic_b.clone())
        })
        .expect("generic change");
        assert_eq!(second.index.generic, generic_b);
        assert!(second.changed);
        assert_eq!(second.parsed_paths, 0, "faces stay cached");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_index_rebuilds_from_discovery() {
        let dir = temp_dir("corrupt");
        let path = write_font(&dir, "a.ttf");
        let root = dir.join("cache");
        std::fs::create_dir_all(&root).expect("cache dir");
        std::fs::write(root.join(INDEX_FILE_NAME), b"not an index").expect("write corruption");
        let outcome =
            load_or_build_with(&root, || discovered(vec![path])).expect("rebuild on corruption");
        assert_eq!(outcome.index.faces.len(), 1);
        assert_eq!(outcome.parsed_paths, 1);
        let text = std::fs::read_to_string(root.join(INDEX_FILE_NAME)).expect("rewritten index");
        assert!(text.starts_with("v2\n"));
        assert!(text.contains("end\t1\t"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_build_is_deterministic() {
        let dir = temp_dir("parallel");
        let paths: Vec<PathBuf> = (0..16)
            .map(|index| write_font(&dir, &format!("font-{index:02}.ttf")))
            .collect();
        let mut stats = HashMap::new();
        for path in &paths {
            stats.insert(path.clone(), Stat::read(path).expect("stat"));
        }
        let first = build_index_counted(&paths, &stats);
        let second = build_index_counted(&paths, &stats);
        assert_eq!(first, second, "worker scheduling must not change the index");
        assert_eq!(first.1, paths.len());
        let index = FontIndex {
            faces: first.0,
            generic: GenericFamilies::default(),
        };
        assert_eq!(serialize_index(&index), serialize_index(&index));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_writers_leave_a_valid_index() {
        let dir = temp_dir("concurrent");
        let path = write_font(&dir, "a.ttf");
        let faces = fixture_faces(&[path]);
        let path = dir.join(INDEX_FILE_NAME);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let path = path.clone();
                let faces = faces.clone();
                scope.spawn(move || {
                    let index = FontIndex {
                        faces,
                        generic: GenericFamilies::default(),
                    };
                    let _ = write_index(&path, &index);
                });
            }
        });
        let index = read_index(&path).expect("concurrent writers leave a readable index");
        assert_eq!(index.faces.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_only_cache_dir_fails_soft() {
        let dir = temp_dir("readonly");
        let font = write_font(&dir, "a.ttf");
        let root = dir.join("not-a-directory");
        std::fs::write(&root, b"blocker").expect("blocker file");
        let outcome = load_or_build_with(&root, || discovered(vec![font]))
            .expect("in-memory index survives a failed write");
        assert_eq!(outcome.index.faces.len(), 1);
        assert!(write_index(&root.join(INDEX_FILE_NAME), &outcome.index).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_directory_fails_soft() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir("readonly-dir");
        let font = write_font(&dir, "a.ttf");
        let root = dir.join("cache");
        std::fs::create_dir_all(&root).expect("cache dir");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500))
            .expect("chmod cache");
        if std::fs::File::create(root.join("probe")).is_ok() {
            eprintln!("skipped: running with permission-bypassing privileges");
            let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let outcome = load_or_build_with(&root, || discovered(vec![font]))
            .expect("in-memory index survives a read-only cache");
        assert_eq!(outcome.index.faces.len(), 1);
        assert!(
            write_index(&root.join(INDEX_FILE_NAME), &outcome.index).is_err(),
            "read-only cache must reject the write"
        );
        let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_walk_accepts_font_extensions_only() {
        let dir = temp_dir("walk");
        let font = write_font(&dir, "a.ttf");
        std::fs::write(dir.join("readme.txt"), b"x").expect("readme");
        std::fs::write(dir.join("font.woff2"), b"x").expect("woff");
        let nested = dir.join("nested");
        std::fs::create_dir_all(&nested).expect("nested");
        let nested_font = write_font(&nested, "b.OTF");
        let (files, stats) = collect_font_files(std::slice::from_ref(&dir));
        assert!(files.contains(&font));
        assert!(files.contains(&nested_font));
        assert!(stats.contains_key(&font));
        assert!(!files.iter().any(|file| file.ends_with("readme.txt")));
        assert!(!files.iter().any(|file| file.ends_with("font.woff2")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    ))]
    #[test]
    fn fontconfig_file_change_changes_discovery() {
        let dir = temp_dir("fontconfig");
        let fonts_a = dir.join("fonts-a");
        let fonts_b = dir.join("fonts-b");
        std::fs::create_dir_all(&fonts_a).expect("fonts-a");
        std::fs::create_dir_all(&fonts_b).expect("fonts-b");
        write_font(&fonts_a, "a.ttf");
        write_font(&fonts_b, "b.ttf");
        let config_a = dir.join("fonts-a.conf");
        let config_b = dir.join("fonts-b.conf");
        std::fs::write(
            &config_a,
            format!(
                "<fontconfig><dir>{}</dir><alias><family>monospace</family><prefer><family>First Mono</family></prefer></alias></fontconfig>",
                fonts_a.display()
            ),
        )
        .expect("config a");
        std::fs::write(
            &config_b,
            format!(
                "<fontconfig><dir>{}</dir><alias><family>monospace</family><prefer><family>Second Mono</family></prefer></alias></fontconfig>",
                fonts_b.display()
            ),
        )
        .expect("config b");
        let (dirs_a, generic_a) =
            fontconfig_discovery_from(Some(&config_a), None, Some("/home/test"))
                .expect("config a parses");
        assert_eq!(dirs_a, vec![fonts_a.clone()]);
        assert_eq!(generic_a.monospace, "First Mono");
        let (dirs_b, generic_b) =
            fontconfig_discovery_from(Some(&config_b), None, Some("/home/test"))
                .expect("config b parses");
        assert_eq!(dirs_b, vec![fonts_b]);
        assert_eq!(generic_b.monospace, "Second Mono");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn mutate_first_face_field(text: &str, field: usize, value: &str) -> String {
        let mut out = String::new();
        let mut changed = false;
        for line in text.lines() {
            if !changed && line.starts_with("face\t") {
                let mut fields: Vec<&str> = line.split('\t').collect();
                if field < fields.len() {
                    fields[field] = value;
                    out.push_str(&fields.join("\t"));
                    out.push('\n');
                    changed = true;
                    continue;
                }
            }
            out.push_str(line);
            out.push('\n');
        }
        assert!(changed, "test mutation must find a face line");
        out
    }

    #[cfg(unix)]
    #[test]
    fn fifo_cache_path_is_rejected_without_blocking() {
        use std::process::Command;
        let dir = temp_dir("fifo");
        let path = dir.join(INDEX_FILE_NAME);
        let status = Command::new("mkfifo").arg(&path).status();
        if !matches!(status, Ok(status) if status.success()) {
            eprintln!("skipped: mkfifo unavailable");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        // A metadata check must reject the FIFO before `File::open` blocks.
        assert!(read_index(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_face_discovered_file_does_not_rewrite_the_warm_index() {
        let dir = temp_dir("junk");
        let good = write_font(&dir, "good.ttf");
        // AppleDouble/resource-fork style junk with a font extension: it is
        // discovered but parses to zero faces.
        std::fs::write(dir.join("._junk.ttf"), b"not a font").expect("junk file");
        let root = dir.join("cache");
        let first = load_or_build_with(&root, || scan_dir(&dir)).expect("cold build");
        assert_eq!(face_paths(&first), vec![good.clone()]);
        assert!(first.changed);
        let index_path = root.join(INDEX_FILE_NAME);
        let before = std::fs::metadata(&index_path).expect("index stat");
        let second = load_or_build_with(&root, || scan_dir(&dir)).expect("warm read");
        assert_eq!(second.index.faces.len(), 1);
        assert_eq!(second.parsed_paths, 1, "junk is still parsed for reporting");
        assert!(
            !second.changed,
            "zero-face files must not force a cache rewrite"
        );
        let after = std::fs::metadata(&index_path).expect("index stat");
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().ok(), after.modified().ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn over_cap_generic_alias_is_truncated() {
        let dir = temp_dir("generic-cap");
        let path = write_font(&dir, "a.ttf");
        let index = FontIndex {
            faces: fixture_faces(&[path]),
            generic: GenericFamilies {
                monospace: "x".repeat(70_000),
                ..GenericFamilies::default()
            },
        };
        let text = serialize_index(&index);
        assert!(
            text.lines().all(|line| line.len() <= MAX_INDEX_LINE_BYTES),
            "generic line must stay under the reader's line cap"
        );
        let parsed = parse_index(&text).expect("capped generic must parse");
        assert_eq!(parsed.generic.monospace.len(), MAX_INDEX_FAMILY_BYTES);
        assert!(parsed.generic.monospace.bytes().all(|byte| byte == b'x'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serialized_size_stays_under_the_read_bound() {
        let family = |face: usize, name: usize| {
            let prefix = format!("{face:04}-{name:02}-");
            let mut value = prefix;
            value.push_str(&"x".repeat(MAX_INDEX_FAMILY_BYTES - value.len()));
            value
        };
        let faces: Vec<IndexFace> = (0..4_200)
            .map(|face| IndexFace {
                path: PathBuf::from(format!("/tmp/bitty-size-{face}.ttf")),
                face_index: 0,
                families: (0..MAX_INDEX_FAMILIES)
                    .map(|name| family(face, name))
                    .collect(),
                post_script_name: "PS".to_string(),
                style: 0,
                weight: 400,
                stretch: 5,
                monospaced: true,
                file_size: 1,
                mtime_secs: 0,
                mtime_nanos: 0,
            })
            .collect();
        let text = serialize_index(&FontIndex {
            faces,
            generic: GenericFamilies::default(),
        });
        assert!(
            text.len() as u64 <= MAX_INDEX_BYTES,
            "writer must stop before the reader's byte bound"
        );
        assert!(text.starts_with("v2\n"));
        let parsed = parse_index(&text).expect("bounded index must parse");
        assert!(!parsed.faces.is_empty());
        assert!(
            parsed.faces.len() < 4_200,
            "the size cap must truncate the face set"
        );
    }

    #[test]
    fn valid_line_mutation_is_rejected_by_the_hash() {
        let dir = temp_dir("mutation");
        let path = write_font(&dir, "a.ttf");
        let index = FontIndex {
            faces: fixture_faces(std::slice::from_ref(&path)),
            generic: GenericFamilies::default(),
        };
        let text = serialize_index(&index);
        let mutated = mutate_first_face_field(&text, 10, "Hacked Family");
        assert!(
            parse_index(&mutated).is_none(),
            "a syntactically valid family mutation must fail the content hash"
        );
        let root = dir.join("cache");
        std::fs::create_dir_all(&root).expect("cache dir");
        std::fs::write(root.join(INDEX_FILE_NAME), &mutated).expect("mutated cache");
        let outcome =
            load_or_build_with(&root, || discovered(vec![path])).expect("mutated cache rebuilds");
        assert_eq!(outcome.index.faces.len(), 1);
        assert_eq!(outcome.index.faces[0].families[0], "Bitty Index Test");
        assert_eq!(outcome.parsed_paths, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ttc_face_mutation_reparses_the_whole_collection() {
        let host = discover_system_fonts();
        let Some(ttc) = host.paths.iter().find(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ttc"))
        }) else {
            eprintln!("skipped: no TrueType collection on this host");
            return;
        };
        let faces = build_index(std::slice::from_ref(ttc));
        if faces.len() < 2 {
            eprintln!("skipped: collection has fewer than two parsable faces");
            return;
        }
        let original_count = faces.len();
        let text = serialize_index(&FontIndex {
            faces,
            generic: GenericFamilies::default(),
        });
        // Mutate one face's mtime on a syntactically valid line.
        let mutated = mutate_first_face_field(&text, 4, "0");
        assert!(
            parse_index(&mutated).is_none(),
            "hash must reject the mutation"
        );
        let dir = temp_dir("ttc-mutation");
        let root = dir.join("cache");
        std::fs::create_dir_all(&root).expect("cache dir");
        std::fs::write(root.join(INDEX_FILE_NAME), &mutated).expect("mutated cache");
        let outcome = load_or_build_with(&root, || discovered(vec![ttc.clone()]))
            .expect("TTC cache rebuilds");
        assert_eq!(
            outcome.index.faces.len(),
            original_count,
            "a rejected TTC cache must reparse every face, not lose one"
        );
        assert_eq!(outcome.parsed_paths, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn face_selection_matches_the_full_scan() {
        let discovered = discover_system_fonts();
        if discovered.paths.is_empty() {
            eprintln!("skipped: no system font files discovered");
            return;
        }
        let index = FontIndex {
            faces: build_index(&discovered.paths),
            generic: discovered.generic,
        };
        if index.faces.is_empty() {
            eprintln!("skipped: index parser accepted no system faces");
            return;
        }
        let index_db = database_from_index(&index);
        let full_db = full_scan_database();
        assert_eq!(
            index_db.family_name(&Family::Monospace),
            full_db.family_name(&Family::Monospace),
            "generic monospace alias must match the full scan"
        );
        for family in bitty_config::types::FONT_FALLBACK_CHAIN
            .iter()
            .map(|family| Family::Name(family))
            .chain(std::iter::once(Family::Monospace))
        {
            let families = [family];
            let query = fontdb::Query {
                families: &families,
                weight: Weight::NORMAL,
                style: Style::Normal,
                stretch: Stretch::Normal,
            };
            let (Some(index_face), Some(full_face)) =
                (index_db.query(&query), full_db.query(&query))
            else {
                assert_eq!(
                    index_db.query(&query).is_some(),
                    full_db.query(&query).is_some(),
                    "face availability for {family:?} must match the full scan"
                );
                continue;
            };
            let index_info = index_db.face(index_face).expect("index info");
            let full_info = full_db.face(full_face).expect("full info");
            let family_name = match &family {
                Family::Name(name) => (*name).to_string(),
                Family::Monospace => full_db.family_name(&Family::Monospace).to_string(),
                other => format!("{other:?}"),
            };
            let has_family = |info: &fontdb::FaceInfo| {
                info.families
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(&family_name))
            };
            let candidates = full_db.faces().filter(|info| has_family(info)).count();
            if candidates <= 1 {
                assert_eq!(
                    file_source(&index_db, Some(index_face)),
                    file_source(&full_db, Some(full_face)),
                    "face selection for {family:?} must match the full scan"
                );
            } else {
                // Several faces legitimately share a family (for example a
                // regular face and an outline variant). The cache contract is
                // query compatibility, not insertion order: require the same
                // family and the same style axes, not the same file.
                assert!(
                    has_family(index_info),
                    "index face for {family:?} must belong to the family"
                );
                assert_eq!(index_info.style, full_info.style);
                assert_eq!(index_info.weight, full_info.weight);
                assert_eq!(index_info.stretch, full_info.stretch);
            }
        }
    }

    #[test]
    fn parses_faces_from_a_system_collection() {
        let discovered = discover_system_fonts();
        let Some((path, faces)) = discovered
            .paths
            .iter()
            .filter(|path| {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("ttc"))
            })
            .find_map(|path| {
                let stat = Stat::read(path)?;
                let faces = read_faces(path, &stat);
                (faces.len() > 1).then_some((path, faces))
            })
        else {
            eprintln!("skipped: no multi-face TrueType collection on this host");
            return;
        };
        assert!(
            faces.len() > 1,
            "a collection must expose its face indices: {path:?} -> {faces:?}"
        );
        assert!(
            faces
                .iter()
                .enumerate()
                .all(|(index, face)| face.face_index == index as u32),
            "face indices must be stable and ordered"
        );
    }

    #[test]
    fn lazy_database_loads_only_requested_faces() {
        let discovered = discover_system_fonts();
        if discovered.paths.is_empty() {
            eprintln!("skipped: no system font files discovered");
            return;
        }
        let index = FontIndex {
            faces: build_index(&discovered.paths),
            generic: discovered.generic,
        };
        if index.faces.is_empty() {
            eprintln!("skipped: index parser accepted no system faces");
            return;
        }
        let db = std::sync::Arc::new(database_from_index(&index));
        let mut shaped = crate::shaped::SwashSingle::with_database(db);
        let query = crate::glyph::FontQuery {
            family: bitty_config::types::FONT_FALLBACK_CHAIN[0].to_string(),
            style: crate::glyph::FontStyle::Normal,
            point_size: 12.0,
        };
        if shaped.load_font(&query).is_err() {
            eprintln!("skipped: configured family is not installed");
            return;
        }
        assert_eq!(
            shaped.loaded_face_count(),
            1,
            "only the primary face may load at startup"
        );
        if let Some(face) = shaped.dynamic_face_for('\u{6F22}', 12.0) {
            assert_eq!(
                shaped.loaded_face_count(),
                2,
                "fallback face loads on demand"
            );
            let key = crate::glyph::RasterKey::new('\u{6F22}', face, 12.0).expect("raster key");
            assert!(
                shaped.rasterize(key).expect("rasterize").is_some(),
                "lazy fallback face must rasterize the missing glyph"
            );
        }
    }
}
