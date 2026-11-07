//! Off-thread presentation for the Wayland swapchain (bitty#2830).
//!
//! # Why this module exists
//!
//! On Wayland with explicit sync (Hyprland/aquamarine + NVIDIA), a
//! `get_current_texture` acquire can park the calling thread inside
//! `drm_syncobj_timeline_wait_ioctl` until the compositor next samples the
//! surface. The driver wait has no timeout. While the event-loop thread called
//! acquire/present itself, a surface that stopped being composited froze
//! input, PTY pumping, plugin timers, and compositing for the whole stall
//! (17.49 s and 13.09 s observed live).
//!
//! This module moves **all** acquire/present work onto a dedicated
//! `bitty-present` worker thread that owns the [`Surface`] and a shared
//! [`GpuContext`]. The event-loop thread only:
//!
//! 1. enqueues the newest fully assembled frame into a **bounded one-slot**
//!    mailbox (a queued frame is replaced by the newest, carrying the union
//!    of both frames' damage; when the union would exceed
//!    [`PRESENT_MAX_CARRIED_DAMAGE_RECTS`] rects the carried damage collapses
//!    to `full` and the next frame repaints the whole surface),
//! 2. sends `Configure` (extent/opacity) and `Pause`/`Resume` messages,
//! 3. drains bounded acknowledgements, and
//! 4. reads lock-free in-flight gauges.
//!
//! It never waits on the worker: `submit` only takes a short mutex, and
//! `shutdown_bounded` gives up after the configured bound. An abandoned
//! worker (stuck detection) owns its own `WorkerShared` and backend; when
//! the blocking driver call finally returns it observes `stop`, exits, and
//! drops its surface — it can never touch the replacement worker's state.
//!
//! # Scope
//!
//! The asynchronous path is selected by the embedder for Wayland on Linux
//! only ([`bitty_platform::DisplayServer::is_wayland`]). X11, macOS, Windows,
//! and every headless/test surface keep the synchronous `Surface::present_*`
//! path unchanged, so no existing render/runtime/terminal test changes
//! behavior.
//!
//! # Acknowledgements
//!
//! Every attempted present reports exactly one [`PresentAck`] through a
//! bounded ring: `presented`, `outdated`, `lost`, `timeout`, `other`, or
//! `error`. The runtime turns those into the #2570 retry/backoff and
//! telemetry path, so retry counters still count only committed presents
//! (the render crate records `present_ok`/acquire classes inside the worker
//! when the driver call actually runs). The ring drops its oldest entry when
//! full (counted as `presenter_acks_dropped`); the newest outcome always
//! survives, which is the one that decides the next action.
//!
//! ## Ack ownership and the configure-failure case
//!
//! The **presenter worker** owns every acknowledgement: it is the only
//! component that runs `configure`/acquire/present, so the event-loop thread
//! never synthesises one. One ack describes **one submitted frame** and
//! carries that frame's [`PresentAck::token`]; consumers therefore attribute
//! acks by token, never by arrival position (several acks can be waiting in
//! the ring, and a superseded frame's ack can arrive after a newer one).
//!
//! A configure request is not a frame. A configure that fails while a frame
//! was taken with it acknowledges **that frame's token** as `error`, because
//! a frame built for the old extent must never be presented. A configure that
//! fails with **no waiting frame** — the common case, since `configure` and
//! `resize` are sent on their own, ahead of the next frame — publishes **no
//! ack at all**: there is no frame to attribute it to, and a synthetic
//! tokenless ack would break the one-ack-per-submitted-frame mapping every
//! consumer relies on (it could be observed ahead of the ack for a frame
//! submitted moments later, bitty#2980). The worker exits instead, and that
//! exit is the signal: the embedder observes `is_active() == false`, retains
//! the damage, and rebuilds a surface that can actually be configured.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bitty_platform::{PhysicalSize, SurfaceTarget};

use crate::atlas::AtlasDims;
use crate::error::RenderError;
use crate::geometry::RectPx;
use crate::gpu::{GpuContext, PresentFailure, PresentFailureClass, PresentStats, Surface};
use crate::grid::{DrawList, ThemePalette};
use crate::telemetry;

/// Default bound after which a present still in flight is declared stuck.
///
/// Invariant: the compositor normally services an acquire within a frame or
/// two; a wait beyond this bound means the surface stopped being composited
/// and the worker's driver call may never return. The value is short enough
/// that the user never sees a multi-second freeze and long enough that a
/// normal 60 Hz present (16.7 ms) can never trip it.
pub const PRESENT_STUCK_BOUND: Duration = Duration::from_millis(1500);

/// Default bound for joining the worker at shutdown.
///
/// Invariant: process exit must never depend on a driver call returning. If
/// the worker cannot stop within this bound it is detached and the process
/// exits without it.
pub const PRESENT_SHUTDOWN_BOUND: Duration = Duration::from_millis(250);

/// Capacity of the bounded worker -> UI acknowledgement ring.
///
/// Invariant: acknowledgement storage is fixed; a full ring drops its oldest
/// (already-superseded) entry and counts the drop. The newest outcome—the one
/// that decides the next present action—is never dropped.
pub const PRESENT_ACK_CAPACITY: usize = 32;

/// Consecutive worker recoveries allowed without a presented frame.
///
/// Invariant: a compositor that never services a replacement surface cannot
/// make the process accumulate abandoned worker threads without bound. After
/// this many consecutive recoveries the presenter stops creating surfaces and
/// the embedder stays responsive (headless) until a frame from the last
/// worker is actually presented, which resets the budget.
pub const PRESENT_RECOVERY_LIMIT: u32 = 3;

/// Consecutive replacement-surface creation failures allowed before the
/// presenter stops trying.
///
/// Invariant: a compositor/driver that refuses every new surface cannot make
/// the event loop create one every retry tick forever. This budget is
/// separate from [`PRESENT_RECOVERY_LIMIT`]: a failed creation abandons no
/// worker, so it must not consume the abandoned-thread budget. A presented
/// frame from the current worker resets both.
pub const PRESENT_CREATION_FAILURE_LIMIT: u32 = 5;

/// Maximum damage rectangles carried when a queued frame is replaced.
///
/// Invariant: the carried union is bounded. Above this count the carry
/// collapses to `full`, which forces a whole-surface repaint; the UI can
/// therefore never lose a replaced frame's damage or allocate an unbounded
/// damage list.
pub const PRESENT_MAX_CARRIED_DAMAGE_RECTS: usize = 16;

/// Wake callback invoked after each acknowledgement is published.
///
/// The desktop embedder points this at the event-loop proxy so an ack
/// resumes the loop immediately instead of waiting for the next timer.
pub type PresentWake = Arc<dyn Fn() + Send + Sync>;

/// Factory used to build a replacement backend after a stuck present.
///
/// Receives the latest configured extent/opacity so a fresh surface is
/// configured before the new worker ever acquires.
pub type PresenterFactory =
    Arc<dyn Fn(PhysicalSize, f32) -> Result<Box<dyn PresentBackend>, RenderError> + Send + Sync>;

/// A bounded backend that performs the actual configure/acquire/present.
///
/// Implemented by [`GpuSurfaceBackend`] for production and by deterministic
/// fakes in tests. Only the presenter worker calls these methods.
pub trait PresentBackend: Send {
    /// Applies `extent`/`opacity`; returns whether premultiplied alpha was
    /// accepted by the platform.
    ///
    /// # Errors
    ///
    /// Propagates the surface configuration failure.
    fn configure(&mut self, extent: PhysicalSize, opacity: f32) -> Result<bool, RenderError>;

    /// Acquires and presents one frame.
    ///
    /// # Errors
    ///
    /// Returns a classified acquire failure or an unclassified render error.
    fn present(&mut self, frame: &FramePayload) -> Result<PresentStats, PresentFailure>;

    /// Whether the last configure accepted premultiplied alpha.
    ///
    /// Defaults to `true` for backends that do not carry an alpha mode.
    fn alpha_supported(&self) -> bool {
        true
    }
}

/// Production backend: owns the real [`Surface`] and shares the GPU context.
pub struct GpuSurfaceBackend {
    surface: Surface,
    gpu: Arc<GpuContext>,
}

impl GpuSurfaceBackend {
    /// Wraps a configured surface and its GPU context.
    #[must_use]
    pub fn new(surface: Surface, gpu: Arc<GpuContext>) -> Self {
        Self { surface, gpu }
    }
}

impl PresentBackend for GpuSurfaceBackend {
    fn configure(&mut self, extent: PhysicalSize, opacity: f32) -> Result<bool, RenderError> {
        self.surface
            .configure_with_opacity(&self.gpu, extent, opacity)?;
        Ok(self.surface.opacity_alpha_supported())
    }

    fn alpha_supported(&self) -> bool {
        self.surface.opacity_alpha_supported()
    }

    fn present(&mut self, frame: &FramePayload) -> Result<PresentStats, PresentFailure> {
        // Consume the carry: the worker has no base image to merge a partial
        // update onto, so only a whole-surface frame is admissible.
        require_full_damage(&frame.damage).map_err(PresentFailure::Error)?;
        // The surface clear color follows the resolved theme; installing it
        // on the worker keeps theme reloads off the event loop.
        self.surface.set_theme_palette(frame.theme);
        self.surface.present_draw_list_color_classified(
            &self.gpu,
            &frame.draw_list,
            frame
                .atlas
                .as_ref()
                .map(|(bytes, dims)| (bytes.as_slice(), *dims)),
            frame
                .color_atlas
                .as_ref()
                .map(|(bytes, dims)| (bytes.as_slice(), *dims)),
        )
    }
}

/// Bounded damage carry for one frame.
///
/// The runtime's combined frame is a full-surface composite, so a replacement
/// frame already contains every pixel of the frame it replaced. The carry is
/// still explicit: a future partial-frame path must merge rather than drop a
/// superseded frame's damage. The production backend refuses frames whose
/// carry is not `full` (see `require_full_damage`), so today every async frame
/// is a whole-surface repaint and the union below can never lose pixels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DamageCarry {
    /// The frame paints the whole surface; individual rects are irrelevant.
    pub full: bool,
    /// Bounded damage rectangles in physical window pixels.
    pub rects: Vec<RectPx>,
}

impl DamageCarry {
    /// A whole-surface repaint carry.
    #[must_use]
    pub const fn full() -> Self {
        Self {
            full: true,
            rects: Vec::new(),
        }
    }

    /// Builds a carry from a frame plan's dirty rects.
    #[must_use]
    pub fn from_rects(rects: Vec<RectPx>, full: bool) -> Self {
        let mut carry = Self {
            full,
            rects: Vec::new(),
        };
        if !full {
            for rect in rects {
                carry.add(rect);
            }
        }
        carry
    }

    /// Merges `other` into this carry.
    ///
    /// Contained rectangles are absorbed; the result is capped at
    /// [`PRESENT_MAX_CARRIED_DAMAGE_RECTS`], beyond which it collapses to
    /// `full` (a whole-surface repaint is the only representation that cannot
    /// lose damage).
    pub fn merge(&mut self, other: &Self) {
        if self.full || other.full {
            self.full = true;
            self.rects.clear();
            return;
        }
        for rect in &other.rects {
            self.add(*rect);
            if self.full {
                return;
            }
        }
    }

    /// Adds one rectangle with containment absorption and the bounded cap.
    fn add(&mut self, rect: RectPx) {
        if self.full {
            return;
        }
        if self
            .rects
            .iter()
            .any(|existing| existing.contains_rect(&rect))
        {
            return;
        }
        self.rects.retain(|existing| !rect.contains_rect(existing));
        self.rects.push(rect);
        if self.rects.len() > PRESENT_MAX_CARRIED_DAMAGE_RECTS {
            self.full = true;
            self.rects.clear();
        }
    }
}

/// The off-thread backend only paints full-surface frames.
///
/// The combined compositor always produces a complete frame, so a carry that
/// is not `full` would mean a partial update the worker cannot merge safely.
/// Refusing it keeps the contract honest instead of painting over the old
/// pixels with an unknown base.
///
/// # Errors
///
/// [`RenderError::InvalidInput`] when `damage.full` is false.
fn require_full_damage(damage: &DamageCarry) -> Result<(), RenderError> {
    if damage.full {
        Ok(())
    } else {
        Err(RenderError::InvalidInput {
            reason: "off-thread presenter requires a full-surface damage carry",
        })
    }
}

/// One fully assembled frame handed to the presenter worker.
///
/// The UI thread builds and owns this value; after `submit` the worker owns
/// it exclusively until the acknowledgement is published. `damage` must be a
/// whole-surface carry (the runtime always sends [`DamageCarry::full`]);
/// partial carries are refused by the backend.
pub struct FramePayload {
    /// Draw list for this frame (a complete composite).
    pub draw_list: Box<DrawList>,
    /// Monochrome atlas texels and their dimensions, when the frame samples
    /// them.
    pub atlas: Option<(Vec<u8>, AtlasDims)>,
    /// Color atlas texels and their dimensions, when the frame samples them.
    pub color_atlas: Option<(Vec<u8>, AtlasDims)>,
    /// Resolved theme palette for the surface clear color.
    pub theme: ThemePalette,
    /// Damage covered by this frame, merged across replacements.
    pub damage: DamageCarry,
}

/// Present outcome reported by the worker.
#[derive(Debug)]
pub enum PresentOutcome {
    /// The frame was acquired, drawn, and committed to the compositor.
    Presented(PresentStats),
    /// Acquire reported an outdated swap chain (reconfigure needed).
    Outdated(String),
    /// Acquire reported a lost surface (reconfigure needed).
    Lost(String),
    /// Acquire reported a driver wait timeout.
    Timeout(String),
    /// Acquire reported out-of-memory or an unknown surface status.
    Other(String),
    /// A validation, configure, or draw failure outside the acquire classes.
    Error(String),
}

impl PresentOutcome {
    /// True when the frame reached the compositor.
    #[must_use]
    pub fn is_presented(&self) -> bool {
        matches!(self, Self::Presented(_))
    }

    /// The diagnostic detail for a failed outcome (`None` when presented).
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Presented(_) => None,
            Self::Outdated(detail)
            | Self::Lost(detail)
            | Self::Timeout(detail)
            | Self::Other(detail)
            | Self::Error(detail) => Some(detail),
        }
    }
}

/// One bounded acknowledgement for one attempted present.
#[derive(Debug)]
pub struct PresentAck {
    /// Token of the submitted frame this outcome belongs to.
    ///
    /// Ack ownership: the presenter worker publishes an ack only for a frame
    /// it actually took from the mailbox, so every ack names a submission.
    /// Consumers match on this token; arrival order carries no meaning.
    pub token: u64,
    /// Classified outcome.
    pub outcome: PresentOutcome,
}

impl PresentAck {
    /// The token this outcome belongs to.
    #[must_use]
    pub fn token(&self) -> u64 {
        self.token
    }

    /// True when the frame reached the compositor.
    #[must_use]
    pub fn is_presented(&self) -> bool {
        self.outcome.is_presented()
    }
}

/// Presenter tuning. Every field has a named default and a documented
/// invariant (see the module constants).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresenterConfig {
    /// Bound after which an in-flight present is declared stuck.
    pub stuck_bound: Duration,
    /// Bound for joining the worker at shutdown.
    pub shutdown_bound: Duration,
    /// Capacity of the acknowledgement ring (at least one).
    pub ack_capacity: usize,
    /// Consecutive recoveries allowed without a presented frame.
    pub recovery_limit: u32,
    /// Consecutive surface-creation failures allowed without a presented
    /// frame (separate from the abandoned-worker budget).
    pub creation_failure_limit: u32,
}

impl Default for PresenterConfig {
    fn default() -> Self {
        Self {
            stuck_bound: PRESENT_STUCK_BOUND,
            shutdown_bound: PRESENT_SHUTDOWN_BOUND,
            ack_capacity: PRESENT_ACK_CAPACITY,
            recovery_limit: PRESENT_RECOVERY_LIMIT,
            creation_failure_limit: PRESENT_CREATION_FAILURE_LIMIT,
        }
    }
}

/// A frame waiting in the one-slot mailbox.
struct QueuedFrame {
    token: u64,
    frame: FramePayload,
}

/// A configure request waiting for the worker.
#[derive(Clone, Copy)]
struct ConfigureRequest {
    extent: PhysicalSize,
    opacity: f32,
    /// True when this request came from a resize/DPI reflow, so the worker
    /// records the same `resize` telemetry as the synchronous path.
    record_resize: bool,
}

#[derive(Default)]
struct Slot {
    frame: Option<QueuedFrame>,
    configure: Option<ConfigureRequest>,
}

/// State shared between one presenter handle and its worker.
///
/// A replacement worker gets a brand-new `WorkerShared`, so an abandoned
/// worker can only ever observe and mutate its own mailbox, gauges, and ack
/// ring.
struct WorkerShared {
    generation: u32,
    slot: Mutex<Slot>,
    cv: Condvar,
    paused: AtomicBool,
    stop: AtomicBool,
    /// True while the worker is blocked in `Condvar::wait` with no job.
    ///
    /// Test/diagnostic seam: negative assertions ("paused, so no acquire")
    /// can synchronise on the worker actually parking instead of sleeping a
    /// fixed number of milliseconds.
    parked: AtomicBool,
    acks: Mutex<VecDeque<PresentAck>>,
    ack_capacity: usize,
    wake: Mutex<Option<PresentWake>>,
    /// Nanoseconds since `epoch` plus one while a present is in flight; zero
    /// when idle. Plus one reserves zero as the idle sentinel.
    in_flight: AtomicU64,
    /// The packed value published to the process telemetry gauge for the
    /// current in-flight episode (zero when idle), used to clear only this
    /// worker's episode.
    published: AtomicU64,
    epoch: Instant,
    stuck: AtomicU64,
    coalesced: AtomicU64,
    alpha_supported: AtomicBool,
    /// Signalled once the worker loop (or its panic guard) has exited.
    exited: Mutex<bool>,
    exit_cv: Condvar,
}

impl WorkerShared {
    fn new(generation: u32, config: PresenterConfig, wake: Option<PresentWake>) -> Self {
        Self {
            generation,
            slot: Mutex::new(Slot::default()),
            cv: Condvar::new(),
            paused: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            parked: AtomicBool::new(false),
            acks: Mutex::new(VecDeque::new()),
            ack_capacity: config.ack_capacity.max(1),
            wake: Mutex::new(wake),
            in_flight: AtomicU64::new(0),
            published: AtomicU64::new(0),
            epoch: Instant::now(),
            stuck: AtomicU64::new(0),
            coalesced: AtomicU64::new(0),
            alpha_supported: AtomicBool::new(true),
            exited: Mutex::new(false),
            exit_cv: Condvar::new(),
        }
    }

    fn lock_slot(&self) -> MutexGuard<'_, Slot> {
        self.slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_acks(&self) -> MutexGuard<'_, VecDeque<PresentAck>> {
        self.acks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Marks the start of one acquire/present episode.
    fn begin(&self) {
        let since = u64::try_from(self.epoch.elapsed().as_nanos())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        self.in_flight.store(since, Ordering::Release);
        let packed = (u64::from(self.generation) << 32) | telemetry::presenter_now_ms();
        self.published.store(packed, Ordering::Release);
        telemetry::presenter_begin(self.generation, packed & 0xFFFF_FFFF);
    }

    /// Marks the end of the episode (also on a panic path).
    fn end(&self) {
        self.in_flight.store(0, Ordering::Release);
        let packed = self.published.swap(0, Ordering::AcqRel);
        if packed != 0 {
            telemetry::presenter_end(packed);
        }
    }

    /// Age of the current in-flight episode, if any.
    fn in_flight_age(&self, now: Instant) -> Option<Duration> {
        let stored = self.in_flight.load(Ordering::Acquire);
        if stored == 0 {
            return None;
        }
        let since = self
            .epoch
            .checked_add(Duration::from_nanos(stored.saturating_sub(1)))?;
        Some(now.saturating_duration_since(since))
    }

    /// Publishes one acknowledgement into the bounded ring.
    fn push_ack(&self, ack: PresentAck) {
        {
            let mut queue = self.lock_acks();
            if queue.len() >= self.ack_capacity {
                let _ = queue.pop_front();
                telemetry::presenter_ack_dropped();
            }
            queue.push_back(ack);
        }
        let wake = self
            .wake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(wake) = wake {
            wake();
        }
    }

    /// Marks the worker as exited and wakes a bounded shutdown waiter.
    ///
    /// Also publishes `stop`: a worker that returned (or unwound) without a
    /// shutdown request leaves an otherwise "active" handle with no consumer,
    /// so `is_active` must report it dead and the embedder can recover.
    fn mark_exited(&self) {
        self.stop.store(true, Ordering::Release);
        {
            let mut exited = self
                .exited
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *exited = true;
            self.exit_cv.notify_all();
        }
        // An unexpected exit (panic) must wake the embedder so it can recover
        // instead of parking in `Wait` with a dead worker and no in-flight
        // gauge to trip.
        let wake = self
            .wake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(wake) = wake {
            wake();
        }
    }

    /// Waits up to `bound` for the worker to exit.
    fn wait_exited(&self, bound: Duration) -> bool {
        let deadline = Instant::now().checked_add(bound);
        let mut exited = self
            .exited
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if *exited {
                return true;
            }
            let Some(deadline) = deadline else {
                return false;
            };
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, timeout) = self
                .exit_cv
                .wait_timeout(exited, deadline.saturating_duration_since(now))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            exited = guard;
            if timeout.timed_out() && !*exited {
                return false;
            }
        }
    }

    /// Requeues a taken frame because the worker paused before acquiring.
    fn requeue(&self, frame: QueuedFrame) {
        let mut slot = self.lock_slot();
        match slot.frame.take() {
            Some(mut newer) => {
                // The queued frame is newer; keep it and carry the older
                // frame's damage forward.
                newer.frame.damage.merge(&frame.frame.damage);
                slot.frame = Some(newer);
            }
            None => slot.frame = Some(frame),
        }
    }

    /// Signals stop and wakes a waiting worker.
    ///
    /// The atomic is stored while holding the same mutex the worker checks it
    /// under, so a stop between the worker's check and its `wait` cannot be
    /// lost.
    fn request_stop(&self) {
        let _slot = self.lock_slot();
        self.stop.store(true, Ordering::Release);
        self.cv.notify_all();
    }

    /// Clears/sets pause with the slot lock held for the same reason.
    fn set_paused(&self, paused: bool) {
        let _slot = self.lock_slot();
        self.paused.store(paused, Ordering::Release);
        self.cv.notify_all();
    }
}

/// Body of the `bitty-present` worker thread.
///
/// The backend owns the only `Surface`, so it is dropped **before** the exit
/// signal: a slow destructor then delays the exit marker (and a bounded
/// `shutdown_bounded` detaches) instead of making `join` overrun its bound.
/// A panic in configure/present unwinds into the outer `catch_unwind`, and the
/// backend is then dropped inside its own guard: a panicking destructor also
/// cannot skip the exit signal.
fn worker_main(shared: Arc<WorkerShared>, backend: Box<dyn PresentBackend>) {
    let mut backend = Some(backend);
    let _ = catch_unwind(AssertUnwindSafe(|| worker_loop(&shared, &mut backend)));
    // A panicking destructor must not skip the exit signal. The panic is
    // contained here; the worker is already leaving and the embedder observes
    // a clean exit so `shutdown_bounded` cannot hang.
    let _ = catch_unwind(AssertUnwindSafe(|| drop(backend.take())));
    shared.mark_exited();
}

/// Worker loop, borrowed so `worker_main` controls drop order.
fn worker_loop(shared: &Arc<WorkerShared>, backend: &mut Option<Box<dyn PresentBackend>>) {
    loop {
        let (frame, configure) = {
            let mut slot = shared.lock_slot();
            loop {
                if shared.stop.load(Ordering::Acquire) {
                    return;
                }
                if !shared.paused.load(Ordering::Acquire)
                    && (slot.frame.is_some() || slot.configure.is_some())
                {
                    break;
                }
                shared.parked.store(true, Ordering::Release);
                slot = shared
                    .cv
                    .wait(slot)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                shared.parked.store(false, Ordering::Release);
            }
            (slot.frame.take(), slot.configure.take())
        };

        if let Some(request) = configure {
            let backend_ref = backend.as_mut().expect("worker backend present");
            match backend_ref.configure(request.extent, request.opacity) {
                Ok(alpha) => {
                    shared.alpha_supported.store(alpha, Ordering::Release);
                    if request.record_resize {
                        telemetry::resize();
                    }
                }
                Err(err) => {
                    // Never present a frame built for the old extent.
                    //
                    // Ack ownership (bitty#2980): the worker is the only
                    // component that may publish an ack, and an ack always
                    // names the frame it belongs to. When a frame was taken
                    // with this failing configure, ack *that frame's* token as
                    // failed so the runtime retains its damage and arms the
                    // #2570 retry. When no frame was taken — `configure` and
                    // `resize` are sent on their own tick — there is nothing
                    // to attribute an ack to, so publish none. A synthetic
                    // tokenless ack here would be observable ahead of the ack
                    // for a frame submitted moments later, and callers that
                    // read "the next ack" would mis-attribute it.
                    //
                    // The exit below is the signal for that case: the
                    // embedder sees `is_active() == false`, keeps the damage,
                    // and recovery builds a surface that can be configured.
                    if let Some(frame) = frame.as_ref() {
                        shared.push_ack(PresentAck {
                            token: frame.token,
                            outcome: PresentOutcome::Error(format!("configure failed: {err}")),
                        });
                    }
                    return;
                }
            }
        }

        let Some(frame) = frame else {
            continue;
        };
        // Pause may have closed between taking the frame and acquiring. The
        // worker must not start an acquire while paused; the frame waits for
        // resume (newest queued frame wins if one arrives meanwhile).
        if shared.paused.load(Ordering::Acquire) {
            shared.requeue(frame);
            continue;
        }

        shared.begin();
        let backend_ref = backend.as_mut().expect("worker backend present");
        let mut panicked = false;
        let outcome = match catch_unwind(AssertUnwindSafe(|| backend_ref.present(&frame.frame))) {
            Ok(Ok(stats)) => PresentOutcome::Presented(stats),
            Ok(Err(PresentFailure::Surface { class, detail })) => match class {
                PresentFailureClass::Outdated => PresentOutcome::Outdated(detail),
                PresentFailureClass::Lost => PresentOutcome::Lost(detail),
                PresentFailureClass::Timeout => PresentOutcome::Timeout(detail),
                PresentFailureClass::Other => PresentOutcome::Other(detail),
            },
            Ok(Err(PresentFailure::Error(err))) => PresentOutcome::Error(err.to_string()),
            Err(_) => {
                panicked = true;
                PresentOutcome::Error(String::from("present backend panicked"))
            }
        };
        shared.end();
        shared.push_ack(PresentAck {
            token: frame.token,
            outcome,
        });
        if panicked {
            // A panicked backend may have poisoned its surface; exit so the
            // embedder can recreate the worker on its next tick.
            return;
        }
        // Loop top: stop/pause is re-checked before the next acquire.
    }
}

/// Monotonic presenter generation; a replacement worker always gets a new
/// value so an abandoned worker's gauge update cannot be mistaken for the
/// live one.
static NEXT_GENERATION: AtomicU32 = AtomicU32::new(1);

fn next_generation() -> u32 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// UI-side handle to the off-thread presenter.
///
/// Dropping the handle (and every clone of its wake callback) signals the
/// worker to stop; a worker blocked in a driver call exits on its own when
/// the call returns. Nothing the handle does waits unboundedly.
pub struct Presenter {
    shared: Arc<WorkerShared>,
    worker: Option<std::thread::JoinHandle<()>>,
    factory: PresenterFactory,
    config: PresenterConfig,
    generation: u32,
    next_token: u64,
    last_extent: PhysicalSize,
    last_opacity: f32,
    next_worker_wake: Option<PresentWake>,
    /// Stuck-present episodes across every worker generation. Kept on the
    /// handle (not the per-worker shared state) so recovery does not reset
    /// the evidence the watchdog reports.
    stuck_total: u64,
    /// Consecutive abandoned workers since the last presented frame, bounded
    /// by [`PresenterConfig::recovery_limit`].
    consecutive_recoveries: u32,
    /// Consecutive replacement-surface creation failures, bounded by
    /// [`PresenterConfig::creation_failure_limit`]. Kept separate from
    /// `consecutive_recoveries`: a failed creation abandons no worker.
    creation_failures: u32,
    /// Workers abandoned on recovery, used to notice when the last one exits
    /// so the embedder can force one correcting repaint (a late abandoned
    /// present must not leave a stale frame on screen).
    abandoned: Vec<std::sync::Weak<WorkerShared>>,
}

impl std::fmt::Debug for Presenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Presenter")
            .field("generation", &self.generation)
            .field("active", &self.is_active())
            .field("last_extent", &self.last_extent)
            .field("last_opacity", &self.last_opacity)
            .finish_non_exhaustive()
    }
}

impl Presenter {
    /// Spawns the `bitty-present` worker for an already-built backend.
    ///
    /// `factory` builds a fresh, configured backend after a stuck-present
    /// recovery. `first_backend` is the backend built by the caller (the
    /// embedder configures the initial surface before transferring
    /// ownership).
    ///
    /// # Errors
    ///
    /// [`RenderError::UpstreamGraphics`] when the OS refuses to start the
    /// worker thread.
    pub fn spawn(
        factory: PresenterFactory,
        first_backend: Box<dyn PresentBackend>,
        extent: PhysicalSize,
        opacity: f32,
        config: PresenterConfig,
        wake: Option<PresentWake>,
    ) -> Result<Self, RenderError> {
        let generation = next_generation();
        let alpha_supported = first_backend.alpha_supported();
        let shared = Arc::new(WorkerShared::new(generation, config, wake.clone()));
        shared
            .alpha_supported
            .store(alpha_supported, Ordering::Release);
        let worker = spawn_worker(Arc::clone(&shared), first_backend)?;
        Ok(Self {
            shared,
            worker: Some(worker),
            factory,
            config,
            generation,
            next_token: 0,
            last_extent: extent,
            last_opacity: opacity,
            next_worker_wake: wake,
            stuck_total: 0,
            consecutive_recoveries: 0,
            creation_failures: 0,
            abandoned: Vec::new(),
        })
    }

    /// Spawns the production presenter from an owned surface and GPU context.
    ///
    /// `surface` **must already be configured** for `extent`/`opacity`: the
    /// embedder does that before transferring ownership (it also needs the
    /// configure result to decide whether to attach at all). No configure runs
    /// here. After this call the surface is worker-owned.
    ///
    /// The recovery factory runs on the event-loop thread, but it only
    /// creates and configures a surface; it never calls acquire/present, so it
    /// cannot hit the explicit-sync timeline wait. It is still an unvalidated
    /// driver call on the UI thread (see the review notes).
    ///
    /// # Errors
    ///
    /// Propagates a worker-spawn failure.
    pub fn spawn_gpu(
        gpu: GpuContext,
        surface: Surface,
        target: SurfaceTarget,
        extent: PhysicalSize,
        opacity: f32,
        config: PresenterConfig,
        wake: Option<PresentWake>,
    ) -> Result<Self, RenderError> {
        let gpu = Arc::new(gpu);
        let factory_gpu = Arc::clone(&gpu);
        let factory_target = target.clone();
        let factory: PresenterFactory = Arc::new(move |extent, opacity| {
            let surface = factory_gpu.create_surface(&factory_target)?;
            surface.configure_with_opacity(&factory_gpu, extent, opacity)?;
            Ok(
                Box::new(GpuSurfaceBackend::new(surface, Arc::clone(&factory_gpu)))
                    as Box<dyn PresentBackend>,
            )
        });
        let first = Box::new(GpuSurfaceBackend::new(surface, gpu));
        Self::spawn(factory, first, extent, opacity, config, wake)
    }

    /// The presenter generation (changes on every recovery).
    #[must_use]
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// True when a worker is running and has not been asked to stop.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.worker.is_some() && !self.shared.stop.load(Ordering::Acquire)
    }

    /// True while the surface-visibility gate holds the worker paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Acquire)
    }

    /// Bound after which an in-flight present is stuck.
    #[must_use]
    pub fn stuck_bound(&self) -> Duration {
        self.config.stuck_bound
    }

    /// Bound used when joining the worker at shutdown.
    #[must_use]
    pub fn shutdown_bound(&self) -> Duration {
        self.config.shutdown_bound
    }

    /// Latest configured extent.
    #[must_use]
    pub fn extent(&self) -> PhysicalSize {
        self.last_extent
    }

    /// Latest requested opacity.
    #[must_use]
    pub fn opacity(&self) -> f32 {
        self.last_opacity
    }

    /// Whether the platform accepted premultiplied alpha at the last
    /// configure.
    #[must_use]
    pub fn alpha_supported(&self) -> bool {
        self.shared.alpha_supported.load(Ordering::Acquire)
    }

    /// Age of the current acquire/present episode, or `None` when idle.
    #[must_use]
    pub fn in_flight_age(&self, now: Instant) -> Option<Duration> {
        self.shared.in_flight_age(now)
    }

    /// Stuck-present episodes recorded on this presenter's live worker.
    #[must_use]
    pub fn stuck_presents(&self) -> u64 {
        self.stuck_total
    }

    /// True when the bounded *surface-creation* budget is spent.
    ///
    /// Only replacement-surface creation stops: the current worker stays
    /// active and must keep receiving frames, because only a presented frame
    /// can reset this budget. A spent budget is therefore not a reason to
    /// refuse `submit`, `present_owed`, or a tick.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.consecutive_recoveries >= self.config.recovery_limit.max(1)
            || self.creation_failures >= self.config.creation_failure_limit.max(1)
    }

    /// Consecutive surface-creation failures (diagnostics/tests).
    #[must_use]
    pub fn creation_failures(&self) -> u32 {
        self.creation_failures
    }

    /// Backends still owned by workers abandoned during recovery.
    ///
    /// The embedder uses this to force one correcting full repaint after the
    /// last abandoned worker exits: an abandoned present can land after the
    /// replacement's commit and must not leave a stale frame forever.
    pub fn abandoned_alive(&mut self) -> usize {
        self.abandoned.retain(|weak| weak.strong_count() > 0);
        self.abandoned.len()
    }

    /// True while the worker is parked in its mailbox wait.
    ///
    /// Diagnostic/test seam: a negative assertion ("paused, so no acquire")
    /// can wait for the worker to actually park instead of sleeping.
    #[must_use]
    pub fn is_worker_parked(&self) -> bool {
        self.shared.parked.load(Ordering::Acquire)
    }

    /// Number of acknowledgements waiting in the bounded ring.
    #[must_use]
    pub fn queued_acks(&self) -> usize {
        self.shared.lock_acks().len()
    }

    /// Frames replaced in the slot before acquisition.
    #[must_use]
    pub fn coalesced_frames(&self) -> u64 {
        self.shared.coalesced.load(Ordering::Relaxed)
    }

    /// Installs or clears the acknowledgement wake callback.
    pub fn set_waker(&mut self, wake: Option<PresentWake>) {
        self.next_worker_wake = wake.clone();
        *self
            .shared
            .wake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = wake;
    }

    /// Pauses the worker before its next acquire.
    ///
    /// A present already blocked in the driver stays blocked off-thread; when
    /// it returns the worker observes the pause and performs no new acquire.
    pub fn pause(&self) {
        self.shared.set_paused(true);
    }

    /// Resumes the worker; a queued frame (the latest submitted) is
    /// presented next.
    pub fn resume(&self) {
        self.shared.set_paused(false);
    }

    /// Records one stuck-present episode on the live worker.
    pub fn note_stuck(&mut self) {
        let _ = self.shared.stuck.fetch_add(1, Ordering::Relaxed);
        self.stuck_total = self.stuck_total.saturating_add(1);
        telemetry::presenter_stuck();
    }

    /// Sends an opacity/configure request; the worker applies it before its
    /// next acquire. Only the newest request is kept.
    pub fn configure(&mut self, extent: PhysicalSize, opacity: f32) {
        self.queue_configure(extent, opacity, false);
    }

    /// Sends a resize request; like [`Self::configure`] but records the
    /// `resize` telemetry the synchronous `Surface::resize` path emits.
    pub fn resize(&mut self, extent: PhysicalSize, opacity: f32) {
        self.queue_configure(extent, opacity, true);
    }

    fn queue_configure(&mut self, extent: PhysicalSize, opacity: f32, record_resize: bool) {
        self.last_extent = extent;
        self.last_opacity = opacity;
        self.shared.lock_slot().configure = Some(ConfigureRequest {
            extent,
            opacity,
            record_resize,
        });
        self.shared.cv.notify_all();
    }

    /// Enqueues the newest frame, replacing (and carrying the damage of) any
    /// frame still waiting in the slot.
    ///
    /// Returns the submission token, or `None` when no worker is active (the
    /// caller retains the frame's damage and retries recovery).
    pub fn submit(&mut self, mut frame: FramePayload) -> Option<u64> {
        // A spent surface-creation budget must not stop the *current* worker
        // from being fed: its ack is the only event that can reset the budget.
        if !self.is_active() {
            return None;
        }
        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);
        let mut coalesced = 0u64;
        {
            let mut slot = self.shared.lock_slot();
            if let Some(old) = slot.frame.take() {
                // The newest frame is a complete composite; carrying the old
                // frame's damage keeps a future partial-frame path lossless.
                frame.damage.merge(&old.frame.damage);
                coalesced = 1;
            }
            slot.frame = Some(QueuedFrame { token, frame });
        }
        if coalesced > 0 {
            let _ = self
                .shared
                .coalesced
                .fetch_add(coalesced, Ordering::Relaxed);
            telemetry::presenter_coalesced(coalesced);
        }
        self.shared.cv.notify_one();
        Some(token)
    }

    /// Drains up to `max` acknowledgements, oldest first.
    ///
    /// A presented frame proves the surface works again, so it resets the
    /// bounded recovery budget.
    ///
    /// Attribute the results by [`PresentAck::token`]: the ring holds one ack
    /// per frame the worker took, and their arrival order is not a
    /// submission order a caller may rely on (a coalesced-away or superseded
    /// frame can be acknowledged after a newer one).
    pub fn drain_acks(&mut self, max: usize) -> Vec<PresentAck> {
        let mut queue = self.shared.lock_acks();
        let take = max.min(queue.len());
        let acks: Vec<PresentAck> = queue.drain(..take).collect();
        drop(queue);
        if acks.iter().any(PresentAck::is_presented) {
            self.consecutive_recoveries = 0;
            self.creation_failures = 0;
        }
        self.abandoned.retain(|weak| weak.strong_count() > 0);
        acks
    }

    /// Builds the replacement *before* abandoning the stuck worker, so a
    /// factory failure neither leaves the process without a worker nor charges
    /// the abandoned-worker budget. Only a successful replacement consumes
    /// one recovery slot; creation failures have their own bounded budget.
    ///
    /// The abandoned worker is never joined: it owns its own shared state and
    /// exits on its own when its blocking call returns. The factory runs on
    /// the event-loop thread but performs no acquire/present.
    ///
    /// # Errors
    ///
    /// Propagates the backend factory failure (surface creation/configure) or
    /// a worker-spawn failure, and refuses further surface creation when
    /// either budget is spent.
    pub fn recover(&mut self) -> Result<(), RenderError> {
        if self.is_exhausted() {
            return Err(RenderError::UpstreamGraphics(String::from(
                "present worker surface-creation budget exhausted",
            )));
        }
        let backend = match (self.factory)(self.last_extent, self.last_opacity) {
            Ok(backend) => backend,
            Err(err) => {
                self.creation_failures = self.creation_failures.saturating_add(1);
                return Err(err);
            }
        };
        self.creation_failures = 0;
        self.abandon_worker();
        self.consecutive_recoveries = self.consecutive_recoveries.saturating_add(1);
        let generation = next_generation();
        let shared = Arc::new(WorkerShared::new(
            generation,
            self.config,
            self.next_worker_wake.clone(),
        ));
        // Recovery must not reopen a closed visibility gate: a worker that was
        // paused when its predecessor got stuck stays paused until the
        // embedder resumes it.
        shared.paused.store(
            self.shared.paused.load(Ordering::Acquire),
            Ordering::Release,
        );
        shared
            .alpha_supported
            .store(backend.alpha_supported(), Ordering::Release);
        let worker = spawn_worker(Arc::clone(&shared), backend)?;
        self.shared = shared;
        self.worker = Some(worker);
        self.generation = generation;
        telemetry::presenter_recovered();
        Ok(())
    }

    /// Stops the worker with a bounded wait.
    ///
    /// Returns `true` when the worker exited and was joined, `false` when the
    /// bound elapsed and it was detached (it will exit when its blocking call
    /// returns). Never waits beyond `bound`.
    pub fn shutdown_bounded(&mut self, bound: Duration) -> bool {
        let Some(worker) = self.worker.take() else {
            return true;
        };
        self.shared.request_stop();
        if self.shared.wait_exited(bound) {
            let _ = worker.join();
            true
        } else {
            // Dropping the handle detaches the thread.
            drop(worker);
            false
        }
    }

    /// Signals the current worker to stop without waiting.
    ///
    /// The abandoned worker's shared state is tracked weakly so the embedder
    /// can detect when the last abandoned driver call has returned and force
    /// one correcting repaint.
    fn abandon_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.abandoned.push(Arc::downgrade(&self.shared));
            self.shared.request_stop();
            drop(worker);
        }
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        // Never join here: shutdown is bounded by the embedder's explicit
        // call, and a drop during unwinding must not block on a driver call.
        self.shared.request_stop();
        if let Some(worker) = self.worker.take() {
            drop(worker);
        }
    }
}

/// Starts one named presenter worker.
fn spawn_worker(
    shared: Arc<WorkerShared>,
    backend: Box<dyn PresentBackend>,
) -> Result<std::thread::JoinHandle<()>, RenderError> {
    std::thread::Builder::new()
        .name(String::from("bitty-present"))
        .spawn(move || worker_main(shared, backend))
        .map_err(|err| RenderError::UpstreamGraphics(format!("present worker spawn failed: {err}")))
}

#[cfg(test)]
mod tests;
