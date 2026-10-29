# `bitty-daemon`

> Part of the `bitty` workspace. Canonical platform documentation lives in
> `bitty-terminal-docs` (mounted at `docs/`) and shared governance in
> `bitty-docs`; this file is a crate-local map, not a canonical contract.

## Purpose

`bitty-daemon` is the crate seam for `bittyd`, the per-user, single-instance
session host admitted to the v0.2.0 slice by
[ADR 0019](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/adrs/ADR-0019-bittyd-staging-advance.md)
and bounded by
[ADR 0008](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/adrs/ADR-0008-headless.md).
The normative service surface is the accepted
[`bittyd` Session-Host Contract](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/bittyd-session-host-contract.md)
(contract tracked by bitty#2546/#2663); this crate seam is bitty#2606
(CTX-1616).

## Status: service loop, terminal sessions, content plane, scopes, and grants

This crate fixes the service version, the capability set, and the version
negotiation, implements the registry half of bitty#2601 (server-generated ids,
the in-memory session registry, generation and revision counters, retention,
and the bounded orphan journal), the event stream of bitty#2792 (snapshot plus
bounded backlog, revision ordering, per-lease routing, control-frame
reservation, the terminal-event guarantee, and heartbeat/last-seen), the
compiled limits and advertised triples of bitty#2603, the two bounded
idempotency stores, the bounded content-free audit ring, the attach/control
half of bitty#2609, and the bitty#2607 service surface: the `session.*` /
`daemon.*` scope registry, `daemon.negotiate`, the eleven-stage authorization
order, host-consent and observer grants, the owner-private audit sink, the
`bittyd start` admission loop, per-frame `SCM_CREDENTIALS`, and the peer
re-check at every privileged boundary.

bitty#2790 adds the process half and the terminal content plane. A
`session.create` with `kind: worker`, or `kind: terminal` plus a bounded spawn
specification, parses that specification, reserves the two-phase journal
record, spawns through the selected containment plan into the per-session
identity, commits the observed identity, and binds generation 1 before the
session is exposed as running. A terminal create without a spawn specification
keeps the `1.2` registry-only shape (`created`, generation 0, no content
plane) for metadata-only clients. Both kinds share one spawn implementation;
the kind decides only whether the bound PTY owns a terminal content plane.
`session.resize`, `session.read`, `session.write`, and `session.subscribe` are
served for a bound terminal under the negotiate-time terminal scopes and the
current controller lease. The spawn path is Linux-only, and `worker_sessions`
is compiled from the cell truth and narrowed to the containment plan the
startup probe selected, so a daemon can never advertise a path it does not
serve.

bitty#2792 adds the delivered event wire: an opted-in connection receives
per-lease event frames on the negotiated socket, drained from the bounded
queues and written after the response they follow. The terminal content plane
adds `session_content` frames on the same wire for a subscribed controller;
content is shed-free by leaving it in the bounded session buffer when the
connection cannot take a whole frame. See
[Pushed event wire](#pushed-event-wire-bitty2792) and
[Terminal content plane](#terminal-content-plane-bitty2790).

bitty#2805 adds the post-handshake request deadline: a request that completes
past the effective `request_timeout_ms` is answered with the typed `Timeout`
class carrying its envelope id and the bound, and the connection keeps serving.
The long-running synchronous handlers (`session.create`, `daemon.orphans.verify`)
keep their own advertised timers and are documented as exempt; see
[Request deadline](#request-deadline-bitty2805).

bitty#2806 serves the session and daemon metadata fields the accepted contract
names but `1.1` left unserved: controller presence and the bounded pending
observer-grant count on every session object, the owner peer (uid and pid only)
in the operator all-session view, effective lease scopes and the server clock on
`session.attach`, the server clock on `lease.heartbeat`, and `daemon_pid`,
uptime, and every orphan class in `daemon.status`. The additions are optional
response keys in service `1.2`, so no existing field changes meaning and `1.0`
through `1.2` clients stay accepted. See
[Served metadata fields](#served-metadata-fields-bitty2806).

`bittyd start` binds the per-user endpoint (`0700` directory, `0600` socket,
single-instance lock), mints the per-boot operator capability, opens the
owner-private orphan journal, runs the startup containment scan, accepts up to
`max_connections_per_endpoint` connections, serves the bitty-ipc v1 envelope,
and stops on `daemon.stop` or a `SIGTERM`/`SIGINT` drain. Plain `bittyd`
creates no runtime file, binds no endpoint, and prints the status, including
the selected containment plan. The daemon opens no TCP listener and signals
only processes inside a recorded containment identity; a worker is spawned
once, drained non-blockingly on the poll tick, and reaped through the plan
before its terminal record is written.

The contract areas that are deliberately deferred, and the typed answer the
running service gives today:

| Area                                                                                      | Owner                        | Answer today                                                                                                                                                                       |
| ----------------------------------------------------------------------------------------- | ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Terminal content authority for observers                                                  | terminal contract follow-up  | content is controller-only in this slice: an observer with `terminal.inspect` is refused `Denied/ControllerRequired`; a future revision may define an explicit observer read grant |
| Worker-kind content plane                                                                 | bitty#2790 follow-up         | a worker is a service process: `session.resize` applies to its PTY, while `session.read`/`write`/`subscribe` answer `Denied/NotRunning`                                            |
| Worker rebind and restart across generations                                              | bitty#2790 follow-up         | a worker binds generation 1 exactly once; a terminal record is never revived and no automatic restart exists                                                                       |
| Executable allowlist for worker `argv[0]`                                                 | bitty#2790 / security corpus | any executable reachable under the daemon user is accepted; no allowlist or digest policy is enforced yet                                                                          |
| Executable path digest in the orphan record; native Windows job-object reaper             | bitty#2791 follow-up         | no executable path is recorded; Windows and macOS keep the typed `UnsupportedPlatform` refusal                                                                                     |
| Per-connection event backlog counters in `daemon.status`; a shed-able observability class | bitty#2792 follow-up         | delivery is served on the Linux cell (`events: true`); `daemon.status` serves pid, uptime, and orphan classes (bitty#2806); no backlog counter or shed-able class                  |
| Post-handshake request deadline (`request_timeout_ms` is advertised, not enforced)        | bitty#2805                   | the negotiate deadline is the only `Timeout` emission today; the advertised limit alone changes nothing                                                                            |
| Per-connection refusal-audit aggregation                                                  | bitty#2603 follow-up         | wire-level refusals are budgeted and aggregated (see below); method refusals remain one individual record each                                                                     |
| Conformance corpus and fake-server parity                                                 | bitty#2605                   | `tests/harness` holds the corpus, the reference fake, the real-service driver, and the parity gate; every shared vector is compared, with no recorded known defect                 |

The post-handshake request deadline is **not** in this table: it is served by
the loop (see [Request deadline](#request-deadline-bitty2805)). Nothing here
authorizes autostart, a service file, a TCP listener, or desktop wiring; all
four stay out of scope.

## Frozen contract values

Source: contract decisions `D-01`, `D-08`, `D-11`, and the "Handshake" section
of the service surface.

| Item                 | Value                                                                                                                                                                         | Where                                                       |
| -------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------- |
| Service API version  | `{major: 1, minor: 3}`                                                                                                                                                        | `contract::SERVICE_API_VERSION`                             |
| Minimum client minor | `0` (`1.0..=1.3` accepted)                                                                                                                                                    | `contract::MIN_CLIENT_MINOR`                                |
| Capabilities         | `terminal_sessions`, `events`, `observer_grants`, `control_transfer`, `idempotent_mutations` enabled; `worker_sessions` compiled for the cell and narrowed to the probed plan | `contract::CAPABILITIES`, `worker::sessions_served`         |
| Handshake method     | `daemon.negotiate`                                                                                                                                                            | `contract::NEGOTIATE_METHOD`                                |
| Negotiation rule     | same major, client minor in `[min_client_minor, server_minor]`; every other case is `VersionMismatch` before any operation                                                    | `handshake::negotiate`                                      |
| Limits advertisement | every `D-14` row plus the inherited `RC-9`/`RC-10` bounds as `{effective, default, system_max}`                                                                               | `handshake::NegotiateResponse`                              |
| Worker sessions      | `true` on a Linux cell whose probe selected an enforced plan, `false` everywhere else; `1.0` clients stay accepted because `min_client_minor` is `0`                          | `worker::WORKER_SESSIONS_SERVED`, `worker::sessions_served` |

The capability bits describe the v1 contract surface this daemon serves. A
non-served cell or a Linux cell whose probe selected no mechanism advertises
`worker_sessions: false` and answers a worker create with
`Denied/UnsupportedCapability`; a served daemon advertises `true` and gates the
same call by `process.spawn`, `session.host` consent, a live host grant, and the
bounded spawn specification. A terminal create that supplies a spawn
specification uses the same gate and answers `Denied/UnsupportedCapability`
where the plan cannot enforce containment; a terminal create without a
specification is the registry-only compatibility shape and needs no containment
plan. `events` is compiled from `contract::EVENTS_SERVED`
and is `true` exactly where the admission loop serves pushed frames (the Linux
Unix-socket cell); every other cell advertises `false` and refuses a negotiate
that asks for delivery with `Denied/UnsupportedCapability`. Do not read any bit
as a claim about a path this README marks deferred.

## Registry core (bitty#2601)

`registry` is the in-memory session registry. It starts no listener, touches no
file, and owns only metadata.

- `DaemonInstanceId` and `SessionId` are 128-bit OS-CSPRNG values encoded as 32
  lowercase hex characters. Only the registry mints ids; a parsed value is an
  echo for lookup, never authority. Entropy failure refuses the operation.
  Every create mints a fresh value, a collision with a live or retained record
  is regenerated, and a collected id is never recycled as a new value.
- `SessionRegistry::create` starts at generation `0` and state revision `0`;
  `bind` increments the generation exactly once (v0.2.0 permits one bind);
  `attach`/`detach` drive the stored attach/detach record count, which the lease
  layer composes into the live `attach_count` for service snapshots
  (`record_lease_mutation` bumps the revision for a transfer that keeps the
  count); `heartbeat` updates last-seen without a revision bump;
  `begin_stopping` and `terminate` apply one revision bump per snapshot-visible
  transition. Observer-grant mint, consume, expiry, and revoke are revision
  bumps owned by the service layer through `note_visible_mutation`: expiry is
  one bump per affected session per sweep, never one per retired record, and a
  sweep that retires nothing bumps nothing. A terminal session has no live
  lease, so the lease layer detaches every lease before it terminates and the
  frozen record reports no attaches. Counters use checked arithmetic and fail
  closed.
- Terminal states are immutable. A terminal record is retained (never live,
  never revived) until `terminal_retention_ms` or
  `max_retained_terminal_records` collects it in deterministic
  `(terminalized_at, session_id)` order.
- A live session's `terminal_id` is unique; a retained terminal record may share
  it. Refusals (`max_sessions`, `session_record_bytes`, duplicate terminal id)
  are whole: no partial record, no counter change.
- Enforced bounds come from the compiled `D-14` table in `limits`; the
  `RegistryLimits` constants and `RegistryLimits::from_limits` cannot drift from
  the advertised values.

## Attach, control, and stop state machine (bitty#2609)

`lease::SessionHost` owns the registry plus the per-session lease table, so an
attach, detach, claim, transfer, or terminal transition is one atomic
`&mut` operation. It holds no connection, queue, socket, file, or process.

- Roles and bounds: one controller lease per session, plus observers up to
  `max_attaches_per_session` (default 8, ceiling 16); the controller is
  additional to that observer cap (`D-09`). Every attach mints a distinct
  128-bit `attach_id`. A new observer at the live cap is shed; existing leases
  are never evicted. The table is hard-bounded to
  `max_attaches_per_session + controller`: an attach checks the live observer
  cap, sweeps the session's expired records (returned as `Attached::expired` for
  the event layer), applies a record backstop, and only then inserts.
  `ControlView::attach_count` and `SessionSnapshot::attach_count` both report
  the live count.
- Control token: 256 bits from the OS CSPRNG, returned once at create, stored
  only as a domain-separated SHA-256 digest, verified in constant time
  (`subtle`), rotated on every claim and transfer, and retired on controller
  revocation and terminal transition. A rotation that repeats the current secret
  is refused as `EntropyUnavailable` rather than accepted as a rotation. The raw
  token is zeroized on drop and its `Debug` prints only
  `ControlToken(<redacted>)`; no snapshot, notice, refusal, or `SessionHost`
  debug output carries it.
- Token delivery (`D-09`): a claim returns the rotated token on the claiming
  connection because that connection becomes the controller. A transfer rotates
  the token for the **target** lease, so `Transferred` carries it only inside a
  `TransferDelivery` addressed to the target attach id; `TransferDelivery::take_for`
  refuses any other attach id and returns the delivery unchanged. The service
  layer routes the secret to the target lease's own queue and must never encode
  it into the requesting connection's response. Nothing else in the requester's
  result contains it.

  | `Transferred` field                                     | Requesting connection | Target lease message                |
  | ------------------------------------------------------- | --------------------- | ----------------------------------- |
  | `session`, `actor`, `previous_controller`, `controller` | response payload      | state/control event                 |
  | `delivery`                                              | **never**             | `take_for(target.attach_id())` only |

- Authorization: the IPC slice authenticates the peer, checks instance, scope,
  visibility, observer grant, and operator capability; `SessionHost` owns the
  lease liveness/role relation, the generation fence, and the token comparison,
  in that order (role/relation, generation, token). A stale generation is
  refused before the token digest is touched. An observer is always
  `ControllerRequired` for control methods and never becomes controller by
  waiting, by disconnect, or by racing an attach — **and never by presenting the
  current token**: a lease-bearing authority is decided by its role alone, and
  the token is only consulted for a request with no lease. `ControlAuthority::operator()`
  is unauthenticated by construction, so only the daemon's own service code may
  build it, behind the bitty#2607 `daemon.control` check.
- Lease reads: `SessionHost::lease` is a lookup that also returns an expired
  record still awaiting the sweep; `authorize_content_read` (role-agnostic) and
  `authorize_controller` (controller-only) are the liveness-checked entry
  points. Both are read-only and never renew; the client heartbeat and
  `record_resize` renew. Status/list use `SessionHost::snapshot`/`snapshots`,
  which compose the live `attach_count` from the lease table; the raw
  `SessionRegistry` snapshot is a storage view and is never served as status.
- Expiry and renewal are driven by the injected `MonotonicMs`; a lease is live
  before `expires_at` and expired exactly at it. Expiry detaches only: the
  worker keeps running, the generation is unchanged, and a controller expiry
  leaves control vacant with the current token still reclaimable. Every
  `LeaseExpiry` carries the `state_revision` of its own detach mutation; when a
  claim or transfer vacates an already-expired controller, that detach revision
  is the operation's final revision minus one because the claim/transfer then
  bumps once more. A later counter fault refuses the operation but leaves the
  due detach in place.
- Stop pipeline: a generation-0 stop commits a cancel intent that refuses
  `bind`; completing it records `not_started`/`swept`/`unverified` cleanup. A
  running stop moves to `stopping`, arms the abort or drain grace, then
  escalates to kill at the deadline. Terminations classify as `worker_exit`,
  `worker_signal`, `spawn_failed`, or the accepted stop reason. The terminal
  transition retires the token, detaches every lease (one visible detach bump
  each, so the frozen record reports no attaches), and returns exactly one
  `TerminalNotice`; a duplicate returns `AlreadyTerminal` and no second notice.
  The caller must reserve a terminal slot for every `attached` lease (including
  an expired-but-unswept one) and enqueue the notice before it releases the
  `&mut SessionHost`, so `session.status` cannot show the terminal snapshot
  first.
- `session.kill` is the operator-abort path and classifies an accepted stop as
  `operator_abort`. `daemon.stop force` must therefore call
  `operator_stop(DaemonStop | HostShutdown)` and then `kill` at the same instant:
  the two-step keeps the daemon-stop reason and moves the deadline to `now`.
- Deliberately not in this module: the observer-grant store, idempotency
  stores, PTY containment/signalling/reaping, event queues, peer
  authentication, scopes, audit files, and endpoint handling. `attach_observer`
  assumes bitty#2607 has already validated a grant; bitty#2607 consumes the
  single-use grant only after the call returns `Ok`, so a refused attach does not
  burn it. Do not expose the observer role without that check.

`orphan` is the two-phase, bounded orphan journal. `reserve` fsyncs a
`reserved` record (session id, generation, boot id, reserved-at, containment
identity) before the spawn; `commit` fills the exact pid, process start time,
process group, and uid after the spawn returns and fsyncs again, before the
worker can be exposed as bound. The journal holds at most
`max_orphan_records` records of at most `orphan_record_bytes`, contains no
argv, environment, cwd, content, or credential, and on Unix lives in a `0700`
directory with `0600` files. `reserve` validates the worst-case committed
encoding (maximum pid, start-time, process-group, and uid widths, the recorded
containment path, plus a later quarantine timestamp), so a reservation never
precedes a spawn unless `commit` can hold any valid identity.

Classification is conservative and never signals an unverified record:

| Observation                                                                 | Outcome                                             |
| --------------------------------------------------------------------------- | --------------------------------------------------- |
| Reserved cgroup record, same boot, pre-created subtree                      | reaches `OrphanReaper`; no pid is invented          |
| Committed cgroup record, same boot, exact or reused pid                     | reaches `OrphanReaper` through the subtree          |
| Exact subreaper record (boot, pid, start time, group, uid)                  | reaches `OrphanReaper`, which still refuses `swept` |
| Different boot id                                                           | `stale`, discarded, never signalled                 |
| Reserved subreaper record with no pid                                       | `unverified`, quarantined, never signalled          |
| Absent pid, unreadable start time, reused start time, group or uid mismatch | `unverified`, quarantined, never signalled          |
| Reaper cannot verify the whole containment identity                         | `unverified`, quarantined                           |

Quarantined records expire after `orphan_record_retention_ms`; operator
retirement removes a quarantined record explicitly and never signals. Boot
comparison precedes quarantine stickiness: a record quarantined under a
previous boot id is stale and removed immediately, without waiting for
retention. `VerifiedOrphan` has a private constructor, so a reaper can only be
asked to terminate a record the journal already classified as actionable.

Windows: the journal compiles and enforces the same record bound, but this slice
does not create a current-user-only DACL and has no boot-id/`/proc` probe.
Until those platform cells exist, opening a journal requires a
caller-supplied boot id and every record classifies as unverified, so nothing is
signalled. The journal inherits the ACL of the daemon-owned runtime directory;
the endpoint slice owns that directory and its current-user-only DACL, and the
Windows job-object reaper remains designed-not-implemented (bitty#2791).

## Limits (bitty#2603)

`limits` is the single compiled source of truth for `D-14` and the inherited
`RC-9`/`RC-10` transport bounds.

- `LimitId::ALL` has 54 entries in contract order: the 46 `D-14` rows, then
  the inherited bounds in prose order: `max_frame_bytes`, `max_chunk_bytes`,
  `max_pending_requests`, `max_channel_capacity`, `max_request_rate_per_sec`,
  `max_request_burst`, `max_connections_per_endpoint`, and
  `max_request_payload_bytes`. Each entry exposes
  `name()`, `default_value()`, `system_max_value()`, `source()`, `floor()`, and
  `stage()`.
- `Limits::default()` uses every compiled default. `Limits::new` applies
  lower-only-or-up-to-maximum overrides; above `system_max`, below the compiled
  floor, a duplicate, or a change to a fixed inherited bound is a typed
  `LimitsError`, and no partial `Limits` is produced. Configuration may raise an
  effective value back up to `system_max`, never above it.
- `Limits::advertised()` yields the `{effective, default, system_max}` triples;
  `handshake::negotiate` returns them in `NegotiateResponse::limits` for the
  run, so a client parameterizes on `effective`, never on a default.
- The byte-bounded identifier limits (`terminal_id_bytes`, `attach_id_bytes`,
  `grant_id_bytes`) have their system maximum as their compiled floor, so a
  lowered configuration cannot advertise a value the registry or wire format
  does not enforce. `TERMINAL_ID_MAX_BYTES` is derived from the same table.
- The ordered stage table in the module docs assigns every limit to a stage.
  The audit-sink availability gate precedes the mutating stages (`D-16`), the
  content cap prunes rather than refusing, retained-record collection is a
  retention stage, and heartbeat/lease/grant/request/idle/grace bounds are
  timer stages that produce timeouts/expiry transitions, not `LimitExceeded`
  refusals.
  A refusal refuses whole before the protected registry, stream, lease, grant,
  or idempotency structure changes; the only write a refusal may perform is its
  own audit record.

## Idempotency (bitty#2603)

`idempotency` implements the two bounded `D-18` stores over one engine.

- `IdempotencyStore::session` is 512 keys / 65,536 accounted bytes at the
  compiled maxima, with a 128-byte descriptor budget per entry.
  `IdempotencyStore::create` is 128 keys / 32,768 bytes, with a 256-byte entry
  budget and the `create_idempotency_ttl_ms` retention floor.
- `begin` returns `Admission::Replay` for the same key, method, target, and
  fingerprint; the executor does not run twice. A duplicate while the original
  call is in flight is `InFlight` and maps to `IdempotencyConflict`. A
  different fingerprint, method, or target is a typed `Conflict`. A lease-bound
  result is replayed only on its originating connection; a create result
  replays across connections. A reservation is bound to its store identity and
  sequence, so it cannot complete an entry in another store or after a later
  re-admission.
- A per-session store is constructed for one `SessionId`; `retire_session`
  refuses a different session and leaves the live store untouched. Store
  wiring/invariant refusals map to `Internal`; capacity maps to
  `IdempotencyCapacity`.
- `mark_control_claimed` flips a completed create descriptor to
  `control_claimed = true`, so a later duplicate returns no token (`D-18`).
  There is deliberately no abandon/cancel: every reservation must complete
  with the real outcome, and an unknown outcome stays in flight and is
  reconciled rather than evicted. C-18's end-to-end termination-under-full-
  store proof needs bitty#2607; the keyed method set structurally excludes
  termination and revocation.
- At capacity the oldest retirable entry is evicted first: a refusal that
  applied nothing, or a create key whose session is terminal and whose retention
  floor passed. In-flight and live applied entries are never evicted; when no
  entry is retirable the store fails closed with `Capacity`. A hostile stream of
  fresh keys stays inside the key and byte bounds.
- `retire_session` releases the per-session store at the terminal transition and
  marks create keys terminal for the TTL floor; `evict_expired` applies that
  floor. Termination and revocation are keyless and never consult either store.
- `IdempotencyKey` has a redacted `Debug` and no `Display`; no refusal carries a
  raw key.

## Audit (bitty#2603)

`audit` owns the typed, content-free record shape and the bounded ring.

- `AuditRecord` carries a stable `AuditEventId`, an injected `MonotonicMs`,
  the daemon instance, the action, the static result, optional session
  counters, optional authenticated actor facts, the action target, optional
  refusal counts, optional terminal classification, and the optional
  suppressed-refusal aggregate (`AuditAggregate`) for a wire budget window.
- There is no `String`, `Vec`, `Box`, payload, argv, environment, clipboard,
  prompt, transcript, raw `terminal_id`, control token, or caller label field. A create
  or terminal transition uses `AuditTarget::terminal_digest`; a refusal carries
  its static `RefusalCode` and numeric `AuditCounts` only, and a suppressed
  refusal is counted in `AuditAggregate` by code, never by frame bytes.
- The ring holds `audit_file_bytes × audit_segments / AUDIT_RECORD_BYTES`
  records (320 accounted bytes each); the accounted ring bytes are therefore
  capped by the sink budget. Overflow drops the oldest record, counts it in
  `AuditCounters::dropped`, and never reuses an event id. `appended`, `dropped`,
  `drained`, `refusals`, and `terminal_transitions` are observable.
- `drain(max)` and `take_until(event_id)` remove bounded, `Copy` records for
  the sink writer; a drain is not an overflow and bumps `drained`, not
  `dropped`. They transfer ownership, so a failed sink write retries the
  returned records. The writer's last-event-id cursor is per
  `daemon_instance_id` and must reset when the instance changes.
- Host-consent grants use the `daemon.host_grant` wire method for mint and the
  audit event labels `host_grant.consume`, `host_grant.revoke`, and
  `host_grant.expire`; every grant action targets `AuditTarget::grant`
  (`grant_id`).
- The ring is the staging buffer; the owner-private rotating file sink and the
  per-connection refusal budget live in `audit_sink` and `service::server`.

`handshake::NegotiateResponse` carries the version, capability, and effective
limit facts. The remaining response fields (`wire_version` (transport-owned),
`daemon_instance_id`, `daemon_pid`, `server_identity`, `peer_identity`,
`client_id`, `scopes`, `server_monotonic_ms`, `daemon_state`) and the optional
request fields (`client_correlation`, `operator_capability`, `host_grant`) are
listed in the module docs and arrive with bitty#2601/#2607; `events` is served
by this slice (see [Pushed event wire](#pushed-event-wire-bitty2792)).
`client_correlation` is bounded to 128 bytes of valid UTF-8 with no control
characters and is recorded only as a truncated 128-bit SHA-256 digest (`D-08`);
`operator_capability` and `host_grant` stay out of this crate until the slice
that owns their secret lifetime and storage exists.

## Pushed event wire (bitty#2792)

A connection opts in with `"events": true` on `daemon.negotiate`. Only then does
the server write event frames on that same connection, for the leases it holds.
Source: contract `D-10`; the frozen envelope is restated in
`bittyd-ipc-contract.md` ("Event schema and capability").

- **Envelope.** `{"v": 1, "id": <server correlation>, "event": {...}}`. `id` is
  chosen by the server per connection (`evt-<counter>`, at most 64 bytes) and is
  never a client value. Event ids occupy the reserved `evt-` namespace; they are
  not request ids, and a client demultiplexes on the `event` key. The event
  object always carries `kind`, `instance_id`, `session_id`, `attach_id`,
  `revision`, and `terminal`; `detail` is present only for `control_transfer`
  (`{from, to}`) and `daemon_stopping` (`{deadline_ms}`), and `terminal_marker`
  only on `session_terminal`. The marker carries `state`, `reason`, and
  `cleanup`; the terminal `exit` information remains a recorded 1.2 narrowing
  (see the contract notes).
- **Ordering.** One event per `state_revision` the session advanced, in revision
  order, per session. The service reports the session it mutated and the last
  emitted revision is the source of truth: a revision the handler did not report
  is filled with a plain `session_state` frame, and a revision is never
  re-emitted or renumbered. The daemon-scoped `daemon_stopping` frame and the
  target-only `lease_revoked` frame carry the current revision without a bump,
  so they can share a revision with a `session_state` frame (for different
  leases in the `lease_revoked` case). Frames are written after the response
  they follow, so an event never preempts a response.
- **Routing.** Every frame is a per-lease copy addressed to the receiving
  connection's own `attach_id`; nothing is broadcast. A revoked lease's final
  `lease_revoked` frame is routed to the connection that owns that lease, not to
  the requester's response.
- **Bounded backlog.** The per-session ring is `max_event_backlog_events` /
  `max_event_backlog_bytes`; the per-lease queue is `max_lease_queue_events` /
  `max_lease_queue_bytes`. A control frame is never shed: when a lease queue
  cannot accept one, the lease is closed with `TerminalEventLost` and its
  connection retires after the current response. The outbound buffer is capped at
  `max_connection_outbound_bytes`; a full buffer stops draining the lease queues
  instead of growing memory, so pressure is absorbed by the bounded queue and
  then reported as the typed close.
- **Terminal guarantee.** The terminal event is emitted before the terminal
  state is readable, pinned until every recipient observes it, and written
  before an idle close shuts the connection down. A client either receives the
  frame or learns it was lost; `session.status`/`session.list` stay the
  reconciliation reads.
- **Attach replay.** An event-opted `session.attach` result carries the optional
  `backlog` (the retained ring, routed to the new lease), `backlog_truncated`,
  and `earliest_revision`. The replay is bounded by the effective
  `max_lease_queue_bytes`, so a lowered limit also lowers the served replay. A
  `1.0` request carries no cursor, so the ring is served whole and the client
  deduplicates by `(session_id, revision, kind)`.
- **Opt-out.** A connection that does not opt in receives no frame of any kind,
  including a lease's final frame, and its attach result carries no replay keys.
- Session metadata events need no subscribe method; they ride the negotiated
  connection. Terminal content is the exception: `session.subscribe` (service
  `1.3`) opts a controller lease in to `session_content` frames on the same
  connection.

## Event stream (bitty#2792)

`events::SessionEventStream` wraps one `SessionRegistry` and owns every
snapshot-visible mutation together with its event, so `state_revision` and the
per-session stream cannot disagree. Source: contract `D-10`, `D-12`, `D-14`,
`D-18`.

- `attach` returns the snapshot at the current revision plus the bounded backlog
  after the requested revision. `backlog_truncated` and `earliest_revision` make
  a dropped history explicit; there is no unbounded replay. A duplicate attach
  key with the same parameters returns the same lease and bumps nothing, and the
  stored key outlives the lease so a retry after detach answers `NotAttached`
  instead of minting a second lease.
- Every visible mutation (bind, attach, detach, lease expiry, control transfer,
  begin-stopping, terminate) appends exactly one event at `previous + 1`;
  heartbeat, `last_seen`, and a repeated idempotent request append none. The
  stream refuses a revision that is not the next one instead of serving a gap.
- Events are routed per lease: the ring stores canonical entries and the stream
  enqueues a materialized copy on each recipient, never on a detached lease.
- Control frames are never shed. A lease whose queue cannot accept one is closed
  with `TerminalEventLost`; at `terminate` that reservation runs before the
  terminal state becomes visible, and the terminal event is pinned until every
  recipient acknowledges it or retention collects the record.
- Compiled caps: backlog 256 events / 262,144 bytes per session
  (`max_event_backlog_events` / `max_event_backlog_bytes`), lease queue 256
  events / 65,536 bytes (`max_lease_queue_events` / `max_lease_queue_bytes`),
  8 leases per session default and 16 maximum (`max_attaches_per_session`),
  15,000 ms lease TTL (`attach_lease_ttl_ms`), 512 attach keys / 65,536
  accounted bytes per session (`max_idempotency_keys_per_session` /
  `max_idempotency_bytes_per_session`), and an 8-frame / 8,192-byte control
  reservation per lease queue (`D-10`).
- Stream state is retention-bound. A terminal entry is dropped when the registry
  collects it by TTL or evicts it at `max_retained_terminal_records`; every
  still-attached lease is closed with `TerminalEventLost` and reported through
  `TerminalReport::retired` or `RetentionReport::retired`. A retained terminal
  snapshot can therefore report
  an `attach_count` higher than the surviving stream leases: the registry record
  is frozen by `D-04`, while closing a lease on it is stream-only.
- `daemon_stopping` is a daemon-scoped control frame: it is queued to every live
  lease without a revision bump and is not part of a session backlog.
- `last_worker_seen_at` is absent: a terminal session has no supervision liveness
  signal, so the stream reports none.
- The compiled `D-14` table advertises these caps as
  `{effective, default, system_max}`; per-connection queue/backlog counters in
  `daemon.status` remain a follow-up, and bitty#2605 adds the V-21, V-23, C-04,
  and C-06 corpus vectors against a real transport.

## Endpoint and peer trust (bitty#2604)

`endpoint::open_endpoint` resolves, locks, attests, and binds the per-user
endpoint; `peer::PeerGuard` captures and re-checks the authenticated peer. The
crate still accepts nothing and parses nothing.

| Item                 | Rule                                                                                                                                                                                      | Where                                 |
| -------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------- |
| Unix endpoint        | `$XDG_RUNTIME_DIR/bitty/bittyd.sock`, socket `0600`, directory `0700`, both owned by the user                                                                                             | `endpoint::EndpointPaths`             |
| Override             | `BITTYD_ENDPOINT` names the socket path directly; it is a path, not a credential, and is absolute with a name                                                                             | `endpoint::ENDPOINT_ENV`              |
| Runtime root         | The derived `$XDG_RUNTIME_DIR` root must be owned by this uid and not group/world-writable; a shared root is `Tampered`                                                                   | `endpoint::attest_runtime_root`       |
| Missing runtime root | Typed `NoEndpoint`; no `/tmp` fallback, no abstract namespace, no TCP                                                                                                                     | `endpoint::EndpointError::NoEndpoint` |
| Path bound           | `> 107` bytes on Linux, `> 103` elsewhere: refused before any directory is created, never truncated                                                                                       | `endpoint::MAX_SOCKET_PATH_BYTES`     |
| Single instance      | Non-blocking exclusive `flock` on `bittyd.lock` held for the process lifetime; a second start is `AlreadyRunning`                                                                         | `endpoint::LOCK_FILE_NAME`            |
| Lock lifetime        | The kernel releases the lock on process death; the lock file is never unlinked (unlinking would defeat single-instance)                                                                   | `endpoint::acquire_instance_lock`     |
| Live endpoint        | A successful probe connect (or `EAGAIN` from a full accept queue) is `AlreadyRunning` and is never unlinked                                                                               | `endpoint::probe_endpoint`            |
| Stale endpoint       | `ECONNREFUSED` after the lock is held: quarantine-rename, re-attest the moved object, then remove                                                                                         | `endpoint::reclaim_stale_socket`      |
| Tamper               | Symlink, non-socket, foreign owner, missing dir/socket, shared root, or a bound socket whose mode/identity drifted: `Tampered`, nothing unlinked                                          | `endpoint::TamperKind`                |
| Clean shutdown       | Remove the socket only while the path still names this process's object (device, inode, and uid)                                                                                          | `endpoint::BoundEndpoint::close`      |
| Tamper interval      | `BoundEndpoint::reattest` re-runs the directory and socket checks for the loop bitty#2607 owns                                                                                            | `endpoint::BoundEndpoint::reattest`   |
| Peer capture         | Takes a connected `&UnixStream` (never a generic descriptor), derives the daemon uid from `geteuid()`, refuses a listening socket via `SO_ACCEPTCONN`, bounded `/proc` reads, fail closed | `peer::PeerGuard::capture`            |
| Peer re-check        | Start time, uid, and user namespace (Linux); a recycled pid or changed identity is refused; macOS is not servable                                                                         | `peer::PeerGuard::recheck`            |
| Boundaries           | `session.create`, controller attach, abort, kill, `session.resize`, transfer, detach-other, grant mint/revoke, `daemon.stop`, orphan verify                                               | `peer::DispatchBoundary`              |
| Unsupported cells    | Typed `UnsupportedPlatform`, no transport created                                                                                                                                         | `platform::PlatformCell`              |

Platform matrix (`D-15`):

| Cell                                   | Endpoint          | Peer mechanism                                                        | This slice                                                                                                |
| -------------------------------------- | ----------------- | --------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| Linux x86_64 / ARM64 / other           | Unix socket       | `SO_PEERCRED` + `/proc` start time + user namespace                   | servable; x86_64 native-tested here, ARM64 needs native evidence                                          |
| macOS ARM64 / x86_64 / other           | Unix socket       | `LOCAL_PEERCRED` + `LOCAL_PEERPID`; no start time in the safe surface | compiled and cross-checked; **not servable**: `open_endpoint`/`capture` refuse with `UnsupportedPlatform` |
| FreeBSD x86_64 / other                 | none              | `LOCAL_PEERCRED` exposes uid/gid only; no pid in the safe API         | `UnsupportedPlatform`; no weaker check                                                                    |
| Windows x86_64 / other                 | named pipe (plan) | client pid + token SID (plan)                                         | `UnsupportedPlatform`                                                                                     |
| Unlisted cells (`PlatformCell::Other`) | none              | none                                                                  | `UnsupportedPlatform`                                                                                     |

Windows plan, not implemented here: a local `\\.\pipe\bittyd-<SID>` with a
current-user-only DACL, remote clients rejected at the pipe level,
`GetNamedPipeClientProcessId` plus token SID comparison, and fail-closed
SID/AppContainer/integrity-level mismatches. It needs the reviewed Win32 FFI
seam (`bitty-ipc-winpipe`) and native-runner conformance before any Windows
build advertises a service endpoint.

A missing endpoint is a normal state, not an error that changes the consumer's
transport selection: clients treat `NoEndpoint` as "the daemon is not running".
The client-side endpoint-owner/mode check before the first request is
bitty#2607's; this crate only owns the daemon side.

`P0-AC-021` is **not** evidenced yet by a real second-account connection: the
current evidence is same-uid capture, the user-namespace refusal (when the host
permits `unshare -Ur`), and the synthetic owner/mode predicates. The native
second-uid vector (`V-25`) belongs to the bitty#2605/#2607 native-runners
evidence.

Recorded follow-ups (not fixed in this slice): a boot id plus a Linux pidfd
(`SO_PEERPIDFD`) would remove the capture-time pid-reuse window and make the
start-time evidence boot-scoped; the `/proc` start-time, effective-uid, and
user-namespace reads are a mixed snapshot, which the per-dispatch start-time
comparison catches after the fact but does not make atomic.

What bitty#2607 must wire:

- call `endpoint::open_endpoint(&EndpointEnv::from_process())` at startup,
  map `Tampered` to exit code 6, and call `BoundEndpoint::reattest` on the
  bounded tamper interval;
- accept on `BoundEndpoint::listener()`, enable `SO_PASSCRED`, and require
  per-frame `SCM_CREDENTIALS` on Linux (`D-07`);
- call `peer::PeerGuard::capture(&stream)` at accept before parsing any frame,
  and `PeerGuard::recheck(boundary)` before each privileged dispatch
  (`DispatchBoundary::ALL` is the current closed set);
- map `PeerError::UnsupportedPlatform` to `Denied/UnsupportedPlatform` and
  every other peer failure to `Unauthenticated`, closing the connection;
- bound the accept queue, connection count, and handshake time (bitty#2603
  supplies the advertised limits).

What bitty#2609 must wire: no peer or endpoint change is required for leases,
but a controller claim, abort, transfer, detach-other, and operator revoke are
privileged dispatches, so each must call `PeerGuard::recheck` with the matching
`DispatchBoundary` before it applies anything.

## Process containment and orphan verification (bitty#2791)

Containment is a whole-tree identity, not a pid and not a process-group
convention. `containment::ContainmentReport::probe()` returns a typed plan for
the compiled cell; there is no silent fallback and no weaker transport:

| Plan                 | Selection                                                                                                      | Cleanup claim | Signals                                         |
| -------------------- | -------------------------------------------------------------------------------------------------------------- | ------------- | ----------------------------------------------- |
| `cgroup_v2`          | delegated cgroup v2 root (`/proc/self/cgroup` + writable `cgroup.procs`/`cgroup.kill` + a create/remove probe) | `swept`       | members from `cgroup.procs`, then `cgroup.kill` |
| `subreaper_fallback` | `PR_SET_CHILD_SUBREAPER` confirmed by read-back when cgroup delegation is unavailable                          | `unverified`  | the exact pidfd and the recorded process group  |
| `none`               | no cgroup v2 and no subreaper, or a non-Linux cell                                                             | none          | no spawn; typed refusal                         |

Linux x86_64 is the first cell. A per-session cgroup is created **before** the
spawn, and the child writes its own pid into `cgroup.procs` from a `pre_exec`
hook, so a descendant cannot be born outside the identity. That hook, the PTY
`setsid`/`TIOCSCTTY` hook, and the descriptor sweep are the only `pre_exec`
closures in the crate, and all live in the single documented syscall seam,
`containment/linux_sys.rs`. The graceful window is `orphan_reap_grace_ms`;
escalation is `cgroup.kill` or `pidfd_send_signal(SIGKILL)`, followed by a
bounded emptiness wait. `swept`
requires an empty `cgroup.procs` **and** `cgroup.events` `populated 0` and a
successful subtree removal; an already-absent subtree is already reaped. A
missing `populated` line is unproved emptiness, never a sweep.

The subreaper fallback is consciously weaker: a `setsid` descendant can leave
the recorded process group, so the fallback reports `unverified` even after it
kills and reaps what it recorded. It never claims a sweep. A Linux host with
neither mechanism refuses the spawn with a typed error rather than running an
uncontained child.

`daemon.orphans.verify` is served on Linux x86_64 when startup selected an
enforced plan and opened the journal. The method takes an optional `action`:

- `verify` (the default when `action` is absent) re-runs verification and
  returns counts by class. The claim is exact and bounded:
  - the record must match the current boot id, the recorded uid, the process
    start time, the process group when a pid is present, and a cgroup path that
    is a direct child of the delegated root and owned by this uid;
  - a reserved cgroup record is actionable without a pid because the subtree
    was created before the spawn; a pid is never invented;
  - a reused or unreadable subreaper identity is quarantined and never
    signalled;
  - an absent or empty cgroup subtree is already reaped;
  - a record whose containment cannot be verified stays quarantined and is
    reported as `unverified`, never claimed verified;
  - the response is counts by class (`reaped`, `unverified`, `stale`,
    `reserved`, `committed`) with no pid, path, or command, and a second call
    on the same journal changes nothing unless the first call deferred a record
    under its pass budget.
- `retire` removes exactly one quarantined record named by its bounded
  `record_id` (the recorded session id, 32 canonical hex characters). It never
  signals, never adopts, and never touches a reserved or committed record
  (that is `Denied/InvalidRequest`); an unknown id is `NotFound`. It is the
  only way to free a quarantined slot before its retention expires. The
  response is `{action, retired, reaped, unverified, stale, reserved,
committed}` and is attributed as `daemon.orphans.retire`.

The startup scan completes before the daemon binds its endpoint (`D-06`):
startup calls `endpoint::prepare_endpoint` (resolve, attest, and take the
single-instance lock), opens the journal, reaps or quarantines every actionable
record, and only then binds the socket through `PreparedEndpoint::bind`. The
lock is held across the scan, so two starts cannot reap the same journal, and no
client can connect while an earlier record is actionable. The startup scan is a
full pass; an on-demand pass is bounded by one `orphan_reap_grace_ms` plus one
kill window (`ContainmentLimits::pass_budget`), so a journal full of live
orphans cannot hold the single-threaded service loop for the full per-record
window on every record. Every wait is clamped to the remaining budget; a
record whose wait is clamped is deferred: it keeps its `reserved`/`committed`
class (visible in the response counts), is never quarantined for the expiry,
and is retried by the next call.

Every other cell (including macOS and Windows) returns
`Denied/UnsupportedCapability`; `daemon.status` reports
`orphans: {verify: "unsupported_capability"}`. Windows has a designed job-object
boundary (one job per session, `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, a
current-user-only DACL) that needs the reviewed `bitty-winjob` Win32 seam and a
native-runner campaign; it is not approximated here. macOS has no containment
implementation in this slice.

Threat-model and evidence references: `T-21` (orphan reaping through a
containment identity), `P0-AC-023` (child scope limitation), and `ME-05`
(orphan reaping). The cgroup plan fences the tree before the spawn on the
tested cell, but this slice **does not close `T-21`, and `ME-05` stays
blocked**: the executable path digest is not recorded, crash injection is not
exercised against a real worker spawn, and no native Windows or macOS run
exists. The fallback leaves the documented
double-run residual (a crash between the spawn syscall and adoption is
quarantined, never guessed at). The reaper only opens a recorded path that is a
direct child of the current delegated root; a restart under a different parent
scope therefore quarantines the record instead of guessing, and a stable
service scope is required for reliable cross-restart reaping. A same-uid owner
can still edit their own cgroup and journal, which the threat model already
places inside the trust boundary. This slice adds live cgroup evidence on Linux
x86_64 but **does not mark any control verified**. The P0 evidence plan status
is unchanged by this README, and its update is a separate tracked task in
`bitty-docs`.

### Live evidence

`cargo test -p bitty-daemon --test containment -- --nocapture` probes the host
and runs real children: one that ignores `SIGTERM`, one that forks a
grandchild, a double-forked `setsid` session leader with closed standard
descriptors, the forced subreaper fallback including a descendant that ignores
`SIGTERM` after its leader exits, a contained child alongside an unrelated
recorded decoy, a reserved record left by a crash before commit, and a real
recorded cgroup orphan that the journal reaps and verifies. The probe line is the enforced plan and the cleanup it may
claim. On the development host (CachyOS, cgroup v2 with a delegated user scope)
the probe selected `cgroup_v2` and the live tests passed with `swept` cleanup;
the decoy was never signalled and the subreaper path reported `unverified`.
Tests must not be read as a cross-cell claim.

## Worker spawn, generation 1, and resize (bitty#2790)

`session.create` takes an optional `kind` (`terminal` by default). A worker
create always spawns; a terminal create spawns when it supplies the bounded
spawn specification and otherwise keeps the `1.2` registry-only shape. The
order is frozen and observable:

1. stage 6 (transport and service scope) runs in the dispatcher before the
   handler: a connection without `process.spawn` is `Denied/ScopeViolation`
   even when `session.host` is present;
2. the handler rejects an unknown `kind` as `InvalidRequest`; a worker create
   on a non-served cell, or a terminal create with a spawn specification whose
   containment plan cannot enforce it, is `Denied/UnsupportedCapability`;
3. stage 7 requires `session.host` (a consumed, live host grant), otherwise
   `Denied/ConsentRequired`;
4. the bounded spawn specification parses; a malformed field is
   `InvalidRequest`, a count or byte bound is `Framing/LimitExceeded`;
5. the idempotency key admits or replays; the spawn digest is part of the
   fingerprint, so a different spec under the same key is
   `Denied/IdempotencyConflict`;
6. the journal reserves the pre-spawn containment record, the child spawns
   into the per-session identity, the observed identity commits, and the
   registry binds **generation 1** with one revision bump. Only then is the
   session exposed as `running`, and only then is the token returned.

`spawn.rs` owns the bounded subset: `argv` (1..=`max_spawn_argv_entries`
entries, each at most `max_spawn_field_bytes`, no NUL), `env` (explicit
`name=value` entries, never a patch over the daemon environment; the child gets
exactly this environment plus `TERM`), one absolute `cwd`, `cols`/`rows`
(1..=1000, the terminal grid bound), and an optional `term`. The whole
specification is also bounded by `max_spawn_spec_bytes`; the byte bound
dominates the per-field bounds so individually valid fields cannot combine into
an oversized spec. The raw spec is never logged, audited, or recorded: the
idempotency store keeps only its domain-separated digest.

The PTY is single-owner. The child is spawned with `setsid` plus
`TIOCSCTTY`, the master is close-on-exec and non-blocking, and stdio is the
slave only: the three stdio descriptors are `F_DUPFD_CLOEXEC` copies from the
syscall seam, and a `close_range(3, ~0, CLOSE_RANGE_CLOEXEC)` sweep in the
child marks every other descriptor close-on-exec just before `exec`, including
descriptors the parent already held. The worker therefore starts with an fd
table of exactly 0/1/2 on the slave, whatever the daemon or a test harness had
open (`worker::tests::the_child_fd_table_is_exactly_the_pty_stdio` probes that
table from the child). `worker.rs` drains the master on every poll tick into
the per-session bounded content buffer (terminal kind) or discards it (worker
kind), checks the leader's exit without reaping it (`waitid` with `WNOWAIT`),
signals the recorded
identity while the unreaped zombie still reserves its process-group id, then
reaps the leader and drains the recorded group. It never blocks the service
loop for a running worker. On exit it signals the recorded identity once (not
a pid), then settles the cleanup class through the plan: a verified-empty cgroup
subtree is `swept` and its journal record is removed; the subreaper fallback is
`unverified` and its record is quarantined for the retention window. A failed
spawn terminalizes the session
as `failed`/`spawn_failed` at generation 0, with the cleanup class the plan
could prove (`not_started` before a child existed, `swept` or `unverified` after
one did), settles the journal record, and returns the normal create shape with a
`null` token.

`WorkerPool::forget` terminates a handle that is not already settled with the
same bounded escalation and reaps its leader before dropping it, so retiring a
session can never leave a live worker or an unreaped zombie. The per-tick PTY
drain is capped at `worker::DRAIN_BUDGET_BYTES` (one transport chunk) divided
across the live workers, so a continuously writing session cannot hold the
service loop; the rest stays queued in the PTY buffer as backpressure and is
drained on the next tick. A terminal's buffer is capped at
`content::CONTENT_BUFFER_BYTES`; a full buffer stops the drain, so a slow
reader applies terminal backpressure instead of growing memory. The
resize ioctl and the descriptor-flag helpers live in `containment/linux_sys.rs`,
the single FFI seam.

`session.resize` requires `terminal.manage` (granted to every connection at
negotiate, alongside `process.spawn`) and the **current controller lease** for
the target generation. Then:

- a stale generation is `Denied/StaleGeneration` and changes nothing;
- a stopping or terminal session is `Denied/SessionStopping` or
  `Denied/NotRunning`; a registry-only terminal is `Denied/NotRunning`;
- an observer lease holder is the typed `Denied/ControllerRequired`, while a
  caller with neither the operator view nor a live lease gets the uniform
  `NotFound` and no existence or revision oracle;
- the PTY ioctl applies first, so a content-plane failure never bumps the
  revision; on success `state_revision` advances exactly once and the
  controller lease renews.

A daemon-level `daemon.orphans.verify` pass skips exactly the sessions the
worker pool still holds a live handle for, so a reconcile read never signals a
running worker. A terminal record is exit cleanup, not supervision: it has no
live handle, so its journal record stays visible and an unprovable tree is
quarantined instead of silently skipped. The startup scan has no registry and
skips nothing.

## Terminal content plane (bitty#2790)

A bound terminal owns the content plane; a worker process does not. Three
methods serve it, all controller-only in this slice:

| Method              | Scope              | Shape                                                                     |
| ------------------- | ------------------ | ------------------------------------------------------------------------- |
| `session.read`      | `terminal.inspect` | `{session_id, generation, max_bytes?}` → `{data, bytes, buffered_bytes}`  |
| `session.write`     | `terminal.input`   | `{session_id, generation, data}` → `{accepted, remaining, backpressure}`  |
| `session.subscribe` | `terminal.inspect` | `{session_id, generation}` → `{subscribed, already_subscribed, sequence}` |

`data` is lowercase hex in both directions, so every byte value survives JSON
without a second wire type. `max_bytes` is optional and capped at
`content::CONTENT_FRAME_BYTES` (one quarter of `max_chunk_bytes`); a larger
value is `Framing/LimitExceeded`, zero is `InvalidRequest`. A read is
non-blocking: an empty buffer is a successful zero-byte read. A write never
queues in the daemon: a partial PTY write reports its accepted count and
`backpressure: true`, and a full PTY input buffer is the typed
`Denied/ChannelFull` with nothing written.

`session.subscribe` additionally requires a connection that negotiated
`events: true`; a non-opted controller gets `Denied/UnsupportedCapability`.
Subscribed frames ride the negotiated event connection as
`{"kind":"session_content","session_id","attach_id","generation","sequence",
"data","buffered_bytes"}`. The server reserves
`content::CONTENT_FRAME_ENCODED_MAX` outbound room before taking a frame, so a
frame is either queued whole or left in the session buffer for a later pass; a
slow reader never causes a drop or an unbounded queue. The sequence starts at
zero per subscription and only advances for a frame actually handed to the
transport.

The generation is a hard fence on all three methods. A terminal record after
exit is read-only metadata: every content method answers `Denied/NotRunning`,
the subscription is removed at the terminal transition, and no content frame
follows the `session_terminal` event. Bytes still buffered at that transition
are discarded; the terminal event is the reconciliation point, and the child
has already stopped producing. An observer lease, even one holding
`terminal.inspect`, is refused `Denied/ControllerRequired`: the observer grant
never becomes a content grant. Detach, revocation, expiry, control transfer,
and connection close all remove the subscription; a disconnected subscriber
releases the lease but never kills the terminal process.

Success is not audited (content methods are high-volume and their bytes must
never enter audit); a refusal is the normal static refusal record with no
content, path, pid, argv, environment, or token.

## Service loop (bitty#2607)

`bittyd start` is the only invocation that binds anything. Startup order is
core-dump hardening (`RLIMIT_CORE=0` and `PR_SET_DUMPABLE=0` on Linux, before
any privileged state exists), endpoint (`open_endpoint`), operator capability
(`0600` file in the `0700` runtime directory), audit sink
(`$XDG_STATE_HOME/bitty/audit`, `0700`/`0600`), then the single-threaded poll
loop. The loop blocks `SIGTERM`/`SIGINT` behind a signal fd, so the signal
drain runs in the same loop as requests.

| Bound                  | Source                                           | Behavior                                                                                                                |
| ---------------------- | ------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------- |
| Connection cap         | `max_connections_per_endpoint`                   | the accept path closes a connection above the cap                                                                       |
| Request rate and burst | `max_request_rate_per_sec` / `max_request_burst` | the accepted `bitty-ipc` RC-9 token bucket, `Denied/RateLimited`                                                        |
| Frame bound            | `max_frame_bytes`                                | a declared length above the bound closes the connection                                                                 |
| Pre-auth budget        | `pre_auth_max_bytes`                             | a connection that sends more before negotiating is closed (completed frames and the frame being assembled count)        |
| Negotiate deadline     | `negotiate_timeout_ms`                           | an unnegotiated connection past the deadline is closed                                                                  |
| Request deadline       | `request_timeout_ms`                             | a post-handshake request past the bound is answered `Timeout` with its id and bound; the connection stays open          |
| Idle bound             | `transport_idle_ms`                              | an idle connection is closed                                                                                            |
| Outbound bound         | `max_connection_outbound_bytes`                  | a response queue above the bound closes the connection                                                                  |
| Grant sweep            | `GRANT_SWEEP_INTERVAL_MS` (1 s)                  | retired grant records are swept on the loop timer                                                                       |
| Close drain            | `CLOSE_DRAIN_TIMEOUT_MS` (1 s)                   | a retired connection that cannot drain its refusal frame is closed; it never collects a second handshake-timeout record |
| Tamper interval        | `TAMPER_INTERVAL_MS` (5 s)                       | `BoundEndpoint::reattest`; a failure is exit code 6                                                                     |

### Request deadline (bitty#2805)

The loop stamps a request when dispatch admits it and compares the stamp when
dispatch returns, using the effective `request_timeout_ms` (never the negotiate
deadline). A completion at or past the bound is answered with the accepted
`Timeout` class:

```json
{
  "v": 1,
  "id": "<request id>",
  "ok": false,
  "error": {
    "class": "Timeout",
    "code": "Timeout",
    "message": "request deadline exceeded",
    "request_id": "<request id>",
    "timeout_ms": 10000
  }
}
```

`request_id` echoes the request envelope `id`; `timeout_ms` is the effective
bound. The refusal is audited once with the method's action and the static
`Timeout` code. It never closes the connection: later requests on the same
connection are served normally, and only the timed-out request is refused. A
timed-out mutation's outcome is unknown, so the client reconciles with
`session.status` and retries only with the same idempotency key and the current
generation (`D-13`).

The deadline is a local admission stamp, not a timer armed in the poll set:
completion drops it, so there is no wakeup, no per-request thread, and no timer
leak between requests. The single-threaded loop cannot preempt a synchronous
handler, so a method whose handler can legitimately block past the bound keeps
its own timer and is exempt from conversion
(`ServiceMethod::request_deadline_exemption`): `daemon.stop` (it terminates
every live session before it answers; each bound worker costs at most
`abort_grace_ms + kill_wait` and the pool is capped by `max_sessions`) and
`daemon.orphans.verify` (the containment pass budget). `session.create` is not
exempt: its synchronous spawn/bind completes within the bounded cleanup, under
the default request bound. `session.abort`/`session.kill` wait at most
`abort_grace_ms` plus the kill window, under the default request bound, and are
not exempt.

Process hardening (`D-16`, `T-23`, `P0-AC-026`): on Linux, `bittyd start`
lowers `RLIMIT_CORE` to zero and clears the dumpable flag
(`PR_SET_DUMPABLE=0`) before the endpoint is bound and before the operator
capability is minted; a failure exits `1` rather than serving dumpable. macOS,
FreeBSD, and Windows never serve in this slice (typed `UnsupportedPlatform`),
so no cell serves with core dumps enabled. `tests/service_loop.rs` pins both
controls against the real process (`/proc/<pid>/limits` and `/proc/<pid>/mem`).

A frame-level, byte-budget, or deadline closure is one aggregated `wire` audit
record (D-16). When the refusal follows a complete envelope the typed refusal
frame is written and then the connection closes; a closure inside a partial
frame has no correlation id to echo, so it is audited and closed without a
partial response. A complete frame whose `id` is over the 64-byte bound is a
**deliberate contract exception**: the refusal is `InvalidRequest` with `id`
omitted, and the connection retires after that frame drains, so the value is
never echoed and the stream cannot be mistaken for a correlated exchange. The
pipelined-tail refusal below is the second, and only other, no-`id` shape; both
are the consequence of refusing bytes the server must not parse. A refusal that
[`Refusal::closes_connection`](../src/service/dispatch.rs) marks (`VersionMismatch`,
`Unauthenticated`, `Denied/AlreadyNegotiated`) keeps its frame queued, stops
reading, and shuts the stream down only after the queued bytes drain.

The loop never opens a TCP listener. `daemon.status` reports
`tcp_listeners`, the daemon's self-attested count of its own TCP listening
sockets from `/proc/self` (`0` expected; `null` outside Linux). The test reads
that field instead of scanning `/proc/<pid>/fd`, which the non-dumpable daemon
does not expose; the count is an observability fact, not an authority input.

### Scopes and method catalog

`service::scopes` is the bittyd-owned registry. It reuses the `bitty-ipc-api`
method grammar, so:

- a malformed name is `InvalidMethod`;
- a well-formed name outside the catalog is `NotFound` (deny-by-default), and
  the v1 families stay owned by `bitty-ipc` (a test proves the catalog never
  redefines one of the 51 v1 methods);
- the transport-scope requirements are `session.resize` → `terminal.manage`,
  `session.read`/`session.subscribe` → `terminal.inspect`, and
  `session.write` → `terminal.input`; the daemon grants all three terminal
  scopes plus `process.spawn` at negotiate;
- a connection with neither `process.spawn` nor `session.host` is
  `Denied/ScopeViolation`, `process.spawn` without `session.host` is
  `Denied/ConsentRequired`, and `session.host` without `process.spawn` is
  `Denied/ScopeViolation`.

| Scope              | Baseline | Grants                                                           |
| ------------------ | -------- | ---------------------------------------------------------------- |
| `session.inspect`  | yes      | metadata status and list of sessions with a live lease           |
| `session.host`     | **no**   | create a session; only a consumed host grant adds it             |
| `session.attach`   | yes      | accept an observer grant                                         |
| `session.control`  | yes      | control relation actions (claim, grant, abort, transfer, revoke) |
| `terminal.inspect` | yes      | read and subscribe to terminal output, still controller-gated    |
| `terminal.input`   | yes      | write terminal input, still controller-gated                     |
| `terminal.manage`  | yes      | resize the content plane, still gated by the controller lease    |
| `daemon.inspect`   | no       | operator status and all-session view                             |
| `daemon.control`   | no       | stop, host grants, orphan verify, operator kill                  |

The connection's transport set is `process.spawn` plus `terminal.inspect`,
`terminal.input`, and `terminal.manage`. They are not the `bitty-ipc` CLI
defaults: the daemon grants no `view.*`, `config.*`, `plugin.*`, or `debug.*`
scope, so the handshake never advertises an ambient content surface. A
terminal scope never authorizes by itself: a resize, read, write, or subscribe
is still refused unless the caller holds the controller lease for that
generation, and an observer lease is never promoted by the scope.
When the executable-policy grant lands, `process.spawn` must come from it
instead of this transport default. The create-authority combinations are
unit-tested: both missing is `ScopeViolation`, consent missing with spawn
present is `ConsentRequired`, spawn missing with consent present is
`ScopeViolation`, both present proceeds.

`session.detach` is scope-free when the caller already owns the attach lease;
detaching another connection's lease needs `session.control` or
`daemon.control`, decided before the target lookup. The operator all-session
view is `daemon.inspect`, while control actions remain `daemon.control` (the
capability always grants both).

`session.attach` spells the request role `attach_role` (`controller` or
`observer`); the response keeps `role`. The transport reserves `role`, `auth`,
and `scope` as ambient-authority fields and refuses any of them at depth 1, so
the service parameter cannot share those names.

The accepted session-host contract spells the effectful parameter `generation`
for `session.abort`, `session.kill`, and `session.control.transfer`, and
`target_attach_id` for the transfer target. Those are the canonical spellings
in the corpus. The pre-freeze `expected_generation` / `attach_id` spellings are
accepted as deprecated aliases for one release; `session.attach` keeps
`expected_generation` because that is its accepted spelling. Presenting both
spellings with different values is `InvalidRequest`, never a precedence guess.
The aliases are a compatibility window, not a second contract: a future
release removes them and the corpus already uses only the canonical names.

### Handshake

`daemon.negotiate` is the only pre-authentication method. The request carries
`client_version`, optional `client_correlation`, optional
`operator_capability`, optional `host_grant`, and optional `events`. The
response carries
`wire_version`, `service_api_version`, `daemon_instance_id`, `daemon_pid`,
`server_identity`, `peer_identity`, `client_id`, `scopes`, `capabilities`,
`limits` (`{effective, default, system_max}` for every row), `server_monotonic_ms`,
and `daemon_state`.

- `client_correlation` is ≤ 128 bytes with no control characters, hashed on
  receipt; only the truncated 128-bit digest is audited and the raw value never
  survives parsing. The same digest binds a grant when the mint names it.
- A present but invalid `operator_capability` is one static
  `Unauthenticated` refusal that closes the connection. Verification is an
  in-memory constant-time compare; the request path never reads the capability
  file, so a missing file and a wrong bearer are indistinguishable.
- A `host_grant` that cannot be consumed adds no scope and does not close the
  connection; `session.create` then returns `Denied/ConsentRequired`.
- A version outside `1.0..=1.3` is `VersionMismatch` and closes. A second
  negotiate, or any frame read before the negotiate request, is
  `Denied/AlreadyNegotiated` and closes after the refusal frame is flushed.
  The gate runs after the per-frame wire version and before the rate limiter,
  the method grammar, and the catalog, so an unnegotiated connection cannot
  probe which methods exist.
- **No pre-auth pipelining.** After a successful `daemon.negotiate`, any byte
  already queued before the response's first byte is written — a complete
  frame or a fragment of one — is refused with a typed
  `Denied/AlreadyNegotiated` frame and the connection closes after that frame
  drains. A single non-blocking `MSG_PEEK` at that decision point decides; the
  queued bytes are never read, parsed, or resumed, so a partially sent second
  frame cannot become a fresh request and no pipelined method is ever served
  after the handshake. A byte that arrives after the response write begins is
  a post-handshake frame, parsed only after negotiation completes, so a client
  that writes its next request as soon as it reads the handshake is served
  normally. The refusal omits `id` because the tail is unattributable without
  parsing it (the second no-`id` contract exception above); a client that
  pipelined must reconnect.
- **A failed handshake is final.** `VersionMismatch`, `Unauthenticated`, and
  every refusal of a `daemon.negotiate` request — including the
  `InvalidRequest` produced by a malformed negotiate payload or a refused
  capability check — drains its refusal frame and then closes. There is no
  second negotiate and no best-effort retry on the same connection.
- **A pre-handshake frame without a wire version is final.** A frame that
  omits `v` before the connection is negotiated is `InvalidRequest`, drains,
  and closes; the transport check runs before the negotiate-only gate, so the
  same connection cannot negotiate afterwards.
- Envelope-level refusals that never resolve to a catalog method (malformed
  envelope or id, unsupported wire version, unknown or malformed method,
  rate limit before parsing, frame or deadline closure) are one `wire` refusal
  record each, subject to the per-connection budget below.

### Grants

`service::grants` holds host-consent and observer grants: 256-bit bearer,
non-secret 128-bit `grant_id`, peer-uid binding, optional correlation-digest
binding, TTL from `host_grant_ttl_ms` / `observer_grant_ttl_ms`, pending caps
(`max_pending_host_grants`, `max_pending_observer_grants_per_session`), single
use, and idempotent revoke by `grant_id`. Missing, expired, consumed, revoked,
wrong-session, wrong-peer, and wrong-correlation cases are one `Unusable`
refusal. The bearer is retained only while the grant is live so a keyed
`session.grant` replay can return the same unconsumed bearer; it is cleared on
consume and revoke. A mint at the cap is `LimitExceeded`.

The accepted optional mint fields are enforced, not ignored:

- `ttl_ms` (both mints) is a positive integer no greater than the compiled
  `observer_grant_ttl_ms` / `host_grant_ttl_ms`; `0` and an over-limit value
  are `InvalidRequest` with `counts.actual` and `counts.limit`, and the stored
  expiry uses the requested TTL. Omitting the field keeps the compiled default.
- `observer_correlation_digest` (`session.grant`) and
  `client_correlation_digest` (`daemon.host_grant`) bind a grant to the digest
  the **mint request names**, not to the minting connection: the operator or
  controller names the 128-bit `client_correlation_digest` that a later client
  or observer will present at `daemon.negotiate`. The value is **exactly 32
  lowercase hex characters**; any other width or case is `InvalidRequest`,
  because it could never equal a connection's 128-bit digest and would mint a
  grant that is dead on arrival. A bound grant is usable only from a connection
  whose negotiation digest matches: a mismatch adds no scope at host-grant
  consume, is `Denied/ObserverGrantRequired` at observer attach (the
  contract's static wrong-peer code), and is `Denied/ConsentRequired` at
  create. On a non-operator `session.grant.revoke` a mismatch returns the same
  `not_active` success as an unknown grant id and revokes nothing, so the
  128-bit id is not an existence oracle for the binding. The operator path
  keeps the contract's "operator may revoke any grant" authority and is not
  correlation-gated; `daemon.host_grant` revoke is operator-only. Omitting the
  field keeps the historical uid-only binding.
- `ttl_ms` and the digest fields are mint-only, and each digest field belongs
  to exactly one method: `client_correlation_digest` on `session.grant` and
  `observer_correlation_digest` on `daemon.host_grant` are cross-method fields
  and are `InvalidRequest`, never silently ignored. Sending a mint-only field
  on either revoke is likewise `InvalidRequest`. A keyed `session.grant`
  duplicate with a different TTL or digest is `Denied/IdempotencyConflict`,
  never a replay of a grant bound differently.

The store is bounded by more than the active caps. The server loop sweeps
retired records on `GRANT_SWEEP_INTERVAL_MS`, a consumed host grant is retained
until its TTL because `host_grant_bound` still authorizes the consuming
connection's create, every other inactive record is dropped immediately, and
`MAX_GRANT_RECORDS` (1,024) is the hard total bound checked at mint (after the
sweep) so the store cannot grow with total mints. A mint at the total bound is a
whole `LimitExceeded` refusal and never evicts a live record.

Expiry is a snapshot-visible mutation, not just a drop. When the sweep retires
an observer grant whose TTL lapsed without a consume or a revoke, the owning
session's `state_revision` advances exactly once — however many of its grants
expired in the same sweep — with the same monotonic rules as mint, consume, and
revoke. A sweep that retires nothing, and every later sweep, bumps nothing, so
the tick is idempotent. The store records the affected sessions as records are
dropped (including the capacity sweep a mint performs) and the service drains
that set on each tick, so no eviction path can retire an expired grant without
reporting it. A terminal session keeps its frozen revision, and a host grant has
no owning session: neither is bumped, because neither lost session authority.

A consumed host grant stays revocable until its TTL: `daemon.host_grant`
`revoke` marks the record revoked and clears any retained bearer, so the
consuming connection's next `session.create` — including a keyed replay of a
key already applied on that connection — is `Denied/ConsentRequired`
immediately rather than at the 300 s TTL. The record is still retained until
the TTL so the bound connection sees the false `host_grant_bound` answer.
Observer grants keep the unconsumed-and-unexpired revoke rule. A revoke on a
terminal record is `Denied/NotRunning` before the authority relation is read:
the record is frozen and nothing can consume the grant, so a former observer
cannot retire another client's pending grant.

A keyed `session.grant` mint is replayable only while the session record is
live. The terminal transition retires the session's `D-18` key store
(`Service::retire_session`), so a later call with the same key is a new request
against a frozen record and takes `Denied/NotRunning`, not a descriptor replay;
the idempotent terminal snapshot covers the keyless termination and revocation
methods, not a keyed mint with no effect left to replay.

### Dispatch order

`service::dispatch` implements the eleven stages: transport, pre-auth, service
version/capability, method resolution, per-frame authentication, scope, derived
authority, target, generation, idempotency, effect. Invariants:

- scope is decided before target lookup, so a caller without the scope learns
  nothing about whether a target exists;
- while `daemon_state` is `stopping`, every mutating method is refused
  `Denied/DaemonStopping` before any stage work; reconciliation reads
  (`daemon.status`, `session.status`, `session.list`, `lease.heartbeat`) and
  the repeat `daemon.stop` keep their own answers;
- visibility is a live lease, a presented current control token, the operator
  path, or (for a former leaseholder) the immutable terminal snapshot;
- every control method (`session.attach`, `session.grant`,
  `session.grant.revoke`, `session.detach` on another lease,
  `session.control.transfer`, and `session.abort` with a control reason) maps
  a missing, stale, or rotated token at a live, vacant control to
  `Denied/InvalidControlToken`; an occupied, terminal, or unknown target stays
  the uniform `NotFound`, no refusal echoes the token, and nothing applies;
- that control mapping is the contract's named denial-order step 4, not a
  general visibility widening: `session.status`, `session.list`, and every
  invisible read keep the uniform `NotFound`, and the mapping requires the
  method scope and a session id the caller already holds;
- every privileged effect re-checks the captured peer after authorization and
  before the effect; a failure is `Unauthenticated` and closes the connection;
- every mutation is admitted by the audit sink first; a degraded sink is
  `Denied/Internal` with no state change;
- keyed methods are `session.create`, controller `session.attach`,
  `session.grant` mint, and `session.control.transfer`; termination and
  revocation are keyless and never consult a store; observer attach follows the
  attached grant text (the grant is validated before the lease and consumed
  after it, so a replay is `Denied/ObserverGrantRequired`);
- the rotated control token from a transfer is queued only for the target
  connection and delivered on that connection's next heartbeat; the requesting
  response never contains it.

### Served methods

`daemon.negotiate`, `daemon.status`, `daemon.stop` (`refuse`/`drain`/`force`),
`daemon.host_grant` (`mint`/`revoke`), `session.create`, `session.status`,
`session.list`, `session.attach` (observer and controller), `session.grant`,
`session.grant.revoke`, `session.detach`, `session.control.transfer`,
`session.resize`, `session.read`, `session.write`, `session.subscribe`,
`session.abort`, `session.kill`, `lease.heartbeat`.

`daemon.status` reports the daemon state, active and per-state session counts,
`audit_degraded`, `daemon_pid`, `uptime_ms`, the drain deadline, the advertised
limit triples, the orphan object (its five class counts exist only on a served
cell), and
`tcp_listeners` (see the service-loop note above).

### Served metadata fields (bitty#2806)

Service `1.2` adds optional response keys only. Service `1.3` adds three
methods and two transport grants (`terminal.inspect` and `terminal.input`;
`terminal.manage` already existed). Every existing key keeps its meaning and
shape, and a `1.0` through `1.2` client that ignores unknown fields keeps
working.

| Method                                    | Added key(s)                                                                  | Rule                                                                                                                                                                                                                                                                                                                                                                                                            |
| ----------------------------------------- | ----------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| every method returning a `session` object | `controller_present` (bool), `pending_observer_grants` (u16)                  | presence only: the controller's `attach_id` is never served. The count is bounded by `max_pending_observer_grants_per_session`; a terminal session serves `0` for both because no lease or grant it still stores can be used.                                                                                                                                                                                   |
| `session.list` operator view              | `owner: {uid, pid}`                                                           | only with `daemon.inspect`, and only as the two numeric facts captured at create. An ordinary view omits the key (omit, never `null`); no path, argv, gid, or start time is retained or served. `session.status` never carries it.                                                                                                                                                                              |
| `session.attach` fresh success            | `scopes` (array)                                                              | the connection's effective lease scopes: the same service-then-transport names and order as the handshake `scopes`. A keyed controller replay does not re-assert them.                                                                                                                                                                                                                                          |
| `session.attach` every success            | `server_monotonic_ms` (u64)                                                   | the server clock at the request; a replay carries it too.                                                                                                                                                                                                                                                                                                                                                       |
| `session.attach` event-opted connection   | `backlog`, `backlog_truncated`, `earliest_revision`                           | already delivered by bitty#2792; absent when the connection did not opt into pushed events.                                                                                                                                                                                                                                                                                                                     |
| `lease.heartbeat`                         | `server_monotonic_ms` (u64)                                                   | the same clock as `lease_expires_at_ms`; the singular `attach_id` request form is unchanged.                                                                                                                                                                                                                                                                                                                    |
| `daemon.status`                           | `daemon_pid` (u32), `uptime_ms` (u64), `orphans.records.reaped`/`stale` (u64) | `daemon_pid` is observational only. `uptime_ms` is `now - daemon start` on the server monotonic clock. The `orphans.records` object exists only when `orphans.verify` is `served`; the unsupported branch carries `verify`, `mechanism`, `cell`, and `reason` and omits `records`. The journal removes a reaped or stale record, so those two fields are this instance's cumulative counts, not stored classes. |

No addition serves terminal text, PTY bytes, a content handle, another
session's fields, or caller bytes. A count is a count, never a grant id or
bearer; the owner object is numeric facts only. The contract rows this revision
can close are the `session.status`/`session.list` controller-presence and
pending-grant rows, the operator owner-peer row, the `session.attach` lease-scope
row (the backlog row was already delivered by bitty#2792), the heartbeat
`server_monotonic_ms` row, and the `daemon_pid`/uptime and orphan-class rows.

### Audit sink

`audit_sink::AuditSink` drains the bounded ring into an owner-private rotating
file. `admit_mutation` flushes and syncs before a privileged effect; a failure
sets `audit_degraded` and refuses with `Denied/Internal` before any state
changes. A record that fails to append stays queued in event-id order and is
retried, so a transient failure neither reorders nor drops the tail; a record
that cannot be rendered sets `audit_degraded` as well, so `daemon.status`
cannot report `false` while mutations are refused. The directory chain is not
followed blindly: each owning directory is attested (no symlink, owned, `0700`
for the leaf) and created one level at a time, and a fresh account's missing
state root is created `0700` one level at a time rather than failing startup.
Rendered lines carry only closed spellings, ids, digests, and numbers.
`service::dispatch` records one individual refusal record for every refused
method with its static code and counts; the server loop records one `wire`
refusal for every envelope, frame, budget, or deadline closure that never
resolves to a method. Only those wire-level refusals are pooled by this budget;
a refused method still gets one individual record and is never counted in the
aggregate histogram, so per-method refusal aggregation remains the bitty#2603
follow-up. Non-closing wire refusals are budgeted per connection at
`refusal_audit_budget_per_sec` records per second. A window that suppresses
refusals ends with exactly one aggregate `wire` record carrying
`sup=<suppressed>`, `bl=<budget>`, and a bounded static code histogram
(`h=code:count,...`, with `ho=<count>` for codes past the
`MAX_AGGREGATE_CLASSES` cap); a closing connection or an endpoint-tamper exit
flushes its pending window so no suppression is lost. The aggregate is
fixed-size and cannot rotate the segments by itself, so a refusal flood stays
bounded without becoming invisible.

The re-attestation interval is the frozen `TAMPER_INTERVAL_MS` in every release
build. Debug builds honour `BITTYD_TEST_TAMPER_INTERVAL_MS` (clamped to
`1..=TAMPER_INTERVAL_MS`) only so the tamper-flush regression test can keep a
refusal-audit window open across the tamper check; the variable is compiled out
of release builds and can never lengthen the interval.

### Per-frame peer credentials

On Linux the accepted socket enables `SO_PASSCRED` before the first read, and
`service::frame::PeerFrameReader` requires `SCM_CREDENTIALS` on every read of
every frame. A client must also enable `SO_PASSCRED` on its own socket before
sending (`service::frame::enable_credentials`): the kernel only fills in the
sending process's credentials for a socket that opted in, and a frame without
them is refused. Clients that skip it are rejected as `Unauthenticated`, not
trusted because the connection was authenticated at accept. The first byte of a frame must carry the captured peer's uid and
pid, every read of the same frame must carry the same credential, and a missing
credential is `Unauthenticated`. The reader reads the header and then exactly
the declared payload, so a `recvmsg` cannot span two frames; a descriptor
passed to another process is attributed to the injecting process's pid and is
refused. Cells without the mechanism fall back to the captured process
re-check before every privileged action; the residual is stated, not hidden,
and those cells are not servable in this slice.

## Conformance harness (bitty#2605)

`tests/harness/` is the shared conformance suite. One machine-readable vector
set (`tests/harness/corpus.json`, schema `bittyd-conformance-corpus/1`) drives
two independent drivers:

- `fake.rs`: an in-process reference server labeled `candidate-not-bittyd` on
  every run. It implements the handshake, create, status, list, attach, detach,
  grant mint/consume/revoke/transfer and grant expiry, abort, kill, heartbeat,
  stop, resize, the content methods over a simulated echo terminal, and the
  closed refusal set over an in-memory transport, with injected time and
  limits. It never claims a real process or a real PTY.
- `real.rs`: the built `bittyd` binary against a throwaway runtime/state root,
  speaking the real `bitty-ipc` v1 frames over the Unix endpoint with
  per-frame credentials. Linux x86_64 is the servable cell in this slice;
  other cells record a typed `cell-not-servable` skip instead of a silent pass.

Both drivers run the same expectations and write separate evidence files under
`target/conformance/` (or `$CARGO_TARGET_DIR/conformance/`):

```text
<cell>.fake.json    # label candidate-not-bittyd
<cell>.real.json    # label bittyd-real-service
<cell>.parity.json  # shared vectors compared, divergences, not_compared
```

Each row records the vector id, driver, status (`pass`, `skip`,
`known-defect`, `fail`), typed skip reason, deferred requirement, daemon
instance id, and a step signature (`ok`, refusal class/code, or closed). The
file records the crate version, commit, cell, advertised capabilities, and the
effective limits from the handshake, never a host path, username, socket path,
port, token, or capability value.

Corpus rules:

- every vector declares setup, injected time/limits, steps, typed expected
  results, post-state assertions, and forbidden observations; a vector that
  asserts only "no crash" is rejected by the parser;
- `session.attach` freezes the request field `attach_role` (the response keeps
  `role`), and the ambient-authority fields `role`, `auth`, and `scope` stay
  refused;
- a vector that does not run on a driver carries a typed skip reason, and a
  vector that needs a served capability declares `requires`; a driver whose
  negotiated capability set lacks it records a typed `cell-not-servable` skip
  instead of failing the vector;
- `V-44` pins the post-handshake request deadline: `request_timeout_ms` is
  lowered to 100 ms, `daemon.status` is given a simulated 250 ms handler
  through `injected.handler_delay_ms`/`handler_delay_method`, the refusal must
  carry `Timeout` with `request_id` and `timeout_ms`, and a later `session.list`
  on the same connection must succeed. The fake advances its injected clock;
  the real driver uses the `test-seams` feature (enabled by
  `just daemon-conformance`; the feature is test-only and no release workflow
  enables it) and records a typed
  `cell-not-servable: request_deadline_seam` skip without it;
- cell-dependent values are seeded variables, never literals: `@worker_sessions`
  is the served capability of the cell a driver models (the reference fake
  models a served worker cell; the real driver seeds the readiness probe's
  served-cell answer)
  and `@cwd` is a directory the host reports as absolute, so the corpus never
  asserts a Linux-only capability or a Unix root path on Windows or macOS;
- `known_defect` rows record the exact failing step and code or result path,
  are excluded from the parity gate, and fail the suite if they pass or fail at
  another step so the marker cannot rot; the corpus currently carries none
  (V-07/bitty#2768 and C-04/bitty#2767 were fixed and their markers removed);
- `C-17` pins the additive `1.2` metadata on both drivers: controller
  presence and the pending-grant count (mint, consume, and post-terminal), the
  operator-only owner peer, attach scopes and server clock, the heartbeat
  clock, and the daemon pid, uptime, and orphan-class keys;
- `V-45`–`V-48` pin the terminal content plane on both drivers: a terminal
  create with a spawn specification binds generation 1 and serves bounded
  read/write; an observer lease is `ControllerRequired`; a non-opted
  connection cannot subscribe; and a registry-only terminal keeps the `1.2`
  `created`/generation-0 shape with no content plane. `V-49` is real-only: a
  spawned terminal delivers a `session_content` frame on the event wire over
  the real PTY; the fake records the typed `requires-real-pty` skip;
- fault injection covers a transport drop during abort, the pre-auth partial
  frame, the frame bound, the pending grant cap, the attach cap, and an
  instance change (restart); queue pressure with an injected outbound cap is
  a fake-only row because the served loop has no clock/limit injection point,
  and the injected-clock rows (V-06 lease expiry, C-10 grant expiry) are
  fake-only for the same reason.

Run it with:

```sh
cargo test -p bitty-daemon --test conformance --locked
```

The `Quality gates` job executes the fake suite and uploads its evidence; the
Linux Tier-1 job executes the real suite and uploads
`bittyd-conformance-linux-x86_64`. The parity check compares the two signatures
for every shared, non-known-defect vector and fails on any divergence.

## CLI

```text
bittyd start       # bind the endpoint and serve until daemon.stop or a signal
bittyd --version   # crate version + contract service API, exit 0
bittyd --help      # usage, exit 0
bittyd             # status only: no endpoint bound, no runtime file, exit 0
bittyd <other>     # usage error, exit 2
```

Exit codes: `0` success (including "already running", where the desired state
holds), `1` startup failure before a listener accepted, `2` usage, `6` endpoint
tamper. `3`, `4`, and `5` are the contract's `daemon.stop` outcome codes; a
running worker is terminated under its containment identity and recorded before
the exit, so a clean stop is still `0`.

## Dependency direction

The daemon sits **above** the mechanism crates. It may depend on
`bitty-term-state`, `bitty-pty`, `bitty-plugin-host`, `bitty-ipc`, and other
mechanism crates; none of them may depend on `bitty-daemon`. The seam declares
no mechanism edge yet; each edge is added by the slice that consumes it
(bitty#2601 for registry state, bitty#2607 for IPC, bitty#2604 for platform
cells).

`tests/dependency_direction.rs` reads `cargo metadata --no-deps` and fails
when:

- any non-daemon crate depends on `bitty-daemon` (normal, dev, build,
  optional, or target-specific edge);
- `bitty-daemon` is not a workspace member;
- the crate has other than one library and one binary (`bittyd`); test targets
  are allowed and are not counted.

A daemon-shaped global in a terminal or mechanism crate is a contract
violation, not a wiring detail (ADR 0008 / ADR 0019).

## Layout

- `Cargo.toml` — package metadata, one `[[bin]]` (`bittyd`), no runtime
  dependency yet.
- `src/lib.rs` — crate docs, status, and the deferral map.
- `src/contract.rs` — service version, capability set, method name.
- `src/handshake.rs` — `daemon.negotiate` request/response skeleton, the `D-11`
  acceptance rule, and the advertised effective limits.
- `src/limits.rs` — the compiled `D-14` and inherited transport table, the
  `{effective, default, system_max}` triples, configuration validation, and the
  enforcement-stage ordering.
- `src/codes.rs` — the closed static refusal-code set from the error taxonomy.
- `src/idempotency.rs` — the two bounded `D-18` stores, replay, conflict,
  capacity retirement, and the create-key TTL floor.
- `src/audit.rs` — the content-free `D-16` record shape, stable event ids,
  injected timestamps, counters, and the bounded ring.
- `src/registry/` — ids, session records and lifecycle, the registry table,
  counters, retention, and refusals.
- `src/events/` — the event envelope and kinds, the bounded backlog ring, lease
  queues with the control reservation, the enforced limits, the per-session
  stream, and the wire delivery bus (`bus.rs`: per-session ring, per-lease
  queues, pinned terminal event, replay window).
- `src/lease/` — the control token and digest, lease records and limits, the
  lease/control/stop state machine, and its closed refusals.
- `src/orphan/` — process identity probes, the bounded journal, classification,
  and the exact-identity reaper seam.
- `src/spawn.rs` — the bounded `session.create` spawn specification, its
  field/count/whole-spec limits, and the domain-separated digest.
- `src/worker.rs` — the process pool: contained PTY spawn, kind-aware
  generation binding, non-blocking drain/supervision, resize, content helper
  access, and journal settlement.
- `src/content.rs` — the bounded terminal byte buffer, the hex codec, the
  content-frame budget, and the `session_content` event spelling.
- `src/operator.rs` — the per-boot operator capability (`0600` file, mint,
  load, constant-time verify, redacted `Debug`).
- `src/audit_sink.rs` — the owner-private rotating audit file and the
  fail-closed admission gate.
- `src/service/scopes.rs` — the `session.*` / `daemon.*` scope registry and the
  frozen method catalog.
- `src/service/wire.rs` — the bounded v1 request/response envelopes and the
  static parameter helpers.
- `src/service/protocol.rs` — per-connection client id, effective scopes,
  leases, and pending control-token delivery.
- `src/service/grants.rs` — host-consent and observer grants.
- `src/service/frame.rs` — the per-frame `SCM_CREDENTIALS` reader.
- `src/service/dispatch.rs` — the eleven-stage authorization order and the
  closed refusal mapping.
- `src/service/server.rs` — endpoint admission, limits, signals, and shutdown.
- `src/seam.rs` — status reporting for the no-argument invocation.
- `src/cli.rs` — argument parsing, rendering, `start`, and the exit-code map.
- `src/main.rs` — thin binary entry point.
- `tests/dependency_direction.rs` — one-way dependency and crate-shape gate.
- `tests/binary.rs` — `--version`, no-argument seam, no runtime files, usage
  error.
- `tests/registry.rs` — public registry lifecycle, ids, refusals, limits.
- `tests/service_loop.rs` (`event_wire`) — real-socket delivery: response-first
  ordering, per-lease routing, the terminal marker, control-transfer detail,
  disconnect cleanup and replay retention, controller-token reclaim, the
  opt-out, the attach replay, a truncated ring, the kill terminal, terminal
  content read/write/subscribe, the observer refusal, the event opt-in refusal,
  the generation fence, typed write backpressure, subscriber disconnect
  cleanup, and content-free audit.
- `tests/events.rs` — public stream behavior: revision ordering, replay and
  truncation boundaries, the terminal guarantee, control frames, heartbeat
  memory stability, and the seeded 10,000-operation no-loss audit.
- `tests/limits.rs` — boundary triples for every cap, configuration refusals,
  the advertised shape, registry/orphan derivation, and whole-request refusal.
- `tests/idempotency.rs` — counting-executor replay, conflict, connection
  binding, TTL boundaries, capacity eviction, and hostile key streams.
- `tests/audit.rs` — overflow visibility, stable ids, structural content-free
  records, refusal representation, and capacity derivation.
- `tests/leases.rs` — public attach, claim, transfer (with target-addressed
  token delivery), expiry, detach, stop, and token-minimization behavior.
- `tests/orphan_journal.rs` — public journal reserve/commit/reap, no-signal
  paths, owner-private Unix files, and the no-argv/env record shape.
- `tests/endpoint.rs` — real Unix endpoint behavior: modes and owners, second
  instance, live-endpoint refusal, stale reclaim, symlink/non-socket refusal,
  path bound, replacement-safe shutdown, missing/replaced tamper re-check,
  lock-path types, shared-root refusal, lock retention on clean shutdown.
- `tests/peer.rs` — real cross-process peer capture and re-check by
  re-executing the test binary as the connecting child, plus the
  `unshare -Ur` user-namespace refusal.
- `tests/service_loop.rs` — the real binary: endpoint modes, second-start
  result, no TCP listener, operator negotiate, the exact baseline scope list,
  refusal without a capability oracle plus a real EOF inside the deadline,
  closing refusals (second negotiate, pre-negotiate frame, wire and service
  version), a stopping daemon refusing a pipelined create, the pre-auth byte
  budget and its aggregated record, the negotiate-deadline record, the
  consumed host grant revoke (fresh key and keyed replay), the vacant-control
  `InvalidControlToken` for every control method (stale, missing, occupied,
  invisible, and post-transfer old/rotated cases), the refusal-burst aggregate
  with suppressed count and histogram, the tamper-exit aggregate flush,
  core-dump hardening (`RLIMIT_CORE` and `/proc/<pid>/mem`),
  `SIGTERM`/`SIGINT` drain, endpoint tamper → exit 6, clean stop, the
  status-only invocation, and the bitty#2790 worker path: generation-1 bind,
  keyed replay and spec conflict, live-session orphan exclusion, controller vs
  observer resize, stale generation, spawn-failure rollback, and worker-exit
  journal settlement.
- `tests/frame_credentials.rs` — per-frame credential acceptance across a real
  process boundary and the fd-passing handoff (grandchild's frame refused).

## Gates

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p bitty-daemon --locked
cargo check --workspace --all-targets --locked
```

`just daemon-seam-test` runs the crate's suite; it is part of `just check`,
and the workspace `cargo nextest run --workspace` in CI discovers the same
tests. The endpoint, service-loop, and cross-process credential tests are
Linux-gated because Linux is the only servable cell in this slice; the macOS
and Windows builds compile the typed `UnsupportedPlatform` refusal paths
(including a `ServerError::UnsupportedPlatform` stub) and the cross-check gate
keeps them warning-free.
