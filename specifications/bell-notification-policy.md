# Bell and notification policy (implemented slice, CTX-0577)

> Status: **implemented only for what this repository proves** (CTX-0577,
> `bitty`). The canonical user-visible policy decision belongs to
> [OQ-076](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/open-questions.md),
> which is owner-pending; this document records the bounded behavior that is
> implemented today and marks every owner-pending surface as such. It is not
> an acceptance decision and does not close OQ-076.

## Why

Untrusted PTY output can emit `BEL`, `OSC 9`, and `OSC 777` at child-output
rate. Without a policy those bytes could drive an unbounded, user-visible
surface or an unbounded queue. The accepted security corpus already requires
"notification rate" and "titles, notifications, cwd … bounded, untrusted
strings" (`bitty-docs/docs/security/threat-model.md`), and the accepted
[Isolation Resource RFC](https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/runtime/isolation-resource-rfc.md)
fixes `RC-8` (notification/title/metadata rate, 10 events/s coalesced). This
slice implements exactly that budget and a default-deny surface.

## Scope

- `BEL` (C0 `0x07`) user-visible handling.
- `OSC 9;<message>` and `OSC 777;notify;<title>;<body>` notifications.
- Capability/consent gating, rate limiting, and the bounded presentation
  surface for both.

## Policy

| Surface                     | Default                               | Gate                                                   | Bound                                                             |
| --------------------------- | ------------------------------------- | ------------------------------------------------------ | ----------------------------------------------------------------- |
| `BEL` visible               | **visual flash** (`BellMode::Visual`) | embedder `Runtime::set_bell_mode`                      | flash expires after `BELL_FLASH_DURATION` (120 ms); RC-8 limited  |
| `BEL` audible               | **off**                               | `BellMode::Audible` / `Both`                           | owner-pending: no OS primitive is wired; requests only counted    |
| `OSC 9` / `OSC 777`         | **deny**                              | embedder `Runtime::set_osc_notification_allowed(true)` | RC-8 limited; bounded queue (`NOTIFICATION_QUEUE_CAPACITY` = 8)   |
| Visible notification banner | at most **one at a time**             | consent (above)                                        | text sanitized + truncated to `NOTIFICATION_TEXT_MAX_CHARS` (256) |

Rules:

1. **Default-deny.** `OSC 9` / `OSC 777` are dropped and counted
   (`notifications_denied`) unless the embedder grants consent. Denied
   requests are never queued and never shown.
2. **Rate limited.** Both surfaces share one `RC-8` fixed-window limiter
   (10 admissions/s). Excess events are dropped and counted
   (`bell_rate_dropped`); nothing queues without bound. A backwards clock
   cannot re-open a window early.
3. **Bounded presentation.** The visual flash is a short, self-expiring
   accent strip; the notification banner shows one notification at a time
   and self-expires after `NOTIFICATION_BANNER_DURATION` (4 s). Neither
   mutates grid truth or the layout.
4. **No ambient authority.** Notification payloads are untrusted observation
   data: control characters are stripped, the text is length-bounded, and it
   is never expanded, executed, or interpreted as a path or command.
5. **Plugin observation unchanged.** The bounded plugin-visible
   `HostObservation::Bell` / `terminal.bell` bridge is independent of the
   display policy; silencing the display never silences plugins.

## Owner-pending surfaces

- **Audible bell:** a real audible sink needs an OS/audio primitive that does
  not exist in `bitty-platform` yet. `BellMode::Audible` records the request
  but produces no sound; the choice of primitive and the fallback chain is
  OQ-076.
- **Desktop notification form:** no platform notification primitive is wired
  yet, so a consented notification renders as an in-window banner, not an OS
  notification. The OS handoff and its consent UX are OQ-076.
- **Composition with plugin notifications:** plugin `platform.notify`
  notifications flow through the plugin runtime's own bounded queue
  (`plugin_runtime::NOTIFICATION_QUEUE_CAPACITY`); their user-visible
  delivery and how they compose with terminal-originated notifications are
  OQ-076.

## Implemented code and evidence

- Parser classification: `crates/bitty-vt/src/parser/dispatch.rs`
  (`parse_osc9_notification`, `parse_osc777_notification`) plus
  `TerminalAction::OscNotification` in `crates/bitty-vt/src/action.rs`.
- Policy and bounds: `crates/bitty-runtime/src/runtime/bell.rs`.
- Wiring: `crates/bitty-runtime/src/runtime/pty.rs` (admission) and
  `crates/bitty-runtime/src/runtime/present.rs` (bounded overlays).
- Tests: `crates/bitty-vt/src/parser/tests.rs` (form classification and
  malformed fail-closed) and
  `crates/bitty-runtime/tests/m1_bell_notification.rs` (default-deny, RC-8
  rate limit, bounded queue, sanitization, expiry).

Terminal state treats the notification action as inert by contract, so the
replay/canonical hash is unchanged (`crates/bitty-term-state/src/state.rs`).

## Open items

- Accepting OQ-076 moves the audible and OS-notification surfaces from
  owner-pending to a scoped implementation task; this document is revised
  then, and the canonical policy is recorded in `bitty-terminal-docs`.
- The `BEL` flash accent color and the notification banner styling are
  presentation choices local to this slice and may change with the accepted
  theme contract.
