//! The in-process reference fake server: `candidate-not-bittyd`.
//!
//! It implements the published surface (handshake, create, status, list,
//! attach, detach, abort, kill, heartbeat, stop, grants, and the closed refusal
//! set) over an in-memory transport. Every run is labeled
//! `candidate-not-bittyd`; a fake row never marks a real row passed.
//!
//! Time and limits are injected, so the vectors that need a clock or a small
//! cap exercise it here. The compiled limit table is the real crate's, so the
//! advertised triples cannot drift from the service under test; the fake's
//! behavior is independent.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use bitty_daemon::contract::CAPABILITIES;
use bitty_daemon::handshake::{ClientCorrelation, GrantCorrelation, hex};
use bitty_daemon::limits::{LimitId, LimitOverride, Limits};
use bitty_daemon::spawn::{SpawnSpec, SpawnSpecError};

use crate::harness::corpus::{DriverKind, Vector};
use crate::harness::evidence;
use crate::harness::runner::{
    Driver, ErrorReply, ExitObservation, Outcome, Reply, RequestRecord, ServiceFacts, Vars,
    cell_vars,
};

const BASELINE: [&str; 5] = [
    "session.inspect",
    "session.attach",
    "session.control",
    "terminal.manage",
    "process.spawn",
];

/// Whether the reference cell models a served worker plane.
///
/// The fake is the contract reference, not a mirror of the host: it serves the
/// worker create/resize surface on every platform, so it must advertise that
/// surface on every platform too. The real driver seeds the compiled cell truth
/// instead (Linux only in this slice).
const FAKE_WORKER_SESSIONS: bool = true;

/// The fake server driver.
pub struct FakeDriver {
    limits: Limits,
    clock_ms: u64,
    id_seq: u64,
    instance: String,
    operator_capability: String,
    host_grant: Option<String>,
    needs_host_grant: bool,
    state: &'static str,
    sessions: BTreeMap<String, FakeSession>,
    grants: BTreeMap<String, FakeGrant>,
    connections: BTreeMap<String, FakeConn>,
    create_keys: BTreeMap<String, (String, String)>,
    /// Keyed observer-grant mints: key -> (grant id, TTL/binding fingerprint).
    grant_keys: BTreeMap<String, (String, String)>,
    deliveries: BTreeMap<String, String>,
    detached: BTreeMap<String, (String, String)>,
    requests: Vec<RequestRecord>,
    audit: Vec<String>,
    facts: ServiceFacts,
}

#[derive(Default)]
struct FakeConn {
    negotiated: bool,
    scopes: BTreeSet<String>,
    leases: BTreeSet<String>,
    host_grant: Option<String>,
    pending: BTreeMap<String, String>,
    /// The 32-hex `client_correlation_digest` this connection negotiated.
    correlation: Option<String>,
    /// The client asked for pushed events at negotiate (`D-10`).
    events: bool,
    /// Last revision handed to the client per lease. The reference fake models
    /// the stream by revision instead of queueing: the wire rule is one event
    /// per revision, in order, routed per lease.
    event_cursor: BTreeMap<String, u64>,
    closed: bool,
}

struct FakeSession {
    id: String,
    kind: &'static str,
    terminal_id: String,
    state: &'static str,
    generation: u64,
    revision: u64,
    controller: Option<String>,
    token: String,
    initial_token: Option<String>,
    leases: BTreeMap<String, FakeLease>,
    created_ms: u64,
    last_seen_ms: u64,
    terminal_reason: Option<&'static str>,
    cleanup: Option<&'static str>,
}

#[derive(Clone)]
struct FakeLease {
    role: &'static str,
    expires_ms: u64,
}

struct FakeGrant {
    id: String,
    session: String,
    kind: &'static str,
    expires_ms: u64,
    consumed: bool,
    revoked: bool,
    /// Optional 32-hex correlation digest the consuming connection must
    /// present.
    correlation: Option<String>,
}

impl FakeDriver {
    pub fn new() -> Self {
        Self {
            limits: Limits::default(),
            clock_ms: 0,
            id_seq: 0,
            instance: String::new(),
            operator_capability: String::new(),
            host_grant: None,
            needs_host_grant: false,
            state: "running",
            sessions: BTreeMap::new(),
            grants: BTreeMap::new(),
            connections: BTreeMap::new(),
            create_keys: BTreeMap::new(),
            grant_keys: BTreeMap::new(),
            deliveries: BTreeMap::new(),
            detached: BTreeMap::new(),
            requests: Vec::new(),
            audit: Vec::new(),
            facts: ServiceFacts::default(),
        }
    }

    fn reset(&mut self, vector: &Vector) -> Result<(), String> {
        let mut overrides = Vec::new();
        for (name, value) in &vector.injected.limits {
            let id =
                LimitId::from_name(name).ok_or_else(|| format!("unknown injected limit {name}"))?;
            overrides.push(LimitOverride::new(id, *value));
        }
        self.limits = Limits::new(&overrides).map_err(|_| "invalid injected limits".to_owned())?;
        self.clock_ms = vector.injected.clock_ms;
        self.id_seq = 0;
        self.instance = self.mint_id();
        self.operator_capability = self.mint_token();
        self.needs_host_grant = vector.host_grant;
        self.state = "running";
        self.sessions.clear();
        self.grants.clear();
        self.connections.clear();
        self.create_keys.clear();
        self.grant_keys.clear();
        self.deliveries.clear();
        self.detached.clear();
        self.requests.clear();
        self.audit.clear();
        self.host_grant = self.needs_host_grant.then(|| self.mint_host_grant());
        self.facts = self.facts_now();
        Ok(())
    }

    /// The advertised value of one capability for this reference cell.
    ///
    /// The worker bit is fixed to the modeled served cell; every other bit is
    /// the compiled contract value.
    fn capability(name: &str, enabled: bool) -> bool {
        if name == "worker_sessions" {
            FAKE_WORKER_SESSIONS
        } else {
            enabled
        }
    }

    fn facts_now(&self) -> ServiceFacts {
        ServiceFacts {
            instance_id: self.instance.clone(),
            cell: evidence::cell(),
            version: bitty_daemon::CRATE_VERSION.to_owned(),
            capabilities: CAPABILITIES
                .all()
                .into_iter()
                .map(|(name, enabled)| (name.to_owned(), Self::capability(name, enabled)))
                .collect(),
            limits: LimitId::ALL
                .into_iter()
                .map(|limit| (limit.name().to_owned(), self.limits.effective(limit)))
                .collect(),
        }
    }

    fn seeds(&self) -> Vars {
        let mut vars = Vars::new();
        vars.insert(
            "operator_capability".to_owned(),
            Value::String(self.operator_capability.clone()),
        );
        if let Some(grant) = &self.host_grant {
            vars.insert("host_grant".to_owned(), Value::String(grant.clone()));
        }
        cell_vars(&mut vars, FAKE_WORKER_SESSIONS);
        vars
    }

    fn mint_id(&mut self) -> String {
        self.id_seq += 1;
        format!("{:032x}", self.id_seq)
    }

    fn mint_token(&mut self) -> String {
        self.id_seq += 1;
        format!("{:064x}", self.id_seq)
    }

    fn mint_host_grant(&mut self) -> String {
        let ttl = self.limits.effective(LimitId::HostGrantTtlMs);
        self.mint_host_grant_with(ttl, None)
    }

    fn mint_host_grant_with(&mut self, ttl_ms: u64, correlation: Option<String>) -> String {
        let id = self.mint_id();
        let bearer = self.mint_token();
        let expires = self.now() + ttl_ms;
        self.grants.insert(
            bearer.clone(),
            FakeGrant {
                id,
                session: String::new(),
                kind: "host",
                expires_ms: expires,
                consumed: false,
                revoked: false,
                correlation,
            },
        );
        bearer
    }

    fn now(&self) -> u64 {
        self.clock_ms
    }

    fn connection(&mut self, name: &str) -> &mut FakeConn {
        self.connections.entry(name.to_owned()).or_default()
    }

    fn conn(&self, name: &str) -> Option<&FakeConn> {
        self.connections.get(name)
    }

    fn audit_line(&mut self, method: &str, result: &str) {
        self.audit
            .push(format!("v1 i={} a={method} r={result}", self.instance));
    }

    fn reply(&mut self, method: &str, value: Value) -> Outcome {
        self.audit_line(method, "ok");
        Outcome::Reply(Reply {
            ok: true,
            result: value,
            error: None,
        })
    }

    fn refuse(
        &mut self,
        method: &str,
        class: &str,
        code: &str,
        counts: Option<(u64, u64)>,
    ) -> Outcome {
        self.audit_line(method, &format!("refused c={code}"));
        Outcome::Reply(Reply {
            ok: false,
            result: Value::Null,
            error: Some(ErrorReply {
                class: class.to_owned(),
                code: code.to_owned(),
                counts,
            }),
        })
    }

    fn call_inner(&mut self, connection: &str, method: &str, params: Value) -> Outcome {
        if !self.connections.contains_key(connection) {
            self.connections
                .insert(connection.to_owned(), FakeConn::default());
        }
        if self.conn(connection).is_some_and(|conn| conn.closed) {
            return Outcome::Closed;
        }
        self.sweep_expired();
        self.requests.push(RequestRecord {
            method: method.to_owned(),
            params: params.clone(),
        });
        if method == "daemon.negotiate" {
            return self.negotiate(connection, &params);
        }
        if !self.conn(connection).is_some_and(|conn| conn.negotiated) {
            let outcome = self.refuse(method, "Denied", "AlreadyNegotiated", None);
            self.connection(connection).closed = true;
            return outcome;
        }
        if !well_formed(method) {
            return self.refuse(method, "Validation", "InvalidMethod", None);
        }
        let outcome = match method {
            "daemon.status" => self.daemon_status(connection),
            "daemon.stop" => self.daemon_stop(connection, &params),
            "daemon.host_grant" => self.daemon_host_grant(connection, &params),
            "daemon.orphans.verify" => self.daemon_orphans_verify(connection, &params),
            "session.create" => self.session_create(connection, &params),
            "session.status" => self.session_status(connection, &params),
            "session.list" => self.session_list(connection),
            "session.attach" => self.session_attach(connection, &params),
            "session.grant" => self.session_grant(connection, &params),
            "session.grant.revoke" => self.session_grant_revoke(connection, &params),
            "session.detach" => self.session_detach(connection, &params),
            "session.control.transfer" => self.session_transfer(connection, &params),
            "session.resize" => self.session_resize(connection, &params),
            "session.abort" => self.session_abort(connection, &params),
            "session.kill" => self.session_kill(connection, &params),
            "lease.heartbeat" => self.lease_heartbeat(connection, &params),
            _ => self.refuse(method, "NotFound", "NotFound", None),
        };
        self.route(connection);
        outcome
    }

    fn negotiate(&mut self, connection: &str, params: &Value) -> Outcome {
        if self.conn(connection).is_some_and(|conn| conn.negotiated) {
            let outcome = self.refuse("daemon.negotiate", "Denied", "AlreadyNegotiated", None);
            self.connection(connection).closed = true;
            return outcome;
        }
        let major = params
            .get("client_version")
            .and_then(|value| value.get("major"))
            .and_then(Value::as_u64);
        let minor = params
            .get("client_version")
            .and_then(|value| value.get("minor"))
            .and_then(Value::as_u64);
        if major != Some(1) || !matches!(minor, Some(0 | 1)) {
            let outcome = self.refuse("daemon.negotiate", "Validation", "VersionMismatch", None);
            self.connection(connection).closed = true;
            return outcome;
        }
        let operator = params
            .get("operator_capability")
            .and_then(Value::as_str)
            .is_some_and(|value| value == self.operator_capability);
        if params.get("operator_capability").is_some() && !operator {
            let outcome = self.refuse(
                "daemon.negotiate",
                "Unauthenticated",
                "Unauthenticated",
                None,
            );
            self.connection(connection).closed = true;
            return outcome;
        }
        let correlation = match params.get("client_correlation").and_then(Value::as_str) {
            Some(raw) => match ClientCorrelation::parse(raw) {
                Ok(correlation) => Some(hex(&correlation.digest())),
                Err(_) => {
                    let outcome =
                        self.refuse("daemon.negotiate", "Validation", "InvalidRequest", None);
                    self.connection(connection).closed = true;
                    return outcome;
                }
            },
            None => None,
        };
        {
            let conn = self.connection(connection);
            conn.negotiated = true;
            conn.scopes = BASELINE.iter().map(|scope| (*scope).to_owned()).collect();
            conn.correlation = correlation.clone();
            conn.events = params
                .get("events")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if operator {
                conn.scopes.insert("daemon.inspect".to_owned());
                conn.scopes.insert("daemon.control".to_owned());
            }
        }
        if let Some(bearer) = params.get("host_grant").and_then(Value::as_str) {
            let now = self.now();
            let valid = self.grants.get(bearer).is_some_and(|grant| {
                grant.kind == "host"
                    && !grant.consumed
                    && !grant.revoked
                    && grant.expires_ms > now
                    && correlation_accepts(grant.correlation.as_deref(), correlation.as_deref())
            });
            if valid {
                if let Some(grant) = self.grants.get_mut(bearer) {
                    grant.consumed = true;
                }
                let conn = self.connection(connection);
                conn.scopes.insert("session.host".to_owned());
                conn.host_grant = Some(bearer.to_owned());
            }
        }
        let client_id = self.mint_id();
        let mut scopes: Vec<String> = [
            "session.inspect",
            "session.host",
            "session.attach",
            "session.control",
            "daemon.inspect",
            "daemon.control",
            "terminal.manage",
            "process.spawn",
        ]
        .into_iter()
        .filter(|scope| self.has_scope(connection, scope))
        .map(str::to_owned)
        .collect();
        // The service-scope catalog order, then the transport scopes, exactly
        // as `EffectiveScopes::names` renders them.
        scopes.sort_by_key(|scope| match scope.as_str() {
            "session.inspect" => 0,
            "session.host" => 1,
            "session.attach" => 2,
            "session.control" => 3,
            "daemon.inspect" => 4,
            "daemon.control" => 5,
            "terminal.manage" => 6,
            "process.spawn" => 7,
            _ => 8,
        });
        let limits: Vec<Value> = LimitId::ALL
            .into_iter()
            .map(|limit| {
                json!({
                    "name": limit.name(),
                    "effective": self.limits.effective(limit),
                    "default": limit.default_value(),
                    "system_max": limit.system_max_value(),
                })
            })
            .collect();
        let capabilities: BTreeMap<String, bool> = CAPABILITIES
            .all()
            .into_iter()
            .map(|(name, enabled)| (name.to_owned(), Self::capability(name, enabled)))
            .collect();
        let value = json!({
            "wire_version": 1,
            "service_api_version": {"major": 1, "minor": 1, "min_client_minor": 0},
            "daemon_instance_id": self.instance,
            "daemon_pid": 4242,
            "server_identity": {
                "uid": 1000,
                "endpoint_kind": "unix_socket",
                "endpoint_owner": 1000,
                "endpoint_mode": 384,
            },
            "peer_identity": {"uid": 1000, "pid": 4242, "start_time": 1},
            "client_id": client_id,
            "scopes": scopes,
            "capabilities": capabilities,
            "limits": limits,
            "server_monotonic_ms": self.now(),
            "daemon_state": self.state,
        });
        self.reply("daemon.negotiate", value)
    }

    /// Detach every lease past its TTL and retire every grant past its TTL.
    ///
    /// A lease detach is one revision bump each. An observer grant that reaches
    /// its TTL without being consumed or revoked is one bump for its session,
    /// however many of its grants expired in the same sweep (`D-03`); a
    /// consumed or revoked record was already accounted for by its own bump,
    /// and a host grant is daemon-level consent with no owning session.
    fn sweep_expired(&mut self) {
        let now = self.now();
        let mut expired = Vec::new();
        for (session_id, session) in &self.sessions {
            for (attach, lease) in &session.leases {
                if lease.expires_ms <= now {
                    expired.push((session_id.clone(), attach.clone()));
                }
            }
        }
        for (session_id, attach) in expired {
            let mut removed = false;
            if let Some(session) = self.sessions.get_mut(&session_id) {
                removed = session.leases.remove(&attach).is_some();
                if removed {
                    session.revision += 1;
                    if session.controller.as_deref() == Some(attach.as_str()) {
                        session.controller = None;
                    }
                }
            }
            if removed {
                for conn in self.connections.values_mut() {
                    conn.leases.remove(&attach);
                }
                self.deliveries.remove(&attach);
            }
        }

        // Retired records leave the table; only a TTL lapse is a visible
        // mutation. The deduplicated set keeps the bump at one per session.
        let retired: Vec<String> = self
            .grants
            .iter()
            .filter(|(_, grant)| grant.expires_ms <= now)
            .map(|(bearer, _)| bearer.clone())
            .collect();
        let mut expired_sessions = BTreeSet::new();
        for bearer in retired {
            if let Some(grant) = self.grants.remove(&bearer) {
                if grant.kind == "observer" && !grant.consumed && !grant.revoked {
                    expired_sessions.insert(grant.session);
                }
            }
        }
        for session_id in expired_sessions {
            if let Some(session) = self.sessions.get_mut(&session_id) {
                if !is_terminal(session.state) {
                    session.revision += 1;
                }
            }
        }
    }

    fn has_scope(&self, connection: &str, scope: &str) -> bool {
        self.conn(connection)
            .is_some_and(|conn| conn.scopes.contains(scope))
    }

    fn session_visible(&self, connection: &str, session_id: &str, token: Option<&str>) -> bool {
        if self.has_scope(connection, "daemon.inspect") {
            return true;
        }
        let now = self.now();
        let owns = self.conn(connection).is_some_and(|conn| {
            conn.leases.iter().any(|attach| {
                self.sessions
                    .get(session_id)
                    .and_then(|session| session.leases.get(attach))
                    .is_some_and(|lease| lease.expires_ms > now)
            })
        });
        if owns {
            return true;
        }
        match token {
            Some(token) => self
                .sessions
                .get(session_id)
                .is_some_and(|session| session.token == token),
            None => false,
        }
    }

    /// The real service's control-method visibility mapping.
    ///
    /// A live vacant control with a missing, stale, or rotated token is
    /// `Denied/InvalidControlToken`; an unknown, terminal, or occupied target
    /// keeps the uniform `NotFound`.
    fn control_visible(
        &self,
        connection: &str,
        session_id: &str,
        token: Option<&str>,
    ) -> Result<(), (&'static str, &'static str)> {
        if self.session_visible(connection, session_id, token) {
            return Ok(());
        }
        if self
            .sessions
            .get(session_id)
            .is_some_and(|session| !is_terminal(session.state) && session.controller.is_none())
        {
            Err(("Denied", "InvalidControlToken"))
        } else {
            Err(("NotFound", "NotFound"))
        }
    }

    fn session_json(session: &FakeSession) -> Value {
        let mut value = json!({
            "session_id": session.id,
            "kind": session.kind,
            "terminal_id": session.terminal_id,
            "state": session.state,
            "generation": session.generation,
            "state_revision": session.revision,
            "attach_count": session.leases.len(),
            "attached": !session.leases.is_empty(),
            "created_at_ms": session.created_ms,
            "last_seen_ms": session.last_seen_ms,
        });
        if matches!(session.state, "exited" | "failed" | "terminated") {
            value["terminal_reason"] = json!(session.terminal_reason);
            value["cleanup"] = json!(session.cleanup);
        }
        value
    }

    fn daemon_status(&mut self, connection: &str) -> Outcome {
        if !self.has_scope(connection, "daemon.inspect") {
            return self.refuse("daemon.status", "Denied", "ScopeViolation", None);
        }
        let active = self.active_sessions();
        let limits: Vec<Value> = LimitId::ALL
            .into_iter()
            .map(|limit| {
                json!({
                    "name": limit.name(),
                    "effective": self.limits.effective(limit),
                    "default": limit.default_value(),
                    "system_max": limit.system_max_value(),
                })
            })
            .collect();
        self.reply(
            "daemon.status",
            json!({
                "daemon_instance_id": self.instance,
                "daemon_state": self.state,
                "active_sessions": active,
                "sessions": {
                    "created": active,
                    "running": 0,
                    "stopping": 0,
                    "terminal": self.sessions.len() - active,
                },
                "audit_degraded": false,
                "drain_deadline_ms": Value::Null,
                "limits": limits,
                "orphans": {
                    "verify": "served",
                    "mechanism": "fake",
                    "records": {"reserved": 0, "committed": 0, "unverified": 0},
                },
            }),
        )
    }

    fn daemon_orphans_verify(&mut self, connection: &str, params: &Value) -> Outcome {
        if !self.has_scope(connection, "daemon.control") {
            return self.refuse("daemon.orphans.verify", "Denied", "ScopeViolation", None);
        }
        let action = match params.get("action") {
            None | Some(Value::Null) => "verify",
            Some(Value::String(action)) => action.as_str(),
            // Match the served parser: a present non-string `action` is a
            // validation refusal, never the default verify.
            Some(_) => {
                return self.refuse(
                    "daemon.orphans.verify",
                    "Validation",
                    "InvalidRequest",
                    None,
                );
            }
        };
        let action = match action {
            "verify" => "verify",
            "retire" => "retire",
            _ => {
                return self.refuse(
                    "daemon.orphans.verify",
                    "Validation",
                    "InvalidRequest",
                    None,
                );
            }
        };
        // The fake has no journal and no process; it models the served shape
        // with an always-empty store and never claims a real reap. A retire
        // therefore names no record and is the uniform NotFound.
        if action == "retire" {
            let known = params
                .get("record_id")
                .and_then(Value::as_str)
                .is_some_and(|record| {
                    // Session ids are canonical lowercase hex; uppercase is
                    // not a known record on the served parser either.
                    record.len() == 32
                        && record
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                });
            if !known {
                return self.refuse(
                    "daemon.orphans.verify",
                    "Validation",
                    "InvalidRequest",
                    None,
                );
            }
            return self.refuse("daemon.orphans.verify", "NotFound", "NotFound", None);
        }
        self.reply(
            "daemon.orphans.verify",
            json!({
                "reaped": 0,
                "unverified": 0,
                "stale": 0,
                "reserved": 0,
                "committed": 0,
            }),
        )
    }

    fn active_sessions(&self) -> usize {
        self.sessions
            .values()
            .filter(|session| !is_terminal(session.state))
            .count()
    }

    fn daemon_stop(&mut self, connection: &str, params: &Value) -> Outcome {
        if !self.has_scope(connection, "daemon.control") {
            return self.refuse("daemon.stop", "Denied", "ScopeViolation", None);
        }
        if self.state == "stopping" {
            return self.refuse("daemon.stop", "Denied", "DaemonStopping", None);
        }
        let mode = params
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("refuse");
        if !matches!(mode, "refuse" | "drain" | "force") {
            return self.refuse("daemon.stop", "Validation", "InvalidRequest", None);
        }
        let active = self.active_sessions();
        if mode == "refuse" && active > 0 {
            return self.refuse(
                "daemon.stop",
                "Denied",
                "ActiveSessions",
                Some((active as u64, self.limits.effective(LimitId::MaxSessions))),
            );
        }
        let mut terminated = 0;
        if mode != "refuse" {
            let ids: Vec<String> = self
                .sessions
                .values()
                .filter(|session| !is_terminal(session.state))
                .map(|session| session.id.clone())
                .collect();
            for id in ids {
                if self.terminate(&id, "daemon_stop") {
                    terminated += 1;
                }
            }
            self.state = "stopping";
        }
        self.reply(
            "daemon.stop",
            json!({
                "accepted": true,
                "mode": mode,
                "active_sessions": active,
                "terminated": terminated,
                "drain_deadline_ms": Value::Null,
            }),
        )
    }

    fn daemon_host_grant(&mut self, connection: &str, params: &Value) -> Outcome {
        if !self.has_scope(connection, "daemon.control") {
            return self.refuse("daemon.host_grant", "Denied", "ScopeViolation", None);
        }
        // A field for another method is refused, never ignored.
        if params.get("observer_correlation_digest").is_some() {
            return self.refuse("daemon.host_grant", "Validation", "InvalidRequest", None);
        }
        let ttl_limit = self.limits.effective(LimitId::HostGrantTtlMs);
        match params.get("action").and_then(Value::as_str) {
            Some("mint") => {
                let ttl_ms = match optional_ttl(params, "ttl_ms", ttl_limit) {
                    Ok(ttl) => ttl,
                    Err(counts) => {
                        return self.refuse(
                            "daemon.host_grant",
                            "Validation",
                            "InvalidRequest",
                            counts,
                        );
                    }
                };
                let correlation = match optional_correlation(params, "client_correlation_digest") {
                    Ok(correlation) => correlation,
                    Err(()) => {
                        return self.refuse(
                            "daemon.host_grant",
                            "Validation",
                            "InvalidRequest",
                            None,
                        );
                    }
                };
                let limit = self.limits.effective_usize(LimitId::MaxPendingHostGrants);
                let active = self
                    .grants
                    .values()
                    .filter(|grant| {
                        grant.kind == "host"
                            && !grant.consumed
                            && !grant.revoked
                            && grant.expires_ms > self.now()
                    })
                    .count();
                if active >= limit {
                    return self.refuse(
                        "daemon.host_grant",
                        "Framing",
                        "LimitExceeded",
                        Some((0, limit as u64)),
                    );
                }
                let bearer = self.mint_host_grant_with(
                    ttl_ms.unwrap_or_else(|| self.limits.effective(LimitId::HostGrantTtlMs)),
                    correlation,
                );
                let grant = &self.grants[&bearer];
                let value = json!({
                    "grant_id": grant.id,
                    "grant": bearer,
                    "expires_at_ms": grant.expires_ms,
                });
                self.reply("daemon.host_grant", value)
            }
            Some("revoke") => {
                // `ttl_ms` and both digest fields are mint-only.
                if params.get("ttl_ms").is_some()
                    || params.get("client_correlation_digest").is_some()
                {
                    return self.refuse("daemon.host_grant", "Validation", "InvalidRequest", None);
                }
                let Some(id) = params.get("grant_id").and_then(Value::as_str) else {
                    return self.refuse("daemon.host_grant", "Validation", "InvalidRequest", None);
                };
                let bearer = self
                    .grants
                    .iter()
                    .find(|(_, grant)| grant.id == id && grant.kind == "host")
                    .map(|(bearer, _)| bearer.clone());
                let now = self.now();
                let (revoked, not_active) = match bearer {
                    Some(bearer) => {
                        let grant = self.grants.get_mut(&bearer).expect("looked up");
                        let active = !grant.consumed && !grant.revoked && grant.expires_ms > now;
                        grant.revoked = true;
                        (active, !active)
                    }
                    None => (false, true),
                };
                self.reply(
                    "daemon.host_grant",
                    json!({"grant_id": id, "revoked": revoked, "not_active": not_active}),
                )
            }
            _ => self.refuse("daemon.host_grant", "Validation", "InvalidRequest", None),
        }
    }

    fn session_create(&mut self, connection: &str, params: &Value) -> Outcome {
        let spawn = self.has_scope(connection, "process.spawn");
        let host = self.has_scope(connection, "session.host");
        match (spawn, host) {
            (false, false) | (false, true) => {
                return self.refuse("session.create", "Denied", "ScopeViolation", None);
            }
            (true, false) => {
                return self.refuse("session.create", "Denied", "ConsentRequired", None);
            }
            (true, true) => {}
        }
        let kind = params
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("terminal");
        // The frozen handler order: kind, consent, then the bounded spawn
        // specification. The fake models the served cell: a worker create
        // binds generation 1 without a process.
        if !matches!(kind, "terminal" | "worker") {
            return self.refuse("session.create", "Validation", "InvalidRequest", None);
        }
        let bound = self
            .conn(connection)
            .and_then(|conn| conn.host_grant.clone());
        let bound = bound.is_some_and(|bearer| {
            self.grants.get(&bearer).is_some_and(|grant| {
                grant.kind == "host"
                    && grant.consumed
                    && !grant.revoked
                    && grant.expires_ms > self.now()
            })
        });
        if !bound {
            return self.refuse("session.create", "Denied", "ConsentRequired", None);
        }
        let spec = if kind == "worker" {
            match SpawnSpec::parse(params.as_object(), &self.limits) {
                Ok(spec) => Some(spec),
                Err(SpawnSpecError::Invalid) => {
                    return self.refuse("session.create", "Validation", "InvalidRequest", None);
                }
                Err(SpawnSpecError::Limit { limit, actual }) => {
                    return self.refuse(
                        "session.create",
                        "Framing",
                        "LimitExceeded",
                        Some((actual, limit)),
                    );
                }
            }
        } else {
            None
        };
        let Some(key) = params.get("idempotency_key").and_then(Value::as_str) else {
            return self.refuse("session.create", "Validation", "InvalidRequest", None);
        };
        let terminal_id = params.get("terminal_id").and_then(Value::as_str);
        // The spawn digest is the only form of argv/env/cwd the store keeps.
        let fingerprint = match &spec {
            Some(spec) => format!("{kind}:{}:{}", terminal_id.unwrap_or(""), spec.digest()),
            None => format!("{kind}:{}", terminal_id.unwrap_or("")),
        };
        if let Some((remembered, session_id)) = self.create_keys.get(key) {
            if remembered == &fingerprint {
                if let Some(session) = self.sessions.get(session_id) {
                    let value = json!({
                        "session": Self::session_json(session),
                        "control_token": session.initial_token,
                        "replayed": true,
                    });
                    return self.reply("session.create", value);
                }
            }
            return self.refuse("session.create", "Denied", "IdempotencyConflict", None);
        }
        let id = self.mint_id();
        let terminal_id = terminal_id
            .map(str::to_owned)
            .unwrap_or_else(|| self.mint_id());
        if self
            .sessions
            .values()
            .any(|session| session.terminal_id == terminal_id && !is_terminal(session.state))
        {
            return self.refuse("session.create", "Denied", "TerminalAlreadyHosted", None);
        }
        let token = self.mint_token();
        let now = self.now();
        let bound_worker = spec.is_some();
        let session = FakeSession {
            id: id.clone(),
            kind: if bound_worker { "worker" } else { "terminal" },
            terminal_id,
            state: if bound_worker { "running" } else { "created" },
            generation: u64::from(bound_worker),
            revision: u64::from(bound_worker),
            controller: None,
            token: token.clone(),
            initial_token: Some(token.clone()),
            leases: BTreeMap::new(),
            created_ms: now,
            last_seen_ms: now,
            terminal_reason: None,
            cleanup: None,
        };
        self.create_keys
            .insert(key.to_owned(), (fingerprint, id.clone()));
        let value = json!({
            "session": Self::session_json(&session),
            "control_token": token,
            "replayed": false,
        });
        self.sessions.insert(id, session);
        self.reply("session.create", value)
    }

    /// `session.resize` in the fake mirrors the served worker content plane:
    /// controller lease, generation fence, and one revision bump. A terminal
    /// session is still registry-only, so a bound terminal has no PTY and
    /// answers `NotRunning` after authority passes.
    fn session_resize(&mut self, connection: &str, params: &Value) -> Outcome {
        if !self.has_scope(connection, "terminal.manage") {
            return self.refuse("session.resize", "Denied", "ScopeViolation", None);
        }
        let Some(session_id) = params.get("session_id").and_then(Value::as_str) else {
            return self.refuse("session.resize", "Validation", "InvalidRequest", None);
        };
        let Some(generation) = params.get("generation").and_then(Value::as_u64) else {
            return self.refuse("session.resize", "Validation", "InvalidRequest", None);
        };
        let sized = |key: &str| {
            params
                .get(key)
                .and_then(Value::as_u64)
                .is_some_and(|value| (1..=1000).contains(&value))
        };
        if !sized("cols") || !sized("rows") {
            return self.refuse("session.resize", "Validation", "InvalidRequest", None);
        }
        // The uniform `NotFound` for a caller that can neither see the session
        // through the operator view nor hold a live lease on it: resize is not
        // an existence or revision oracle.
        if !self.session_visible(connection, session_id, None) {
            return self.refuse("session.resize", "NotFound", "NotFound", None);
        }
        let Some(session) = self.sessions.get(session_id) else {
            return self.refuse("session.resize", "NotFound", "NotFound", None);
        };
        if session.generation != generation {
            return self.refuse(
                "session.resize",
                "Denied",
                "StaleGeneration",
                Some((session.generation, session.revision)),
            );
        }
        if is_terminal(session.state) {
            return self.refuse("session.resize", "Denied", "NotRunning", None);
        }
        if session.state == "stopping" {
            return self.refuse("session.resize", "Denied", "SessionStopping", None);
        }
        let now = self.now();
        let controller = session.controller.clone();
        let authorized = controller.as_ref().is_some_and(|attach| {
            self.conn(connection)
                .is_some_and(|conn| conn.leases.contains(attach))
                && session
                    .leases
                    .get(attach)
                    .is_some_and(|lease| lease.role == "controller" && lease.expires_ms > now)
        });
        if !authorized {
            return self.refuse("session.resize", "Denied", "ControllerRequired", None);
        }
        if session.kind != "worker" || session.state != "running" {
            return self.refuse("session.resize", "Denied", "NotRunning", None);
        }
        // `record_resize` renews the controller lease, so the fake must move
        // the same expiry a real resize does.
        let ttl = self.limits.effective(LimitId::AttachLeaseTtlMs);
        let controller = session.controller.clone();
        let session = self.sessions.get_mut(session_id).expect("checked");
        session.revision += 1;
        session.last_seen_ms = now;
        if let Some(lease) = controller
            .as_ref()
            .and_then(|attach| session.leases.get_mut(attach))
        {
            lease.expires_ms = now + ttl;
        }
        let value = Self::session_json(session);
        self.reply("session.resize", json!({"session": value}))
    }

    fn session_status(&mut self, connection: &str, params: &Value) -> Outcome {
        let Some(session_id) = params.get("session_id").and_then(Value::as_str) else {
            return self.refuse("session.status", "Validation", "InvalidRequest", None);
        };
        let token = params.get("control_token").and_then(Value::as_str);
        if !self.session_visible(connection, session_id, token) {
            return self.refuse("session.status", "NotFound", "NotFound", None);
        }
        let Some(session) = self.sessions.get(session_id) else {
            return self.refuse("session.status", "NotFound", "NotFound", None);
        };
        let value = json!({"session": Self::session_json(session)});
        self.reply("session.status", value)
    }

    fn session_list(&mut self, connection: &str) -> Outcome {
        let operator = self.has_scope(connection, "daemon.inspect");
        let now = self.now();
        let leases: BTreeSet<String> = self
            .conn(connection)
            .map(|conn| conn.leases.clone())
            .unwrap_or_default();
        let sessions: Vec<Value> = self
            .sessions
            .values()
            .filter(|session| {
                operator
                    || leases.iter().any(|attach| {
                        session
                            .leases
                            .get(attach)
                            .is_some_and(|lease| lease.expires_ms > now)
                    })
            })
            .map(Self::session_json)
            .collect();
        self.reply("session.list", json!({"sessions": sessions}))
    }

    fn session_attach(&mut self, connection: &str, params: &Value) -> Outcome {
        match params.get("attach_role").and_then(Value::as_str) {
            Some("controller") => self.attach_controller(connection, params),
            Some("observer") => self.attach_observer(connection, params),
            _ => self.refuse("session.attach", "Validation", "InvalidRequest", None),
        }
    }

    fn attach_controller(&mut self, connection: &str, params: &Value) -> Outcome {
        if !self.has_scope(connection, "session.control") {
            return self.refuse("session.attach", "Denied", "ScopeViolation", None);
        }
        let Some(session_id) = params.get("session_id").and_then(Value::as_str) else {
            return self.refuse("session.attach", "Validation", "InvalidRequest", None);
        };
        let Some(generation) = params.get("expected_generation").and_then(Value::as_u64) else {
            return self.refuse("session.attach", "Validation", "InvalidRequest", None);
        };
        let token = params.get("control_token").and_then(Value::as_str);
        if let Err((class, code)) = self.control_visible(connection, session_id, token) {
            return self.refuse("session.attach", class, code, None);
        }
        let Some(session) = self.sessions.get(session_id) else {
            return self.refuse("session.attach", "NotFound", "NotFound", None);
        };
        if session.generation != generation {
            return self.refuse(
                "session.attach",
                "Denied",
                "StaleGeneration",
                Some((session.generation, session.revision)),
            );
        }
        if session.controller.is_some() {
            return self.refuse("session.attach", "Denied", "ControlHeld", None);
        }
        if token != Some(session.token.as_str()) {
            return self.refuse("session.attach", "Denied", "InvalidControlToken", None);
        }
        let attach = self.mint_id();
        let rotated = self.mint_token();
        let expires = self.now() + self.limits.effective(LimitId::AttachLeaseTtlMs);
        let session = self.sessions.get_mut(session_id).expect("checked");
        session.revision += 1;
        session.controller = Some(attach.clone());
        session.token = rotated.clone();
        session.initial_token = None;
        session.leases.insert(
            attach.clone(),
            FakeLease {
                role: "controller",
                expires_ms: expires,
            },
        );
        let value = json!({
            "session": Self::session_json(session),
            "attach_id": attach.clone(),
            "role": "controller",
            "lease_expires_at_ms": expires,
            "control_token": rotated,
            "replayed": false,
        });
        {
            let revision = self.sessions[session_id].revision;
            let conn = self.connection(connection);
            conn.leases.insert(attach.clone());
            conn.event_cursor.insert(attach, revision);
        }
        self.reply("session.attach", value)
    }

    fn attach_observer(&mut self, connection: &str, params: &Value) -> Outcome {
        if !self.has_scope(connection, "session.attach") {
            return self.refuse("session.attach", "Denied", "ScopeViolation", None);
        }
        let Some(session_id) = params.get("session_id").and_then(Value::as_str) else {
            return self.refuse("session.attach", "Validation", "InvalidRequest", None);
        };
        let Some(bearer) = params.get("observer_grant").and_then(Value::as_str) else {
            return self.refuse("session.attach", "Denied", "ObserverGrantRequired", None);
        };
        let now = self.now();
        let observed = self
            .conn(connection)
            .and_then(|conn| conn.correlation.as_deref());
        let valid = self.grants.get(bearer).is_some_and(|grant| {
            grant.kind == "observer"
                && grant.session == session_id
                && !grant.consumed
                && !grant.revoked
                && grant.expires_ms > now
                && correlation_accepts(grant.correlation.as_deref(), observed)
        });
        if !valid {
            return self.refuse("session.attach", "Denied", "ObserverGrantRequired", None);
        }
        let Some(session) = self.sessions.get(session_id) else {
            return self.refuse("session.attach", "NotFound", "NotFound", None);
        };
        let limit = self.limits.effective_usize(LimitId::MaxAttachesPerSession);
        let observers = session
            .leases
            .values()
            .filter(|lease| lease.role == "observer" && lease.expires_ms > now)
            .count();
        if observers >= limit {
            return self.refuse(
                "session.attach",
                "Framing",
                "LimitExceeded",
                Some((0, limit as u64)),
            );
        }
        if let Some(grant) = self.grants.get_mut(bearer) {
            grant.consumed = true;
        }
        let attach = self.mint_id();
        let expires = now + self.limits.effective(LimitId::AttachLeaseTtlMs);
        let session = self.sessions.get_mut(session_id).expect("checked");
        // The contract counts grant consume and attach as two visible
        // mutations, matching the real service's post-consume snapshot bump.
        session.revision += 2;
        session.leases.insert(
            attach.clone(),
            FakeLease {
                role: "observer",
                expires_ms: expires,
            },
        );
        let value = json!({
            "session": Self::session_json(session),
            "attach_id": attach.clone(),
            "role": "observer",
            "lease_expires_at_ms": expires,
            "replayed": false,
        });
        {
            let revision = self.sessions[session_id].revision;
            let conn = self.connection(connection);
            conn.leases.insert(attach.clone());
            conn.event_cursor.insert(attach, revision);
        }
        self.reply("session.attach", value)
    }

    fn control_authority(
        &self,
        connection: &str,
        session_id: &str,
        token: Option<&str>,
    ) -> Result<(), (&'static str, &'static str)> {
        if self.has_scope(connection, "daemon.control") {
            return Ok(());
        }
        let Some(session) = self.sessions.get(session_id) else {
            return Err(("NotFound", "NotFound"));
        };
        let now = self.now();
        let mut own_observer = false;
        if let Some(conn) = self.conn(connection) {
            for attach in &conn.leases {
                if let Some(lease) = session.leases.get(attach) {
                    if lease.expires_ms > now {
                        if lease.role == "controller"
                            && session.controller.as_deref() == Some(attach.as_str())
                        {
                            return Ok(());
                        }
                        own_observer = true;
                    }
                }
            }
        }
        if own_observer {
            return Err(("Denied", "ControllerRequired"));
        }
        if session.controller.is_some() {
            return Err(("Denied", "ControlHeld"));
        }
        match token {
            Some(token) if token == session.token => Ok(()),
            _ => Err(("Denied", "InvalidControlToken")),
        }
    }

    fn session_grant(&mut self, connection: &str, params: &Value) -> Outcome {
        // A field for another method is refused, never ignored.
        if params.get("client_correlation_digest").is_some() {
            return self.refuse("session.grant", "Validation", "InvalidRequest", None);
        }
        let Some(session_id) = params.get("session_id").and_then(Value::as_str) else {
            return self.refuse("session.grant", "Validation", "InvalidRequest", None);
        };
        let token = params.get("control_token").and_then(Value::as_str);
        if let Err((class, code)) = self.control_visible(connection, session_id, token) {
            return self.refuse("session.grant", class, code, None);
        }
        // A terminal record is read-only: a new mint and a keyed repeat both
        // take the contract's `NotRunning`.
        if self
            .sessions
            .get(session_id)
            .is_some_and(|session| is_terminal(session.state))
        {
            return self.refuse("session.grant", "Denied", "NotRunning", None);
        }
        if let Err((class, code)) = self.control_authority(connection, session_id, token) {
            return self.refuse("session.grant", class, code, None);
        }
        let Some(key) = params.get("idempotency_key").and_then(Value::as_str) else {
            return self.refuse("session.grant", "Validation", "InvalidRequest", None);
        };
        let ttl_ms = match optional_ttl(
            params,
            "ttl_ms",
            self.limits.effective(LimitId::ObserverGrantTtlMs),
        ) {
            Ok(ttl) => ttl,
            Err(counts) => {
                return self.refuse("session.grant", "Validation", "InvalidRequest", counts);
            }
        };
        let correlation = match optional_correlation(params, "observer_correlation_digest") {
            Ok(correlation) => correlation,
            Err(()) => {
                return self.refuse("session.grant", "Validation", "InvalidRequest", None);
            }
        };
        let limit = self
            .limits
            .effective_usize(LimitId::MaxPendingObserverGrantsPerSession);
        let active = self
            .grants
            .values()
            .filter(|grant| {
                grant.kind == "observer"
                    && grant.session == session_id
                    && !grant.consumed
                    && !grant.revoked
                    && grant.expires_ms > self.now()
            })
            .count();
        if active >= limit {
            return self.refuse(
                "session.grant",
                "Framing",
                "LimitExceeded",
                Some((0, limit as u64)),
            );
        }
        let key = format!("grant:{session_id}:{key}");
        // A keyed duplicate with a different TTL or binding is a different
        // request, never a descriptor replay.
        let fingerprint = format!(
            "{}|{}",
            ttl_ms.unwrap_or_default(),
            correlation_str(&correlation)
        );
        if let Some((id, recorded)) = self.grant_keys.get(&key) {
            if recorded != &fingerprint {
                return self.refuse("session.grant", "Denied", "IdempotencyConflict", None);
            }
            let id = id.clone();
            // The record may have been retired by the sweep. The key still
            // remembers the id, so the replay answers the original grant with
            // no bearer and no revision bump, exactly as the daemon does.
            let bearer = self
                .grants
                .iter()
                .find(|(_, grant)| grant.id == id)
                .map(|(bearer, _)| bearer.clone());
            let value = match bearer {
                Some(bearer) => json!({
                    "grant_id": id,
                    "grant": bearer,
                    "grant_active": !self.grants[&bearer].consumed,
                    "session_id": session_id,
                    "replayed": true,
                }),
                None => json!({
                    "grant_id": id,
                    "grant": Value::Null,
                    "grant_active": false,
                    "session_id": session_id,
                    "replayed": true,
                }),
            };
            return self.reply("session.grant", value);
        }
        let id = self.mint_id();
        let bearer = self.mint_token();
        let expires = self.now()
            + ttl_ms.unwrap_or_else(|| self.limits.effective(LimitId::ObserverGrantTtlMs));
        self.grants.insert(
            bearer.clone(),
            FakeGrant {
                id: id.clone(),
                session: session_id.to_owned(),
                kind: "observer",
                expires_ms: expires,
                consumed: false,
                revoked: false,
                correlation,
            },
        );
        self.grant_keys.insert(key, (id.clone(), fingerprint));
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.revision += 1;
        }
        self.reply(
            "session.grant",
            json!({
                "grant_id": id,
                "grant": bearer,
                "expires_at_ms": expires,
                "session_id": session_id,
                "replayed": false,
            }),
        )
    }

    fn session_grant_revoke(&mut self, connection: &str, params: &Value) -> Outcome {
        // `ttl_ms` and both digest fields are mint-only.
        if params.get("ttl_ms").is_some()
            || params.get("observer_correlation_digest").is_some()
            || params.get("client_correlation_digest").is_some()
        {
            return self.refuse("session.grant.revoke", "Validation", "InvalidRequest", None);
        }
        let Some(id) = params.get("grant_id").and_then(Value::as_str) else {
            return self.refuse("session.grant.revoke", "Validation", "InvalidRequest", None);
        };
        let bearer = self
            .grants
            .iter()
            .find(|(_, grant)| grant.id == id && grant.kind == "observer")
            .map(|(bearer, _)| bearer.clone());
        if let Some(bearer) = &bearer {
            let correlation = self.grants[bearer].correlation.clone();
            let observed = self
                .conn(connection)
                .and_then(|conn| conn.correlation.clone());
            if !self.has_scope(connection, "daemon.control")
                && !correlation_accepts(correlation.as_deref(), observed.as_deref())
            {
                // A correlation mismatch is the unknown-grant answer: the
                // revoke is not attempted and nothing is disclosed.
                return self.reply(
                    "session.grant.revoke",
                    json!({"grant_id": id, "revoked": false, "not_active": true}),
                );
            }
            let session_id = self.grants[bearer].session.clone();
            let token = params.get("control_token").and_then(Value::as_str);
            if let Err((class, code)) = self.control_visible(connection, &session_id, token) {
                return self.refuse("session.grant.revoke", class, code, None);
            }
            // A terminal record refuses every mutation with `Denied/NotRunning`
            // before authority, so a former observer cannot revoke another
            // client's pending grant on a frozen session.
            if self
                .sessions
                .get(&session_id)
                .is_some_and(|session| is_terminal(session.state))
            {
                return self.refuse("session.grant.revoke", "Denied", "NotRunning", None);
            }
            if let Err((class, code)) = self.control_authority(connection, &session_id, token) {
                return self.refuse("session.grant.revoke", class, code, None);
            }
        }
        let now = self.now();
        let (revoked, not_active) = match bearer {
            Some(bearer) => {
                let grant = self.grants.get_mut(&bearer).expect("looked up");
                let active = !grant.consumed && !grant.revoked && grant.expires_ms > now;
                let session_id = grant.session.clone();
                grant.revoked = true;
                if active {
                    if let Some(session) = self.sessions.get_mut(&session_id) {
                        session.revision += 1;
                    }
                }
                (active, !active)
            }
            None => (false, true),
        };
        self.reply(
            "session.grant.revoke",
            json!({"grant_id": id, "revoked": revoked, "not_active": not_active}),
        )
    }

    fn session_detach(&mut self, connection: &str, params: &Value) -> Outcome {
        let Some(attach) = params.get("attach_id").and_then(Value::as_str) else {
            return self.refuse("session.detach", "Validation", "InvalidRequest", None);
        };
        let own = self
            .conn(connection)
            .is_some_and(|conn| conn.leases.contains(attach));
        let target_session = self
            .sessions
            .iter()
            .find(|(_, session)| session.leases.contains_key(attach))
            .map(|(id, _)| id.clone());
        if !own {
            let control = self.has_scope(connection, "session.control")
                || self.has_scope(connection, "daemon.control");
            if !control {
                return self.refuse("session.detach", "Denied", "ScopeViolation", None);
            }
            if target_session.is_none() {
                // A handle this connection already detached is the
                // idempotent acknowledgement, with no revision change.
                if let Some((session_id, owner)) = self.detached.get(attach) {
                    if owner == connection {
                        if let Some(session) = self.sessions.get(session_id) {
                            let value = json!({
                                "session": Self::session_json(session),
                                "attach_id": attach,
                                "was_attached": false,
                                "not_attached": true,
                                "revoked": false,
                            });
                            return self.reply("session.detach", value);
                        }
                    }
                }
                return self.refuse("session.detach", "NotFound", "NotFound", None);
            }
            let Some(session_id) = &target_session else {
                unreachable!("checked above");
            };
            let token = params.get("control_token").and_then(Value::as_str);
            if let Err((class, code)) = self.control_visible(connection, session_id, token) {
                return self.refuse("session.detach", class, code, None);
            }
            if let Err((class, code)) = self.control_authority(connection, session_id, token) {
                return self.refuse("session.detach", class, code, None);
            }
        }
        let Some(session_id) = target_session else {
            return self.refuse("session.detach", "NotFound", "NotFound", None);
        };
        let (removed, value) = {
            let session = self.sessions.get_mut(&session_id).expect("found");
            let removed = session.leases.remove(attach).is_some();
            if removed {
                session.revision += 1;
                if session.controller.as_deref() == Some(attach) {
                    session.controller = None;
                }
            }
            let revoked = removed && !own;
            let value = json!({
                "session": Self::session_json(session),
                "attach_id": attach,
                "was_attached": removed,
                "not_attached": !removed,
                "revoked": revoked,
            });
            (removed, value)
        };
        self.connection(connection).leases.remove(attach);
        if removed {
            self.detached.insert(
                attach.to_owned(),
                (session_id.clone(), connection.to_owned()),
            );
        }
        self.reply("session.detach", value)
    }

    fn session_transfer(&mut self, connection: &str, params: &Value) -> Outcome {
        let Some(session_id) = params.get("session_id").and_then(Value::as_str) else {
            return self.refuse(
                "session.control.transfer",
                "Validation",
                "InvalidRequest",
                None,
            );
        };
        let Some(target) = canonical_target_attach(params) else {
            return self.refuse(
                "session.control.transfer",
                "Validation",
                "InvalidRequest",
                None,
            );
        };
        let Some(generation) = canonical_generation(params) else {
            return self.refuse(
                "session.control.transfer",
                "Validation",
                "InvalidRequest",
                None,
            );
        };
        let token = params.get("control_token").and_then(Value::as_str);
        if let Err((class, code)) = self.control_visible(connection, session_id, token) {
            return self.refuse("session.control.transfer", class, code, None);
        }
        let Some(session) = self.sessions.get(session_id) else {
            return self.refuse("session.control.transfer", "NotFound", "NotFound", None);
        };
        if session.generation != generation {
            return self.refuse(
                "session.control.transfer",
                "Denied",
                "StaleGeneration",
                None,
            );
        }
        if let Err((class, code)) = self.control_authority(connection, session_id, token) {
            return self.refuse("session.control.transfer", class, code, None);
        }
        let now = self.now();
        let target_lease = self
            .sessions
            .get(session_id)
            .and_then(|session| session.leases.get(target))
            .cloned();
        let Some(target_lease) = target_lease else {
            return self.refuse(
                "session.control.transfer",
                "Denied",
                "ControllerRequired",
                None,
            );
        };
        if target_lease.role != "observer" || target_lease.expires_ms <= now {
            return self.refuse(
                "session.control.transfer",
                "Denied",
                "ControllerRequired",
                None,
            );
        }
        let previous = self.sessions[session_id].controller.clone();
        let rotated = self.mint_token();
        let session = self.sessions.get_mut(session_id).expect("checked");
        if let Some(previous) = &previous {
            if let Some(lease) = session.leases.get_mut(previous) {
                lease.role = "observer";
            }
        }
        if let Some(lease) = session.leases.get_mut(target) {
            lease.role = "controller";
        }
        session.controller = Some(target.to_owned());
        session.token = rotated.clone();
        session.initial_token = None;
        session.revision += 1;
        self.deliveries.insert(target.to_owned(), rotated);
        let value = json!({
            "session": Self::session_json(session),
            "controller": target,
            "previous_controller": previous,
            "replayed": false,
        });
        self.reply("session.control.transfer", value)
    }

    fn session_abort(&mut self, connection: &str, params: &Value) -> Outcome {
        let Some(session_id) = params.get("session_id").and_then(Value::as_str) else {
            return self.refuse("session.abort", "Validation", "InvalidRequest", None);
        };
        let Some(generation) = canonical_generation(params) else {
            return self.refuse("session.abort", "Validation", "InvalidRequest", None);
        };
        let reason = params
            .get("reason_class")
            .and_then(Value::as_str)
            .unwrap_or("");
        let token = params.get("control_token").and_then(Value::as_str);
        if let Err((class, code)) = self.control_visible(connection, session_id, token) {
            return self.refuse("session.abort", class, code, None);
        }
        let Some(session) = self.sessions.get(session_id) else {
            return self.refuse("session.abort", "NotFound", "NotFound", None);
        };
        if is_terminal(session.state) {
            let value = json!({
                "session": Self::session_json(session),
                "accepted": false,
                "not_running": true,
            });
            return self.reply("session.abort", value);
        }
        if session.generation != generation {
            return self.refuse(
                "session.abort",
                "Denied",
                "StaleGeneration",
                Some((session.generation, session.revision)),
            );
        }
        if !matches!(reason, "cancel" | "timeout" | "operator") {
            return self.refuse("session.abort", "Validation", "InvalidRequest", None);
        }
        if reason == "operator" {
            if !self.has_scope(connection, "daemon.control") {
                return self.refuse("session.abort", "Denied", "ScopeViolation", None);
            }
        } else if let Err((class, code)) = self.control_authority(connection, session_id, token) {
            return self.refuse("session.abort", class, code, None);
        }
        let terminal = match reason {
            "cancel" => "consumer_cancel",
            "timeout" => "consumer_timeout",
            _ => "operator_abort",
        };
        self.terminate(session_id, terminal);
        let session = &self.sessions[session_id];
        let value = json!({
            "session": Self::session_json(session),
            "accepted": true,
            "reason": terminal,
            "stage": "not_started",
            "cleanup": "not_started",
        });
        self.reply("session.abort", value)
    }

    fn session_kill(&mut self, connection: &str, params: &Value) -> Outcome {
        let Some(session_id) = params.get("session_id").and_then(Value::as_str) else {
            return self.refuse("session.kill", "Validation", "InvalidRequest", None);
        };
        let Some(generation) = canonical_generation(params) else {
            return self.refuse("session.kill", "Validation", "InvalidRequest", None);
        };
        if !self.session_visible(connection, session_id, None) {
            return self.refuse("session.kill", "NotFound", "NotFound", None);
        }
        let Some(session) = self.sessions.get(session_id) else {
            return self.refuse("session.kill", "NotFound", "NotFound", None);
        };
        if is_terminal(session.state) {
            let value = json!({
                "session": Self::session_json(session),
                "accepted": false,
                "not_running": true,
            });
            return self.reply("session.kill", value);
        }
        if session.generation != generation {
            return self.refuse("session.kill", "Denied", "StaleGeneration", None);
        }
        if let Err((class, code)) = self.control_authority(connection, session_id, None) {
            return self.refuse("session.kill", class, code, None);
        }
        self.terminate(session_id, "operator_abort");
        let session = &self.sessions[session_id];
        let value = json!({
            "session": Self::session_json(session),
            "accepted": true,
            "reason": "operator_abort",
            "stage": "not_started",
            "cleanup": "not_started",
        });
        self.reply("session.kill", value)
    }

    fn lease_heartbeat(&mut self, connection: &str, params: &Value) -> Outcome {
        let Some(attach) = params.get("attach_id").and_then(Value::as_str) else {
            return self.refuse("lease.heartbeat", "Validation", "InvalidRequest", None);
        };
        if !self
            .conn(connection)
            .is_some_and(|conn| conn.leases.contains(attach))
        {
            return self.refuse("lease.heartbeat", "NotFound", "NotFound", None);
        }
        let now = self.now();
        let mut found: Option<(String, u64)> = None;
        for session in self.sessions.values_mut() {
            if let Some(lease) = session.leases.get_mut(attach) {
                if lease.expires_ms <= now {
                    break;
                }
                lease.expires_ms = now + self.limits.effective(LimitId::AttachLeaseTtlMs);
                session.last_seen_ms = now;
                found = Some((session.id.clone(), lease.expires_ms));
                break;
            }
        }
        let Some((session_id, expires)) = found else {
            return self.refuse("lease.heartbeat", "NotFound", "NotFound", None);
        };
        let token = self.connection(connection).pending.remove(attach);
        let session = &self.sessions[&session_id];
        let value = json!({
            "session": Self::session_json(session),
            "attach_id": attach,
            "lease_expires_at_ms": expires,
            "control_token": token,
        });
        self.reply("lease.heartbeat", value)
    }

    /// Complete a pre-bind stop: detach every live lease, then record the
    /// terminal state. Each detach and the terminal transition bump the
    /// revision, matching the real host's one-bump-per-visible-mutation rule.
    fn terminate(&mut self, session_id: &str, reason: &'static str) -> bool {
        let now = self.now();
        let Some(session) = self.sessions.get_mut(session_id) else {
            return false;
        };
        if is_terminal(session.state) {
            return false;
        }
        let leases: Vec<String> = session.leases.keys().cloned().collect();
        for attach in &leases {
            session.leases.remove(attach);
            session.revision += 1;
        }
        session.controller = None;
        session.initial_token = None;
        session.state = "terminated";
        session.terminal_reason = Some(reason);
        session.cleanup = Some("not_started");
        session.last_seen_ms = now;
        session.revision += 1;
        for attach in leases {
            for conn in self.connections.values_mut() {
                conn.leases.remove(&attach);
            }
            self.deliveries.remove(&attach);
        }
        true
    }

    fn route(&mut self, connection: &str) {
        let attaches: Vec<String> = self
            .conn(connection)
            .map(|conn| conn.leases.iter().cloned().collect())
            .unwrap_or_default();
        for attach in attaches {
            if let Some(token) = self.deliveries.remove(&attach) {
                self.connection(connection).pending.insert(attach, token);
            }
        }
    }
}

fn is_terminal(state: &str) -> bool {
    matches!(state, "exited" | "failed" | "terminated")
}

fn well_formed(method: &str) -> bool {
    let mut segments = 0;
    for segment in method.split('.') {
        if segment.is_empty() {
            return false;
        }
        if !segment
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return false;
        }
        segments += 1;
    }
    segments >= 2
}

impl Driver for FakeDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Fake
    }

    fn prepare(&mut self, vector: &Vector) -> Result<Vars, String> {
        self.reset(vector)?;
        Ok(self.seeds())
    }

    fn call(&mut self, connection: &str, method: &str, params: Value) -> Outcome {
        self.call_inner(connection, method, params)
    }

    fn call_dropping(&mut self, connection: &str, method: &str, params: Value) -> Outcome {
        let _ = self.call_inner(connection, method, params);
        self.connection(connection).closed = true;
        Outcome::Closed
    }

    fn expect_eof(&mut self, connection: &str) -> bool {
        self.connection(connection).closed
    }

    fn peer_exit(
        &mut self,
        _connection: &str,
        _method: &str,
        _params: &Value,
    ) -> Result<(), String> {
        Err("the fake driver has no peer process".to_owned())
    }

    fn queue_pressure(
        &mut self,
        connection: &str,
        method: &str,
        count: usize,
        params: &Value,
    ) -> Outcome {
        let limit = self
            .limits
            .effective_usize(LimitId::MaxConnectionOutboundBytes);
        let mut queued = 0usize;
        for _ in 0..count {
            let outcome = self.call_inner(connection, method, params.clone());
            let Outcome::Reply(reply) = outcome else {
                return Outcome::Closed;
            };
            queued = queued.saturating_add(
                serde_json::to_vec(&reply.result)
                    .map(|bytes| bytes.len())
                    .unwrap_or(0),
            );
            if queued > limit {
                self.connection(connection).closed = true;
                self.audit
                    .push("v1 a=wire r=refused c=limit_exceeded".to_owned());
                return Outcome::Closed;
            }
        }
        self.refuse(method, "Harness", "DidNotClose", None)
    }

    fn refusal_burst(
        &mut self,
        connection: &str,
        method: &str,
        count: usize,
        code: &str,
    ) -> Result<(), String> {
        for _ in 0..count {
            let outcome = self.call_inner(connection, method, Value::Null);
            let Outcome::Reply(reply) = outcome else {
                return Err("the connection closed during the burst".to_owned());
            };
            let Some(error) = reply.error else {
                return Err("the burst produced a success".to_owned());
            };
            if error.code != code {
                return Err(format!(
                    "the burst returned {}, expected {code}",
                    error.code
                ));
            }
        }
        Ok(())
    }

    fn oversized_frame(&mut self, connection: &str, declared: u32) -> Outcome {
        let limit = self.limits.effective(LimitId::MaxFrameBytes);
        if u64::from(declared) <= limit {
            return Outcome::Closed;
        }
        self.connection(connection).closed = true;
        self.audit
            .push("v1 a=wire r=refused c=limit_exceeded".to_owned());
        Outcome::Closed
    }

    fn partial_frame(&mut self, connection: &str, declared: u32, body: usize) -> Outcome {
        let budget = self.limits.effective_usize(LimitId::PreAuthMaxBytes);
        let frame_limit = self.limits.effective(LimitId::MaxFrameBytes);
        if body + 4 <= budget && u64::from(declared) <= frame_limit {
            return Outcome::Closed;
        }
        self.connection(connection).closed = true;
        self.audit
            .push("v1 a=wire r=refused c=limit_exceeded".to_owned());
        Outcome::Closed
    }

    fn raw_request(&mut self, _connection: &str, _request: Value, _expect_eof: bool) -> Outcome {
        // The in-memory fake has no frame envelope, so the raw-frame vectors
        // are real-only and carry a typed skip here.
        Outcome::Closed
    }

    fn pipelined_negotiate(
        &mut self,
        _connection: &str,
        _second: &Value,
        _partial_body: Option<usize>,
    ) -> Outcome {
        // The fake dispatches synchronously per call and has no socket batch,
        // so the pipelining vectors are real-only and carry a typed skip here.
        Outcome::Closed
    }

    fn read_events(&mut self, connection: &str, count: usize) -> Result<Vec<Value>, String> {
        let Some(conn) = self.conn(connection) else {
            return Err("unknown connection".to_owned());
        };
        if !conn.events {
            return Err("connection did not opt in to events".to_owned());
        }
        let leases: Vec<String> = conn.leases.iter().cloned().collect();
        let mut cursors = conn.event_cursor.clone();
        let instance = self.instance.clone();
        let mut events = Vec::new();
        for attach in leases {
            let Some(session) = self
                .sessions
                .values()
                .find(|session| session.leases.contains_key(&attach))
            else {
                continue;
            };
            let cursor = cursors.get(&attach).copied().unwrap_or(0);
            for revision in cursor.saturating_add(1)..=session.revision {
                let terminal = is_terminal(session.state) && revision == session.revision;
                events.push(json!({
                    "kind": if terminal { "session_terminal" } else { "session_state" },
                    "instance_id": instance,
                    "session_id": session.id,
                    "attach_id": attach,
                    "revision": revision,
                    "terminal": terminal,
                }));
            }
            cursors.insert(attach, session.revision);
        }
        if events.len() != count {
            return Err(format!(
                "fake has {} queued events, expected {count}",
                events.len()
            ));
        }
        self.connection(connection).event_cursor = cursors;
        Ok(events)
    }

    fn restart(&mut self) -> Result<Vars, String> {
        self.instance = self.mint_id();
        self.operator_capability = self.mint_token();
        self.state = "running";
        self.sessions.clear();
        self.grants.clear();
        self.connections.clear();
        self.create_keys.clear();
        self.deliveries.clear();
        self.detached.clear();
        self.host_grant = self.needs_host_grant.then(|| self.mint_host_grant());
        self.facts = self.facts_now();
        Ok(self.seeds())
    }

    fn crash(&mut self) -> Result<(), String> {
        Err("the fake driver cannot crash a real process".to_owned())
    }

    fn advance_clock(&mut self, millis: u64) {
        self.clock_ms = self.clock_ms.saturating_add(millis);
    }

    fn audit_text(&self) -> String {
        self.audit.join("\n")
    }

    fn file_mode(&self, _role: &str) -> Option<u32> {
        None
    }

    fn no_tcp_listener(&self) -> Result<(), String> {
        Err("the fake driver has no process".to_owned())
    }

    fn daemon_absent(&mut self) -> Result<ExitObservation, String> {
        Err("the fake driver has no process".to_owned())
    }

    fn requests(&self) -> Vec<RequestRecord> {
        self.requests.clone()
    }

    fn facts(&self) -> ServiceFacts {
        self.facts.clone()
    }

    fn finish(&mut self) {
        self.connections.clear();
    }
}

/// An optional positive `ttl_ms` no greater than the compiled limit.
///
/// `Err(None)` is a malformed value, `Err(Some(counts))` an out-of-range one
/// that carries the observed value and the bound.
fn optional_ttl(
    params: &Value,
    key: &str,
    limit_ms: u64,
) -> Result<Option<u64>, Option<(u64, u64)>> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };
    let ttl = value.as_u64().ok_or(None)?;
    if ttl == 0 || ttl > limit_ms {
        return Err(Some((ttl, limit_ms)));
    }
    Ok(Some(ttl))
}

/// An optional correlation digest, normalized to its 32-hex binding.
fn optional_correlation(params: &Value, key: &str) -> Result<Option<String>, ()> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };
    let raw = value.as_str().ok_or(())?;
    let parsed = GrantCorrelation::parse(raw).map_err(|_| ())?;
    Ok(Some(hex(parsed.as_bytes())))
}

/// Whether a grant's optional binding accepts the acting connection.
fn correlation_accepts(bound: Option<&str>, observed: Option<&str>) -> bool {
    match bound {
        None => true,
        Some(bound) => observed == Some(bound),
    }
}

fn correlation_str(correlation: &Option<String>) -> String {
    correlation.clone().unwrap_or_else(|| "none".to_owned())
}

/// The canonical `generation` with the deprecated `expected_generation` alias.
///
/// The raw JSON values are compared before either is converted, matching the
/// service: a wrongly typed field is still a conflict, so `1` and `"1"` are an
/// ambiguous pair rather than one usable alias.
fn canonical_generation(params: &Value) -> Option<u64> {
    let canonical = params.get("generation");
    let alias = params.get("expected_generation");
    let value = match (canonical, alias) {
        (Some(canonical), Some(alias)) if canonical != alias => return None,
        (Some(canonical), _) => canonical,
        (None, Some(alias)) => alias,
        (None, None) => return None,
    };
    value.as_u64()
}

/// The canonical `target_attach_id` with the deprecated `attach_id` alias.
///
/// The raw JSON values are compared before either is converted, matching the
/// service, so a wrongly typed field cannot be dropped before the conflict
/// check.
fn canonical_target_attach(params: &Value) -> Option<&str> {
    let canonical = params.get("target_attach_id");
    let alias = params.get("attach_id");
    let value = match (canonical, alias) {
        (Some(canonical), Some(alias)) if canonical != alias => return None,
        (Some(canonical), _) => canonical,
        (None, Some(alias)) => alias,
        (None, None) => return None,
    };
    value.as_str()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{canonical_generation, canonical_target_attach};

    #[test]
    fn generation_alias_conflicts_use_the_raw_json_value() {
        assert_eq!(canonical_generation(&json!({"generation": 1})), Some(1));
        assert_eq!(
            canonical_generation(&json!({"expected_generation": 1})),
            Some(1)
        );
        assert_eq!(
            canonical_generation(&json!({"generation": 1, "expected_generation": 1})),
            Some(1)
        );
        assert_eq!(
            canonical_generation(&json!({"generation": 1, "expected_generation": 2})),
            None
        );
        assert_eq!(
            canonical_generation(&json!({"generation": 1, "expected_generation": "1"})),
            None
        );
        assert_eq!(
            canonical_generation(&json!({"generation": 1, "expected_generation": 1.0})),
            None
        );
        assert_eq!(
            canonical_generation(&json!({"generation": "1", "expected_generation": 1})),
            None
        );
        assert_eq!(canonical_generation(&json!({})), None);
    }

    #[test]
    fn target_attach_alias_conflicts_use_the_raw_json_value() {
        let target = "1".repeat(32);
        let other = "2".repeat(32);
        assert_eq!(
            canonical_target_attach(&json!({"target_attach_id": &target})),
            Some(target.as_str())
        );
        assert_eq!(
            canonical_target_attach(&json!({
                "target_attach_id": &target,
                "attach_id": &target,
            })),
            Some(target.as_str())
        );
        assert_eq!(
            canonical_target_attach(&json!({
                "target_attach_id": &target,
                "attach_id": &other,
            })),
            None
        );
        assert_eq!(
            canonical_target_attach(&json!({
                "target_attach_id": &target,
                "attach_id": 1,
            })),
            None
        );
        assert_eq!(canonical_target_attach(&json!({})), None);
    }
}
