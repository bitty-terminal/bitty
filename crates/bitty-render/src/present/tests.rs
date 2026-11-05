//! Deterministic presenter tests: a fake backend blocks on demand, so every
//! concurrency claim is exercised without a GPU or a display server.

use super::*;
use crate::frame::{FrameMode, FramePlan};
use crate::geometry::{ExtentPx, RectPx};
use std::sync::atomic::AtomicU32;
use std::time::Duration;

const TEST_WAIT: Duration = Duration::from_secs(5);

/// One observed present.
#[derive(Clone, Debug)]
struct Observed {
    generation: u64,
    damage: DamageCarry,
}

/// One observed call in order (`configure` and `present` share the log so
/// ordering is assertable).
#[derive(Clone, Debug)]
enum Call {
    Configure(PhysicalSize, f32),
    Present(Observed),
}

/// A deterministic block gate.
struct Gate {
    state: Mutex<GateState>,
    cv: Condvar,
}

struct GateState {
    blocked: bool,
    entered: u32,
    released: bool,
}

impl Gate {
    fn new(blocked: bool) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState {
                blocked,
                entered: 0,
                released: false,
            }),
            cv: Condvar::new(),
        })
    }

    fn enter(&self) {
        let mut state = self.state.lock().expect("gate poisoned");
        state.entered = state.entered.saturating_add(1);
        self.cv.notify_all();
        while state.blocked && !state.released {
            state = self.cv.wait(state).expect("gate poisoned");
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("gate poisoned");
        state.released = true;
        self.cv.notify_all();
    }

    fn entered(&self) -> u32 {
        self.state.lock().expect("gate poisoned").entered
    }

    fn wait_entered(&self, count: u32, timeout: Duration) -> bool {
        wait_until(
            || self.state.lock().expect("gate poisoned").entered >= count,
            timeout,
        )
    }
}

/// Shared fake state handed to the presenter factory and the test.
struct FakeControl {
    gate: Arc<Gate>,
    calls: Mutex<Vec<Call>>,
    presented: AtomicU64,
    built: AtomicU32,
    alive: AtomicU32,
    fail_next: AtomicBool,
    panic_next: AtomicBool,
    fail_configure_next: AtomicBool,
    panic_drop: AtomicBool,
    factory_failures: AtomicU32,
    drop_gate: Mutex<Option<Arc<Gate>>>,
    wakes: AtomicU64,
}

impl FakeControl {
    fn new(blocked: bool) -> Arc<Self> {
        Arc::new(Self {
            gate: Gate::new(blocked),
            calls: Mutex::new(Vec::new()),
            presented: AtomicU64::new(0),
            built: AtomicU32::new(0),
            alive: AtomicU32::new(0),
            fail_next: AtomicBool::new(false),
            panic_next: AtomicBool::new(false),
            fail_configure_next: AtomicBool::new(false),
            panic_drop: AtomicBool::new(false),
            factory_failures: AtomicU32::new(0),
            drop_gate: Mutex::new(None),
            wakes: AtomicU64::new(0),
        })
    }

    fn factory(self: &Arc<Self>) -> PresenterFactory {
        let control = Arc::clone(self);
        Arc::new(move |_extent, _opacity| {
            if control
                .factory_failures
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Err(RenderError::SurfaceCreate(String::from(
                    "injected factory failure",
                )));
            }
            control.built.fetch_add(1, Ordering::Relaxed);
            control.alive.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(FakeBackend {
                control: Arc::clone(&control),
            }) as Box<dyn PresentBackend>)
        })
    }

    fn first(self: &Arc<Self>) -> Box<dyn PresentBackend> {
        self.built.fetch_add(1, Ordering::Relaxed);
        self.alive.fetch_add(1, Ordering::Relaxed);
        Box::new(FakeBackend {
            control: Arc::clone(self),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("calls poisoned").clone()
    }

    fn observed(&self) -> Vec<Observed> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Present(observed) => Some(observed),
                Call::Configure(..) => None,
            })
            .collect()
    }

    fn presented(&self) -> u64 {
        self.presented.load(Ordering::Acquire)
    }

    fn alive(&self) -> u32 {
        self.alive.load(Ordering::Acquire)
    }

    fn fail_configure_once(&self) {
        self.fail_configure_next.store(true, Ordering::Release);
    }

    fn fail_factory(&self, count: u32) {
        self.factory_failures.store(count, Ordering::Release);
    }

    fn block_drop(self: &Arc<Self>) -> Arc<Gate> {
        let gate = Gate::new(true);
        *self.drop_gate.lock().expect("drop gate poisoned") = Some(Arc::clone(&gate));
        gate
    }

    fn wake_count(&self) -> u64 {
        self.wakes.load(Ordering::Acquire)
    }
}

struct FakeBackend {
    control: Arc<FakeControl>,
}

impl Drop for FakeBackend {
    fn drop(&mut self) {
        let gate = self
            .control
            .drop_gate
            .lock()
            .expect("drop gate poisoned")
            .clone();
        if let Some(gate) = gate {
            gate.enter();
        }
        self.control.alive.fetch_sub(1, Ordering::AcqRel);
        if self.control.panic_drop.load(Ordering::Acquire) {
            panic!("injected backend destructor panic");
        }
    }
}

impl PresentBackend for FakeBackend {
    fn configure(&mut self, extent: PhysicalSize, opacity: f32) -> Result<bool, RenderError> {
        self.control
            .calls
            .lock()
            .expect("calls poisoned")
            .push(Call::Configure(extent, opacity));
        if self
            .control
            .fail_configure_next
            .swap(false, Ordering::AcqRel)
        {
            return Err(RenderError::SurfaceConfigure(String::from(
                "injected configure failure",
            )));
        }
        Ok(true)
    }

    fn present(&mut self, frame: &FramePayload) -> Result<PresentStats, PresentFailure> {
        self.control.gate.enter();
        if self.control.panic_next.swap(false, Ordering::AcqRel) {
            panic!("injected presenter panic");
        }
        if self.control.fail_next.swap(false, Ordering::AcqRel) {
            return Err(PresentFailure::Surface {
                class: PresentFailureClass::Timeout,
                detail: String::from("injected timeout"),
            });
        }
        let frame_number = self.control.presented.fetch_add(1, Ordering::AcqRel) + 1;
        self.control
            .calls
            .lock()
            .expect("calls poisoned")
            .push(Call::Present(Observed {
                generation: frame.draw_list.generation,
                damage: frame.damage.clone(),
            }));
        Ok(PresentStats {
            frame: frame_number,
            fills: 0,
            rounded_fills: 0,
            glyphs: 0,
            headless: false,
            images: 0,
            images_skipped: 0,
        })
    }
}

fn draw_list(generation: u64, rects: Vec<RectPx>) -> Box<DrawList> {
    let extent = ExtentPx::new(800, 600);
    Box::new(DrawList {
        generation,
        atlas_epoch: 0,
        plan: FramePlan {
            extent,
            mode: if rects.is_empty() {
                FrameMode::Clean
            } else {
                FrameMode::Full
            },
            dirty_rects: rects,
        },
        fills: Vec::new(),
        rounded_fills: Vec::new(),
        backgrounds: Vec::new(),
        overlay_fills: Vec::new(),
        overlay_card_rounded_fills: Vec::new(),
        overlay_content_fills: Vec::new(),
        overlay_rounded_fills: Vec::new(),
        glyphs: Vec::new(),
        images: Vec::new(),
    })
}

fn frame(generation: u64, rects: Vec<RectPx>) -> FramePayload {
    let list = draw_list(generation, rects);
    let damage = DamageCarry::from_rects(list.plan.dirty_rects.clone(), false);
    FramePayload {
        draw_list: list,
        atlas: None,
        color_atlas: None,
        theme: ThemePalette::bitty_dark(),
        damage,
    }
}

fn spawn(control: &Arc<FakeControl>, blocked: bool, config: PresenterConfig) -> Presenter {
    control.gate.state.lock().expect("gate poisoned").blocked = blocked;
    Presenter::spawn(
        control.factory(),
        control.first(),
        PhysicalSize::new(800, 600),
        1.0,
        config,
        None,
    )
    .expect("presenter spawns")
}

fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn drain_until(presenter: &mut Presenter, count: usize, timeout: Duration) -> Vec<PresentAck> {
    let mut acks = Vec::new();
    wait_until(
        || {
            acks.extend(presenter.drain_acks(16));
            acks.len() >= count
        },
        timeout,
    );
    acks
}

#[test]
fn submit_never_waits_for_a_blocked_present() {
    let control = FakeControl::new(true);
    let mut presenter = spawn(
        &control,
        true,
        PresenterConfig {
            stuck_bound: TEST_WAIT,
            shutdown_bound: Duration::from_millis(50),
            ..PresenterConfig::default()
        },
    );

    let first = presenter.submit(frame(1, vec![RectPx::new(0, 0, 10, 10)]));
    assert!(first.is_some());
    assert!(
        control.gate.wait_entered(1, TEST_WAIT),
        "worker must reach the blocked acquire"
    );
    assert!(presenter.in_flight_age(Instant::now()).is_some());

    let started = Instant::now();
    let second = presenter.submit(frame(2, vec![RectPx::new(20, 20, 30, 30)]));
    presenter.pause();
    presenter.resume();
    presenter.configure(PhysicalSize::new(801, 601), 1.0);
    let elapsed = started.elapsed();
    assert!(second.is_some(), "the slot stays usable while blocked");
    assert!(
        elapsed < Duration::from_millis(100),
        "UI calls must not wait on the blocked worker (took {elapsed:?})"
    );
    assert!(
        presenter.drain_acks(16).is_empty(),
        "a blocked present has no ack yet"
    );

    control.gate.release();
    let acks = drain_until(&mut presenter, 2, TEST_WAIT);
    assert_eq!(acks.len(), 2, "one ack per attempted present");
    assert!(acks.iter().all(PresentAck::is_presented));
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn replacing_a_queued_frame_carries_the_damage_union() {
    let control = FakeControl::new(false);
    let mut presenter = spawn(&control, false, PresenterConfig::default());
    presenter.pause();

    assert!(
        presenter
            .submit(frame(1, vec![RectPx::new(0, 0, 10, 10)]))
            .is_some()
    );
    assert!(
        presenter
            .submit(frame(2, vec![RectPx::new(50, 50, 10, 10)]))
            .is_some()
    );
    assert!(
        wait_until(|| presenter.is_worker_parked(), TEST_WAIT),
        "the paused worker parks instead of acquiring"
    );
    assert_eq!(control.gate.entered(), 0, "paused worker must not acquire");
    assert!(presenter.drain_acks(16).is_empty());

    presenter.resume();
    let acks = drain_until(&mut presenter, 1, TEST_WAIT);
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0].token(), 1, "the newest frame is the one presented");
    let observed = control.observed();
    assert_eq!(observed.len(), 1, "the replaced frame is never presented");
    assert_eq!(observed[0].generation, 2, "newest frame wins");
    let damage = &observed[0].damage;
    assert!(!damage.full);
    assert!(
        damage
            .rects
            .iter()
            .any(|rect| *rect == RectPx::new(0, 0, 10, 10)),
        "the replaced frame's damage is carried: {:?}",
        damage.rects
    );
    assert!(
        damage
            .rects
            .iter()
            .any(|rect| *rect == RectPx::new(50, 50, 10, 10)),
        "the newest frame's damage is carried: {:?}",
        damage.rects
    );
    assert!(presenter.coalesced_frames() >= 1);
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn damage_union_collapses_to_full_past_the_cap() {
    let mut carry = DamageCarry::from_rects(
        (0..PRESENT_MAX_CARRIED_DAMAGE_RECTS + 1)
            .map(|index| RectPx::new(index as i32 * 2, 0, 1, 1))
            .collect(),
        false,
    );
    assert!(carry.full, "the bounded cap forces a full repaint");
    assert!(carry.rects.is_empty());

    let mut absorb = DamageCarry::from_rects(vec![RectPx::new(0, 0, 100, 100)], false);
    absorb.merge(&DamageCarry::from_rects(
        vec![RectPx::new(10, 10, 5, 5)],
        false,
    ));
    assert_eq!(
        absorb.rects,
        vec![RectPx::new(0, 0, 100, 100)],
        "contained damage is absorbed"
    );
    carry.merge(&DamageCarry {
        full: true,
        rects: Vec::new(),
    });
    assert!(carry.full);
}

#[test]
fn pause_prevents_acquire_and_resume_presents_the_latest_frame() {
    let control = FakeControl::new(false);
    let mut presenter = spawn(&control, false, PresenterConfig::default());
    presenter.pause();
    let first = presenter.submit(frame(10, vec![RectPx::new(0, 0, 4, 4)]));
    let second = presenter.submit(frame(11, vec![RectPx::new(4, 4, 4, 4)]));
    assert!(first.is_some() && second.is_some());
    assert!(
        wait_until(|| presenter.is_worker_parked(), TEST_WAIT),
        "the paused worker parks instead of acquiring"
    );
    assert_eq!(control.gate.entered(), 0);
    assert_eq!(control.presented(), 0);
    assert!(presenter.is_paused());

    presenter.resume();
    let acks = drain_until(&mut presenter, 1, TEST_WAIT);
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0].token(), 1);
    assert_eq!(control.observed()[0].generation, 11);
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn stuck_detection_recovers_the_worker_and_the_old_one_exits() {
    let control = FakeControl::new(true);
    let mut presenter = spawn(
        &control,
        true,
        PresenterConfig {
            stuck_bound: Duration::from_millis(20),
            shutdown_bound: Duration::from_millis(20),
            ack_capacity: 4,
            recovery_limit: 4,
            ..PresenterConfig::default()
        },
    );
    assert!(presenter.submit(frame(1, Vec::new())).is_some());
    assert!(control.gate.wait_entered(1, TEST_WAIT));
    let generation = presenter.generation();
    let age = presenter.in_flight_age(Instant::now()).expect("in flight");
    assert!(age > Duration::ZERO);

    presenter.note_stuck();
    assert_eq!(presenter.stuck_presents(), 1);
    assert!(
        telemetry::snapshot().presenter_stuck_presents >= 1,
        "the watchdog gauge records the episode"
    );
    presenter.recover().expect("recovery builds a fresh worker");
    assert_ne!(presenter.generation(), generation);
    assert!(presenter.is_active());
    assert_eq!(control.built.load(Ordering::Acquire), 2);
    assert!(
        presenter.in_flight_age(Instant::now()).is_none(),
        "a fresh worker starts idle"
    );

    // The shared gate keeps the new worker blocked too; proving the old
    // worker exits on its own needs the gate released.
    assert!(presenter.submit(frame(2, Vec::new())).is_some());
    assert!(control.gate.wait_entered(2, TEST_WAIT));
    control.gate.release();
    assert!(
        wait_until(|| control.alive() == 1, TEST_WAIT),
        "the abandoned worker exits when its blocking call returns"
    );
    let acks = drain_until(&mut presenter, 1, TEST_WAIT);
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0].token(), 1, "only the live worker's ack is visible");
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn shutdown_with_a_blocked_worker_is_bounded() {
    let control = FakeControl::new(true);
    let mut presenter = spawn(
        &control,
        true,
        PresenterConfig {
            stuck_bound: TEST_WAIT,
            shutdown_bound: Duration::from_millis(40),
            ..PresenterConfig::default()
        },
    );
    assert!(presenter.submit(frame(1, Vec::new())).is_some());
    assert!(control.gate.wait_entered(1, TEST_WAIT));

    let started = Instant::now();
    let joined = presenter.shutdown_bounded(Duration::from_millis(40));
    let elapsed = started.elapsed();
    assert!(!joined, "a blocked worker cannot be joined in the bound");
    assert!(
        elapsed < Duration::from_millis(500),
        "shutdown must not wait on the driver call (took {elapsed:?})"
    );
    assert!(!presenter.is_active());
    assert!(presenter.submit(frame(2, Vec::new())).is_none());

    control.gate.release();
    assert!(
        wait_until(|| control.alive() == 0, TEST_WAIT),
        "the detached worker exits once its call returns"
    );
}

#[test]
fn acknowledgements_are_bounded_and_keep_the_newest_outcome() {
    let control = FakeControl::new(false);
    let mut presenter = spawn(
        &control,
        false,
        PresenterConfig {
            ack_capacity: 2,
            ..PresenterConfig::default()
        },
    );
    for generation in 0..8 {
        assert!(presenter.submit(frame(generation, Vec::new())).is_some());
        let token = generation;
        assert!(
            wait_until(|| control.presented() > token, TEST_WAIT),
            "present {generation} runs"
        );
    }
    let acks = presenter.drain_acks(16);
    assert_eq!(acks.len(), 2, "the ack ring keeps its fixed capacity");
    assert_eq!(acks[0].token(), 6);
    assert_eq!(acks[1].token(), 7);
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn configure_is_applied_before_the_next_acquire() {
    let control = FakeControl::new(false);
    let mut presenter = spawn(&control, false, PresenterConfig::default());
    presenter.pause();
    presenter.configure(PhysicalSize::new(1024, 768), 0.5);
    assert!(
        presenter
            .submit(frame(1, vec![RectPx::new(0, 0, 7, 7)]))
            .is_some()
    );
    presenter.resume();
    let acks = drain_until(&mut presenter, 1, TEST_WAIT);
    assert_eq!(acks.len(), 1);
    let calls = control.calls();
    assert!(matches!(
        calls.first(),
        Some(Call::Configure(extent, opacity))
            if extent.width() == 1024
                && extent.height() == 768
                && (*opacity - 0.5).abs() < f32::EPSILON
    ));
    assert!(matches!(calls.get(1), Some(Call::Present(_))));
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn a_present_failure_is_acked_with_its_class() {
    let control = FakeControl::new(false);
    control.fail_next.store(true, Ordering::Release);
    let mut presenter = spawn(&control, false, PresenterConfig::default());
    assert!(presenter.submit(frame(1, Vec::new())).is_some());
    let acks = drain_until(&mut presenter, 1, TEST_WAIT);
    assert_eq!(acks.len(), 1);
    assert!(
        matches!(
            acks[0].outcome,
            PresentOutcome::Timeout(ref detail) if detail == "injected timeout"
        ),
        "the acquire class survives the worker boundary"
    );
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn a_worker_that_exits_is_reported_inactive_and_can_be_recovered() {
    let control = FakeControl::new(false);
    control.panic_next.store(true, Ordering::Release);
    let mut presenter = spawn(
        &control,
        false,
        PresenterConfig {
            stuck_bound: TEST_WAIT,
            shutdown_bound: Duration::from_millis(50),
            ..PresenterConfig::default()
        },
    );
    assert!(presenter.submit(frame(1, Vec::new())).is_some());
    assert!(
        wait_until(|| !presenter.is_active(), TEST_WAIT),
        "an exited worker must not look active"
    );
    assert!(
        presenter.submit(frame(2, Vec::new())).is_none(),
        "a dead worker accepts no frames"
    );
    presenter.recover().expect("recovery starts a fresh worker");
    assert!(presenter.is_active());
    assert!(presenter.submit(frame(3, Vec::new())).is_some());
    let acks = drain_until(&mut presenter, 1, TEST_WAIT);
    assert_eq!(acks.len(), 1);
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn recovery_keeps_a_closed_visibility_gate_paused() {
    let control = FakeControl::new(false);
    let mut presenter = spawn(&control, false, PresenterConfig::default());
    presenter.pause();
    assert!(presenter.submit(frame(1, Vec::new())).is_some());
    presenter.recover().expect("recovery builds a fresh worker");
    assert!(
        presenter.is_paused(),
        "a hidden surface must stay hidden across worker recovery"
    );
    assert!(
        wait_until(|| presenter.is_worker_parked(), TEST_WAIT),
        "the replacement worker parks while paused"
    );
    assert_eq!(control.gate.entered(), 0, "the new worker stays paused");
    // The abandoned frame belonged to the dead worker; the embedder retains
    // its damage and submits the latest frame to the replacement on resume.
    assert!(presenter.submit(frame(2, Vec::new())).is_some());
    presenter.resume();
    let acks = drain_until(&mut presenter, 1, TEST_WAIT);
    assert_eq!(acks.len(), 1, "resume presents the latest submitted frame");
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn recovery_budget_stops_only_surface_creation_and_the_ack_resets_it() {
    let control = FakeControl::new(true);
    let mut presenter = spawn(
        &control,
        true,
        PresenterConfig {
            stuck_bound: TEST_WAIT,
            shutdown_bound: Duration::from_millis(50),
            ack_capacity: 4,
            recovery_limit: 2,
            ..PresenterConfig::default()
        },
    );
    assert!(!presenter.is_exhausted());
    presenter.recover().expect("first recovery");
    presenter.recover().expect("second recovery");
    assert!(presenter.is_exhausted());
    let built = control.built.load(Ordering::Acquire);
    assert!(
        presenter.recover().is_err(),
        "the budget refuses another surface"
    );
    assert_eq!(
        control.built.load(Ordering::Acquire),
        built,
        "no worker is created past the budget"
    );

    // The current worker must still be fed: its presented ack is the only
    // event that can reset the budget. Refusing frames here would strand the
    // window forever after the third stuck present.
    assert!(
        presenter.submit(frame(1, Vec::new())).is_some(),
        "an exhausted presenter still feeds its current worker"
    );
    control.gate.release();
    let acks = drain_until(&mut presenter, 1, TEST_WAIT);
    assert_eq!(acks.len(), 1);
    assert!(acks[0].is_presented());
    assert!(
        !presenter.is_exhausted(),
        "a presented frame resets the surface-creation budget"
    );
    presenter
        .recover()
        .expect("surface creation is allowed again after a presented frame");
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn creation_failures_do_not_charge_the_abandoned_worker_budget() {
    let control = FakeControl::new(false);
    let mut presenter = spawn(&control, false, PresenterConfig::default());
    control.fail_factory(1);
    assert!(
        presenter.recover().is_err(),
        "the factory failure propagates"
    );
    assert_eq!(presenter.creation_failures(), 1);
    assert!(
        presenter.is_active(),
        "a failed creation leaves the current worker in place"
    );
    assert!(!presenter.is_exhausted());
    assert_eq!(
        control.built.load(Ordering::Acquire),
        1,
        "no replacement worker was created"
    );

    // The untouched worker can still present, which resets both budgets.
    assert!(presenter.submit(frame(1, Vec::new())).is_some());
    let acks = drain_until(&mut presenter, 1, TEST_WAIT);
    assert!(acks[0].is_presented());
    assert_eq!(presenter.creation_failures(), 0);
    presenter
        .recover()
        .expect("creation works again after a presented frame");
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn a_configure_failure_never_presents_the_old_extent_frame() {
    let control = FakeControl::new(false);
    let mut presenter = spawn(&control, false, PresenterConfig::default());
    control.fail_configure_once();
    presenter.configure(PhysicalSize::new(1024, 768), 1.0);
    assert!(presenter.submit(frame(1, Vec::new())).is_some());
    let acks = drain_until(&mut presenter, 1, TEST_WAIT);
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0].token(), 0, "the frame's own token is acknowledged");
    assert!(
        matches!(
            acks[0].outcome,
            PresentOutcome::Error(ref detail) if detail.contains("configure failed")
        ),
        "the taken frame is failed, not presented with the old configuration"
    );
    assert_eq!(control.presented(), 0, "nothing was presented");
    assert!(
        wait_until(|| !presenter.is_active(), TEST_WAIT),
        "the worker exits for recovery instead of acquiring with stale state"
    );

    presenter.recover().expect("recovery builds a fresh worker");
    assert!(presenter.submit(frame(2, Vec::new())).is_some());
    let acks = drain_until(&mut presenter, 1, TEST_WAIT);
    assert!(acks[0].is_presented());
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn shutdown_bound_holds_with_a_blocking_backend_destructor() {
    let control = FakeControl::new(false);
    let drop_gate = control.block_drop();
    let mut presenter = spawn(&control, false, PresenterConfig::default());
    let started = Instant::now();
    assert!(
        !presenter.shutdown_bounded(Duration::from_millis(40)),
        "a destructor stuck past the bound is detached, not waited on"
    );
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "shutdown must return within its bound (took {:?})",
        started.elapsed()
    );
    drop_gate.release();
    assert!(
        wait_until(|| control.alive() == 0, TEST_WAIT),
        "the detached worker finishes its destructor and exits"
    );
}

#[test]
fn shutdown_wakes_a_paused_parked_worker() {
    let control = FakeControl::new(false);
    let mut presenter = spawn(&control, false, PresenterConfig::default());
    presenter.pause();
    assert!(
        wait_until(|| presenter.is_worker_parked(), TEST_WAIT),
        "the paused worker is parked"
    );
    assert!(
        presenter.shutdown_bounded(TEST_WAIT),
        "stop must not be lost against a parked waiter"
    );
    assert!(!presenter.is_active());
}

#[test]
fn pause_resume_races_do_not_strand_the_worker() {
    let control = FakeControl::new(false);
    let mut presenter = spawn(&control, false, PresenterConfig::default());
    for generation in 0..64 {
        presenter.pause();
        assert!(presenter.submit(frame(generation, Vec::new())).is_some());
        presenter.resume();
    }
    assert!(
        !drain_until(&mut presenter, 1, TEST_WAIT).is_empty(),
        "the worker keeps draining frames across pause/resume races"
    );
    assert!(presenter.shutdown_bounded(TEST_WAIT));
}

#[test]
fn abandoned_worker_liveness_is_observable() {
    let control = FakeControl::new(true);
    let mut presenter = spawn(&control, true, PresenterConfig::default());
    assert!(presenter.submit(frame(1, Vec::new())).is_some());
    assert!(control.gate.wait_entered(1, TEST_WAIT));
    presenter.recover().expect("recovery builds a fresh worker");
    assert_eq!(
        presenter.abandoned_alive(),
        1,
        "the blocked abandoned worker is still alive"
    );
    control.gate.release();
    assert!(
        wait_until(|| presenter.abandoned_alive() == 0, TEST_WAIT),
        "the abandoned worker is observed once its driver call returns"
    );
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn an_ack_invokes_the_configured_wake_callback() {
    let control = FakeControl::new(false);
    let mut presenter = spawn(&control, false, PresenterConfig::default());
    let wakes = Arc::clone(&control);
    presenter.set_waker(Some(Arc::new(move || {
        let _ = wakes.wakes.fetch_add(1, Ordering::Relaxed);
    })));
    assert!(presenter.submit(frame(1, Vec::new())).is_some());
    assert!(!drain_until(&mut presenter, 1, TEST_WAIT).is_empty());
    assert!(
        control.wake_count() >= 1,
        "the ack must wake the embedder loop"
    );
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn partial_damage_carries_are_refused_by_the_worker_contract() {
    let partial = DamageCarry::from_rects(vec![RectPx::new(0, 0, 1, 1)], false);
    assert!(require_full_damage(&partial).is_err());
    assert!(require_full_damage(&DamageCarry::full()).is_ok());
}

#[test]
fn creation_failure_budget_stops_surface_attempts() {
    let control = FakeControl::new(false);
    let mut presenter = spawn(
        &control,
        false,
        PresenterConfig {
            creation_failure_limit: 2,
            ..PresenterConfig::default()
        },
    );
    control.fail_factory(2);
    assert!(presenter.recover().is_err());
    assert_eq!(presenter.creation_failures(), 1);
    assert!(!presenter.is_exhausted());
    assert!(presenter.recover().is_err());
    assert!(presenter.is_exhausted());
    let built = control.built.load(Ordering::Acquire);
    assert!(
        presenter.recover().is_err(),
        "the creation budget refuses another surface"
    );
    assert_eq!(control.built.load(Ordering::Acquire), built);

    // The current worker can still present, which resets the budget.
    assert!(presenter.submit(frame(1, Vec::new())).is_some());
    assert!(!drain_until(&mut presenter, 1, TEST_WAIT).is_empty());
    assert!(!presenter.is_exhausted());
    presenter
        .recover()
        .expect("a presented frame allows surface creation again");
    assert!(presenter.shutdown_bounded(Duration::from_millis(500)));
}

#[test]
fn a_panicking_backend_destructor_still_signals_exit() {
    let control = FakeControl::new(false);
    control.panic_drop.store(true, Ordering::Release);
    let mut presenter = spawn(&control, false, PresenterConfig::default());
    assert!(
        presenter.shutdown_bounded(TEST_WAIT),
        "the exit signal must survive a panicking surface destructor"
    );
    assert!(!presenter.is_active());
}
