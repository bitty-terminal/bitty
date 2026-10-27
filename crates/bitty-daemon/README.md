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

## Status: seam plus the in-memory state core, event stream, limits, idempotency, audit, and attach/control state machine

This crate fixes the service version, the capability set, and the
version-negotiation skeleton, implements the registry half of bitty#2601
(server-generated ids, the in-memory session registry, generation and revision
counters, retention, and the bounded orphan journal), the event stream of
bitty#2602 (snapshot plus bounded backlog, revision ordering, per-lease
routing, control-frame reservation, the terminal-event guarantee, and
heartbeat/last-seen), the compiled limits and advertised triples of bitty#2603,
the two bounded idempotency stores, the bounded content-free audit ring, and the
attach/control half of bitty#2609 (observer and controller leases, the 256-bit
control token, explicit transfer, detach/revocation, lease expiry, and the
abort/kill pipeline). It **starts no listener**, binds no endpoint, creates no
socket file, spawns no worker, signals no process, and serves no IPC method.
`bittyd` with no arguments still reports the no-endpoint seam and exits 0; the
state core, the event stream, the limit/idempotency/audit stores, and the lease
state machine are library surfaces for bitty#2607 to drive.

The contract areas that follow are deliberately deferred to their owning
slices:

| Area                                                                                                                                                                                                                                                    | Owner                |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------- |
| `bitty-ipc` v1 server loop, peer capture and re-check, scope evaluation, observer-grant consumption, endpoint lock and tamper detection, method dispatch, error mapping, event framing and correlation id, refusal-to-result mapping, audit sink writer | bitty#2607           |
| Observer-grant store (mint, single-use consume, TTL, peer binding, revoke)                                                                                                                                                                              | bitty#2609 follow-up |
| Endpoint discovery, cgroup/subreaper containment, signal delivery and reaping, production orphan reaper, executable path digest, non-Linux probes and Windows ACLs                                                                                      | bitty#2604           |
| Conformance corpus and fake-server parity                                                                                                                                                                                                               | bitty#2605           |

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
  `attach`/`detach` drive the derived attach count; `record_lease_mutation`
  bumps the revision for a transfer that keeps the count; `heartbeat` updates
  last-seen without a revision bump; `begin_stopping` and `terminate` apply one
  revision bump per snapshot-visible transition. Counters use checked
  arithmetic and fail closed.
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
  128-bit `attach_id`. A new observer at the cap is shed; existing leases are
  never evicted. An expired observer that the sweep has not removed yet does not
  consume a slot (the sweep still reclaims its record), and
  `ControlView::attach_count` counts only live leases.
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
  `record_resize` renew.
- Expiry and renewal are driven by the injected `MonotonicMs`; a lease is live
  before `expires_at` and expired exactly at it. Expiry detaches only: the
  worker keeps running, the generation is unchanged, and a controller expiry
  leaves control vacant with the current token still reclaimable. `expire_due`
  reports each removal with the post-detach `state_revision` the event slice
  needs. A claim or transfer that vacates an already-expired controller commits
  that due detach first, so it advances the revision twice; a later counter fault
  refuses the operation but leaves the due detach in place.
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
  refusal counts, and optional terminal classification.
- There is no `String`, `Vec`, `Box`, payload, argv, environment, clipboard,
  prompt, transcript, raw `terminal_id`, control token, or caller label field. A create
  or terminal transition uses `AuditTarget::terminal_digest`; a refusal carries
  its static `RefusalCode` and numeric `AuditCounts` only.
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
  per-connection refusal budget stay with bitty#2607.

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

## CLI

```text
bittyd --version   # crate version + contract service API, exit 0
bittyd --help      # usage, exit 0
bittyd             # seam status (no endpoint bound), exit 0
bittyd <other>     # usage error, exit 2
```

Exit codes 3 through 6 are reserved by the contract for `daemon.stop` modes and
endpoint tamper. The seam cannot reach them: it hosts no session and binds no
endpoint.

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
- `src/seam.rs` — explicit no-listener/no-endpoint status.
- `src/cli.rs` — pure argument parsing and rendering, exit codes.
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

## Gates

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p bitty-daemon --locked
cargo check --workspace --all-targets --locked
```

`just daemon-seam-test` runs the crate's suite; it is part of `just check`,
and the workspace `cargo nextest run --workspace` in CI discovers the same
tests.The endpoint and cross-process peer tests are Linux-gated because Linux
is the only servable cell in this slice; the macOS and Windows builds compile
the typed `UnsupportedPlatform` refusal paths and the cross-check gate keeps
them warning-free.
