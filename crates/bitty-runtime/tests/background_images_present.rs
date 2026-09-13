#![forbid(unsafe_code)]
//! Per-`View` background images at runtime (CTX-0347, RFC-0001/OQ-042).
//!
//! Accepted contract evidence:
//! - the global `decoration.background_image` / `background_fit` pair is the
//!   base tier and a `views` rule overrides either field independently;
//! - every configured image is loaded fail-closed at construction, and a
//!   failure names the owning config key (`decoration.background_image` or
//!   `views[<selector>].background_image`);
//! - the present path paints the image inside the content rect, above cell
//!   backgrounds and below overlays/glyphs, bounded by BG-7;
//! - a config with no image opens nothing (`background_loads() == 0`), which
//!   is exactly what `bitty --safe` produces.

use bitty_runtime::{
    LayoutNode, Runtime, RuntimeConfig, RuntimeViewTarget, View, ViewAppearanceRule, ViewId,
};

/// 4x4 opaque-red PNG (generated once; embedded so the test is hermetic).
const RED_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x04, 0x08, 0x06, 0x00, 0x00, 0x00, 0xA9, 0xF1, 0x9E,
    0x7E, 0x00, 0x00, 0x00, 0x15, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xFC, 0xCF, 0xC0, 0xF0,
    0x9F, 0x01, 0x09, 0x30, 0x21, 0x73, 0x88, 0x13, 0x00, 0x00, 0x83, 0xD1, 0x02, 0x06, 0x04, 0xBC,
    0x24, 0x47, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx0347-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn single_leaf(rt: &mut Runtime) {
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 80, 24));
}

fn probe(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let idx = (y * width + x) * 4;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

fn surface_width(rt: &Runtime) -> usize {
    let extent = rt.config().window_extent();
    usize::try_from(extent.width()).expect("width fits usize")
}

#[test]
fn image_loads_at_construction_and_paints_behind_cells() {
    let root = scratch("present");
    let path = root.join("red.png");
    std::fs::write(&path, RED_PNG).expect("write fixture");
    let path = path.display().to_string();
    let roots = vec![root.display().to_string()];
    let mut rt = Runtime::new(RuntimeConfig {
        background_image: Some(path),
        background_fit: "stretch".to_string(),
        background_image_roots: roots,
        ..RuntimeConfig::default()
    })
    .expect("one approved image builds");
    assert_eq!(rt.background_image_count(), 1);
    assert_eq!(rt.background_loads(), 1, "decoded once at construction");
    single_leaf(&mut rt);
    let stats = rt.tick().expect("first tick presents");
    assert_eq!(stats.backgrounds, 1, "one background blit presented");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let width = surface_width(&rt);
    // Probe inside the content rect, below the first text line: the erased
    // primary grid paints only cell backgrounds there, so a red pixel proves
    // the image composites above cells.
    let frame = rt.present_frames()[0];
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
    let x = pad + usize::try_from(frame.content.x).expect("content x") + 20;
    let y = pad + usize::try_from(frame.content.y).expect("content y") + 300;
    assert_eq!(
        probe(&rgba, width, x, y),
        [255, 0, 0, 255],
        "background image must cover the pane cell background"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn per_view_rule_overrides_global_image_per_field() {
    let cfg = RuntimeConfig {
        background_image: Some("/wall/global.png".to_string()),
        background_fit: "fit".to_string(),
        view_appearance: vec![ViewAppearanceRule {
            selector: "view:7".to_string(),
            background_image: Some("/wall/seven.png".to_string()),
            ..Default::default()
        }],
        ..RuntimeConfig::default()
    };
    let seven = cfg.resolve_view_background(&RuntimeViewTarget {
        content: "terminal",
        workspace_label: 1,
        view_id: 7,
    });
    assert_eq!(seven.image.as_deref(), Some("/wall/seven.png"));
    assert_eq!(seven.fit, "fit", "the inherited fit survives the override");
    let other = cfg.resolve_view_background(&RuntimeViewTarget {
        content: "terminal",
        workspace_label: 1,
        view_id: 8,
    });
    assert_eq!(other.image.as_deref(), Some("/wall/global.png"));
    assert_eq!(other.fit, "fit");
}

#[test]
fn invalid_images_fail_closed_naming_the_key() {
    // Global field: a missing file under an approved root rejects the whole
    // config and names `decoration.background_image`.
    let root = scratch("missing");
    let missing = root.join("nope.png").display().to_string();
    let err = Runtime::new(RuntimeConfig {
        background_image: Some(missing),
        background_image_roots: vec![root.display().to_string()],
        ..RuntimeConfig::default()
    })
    .expect_err("missing image must fail closed");
    assert!(
        matches!(err, bitty_runtime::RuntimeError::BackgroundImage(_)),
        "typed background error: {err}"
    );
    assert!(
        err.to_string().contains("decoration.background_image"),
        "must name the owning key: {err}"
    );

    // Per-View field: an unapproved path under a rule names the selector.
    let approved = scratch("approved");
    let outside = scratch("outside");
    let outside_png = outside.join("wall.png");
    std::fs::write(&outside_png, RED_PNG).expect("write outside fixture");
    let err = Runtime::new(RuntimeConfig {
        background_image_roots: vec![approved.display().to_string()],
        view_appearance: vec![ViewAppearanceRule {
            selector: "ws:2".to_string(),
            background_image: Some(outside_png.display().to_string()),
            ..Default::default()
        }],
        ..RuntimeConfig::default()
    })
    .expect_err("outside an approved root must fail closed");
    assert!(
        err.to_string().contains("views[ws:2].background_image"),
        "must name the selector-qualified key: {err}"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&approved);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn empty_roots_deny_every_image() {
    let root = scratch("deny");
    let path = root.join("red.png");
    std::fs::write(&path, RED_PNG).expect("write fixture");
    let err = Runtime::new(RuntimeConfig {
        background_image: Some(path.display().to_string()),
        ..RuntimeConfig::default()
    })
    .expect_err("deny-by-default roots must reject an image");
    assert!(
        err.to_string().contains("decoration.background_image"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn validate_background_images_gate_matches_startup() {
    // `bitty config check` runs this same pipeline without constructing a
    // runtime; a missing image fails here too.
    let root = scratch("gate");
    let cfg = RuntimeConfig {
        background_image: Some(root.join("absent.png").display().to_string()),
        background_image_roots: vec![root.display().to_string()],
        ..RuntimeConfig::default()
    };
    let err = bitty_runtime::validate_background_images(&cfg).expect_err("gate must reject");
    assert!(
        err.to_string().contains("decoration.background_image"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn no_image_config_opens_nothing_and_presents_no_background() {
    // The `--safe` shape: no image, deny-by-default roots, no rules.
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("default runtime");
    assert_eq!(rt.background_image_count(), 0);
    assert_eq!(rt.background_loads(), 0, "no file may be opened");
    single_leaf(&mut rt);
    let stats = rt.tick().expect("presents");
    assert_eq!(stats.backgrounds, 0);
    let (hits, misses, entries, bytes) = rt.background_raster_stats();
    assert_eq!((hits, misses, entries, bytes), (0, 0, 0, 0));
}
