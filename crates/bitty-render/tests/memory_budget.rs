//! Idle memory-budget gate for issue #1809 (CTX-1026, CTX-1036).
//!
//! A single idle panel reported 382 MB RSS / 1.5 GB VSZ. GPU-driver mappings
//! own most of the VSZ floor (out of scope here); this gate pins the heap
//! Bitty itself controls so a cap doubled or an eager load reintroduced
//! fails CI instead of regressing silently:
//!
//! - startup maps exactly one font face (fallback tails stay pending until
//!   the first fallback miss);
//! - every glyph/shape/atlas/dynamic cache keeps a named cap, each with a
//!   byte line-item under the idle budget;
//! - the atlas starts at the 512 px lazy dimension (256 KiB R8), never the
//!   2048 px maximum, until glyph pressure grows it;
//! - (CTX-1036, phase 2) the GPU surface negotiates `Fifo` first (double,
//!   not triple, buffering), the Linux instance excludes the GL backend so
//!   its driver stack never becomes resident under Vulkan, the logical
//!   device requests `MemoryUsage` sizing, and atlas-less frames size their
//!   texture at the lazy 512 px dimension.
//!
//! Budgets are deliberately headroom-tight (not exact): a legitimate new
//! cache must update the named constant AND this gate together, which is
//! the review tripwire.

use std::collections::HashMap;

use bitty_render::{
    FallbackRasterizer, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphRasterizer, RasterKey,
    RenderError,
    atlas::{DEFAULT_ATLAS_DIMENSION, INITIAL_ATLAS_DIMENSION},
    batch::MAX_ATLAS_DIMENSION,
    cache::DEFAULT_GLYPH_CACHE_CAPACITY,
    glyph::{BitmapFormat, GlyphMetrics},
    gpu::{BackendSelection, PresentMode, pick_present_mode, resolve_instance_backends},
    shaped::{MAX_DYNAMIC_CACHE_ENTRIES, MAX_RUN_CACHE_ENTRIES, MAX_SHAPED_GLYPH_CACHE_ENTRIES},
};

/// Combined worst-case ceiling for the render-side caches (issue #1809).
///
/// Pathology, not idle: both glyph caches full of 64x64 Rgb strikes plus a
/// maxed 2048x2048 R8 atlas. Idle is a small fraction (warm ASCII working
/// set + 512 px atlas); this ceiling only trips when a cap grows or a new
/// unbounded cache lands.
const RENDER_WORST_CASE_BUDGET_BYTES: usize = 56 * 1024 * 1024;

/// Counting fake: records every upstream face load.
#[derive(Debug, Default)]
struct Fake {
    next_id: u64,
    loads: Vec<String>,
    blank: Vec<(String, char)>,
    families: HashMap<FontId, String>,
}

impl Fake {
    fn bitmap_for(character: char) -> GlyphBitmap {
        let side = i32::try_from(u32::from(character) % 3 + 6).unwrap();
        GlyphBitmap::try_new(
            GlyphMetrics {
                left: 0,
                top: 6,
                width: side,
                height: side,
                advance: [side, 0],
            },
            BitmapFormat::Rgb,
            vec![0xAA; side as usize * side as usize * 3],
        )
        .unwrap()
    }

    fn family_of(&self, id: FontId) -> String {
        self.families.get(&id).cloned().unwrap_or_default()
    }
}

impl GlyphRasterizer for Fake {
    fn load_font(&mut self, query: &FontQuery) -> Result<FontId, RenderError> {
        self.loads.push(query.family.clone());
        let id = FontId::next(&mut self.next_id);
        self.families.insert(id, query.family.clone());
        Ok(id)
    }

    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        if self
            .blank
            .contains(&(self.family_of(key.font), key.character))
        {
            return Ok(None);
        }
        Ok(Some(Self::bitmap_for(key.character)))
    }
}

fn query(family: &str) -> FontQuery {
    FontQuery {
        family: family.into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    }
}

fn tails() -> Vec<String> {
    vec![
        "JetBrains Mono".to_string(),
        "monospace".to_string(),
        "DejaVu Sans Mono".to_string(),
        "Noto Sans Symbols 2".to_string(),
    ]
}

#[test]
fn startup_maps_exactly_one_face_until_first_miss() {
    let mut raster = FallbackRasterizer::new(Fake::default(), tails());
    let primary = raster.load_font(&query("Primary Mono")).unwrap();
    // One upstream load at startup: the primary. The four tails stay
    // pending (unmapped) instead of mapping their face files.
    assert_eq!(raster.inner().loads.len(), 1, "startup must load one face");
    assert_eq!(raster.fonts(), &[primary]);
    assert_eq!(raster.pending_tails().len(), tails().len());
    // Primary-covered glyphs never warm the chain, no matter how many.
    for c in ['a', 'z', '0', ' '] {
        let key = RasterKey::new(c, primary, 12.0).unwrap();
        assert!(raster.rasterize(key).unwrap().is_some());
    }
    assert_eq!(raster.inner().loads.len(), 1, "ASCII must not warm tails");
    assert_eq!(raster.fonts(), &[primary]);
    // A primary miss warms every tail exactly once, in chain order.
    raster
        .inner_mut()
        .blank
        .push(("Primary Mono".to_string(), '✔'));
    let key = RasterKey::new('✔', primary, 12.0).unwrap();
    let resolved = raster.resolve(key).unwrap();
    assert!(resolved.covered);
    assert_eq!(raster.inner().loads.len(), 1 + tails().len());
    assert!(raster.pending_tails().is_empty());
    assert_eq!(raster.fonts().len(), 1 + tails().len());
}

#[test]
fn cache_caps_hold_named_byte_lines() {
    // Glyph bitmap worst case: a 64x64 Rgb strike (large for a terminal
    // cell at sane point sizes; emoji CBDT strikes stay near this order).
    const WORST_STRIKE_BYTES: usize = 64 * 64 * 3;
    let glyph_line = DEFAULT_GLYPH_CACHE_CAPACITY * WORST_STRIKE_BYTES;
    assert_eq!(DEFAULT_GLYPH_CACHE_CAPACITY, 2048);
    assert!(
        glyph_line <= 24 * 1024 * 1024,
        "char glyph cache line must stay under 24 MiB, is {glyph_line}"
    );
    let shaped_line = MAX_SHAPED_GLYPH_CACHE_ENTRIES * WORST_STRIKE_BYTES;
    assert_eq!(MAX_SHAPED_GLYPH_CACHE_ENTRIES, 2048);
    assert!(
        shaped_line <= 24 * 1024 * 1024,
        "shaped glyph cache line must stay under 24 MiB, is {shaped_line}"
    );
    // Run cache: 512 short owned vecs; dynamic outcome cache: 4096 tiny
    // map entries (~48 B each with HashMap overhead).
    assert_eq!(MAX_RUN_CACHE_ENTRIES, 512);
    assert_eq!(MAX_DYNAMIC_CACHE_ENTRIES, 4096);
    let dynamic_line = MAX_DYNAMIC_CACHE_ENTRIES * 48;
    assert!(
        dynamic_line <= 256 * 1024,
        "dynamic outcome cache line must stay under 256 KiB, is {dynamic_line}"
    );
    // Atlas: lazy start 512x512 R8, absolute max 2048x2048 R8 (upload cap
    // 4096 keeps one R8 texture at 16 MiB worst case).
    assert_eq!(INITIAL_ATLAS_DIMENSION, 512);
    assert_eq!(DEFAULT_ATLAS_DIMENSION, 2048);
    assert_eq!(MAX_ATLAS_DIMENSION, 4096);
    let initial_texels =
        usize::from(INITIAL_ATLAS_DIMENSION) * usize::from(INITIAL_ATLAS_DIMENSION);
    assert_eq!(
        initial_texels,
        256 * 1024,
        "R8 march: 512x512 must be 256 KiB"
    );
    let max_texels = usize::from(DEFAULT_ATLAS_DIMENSION) * usize::from(DEFAULT_ATLAS_DIMENSION);
    assert_eq!(
        max_texels,
        4 * 1024 * 1024,
        "R8 march: 2048x2048 must be 4 MiB"
    );
    // Combined worst-case owned heap stays inside the ceiling.
    let worst_owned = glyph_line + shaped_line + dynamic_line + max_texels;
    assert!(
        worst_owned <= RENDER_WORST_CASE_BUDGET_BYTES,
        "combined cache/atlas worst case {worst_owned} must stay under {RENDER_WORST_CASE_BUDGET_BYTES}"
    );
}

/// Phase-2 GPU surface policy (CTX-1036, issue #1809).
///
/// Headed measurement on ws5 (same method as the issue: Hyprland idle 10 s,
/// 12 pt, empty scrollback) showed the Vulkan driver arena dominating idle
/// RSS (145 MB `RssShmem` on `/dev/nvidiactl`) with the GL stack resident
/// beside it (~15 MB file mappings + init threads) even though the surface
/// negotiates Vulkan. This gate pins the three policy choices that keep the
/// driver set minimal; the numeric budget (interim headed gate: idle
/// single-panel main-process RSS at or below 250 MB toward 150 MB) is
/// measured headed by `scripts/mem-measure.sh`, while CI pins the policy here
/// so a default flipped back can never regress silently.
#[test]
fn gpu_surface_policy_stays_memory_lean() {
    // Present mode: Fifo (double-buffered vsync) first — one fewer
    // resident frame than Mailbox triple buffering.
    let both = wgpu::SurfaceCapabilities {
        usages: wgpu::TextureUsages::RENDER_ATTACHMENT,
        formats: vec![wgpu::TextureFormat::Bgra8UnormSrgb],
        present_modes: vec![wgpu::PresentMode::Mailbox, wgpu::PresentMode::Fifo],
        alpha_modes: vec![wgpu::CompositeAlphaMode::Auto],
    };
    assert_eq!(pick_present_mode(&both), PresentMode::Fifo);
    // Mailbox-only surfaces still negotiate (degraded, never rejected).
    let mailbox_only = wgpu::SurfaceCapabilities {
        usages: wgpu::TextureUsages::RENDER_ATTACHMENT,
        formats: vec![wgpu::TextureFormat::Bgra8UnormSrgb],
        present_modes: vec![wgpu::PresentMode::Mailbox],
        alpha_modes: vec![wgpu::CompositeAlphaMode::Auto],
    };
    assert_eq!(pick_present_mode(&mailbox_only), PresentMode::Mailbox);

    // Linux default: GL excluded so its driver stack never becomes
    // resident when Vulkan serves the surface; Vulkan kept. (initialize()
    // retries with full backends when no adapter is found, so Vulkan-less
    // hosts still reach GL.)
    let linux_defaults = wgpu::Backends::VULKAN | wgpu::Backends::GL;
    let (backends, selection) = resolve_instance_backends(linux_defaults, None, false, true);
    assert_eq!(selection, BackendSelection::LinuxGlExcluded);
    assert!(!backends.contains(wgpu::Backends::GL), "GL must stay out");
    assert!(backends.contains(wgpu::Backends::VULKAN), "Vulkan kept");
    // Operator pin wins verbatim on every platform.
    let (pinned, selection) =
        resolve_instance_backends(wgpu::Backends::all(), Some(wgpu::Backends::GL), false, true);
    assert_eq!(selection, BackendSelection::EnvOverride);
    assert_eq!(pinned, wgpu::Backends::GL);

    // Atlas-less frames size their texture at the lazy 512 px dimension
    // (256 KiB R8), never the 2048 px maximum (4 MiB): the first atlas
    // frame recreates at its real dims through the resource-match check.
    assert_eq!(INITIAL_ATLAS_DIMENSION, 512);
    let fallback_texels =
        usize::from(INITIAL_ATLAS_DIMENSION) * usize::from(INITIAL_ATLAS_DIMENSION);
    assert_eq!(
        fallback_texels,
        256 * 1024,
        "atlas-less frames must size at 256 KiB, not 4 MiB"
    );
}
