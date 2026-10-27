//! Kitty decode/raster dyn-seam injection (CTX-1614, #2163).
//!
//! Proves the composition-root binding: a runtime accepts an arbitrary
//! [`bitty_rich::kitty_seam::KittyBackend`] handle
//! ([`Runtime::set_kitty_backend`]) and every decode (transmit/frame) and
//! raster (present) call site dispatches through it. The stub is deliberately
//! unlike the in-tree backend — it returns a sentinel bitmap and can refuse
//! decode — so a fallback to the Core-local free functions would show up as a
//! stored image or a painted blit where this suite expects none.
//!
//! The default binding stays the in-tree
//! [`bitty_rich::kitty_seam::CoreKittyBackend`], so an unbound runtime is
//! byte-identical to the pre-seam behavior (`kitty_apc_wiring` covers the
//! untouched suites).

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bitty_rich::kitty_seam::{
    KittyBackend, KittyDecodeBackend, KittyRasterBackend, KittyTransmitFormat,
};
use bitty_rich::{KittyDecodeError, KittyDecodedImage, KittyPlacedImage, RectPx};
use bitty_runtime::Runtime;

/// 2x2 opaque red RGBA payload (`f=32`), base64 `/wAA//8AAP//AAD//wAA/w==`.
const RED_2X2_B64: &str = "/wAA//8AAP//AAD//wAA/w==";

/// Sentinel RGBA written by every stub decode/raster result.
const STUB_RGBA: [u8; 4] = [0x12, 0x34, 0x56, 0xFF];

/// Recording stub backend: counts every seam call, returns sentinel pixels,
/// and can refuse decode to prove the runtime has no in-tree fallback.
#[derive(Debug)]
struct StubBackend {
    decode_calls: AtomicUsize,
    decode_owned_calls: AtomicUsize,
    raster_calls: AtomicUsize,
    refuse: AtomicBool,
}

impl Default for StubBackend {
    fn default() -> Self {
        Self {
            decode_calls: AtomicUsize::new(0),
            decode_owned_calls: AtomicUsize::new(0),
            raster_calls: AtomicUsize::new(0),
            refuse: AtomicBool::new(false),
        }
    }
}

impl StubBackend {
    fn set_refuse(&self, refuse: bool) {
        self.refuse.store(refuse, Ordering::SeqCst);
    }

    fn decodes(&self) -> usize {
        self.decode_calls.load(Ordering::SeqCst) + self.decode_owned_calls.load(Ordering::SeqCst)
    }

    fn rasters(&self) -> usize {
        self.raster_calls.load(Ordering::SeqCst)
    }

    /// Sentinel bitmap with the requested extent (1x1 when the payload does
    /// not supply dimensions, which only happens for PNG).
    fn bitmap(&self, width: Option<u32>, height: Option<u32>) -> KittyDecodedImage {
        let width = width.unwrap_or(1);
        let height = height.unwrap_or(1);
        let pixels = usize::try_from(u64::from(width) * u64::from(height)).expect("pixel count");
        KittyDecodedImage {
            width,
            height,
            rgba: STUB_RGBA.repeat(pixels),
        }
    }

    fn visible_bytes(&self, visible: RectPx) -> Option<Vec<u8>> {
        self.raster_calls.fetch_add(1, Ordering::SeqCst);
        let pixels = usize::try_from(u64::from(visible.width) * u64::from(visible.height))
            .expect("visible pixel count");
        Some(STUB_RGBA.repeat(pixels))
    }
}

impl KittyDecodeBackend for StubBackend {
    fn decode_transmit(
        &self,
        _format: KittyTransmitFormat,
        width: Option<u32>,
        height: Option<u32>,
        _payload: &[u8],
    ) -> Result<KittyDecodedImage, KittyDecodeError> {
        self.decode_calls.fetch_add(1, Ordering::SeqCst);
        if self.refuse.load(Ordering::SeqCst) {
            return Err(KittyDecodeError::MalformedPng(String::from(
                "stub backend refuses decode",
            )));
        }
        Ok(self.bitmap(width, height))
    }

    fn decode_transmit_owned(
        &self,
        _format: KittyTransmitFormat,
        width: Option<u32>,
        height: Option<u32>,
        _payload: Box<[u8]>,
    ) -> Result<KittyDecodedImage, KittyDecodeError> {
        self.decode_owned_calls.fetch_add(1, Ordering::SeqCst);
        if self.refuse.load(Ordering::SeqCst) {
            return Err(KittyDecodeError::MalformedPng(String::from(
                "stub backend refuses decode",
            )));
        }
        Ok(self.bitmap(width, height))
    }
}

impl KittyRasterBackend for StubBackend {
    fn rasterize_placed(&self, _image: &KittyPlacedImage, rect: RectPx) -> Option<Vec<u8>> {
        self.visible_bytes(rect)
    }

    fn rasterize_placed_clipped(
        &self,
        _image: &KittyPlacedImage,
        _full: RectPx,
        visible: RectPx,
    ) -> Option<Vec<u8>> {
        self.visible_bytes(visible)
    }

    fn rasterize_frame(
        &self,
        _width: u32,
        _height: u32,
        _rgba: &[u8],
        _full: RectPx,
        visible: RectPx,
    ) -> Option<Vec<u8>> {
        self.visible_bytes(visible)
    }
}

fn runtime_with(backend: Arc<StubBackend>) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("defaults build");
    rt.set_kitty_backend(as_dyn_backend(backend));
    rt
}

/// Erases the stub into the seam handle: the stub is a [`KittyBackend`]
/// through the blanket impl, which is the same shape the extension binds.
fn as_dyn_backend<T: KittyBackend + 'static>(
    backend: Arc<T>,
) -> bitty_rich::kitty_seam::KittyBackendHandle {
    backend
}

fn transmit(rt: &mut Runtime) {
    let seq = format!("\x1b_Gf=32,s=2,v=2,m=0;{RED_2X2_B64}\x1b\\");
    rt.handle_pty_bytes(seq.as_bytes());
}

#[test]
fn injected_backend_serves_transmit_and_present() {
    let stub = Arc::new(StubBackend::default());
    let mut rt = runtime_with(stub.clone());

    transmit(&mut rt);

    assert_eq!(rt.kitty_image_count(), 1, "the stub decode was stored");
    assert_eq!(rt.kitty_placement_count(), 1);
    assert_eq!(stub.decodes(), 1, "one decode dispatched through the stub");

    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
    assert!(
        stub.rasters() >= 1,
        "present rasterized through the stub: {} calls",
        stub.rasters()
    );
}

#[test]
fn injected_backend_refusal_blocks_storage_and_paint() {
    let stub = Arc::new(StubBackend::default());
    stub.set_refuse(true);
    let mut rt = runtime_with(stub.clone());

    transmit(&mut rt);

    // The in-tree backend decodes this payload fine; an empty layer proves
    // the runtime called the injected backend and did not fall back.
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
    assert_eq!(stub.decodes(), 1);

    let _ = rt.tick();
    assert_eq!(stub.rasters(), 0, "nothing stored, so nothing rasterized");
}

#[test]
fn rebinding_replaces_the_previous_backend() {
    let first = Arc::new(StubBackend::default());
    let second = Arc::new(StubBackend::default());
    let mut rt = runtime_with(first.clone());
    rt.set_kitty_backend(second.clone());

    transmit(&mut rt);

    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(first.decodes(), 0, "the replaced backend is never called");
    assert_eq!(second.decodes(), 1);
}

#[test]
fn default_binding_keeps_the_in_tree_backend() {
    let mut rt = Runtime::with_defaults().expect("defaults build");

    transmit(&mut rt);

    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
}
