# Bell and notification policy (implemented slice, CTX-0577/CTX-1008)

> Status: **implemented only for what this repository proves** (CTX-0577,
> CTX-1008 for issue #1763, `bitty`). The canonical user-visible policy
> decision belongs to
> [OQ-076](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/open-questions.md),
> which is owner-pending; this document records the bounded behavior that is
> implemented today and marks every owner-pending surface as such. It is not
> an acceptance decision and does not close OQ-076.

## Why

Untrusted PTY output can emit `BEL`, `OSC 9`, `OSC 777`, and Kitty `OSC 99`
at child-output rate. Without a policy those bytes could drive an unbounded,
user-visible surface or an unbounded queue. The accepted security corpus already requires
"notification rate" and "titles, notifications, cwd … bounded, untrusted
strings" (`bitty-docs/docs/security/threat-model.md`), and the accepted
[Isolation Resource RFC](https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/runtime/isolation-resource-rfc.md)
fixes `RC-8` (notification/title/metadata rate, 10 events/s coalesced). This
slice implements exactly that budget and a default-deny surface.

## Scope

- `BEL` (C0 `0x07`) user-visible handling.
- `OSC 9;<message>`, `OSC 777;notify;<title>;<body>`, and Kitty
  `OSC 99;metadata;payload` notifications (CTX-1008).
  The ConEmu `OSC 9;<n>[;...]` sub-commands (`1` sleep, `2` message box,
  `3` tab title, `4` progress, `5`..=`9`) share the OSC 9 code but are not
  notifications and stay inert. Kitty `p=?` capability queries,
  `p=close`/`icon`/`alive`/`buttons`, and malformed metadata stay inert;
  `p=title` (default) and `p=body` assemble by `i=` with `d=0` buffering and
  `d=1` (default) completing, `e=1` base64-decoded fail-closed.
- Capability/consent gating, rate limiting, and the bounded presentation
  surface for all.

## Policy

| Surface                        | Default                               | Gate                                                   | Bound                                                              |
| ------------------------------ | ------------------------------------- | ------------------------------------------------------ | ------------------------------------------------------------------ |
| `BEL` visible                  | **visual flash** (`BellMode::Visual`) | embedder `Runtime::set_bell_mode`                      | flash expires after `BELL_FLASH_DURATION` (120 ms); RC-8 limited   |
| `BEL` audible                  | **off**                               | `BellMode::Audible` / `Both`                           | sink-gated (`BellSink`); no sink means counted-only, never sounded |
| `OSC 9` / `OSC 777` / `OSC 99` | **deny**                              | embedder `Runtime::set_osc_notification_allowed(true)` | RC-8 limited; bounded queue (`NOTIFICATION_QUEUE_CAPACITY` = 8)    |
| Visible notification banner    | at most **one at a time**             | consent (above)                                        | text sanitized + truncated to `NOTIFICATION_TEXT_MAX_CHARS` (256)  |

Rules:

1. **Default-deny.** `OSC 9` / `OSC 777` / `OSC 99` are dropped and counted
   (`notifications_denied`) unless the embedder grants consent. Denied
   requests are never queued and never shown; denied Kitty `d=0` chunks never
   buffer partials.
2. **Rate limited.** All surfaces share one `RC-8` fixed-window limiter
   (10 admissions/s). Excess events are dropped and counted
   (`bell_rate_dropped`); nothing queues without bound. A backwards clock
   cannot re-open a window early. Admitted notifications additionally pass
   the bridge defensive cap (`bitty-platform-services`, same ceiling);
   over-ceiling bridge calls drop fail-closed and count in
   `notifications_bridge_dropped`.
3. **Bounded presentation.** The visual flash is a short, self-expiring
   accent strip (`BELL_FLASH_DURATION`, 120 ms); the notification banner
   shows one notification at a time and self-expires after
   `NOTIFICATION_BANNER_DURATION` (4 s). Expiry runs in the tick's time
   gates and the app arms a wake at `bell_notification_deadline()`, so both
   clear on time even on a quiet window with no PTY output or layout change.
   Neither mutates grid truth or the layout.
4. **No ambient authority.** Notification payloads are untrusted observation
   data: control characters are stripped, the text is length-bounded, and it
   is never expanded, executed, or interpreted as a path or command. No shell
   is constructed or interpolated anywhere in the delivery path (fixed argv
   or native API only).
5. **Plugin observation unchanged.** The bounded plugin-visible
   `HostObservation::Bell` / `terminal.bell` bridge is independent of the
   display policy; silencing the display never silences plugins.

## Owner-pending surfaces

- **Audible bell:** `BellMode::Audible`/`Both` ring the installed
  `BellSink` (`OsBellSink` writes one `BEL` byte to stderr for real runs);
  richer audio synthesis awaits OQ-076.
- **Desktop notification form:** consented notifications render as an
  in-window banner plus best-effort OS delivery through the installed
  `NotificationSink` (`OsNotificationSink`: Linux `notify-send`, macOS
  `osascript`, both fixed argv, no shell). The full OS consent UX and
  native D-Bus/notification-center/WinRT wiring await OQ-076; the
  `bitty-platform-services` bridge provides the defensive second cap.
- **Composition with plugin notifications:** plugin `platform.notify`
  notifications flow through the plugin runtime's own bounded queue
  (`plugin_runtime::NOTIFICATION_QUEUE_CAPACITY`); their user-visible
  delivery and how they compose with terminal-originated notifications are
  OQ-076.

## Implemented code and evidence

- Parser classification: `crates/bitty-vt/src/parser/dispatch.rs`
  (`parse_osc9_notification` — bare text only, ConEmu `OSC 9;<n>` denied;
  `parse_osc777_notification`; `parse_osc99_chunk` with `i=`/`d=`/`p=`/`e=`
  plus fail-closed base64) plus `TerminalAction::OscNotification` and
  `TerminalAction::KittyNotificationChunk` in `crates/bitty-vt/src/action.rs`.
- Policy and bounds: `crates/bitty-runtime/src/runtime/bell.rs` (RC-8 limiter,
  bounded queue, `KittyNotificationAssembler` with `KITTY_PARTIALS_CAPACITY`
  groups and `KITTY_ASSEMBLED_MAX_CHARS` sides).
- Bridge consumption: `crates/bitty-runtime/Cargo.toml`
  (`bitty-platform-services` exact-rev pin, same pattern as
  `bitty-network-wire`) and `crates/bitty-runtime/src/runtime.rs`
  (defensive `NotificationBridge` gate before queueing).
- Wiring: `crates/bitty-runtime/src/runtime/pty.rs` (admission) and
  `crates/bitty-runtime/src/runtime/present.rs` (bounded overlays).
- Tests: `crates/bitty-vt/src/parser/tests.rs` (form classification and
  malformed fail-closed, including Kitty title/body/base64/query/inert) and
  `crates/bitty-runtime/tests/m1_bell_notification.rs` (default-deny, RC-8
  rate limit, bounded queue, sanitization, expiry) plus
  `crates/bitty-runtime/tests/m1_kitty_notification.rs` (Kitty consent,
  chunked assembly, base64, rapid-output RC-8, mixed-budget sharing,
  hostile sanitization) and
  `crates/bitty-runtime/tests/m1_bell_os_delivery.rs` (sink seams).

Terminal state treats the notification actions as inert by contract, so the
replay/canonical hash is unchanged (`crates/bitty-term-state/src/state.rs`).

## Open items

- Accepting OQ-076 moves the audible and OS-notification surfaces from
  owner-pending to a scoped implementation task; this document is revised
  then, and the canonical policy is recorded in `bitty-terminal-docs`.
- The `BEL` flash accent color and the notification banner styling are
  presentation choices local to this slice and may change with the accepted
  theme contract.
