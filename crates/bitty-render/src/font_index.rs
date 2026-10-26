//! Persistent system font index for the shaped text stack (CTX-1607).
//!
//! Font discovery is the shaped stack's only cold-start cost that scales with
//! the host's font collection: a full `fontdb::Database::load_system_fonts`
//! pass parses every face on every process launch (hundreds of milliseconds
//! on a font-rich seat), which blows the accepted PB-1 startup budget
//! (`p50 <= 100 ms`). This module keeps the parsed face metadata in a small
//! persistent index under `$XDG_CACHE_HOME/bitty/font-index-v1` and rebuilds
//! it only when it is missing, stale, or corrupt.
//!
//! # Design
//!
//! - **Cold build (cache miss):** discover the font file paths, parse them in
//!   parallel with bounded `std::thread::scope` workers, and write the index
//!   atomically (unique temp file + `rename`). The cold build is a one-time
//!   cost; the next process reads the index in a few milliseconds.
//! - **Warm start:** read the versioned index (`v1`), stat each referenced
//!   path once, drop missing entries, reparse only changed paths (lazy
//!   per-path rebuild), and build a metadata-only[`fontdb::Database`] from
//!   the result. Face bytes are not read at startup; [`super::shaped::SwashSingle`]
//!   loads only the faces the first frames need and keeps dynamic
//!   (missing-glyph) fallback on demand.
//! - **Correctness:** entries carry path, face index, family names, style,
//!   weight, stretch, monospace flag, and the file size/mtime they were
//!   parsed from. Selection metadata is byte-compatible with fontdb's own
//!   `FaceInfo` (the query only reads the family name string, style, weight,
//!   stretch, and monospace flag), so a query over the index resolves the
//!   same face as a full scan. A corrupt/truncated/version-mismatched index
//!   is discarded and rebuilt; a single changed font only reparses that file.
//! - **Concurrency:** builds run in-process only; writers use a unique temp
//!   file plus an atomic rename, so a reader sees either the previous or the
//!   next complete index. Concurrent processes are last-writer-wins.
//! - **Fail soft:** a missing cache root, read-only cache directory, failed
//!   write, or malformed entry never fails startup; the in-memory index is
//!   used and the full scan remains the final fallback.
//!
//! # Freshness
//!
//! The index detects removed/changed files (path stat drift) but not newly
//! installed files in a directory it already scanned: a new font becomes
//! visible after the cache is refreshed (delete
//! `$XDG_CACHE_HOME/bitty/font-index-v1`) or when `BITTY_FONT_INDEX=0` runs a
//! full scan. This mirrors the one-shot discovery the shaped stack had
//! before the cache; watching font directories for additions is a separate
//! problem.
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

use std::collections::{BTreeSet, HashMap, HashSet};
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
const INDEX_FILE_NAME: &str = "font-index-v1";

/// On-disk format version.
const INDEX_VERSION: &str = "v1";

/// Refuse to read an index larger than this (hostile/truncated files).
const MAX_INDEX_BYTES: u64 = 32 * 1024 * 1024;

/// Refuse to parse an individual line longer than this.
const MAX_INDEX_LINE_BYTES: usize = 64 * 1024;

/// Bound on retained faces (the reference host has ~1.8k).
const MAX_INDEX_FACES: usize = 16_384;

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

/// Paths + aliases produced by one discovery pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredFonts {
    /// Candidate font files (deduplicated, sorted).
    pub paths: Vec<PathBuf>,
    /// Generic family aliases discovered alongside the paths.
    pub generic: GenericFamilies,
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

/// Builds the process-wide database: index read, or a rebuild/scan on miss.
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
            return database_from_index(&index);
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

/// Loads a valid index or rebuilds it from a discovery pass.
fn load_or_build(root: &Path) -> Option<FontIndex> {
    load_or_build_with(root, discover_system_fonts)
}

/// Testable form of [`load_or_build`] with injected discovery.
fn load_or_build_with<F>(root: &Path, discover: F) -> Option<FontIndex>
where
    F: FnOnce() -> DiscoveredFonts,
{
    let path = root.join(INDEX_FILE_NAME);
    if let Some((index, dirty)) = read_index(&path) {
        if dirty {
            let _ = write_index(&path, &index);
        }
        if !index.faces.is_empty() {
            return Some(index);
        }
    }
    let discovered = discover();
    let mut index = if discovered.paths.is_empty() {
        // Discovery found nothing (bare host or unsupported target): let the
        // upstream scan decide, and serialize its metadata on success.
        index_from_database(&full_scan_database(), discovered.generic)
    } else {
        FontIndex {
            faces: build_index(&discovered.paths),
            generic: discovered.generic,
        }
    };
    normalize_faces(&mut index.faces);
    if index.faces.is_empty() {
        // Every candidate was rejected by the metadata parser; the upstream
        // scan is the coverage backstop.
        index = index_from_database(&full_scan_database(), index.generic);
    }
    if index.faces.is_empty() {
        return None;
    }
    let _ = write_index(&path, &index);
    Some(index)
}

/// Reads and validates an index file, reparsing stale paths only.
///
/// Returns `None` when the file is missing, too large, unreadable, has the
/// wrong version, or is malformed. A stale individual path drops its cached
/// entries and is reparsed; the rest of the index is preserved.
fn read_index(path: &Path) -> Option<(FontIndex, bool)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_INDEX_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let parsed = parse_index(&text)?;
    let mut dirty = false;
    let mut stat_cache: HashMap<PathBuf, Option<Stat>> = HashMap::new();
    let mut faces = Vec::new();
    let mut stale: BTreeSet<PathBuf> = BTreeSet::new();
    for face in parsed.faces {
        let stat = stat_cache
            .entry(face.path.clone())
            .or_insert_with(|| Stat::read(&face.path));
        match stat {
            Some(current) if current.matches(&face) => faces.push(face),
            Some(_) => {
                // Changed file: reparse only this path (lazily), keep the
                // rest of the cache.
                dirty = true;
                stale.insert(face.path.clone());
            }
            None => {
                // Missing/unreadable file: drop its entries.
                dirty = true;
            }
        }
    }
    if !stale.is_empty() {
        let paths: Vec<PathBuf> = stale.into_iter().collect();
        faces.extend(build_index(&paths));
        dirty = true;
    }
    normalize_faces(&mut faces);
    Some((
        FontIndex {
            faces,
            generic: parsed.generic,
        },
        dirty,
    ))
}

/// Converts an index into the metadata-only fontdb database the shaper uses.
///
/// No face bytes are read here; `Source::File` keeps the path so
/// [`super::shaped::SwashSingle::load_font`] can load the selected face and
/// dynamic fallback can load a missing-glyph face on demand.
pub(crate) fn database_from_index(index: &FontIndex) -> Database {
    let mut db = Database::new();
    index.generic.apply(&mut db);
    for face in &index.faces {
        db.push_face_info(FaceInfo {
            id: ID::dummy(),
            source: Source::File(face.path.clone()),
            index: face.face_index,
            families: face
                .families
                .iter()
                .map(|family| (family.clone(), Language::English_UnitedStates))
                .collect(),
            post_script_name: face.post_script_name.clone(),
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
    for (kind, name) in index.generic.entries() {
        out.push_str("generic\t");
        out.push_str(kind);
        out.push('\t');
        out.push_str(&escape_field(name));
        out.push('\n');
    }
    for face in &index.faces {
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
        out.push('\n');
    }
    out
}

/// Parses the versioned line format.
///
/// Strict on v1: any malformed line rejects the whole file so the caller
/// rebuilds from discovery rather than trusting a partially corrupted cache.
fn parse_index(text: &str) -> Option<FontIndex> {
    let mut lines = text.lines();
    if lines.next()? != INDEX_VERSION {
        return None;
    }
    let mut generic = GenericFamilies::default();
    let mut faces = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.len() > MAX_INDEX_LINE_BYTES {
            return None;
        }
        let mut fields = line.split('\t');
        match fields.next()? {
            "generic" => {
                let kind = fields.next()?;
                let name = unescape_field(fields.next()?)?;
                if fields.next().is_some() {
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
            "face" => faces.push(parse_face_line(fields)?),
            _ => return None,
        }
        if faces.len() > MAX_INDEX_FACES {
            return None;
        }
    }
    normalize_faces(&mut faces);
    Some(FontIndex { faces, generic })
}

/// Parses one `face` line (the `face` tag is already consumed).
fn parse_face_line<'a>(mut fields: impl Iterator<Item = &'a str>) -> Option<IndexFace> {
    let path = unescape_field(fields.next()?)?;
    if path.is_empty() || path.len() > 4096 {
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
    if families.is_empty() || families.len() > 64 || families.iter().any(|f| f.len() > 1024) {
        return None;
    }
    let post_script_name = unescape_field(fields.next()?)?;
    if post_script_name.len() > 512 || fields.next().is_some() {
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
        match std::fs::rename(&temp, path) {
            Ok(()) => Ok(()),
            #[cfg(windows)]
            Err(_) => {
                // Windows cannot atomically replace an existing file; the
                // remove+rename fallback is last-writer-wins (never corrupt:
                // the temp file is complete before the swap).
                let _ = std::fs::remove_file(path);
                std::fs::rename(&temp, path)
            }
            #[cfg(not(windows))]
            Err(err) => Err(err),
        }
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Sorts and deduplicates faces by `(path, face_index)`.
fn normalize_faces(faces: &mut Vec<IndexFace>) {
    faces.sort_by(|a, b| a.path.cmp(&b.path).then(a.face_index.cmp(&b.face_index)));
    faces.dedup_by(|a, b| a.path == b.path && a.face_index == b.face_index);
    // Keep the cache self-consistent with `parse_index`'s read bound; a host
    // beyond this bound keeps the lexicographically first 16k faces.
    faces.truncate(MAX_INDEX_FACES);
}

/// Builds index entries for `paths` using bounded parallel workers.
///
/// Paths are sorted before chunking so the result is deterministic regardless
/// of worker scheduling or `read_dir` order.
fn build_index(paths: &[PathBuf]) -> Vec<IndexFace> {
    let mut paths = paths.to_vec();
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return Vec::new();
    }
    let workers = scan_workers(paths.len());
    let mut faces = if workers <= 1 {
        paths.iter().flat_map(|path| parse_font(path)).collect()
    } else {
        let chunk = paths.len().div_ceil(workers);
        let parts = std::thread::scope(|scope| {
            let handles: Vec<_> = paths
                .chunks(chunk)
                .map(|chunk| {
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .flat_map(|path| parse_font(path))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap_or_default())
                .collect::<Vec<Vec<IndexFace>>>()
        });
        parts.into_iter().flatten().collect()
    };
    normalize_faces(&mut faces);
    faces
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
fn parse_font(path: &Path) -> Vec<IndexFace> {
    // The line format is UTF-8; a non-UTF-8 path would not survive the
    // round trip and would look stale on every launch. Such paths are rare
    // and remain reachable through `BITTY_FONT_INDEX=0`/the full scan.
    if path.to_str().is_none() {
        return Vec::new();
    }
    let Some(stat) = Stat::read(path) else {
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
        let (dirs, generic) = fontconfig_discovery()
            .unwrap_or_else(|| (fontconfig_fallback_dirs(), GenericFamilies::default()));
        let paths = collect_font_files(&dirs);
        return DiscoveredFonts { paths, generic };
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "windows"))]
    {
        let paths = collect_font_files(&platform_font_dirs());
        return DiscoveredFonts {
            paths,
            generic: GenericFamilies::default(),
        };
    }

    #[allow(unreachable_code)]
    DiscoveredFonts {
        paths: Vec::new(),
        generic: GenericFamilies::default(),
    }
}

/// Linux/BSD discovery through fontconfig, matching fontdb's parser setup.
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
))]
fn fontconfig_discovery() -> Option<(Vec<PathBuf>, GenericFamilies)> {
    use std::path::Path;

    let home = std::env::var("HOME").ok();
    let mut fontconfig = fontconfig_parser::FontConfig::default();
    if let Ok(config_file) = std::env::var("FONTCONFIG_FILE") {
        let _ = fontconfig.merge_config(Path::new(&config_file));
    } else {
        let xdg_config_home = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| Path::new(home).join(".config")));
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
        .filter_map(|dir| expand_home(dir.path, home.as_deref()))
        .collect::<Vec<_>>();
    if dirs.is_empty() {
        // Keep the parsed aliases even when no <dir> entry was configured;
        // fontdb's full scan sets aliases before falling back to known dirs.
        return Some((fontconfig_fallback_dirs(), generic));
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
fn fontconfig_fallback_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
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
fn collect_font_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    let mut seen_files: HashSet<PathBuf> = HashSet::new();
    for dir in dirs {
        walk_font_dir(dir, 0, &mut seen_dirs, &mut seen_files, &mut files);
        if files.len() >= MAX_FONT_FILES {
            break;
        }
    }
    files.sort();
    files.dedup();
    files.truncate(MAX_FONT_FILES);
    files
}

/// One recursive directory step.
fn walk_font_dir(
    dir: &Path,
    depth: usize,
    seen_dirs: &mut HashSet<PathBuf>,
    seen_files: &mut HashSet<PathBuf>,
    files: &mut Vec<PathBuf>,
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
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if seen_dirs.insert(key) {
                walk_font_dir(&path, depth + 1, seen_dirs, seen_files, files);
            }
            continue;
        }
        if !meta.is_file() || !is_font_extension(&path) {
            continue;
        }
        let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !seen_files.insert(key) {
            continue;
        }
        // Keep the walked path for regular files (fontdb does the same) and
        // the resolved path for symlinks (fontdb canonicalizes those).
        let stored = if std::fs::symlink_metadata(&path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
        {
            std::fs::canonicalize(&path).unwrap_or(path)
        } else {
            path
        };
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
        DiscoveredFonts {
            paths,
            generic: GenericFamilies::default(),
        }
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
        // Same metadata through fontdb's parser (the full-scan authority).
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
        assert!(text.starts_with("v1\n"));
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
        assert!(parse_index("v2\n").is_none());
        let good = serialize_index(&FontIndex {
            faces: Vec::new(),
            generic: GenericFamilies::default(),
        });
        assert!(parse_index(&good).is_some());
        assert!(
            parse_index("v1\nface\tbroken\n").is_none(),
            "truncated face line must reject the cache"
        );
    }

    #[test]
    fn stale_entry_rebuilds_only_the_changed_path() {
        let dir = temp_dir("stale");
        let changed = write_font(&dir, "changed.ttf");
        let stable = write_font(&dir, "stable.ttf");
        let root = dir.join("cache");
        let first = load_or_build_with(&root, || discovered(vec![changed.clone(), stable.clone()]))
            .expect("cold build");
        assert_eq!(first.faces.len(), 2);

        // Move only `changed`'s mtime; discovery must not run again.
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::open(&changed)
            .expect("open changed")
            .set_modified(future)
            .expect("set mtime");
        let second =
            load_or_build_with(&root, || panic!("discovery must not run")).expect("warm read");
        assert_eq!(second.faces.len(), 2);
        let changed_face = second
            .faces
            .iter()
            .find(|face| face.path == changed)
            .expect("changed entry");
        let stable_face = second
            .faces
            .iter()
            .find(|face| face.path == stable)
            .expect("stable entry");
        assert_eq!(
            changed_face.mtime_secs,
            future.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
        );
        assert_eq!(
            stable_face.mtime_secs,
            first
                .faces
                .iter()
                .find(|f| f.path == stable)
                .unwrap()
                .mtime_secs
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_dropped_without_losing_the_rest() {
        let dir = temp_dir("missing");
        let kept = write_font(&dir, "kept.ttf");
        let removed = write_font(&dir, "removed.ttf");
        let root = dir.join("cache");
        let first = load_or_build_with(&root, || discovered(vec![kept.clone(), removed.clone()]))
            .expect("cold build");
        assert_eq!(first.faces.len(), 2);
        std::fs::remove_file(&removed).expect("remove font");
        let second =
            load_or_build_with(&root, || panic!("discovery must not run")).expect("warm read");
        assert_eq!(second.faces.len(), 1);
        assert_eq!(second.faces[0].path, kept);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_index_rebuilds_from_discovery() {
        let dir = temp_dir("corrupt");
        let path = write_font(&dir, "a.ttf");
        let root = dir.join("cache");
        std::fs::create_dir_all(&root).expect("cache dir");
        std::fs::write(root.join(INDEX_FILE_NAME), b"not an index").expect("write corruption");
        let index =
            load_or_build_with(&root, || discovered(vec![path])).expect("rebuild on corruption");
        assert_eq!(index.faces.len(), 1);
        let text = std::fs::read_to_string(root.join(INDEX_FILE_NAME)).expect("rewritten index");
        assert!(text.starts_with("v1\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_build_is_deterministic() {
        let dir = temp_dir("parallel");
        let paths: Vec<PathBuf> = (0..16)
            .map(|index| write_font(&dir, &format!("font-{index:02}.ttf")))
            .collect();
        let first = build_index(&paths);
        let second = build_index(&paths);
        assert_eq!(first, second, "worker scheduling must not change the index");
        assert_eq!(first.len(), paths.len());
        let index = FontIndex {
            faces: first,
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
        let (index, _) = read_index(&path).expect("concurrent writers leave a readable index");
        assert_eq!(index.faces.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_only_cache_dir_fails_soft() {
        let dir = temp_dir("readonly");
        let font = write_font(&dir, "a.ttf");
        // A regular file where the cache directory should be makes every
        // cache write fail, including on privileged test users (root).
        let root = dir.join("not-a-directory");
        std::fs::write(&root, b"blocker").expect("blocker file");
        let index = load_or_build_with(&root, || discovered(vec![font]))
            .expect("in-memory index survives a failed write");
        assert_eq!(index.faces.len(), 1);
        assert!(write_index(&root.join(INDEX_FILE_NAME), &index).is_err());
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
        let files = collect_font_files(std::slice::from_ref(&dir));
        assert!(files.contains(&font));
        assert!(files.contains(&nested_font));
        assert!(!files.iter().any(|file| file.ends_with("readme.txt")));
        assert!(!files.iter().any(|file| file.ends_with("font.woff2")));
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
            let index_face = index_db.query(&query);
            let full_face = full_db.query(&query);
            let index_source = file_source(&index_db, index_face);
            let full_source = file_source(&full_db, full_face);
            assert_eq!(
                index_source, full_source,
                "face selection for {family:?} must match the full scan"
            );
        }
    }

    #[test]
    fn parses_faces_from_a_system_collection() {
        let discovered = discover_system_fonts();
        let Some(path) = discovered.paths.iter().find(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ttc"))
        }) else {
            eprintln!("skipped: no TrueType collection on this host");
            return;
        };
        let stat = Stat::read(path).expect("collection stat");
        let faces = read_faces(path, &stat);
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
        // Privileged test users bypass permission bits; skip instead of
        // asserting a write failure that cannot happen.
        if std::fs::File::create(root.join("probe")).is_ok() {
            eprintln!("skipped: running with permission-bypassing privileges");
            let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let index = load_or_build_with(&root, || discovered(vec![font]))
            .expect("in-memory index survives a read-only cache");
        assert_eq!(index.faces.len(), 1);
        assert!(
            write_index(&root.join(INDEX_FILE_NAME), &index).is_err(),
            "read-only cache must reject the write"
        );
        let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&dir);
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
