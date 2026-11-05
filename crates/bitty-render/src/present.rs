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

/// Acknowledgement token used for events that are not a frame outcome (a
/// configure failure). Frame tokens start at zero, so an impossible value is
/// reserved here instead of colliding with the first submission.
pub const PRESENT_NON_FRAME_TOKEN: u64 = u64::MAX;

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
/// superseded frame's damage, and diagnostics can report what was carried.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DamageCarry {
    /// The frame paints the whole surface; individual rects are irrelevant.
    pub full: bool,
    /// Bounded damage rectangles in physical window pixels.
    pub rects: Vec<RectPx>,
}

impl DamageCarry {
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

/// One fully assembled frame handed to the presenter worker.
///
/// The UI thread builds and owns this value; after `submit` the worker owns
/// it exclusively until the acknowledgement is published.
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
    /// Token of the submitted frame this outcome belongs to (`0` for a
    /// configure failure, which is not a frame outcome).
    pub token: u64,
    /// Classified outcome.
    pub outcome: PresentOutcome,
    /// Damage carried by the frame (including superseded frames' damage).
    pub damage: DamageCarry,
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
}

impl Default for PresenterConfig {
    fn default() -> Self {
        Self {
            stuck_bound: PRESENT_STUCK_BOUND,
            shutdown_bound: PRESENT_SHUTDOWN_BOUND,
            ack_capacity: PRESENT_ACK_CAPACITY,
            recovery_limit: PRESENT_RECOVERY_LIMIT,
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
    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.cv.notify_all();
    }
}

/// Sets the exit flag when the worker loop returns or unwinds.
struct ExitGuard(Arc<WorkerShared>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.0.mark_exited();
    }
}

/// Body of the `bitty-present` worker thread.
fn worker_main(shared: Arc<WorkerShared>, mut backend: Box<dyn PresentBackend>) {
    let _exit = ExitGuard(Arc::clone(&shared));
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
                slot = shared
                    .cv
                    .wait(slot)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            (slot.frame.take(), slot.configure.take())
        };

        if let Some(request) = configure {
            match backend.configure(request.extent, request.opacity) {
                Ok(alpha) => shared.alpha_supported.store(alpha, Ordering::Release),
                Err(err) => shared.push_ack(PresentAck {
                    token: PRESENT_NON_FRAME_TOKEN,
                    outcome: PresentOutcome::Error(err.to_string()),
                    damage: DamageCarry::default(),
                }),
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
        let mut panicked = false;
        let outcome = match catch_unwind(AssertUnwindSafe(|| backend.present(&frame.frame))) {
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
            damage: frame.frame.damage,
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
    /// Consecutive recoveries since the last presented frame, bounded by
    /// [`PresenterConfig::recovery_limit`].
    consecutive_recoveries: u32,
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
        })
    }

    /// Spawns the production presenter from an owned surface and GPU context.
    ///
    /// The surface is configured once here (before ownership transfer, on the
    /// calling thread) and then owned exclusively by the worker. Recovery
    /// creates and configures a new surface through the same factory without
    /// touching the event-loop thread's state.
    ///
    /// # Errors
    ///
    /// Propagates a surface reconfiguration or worker-spawn failure.
    pub fn spawn_gpu(
        gpu: GpuContext,
        surface: Surface,
        target: SurfaceTarget,
        extent: PhysicalSize,
        opacity: f32,
        config: PresenterConfig,
        wake: Option<PresentWake>,
    ) -> Result<Self, RenderError> {
        // Reconfigure through the same path recovery will use, so the first
        // worker and every replacement see identical state.
        surface.configure_with_opacity(&gpu, extent, opacity)?;
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

    /// True when the bounded recovery budget is spent.
    ///
    /// The embedder stops submitting and stays responsive until a frame from
    /// the last worker is actually presented (which resets the budget), so a
    /// compositor that refuses every replacement surface cannot accumulate
    /// worker threads indefinitely.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.consecutive_recoveries >= self.config.recovery_limit.max(1)
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
        self.shared.paused.store(true, Ordering::Release);
    }

    /// Resumes the worker; a queued frame (the latest submitted) is
    /// presented next.
    pub fn resume(&self) {
        self.shared.paused.store(false, Ordering::Release);
        self.shared.cv.notify_all();
    }

    /// Records one stuck-present episode on the live worker.
    pub fn note_stuck(&mut self) {
        let _ = self.shared.stuck.fetch_add(1, Ordering::Relaxed);
        self.stuck_total = self.stuck_total.saturating_add(1);
        telemetry::presenter_stuck();
    }

    /// Sends a configure request; the worker applies it before its next
    /// acquire. Only the newest request is kept.
    pub fn configure(&mut self, extent: PhysicalSize, opacity: f32) {
        self.last_extent = extent;
        self.last_opacity = opacity;
        self.shared.lock_slot().configure = Some(ConfigureRequest { extent, opacity });
        self.shared.cv.notify_all();
    }

    /// Enqueues the newest frame, replacing (and carrying the damage of) any
    /// frame still waiting in the slot.
    ///
    /// Returns the submission token, or `None` when no worker is active (the
    /// caller retains the frame's damage and retries recovery).
    pub fn submit(&mut self, mut frame: FramePayload) -> Option<u64> {
        if !self.is_active() || self.is_exhausted() {
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
    pub fn drain_acks(&mut self, max: usize) -> Vec<PresentAck> {
        let mut queue = self.shared.lock_acks();
        let take = max.min(queue.len());
        let acks: Vec<PresentAck> = queue.drain(..take).collect();
        drop(queue);
        if acks.iter().any(PresentAck::is_presented) {
            self.consecutive_recoveries = 0;
        }
        acks
    }

    /// Abandons a stuck worker and starts a fresh surface + worker.
    ///
    /// The abandoned worker is never joined: it owns its own shared state and
    /// exits on its own when its blocking call returns. On failure the
    /// presenter stays inactive and [`Self::recover`] can be retried.
    ///
    /// # Errors
    ///
    /// Propagates the backend factory failure (surface creation/configure).
    pub fn recover(&mut self) -> Result<(), RenderError> {
        if self.is_exhausted() {
            return Err(RenderError::UpstreamGraphics(String::from(
                "present worker recovery budget exhausted",
            )));
        }
        self.consecutive_recoveries = self.consecutive_recoveries.saturating_add(1);
        self.abandon_worker();
        let backend = (self.factory)(self.last_extent, self.last_opacity)?;
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
    fn abandon_worker(&mut self) {
        self.shared.request_stop();
        if let Some(worker) = self.worker.take() {
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
