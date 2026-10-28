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

## Status: service loop, scopes, handshake, grants, audit sink, and peer re-check

This crate fixes the service version, the capability set, and the version
negotiation, implements the registry half of bitty#2601 (server-generated ids,
the in-memory session registry, generation and revision counters, retention,
and the bounded orphan journal), the event stream of bitty#2602 (snapshot plus
bounded backlog, revision ordering, per-lease routing, control-frame
reservation, the terminal-event guarantee, and heartbeat/last-seen), the
compiled limits and advertised triples of bitty#2603, the two bounded
idempotency stores, the bounded content-free audit ring, the attach/control
half of bitty#2609, and the bitty#2607 service surface: the `session.*` /
`daemon.*` scope registry, `daemon.negotiate`, the eleven-stage authorization
order, host-consent and observer grants, the owner-private audit sink, the
`bittyd start` admission loop, per-frame `SCM_CREDENTIALS`, and the peer
re-check at every privileged boundary.

`bittyd start` binds the per-user endpoint (`0700` directory, `0600` socket,
single-instance lock), mints the per-boot operator capability, accepts up to
`max_connections_per_endpoint` connections, serves the bitty-ipc v1 envelope,
and stops on `daemon.stop` or a `SIGTERM`/`SIGINT` drain. Plain `bittyd`
creates no runtime file, binds no endpoint, and prints the status. The daemon
still spawns no worker, signals no process, and opens no TCP listener.

The contract areas that are deliberately deferred, and the typed answer the
running service gives today:

| Area | Owner | Answer today |
| --- | --- | --- |
| Worker spawn and bind (`session.create` registers a `created` session; nothing binds generation 1) | bitty#2604 | `session.create` returns the `created` snapshot with `generation: 0`; abort/kill/stop complete a pre-bind cancel with `not_started` |
| cgroup/subreaper containment, signal delivery and reaping, production orphan reaper, executable path digest, non-Linux probes and Windows ACLs | bitty#2604 | `daemon.orphans.verify` is `Denied/UnsupportedCapability`; no orphan is claimed verified |
| Terminal content plane, including `session.resize` (`terminal.manage`) | bitty#2604 / terminal contract | the daemon grants no `terminal.*` scope, so `session.resize` is `Denied/ScopeViolation` |
| Pushing session events on a connection (no subscribe method; the event stream owns its own registry) | bitty#2602 | `session.status`/`session.list` are the reconciliation reads; no event frame is sent |
| Per-connection refusal-audit aggregation | bitty#2603 follow-up | wire-level refusals are budgeted and aggregated (see below); method refusals remain one individual record each |
| Conformance corpus and fake-server parity | bitty#2605 | `tests/harness` holds the corpus, the reference fake, the real-service driver, and the parity gate; V-07 (bitty#2768) and C-04 (bitty#2767) are recorded known defects |

Nothing here authorizes autostart, a service file, a TCP listener, or desktop
wiring; all four stay out of scope.

## Frozen contract values

Source: contract decisions `D-01`, `D-08`, `D-11`, and the "Handshake" section
of the service surface.

| Item                 | Value                                                                                                                            | Where                           |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------------- | ------------------------------- |
| Service API version  | `{major: 1, minor: 0}`                                                                                                           | `contract::SERVICE_API_VERSION` |
| Minimum client minor | `0` (`1.0..=1.0` accepted)                                                                                                       | `contract::MIN_CLIENT_MINOR`    |
| Capabilities         | `terminal_sessions`, `events`, `observer_grants`, `control_transfer`, `idempotent_mutations` enabled; `worker_sessions` disabled | `contract::CAPABILITIES`        |
| Handshake method     | `daemon.negotiate`                                                                                                               | `contract::NEGOTIATE_METHOD`    |
| Negotiation rule     | same major, client minor in `[min_client_minor, server_minor]`; every other case is `VersionMismatch` before any operation       | `handshake::negotiate`          |
| Limits advertisement | every `D-14` row plus the inherited `RC-9`/`RC-10` bounds as `{effective, default, system_max}`                                  | `handshake::NegotiateResponse`  |
| Worker sessions      | `false` in every v0.2.0 build (`D-01`); enabling needs an ADR 0019 amendment and a dedicated security review                     | `contract::CAPABILITIES`        |

The enabled capability bits describe the v1 contract surface the crate is being
built toward, not delivered method behavior: this slice still binds no endpoint
and serves no method, so nothing can consume `events`,
`observer_grants`, `control_transfer`, or `idempotent_mutations` until
bitty#2607/#2602/#2603 land. Do not cite the bit set as evidence that a
capability works.

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
  transition. A terminal session has no live lease, so the lease layer detaches
  every lease before it terminates and the frozen record reports no attaches.
  Counters use checked arithmetic and fail closed.
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
`reserved` record (session id, generation, boot id, reserved-at) before the
spawn; `commit` fills the exact pid, process start time, and process group after
the spawn returns and fsyncs again, before the worker can be exposed as bound.
The journal holds at most `max_orphan_records` records of at most
`orphan_record_bytes`, contains no argv, environment, cwd, content, or
credential, and on Unix lives in a `0700` directory with `0600` files.
`reserve` validates the worst-case committed encoding (maximum pid, start
time, and process-group widths, plus a later quarantine timestamp), so a
reservation never precedes a spawn unless `commit` can hold any valid
identity.

Classification is conservative and never signals by pid:

| Observation                                                          | Outcome                                    |
| -------------------------------------------------------------------- | ------------------------------------------ |
| Exact boot id, pid, start time, and process group                    | only this reaches `OrphanReaper`           |
| Different boot id                                                    | `stale`, discarded, never signalled        |
| Reserved record with no pid                                          | `unverified`, quarantined, never signalled |
| Absent pid, unreadable start time, reused start time, group mismatch | `unverified`, quarantined, never signalled |
| Reaper cannot verify the whole containment identity                  | `unverified`, quarantined                  |

Quarantined records expire after `orphan_record_retention_ms`; operator
retirement removes a quarantined record explicitly and never signals. Boot
comparison precedes quarantine stickiness: a record quarantined under a
previous boot id is stale and removed immediately, without waiting for
retention. `VerifiedOrphan` has a private constructor, so a reaper can only be
asked to terminate an exact match.

Windows: the journal compiles and enforces the same record bound, but this slice
does not create a current-user-only DACL and has no boot-id/`/proc` probe.
Until bitty#2604 supplies those platform cells, opening a journal requires a
caller-supplied boot id and every record classifies as unverified, so nothing is
signalled. The journal inherits the ACL of the daemon-owned runtime directory;
bitty#2604 owns that directory and its current-user-only DACL.

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
listed in the module docs and arrive with bitty#2601/#2607. `client_correlation`
is bounded to 128 bytes of valid UTF-8 with no control characters and is
recorded only as a truncated 128-bit SHA-256 digest (`D-08`);
`operator_capability` and `host_grant` stay out of this crate until the slice
that owns their secret lifetime and storage exists.

## Event stream (bitty#2602)

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
  `{effective, default, system_max}`; `daemon.status` queue/backlog counters
  arrive with bitty#2607, and bitty#2605 adds the V-21, V-23, C-04, and C-06
  corpus vectors against a real transport.

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
| Boundaries           | `session.create`, controller attach, abort, kill, transfer, detach-other, grant mint/revoke, `daemon.stop`                                                                                | `peer::DispatchBoundary`              |
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
| Idle bound             | `transport_idle_ms`                              | an idle connection is closed                                                                                            |
| Outbound bound         | `max_connection_outbound_bytes`                  | a response queue above the bound closes the connection                                                                  |
| Grant sweep            | `GRANT_SWEEP_INTERVAL_MS` (1 s)                  | retired grant records are swept on the loop timer                                                                       |
| Close drain            | `CLOSE_DRAIN_TIMEOUT_MS` (1 s)                   | a retired connection that cannot drain its refusal frame is closed; it never collects a second handshake-timeout record |
| Tamper interval        | `TAMPER_INTERVAL_MS` (5 s)                       | `BoundEndpoint::reattest`; a failure is exit code 6                                                                     |

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
partial response. A refusal that
[`Refusal::closes_connection`](../src/service/dispatch.rs) marks (`VersionMismatch`,
`Unauthenticated`, `Denied/AlreadyNegotiated`) keeps its frame queued, stops
reading, and shuts the stream down only after the queued bytes drain.

The loop never opens a TCP listener. `tests/service_loop.rs` snapshots the
namespace's `LISTEN` rows in `/proc/net/tcp` and `/proc/net/tcp6` around
`bittyd start` and fails on any added listener; the daemon is non-dumpable, so
per-process fd attribution is no longer available to the test.

### Scopes and method catalog

`service::scopes` is the bittyd-owned registry. It reuses the `bitty-ipc-api`
method grammar, so:

- a malformed name is `InvalidMethod`;
- a well-formed name outside the catalog is `NotFound` (deny-by-default), and
  the v1 families stay owned by `bitty-ipc` (a test proves the catalog never
  redefines one of the 51 v1 methods);
- the only transport-scope requirement is `session.resize` → `terminal.manage`,
  and the daemon grants no `terminal.*` scope in this slice.

| Scope | Baseline | Grants |
| --- | --- | --- |
| `session.inspect` | yes | metadata status and list of sessions with a live lease |
| `session.host` | **no** | create a session; only a consumed host grant adds it |
| `session.attach` | yes | accept an observer grant |
| `session.control` | yes | control relation actions (claim, grant, abort, transfer, revoke) |
| `daemon.inspect` | no | operator status and all-session view |
| `daemon.control` | no | stop, host grants, orphan verify, operator kill |

The connection's transport set is **only** `process.spawn`, the one transport
scope the daemon catalog evaluates (`session.create`'s two-authority rule). It
is not the `bitty-ipc` CLI default: the daemon grants no `terminal.*`, `view.*`,
`config.*`, `plugin.*`, or `debug.*` scope, so the handshake never advertises an
ambient content scope. The daemon spawns nothing yet, so `process.spawn` has no
executable to gate; `session.host` consent is the effective create authority.
When the spawn path lands (bitty#2604), the grant must come from the executable
policy instead of this transport default. The four create-authority
combinations are unit-tested: both missing is `ScopeViolation`, consent missing
with spawn present is `ConsentRequired`, spawn missing with consent present is
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

### Handshake

`daemon.negotiate` is the only pre-authentication method. The request carries
`client_version`, optional `client_correlation`, optional
`operator_capability`, and optional `host_grant`. The response carries
`wire_version`, `service_api_version`, `daemon_instance_id`, `daemon_pid`,
`server_identity`, `peer_identity`, `client_id`, `scopes`, `capabilities`,
`limits` (`{effective, default, system_max}` for every row), `server_monotonic_ms`,
and `daemon_state`.

- `client_correlation` is ≤ 128 bytes with no control characters, hashed on
  receipt; only the truncated 128-bit digest is audited and the raw value never
  survives parsing.
- A present but invalid `operator_capability` is one static
  `Unauthenticated` refusal that closes the connection. Verification is an
  in-memory constant-time compare; the request path never reads the capability
  file, so a missing file and a wrong bearer are indistinguishable.
- A `host_grant` that cannot be consumed adds no scope and does not close the
  connection; `session.create` then returns `Denied/ConsentRequired`.
- A version outside `1.0..=1.0` is `VersionMismatch` and closes. A second
  negotiate, or any frame read before the negotiate request, is
  `Denied/AlreadyNegotiated` and closes after the refusal frame is flushed.
  The gate runs after the per-frame wire version and before the rate limiter,
  the method grammar, and the catalog, so an unnegotiated connection cannot
  probe which methods exist. A frame that
  is already in the socket buffer when a successful negotiate is processed is
  served after the handshake (the server does not discard pipelined bytes);
  it can never widen the connection's scopes, and there is still exactly one
  handshake per connection.
- Envelope-level refusals that never resolve to a catalog method (malformed
  envelope or id, unsupported wire version, unknown or malformed method,
  rate limit before parsing, frame or deadline closure) are one `wire` refusal
  record each, subject to the per-connection budget below.

### Grants

`service::grants` holds host-consent and observer grants: 256-bit bearer,
non-secret 128-bit `grant_id`, peer-uid binding, TTL from `host_grant_ttl_ms` /
`observer_grant_ttl_ms`, pending caps (`max_pending_host_grants`,
`max_pending_observer_grants_per_session`), single use, and idempotent revoke
by `grant_id`. Missing, expired, consumed, revoked, wrong-session, and
wrong-peer cases are one `Unusable` refusal. The bearer is retained only while
the grant is live so a keyed `session.grant` replay can return the same
unconsumed bearer; it is cleared on consume and revoke. A mint at the cap is
`LimitExceeded`.

The store is bounded by more than the active caps. The server loop sweeps
retired records on `GRANT_SWEEP_INTERVAL_MS`, a consumed host grant is retained
until its TTL because `host_grant_bound` still authorizes the consuming
connection's create, every other inactive record is dropped immediately, and
`MAX_GRANT_RECORDS` (1,024) is the hard total bound checked at mint (after the
sweep) so the store cannot grow with total mints. A mint at the total bound is a
whole `LimitExceeded` refusal and never evicts a live record.

A consumed host grant stays revocable until its TTL: `daemon.host_grant`
`revoke` marks the record revoked and clears any retained bearer, so the
consuming connection's next `session.create` — including a keyed replay of a
key already applied on that connection — is `Denied/ConsentRequired`
immediately rather than at the 300 s TTL. The record is still retained until
the TTL so the bound connection sees the false `host_grant_bound` answer.
Observer grants keep the unconsumed-and-unexpired revoke rule.

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
  path, or (for a former leaseholder) the immutable terminal snapshot; a
  presented token at a live, vacant control that is not current maps to
  `Denied/InvalidControlToken`, while an occupied, terminal, or unknown target
  stays the uniform `NotFound` and no refusal echoes the token;
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
`session.abort`, `session.kill`, `lease.heartbeat`.

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
resolves to a method. Non-closing wire refusals are budgeted per connection at
`refusal_audit_budget_per_sec` records per second. A window that suppresses
refusals ends with exactly one aggregate `wire` record carrying
`sup=<suppressed>`, `bl=<budget>`, and a bounded static code histogram
(`h=code:count,...`, with `ho=<count>` for codes past the
`MAX_AGGREGATE_CLASSES` cap); a closing connection flushes its pending window
so no suppression is lost. The aggregate is fixed-size and cannot rotate the
segments by itself, so a refusal flood stays bounded without becoming
invisible.

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
  grant mint/consume/revoke/transfer, abort, kill, heartbeat, stop, and the
  closed refusal set over an in-memory transport, with injected time and
  limits. It never claims a real process.
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
- a vector that does not run on a driver carries a typed skip reason;
- `known_defect` rows (currently V-07/bitty#2768 and C-04/bitty#2767) record the
  exact failing step and code or result path, are excluded from the parity
  gate, and fail the suite if they pass or fail at another step so the marker
  cannot rot;
- fault injection covers a transport drop during abort, the pre-auth partial
  frame, the frame bound, the pending grant cap, the attach cap, and an
  instance change (restart); queue pressure with an injected outbound cap is
  a fake-only row because the served loop has no clock/limit injection point.

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
tamper. `3`, `4`, and `5` are the contract's `daemon.stop` outcome codes; this
slice has no worker to drain, so a clean stop is `0`.

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
  queues with the control reservation, the enforced limits, and the per-session
  stream.
- `src/lease/` — the control token and digest, lease records and limits, the
  lease/control/stop state machine, and its closed refusals.
- `src/orphan/` — process identity probes, the bounded journal, classification,
  and the exact-identity reaper seam.
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
  `InvalidControlToken` and its occupied/invisible boundaries, the refusal-burst
  aggregate with suppressed count and histogram, core-dump hardening
  (`RLIMIT_CORE` and `/proc/<pid>/mem`), `SIGTERM`/`SIGINT` drain, endpoint
  tamper → exit 6, clean stop, and the status-only invocation.
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
