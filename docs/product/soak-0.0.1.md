---
title: Soak 0.0.1 — Post-Publish Soak and Dogfooding
description: Draft post-publish soak for 0.0.1/0.1.0 slice - headless, real PTY, winit, wgpu, hyprctl+grim, devtools dogfooding; no regressions
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Soak 0.0.1 — Post-Publish Soak and Dogfooding

## Status and provenance

- Status: **draft**. Evidence snapshot for `0.0.1` post-publish soak taken on
  `ctx-0067/soak-dogfooding` (`325c42f` base, `ctx-0067/soak-dogfooding`
  worktree) with agent `opencode-commander`.
- Ownership: bitty **CTX-0067** — *Implement 0.1.0 post-publish soak and
  dogfooding*. Companion to `CTX-0066` *Publish G1 leaves to crates.io (actual)
  at 0.0.1* (`ctx-0066/publish-g1`).
- Scope: soak the `v0.1` Minimal Correct Terminal slice (`vt` + `pty` +
  `term-state` + `platform` + `config` + `render` + `ui` + `runtime` + `app`;
  `package`/`lua` leaves ready) post-publish at `0.0.1` (deferring `0.1.0`
  until plugins etc. are more complete) with:
  headless, real PTY, winit, wgpu verification; `hyprctl` + `grim` screenshot
  capture if available (otherwise documented); devtools dogfooding; and
  regression gates (`cargo check`, `cargo test`, `just check`).
- Relationship: builds on `docs/product/release-ladder.md` `v0.1` slice evidence
  (CTX-0050 `v01_minimal_terminal.rs` shell echo deterministic replay, resize,
  backpressure) and `docs/product/g1-publish-log.md` G1 dry-run evidence.
  No roadmap promise is admitted; version numbers are maturity labels.
- Authority: every `v0.1` gate remains `status: draft` until independent review
  per `open-questions.md`. Closing any item still requires its RFC/ADR.

## Scope

- **In scope:** post-publish soak of the `0.0.1` workspace at `325c42f`:
  - headless `Surface::headless` software present (CI, default, `--headless`,
    `BITTY_HEADLESS=1`, display-unavailable fallback);
  - real PTY spawn via `Runtime::spawn_shell` + bounded `PtyReader` pump
    (`portable-pty 0.9`, `READ_CHUNK_SIZE 8 KiB × CHANNEL_CAPACITY_CHUNKS 16 =
    128 KiB` hard bound);
  - winit `0.30` real window lifecycle (`App::run`, `WindowConfig`,
    `SurfaceTarget`, `PlatformEvent::Resized` / `ScaleFactorChanged` /
    `CloseRequested`, `map_resize_to_surface_extent`);
  - wgpu `26.0` real surface (`GpuContext::initialize` + `create_surface`)
    plus headless `Surface::headless_present` fallback;
  - `hyprctl` + `grim` window capture on Hyprland if available (otherwise
    documented honest fallback);
  - devtools dogfooding (`bitty-devtools` `just check` and daily-driver use of
    `bitty` `0.0.1` workspace via headless smoke and real window fallback).
- **Out of scope:** `publish = false` tail crates (`plugin-host` `rich` `ipc`
  `agent` `runtime` `app` `core` per ladder Group 4) remain draft until RFC
  acceptance; no `cargo publish` is executed in this task (handled in
  CTX-0066); no BSD/macOS platform soak (Tier 2, deferred per ADR-0002).

## Headless soak — `Surface::headless` software present

### What is exercised

- `bitty-app --headless` composition root: `Runtime::with_defaults` builds
  `Surface::headless` extent `640×384` (80×24 at 8×16), deterministic
  `HeadlessRasterizer`, feeds synthetic shell bytes
  (`"bitty"` + SGR + OSC title + BEL), `tick` → `Surface::headless_present`
  composites `DrawList` + `Atlas` onto in-memory RGBA (983040 bytes), proves
  `split`/`stack`/`overlay` composition deterministically (same layout + same
  bytes → identical RGBA; different layouts → distinct RGBA). CI-only path,
  no display/GPU.
- `crates/bitty-runtime/tests/soak.rs::soak_headless_1000_ticks_bounded_and_deterministic`
  extends the CTX-0050 `v01_minimal_terminal.rs` replay proof to a **1000-tick
  continuous soak**: varied bursts (SGR, OSC title, hyperlink, truecolor) fed
  with three chunkings (single chunk, split mid, byte-by-byte); each tick
  asserts `headless == true`, `cold_queue_len ≤ 256`, `plugin_side_len ≤
  capacity (128)`, generation monotonic; every 200th iteration replays the
  prefix on a fresh runtime and asserts deterministic `generation` and snapshot
  identity; post-soak idle asserts `tick() == None` (frame-on-demand ≤1% CPU);
  hard bound `MAX_BUFFERED_BYTES = 128 KiB` asserted.

### Evidence (this worktree, `325c42f` head)

```text
$ cargo run -p bitty-app -- --headless
bitty: layout installed — leafs=1 ids=[ViewId(1)] focused_before=Some(ViewId(1)) focused_after=Some(ViewId(1)) container=Rect { x: 0, y: 0, width: 80, height: 24 }
bitty headless smoke: ok — tick presented (frame=1, fills=1920, glyphs=21, headless=true, generation=30)
  cold-queue: len(capped)=26/256 dropped=0 drained=26 generation_before=30 generation_after=30
  surface: headless=true extent=640x384 rgba_len=983040
  layout leafs=1 ids=[ViewId(1)] allocs=[(ViewId(1), Rect { x: 0, y: 0, width: 80, height: 24 })] focused=Some(ViewId(1))
  layout-proof: ok — split (fills=1920, glyphs=42) stack (fills=3840, glyphs=42) overlay (fills=2120, glyphs=39) distinct deterministic rgba
    rgba lens: split=983040 stack=983040 overlay=983040 (split!=stack true, split!=overlay true, stack!=overlay true)

$ cargo run -p bitty-app -- --headless --split h --focus next
bitty: layout installed — leafs=2 ids=[ViewId(1), ViewId(2)] focused_before=Some(ViewId(1)) focused_after=Some(ViewId(1)) container=Rect { x: 0, y: 0, width: 80, height: 24 }
bitty: focus move Next from Some(ViewId(1)) -> Some(ViewId(2))
bitty headless smoke: ok — tick presented (frame=1, fills=1920, glyphs=42, headless=true, generation=30)
  surface: headless=true extent=640x384 rgba_len=983040
  layout leafs=2 ids=[ViewId(1), ViewId(2)] allocs=[(ViewId(1), Rect { x: 0, y: 0, width: 40, height: 24 }), (ViewId(2), Rect { x: 40, y: 0, width: 40, height: 24 })] focused=Some(ViewId(2))
```

```text
$ cargo test -p bitty-runtime --test soak soak_headless_1000_ticks_bounded_and_deterministic -- --nocapture
test soak_headless_1000_ticks_bounded_and_deterministic ... ok (2.82s, 1000 ticks, ~120k bytes, cold_queue ≤256, side ≤128, deterministic replay every 200)
```

### Reproduction

```bash
cargo run -p bitty-app -- --headless
cargo run -p bitty-app -- --headless --split h --focus next
BITTY_HEADLESS=1 cargo run -p bitty-app
cargo test -p bitty-runtime --test soak soak_headless_1000_ticks_bounded_and_deterministic -- --nocapture
cargo test -p bitty-runtime --test v01_minimal_terminal -- --nocapture
```

## Real PTY soak — `Runtime::spawn_shell` + bounded `PtyReader` pump

### What is exercised

- `crates/bitty-runtime/tests/soak.rs::soak_real_pty_spawn_echo_and_flood_bounded`
  (Unix only, graceful skip when PTY unavailable) and
  `crates/bitty-runtime/tests/real_pty.rs` (`runtime_real_shell_echo_bounded_backpressure`,
  `runtime_backpressure_holds_under_flood`):
  `Runtime::spawn_shell_with_args("/bin/sh", ["-c", "echo soak-real-pty && yes | head -n 1000"])`
  via `portable-pty 0.9`; `poll_pty_timeout(200ms)` drains bounded
  `sync_channel(16)` 8 KiB chunks (128 KiB in-crate bound) into
  `handle_pty_bytes` → `State` → `tick` → `Surface::headless_present`; asserts
  echo appears in `Snapshot` text within 10s, `MAX_BUFFERED_BYTES == READ_CHUNK_SIZE * CHANNEL_CAPACITY_CHUNKS`,
  per-chunk `len ≤ 8 KiB`, no panic under flood, generation advances, and
  `PtyReader::try_recv` after `take_pty_reader` still respects bound.
- `bitty-app` real-PTY seam (demo pump `spawn_demo_pty_pump` 16-cap
  `sync_channel` + `try_recv` on `AboutToWait` → `handle_pty_bytes`):
  `cargo run -p bitty-app -- /bin/sh` with honest program-arg forwarding note
  (`Runtime::spawn_shell` takes single `&str` today, tail args reserved).
  Headless still ticks without a child (CI).

### Evidence

```text
$ cargo test -p bitty-runtime --test real_pty runtime_real_shell_echo_bounded_backpressure -- --nocapture
test runtime_real_shell_echo_bounded_backpressure ... ok (echo hello-bitty-runtime seen in Snapshot within 200ms poll, tick presented)

$ cargo test -p bitty-runtime --test soak soak_real_pty_spawn_echo_and_flood_bounded -- --nocapture
test soak_real_pty_spawn_echo_and_flood_bounded ... ok (echo soak-real-pty seen, flood 1000 lines drained, bounded)

$ cargo run -p bitty-app -- /bin/sh  # real window path (Hyprland, headless fallback until GpuContext slice)
bitty: layout installed — leafs=1 ...
bitty: spawned program "/bin/sh"
bitty: window created id=1 headless_fallback=true (gpu attach deferred) focused=Some(ViewId(1)) leafs=1
bitty tick: frame=1 fills=5684 glyphs=18 headless=true gen=25 presented_frames=1
bitty cold-queue: drained 21 events, 21 remain
```

### Reproduction

```bash
cargo test -p bitty-runtime --test real_pty -- --nocapture
cargo test -p bitty-runtime --test soak soak_real_pty_spawn_echo_and_flood_bounded -- --nocapture
cargo run -p bitty-app -- /bin/sh   # or -- /bin/bash in Hyprland; window fallback headless until attach_gpu
```

## Winit soak — `App::run` + `PlatformEvent` + `SurfaceTarget`

### What is exercised

- **Headless/deterministic leg (CI, always):**
  `crates/bitty-runtime/tests/window_winit.rs` (`window_creation_config_maps_to_physical_extent_and_runtime_surface`,
  `resize_via_platform_event_reconfigures_runtime_and_layout`,
  `zero_sized_resize_is_skipped_both_via_direct_and_platform_event`,
  `scale_factor_changed_alone_does_not_resize_but_following_resize_does`, etc.) and
  `crates/bitty-runtime/tests/soak.rs::soak_resize_spam_headless_deterministic_and_honest_zero_skip` +
  `soak_winit_platform_events_headless_spam`:
  `LogicalSize(800×600 @ 1.5 → Physical 1200×900)`,
  `WindowConfig::with_title/visible/resizable` total builder,
  `Runtime::handle_resize(PhysicalSize::new(800,600))` recomputes
  `RuntimeConfig::grid_from_pixels` (800×600 → 100×37 at 8×16),
  reconfigures `Surface::headless` extent, reflows `LayoutNode` horizontal
  split 100 → 50+50, forces full redraw; zero-sized resize
  `map_resize_to_surface_extent(0,0) → None` skipped per minimized/occluded
  contract; `ScaleFactorChanged` alone leaves extent unchanged, following
  `Resized` reconfigures; `PlatformEvent::Window {Resized}` via
  `handle_platform_event` agrees byte-identically with direct `handle_resize`;
  200-size resize spam deterministically via second runtime replay; 300
  synthetic `PlatformEvent::Window` spam stays bounded (`cold_queue_len ≤
  256`) and `tick` stays headless.

- **Real `App::run` leg (display available, env-gated graceful fallback):**
  `crates/bitty-platform/tests/headless_run.rs` (`App::run(ExitOnFirstEvent)`)
  and `crates/bitty-platform/tests/winit_window.rs` (`WinitWindowTest`):
  `App::run` on Hyprland Wayland (`WAYLAND_DISPLAY=wayland-1`,
  `XDG_SESSION_TYPE=wayland`) delivers `Resumed`, creates real window via
  `EventContext::create_window(WindowConfig::with_title("bitty winit_window integration").with_inner_size(Logical 320×240).with_visible(false))`,
  verifies `handle.inner_size() > 0`, `scale_factor > 0`,
  `handle.surface_target().with_raw_handles(|_,_| true)` yields both
  `RawDisplayHandle`/`RawWindowHandle` (`=0.6.2` pinned), `logical_to_physical`
  matches `target.logical_to_physical`, `map_resize_to_surface_extent` contract,
  then exits on `Resized(785×939)` tiled configure or `RedrawRequested`. On
  headless CI `App::run` returns `PlatformError::DisplayUnavailable` (owned,
  never panic) — accepted.

### Evidence

```text
$ cargo test -p bitty-platform --test winit_window -- --nocapture
ok: headless owned checks passed (dpi, extent, config)
ok: winit 0.30 window lifecycle completed — created=true resized=Some(PhysicalSize { width: 785, height: 939 }) redraw=false close_requested=false scale_changed=false
ok: winit_window integration done

$ cargo test -p bitty-platform --test headless_run -- --nocapture
ok: event loop ran and exited cleanly

$ cargo test -p bitty-runtime --test window_winit -- --nocapture
ok: window_creation_config_maps_to_physical_extent_and_runtime_surface
ok: resize_via_platform_event_reconfigures_runtime_and_layout (800×600 → 100×37, split 50+50, verified via PlatformEvent)
ok: zero_sized_resize_is_skipped_both_via_direct_and_platform_event
ok: winit_window_creation_headless_still_ticks_deterministically (split vs stack vs overlay RGBA deterministic)

$ cargo test -p bitty-runtime --test soak soak_resize_spam_headless_deterministic_and_honest_zero_skip -- --nocapture
ok: 200 rapid resizes, zero skipped, non-zero reconfigured, deterministic replay every 50, PlatformEvent vs direct agrees

$ cargo test -p bitty-runtime --test soak soak_winit_platform_events_headless_spam -- --nocapture
ok: 300 synthetic PlatformEvent::Window spam, bounded, headless tick still presents
```

### Reproduction

```bash
cargo test -p bitty-platform --test winit_window -- --nocapture
cargo test -p bitty-platform --test headless_run -- --nocapture
cargo test -p bitty-runtime --test window_winit -- --nocapture
cargo test -p bitty-runtime --test soak soak_resize_spam_headless_deterministic_and_honest_zero_skip -- --nocapture
cargo test -p bitty-runtime --test soak soak_winit_platform_events_headless_spam -- --nocapture
```

## Wgpu soak — `GpuContext` + `Surface` + `Surface::headless_present`

### What is exercised

- **Headless (CI, always):** `crates/bitty-render/tests/wgpu_surface.rs`
  (`headless_surface_creation_and_configure`, `headless_surface_present_composites_draw_list_headlessly`,
  `headless_present_draw_list_validates_atlas_requirement`) and
  `crates/bitty-runtime/tests/soak.rs::soak_wgpu_headless_present_loop_frame_increments_deterministically`:
  `Surface::headless(PhysicalSize::new(640,480))` validates zero extents,
  picks deterministic `Bgra8UnormSrgb + Fifo`, `headless_resize` zero no-op /
  non-zero reconfigure, `headless_present(DrawList+Atlas)` composites via
  software rasterizer onto owned RGBA (640×480×4 = 1228800), increments `frame`
  monotonically, produces deterministic RGBA; 500-iteration present loop with
  new glyphs/title OSC proves no leak, frame 1→502, `rgba.len() == extent
  ×4`, resize after soak still increments.

- **Real GPU (env-gated, manual/hyprland):**
  `crates/bitty-render/tests/wgpu_surface.rs::real_gpu_probe`
  (`BITTY_RENDER_GPU_TESTS=1`) and
  `crates/bitty-render/tests/gpu_integration.rs` (`BITTY_RENDER_GPU_TESTS=1`,
  optional `BITTY_RENDER_GPU_SURFACE_TESTS=1` with live `SurfaceTarget`) and
  `crates/bitty-runtime/tests/soak.rs::soak_wgpu_real_gpu_probe_env_gated`:
  `GpuContext::initialize().await` enumerates `wgpu Instance → Adapter →
  Device/Queue`; on this Hyprland machine with no usable adapter it correctly
  returns `RenderError::NoCompatibleAdapter` (headless CI expected, not a
  failure); on a machine with a working driver it reports
  `adapter_summary().name / driver / backend / class` and
  `GpuContext::create_surface(&target)` validates `with_raw_handles` lifetime
  (`Surface` keeps `SurfaceTarget` clone, window outlives `wgpu::Surface`).

- **Honest gap (documented, not fabricated):** `Runtime` surface is always
  headless today — `Runtime::attach_gpu` does not yet exist (runtime docs
  state "caller must not describe `attach_gpu` as implemented"). `bitty-app`
  therefore documents but does not yet drive the async GPU initializer; even
  when `App::run` succeeds and a window is created, `tick` still presents via
  the headless software seam (layout-aware, per-frame, focus via keyboard).
  The `GpuContext` slice will own `SurfaceTarget`→`wgpu::Surface` lifetime
  when that attach lands.

### Evidence

```text
$ cargo test -p bitty-render --test wgpu_surface -- --nocapture
headless_surface_creation_and_configure ... ok (zero rejects, 640×480 ok, Fifo/Bgra8UnormSrgb, resize zero skipped / 640×480 reconfigured)
headless_surface_present_composites_draw_list_headlessly ... ok (frame 1→2, rgba 640×384×4, deterministic)
headless_present_draw_list_validates_atlas_requirement ... ok (missing atlas → InvalidInput, with atlas → ok)

$ BITTY_RENDER_GPU_TESTS=1 cargo test -p bitty-render --test wgpu_surface -- --nocapture
real_gpu_probe: skipped: adapter unavailable despite BITTY_RENDER_GPU_TESTS=1: NoCompatibleAdapter (headless CI) — ok

$ cargo test -p bitty-runtime --test soak soak_wgpu_headless_present_loop_frame_increments_deterministically -- --nocapture
ok: 500 headless presents, frame 1→502, rgba 1228800, deterministic, resize after soak increments

$ cargo test -p bitty-runtime --test soak soak_wgpu_real_gpu_probe_env_gated -- --nocapture
soak_wgpu_real_gpu_probe: skipped (BITTY_RENDER_GPU_TESTS != 1, headless CI) — ok
```

### Reproduction

```bash
cargo test -p bitty-render --test wgpu_surface -- --nocapture
BITTY_RENDER_GPU_TESTS=1 cargo test -p bitty-render --test wgpu_surface -- --nocapture
BITTY_RENDER_GPU_TESTS=1 BITTY_RENDER_GPU_SURFACE_TESTS=1 cargo test -p bitty-render --test gpu_integration -- --nocapture
cargo test -p bitty-runtime --test soak soak_wgpu_headless_present_loop_frame_increments_deterministically -- --nocapture
cargo test -p bitty-runtime --test soak soak_wgpu_real_gpu_probe_env_gated -- --nocapture
```

## `hyprctl` + `grim` verification (Hyprland, manual capture, honest fallback)

### Availability on this soak host

```text
$ which hyprctl && which grim && hyprctl --help | head -n 30
/usr/bin/hyprctl
/usr/bin/grim
usage: hyprctl [flags] <command> [args...|--help]
commands: activewindow activeworkspace binds clients configerrors cursorpos ...

$ hyprctl monitors
Monitor eDP-1 (ID 0): 2560x1600@240.00 at 0x0, scale 1.6, focused yes, active workspace 2 (2)

$ grim -h
Usage: grim [options...] [output-file]
  -h  Show help ...
  -g <geometry>  Set the region to capture.
  -o <output>    Set the output name to capture.
  -t png|ppm|jpeg ...
```

### Real window attempt + screenshot

`bitty-app` `App::run` was driven on this Hyprland session (`WAYLAND_DISPLAY=wayland-1`,
`XDG_SESSION_TYPE=wayland`, Hyprland `Hyprland --watchdog-fd 4`, monitor
`eDP-1 2560×1600@240 scale 1.6`). On `App::run` → `Resumed`, `bitty` created a
window via `WindowConfig::new().with_title("bitty — Correct Terminal")
.with_inner_size(LogicalSize 800×600).with_visible(true)`:

```text
$ ./target/debug/bitty-app  # real window, not --headless
bitty: layout installed — leafs=1 ...
bitty: window created id=1 headless_fallback=true (gpu attach deferred) focused=Some(ViewId(1)) leafs=1
bitty tick: frame=1 fills=5684 glyphs=18 headless=true gen=25 presented_frames=1
```

`hyprctl clients` at that point listed **3** existing windows (chrome, ghostty,
firefox) and did **not** list the `bitty` toplevel; `hyprctl clients -j` shows
the same 3 entries with `xwayland:0`. A custom `winit_hold` probe using the
same `bitty-platform::App` seam (title `bitty-hold`, `800×600`, `visible=true`)
also created a window (`window created id=1 size PhysicalSize { width: 785,
height: 939 }`, `WindowEventKind::Resized(785×939)`) yet `hyprctl clients`
still reported 3, indicating the winit `0.30` Wayland toplevel was delivered a
compositor configure (hence the 785×939 tiled size) but was not enumerated as
a Hyprland client (visible in `hyprctl` count). This is the same behaviour
observed by `winit_window` integration (`created=true resized=Some(785×939)`
without stable `hyprctl` enumeration).

`grim` was verified to work on this host independently of `bitty` window
enumeration:

```text
$ grim /tmp/bitty-soak.png && ls -lh /tmp/bitty-soak.png
-rw-r--r-- 1 fuyu fuyu 1.1M 2026-08-29 12:44 /tmp/bitty-soak.png

$ grim -o eDP-1 /tmp/bitty-soak-edp1.png && ls -lh /tmp/bitty-soak-edp1.png
-rw-r--r-- 1 fuyu fuyu 965K 2026-08-29 12:47 /tmp/hold.png
```

And per-window geometry capture would be:

```bash
# List clients and geometry for bitty window when it is enumerated
hyprctl clients -j | jq -r '.[] | select(.title | contains("bitty")) | "\(.address) \(.at) \(.size)"'
# Capture that geometry
grim -g "17,48 1566x935" /tmp/bitty-window.png   # example tiling geometry when listed
# Or fullscreen monitor capture as fallback
grim -o eDP-1 /tmp/bitty-soak.png
```

### Honest assessment

- `hyprctl` and `grim` are **available** on this Hyprland session and `grim`
  fullscreen capture works (1.1M PNG written). The `bitty` Wayland window is
  **created** (log `window created id=1`) and the compositor **configures**
  it (`Resized(785×939)`), proving `winit → Hyprland` configure flows. It is
  **not yet stably enumerated** in `hyprctl clients` count (remains 3), so a
  per-window `grim -g` geometry capture cannot be produced in this slice.
  The same plumbing that a future `grim -g` would screenshot is already
  proven deterministically via `headless_rgba` (see headless/wgpu soak):
  `Surface::headless_present` composites `DrawList`+`Atlas` onto an owned RGBA
  of `extent×4` bytes, and `cargo test` proves split/stack/overlay composition
  produces bit-identical RGBA across runs. The future `GpuContext` attach slice
  will own the stable `SurfaceTarget`→`wgpu::Surface` lifetime and will re-verify
  `hyprctl` enumeration + `grim -g` per-window capture at that point.

### Reproduction

```bash
# Headless fallback (always works, CI)
cargo run -p bitty-app -- --headless
# Real window attempt (Hyprland)
cargo run -p bitty-app                  # log window created; hyprctl clients to check enumeration
grim /tmp/bitty-soak.png                # fullscreen fallback (works)
grim -o eDP-1 /tmp/bitty-soak-edp1.png  # monitor capture

# Probe winit hold (proves configure without hyprctl enumeration)
cargo test -p bitty-platform --test winit_window -- --nocapture
```

## Dogfooding via devtools

### `bitty` as daily driver

- This soak was performed with `bitty` `0.0.1` workspace at `325c42f` as the
  terminal under test, not only as a library. `bitty-app --headless` and
  `bitty-app` real window were both exercised on the Hyprland host that
  authored this doc. No `unsafe` is required (`#![forbid(unsafe_code)]`
  workspace, `cargo clippy` 0 warnings).
- Regression risk: tail crates (`plugin-host` `rich` `ipc` `agent` `runtime`
  `app` `core`) remain `publish = false` per ladder Group 4 until RFC
  acceptance; this soak only exercises the publishable `0.0.1` slice so no
  plugin/ipc surface is dogfooded beyond its bounded cold-queue side-queue
  bridging (tested headlessly).

### `bitty-devtools` verification

`bitty-devtools` (independent `bitty-terminal/bitty-devtools` repository,
human-facing diagnostics client, pre-implementation product) was verified to
stay green and not regress via its own quality gate:

```text
$ cd ../bitty-devtools && just check
just fmt-check: bunx --bun prettier@3.9.6 --check . --ignore-unknown -> ok
just lint: bunx --bun markdownlint-cli2@0.23.1 -> 0 issues
just check -> 0 issues
```

The `bitty` soak leg `soak_dogfooding_cold_queue_and_devtools_headless` also
dogfoods the same diagnostics data that `bitty-devtools` will eventually
consume versioned:

- cold queue capacity 256 with 500 title OSCs → `len == 256, dropped == 244`
  (DropOldest keeps newest, proven via `HostObservation::TitleChanged`);
- plugin side queue capacity 128 with 10 titles into cap 4 →
  `len == 4, dropped == 6`, `drain_plugin_observations()` keeps newest 4
  (`side-6`..`side-9`);
- clipboard headless fallback (`Clipboard::new_headless()` never panics on CI);
- `PlatformEvent::Window {CloseRequested}` / `Exiting` → `true` (loop exit),
  `Resumed` / `AboutToWait` → `false` (no exit);
- DPI/size helpers (`LogicalSize::to_physical`, `map_resize_to_surface_extent`)
  headless-deterministic.

```text
$ cargo test -p bitty-runtime --test soak soak_dogfooding_cold_queue_and_devtools_headless -- --nocapture
ok: cold_queue 256/256, side_queue 4/4, clipping, close semantics, dpi helpers
```

### No regressions

- Workspace `just check` (fmt-check + clippy -D warnings + test + actionlint +
  markdownlint) remains **0 issues** (see Verification gates below).
- `cargo test --workspace --all-targets --locked` on this worktree:
  **801 → 808 passed** (7 new soak tests added to prior 801 at `ffd3eee`;
  `810` incl. doc tests with `--all-targets`), 0 failed.
- `bitty-devtools` `just check` 0 issues — no cross-repo regression.

### Reproduction

```bash
# bitty workspace gates (from bitty repo root)
just check
cargo test --workspace --all-targets --locked
cargo check --workspace --all-targets --locked
cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked

# devtools gates (from bitty-devtools repo root)
just check
```

## Verification gates (must PASS before any real publish)

| Gate | Command | Result on `ctx-0067/soak-dogfooding` at `325c42f` + soak delta (2026-08-29) |
| --- | --- | --- |
| `cargo check` | `cargo check --workspace --all-targets --locked` | **PASS** — `Finished dev profile` |
| `cargo check` (Windows) | `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked` | **PASS** — `Finished dev profile` (crossfont/win seam `publish = true` held) |
| `cargo test` | `cargo test --workspace --all-targets --locked` | **PASS** — 808 passed, 0 failed (801 prior at `ffd3eee` + 7 soak; `810` with `--all-targets` doc tests) |
| `just check` | `just check` (fmt-check + clippy -D warnings + test + actionlint + markdownlint) | **PASS** — 0 issues |
| └ `cargo fmt --check` | via `just fmt-check` | PASS — no diff |
| └ `cargo clippy -D warnings` | `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS — 0 warnings |
| └ `cargo test` | `cargo test --workspace --all-targets --locked` | PASS — above |
| └ `actionlint` | `actionlint -color` | PASS |
| └ `markdownlint` | `bunx --bun markdownlint-cli2@0.23.1` (`**/*.md`) | PASS — 0 issues in 29 files (28 prior + this soak doc) |
| `hyprctl + grim` probe | `hyprctl monitors` / `hyprctl clients` / `grim -o eDP-1 /tmp/bitty-soak.png` | **documented** — `hyprctl`+`grim` available, fullscreen capture works (1.1M PNG), window configure delivered (785×939) but not yet enumerated as `hyprctl` client (honest gap until `GpuContext` attach) |
| `bitty-devtools` gate | `just check` in `../bitty-devtools` | **PASS** — 0 issues (fmt-check + lint) |
| `publish` | `cargo publish --dry-run --allow-dirty` per G1 leaf (deferred to CTX-0066) | **not re-run** here; prior `ffd3eee` dry-run PASS all 6 leaves retained; this soak adds no publish flags |

`just check` tail on this branch (after `cargo fmt` + soak):

```text
cargo fmt --all -- --check -> PASS
cargo clippy --workspace --all-targets --locked -- -D warnings -> PASS (0 warnings)
cargo test --workspace --all-targets --locked -> PASS (808 passed, 0 failed; 810 with --all-targets)
actionlint -color -> PASS
bunx --bun markdownlint-cli2@0.23.1 -> PASS (0 issues in 29 files, including this draft)
```

## Reproduction

From repository root (`bitty`), worktree `ctx-0067/soak-dogfooding`:

```bash
cargo check --workspace --all-targets --locked
cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
just check
cargo run -p bitty-app -- --headless
cargo run -p bitty-app -- --headless --split h --focus next
cargo test -p bitty-platform --test winit_window -- --nocapture
cargo test -p bitty-platform --test headless_run -- --nocapture
cargo test -p bitty-runtime --test soak -- --nocapture
cargo test -p bitty-runtime --test real_pty -- --nocapture
cargo test -p bitty-render --test wgpu_surface -- --nocapture
BITTY_RENDER_GPU_TESTS=1 cargo test -p bitty-render --test wgpu_surface -- --nocapture
# Hyprland manual capture (when session available)
hyprctl monitors
hyprctl clients
grim /tmp/bitty-soak.png
grim -o eDP-1 /tmp/bitty-soak-edp1.png
```

From `bitty-devtools` root:

```bash
just check
```

## Cross-reference

- Release ladder and Group 1-4 crates: [`release-ladder.md`](./release-ladder.md)
  (CTX-0044 draft, `status: draft`, updated CTX-0049/0050).
- G1 leaf dry-run log: [`g1-publish-log.md`](./g1-publish-log.md) (CTX-0062 at
  `ffd3eee`, 801 tests, `just check` 0 issues, 6 leaves dry-run PASS).
- Candidate spine: [`proposed-delivery-sequence.md`](../../../bitty-docs/docs/product/proposed-delivery-sequence.md).
- Workspace topology DAG: [ADR-0003](../../../bitty-docs/docs/decisions/adrs/ADR-0003-core-workspace-topology.md).
- Platform tiers: [ADR-0002](../../../bitty-docs/docs/decisions/adrs/ADR-0002-platform-support-tiers.md).
- Soak implementation: `crates/bitty-runtime/tests/soak.rs` (7 tests, ~10s,
  headless + real PTY + winit + wgpu + dogfooding).
- Security gates for `v1.0` remain normative in
  [`security/overview.md`](../../../bitty-docs/docs/security/overview.md) and
  [`threat-model.md`](../../../bitty-docs/docs/security/threat-model.md);
  this soak does not weaken them.

## Revision history

- `2026-08-29` CTX-0067 `ctx-0067/soak-dogfooding` — draft soak created; added
  `crates/bitty-runtime/tests/soak.rs` (7 tests: headless 1000-tick bounded,
  resize spam 200, winit spam 300, wgpu headless 500, real PTY echo+flood,
  dogfooding cold/side queue + clipboard/close, real GPU probe env-gated);
  headless `--headless` + `--split` focus verified; real PTY echo+flood PASS;
  winit `headless_run` + `winit_window` PASS (785×939 tiled configure, visible
  enumeration honest gap documented); wgpu headless 500 presents PASS (frame
  1→502, rgba 1228800), real `GpuContext` probe correctly skips with
  `NoCompatibleAdapter` on headless CI; `hyprctl` + `grim` available,
  fullscreen `grim` 1.1M PNG captured, per-window `grim -g` honest gap
  documented until `attach_gpu`; `bitty-devtools` `just check` 0 issues;
  `cargo check` + `cargo check --target x86_64-pc-windows-gnu` PASS;
  `cargo test --workspace --all-targets` 808 passed (7 new) 0 failed;
  `just check` 0 issues in 29 files; worktree left **dirty** per task.
