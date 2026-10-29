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
      "requires": ["worker_sessions"],
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
- `requires` names capabilities the served cell must advertise for the vector
  to run. A driver whose negotiated capability set lacks one records the typed
  `cell-not-servable: <capability>` skip after preparation, so a real Linux cell
  whose containment probe selected no mechanism never fails the worker vectors
  it cannot serve. The reference fake models a served worker cell and always
  advertises `worker_sessions`.
- `injected` is honored by the fake. The real service uses its process clock and
  compiled limits; a vector that needs injection records a typed real skip.
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
`closed` (EOF after the reply), `transport` (`"closed"` when no reply arrives),
`result`, `result_min`, `present`, `absent`, `array` (exact), `array_len`,
`min_array_len`, `hex_len`, `distinct`, `not_equal`, `not_equal_any`,
`request_log`,
`exit_code`, `stdout_contains`, `file_mode`, and `audit`. Paths are relative to
the response `result`; a leading `result.` is accepted and normalized.

Actions: `restart`, `crash`, `advance_clock`, `oversized_frame`, `partial_frame`,
`queue_pressure`, `audit`, `file_mode`, `no_tcp_listener`, `daemon_absent`,
`request_log`, `read_events`, `peer_exit`, and `refusal_burst`. `read_events`
reads `args.count` pushed frames; `args.events` is an optional per-frame array
of expectation objects (index-aligned with the frames), and `expect.result` is
the fallback applied to every frame. `retry_ms` is the only bounded
settlement: it re-sends the same request until the typed expectation holds or
the deadline passes, for an effect the service applies on another connection.

## Evidence

`target/conformance/<cell>.<suite>.json` (`$CARGO_TARGET_DIR` wins) records the
suite (`fake`, `real`, `parity`), label, cell, version, commit, corpus schema,
the corpus `frozen` map, instance id, capabilities, effective limits, and one
row per vector with status, skip reason, deferred requirement, known-defect
issue, instance id, step count, typed signature, failure step, and failure. The record contains no
host path, username, socket path, port, token, or capability value.

`<cell>.parity.json` lists the shared vectors compared and the divergences. A
fake run never closes a real row: the files are separate and the parity report
names only the shared ids it compared.
