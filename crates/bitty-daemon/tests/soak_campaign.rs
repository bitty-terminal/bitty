//! Long-duration soak campaign driver (bitty#2830, daemon event plane).
//!
//! The presentation half lives in `bitty-runtime/tests/soak_campaign.rs`; this
//! file is the daemon half of the same campaign and shares its contract:
//!
//! - the wall-clock budget comes from `BITTY_SOAK_CAMPAIGN_SECS` (default
//!   [`DEFAULT_CAMPAIGN_SECS`]), so the nightly job runs minutes and an owner
//!   campaign runs hours with the same driver;
//! - the event plane is sampled at the start, near the middle, and at the end
//!   of the budget (plus an even cadence in between) for process RSS, open file
//!   descriptors, retained event-ring depth and bytes, lease count, undelivered
//!   events (`pending_deliveries`), and the stream's accounted bytes;
//! - every assertion goes through [`Series::check`], so a regression prints the
//!   whole sampled series into the CI log and writes it under
//!   `BITTY_SOAK_EVIDENCE_DIR` when the nightly job sets it.
//!
//! Workload per cycle against an injected monotonic clock (no sleeping):
//!
//! - repeated attach/detach: two leases are attached, detached, and re-attached
//!   on a fixed cadence, and the lease count must return to zero after each
//!   detach pair;
//! - daemon event-stream traffic: `control_transfer` mutations publish events
//!   into the retained ring and both lease queues, and the transport drains a
//!   bounded prefix of them, so `pending_deliveries` is a live queue depth;
//! - terminal churn: every terminal cycle terminates the session (which pins
//!   the terminal frame for its consumers and frees the ring), closes the
//!   pinned leases, collects the retained record past its window, and starts a
//!   fresh session.
//!
//! The sustained PTY byte volume of the campaign is driven on the presentation
//! plane: the daemon's content plane is fed by that PTY, and this plane owns
//! the session/lease/event state behind it. Splitting the two planes is
//! required by the workspace dependency direction (the daemon sits above the
//! mechanism crates and no crate may depend on both harnesses).
//!
//! The test is `#[ignore]`d so the PR-required `cargo nextest run --workspace`
//! never pays for a multi-minute campaign. Run it with
//! `just soak-campaign [secs]`.

use std::time::{Duration, Instant};

use bitty_daemon::events::{AttachRequest, DetachCause, EventLimits, SessionEventStream};
use bitty_daemon::registry::{
    AttachId, CleanupClass, CreateSession, ExitInformation, MonotonicMs, RegistryLimits, SessionId,
    SessionRegistry, TerminalId, TerminalOutcome, TerminalReason,
};

/// Campaign budget when `BITTY_SOAK_CAMPAIGN_SECS` is unset.
const DEFAULT_CAMPAIGN_SECS: u64 = 120;

/// Smallest accepted campaign budget.
const MIN_CAMPAIGN_SECS: u64 = 8;

/// Largest accepted campaign budget (one day).
const MAX_CAMPAIGN_SECS: u64 = 86_400;

/// Simulated milliseconds per event publish.
const EVENT_STEP_MS: u64 = 1;

/// Control-transfer publishes per lease direction per cycle.
const PUBLISHES_PER_CYCLE: u64 = 4;

/// Cycles between the detach/re-attach pair.
const LEASE_CHURN_EVERY: u64 = 5;

/// Cycles between terminal transitions (terminate, close, collect, recreate).
const TERMINAL_EVERY: u64 = 40;

/// Simulated milliseconds the terminal record is collected past.
const RETENTION_WINDOW_MS: u64 = 600_000;

/// Hard cap on RSS growth between the first and last sample.
#[cfg(target_os = "linux")]
const MAX_RSS_GROWTH_KIB: u64 = 64 * 1024;

/// Hard cap on file-descriptor growth between the first and last sample.
#[cfg(target_os = "linux")]
const MAX_FD_GROWTH: i64 = 16;

fn campaign_budget() -> Duration {
    let secs = std::env::var("BITTY_SOAK_CAMPAIGN_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CAMPAIGN_SECS)
        .clamp(MIN_CAMPAIGN_SECS, MAX_CAMPAIGN_SECS);
    Duration::from_secs(secs)
}

fn sample_cadence(budget: Duration) -> Duration {
    (budget / 8).clamp(Duration::from_millis(500), Duration::from_secs(60))
}

fn ms(value: u64) -> MonotonicMs {
    MonotonicMs::from_millis(value)
}

// ---------------------------------------------------------------------------
// Sampled series (same contract as the presentation-plane campaign)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Bounds {
    rss_growth_kib: u64,
    fd_growth: i64,
    ring_events: u64,
    ring_bytes: u64,
    lease_events: u64,
    accounted_bytes: u64,
}

struct Sample {
    label: String,
    elapsed_ms: u128,
    simulated_ms: u64,
    cycles: u64,
    rss_kib: u64,
    fds: i64,
    ring_events: u64,
    ring_bytes: u64,
    leases: u64,
    pending_deliveries: u64,
    terminal_consumers: u64,
    accounted_bytes: u64,
    sessions: u64,
}

struct Series {
    name: &'static str,
    budget: Duration,
    bounds: Bounds,
    samples: Vec<Sample>,
}

impl Series {
    fn new(name: &'static str, budget: Duration, bounds: Bounds) -> Self {
        Self {
            name,
            budget,
            bounds,
            samples: Vec::new(),
        }
    }

    fn push(&mut self, sample: Sample) {
        self.samples.push(sample);
    }

    /// Name the sample closest to the half-way point `middle`, so the series
    /// always names start/middle/end even when the cadence drifts.
    fn finalize_labels(&mut self) {
        let last = self.samples.len().saturating_sub(1);
        for (index, sample) in self.samples.iter_mut().enumerate() {
            if sample.label.is_empty() {
                sample.label = format!("s{index:02}");
            }
        }
        if self.samples.iter().any(|sample| sample.label == "middle") {
            return;
        }
        let half = self.budget.as_millis() / 2;
        let mut best: Option<(u128, usize)> = None;
        for (index, sample) in self.samples.iter().enumerate() {
            if index == 0 || index == last {
                continue;
            }
            let distance = sample.elapsed_ms.abs_diff(half);
            if best.is_none_or(|(current, _)| distance < current) {
                best = Some((distance, index));
            }
        }
        if let Some((_, index)) = best {
            self.samples[index].label = "middle".to_string();
        }
    }

    fn rss_growth_kib(&self) -> u64 {
        match (self.samples.first(), self.samples.last()) {
            (Some(first), Some(last)) => last.rss_kib.saturating_sub(first.rss_kib),
            _ => 0,
        }
    }

    fn fd_growth(&self) -> i64 {
        match (self.samples.first(), self.samples.last()) {
            (Some(first), Some(last)) => last.fds.saturating_sub(first.fds),
            _ => 0,
        }
    }

    fn peak_pending_deliveries(&self) -> u64 {
        self.samples
            .iter()
            .map(|sample| sample.pending_deliveries)
            .max()
            .unwrap_or(0)
    }

    fn render(&self) -> String {
        let bounds = self.bounds;
        let mut out = format!(
            "\nsoak campaign `{}` failed; budget={:?} samples={} bounds: \
             rss_growth<={}KiB fd_growth<={} ring_events<={} ring_bytes<={} \
             lease_queue_events<={} accounted_bytes<={}\n  {:<8} {:>10} {:>12} {:>8} \
             {:>9} {:>4} {:>6} {:>9} {:>6} {:>10} {:>6} {:>10} {:>7}",
            self.name,
            self.budget,
            self.samples.len(),
            bounds.rss_growth_kib,
            bounds.fd_growth,
            bounds.ring_events,
            bounds.ring_bytes,
            bounds.lease_events,
            bounds.accounted_bytes,
            "label",
            "elapsed_ms",
            "simulated_ms",
            "cycles",
            "rss_kib",
            "fds",
            "ring_ev",
            "ring_bytes",
            "leases",
            "pending_deliv",
            "term_cons",
            "accounted",
            "sessions",
        );
        for sample in &self.samples {
            out.push_str(&format!(
                "\n  {:<8} {:>10} {:>12} {:>8} {:>9} {:>4} {:>6} {:>9} {:>6} {:>10} {:>6} {:>10} {:>7}",
                sample.label,
                sample.elapsed_ms,
                sample.simulated_ms,
                sample.cycles,
                sample.rss_kib,
                sample.fds,
                sample.ring_events,
                sample.ring_bytes,
                sample.leases,
                sample.pending_deliveries,
                sample.terminal_consumers,
                sample.accounted_bytes,
                sample.sessions,
            ));
        }
        out
    }

    fn check(&self, condition: bool, message: &str) {
        if !condition {
            panic!("{message}{}", self.render());
        }
    }

    fn publish(&self, verdict: &str) {
        let rendered = format!(
            "campaign: {}\nbudget: {:?}\nsamples: {}\nverdict: {}\nseries:{}\n",
            self.name,
            self.budget,
            self.samples.len(),
            verdict,
            self.render().replace(" failed;", " recorded;"),
        );
        println!("{rendered}");
        let Ok(dir) = std::env::var("BITTY_SOAK_EVIDENCE_DIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        if std::fs::create_dir_all(&dir).is_err() {
            eprintln!(
                "soak campaign: cannot create evidence dir {}",
                dir.display()
            );
            return;
        }
        let path = dir.join(format!("{}.txt", self.name));
        if let Err(error) = std::fs::write(&path, rendered) {
            eprintln!(
                "soak campaign: cannot write evidence {}: {error}",
                path.display()
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.strip_prefix("VmRSS:")?;
                rest.split_whitespace().next()?.parse::<u64>().ok()
            })
        })
        .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn rss_kib() -> u64 {
    0
}

#[cfg(target_os = "linux")]
fn fd_count() -> i64 {
    std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count() as i64)
        .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn fd_count() -> i64 {
    0
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn new_stream() -> SessionEventStream {
    SessionEventStream::with_limits(
        SessionRegistry::new(RegistryLimits::new(4, 4, 4096, 600_000).expect("registry limits"))
            .expect("registry"),
        EventLimits::default(),
    )
    .expect("empty registry")
}

fn create(stream: &mut SessionEventStream, terminal_id: &str, now: u64) -> SessionId {
    stream
        .create(
            CreateSession {
                terminal_id: Some(TerminalId::supplied(terminal_id).expect("terminal id")),
                owner_peer: None,
            },
            ms(now),
        )
        .expect("create")
        .session_id
}

fn attach(stream: &mut SessionEventStream, session: SessionId, now: u64) -> AttachId {
    stream
        .attach(
            AttachRequest {
                session_id: session,
                after_revision: 0,
                idempotency_key: None,
            },
            ms(now),
        )
        .expect("attach")
        .attach_id
}

fn terminal_outcome() -> TerminalOutcome {
    TerminalOutcome::new(
        TerminalReason::OperatorAbort,
        ExitInformation::NotApplicable,
        CleanupClass::Swept,
    )
    .expect("outcome")
}

fn sample(
    stream: &SessionEventStream,
    session: SessionId,
    leases: &[AttachId],
    label: &str,
    elapsed: Duration,
    simulated_ms: u64,
    cycles: u64,
) -> Sample {
    let mut pending = 0usize;
    let mut queued_bytes = 0usize;
    for attach_id in leases {
        pending = pending.saturating_add(stream.lease_queue_len(session, *attach_id).unwrap_or(0));
        queued_bytes =
            queued_bytes.saturating_add(stream.lease_queue_bytes(session, *attach_id).unwrap_or(0));
    }
    Sample {
        label: label.to_string(),
        elapsed_ms: elapsed.as_millis(),
        simulated_ms,
        cycles,
        rss_kib: rss_kib(),
        fds: fd_count(),
        ring_events: u64::try_from(stream.backlog_len(session).unwrap_or(0)).unwrap_or(u64::MAX),
        ring_bytes: u64::try_from(stream.backlog_bytes(session).unwrap_or(0)).unwrap_or(u64::MAX),
        leases: u64::try_from(stream.lease_count(session).unwrap_or(0)).unwrap_or(u64::MAX),
        pending_deliveries: u64::try_from(pending).unwrap_or(u64::MAX),
        terminal_consumers: u64::try_from(stream.pending_terminal_consumers(session).unwrap_or(0))
            .unwrap_or(u64::MAX),
        accounted_bytes: u64::try_from(stream.accounted_bytes(session).unwrap_or(0))
            .unwrap_or(u64::MAX)
            .saturating_add(u64::try_from(queued_bytes).unwrap_or(u64::MAX)),
        sessions: u64::try_from(stream.session_count()).unwrap_or(u64::MAX),
    }
}

#[test]
#[ignore = "long-duration soak campaign; run via `just soak-campaign`"]
fn event_plane_campaign_stays_bounded() {
    let budget = campaign_budget();
    let cadence = sample_cadence(budget);
    let limits = EventLimits::default();
    let bounds = Bounds {
        #[cfg(target_os = "linux")]
        rss_growth_kib: MAX_RSS_GROWTH_KIB,
        #[cfg(not(target_os = "linux"))]
        rss_growth_kib: 0,
        #[cfg(target_os = "linux")]
        fd_growth: MAX_FD_GROWTH,
        #[cfg(not(target_os = "linux"))]
        fd_growth: 0,
        ring_events: u64::try_from(limits.max_event_backlog_events()).unwrap_or(u64::MAX),
        ring_bytes: u64::try_from(limits.max_event_backlog_bytes()).unwrap_or(u64::MAX),
        lease_events: u64::try_from(limits.max_lease_queue_events()).unwrap_or(u64::MAX),
        // One ring plus two lease queues plus the per-session attach-key store:
        // every accounted byte the stream can hold for one session.
        accounted_bytes: u64::try_from(
            limits
                .max_event_backlog_bytes()
                .saturating_add(limits.max_lease_queue_bytes().saturating_mul(2))
                .saturating_add(limits.max_attach_keys_bytes_per_session()),
        )
        .unwrap_or(u64::MAX),
    };
    let mut series = Series::new("daemon-event-plane", budget, bounds);

    let mut stream = new_stream();
    let start = Instant::now();
    let mut now = 1u64;
    let mut session = create(&mut stream, "campaign-0", now);
    let mut leases: Vec<AttachId> = Vec::new();
    let mut cycles = 0u64;
    let mut transfers = 0u64;
    let mut delivered = 0u64;
    let mut terminal_cycles = 0u64;
    let mut next_sample = start + cadence;

    now += 1;
    leases.push(attach(&mut stream, session, now));
    now += 1;
    leases.push(attach(&mut stream, session, now));

    series.push(sample(
        &stream,
        session,
        &leases,
        "start",
        start.elapsed(),
        now,
        cycles,
    ));

    while start.elapsed() < budget {
        cycles += 1;

        // Daemon event-stream traffic: control transfers in both directions
        // publish into the retained ring and both lease queues.
        for _ in 0..PUBLISHES_PER_CYCLE {
            let (from, to) = match (leases.first(), leases.get(1)) {
                (Some(first), Some(second)) => (*first, *second),
                _ => break,
            };
            now += EVENT_STEP_MS;
            stream
                .publish_control_transfer(session, from, to, ms(now))
                .expect("transfer out");
            transfers += 1;
            now += EVENT_STEP_MS;
            stream
                .publish_control_transfer(session, to, from, ms(now))
                .expect("transfer back");
            transfers += 1;
        }

        // The transport drains a bounded prefix every pass; the rest stays
        // queued, so `pending_deliveries` is a live depth under steady writes.
        if cycles % 3 != 0 {
            if let Some(lease) = leases.first().copied() {
                if let Ok(batch) = stream.take_deliveries(session, lease) {
                    delivered += u64::try_from(batch.len()).unwrap_or(0);
                }
            }
        }

        // Repeated attach/detach: both leases leave and come back.
        if cycles % LEASE_CHURN_EVERY == 0 {
            for lease in leases.drain(..) {
                now += 1;
                stream
                    .detach(session, lease, DetachCause::SelfDetach, ms(now))
                    .expect("detach");
            }
            series.check(
                stream.lease_count(session) == Some(0),
                &format!("cycle {cycles}: detach must release every lease"),
            );
            now += 1;
            leases.push(attach(&mut stream, session, now));
            now += 1;
            leases.push(attach(&mut stream, session, now));
        }

        // Terminal churn: pin the terminal frame, close the pinned leases,
        // collect the retained record past its window, and open a new session.
        if cycles % TERMINAL_EVERY == 0 {
            terminal_cycles += 1;
            now += 1;
            let terminal = stream
                .terminate(session, terminal_outcome(), ms(now))
                .expect("terminate");
            series.check(
                terminal.lost.is_empty(),
                &format!("cycle {cycles}: a lease lost the terminal frame"),
            );
            series.check(
                stream.backlog_len(session) == Some(0),
                &format!("cycle {cycles}: the ring must be freed at the terminal transition"),
            );
            for lease in leases.drain(..) {
                now += 1;
                let _ = stream.close_lease(session, lease, ms(now));
            }
            now += RETENTION_WINDOW_MS;
            let collected = stream.collect_retained(ms(now));
            series.check(
                collected.collected.contains(&session),
                &format!("cycle {cycles}: the retained terminal record must be collected"),
            );
            series.check(
                stream.lease_count(session).is_none(),
                &format!("cycle {cycles}: a collected session must leave no lease"),
            );
            now += 1;
            session = create(&mut stream, &format!("campaign-{terminal_cycles}"), now);
            now += 1;
            leases.push(attach(&mut stream, session, now));
            now += 1;
            leases.push(attach(&mut stream, session, now));
        }

        if Instant::now() >= next_sample && start.elapsed() < budget {
            series.push(sample(
                &stream,
                session,
                &leases,
                "",
                start.elapsed(),
                now,
                cycles,
            ));
            next_sample += cadence;
        }
    }

    series.push(sample(
        &stream,
        session,
        &leases,
        "end",
        start.elapsed(),
        now,
        cycles,
    ));
    series.finalize_labels();

    series.check(
        series.samples.len() >= 3,
        "the campaign must sample at least start, middle, and end",
    );
    series.check(
        series.samples.iter().any(|sample| sample.label == "middle"),
        "the campaign series must name a middle sample",
    );
    series.check(cycles > 0, "the campaign must run at least one cycle");
    series.check(
        transfers > 0 && delivered > 0,
        &format!(
            "the event stream must carry traffic (transfers {transfers}, delivered {delivered})"
        ),
    );
    series.check(
        terminal_cycles > 0,
        "the campaign must execute terminal cycles",
    );
    series.check(
        stream.lease_count(session) == Some(2),
        "the campaign must end with exactly its two live leases",
    );
    series.check(
        stream.session_count() == 1,
        &format!(
            "collected sessions must not accumulate, {} live",
            stream.session_count()
        ),
    );

    // Per-sample bounds: no sample may sit outside a compiled cap.
    for entry in &series.samples {
        series.check(
            entry.ring_events <= bounds.ring_events,
            &format!(
                "sample {}: retained ring {} events exceeds the compiled cap {}",
                entry.label, entry.ring_events, bounds.ring_events
            ),
        );
        series.check(
            entry.ring_bytes <= bounds.ring_bytes,
            &format!(
                "sample {}: retained ring {} bytes exceeds the compiled cap {}",
                entry.label, entry.ring_bytes, bounds.ring_bytes
            ),
        );
        series.check(
            entry.leases <= u64::try_from(limits.max_attaches_per_session()).unwrap_or(u64::MAX),
            &format!(
                "sample {}: {} leases exceeds the per-session cap",
                entry.label, entry.leases
            ),
        );
        series.check(
            entry.pending_deliveries <= bounds.lease_events.saturating_mul(2),
            &format!(
                "sample {}: {} undelivered events exceeds the two lease queues' caps",
                entry.label, entry.pending_deliveries
            ),
        );
        series.check(
            entry.accounted_bytes <= bounds.accounted_bytes,
            &format!(
                "sample {}: {} accounted bytes exceeds the per-session budget {}",
                entry.label, entry.accounted_bytes, bounds.accounted_bytes
            ),
        );
    }
    series.check(
        series.peak_pending_deliveries() > 0,
        "the workload must leave events queued for delivery",
    );

    // Bounded growth across the whole budget.
    series.check(
        series.rss_growth_kib() <= bounds.rss_growth_kib,
        &format!(
            "RSS grew {} KiB across the campaign (cap {} KiB)",
            series.rss_growth_kib(),
            bounds.rss_growth_kib
        ),
    );
    series.check(
        series.fd_growth() <= bounds.fd_growth,
        &format!(
            "open fds grew {} across the campaign (cap {})",
            series.fd_growth(),
            bounds.fd_growth
        ),
    );

    series.publish("bounded");
}
