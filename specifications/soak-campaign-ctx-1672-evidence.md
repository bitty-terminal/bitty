# Long-duration soak campaign evidence (CTX-1672; bitty#2830)

Campaign: `soak-campaign` — the compressed long-run campaign driver for the
P0 long-uptime unresponsiveness class. It closes the last acceptance bullet on
bitty#2830 that was still open after the merged instrumentation, the bounded
opencode soak, the daemon self-heal, and the bounded present nudge: a
long-duration run with sampled bounded-growth assertions.

This record is a measurement of a run executed on the authoring host, not a
projection. It does **not** claim the owner-hours form was executed; see
"Residual gap" below for exactly what remains.

## Campaign shape

One campaign name, two planes, split where the workspace dependency direction
forces it (the daemon sits above the mechanism crates, and `bitty-runtime`
must not depend on it):

| Plane | Target | Drives |
| --- | --- | --- |
| Presentation | `bitty-runtime/tests/soak_campaign.rs` (`presentation_plane_campaign_stays_bounded`) | sustained PTY byte volume (truecolor TUI repaint, title/query traffic, 32 KiB paste writes), repeated attach/detach of real shell panes, panel border-drag and window resize, alternate-screen/scrollback churn, and app-side event-stream traffic into a subscribed plugin's bounded delivery queue |
| Plugin churn | `bitty-runtime/tests/soak_campaign.rs` (`plugin_activation_campaign_stays_bounded`) | repeated plugin activate → dispatch → dispose cycles (worker leases) with a real Lua VM |
| Daemon event | `bitty-daemon/tests/soak_campaign.rs` (`event_plane_campaign_stays_bounded`) | daemon event-stream traffic (`control_transfer` mutations into the retained ring and both lease queues), repeated lease attach/detach, terminal transitions that pin the terminal frame, close the pinned leases, collect the retained record past its window, and re-create the session |

Budget: `BITTY_SOAK_CAMPAIGN_SECS` (or the `just soak-campaign <secs>`
argument), clamped to 8..86400 s, applied per test. Sampling cadence is one
eighth of the budget, clamped to 0.5..60 s, so start/middle/end are always
sampled and an hours-long run does not emit a line per second. Every assertion
goes through `Series::check`, which prints the full sampled series on failure;
`BITTY_SOAK_EVIDENCE_DIR` writes the same series as files.

## Measured run

- Revision: `80c60175` (origin/main at branch start) plus this change in the
  worktree; no release tag claims anything here.
- Command: `BITTY_SOAK_EVIDENCE_DIR=<dir> just soak-campaign 180`
  (which runs `cargo test --locked -p bitty-runtime --test soak_campaign
  -p bitty-daemon --test soak_campaign -- --ignored --nocapture
  --test-threads=1` with `BITTY_SOAK_CAMPAIGN_SECS=180`).
- Host: Linux 7.3.0-4-cachyos x86_64, 24 hardware threads, `rustc 1.98.1`,
  unoptimized `test` profile (`CARGO_PROFILE_TEST_DEBUG=0` not set: default
  test debug info), headless throughout (no window, GPU, or display server).
- Wall clock: 360.1 s for all three tests (two runtime tests at 180 s each, one
  daemon test at 180 s); every budget was honored to within 0.2 s.
- Result: `3 passed; 0 failed`.

### Presentation plane (180 s budget, 1,190 cycles, 40 min simulated)

Bounds: `rss_growth<=98304 KiB fd_growth<=16 pending_deliveries<=64
event_ring<=128 content_bytes<=131072 KiB`.

| sample | elapsed_ms | simulated_ms | cycles | rss_kib | fds | pending_deliv | event_ring | workers | content_kib | cold_q | panes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| start | 455 | 24000 | 0 | 33284 | 4 | 12 | 128 | 0 | 530 | 0 | 0 |
| s01 | 22604 | 344000 | 160 | 55576 | 4 | 62 | 128 | 0 | 6997 | 0 | 0 |
| s02 | 45106 | 642000 | 309 | 50724 | 4 | 64 | 128 | 0 | 6997 | 0 | 0 |
| s03 | 67522 | 930000 | 453 | 50724 | 4 | 64 | 128 | 0 | 6997 | 0 | 0 |
| middle | 90050 | 1228000 | 602 | 56140 | 4 | 64 | 128 | 0 | 7205 | 0 | 0 |
| s05 | 112548 | 1520000 | 748 | 55576 | 4 | 64 | 128 | 0 | 7205 | 0 | 0 |
| s06 | 135283 | 1816000 | 896 | 55576 | 4 | 62 | 128 | 0 | 7205 | 0 | 0 |
| s07 | 157595 | 2112000 | 1044 | 56140 | 4 | 64 | 128 | 0 | 7205 | 0 | 0 |
| end | 180107 | 2404000 | 1190 | 56688 | 4 | 64 | 0 | 0 | 5833 | 0 | 0 |

Result: bounded. RSS growth 23,404 KiB against the 98,304 KiB cap and flat
after the first sample (the early rise is scrollback filling to its 4,000-line
cap); file descriptors constant at 4; the per-subscriber delivery queue
saturates at its compiled 64-event cap and never exceeds it; the retained side
ring saturates at its 128-event cap and drains to 0 at the end; retained
content plateaus at ~7.0 MiB; no pane session, parked command, active command,
pending reply, or hidden-damage frame survives the run.

### Plugin activation churn (180 s budget, 110,271 cycles)

Bounds: `rss_growth<=98304 KiB fd_growth<=16`.

| sample | elapsed_ms | cycles | rss_kib | fds | workers |
| --- | --- | --- | --- | --- | --- |
| start | 0 | 0 | 21072 | 4 | 0 |
| s01 | 22500 | 13683 | 25392 | 4 | 0 |
| s02 | 45000 | 27623 | 25392 | 4 | 0 |
| s03 | 67500 | 41476 | 25392 | 4 | 0 |
| middle | 90001 | 55209 | 25392 | 4 | 0 |
| s05 | 112500 | 69021 | 25392 | 4 | 0 |
| s06 | 135000 | 82650 | 25392 | 4 | 0 |
| s07 | 157500 | 96318 | 25392 | 4 | 0 |
| end | 180001 | 110271 | 25392 | 4 | 0 |

Result: bounded. RSS is flat at 25,392 KiB after the first VM is built (4,320
KiB of one-off allocator growth) across 110,271 activate/dispose cycles;
worker leases return to 0 on every cycle and no command is left active or
parked.

### Daemon event plane (180 s budget, 12,277,039 cycles)

Bounds: `rss_growth<=65536 KiB fd_growth<=16 ring_events<=256
ring_bytes<=262144 lease_queue_events<=256 accounted_bytes<=458752`.

| sample | elapsed_ms | cycles | rss_kib | fds | ring_ev | ring_bytes | leases | pending_deliv | term_cons | accounted |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| start | 0 | 0 | 3872 | 4 | 2 | 240 | 2 | 1 | 0 | 904 |
| s01 | 22500 | 1534260 | 3996 | 4 | 178 | 21360 | 2 | 1 | 0 | 22024 |
| s02 | 45000 | 3071880 | 3996 | 4 | 2 | 240 | 2 | 1 | 0 | 904 |
| s03 | 67500 | 4602335 | 3996 | 4 | 134 | 16080 | 2 | 1 | 0 | 16744 |
| middle | 90000 | 6135344 | 3996 | 4 | 210 | 25200 | 2 | 32 | 0 | 33304 |
| s05 | 112500 | 7669087 | 3996 | 4 | 62 | 7440 | 2 | 16 | 0 | 11704 |
| s06 | 135000 | 9185336 | 3996 | 4 | 142 | 17040 | 2 | 8 | 0 | 19384 |
| s07 | 157500 | 10738288 | 3996 | 4 | 70 | 8400 | 2 | 24 | 0 | 14584 |
| end | 180000 | 12277039 | 3996 | 4 | 256 | 30720 | 2 | 32 | 0 | 38824 |

Result: bounded. RSS growth 124 KiB and file descriptors constant at 4 across
12.3 M publish/drain/attach/detach/terminal cycles; the retained ring never
exceeds 256 events / 262,144 bytes (observed peak 256 events, 30,720 bytes);
undelivered events stay at or below the two 256-event lease-queue caps
(observed peak 32); accounted bytes peak at 38,824 against the 458,752-byte
per-session budget; exactly one session is live at the end and its two leases
are the only leases (collected sessions do not accumulate).

## CI wiring

`.github/workflows/soak-nightly.yml` runs the multi-minute form (300 s per
plane by default, `workflow_dispatch` input `duration_secs` clamped to
60..600 s) nightly at 03:17 UTC on `ubuntu-24.04`, with `timeout-minutes: 60`,
a build step before the run, and `actions/upload-artifact` of
`soak-evidence/` (`if: always()`, 30-day retention). The 600 s cap is the
largest per-plane budget that keeps three serial plane runs plus a cold
dependency build inside the 60-minute job timeout; an owner campaign that needs
hours runs `just soak-campaign <secs>` directly (the driver itself accepts up
to 86,400 s). It is a separate workflow from `.github/workflows/ci.yml` and is
not in the branch-protection required check set, so the PR jobs keep their
current wall clock. The campaign tests are `#[ignore]`d, so
`cargo nextest run --workspace` does not execute them on any push or pull
request.

## Residual gap

- This record is the **180 s-per-plane** form on one Linux developer host: 360 s
  of driving, 40 simulated minutes of presentation churn, 110 k plugin
  activate/dispose cycles, and 12.3 M daemon event-plane cycles. The
  owner-hours form (hours of wall clock, `BITTY_SOAK_CAMPAIGN_SECS` up to
  86,400) and the nightly CI form (300 s per plane) were **not** executed as
  part of this change; the nightly workflow is the first place the CI form
  runs, and its evidence artifact is the record for that run.
- The campaign compresses time on the injected clock rather than waiting for a
  real multi-hour session; it cannot prove the absence of a failure mode that
  only appears under real wall-clock conditions (kernel scheduling, thermal
  state, long-lived GPU driver state). It bounds the class the issue names -
  monotonic growth of RSS, descriptors, leases, queues, and retained rings -
  over a run long enough for leaks to separate from warmup noise.
- RSS and file-descriptor samples are Linux-only (`/proc`); other targets run
  the identical workload and keep the queue/lease/ring assertions.
- The presentation plane's sustained byte volume is synthetic and
  deterministic; it does not replay a captured real application session.
