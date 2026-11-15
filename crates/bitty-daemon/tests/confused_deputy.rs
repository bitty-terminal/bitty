#![cfg(target_os = "linux")]

//! P0-AC-024 (bitty#2921): the confused-deputy suite for the daemon's IPC
//! authority plane.
//!
//! A confused deputy holds authority and then spends it for a caller that never
//! held it. On `bittyd` the authority that could be borrowed is:
//!
//! - the per-connection service scope set (baseline `session.*`, host-consent
//!   `session.host`, operator `daemon.*`) and its transport grants;
//! - the single-use, peer- and correlation-bound grant bearers
//!   (`daemon.host_grant`, `session.grant`);
//! - the lease and control relation (`session.attach`, `session.detach`,
//!   `session.grant`, the content plane).
//!
//! This target attacks each path over the real Unix socket against a served
//! `bittyd` and asserts the typed refusal the daemon actually returns, so "a
//! connection cannot obtain authority it was not granted, by indirect means" is
//! a runnable statement rather than a review note. Every asserted refusal is
//! printed under a `CD_*` marker with its class and code.
//!
//! | Test | Vector |
//! | --- | --- |
//! | [`a_controller_cannot_reach_another_consumers_content_plane`] | (a) target-addressed content verbs at a session the caller does not own |
//! | [`an_attach_handle_owned_by_another_connection_is_never_found`] | (a) target-addressed verbs at an attachment the caller does not own |
//! | [`a_baseline_consumer_cannot_mint_authority_it_does_not_hold`] | (b) a narrow holder cannot mint a broader grant |
//! | [`a_host_grant_cannot_be_forwarded_to_another_connection`] | (b) grant forwarding, single use, and the correlation binding |
//! | [`an_observer_grant_cannot_become_control_or_be_re_aimed`] | (b)/(e) a small grant cannot be spent as a larger one or at another target |
//! | [`an_operator_less_cli_cannot_stop_the_daemon`] | (c) the operator-less CLI surface inherits no daemon authority |
//! | [`a_client_supplied_consumer_identity_does_not_change_the_effective_scopes`] | (d) forged consumer kind, role, and trust fields |
//! | [`authority_accumulated_by_a_low_scope_consumer_stops_at_its_own_target`] | (e) nesting and laundering through a self-created session |
//!
//! What this target deliberately does not claim:
//!
//! - it is not the MCP/Agent adapter audit and not the instruction/data
//!   channel-separation review, both of which the criterion names and neither of
//!   which lives in this repository;
//! - it runs as one uid, so a "different consumer" is always a different
//!   connection, never a different account: the peer-uid half of a grant's
//!   binding is asserted structurally elsewhere (`tests/peer.rs`) and the
//!   second-uid cell stays unrun, exactly as the dated evidence records say;
//! - it adds no production code. Where a denial is a *design* refusal (the
//!   uniform `NotFound`, `Denied/ScopeViolation`, `Denied/ControllerRequired`)
//!   the test asserts the typed pair, so a refactor that merges two refusal
//!   stages would fail here rather than pass silently.
//!
//! Run with `cargo test -p bitty-daemon --test confused_deputy --offline -- --
//! nocapture`.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use bitty_daemon::handshake::ClientCorrelation;

const BITTYD: &str = env!("CARGO_BIN_EXE_bittyd");

static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

/// A throwaway `XDG_RUNTIME_DIR`/`XDG_STATE_HOME` pair (mode `0700`).
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let suffix = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bittyd-deputy-{label}-{}-{suffix}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp root");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0700");
        Self { path }
    }

    fn runtime(&self) -> PathBuf {
        self.path.join("runtime")
    }

    fn state(&self) -> PathBuf {
        self.path.join("state")
    }

    fn endpoint(&self) -> PathBuf {
        self.runtime().join("bitty").join("bittyd.sock")
    }

    fn capability(&self) -> PathBuf {
        self.runtime().join("bitty").join("bittyd.operator")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The `bittyd start` process this test owns.
struct Daemon {
    child: Child,
}

impl Daemon {
    fn start(root: &TempRoot) -> Self {
        std::fs::create_dir_all(root.runtime()).expect("runtime dir");
        std::fs::set_permissions(root.runtime(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod runtime");
        std::fs::create_dir_all(root.state()).expect("state dir");
        let child = Command::new(BITTYD)
            .arg("start")
            .env("XDG_RUNTIME_DIR", root.runtime())
            .env("XDG_STATE_HOME", root.state())
            .env_remove("BITTYD_ENDPOINT")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bittyd start");
        let daemon = Self { child };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if root.endpoint().exists() {
                return daemon;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("bittyd did not bind {} in time", root.endpoint().display());
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Wait up to `budget` for the daemon to exit, then kill it.
    fn wait_within(mut self, budget: Duration) -> ExitStatus {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("bittyd did not exit within {budget:?}");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// One `bitty-ipc` v1 client connection with per-frame credentials.
struct Client {
    stream: UnixStream,
    next_id: u64,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect");
        // Linux attaches SCM_CREDENTIALS on send when the sending socket has
        // SO_PASSCRED; a client that skips this is refused as Unauthenticated.
        bitty_daemon::service::frame::enable_credentials(&stream).expect("per-frame credentials");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        Self { stream, next_id: 1 }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let request = json!({"v": 1, "id": id, "method": method, "params": params});
        let payload = serde_json::to_vec(&request).expect("encode request");
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        self.stream.write_all(&frame).expect("write frame");
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header).expect("read header");
        let len = u32::from_be_bytes(header) as usize;
        let mut body = vec![0u8; len];
        self.stream.read_exact(&mut body).expect("read payload");
        serde_json::from_slice(&body).expect("response json")
    }

    fn negotiate(&mut self, operator_capability: Option<&str>) -> Value {
        self.negotiate_full(operator_capability, None, None)
    }

    fn negotiate_full(
        &mut self,
        operator_capability: Option<&str>,
        host_grant: Option<&str>,
        client_correlation: Option<&str>,
    ) -> Value {
        let mut params = json!({"client_version": {"major": 1, "minor": 0}});
        if let Some(capability) = operator_capability {
            params["operator_capability"] = Value::String(capability.to_owned());
        }
        if let Some(grant) = host_grant {
            params["host_grant"] = Value::String(grant.to_owned());
        }
        if let Some(correlation) = client_correlation {
            params["client_correlation"] = Value::String(correlation.to_owned());
        }
        self.call("daemon.negotiate", params)
    }
}

/// One session created with host consent and claimed by its creator.
struct Consumer {
    client: Client,
    session_id: String,
    attach_id: String,
    control_token: String,
}

/// Read the per-boot operator capability once the daemon has written it.
///
/// The socket appears before the capability file, so a client that can connect
/// is not yet guaranteed a readable capability.
fn read_capability(root: &TempRoot) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::read_to_string(root.capability()) {
            Ok(raw) => return raw.trim_end_matches('\n').to_owned(),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("capability file: {error:?}"),
        }
    }
}

/// Lowercase hex, for the correlation digest and the content plane.
fn hex_lower(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// Assert a typed refusal and that no result body came with it.
fn assert_refused(value: &Value, class: &str, code: &str, context: &str) {
    assert_eq!(value["ok"], Value::Bool(false), "{context}: {value}");
    assert_eq!(value["error"]["class"], class, "{context}: {value}");
    assert_eq!(value["error"]["code"], code, "{context}: {value}");
    assert!(
        value.get("result").is_none(),
        "{context} must carry no result body: {value}"
    );
}

/// The scope names a handshake advertised.
fn scopes_of(handshake: &Value) -> Vec<String> {
    handshake["result"]["scopes"]
        .as_array()
        .expect("the handshake advertises its scopes")
        .iter()
        .map(|value| value.as_str().expect("a scope string").to_owned())
        .collect()
}

fn has_scope(handshake: &Value, scope: &str) -> bool {
    scopes_of(handshake).iter().any(|name| name == scope)
}

/// Mint a host-consent grant, optionally bound to one client's correlation
/// digest.
fn mint_host_grant(operator: &mut Client, correlation: Option<&str>) -> String {
    let mut params = json!({"action": "mint"});
    if let Some(value) = correlation {
        let digest = ClientCorrelation::parse(value)
            .expect("a correlation value")
            .digest();
        params["client_correlation_digest"] = Value::String(hex_lower(&digest));
    }
    let minted = operator.call("daemon.host_grant", params);
    assert_eq!(minted["ok"], true, "{minted}");
    minted["result"]["grant"]
        .as_str()
        .expect("a host grant bearer")
        .to_owned()
}

/// Connect with a host grant, create a terminal session, and claim control.
fn controlled_consumer(root: &TempRoot, host_grant: &str, label: &str) -> Consumer {
    let mut client = Client::connect(&root.endpoint());
    let handshake = client.negotiate_full(None, Some(host_grant), None);
    assert_eq!(handshake["ok"], true, "{handshake}");
    assert!(
        has_scope(&handshake, "session.host"),
        "a consumed host grant adds session.host, never more: {handshake}"
    );
    assert!(
        !has_scope(&handshake, "daemon.control"),
        "a host grant must not carry operator authority: {handshake}"
    );

    let created = client.call(
        "session.create",
        json!({
            "kind": "terminal",
            "idempotency_key": format!("{label}-create"),
            "argv": ["/bin/cat"],
            "cwd": "/",
        }),
    );
    assert_eq!(created["ok"], true, "{created}");
    let session_id = created["result"]["session"]["session_id"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let create_token = created["result"]["control_token"]
        .as_str()
        .expect("a create token")
        .to_owned();

    let claimed = client.call(
        "session.attach",
        json!({
            "attach_role": "controller",
            "session_id": session_id.as_str(),
            "expected_generation": 1,
            "control_token": create_token,
            "idempotency_key": format!("{label}-claim"),
        }),
    );
    assert_eq!(claimed["ok"], true, "{claimed}");
    let attach_id = claimed["result"]["attach_id"]
        .as_str()
        .expect("a controller attach")
        .to_owned();
    let control_token = claimed["result"]["control_token"]
        .as_str()
        .expect("a rotated control token")
        .to_owned();

    Consumer {
        client,
        session_id,
        attach_id,
        control_token,
    }
}

/// Mint an observer grant on a session the caller controls.
fn mint_observer_grant(controller: &mut Client, session_id: &str, key: &str) -> (String, String) {
    let minted = controller.call(
        "session.grant",
        json!({"session_id": session_id, "idempotency_key": key}),
    );
    assert_eq!(minted["ok"], true, "{minted}");
    let grant_id = minted["result"]["grant_id"]
        .as_str()
        .expect("a grant id")
        .to_owned();
    let bearer = minted["result"]["grant"]
        .as_str()
        .expect("an observer grant bearer")
        .to_owned();
    (grant_id, bearer)
}

/// One `bittyd` CLI invocation.
#[derive(Debug)]
struct CliRun {
    code: i32,
    stderr: String,
}

fn run_bittyd(root: &TempRoot, args: &[&str]) -> CliRun {
    let output = Command::new(BITTYD)
        .args(args)
        .env("XDG_RUNTIME_DIR", root.runtime())
        .env("XDG_STATE_HOME", root.state())
        .env_remove("BITTYD_ENDPOINT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run bittyd");
    CliRun {
        code: output.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Replace the owner-private capability file with a well-formed but foreign
/// bearer, which is what a same-uid process without the real capability holds.
fn write_capability(root: &TempRoot, bearer: &str) {
    std::fs::write(root.capability(), format!("{bearer}\n")).expect("write the capability file");
    std::fs::set_permissions(root.capability(), std::fs::Permissions::from_mode(0o600))
        .expect("chmod the capability file");
}

/// (a) A controller of one session cannot read, write, subscribe to, resize,
/// or inspect another consumer's session.
///
/// Both consumers hold a live controller lease and every baseline scope, so
/// neither the scope stage nor the visibility relation can accidentally allow
/// the call: the only correct answer is the uniform `NotFound`, which reveals
/// nothing about whether the target exists. The caller's own session keeps
/// working in the same breath, so the denials are not a broken plane.
#[test]
fn a_controller_cannot_reach_another_consumers_content_plane() {
    bitty_test_support::require_pty!();
    let root = TempRoot::new("cross-content");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    let alpha_grant = mint_host_grant(&mut operator, None);
    let beta_grant = mint_host_grant(&mut operator, None);
    let mut alpha = controlled_consumer(&root, &alpha_grant, "deputy-alpha");
    let mut beta = controlled_consumer(&root, &beta_grant, "deputy-beta");
    let foreign = beta.session_id.clone();

    for (method, params) in [
        (
            "session.read",
            json!({"session_id": foreign.as_str(), "generation": 1, "max_bytes": 1024}),
        ),
        (
            "session.write",
            json!({"session_id": foreign.as_str(), "generation": 1, "data": "41"}),
        ),
        (
            "session.subscribe",
            json!({"session_id": foreign.as_str(), "generation": 1}),
        ),
        (
            "session.resize",
            json!({"session_id": foreign.as_str(), "generation": 1, "cols": 80, "rows": 24}),
        ),
        ("session.status", json!({"session_id": foreign.as_str()})),
    ] {
        let refused = alpha.client.call(method, params);
        assert_refused(&refused, "NotFound", "NotFound", method);
        assert_eq!(
            refused["error"]["message"], "target not found",
            "{method} must not describe the target: {refused}"
        );
        assert!(
            refused.get("data").is_none() && refused.get("bytes").is_none(),
            "{method} must carry no content: {refused}"
        );
        println!("CD_CROSS_CONSUMER {method} {refused}");
    }

    // The same connection still owns its own session, so the uniform refusal
    // above is about the target and not about the caller.
    let own = alpha.client.call(
        "session.read",
        json!({"session_id": alpha.session_id.as_str(), "generation": 1, "max_bytes": 1024}),
    );
    assert_eq!(own["ok"], true, "{own}");
    assert_eq!(own["result"]["is_untrusted_surface"], Value::Bool(true));
    assert_eq!(own["result"]["session_id"], alpha.session_id);

    // Beta never lost anything either.
    let beta_own = beta.client.call(
        "session.write",
        json!({"session_id": beta.session_id.as_str(), "generation": 1, "data": "42"}),
    );
    assert_eq!(beta_own["ok"], true, "{beta_own}");

    assert!(!alpha.control_token.is_empty(), "the claim rotated a token");
    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

/// (a) A connection cannot address another connection's lease handle, even
/// though `session.detach` is a baseline-scope verb (`session.control`) and the
/// handle id is a strong handle rather than a secret.
///
/// The attach id names a target, and the target lookup is what refuses: the
/// attacker's own control token does not turn a foreign handle into a visible
/// one, and the victim's lease is provably still live afterwards because its
/// own heartbeat and content read keep succeeding.
#[test]
fn an_attach_handle_owned_by_another_connection_is_never_found() {
    bitty_test_support::require_pty!();
    let root = TempRoot::new("cross-attach");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    let alpha_grant = mint_host_grant(&mut operator, None);
    let beta_grant = mint_host_grant(&mut operator, None);
    let mut alpha = controlled_consumer(&root, &alpha_grant, "handle-alpha");
    let mut beta = controlled_consumer(&root, &beta_grant, "handle-beta");
    let foreign_attach = beta.attach_id.clone();

    let detach = alpha.client.call(
        "session.detach",
        json!({"attach_id": foreign_attach.as_str()}),
    );
    assert_refused(&detach, "NotFound", "NotFound", "detach a foreign attach");
    println!("CD_FOREIGN_ATTACH session.detach {detach}");

    let beat = alpha.client.call(
        "lease.heartbeat",
        json!({"attach_id": foreign_attach.as_str()}),
    );
    assert_refused(&beat, "NotFound", "NotFound", "heartbeat a foreign attach");
    println!("CD_FOREIGN_ATTACH lease.heartbeat {beat}");

    // Presenting the attacker's own control token does not launder the handle.
    let with_token = alpha.client.call(
        "session.detach",
        json!({
            "attach_id": foreign_attach.as_str(),
            "control_token": alpha.control_token.as_str(),
        }),
    );
    assert_refused(
        &with_token,
        "NotFound",
        "NotFound",
        "detach a foreign attach with the attacker's own token",
    );

    // The victim's lease is intact: its own heartbeat and read still answer.
    let own_beat = beta.client.call(
        "lease.heartbeat",
        json!({"attach_id": foreign_attach.as_str()}),
    );
    assert_eq!(own_beat["ok"], true, "{own_beat}");
    assert_eq!(own_beat["result"]["attach_id"], foreign_attach);
    let own_read = beta.client.call(
        "session.read",
        json!({"session_id": beta.session_id.as_str(), "generation": 1, "max_bytes": 64}),
    );
    assert_eq!(own_read["ok"], true, "{own_read}");

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

/// (b) The baseline scope set includes `session.control`, yet it holds no grant
/// authority: it cannot mint host consent (the `session.host` scope it lacks)
/// and it cannot mint an observer grant for a session someone else controls.
///
/// The two refusals come from different stages on purpose. `daemon.host_grant`
/// is a `daemon.control` verb, refused at the scope stage with no target
/// lookup; `session.grant` is a `session.control` verb, so the scope stage
/// passes and the target stage answers the uniform `NotFound`. Neither one
/// becomes a grant, and the daemon is still serving afterwards.
#[test]
fn a_baseline_consumer_cannot_mint_authority_it_does_not_hold() {
    bitty_test_support::require_pty!();
    let root = TempRoot::new("narrow-mint");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    let owner_grant = mint_host_grant(&mut operator, None);
    let mut owner = controlled_consumer(&root, &owner_grant, "narrow-owner");

    let mut deputy = Client::connect(&root.endpoint());
    let handshake = deputy.negotiate(None);
    assert_eq!(handshake["ok"], true, "{handshake}");
    // The baseline really does carry the control verb, so the refusals below
    // are not an accident of a missing scope.
    assert!(has_scope(&handshake, "session.control"), "{handshake}");
    assert!(!has_scope(&handshake, "session.host"), "{handshake}");
    assert!(!has_scope(&handshake, "daemon.control"), "{handshake}");

    let unknown_session = "0".repeat(32);
    for (method, params) in [
        ("daemon.host_grant", json!({"action": "mint"})),
        (
            "daemon.host_grant",
            json!({"action": "revoke", "grant_id": unknown_session.as_str()}),
        ),
        ("daemon.status", json!({})),
        ("daemon.orphans.verify", json!({})),
        (
            "session.kill",
            json!({"session_id": owner.session_id.as_str(), "generation": 1}),
        ),
        ("daemon.stop", json!({"mode": "force"})),
    ] {
        let refused = deputy.call(method, params);
        assert_refused(&refused, "Denied", "ScopeViolation", method);
        println!("CD_NARROW_MINT {method} {refused}");
    }

    // `session.control` is enough to reach the target stage for `session.grant`
    // and not enough to reach another consumer's session.
    let grant_attempt = deputy.call(
        "session.grant",
        json!({
            "session_id": owner.session_id.as_str(),
            "idempotency_key": "deputy-grant",
        }),
    );
    assert_refused(&grant_attempt, "NotFound", "NotFound", "session.grant");
    println!("CD_NARROW_MINT session.grant {grant_attempt}");

    // The owner's session never grew a pending grant from that attempt.
    let owner_status = owner.client.call(
        "session.status",
        json!({"session_id": owner.session_id.as_str()}),
    );
    assert_eq!(owner_status["ok"], true, "{owner_status}");
    assert_eq!(
        owner_status["result"]["session"]["pending_observer_grants"], 0,
        "a refused mint must leave no grant behind: {owner_status}"
    );

    let status = operator.call("daemon.status", json!({}));
    assert_eq!(status["ok"], true, "the daemon still serves: {status}");
    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

/// (b) A host grant is single use and bound to one connection, so it cannot be
/// forwarded: the same bearer reused by a second connection adds no scope, and
/// a fresh bearer minted for one correlation cannot be consumed by a different
/// one.
///
/// The refused consume must not burn the bearer, which the last step proves by
/// consuming the very same grant from the connection it was minted for.
#[test]
fn a_host_grant_cannot_be_forwarded_to_another_connection() {
    bitty_test_support::require_pty!();
    let root = TempRoot::new("forward-host");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    // A grant bound to the first client's correlation digest.
    let first = mint_host_grant(&mut operator, Some("deputy-alpha-connection"));
    let mut alpha = Client::connect(&root.endpoint());
    let alpha_handshake = alpha.negotiate_full(None, Some(&first), Some("deputy-alpha-connection"));
    assert_eq!(alpha_handshake["ok"], true, "{alpha_handshake}");
    assert!(
        has_scope(&alpha_handshake, "session.host"),
        "the bound connection consumes its own grant: {alpha_handshake}"
    );
    let created = alpha.call(
        "session.create",
        json!({"kind": "terminal", "idempotency_key": "forward-alpha", "argv": ["/bin/cat"], "cwd": "/"}),
    );
    assert_eq!(created["ok"], true, "{created}");

    // Replaying the consumed bearer on a third connection adds no scope.
    let mut replay = Client::connect(&root.endpoint());
    let replay_handshake =
        replay.negotiate_full(None, Some(&first), Some("deputy-alpha-connection"));
    assert_eq!(replay_handshake["ok"], true, "{replay_handshake}");
    assert!(
        !has_scope(&replay_handshake, "session.host"),
        "a consumed grant must add no scope: {replay_handshake}"
    );
    let replay_create = replay.call(
        "session.create",
        json!({"kind": "terminal", "idempotency_key": "forward-replay", "argv": ["/bin/cat"], "cwd": "/"}),
    );
    assert_refused(
        &replay_create,
        "Denied",
        "ConsentRequired",
        "replayed grant",
    );
    println!("CD_FORWARD_HOST replay {replay_create}");

    // A fresh grant minted for the first client's digest, presented by a
    // connection with a different correlation: the binding refuses it.
    let second = mint_host_grant(&mut operator, Some("deputy-alpha-connection"));
    let mut beta = Client::connect(&root.endpoint());
    let beta_handshake = beta.negotiate_full(None, Some(&second), Some("deputy-beta-connection"));
    assert_eq!(beta_handshake["ok"], true, "{beta_handshake}");
    assert!(
        !has_scope(&beta_handshake, "session.host"),
        "a correlation-bound grant adds no scope to another connection: {beta_handshake}"
    );
    let beta_create = beta.call(
        "session.create",
        json!({"kind": "terminal", "idempotency_key": "forward-beta", "argv": ["/bin/cat"], "cwd": "/"}),
    );
    assert_refused(
        &beta_create,
        "Denied",
        "ConsentRequired",
        "forwarded correlation-bound grant",
    );
    println!("CD_FORWARD_HOST foreign-correlation {beta_create}");

    // The refused attempt did not spend the bearer: the connection it was
    // minted for consumes it and creates.
    let mut alpha_second = Client::connect(&root.endpoint());
    let second_handshake =
        alpha_second.negotiate_full(None, Some(&second), Some("deputy-alpha-connection"));
    assert_eq!(second_handshake["ok"], true, "{second_handshake}");
    assert!(
        has_scope(&second_handshake, "session.host"),
        "the bound connection consumes the untouched grant: {second_handshake}"
    );
    let second_create = alpha_second.call(
        "session.create",
        json!({"kind": "terminal", "idempotency_key": "forward-alpha-2", "argv": ["/bin/cat"], "cwd": "/"}),
    );
    assert_eq!(second_create["ok"], true, "{second_create}");

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

/// (b)/(e) An observer grant is a narrow, single-target authority: it cannot be
/// spent as control on the session it names, it cannot be re-aimed at a
/// different session, and the lease it produces cannot mint a further grant.
///
/// Every refusal is the typed stage answer (`InvalidControlToken`,
/// `ControllerRequired`, `ObserverGrantRequired`), and the grant is still
/// usable for exactly what it was minted for afterwards, so nothing was
/// laundered.
#[test]
fn an_observer_grant_cannot_become_control_or_be_re_aimed() {
    bitty_test_support::require_pty!();
    let root = TempRoot::new("observer-grant");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    let alpha_grant = mint_host_grant(&mut operator, None);
    let beta_grant = mint_host_grant(&mut operator, None);
    let mut alpha = controlled_consumer(&root, &alpha_grant, "observer-alpha");
    let mut beta = controlled_consumer(&root, &beta_grant, "observer-beta");

    let (_, bearer) = mint_observer_grant(&mut alpha.client, &alpha.session_id, "obs-1");

    // The grant is not a control credential: a connection presenting it as one
    // is outside the visibility relation and gets the uniform NotFound.
    let mut outsider = Client::connect(&root.endpoint());
    assert_eq!(outsider.negotiate(None)["ok"], true);
    let as_control = outsider.call(
        "session.attach",
        json!({
            "attach_role": "controller",
            "session_id": alpha.session_id.as_str(),
            "expected_generation": 1,
            "observer_grant": bearer.as_str(),
            "idempotency_key": "obs-as-control",
        }),
    );
    assert_refused(
        &as_control,
        "NotFound",
        "NotFound",
        "an observer grant presented as a control claim",
    );
    println!("CD_OBSERVER_GRANT control-claim {as_control}");

    // The grant names one session: re-aimed at another, it is unusable.
    let (_, beta_bearer) = mint_observer_grant(&mut beta.client, &beta.session_id, "obs-beta");
    let mut re_aim = Client::connect(&root.endpoint());
    assert_eq!(re_aim.negotiate(None)["ok"], true);
    let wrong_target = re_aim.call(
        "session.attach",
        json!({
            "attach_role": "observer",
            "session_id": alpha.session_id.as_str(),
            "observer_grant": beta_bearer.as_str(),
        }),
    );
    assert_refused(
        &wrong_target,
        "Denied",
        "ObserverGrantRequired",
        "an observer grant re-aimed at another session",
    );
    println!("CD_OBSERVER_GRANT re-aimed {wrong_target}");

    // The mis-aimed attempt did not spend it: the same bearer attaches to the
    // session it names.
    let attached = re_aim.call(
        "session.attach",
        json!({
            "attach_role": "observer",
            "session_id": beta.session_id.as_str(),
            "observer_grant": beta_bearer.as_str(),
        }),
    );
    assert_eq!(attached["ok"], true, "{attached}");
    assert_eq!(attached["result"]["role"], "observer");
    let observer_attach = attached["result"]["attach_id"]
        .as_str()
        .expect("an observer attach")
        .to_owned();

    // The narrow lease cannot widen itself: minting, revoking another lease,
    // and resizing all answer the one control-stage refusal.
    for (method, params) in [
        (
            "session.grant",
            json!({"session_id": beta.session_id.as_str(), "idempotency_key": "obs-escalate"}),
        ),
        (
            "session.control.transfer",
            json!({
                "session_id": beta.session_id.as_str(),
                "attach_id": observer_attach.as_str(),
                "generation": 1,
                "idempotency_key": "obs-transfer",
            }),
        ),
        (
            "session.detach",
            json!({"attach_id": beta.attach_id.as_str()}),
        ),
        (
            "session.abort",
            json!({
                "session_id": beta.session_id.as_str(),
                "generation": 1,
                "reason_class": "cancel",
            }),
        ),
    ] {
        let refused = re_aim.call(method, params);
        assert_refused(&refused, "Denied", "ControllerRequired", method);
        println!("CD_OBSERVER_GRANT {method} {refused}");
    }

    // The observer's own content plane is still the controller's, not theirs.
    let read = re_aim.call(
        "session.read",
        json!({"session_id": beta.session_id.as_str(), "generation": 1, "max_bytes": 64}),
    );
    assert_refused(&read, "Denied", "ControllerRequired", "observer read");
    println!("CD_OBSERVER_GRANT session.read {read}");

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

/// (c) The operator-less CLI surface inherits no daemon authority.
///
/// Two shapes are exercised. On the wire, a connection that presents no
/// capability is refused `Denied/ScopeViolation` for `daemon.stop` while the
/// daemon keeps serving. On the CLI, the only endpoint-speaking verb is
/// `bittyd stop`, and with a well-formed but foreign capability file it exits
/// `6` (the authentication class) without stopping anything; the same verb with
/// the real capability stops the daemon, so the exit `6` above is the
/// capability and not the transport. A `ctl`-shaped subcommand does not exist
/// on this binary at all (usage exit `2`), so there is no unauthenticated
/// control surface to inherit authority through.
#[test]
fn an_operator_less_cli_cannot_stop_the_daemon() {
    let root = TempRoot::new("operatorless-cli");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let pid = daemon.pid();

    // Wire shape: the baseline scope set has no `daemon.control`.
    let mut deputy = Client::connect(&root.endpoint());
    let handshake = deputy.negotiate(None);
    assert_eq!(handshake["ok"], true, "{handshake}");
    assert!(!has_scope(&handshake, "daemon.control"), "{handshake}");
    let refused = deputy.call("daemon.stop", json!({"mode": "force"}));
    assert_refused(&refused, "Denied", "ScopeViolation", "daemon.stop");
    println!("CD_OPERATORLESS wire {refused}");

    // CLI shape: a foreign but well-formed capability is the authentication
    // class, and it stops nothing.
    write_capability(&root, &"a".repeat(64));
    let foreign = run_bittyd(&root, &["stop", "--force"]);
    assert_eq!(foreign.code, 6, "{foreign:?}");
    assert!(
        foreign.stderr.contains("operator authentication failed"),
        "{foreign:?}"
    );
    println!(
        "CD_OPERATORLESS cli-foreign code={} stderr={:?}",
        foreign.code, foreign.stderr
    );

    // The daemon is alive and unchanged: the same pid still answers the real
    // capability.
    assert!(root.endpoint().exists(), "the endpoint survived");
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let status = operator.call("daemon.status", json!({}));
    assert_eq!(status["ok"], true, "{status}");
    assert_eq!(status["result"]["daemon_pid"], json!(pid));

    // There is no `ctl` verb to try: the daemon binary rejects it as usage.
    let ctl = run_bittyd(&root, &["ctl"]);
    assert_eq!(ctl.code, 2, "{ctl:?}");
    assert!(ctl.stderr.contains("usage"), "{ctl:?}");
    println!("CD_OPERATORLESS cli-ctl code={} ", ctl.code);

    // With the real capability restored the same verb does stop the daemon, so
    // the refusals above were authorization and not a broken transport.
    write_capability(&root, &capability);
    let real = run_bittyd(&root, &["stop", "--force"]);
    assert_eq!(real.code, 0, "{real:?}");
    assert_eq!(daemon.wait_within(Duration::from_secs(10)).code(), Some(0));
}

/// (d) The server decides the consumer's identity. Two shapes are proved, and
/// they are not the same refusal:
///
/// - a request that spells ambient authority (`role`, `scope`, or `auth` at the
///   params top level) is refused by the envelope guard with
///   `Validation/InvalidRequest` before any dispatch, on `daemon.negotiate` and
///   on an ordinary method alike, so a forged role never reaches a handler;
/// - every other trust-shaped spelling (`consumer_kind`, `is_trusted`,
///   `trusted`, `operator`, `scopes`, `service_scopes`, `transport_scopes`,
///   `capabilities`) is an unknown field: the request is served and the
///   handshake's scope set, capabilities, and server-assigned client id are
///   exactly what a plain consumer gets, so the effective set does not move.
///
/// The forged connection is then exercised: create is `ConsentRequired`,
/// `daemon.status` is `ScopeViolation`, and an observer attach carrying the
/// trust fields still produces an observer lease whose read is
/// `ControllerRequired`.
#[test]
fn a_client_supplied_consumer_identity_does_not_change_the_effective_scopes() {
    bitty_test_support::require_pty!();
    let root = TempRoot::new("forged-identity");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    let owner_grant = mint_host_grant(&mut operator, None);
    let mut owner = controlled_consumer(&root, &owner_grant, "forged-owner");

    // Shape 1: an ambient-authority spelling is a typed refusal at the envelope
    // guard. Each attempt gets its own connection because a refused negotiate
    // is terminal, and the guard applies to every method, not only to
    // `daemon.negotiate`.
    for (method, params, context) in [
        (
            "daemon.negotiate",
            json!({
                "client_version": {"major": 1, "minor": 0},
                "role": "controller",
                "scopes": ["daemon.control"],
            }),
            "daemon.negotiate with role",
        ),
        (
            "daemon.negotiate",
            json!({
                "client_version": {"major": 1, "minor": 0},
                "scope": "daemon.control",
            }),
            "daemon.negotiate with scope",
        ),
    ] {
        let mut attempt = Client::connect(&root.endpoint());
        let refused = attempt.call(method, params);
        assert_refused(&refused, "Validation", "InvalidRequest", context);
        println!("CD_FORGED_IDENTITY {context} {refused}");
    }

    // Shape 2: the fields the wire schema does not know are ignored, and the
    // whole authority-bearing part of the handshake is server-decided.
    let mut plain = Client::connect(&root.endpoint());
    let plain_handshake = plain.negotiate(None);
    assert_eq!(plain_handshake["ok"], true, "{plain_handshake}");

    let mut forged = Client::connect(&root.endpoint());
    let forged_handshake = forged.call(
        "daemon.negotiate",
        json!({
            "client_version": {"major": 1, "minor": 0},
            "consumer_kind": "operator",
            "consumer_kind_name": "operator",
            "is_trusted": true,
            "trusted": true,
            "operator": true,
            "scopes": ["daemon.control", "daemon.inspect", "session.host"],
            "service_scopes": ["daemon.control", "session.host"],
            "transport_scopes": ["debug.control", "config.modify"],
            "capabilities": {"events": true, "sessions": true, "trace": true},
        }),
    );
    assert_eq!(forged_handshake["ok"], true, "{forged_handshake}");

    assert_eq!(
        scopes_of(&forged_handshake),
        scopes_of(&plain_handshake),
        "a forged identity must not change the scope set"
    );
    assert_eq!(
        forged_handshake["result"]["capabilities"], plain_handshake["result"]["capabilities"],
        "a forged identity must not change the capabilities"
    );
    assert_eq!(
        forged_handshake["result"]["daemon_instance_id"],
        plain_handshake["result"]["daemon_instance_id"]
    );
    let forged_client = forged_handshake["result"]["client_id"]
        .as_str()
        .expect("a server-assigned client id");
    assert_eq!(forged_client.len(), 32, "a 128-bit server id");
    assert_ne!(
        forged_handshake["result"]["client_id"], plain_handshake["result"]["client_id"],
        "the server assigns each connection its own id"
    );
    assert!(!has_scope(&forged_handshake, "daemon.control"));
    assert!(!has_scope(&forged_handshake, "session.host"));
    assert!(!has_scope(&forged_handshake, "debug.control"));
    println!(
        "CD_FORGED_IDENTITY scopes={:?}",
        scopes_of(&forged_handshake)
    );

    let created = forged.call(
        "session.create",
        json!({"kind": "terminal", "idempotency_key": "forged-create", "argv": ["/bin/cat"], "cwd": "/"}),
    );
    assert_refused(&created, "Denied", "ConsentRequired", "forged create");
    let status = forged.call("daemon.status", json!({}));
    assert_refused(&status, "Denied", "ScopeViolation", "forged daemon.status");
    let list = forged.call("session.list", json!({}));
    assert_eq!(list["ok"], true, "{list}");
    assert_eq!(
        list["result"]["sessions"].as_array().map(Vec::len),
        Some(0),
        "a connection with no lease sees no sessions: {list}"
    );
    println!("CD_FORGED_IDENTITY create {created}");

    // The same forgery on the attach path: the lease role is the one the
    // method's own argument selects, and no trust field can raise it. A
    // `role`-shaped attach is refused at the envelope guard.
    let (_id, bearer) = mint_observer_grant(&mut owner.client, &owner.session_id, "forged-obs");
    let role_attach = forged.call(
        "session.attach",
        json!({
            "attach_role": "controller",
            "session_id": owner.session_id.as_str(),
            "expected_generation": 1,
            "observer_grant": bearer.as_str(),
            "role": "controller",
        }),
    );
    assert_refused(
        &role_attach,
        "Validation",
        "InvalidRequest",
        "an attach that spells role",
    );
    println!("CD_FORGED_IDENTITY attach-role {role_attach}");

    let attached = forged.call(
        "session.attach",
        json!({
            "attach_role": "observer",
            "session_id": owner.session_id.as_str(),
            "observer_grant": bearer.as_str(),
            "is_trusted": true,
            "consumer_kind": "operator",
            "trusted": true,
            "attach_id": "0".repeat(32),
        }),
    );
    assert_eq!(attached["ok"], true, "{attached}");
    assert_eq!(
        attached["result"]["role"], "observer",
        "the forged kind must not raise the lease: {attached}"
    );
    let attach_scopes: Vec<String> = attached["result"]["scopes"]
        .as_array()
        .expect("the attach result carries the connection scopes")
        .iter()
        .map(|value| value.as_str().expect("a scope string").to_owned())
        .collect();
    assert!(!attach_scopes.iter().any(|name| name == "daemon.control"));
    assert!(!attach_scopes.iter().any(|name| name == "session.host"));
    let read = forged.call(
        "session.read",
        json!({"session_id": owner.session_id.as_str(), "generation": 1, "max_bytes": 64}),
    );
    assert_refused(
        &read,
        "Denied",
        "ControllerRequired",
        "forged observer read",
    );
    println!("CD_FORGED_IDENTITY attach {attached}");

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

/// (e) Authority accumulated through a self-created session is still bounded by
/// that session.
///
/// A consumer that holds host consent, the baseline scopes, a controller lease,
/// and an attached observer has more authority than a fresh connection and
/// still reaches nothing else: no `session.kill`, no `daemon.*`, no foreign
/// session metadata, no foreign grant revoke, and no foreign content. The
/// observer lane stays `ControllerRequired` on the session it observes and
/// `NotFound` on a session it does not, which is the two-code split the
/// criterion needs: "denied" on a visible target, "not found" on an invisible
/// one.
#[test]
fn authority_accumulated_by_a_low_scope_consumer_stops_at_its_own_target() {
    bitty_test_support::require_pty!();
    let root = TempRoot::new("laundering");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    let alpha_grant = mint_host_grant(&mut operator, None);
    let beta_grant = mint_host_grant(&mut operator, None);
    let mut alpha = controlled_consumer(&root, &alpha_grant, "launder-alpha");
    let mut beta = controlled_consumer(&root, &beta_grant, "launder-beta");

    // Beta's own pending grant, whose id the attacker is given on purpose: an
    // id is not authority.
    let (beta_grant_id, _) =
        mint_observer_grant(&mut beta.client, &beta.session_id, "launder-beta-grant");

    // The strong verbs stay out of reach of the accumulated consumer.
    for (method, params) in [
        (
            "session.kill",
            json!({"session_id": alpha.session_id.as_str(), "generation": 1}),
        ),
        ("daemon.status", json!({})),
        ("daemon.host_grant", json!({"action": "mint"})),
        ("daemon.stop", json!({"mode": "force"})),
    ] {
        let refused = alpha.client.call(method, params);
        assert_refused(&refused, "Denied", "ScopeViolation", method);
        println!("CD_LAUNDER {method} {refused}");
    }

    // Another consumer's session and grant are addressed targets: the uniform
    // NotFound, before any authority or existence answer.
    for (method, params) in [
        (
            "session.status",
            json!({"session_id": beta.session_id.as_str()}),
        ),
        (
            "session.grant",
            json!({
                "session_id": beta.session_id.as_str(),
                "idempotency_key": "launder-foreign-grant",
            }),
        ),
        (
            "session.grant.revoke",
            json!({
                "session_id": beta.session_id.as_str(),
                "grant_id": beta_grant_id.as_str(),
            }),
        ),
        (
            "session.read",
            json!({"session_id": beta.session_id.as_str(), "generation": 1, "max_bytes": 64}),
        ),
        (
            "session.detach",
            json!({"attach_id": beta.attach_id.as_str()}),
        ),
    ] {
        let refused = alpha.client.call(method, params);
        assert_refused(&refused, "NotFound", "NotFound", method);
        println!("CD_LAUNDER {method} {refused}");
    }

    // The grant is still pending and the beta lease is still live: none of the
    // attempts applied anything.
    let beta_status = beta.client.call(
        "session.status",
        json!({"session_id": beta.session_id.as_str()}),
    );
    assert_eq!(beta_status["ok"], true, "{beta_status}");
    assert_eq!(
        beta_status["result"]["session"]["pending_observer_grants"], 1,
        "a foreign revoke must not retire a pending grant: {beta_status}"
    );
    let beta_beat = beta.client.call(
        "lease.heartbeat",
        json!({"attach_id": beta.attach_id.as_str()}),
    );
    assert_eq!(beta_beat["ok"], true, "{beta_beat}");

    // The list view is per-lease, not per-daemon: the attacker sees one session
    // and it is its own, without the operator-only owner block.
    let list = alpha.client.call("session.list", json!({}));
    assert_eq!(list["ok"], true, "{list}");
    let sessions = list["result"]["sessions"]
        .as_array()
        .expect("a sessions array");
    assert_eq!(sessions.len(), 1, "{list}");
    assert_eq!(sessions[0]["session_id"], alpha.session_id);
    assert!(
        sessions[0].get("owner").is_none(),
        "the owner block is operator-only: {list}"
    );

    // The observer lane on the observed session: denied, but denied as an
    // authority answer rather than as a missing target.
    let mut observer = Client::connect(&root.endpoint());
    assert_eq!(observer.negotiate(None)["ok"], true);
    let (_, own_bearer) = mint_observer_grant(&mut alpha.client, &alpha.session_id, "launder-own");
    let attached = observer.call(
        "session.attach",
        json!({
            "attach_role": "observer",
            "session_id": alpha.session_id.as_str(),
            "observer_grant": own_bearer.as_str(),
        }),
    );
    assert_eq!(attached["ok"], true, "{attached}");
    let same_target = observer.call(
        "session.read",
        json!({"session_id": alpha.session_id.as_str(), "generation": 1, "max_bytes": 64}),
    );
    assert_refused(
        &same_target,
        "Denied",
        "ControllerRequired",
        "observer read on the observed session",
    );
    let other_target = observer.call(
        "session.read",
        json!({"session_id": beta.session_id.as_str(), "generation": 1, "max_bytes": 64}),
    );
    assert_refused(
        &other_target,
        "NotFound",
        "NotFound",
        "observer read on a session it does not observe",
    );
    println!("CD_LAUNDER observer-read-observed {same_target}");
    println!("CD_LAUNDER observer-read-other {other_target}");

    // The observer's own view is still exactly one session.
    let observer_list = observer.call("session.list", json!({}));
    assert_eq!(observer_list["ok"], true, "{observer_list}");
    assert_eq!(
        observer_list["result"]["sessions"].as_array().map(Vec::len),
        Some(1),
        "{observer_list}"
    );

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

/// Stop the daemon through its operator path and wait for the exit.
fn stop_cleanly(root: &TempRoot, daemon: Daemon) -> ExitStatus {
    let capability = read_capability(root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    daemon.wait_within(Duration::from_secs(10))
}
