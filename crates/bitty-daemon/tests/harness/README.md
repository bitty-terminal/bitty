# bittyd conformance harness

The shared corpus runner for bitty#2605. `corpus.json` is the single
machine-readable vector set; both drivers parse it through `corpus.rs` and
never fork expectations.

## Files

| File          | Role                                                               |
| ------------- | ------------------------------------------------------------------ |
| `corpus.json` | The frozen vector set (schema `bittyd-conformance-corpus/1`)       |
| `corpus.rs`   | Strict parser, typed model, path normalization                     |
| `runner.rs`   | Driver trait, expectation engine, retry and capture rules          |
| `fake.rs`     | In-process reference server, always labeled `candidate-not-bittyd` |
| `real.rs`     | Built `bittyd` over a throwaway Unix runtime root (Linux only)     |
| `evidence.rs` | Per-cell JSON evidence and the parity document                     |

## Corpus shape

```json
{
  "schema": "bittyd-conformance-corpus/1",
  "contract": "bittyd-session-host-contract (accepted 2026-10-27)",
  "frozen": { "session.attach.request_role": "attach_role" },
  "vectors": [
    {
      "id": "V-01",
      "title": "…",
      "area": "handshake",
      "decisions": ["D-08"],
      "drivers": ["fake", "real"],
      "requires": ["terminal_spawn"],
      "setup": { "host_grant": false },
      "injected": { "clock_ms": 0, "limits": {} },
      "steps": [],
      "state": [],
      "forbidden": ["…"],
      "real_skip": "requires-injected-clock",
      "fake_skip": "adapter-owned",
      "deferred": "bitty#2604",
      "known_defect": {
        "issue": "bitty#2768",
        "reason": "…",
        "drivers": ["real"],
        "step": 6,
        "code": "NotFound"
      }
    }
  ]
}
```

- `drivers` names the drivers that must run the vector. A driver that does not
  run it must have a `real_skip`/`fake_skip` reason; the row is recorded as a
  typed skip, never a pass.
- `requires` names capabilities or modeled features the served cell must
  advertise or provide for the vector to run. A driver whose negotiated
  capability set lacks one records the typed `cell-not-servable: <requirement>`
  skip after preparation. `worker_sessions` is `false` in every v0.2.0 build
  (`D-01`), so the reserved worker vectors skip on both drivers; the accepted
  1.3 terminal-spawn vectors require the modeled `terminal_spawn` feature, which
  is `true` only when the containment plan can contain a child. The reference
  fake models both the reserved worker bit (`false`) and the served terminal
  plan (`terminal_spawn: true`). A harness seam (`request_deadline_seam`) is
  resolved through the driver's `seams` map, never the wire handshake.
- `injected` is honored by the fake. The real service uses its process clock and
  compiled limits; a vector that needs injection records a typed real skip. The
  request-deadline vector is the exception: `handler_delay_ms` and
  `handler_delay_method` plus a `request_timeout_ms` limit are honored by the
  real driver through the `test-seams` feature, which `just daemon-conformance`
  enables. Without the feature the real row records
  `cell-not-servable: request_deadline_seam`.
- `known_defect.drivers` scopes the marker to the driver that still carries the
  defect. `step` is the zero-based index (across `steps` then `state`) that must
  fail, and `code`/`path` names the expected refusal or result mismatch. A
  marked row is `known-defect` only when the failure matches that location; a
  pass, a failure at another step, or a different code is a failure, so a
  regression cannot hide behind the marker. Known-defect rows are excluded from
  parity.
- `forbidden` is required and non-empty: it names the observations the vector
  exists to rule out. A step without a typed expectation is rejected, so a
  vector cannot assert only "no crash".

## Steps

A step has exactly one of `method` (with `connection` and `params`) or
`action` (with `args`). `capture` maps a variable name to a dotted path in the
response `result`; `@name` in `params`, expectations, and audit needles resolves
to the captured JSON value, and `{{name}}` substitutes inside a string.

```json
{
  "connection": "creator",
  "method": "session.create",
  "params": { "kind": "terminal", "idempotency_key": "v01-create" },
  "capture": { "session_id": "session.session_id" },
  "expect": {
    "ok": true,
    "result": { "session.state": "created" },
    "present": ["control_token"],
    "absent": ["replayed"],
    "array_len": { "limits": 54 },
    "hex_len": { "session_id": 32 },
    "not_equal": { "control_token": "@create_token" },
    "retry_ms": 2000
  }
}
```

Expectation vocabulary: `ok`, `error_class`, `error_code`, `counts`,
`error_fields` (extra error-object fields by exact value), `error_present`
(extra error-object fields that must be present and non-null),
`closed` (EOF after the reply), `transport` (`"closed"` when no reply arrives),
`result`, `result_min`, `present`, `absent`, `array` (exact), `array_len`,
`min_array_len`, `hex_len`, `distinct`, `not_equal`, `not_equal_any`,
`request_log`,
`exit_code`, `stdout_contains`, `file_mode`, and `audit`. Paths are relative to
the response `result`; a leading `result.` is accepted and normalized.

Actions: `restart`, `crash`, `advance_clock`, `oversized_frame`, `partial_frame`,
`queue_pressure`, `audit`, `file_mode`, `no_tcp_listener`, `child_processes`,
`daemon_absent`, `request_log`, `read_events`, `peer_exit`, and
`refusal_burst`. `child_processes` asserts `expect.result.count` against the
service's live direct-child count; a failed process-table read fails the row
rather than reading as zero. The real driver reads the daemon's per-task
`children` tables and
`harness::real::tests::a_live_child_is_observable_so_a_zero_child_row_cannot_be_vacuous`
spawns a short-lived child and asserts the reader sees its pid, so a zero count
is a measurement rather than a constant. The fake has no process plane; it
derives the count from its own model instead of returning zero — one child per
bound, non-terminal session
(`harness::fake::tests::the_fake_child_count_follows_its_own_session_state`), so
a refused create that never binds reads zero because the fake models the
absence, not because the step cannot observe anything. `read_events` reads `args.count` pushed frames;
`args.events` is an optional per-frame array of expectation objects
(index-aligned with the frames), and `expect.result` is the fallback applied to
every frame. `retry_ms` is the only bounded settlement: it re-sends the same
request until the typed expectation holds or the deadline passes, for an effect
the service applies on another connection.

## Evidence

`target/conformance/<cell>.<suite>.json` (`$CARGO_TARGET_DIR` wins) records the
suite (`fake`, `real`, `parity`), label, cell, version, commit, corpus schema,
the corpus `frozen` map, instance id, capabilities, modeled features, effective
limits, and one row per vector with status, skip reason, deferred requirement,
known-defect issue, instance id, step count, typed signature, failure step, and
failure. The record contains no host path, username, socket path, port, token,
or capability value. `features` carries driver-modeled service facts that are
never advertised on the wire (for example `terminal_spawn`, the
containment-plan gate of the accepted 1.3 terminal-spawn slice); `capabilities`
stays the exact handshake set.

`<cell>.parity.json` lists the shared vectors compared and the divergences. A
fake run never closes a real row: the files are separate and the parity report
names only the shared ids it compared.
