//! Driver trait, expectation engine, and per-vector execution.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::harness::corpus::{AuditExpect, DriverKind, Expect, RequestLogExpect, Step, Vector};

/// Variables captured from responses and seeded at setup.
pub type Vars = BTreeMap<String, Value>;

/// One error response, reduced to the typed fields the corpus asserts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorReply {
    pub class: String,
    pub code: String,
    pub counts: Option<(u64, u64)>,
}

/// One successful or refused reply.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub ok: bool,
    pub result: Value,
    pub error: Option<ErrorReply>,
}

/// What one call returned.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A complete response envelope.
    Reply(Reply),
    /// The transport closed without a reply.
    Closed,
}

/// Facts recorded once per vector for the evidence row.
#[derive(Debug, Clone, Default)]
pub struct ServiceFacts {
    pub instance_id: String,
    pub cell: String,
    pub version: String,
    pub capabilities: BTreeMap<String, bool>,
    pub limits: BTreeMap<String, u64>,
}

/// One request the driver actually sent.
#[derive(Debug, Clone)]
pub struct RequestRecord {
    pub method: String,
    pub params: Value,
}

/// The observed result of a one-shot command (the daemon-absence vector).
#[derive(Debug, Clone)]
pub struct ExitObservation {
    pub code: i32,
    pub stdout: String,
}

/// One service under test.
pub trait Driver {
    fn kind(&self) -> DriverKind;

    /// Start a fresh instance and return the seeded variables.
    ///
    /// # Errors
    ///
    /// A static message when the instance cannot start; the row is a failure.
    fn prepare(&mut self, vector: &Vector) -> Result<Vars, String>;

    fn call(&mut self, connection: &str, method: &str, params: Value) -> Outcome;

    /// Send a request and drop the connection without reading the reply.
    fn call_dropping(&mut self, connection: &str, method: &str, params: Value) -> Outcome;

    /// Whether the connection reaches EOF within the harness budget.
    fn expect_eof(&mut self, connection: &str) -> bool;

    /// One peer process negotiates, sends the request, and exits before the
    /// service dispatches it. The service must refuse the privileged action.
    ///
    /// # Errors
    ///
    /// A static message when the helper cannot be run.
    fn peer_exit(&mut self, connection: &str, method: &str, params: &Value) -> Result<(), String>;

    /// Send `count` requests without reading, until the outbound cap closes.
    ///
    /// # Errors
    ///
    /// A static message when the connection does not close.
    fn queue_pressure(
        &mut self,
        connection: &str,
        method: &str,
        count: usize,
        params: &Value,
    ) -> Outcome;

    /// Send `count` requests of `method` on one connection, asserting each is
    /// the typed refusal `code`.
    ///
    /// # Errors
    ///
    /// A static message when a response does not match.
    fn refusal_burst(
        &mut self,
        connection: &str,
        method: &str,
        count: usize,
        code: &str,
    ) -> Result<(), String>;

    fn oversized_frame(&mut self, connection: &str, declared: u32) -> Outcome;

    fn partial_frame(&mut self, connection: &str, declared: u32, body: usize) -> Outcome;

    /// Send one complete raw request envelope, including its own `id`, and
    /// read the reply. Used where the envelope itself is the subject (an
    /// over-long `id`) and cannot be built by the typed call path. When
    /// `expect_eof` is set, the socket must reach EOF after the reply.
    fn raw_request(&mut self, connection: &str, request: Value, expect_eof: bool) -> Outcome;

    /// Write a `daemon.negotiate` and a second request in one batch, read the
    /// handshake response and the typed refusal, then require EOF. With
    /// `partial_body` set, only the leading `4 + partial_body` bytes of the
    /// second frame are written first and the tail after the refusal, so a
    /// partially sent frame cannot resume as a fresh request. Returns the
    /// refusal.
    fn pipelined_negotiate(
        &mut self,
        connection: &str,
        second: &Value,
        partial_body: Option<usize>,
    ) -> Outcome;

    /// Replace the instance; every connection is gone afterwards.
    ///
    /// # Errors
    ///
    /// A static message when the instance cannot be restarted.
    fn restart(&mut self) -> Result<Vars, String>;

    /// Kill the running instance without a graceful stop.
    ///
    /// # Errors
    ///
    /// A static message when the instance cannot be killed.
    fn crash(&mut self) -> Result<(), String>;

    fn advance_clock(&mut self, millis: u64);

    fn audit_text(&self) -> String;

    fn file_mode(&self, role: &str) -> Option<u32>;

    /// # Errors
    ///
    /// A static message when a listener is found or cannot be checked.
    fn no_tcp_listener(&self) -> Result<(), String>;

    /// Run the plain binary with no endpoint.
    ///
    /// # Errors
    ///
    /// A static message when the invocation cannot be observed.
    fn daemon_absent(&mut self) -> Result<ExitObservation, String>;

    fn requests(&self) -> Vec<RequestRecord>;

    fn facts(&self) -> ServiceFacts;

    /// Stop and reap the instance.
    fn finish(&mut self);
}

/// Seed the variables a corpus vector needs to stay cell-independent.
///
/// `worker_sessions` is the served capability of the cell the driver models, so
/// a handshake vector asserts what that driver must advertise instead of a
/// Linux-only surface on Windows or macOS. The fake models a served reference
/// cell and passes `true`; the real driver passes the capability its probe
/// negotiated. `cwd` is an absolute directory the host really has: the spawn
/// contract requires an absolute path, and `"/"` is not absolute on Windows.
pub fn cell_vars(vars: &mut Vars, worker_sessions: bool) {
    vars.insert("worker_sessions".to_owned(), Value::Bool(worker_sessions));
    vars.insert("cwd".to_owned(), Value::String(absolute_cwd()));
}

/// An absolute, UTF-8 working directory the host really has.
///
/// `std::env::temp_dir()` honors `TMPDIR`, which may be relative; resolve it
/// against the current directory so the spawn specification stays valid. A
/// non-UTF-8 path is rendered lossily rather than panicking in every driver
/// preparation.
fn absolute_cwd() -> String {
    let temp = std::env::temp_dir();
    let absolute = if temp.is_absolute() {
        temp
    } else {
        std::env::current_dir().map_or_else(
            |_| {
                if cfg!(windows) {
                    std::path::PathBuf::from(r"C:\")
                } else {
                    std::path::PathBuf::from("/")
                }
            },
            |cwd| cwd.join(&temp),
        )
    };
    absolute.to_string_lossy().into_owned()
}

/// One row of the evidence file.
#[derive(Debug, Clone)]
pub struct VectorOutcome {
    pub id: String,
    pub title: String,
    pub area: String,
    pub decisions: Vec<String>,
    pub driver: DriverKind,
    pub status: RowStatus,
    pub skip_reason: Option<String>,
    pub deferred: Option<String>,
    pub known_defect: Option<String>,
    pub instance_id: String,
    pub steps: usize,
    pub signature: Vec<String>,
    pub failure_step: Option<usize>,
    pub failure: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    Pass,
    Skip,
    Fail,
    KnownDefect,
}

impl RowStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Skip => "skip",
            Self::Fail => "fail",
            Self::KnownDefect => "known-defect",
        }
    }
}

/// Run every vector assigned to `driver` and collect the rows.
pub fn run_corpus(
    corpus: &crate::harness::corpus::Corpus,
    driver: &mut dyn Driver,
) -> Vec<VectorOutcome> {
    corpus
        .vectors
        .iter()
        .map(|vector| run_vector(vector, driver))
        .collect()
}

/// Run one vector and tear the instance down.
pub fn run_vector(vector: &Vector, driver: &mut dyn Driver) -> VectorOutcome {
    if !vector.runs_on(driver.kind()) {
        return VectorOutcome {
            id: vector.id.clone(),
            title: vector.title.clone(),
            area: vector.area.clone(),
            decisions: vector.decisions.clone(),
            driver: driver.kind(),
            status: RowStatus::Skip,
            skip_reason: Some(
                vector
                    .skip_reason(driver.kind())
                    .unwrap_or("driver-not-required")
                    .to_owned(),
            ),
            deferred: vector.deferred.clone(),
            known_defect: None,
            instance_id: String::new(),
            steps: 0,
            signature: Vec::new(),
            failure_step: None,
            failure: None,
        };
    }
    let mut row = VectorOutcome {
        id: vector.id.clone(),
        title: vector.title.clone(),
        area: vector.area.clone(),
        decisions: vector.decisions.clone(),
        driver: driver.kind(),
        status: RowStatus::Pass,
        skip_reason: vector.skip_reason(driver.kind()).map(str::to_owned),
        deferred: vector.deferred.clone(),
        known_defect: known_defect_marker(vector, driver.kind()),
        instance_id: String::new(),
        steps: 0,
        signature: Vec::new(),
        failure_step: None,
        failure: None,
    };
    if row.skip_reason.is_some() {
        row.status = RowStatus::Skip;
        return row;
    }
    let mut vars = match driver.prepare(vector) {
        Ok(vars) => vars,
        Err(error) => {
            row.status = RowStatus::Fail;
            row.failure = Some(format!("setup: {error}"));
            driver.finish();
            return row;
        }
    };
    // A cell that does not serve a required capability is a typed skip, not a
    // failure: a real Linux cell whose containment probe selected no mechanism
    // refuses the worker path, while the reference fake models a served cell.
    let facts = driver.facts();
    if let Some(missing) = vector
        .requires
        .iter()
        .find(|capability| facts.capabilities.get(*capability).copied() != Some(true))
    {
        row.status = RowStatus::Skip;
        row.skip_reason = Some(format!("cell-not-servable: {missing}"));
        driver.finish();
        return row;
    }
    for (index, step) in vector.steps.iter().chain(vector.state.iter()).enumerate() {
        row.steps += 1;
        match execute_step(step, driver, &mut vars) {
            Ok(signature) => row.signature.push(format!("{index}:{signature}")),
            Err(error) => {
                row.status = RowStatus::Fail;
                row.failure = Some(error);
                row.failure_step = Some(index);
                row.signature.push(format!("{index}:error"));
                break;
            }
        }
    }
    row.instance_id = driver.facts().instance_id;
    driver.finish();
    let known = vector
        .known_defect
        .as_ref()
        .is_some_and(|defect| defect.drivers.contains(&driver.kind()));
    if row.status == RowStatus::Fail && known {
        let defect = vector.known_defect.as_ref().expect("checked");
        if known_defect_matches(defect, row.failure_step, row.failure.as_deref()) {
            row.status = RowStatus::KnownDefect;
        } else {
            row.failure = Some(format!(
                "known defect {} expected at step {} ({:?}), observed at {:?}: {}",
                defect.issue,
                defect.step,
                defect
                    .code
                    .as_deref()
                    .or(defect.path.as_deref())
                    .unwrap_or("?"),
                row.failure_step,
                row.failure.as_deref().unwrap_or("no failure")
            ));
        }
    } else if row.status == RowStatus::Pass && known {
        row.status = RowStatus::Fail;
        row.failure = Some(format!(
            "known defect {} no longer reproduces; remove the marker",
            vector
                .known_defect
                .as_ref()
                .map(|defect| defect.issue.as_str())
                .unwrap_or("?")
        ));
    }
    row
}

/// Whether the observed failure is the recorded known defect.
///
/// The step is exact; `code` and `path` must appear in the failure text. A
/// mismatch keeps the row a failure, so a regression at another step cannot be
/// masked by the marker.
pub fn known_defect_matches(
    defect: &crate::harness::corpus::KnownDefect,
    failure_step: Option<usize>,
    failure: Option<&str>,
) -> bool {
    if failure_step != Some(defect.step) {
        return false;
    }
    let text = failure.unwrap_or_default();
    defect.code.as_ref().is_none_or(|code| text.contains(code))
        && defect.path.as_ref().is_none_or(|path| text.contains(path))
}

fn known_defect_marker(vector: &Vector, driver: DriverKind) -> Option<String> {
    vector
        .known_defect
        .as_ref()
        .filter(|defect| defect.drivers.contains(&driver))
        .map(|defect| format!("{}: {}", defect.issue, defect.reason))
}

fn execute_step(step: &Step, driver: &mut dyn Driver, vars: &mut Vars) -> Result<String, String> {
    if let Some(action) = step.action.as_deref() {
        return execute_action(action, step, driver, vars).map(|()| format!("action:{action}"));
    }
    let connection = step
        .connection
        .as_deref()
        .ok_or_else(|| "method step without a connection".to_owned())?;
    let method = step
        .method
        .as_deref()
        .ok_or_else(|| "step without a method".to_owned())?;
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(step.expect.retry_ms);
    loop {
        let params = resolve(&step.params, vars);
        let outcome = match step.fault.as_deref() {
            None => driver.call(connection, method, params),
            Some("drop_before_reply") => driver.call_dropping(connection, method, params),
            Some(other) => return Err(format!("unknown fault {other:?}")),
        };
        match check_expect(&step.expect, &outcome, driver, connection, vars) {
            Ok(()) => {
                if let Outcome::Reply(reply) = &outcome {
                    if reply.ok {
                        for (name, path) in &step.capture {
                            let value = path_get(&reply.result, path)
                                .ok_or_else(|| format!("capture {name}: missing {path}"))?;
                            vars.insert(name.clone(), value.clone());
                        }
                    }
                }
                return Ok(signature(&outcome));
            }
            Err(error) => {
                if step.expect.retry_ms == 0 || std::time::Instant::now() >= deadline {
                    return Err(format!("{method}: {error}"));
                }
                // A bounded settlement for an effect the service applies on
                // another connection; the outcome is deterministic.
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

fn execute_action(
    action: &str,
    step: &Step,
    driver: &mut dyn Driver,
    vars: &mut Vars,
) -> Result<(), String> {
    let connection = step.connection.as_deref().unwrap_or("default");
    match action {
        "restart" => {
            let seeded = driver.restart()?;
            vars.extend(seeded);
            Ok(())
        }
        "crash" => driver.crash(),
        "advance_clock" => {
            let millis = step
                .args
                .get("ms")
                .and_then(Value::as_u64)
                .ok_or_else(|| "advance_clock needs args.ms".to_owned())?;
            driver.advance_clock(millis);
            check_clock_advance(&step.expect)
        }
        "oversized_frame" => {
            let declared = arg_u32(step, "declared_len")?;
            let outcome = driver.oversized_frame(connection, declared);
            check_expect(&step.expect, &outcome, driver, connection, vars)
                .map_err(|error| format!("oversized_frame: {error}"))
        }
        "partial_frame" => {
            let declared = arg_u32(step, "declared_len")?;
            let body = arg_usize(step, "body_bytes")?;
            let outcome = driver.partial_frame(connection, declared, body);
            check_expect(&step.expect, &outcome, driver, connection, vars)
                .map_err(|error| format!("partial_frame: {error}"))
        }
        "raw_request" => {
            let request = step
                .args
                .get("request")
                .cloned()
                .ok_or_else(|| "raw_request needs args.request".to_owned())?;
            let expect_eof = step
                .args
                .get("expect_eof")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let outcome = driver.raw_request(connection, request, expect_eof);
            check_expect(&step.expect, &outcome, driver, connection, vars)
                .map_err(|error| format!("raw_request: {error}"))
        }
        "pipelined_negotiate" => {
            let second = step
                .args
                .get("second")
                .cloned()
                .ok_or_else(|| "pipelined_negotiate needs args.second".to_owned())?;
            let partial_body = step
                .args
                .get("partial_body")
                .and_then(Value::as_u64)
                .map(|body| body as usize);
            let outcome = driver.pipelined_negotiate(connection, &second, partial_body);
            check_expect(&step.expect, &outcome, driver, connection, vars)
                .map_err(|error| format!("pipelined_negotiate: {error}"))
        }
        "audit" => {
            let audit = step
                .expect
                .audit
                .as_ref()
                .ok_or_else(|| "audit action without expect.audit".to_owned())?;
            check_audit(audit, &driver.audit_text(), vars)
        }
        "peer_exit" => {
            let method = arg_string(step, "method")?;
            let params = step.args.get("params").cloned().unwrap_or(Value::Null);
            driver.peer_exit(connection, method, &params)?;
            if let Some(audit) = step.expect.audit.as_ref() {
                check_audit(audit, &driver.audit_text(), vars)?;
            }
            Ok(())
        }
        "queue_pressure" => {
            let method = arg_string(step, "method")?;
            let count = arg_usize(step, "count")?;
            let params = step.args.get("params").cloned().unwrap_or(Value::Null);
            let outcome = driver.queue_pressure(connection, method, count, &params);
            check_expect(&step.expect, &outcome, driver, connection, vars)
                .map_err(|error| format!("queue_pressure: {error}"))
        }
        "refusal_burst" => {
            let method = arg_string(step, "method")?;
            let count = arg_usize(step, "count")?;
            let code = step
                .expect
                .error_code
                .as_deref()
                .ok_or_else(|| "refusal_burst needs expect.error_code".to_owned())?;
            driver.refusal_burst(connection, method, count, code)
        }
        "file_mode" => {
            let role = arg_string(step, "role")?;
            let mode = driver
                .file_mode(role)
                .ok_or_else(|| format!("no file role {role:?} on this driver"))?;
            let expected = step
                .expect
                .file_mode
                .ok_or_else(|| "file_mode action without expect.file_mode".to_owned())?;
            if mode != expected {
                return Err(format!("{role} mode {mode:o}, expected {expected:o}"));
            }
            Ok(())
        }
        "no_tcp_listener" => driver.no_tcp_listener(),
        "daemon_absent" => {
            let observed = driver.daemon_absent()?;
            if let Some(code) = step.expect.exit_code {
                if observed.code != code {
                    return Err(format!("exit {}, expected {code}", observed.code));
                }
            }
            for needle in &step.expect.stdout_contains {
                if !observed.stdout.contains(needle) {
                    return Err(format!("stdout does not contain {needle:?}"));
                }
            }
            Ok(())
        }
        "request_log" => {
            let expect = step
                .expect
                .request_log
                .as_ref()
                .ok_or_else(|| "request_log action without expect.request_log".to_owned())?;
            check_request_log(expect, &driver.requests())
        }
        other => Err(format!("unknown action {other:?}")),
    }
}

fn check_clock_advance(expect: &Expect) -> Result<(), String> {
    // The action itself is deterministic; the following steps assert the
    // transition. An expectation on the action is optional but validated.
    if expect.ok.is_some() {
        return Err("advance_clock does not take an ok expectation".to_owned());
    }
    Ok(())
}

pub fn check_expect(
    expect: &Expect,
    outcome: &Outcome,
    driver: &mut dyn Driver,
    connection: &str,
    vars: &Vars,
) -> Result<(), String> {
    if expect.transport_closed {
        if !matches!(outcome, Outcome::Closed) {
            return Err("expected the transport to close without a reply".to_owned());
        }
        return Ok(());
    }
    let Outcome::Reply(reply) = outcome else {
        return Err("expected a reply, transport closed".to_owned());
    };
    if let Some(ok) = expect.ok {
        if reply.ok != ok {
            return Err(format!(
                "expected ok={ok}, observed ok={} ({})",
                reply.ok,
                describe(reply)
            ));
        }
    }
    if reply.error.is_some() && !reply.result.is_null() {
        return Err("an error reply must not carry a result".to_owned());
    }
    if let Some(code) = &expect.error_code {
        let error = reply
            .error
            .as_ref()
            .ok_or_else(|| format!("expected error {code}, observed success"))?;
        if &error.code != code {
            return Err(format!(
                "expected error code {code}, observed {}",
                error.code
            ));
        }
        if let Some(class) = &expect.error_class {
            if &error.class != class {
                return Err(format!(
                    "expected error class {class}, observed {}",
                    error.class
                ));
            }
        }
        if let Some((actual, limit)) = expect.error_counts {
            if error.counts != Some((actual, limit)) {
                return Err(format!(
                    "expected counts {actual}/{limit}, observed {:?}",
                    error.counts
                ));
            }
        }
    }
    for (path, expected) in &expect.result {
        let expected = resolve(expected, vars);
        let actual =
            path_get(&reply.result, path).ok_or_else(|| format!("missing result path {path}"))?;
        if actual != &expected {
            return Err(mismatch(path, actual, &expected));
        }
    }
    for (path, minimum) in &expect.result_min {
        let actual = path_get(&reply.result, path)
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("missing numeric result path {path}"))?;
        if actual < *minimum {
            return Err(format!(
                "result {path} is {actual}, expected at least {minimum}"
            ));
        }
    }
    for path in &expect.present {
        if path_get(&reply.result, path).is_none_or(Value::is_null) {
            return Err(format!("missing field {path}"));
        }
    }
    for path in &expect.absent {
        if path_get(&reply.result, path).is_some_and(|value| !value.is_null()) {
            return Err(format!("field {path} must be absent"));
        }
    }
    for (path, expected) in &expect.array {
        let expected = resolve(expected, vars);
        let actual =
            path_get(&reply.result, path).ok_or_else(|| format!("missing array path {path}"))?;
        if actual != &expected {
            return Err(format!("array {path} is {actual}, expected {expected}"));
        }
    }
    for (path, expected) in &expect.array_len {
        let actual = path_get(&reply.result, path)
            .and_then(Value::as_array)
            .map(Vec::len)
            .ok_or_else(|| format!("missing array path {path}"))?;
        if actual != *expected {
            return Err(format!(
                "array {path} has {actual} entries, expected {expected}"
            ));
        }
    }
    for (path, expected) in &expect.min_array_len {
        let actual = path_get(&reply.result, path)
            .and_then(Value::as_array)
            .map(Vec::len)
            .ok_or_else(|| format!("missing array path {path}"))?;
        if actual < *expected {
            return Err(format!(
                "array {path} has {actual} entries, expected at least {expected}"
            ));
        }
    }
    for (path, expected) in &expect.hex_len {
        let actual = path_get(&reply.result, path)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("missing string path {path}"))?;
        if actual.len() != *expected || !actual.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "{path} is not {expected} hex characters ({} bytes)",
                actual.len()
            ));
        }
    }
    if !expect.distinct.is_empty() {
        let mut seen = std::collections::BTreeSet::new();
        for path in &expect.distinct {
            let value =
                path_get(&reply.result, path).ok_or_else(|| format!("missing path {path}"))?;
            if !seen.insert(value.to_string()) {
                return Err(format!("{path} repeats a value"));
            }
        }
    }
    for (path, variable) in &expect.not_equal {
        let actual = path_get(&reply.result, path).ok_or_else(|| format!("missing path {path}"))?;
        let expected = vars
            .get(variable.trim_start_matches('@'))
            .ok_or_else(|| format!("unknown variable {variable}"))?;
        if actual == expected {
            return Err(format!("{path} equals {variable}"));
        }
    }
    for (path, variables) in &expect.not_equal_any {
        let actual = path_get(&reply.result, path).ok_or_else(|| format!("missing path {path}"))?;
        for variable in variables {
            let expected = vars
                .get(variable.trim_start_matches('@'))
                .ok_or_else(|| format!("unknown variable {variable}"))?;
            if actual == expected {
                return Err(format!("{path} equals {variable}"));
            }
        }
    }
    if expect.closed && !driver.expect_eof(connection) {
        return Err("expected the connection to close after the refusal".to_owned());
    }
    Ok(())
}

fn signature(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Closed => "closed".to_owned(),
        Outcome::Reply(reply) if reply.ok => "ok".to_owned(),
        Outcome::Reply(reply) => match &reply.error {
            Some(error) => format!("error:{}:{}", error.class, error.code),
            None => "error:unknown".to_owned(),
        },
    }
}

fn describe(reply: &Reply) -> String {
    match &reply.error {
        Some(error) => format!("{} / {}", error.class, error.code),
        None => "no error".to_owned(),
    }
}

/// A mismatch message that never echoes a token, bearer, or capability.
fn mismatch(path: &str, actual: &Value, expected: &Value) -> String {
    let sensitive = ["token", "grant", "capability", "secret", "bearer"];
    if sensitive.iter().any(|needle| path.contains(needle)) {
        return format!("result {path} does not match the expectation");
    }
    format!("result {path} is {actual}, expected {expected}")
}

pub fn check_audit(expect: &AuditExpect, text: &str, vars: &Vars) -> Result<(), String> {
    for needle in &expect.contains {
        let needle = resolve_needle(needle, vars);
        if !text.contains(&needle) {
            return Err(format!("audit does not contain {needle:?}"));
        }
    }
    for needle in &expect.not_contains {
        let needle = resolve_needle(needle, vars);
        if !needle.is_empty() && text.contains(&needle) {
            return Err("audit contains a forbidden value".to_owned());
        }
    }
    for bound in &expect.line_bounds {
        let count = text
            .lines()
            .filter(|line| line.contains(&bound.needle))
            .count();
        if count < bound.min || count > bound.max {
            return Err(format!(
                "audit has {count} lines with {:?}, expected {}..={}",
                bound.needle, bound.min, bound.max
            ));
        }
    }
    Ok(())
}

fn check_request_log(expect: &RequestLogExpect, requests: &[RequestRecord]) -> Result<(), String> {
    for request in requests {
        for field in &expect.no_fields {
            if has_field(&request.params, field) {
                return Err(format!("request {} carries field {field}", request.method));
            }
        }
    }
    for (method, expected) in &expect.method_counts {
        let actual = requests
            .iter()
            .filter(|request| &request.method == method)
            .count();
        if actual != *expected {
            return Err(format!(
                "sent {actual} {method} requests, expected {expected}"
            ));
        }
    }
    Ok(())
}

fn has_field(value: &Value, field: &str) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| key == field || has_field(value, field)),
        Value::Array(items) => items.iter().any(|item| has_field(item, field)),
        _ => false,
    }
}

fn arg_u32(step: &Step, field: &str) -> Result<u32, String> {
    let value = step
        .args
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("action needs args.{field}"))?;
    u32::try_from(value).map_err(|_| format!("args.{field} does not fit u32"))
}

fn arg_usize(step: &Step, field: &str) -> Result<usize, String> {
    let value = step
        .args
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("action needs args.{field}"))?;
    usize::try_from(value).map_err(|_| format!("args.{field} does not fit usize"))
}

fn arg_string<'a>(step: &'a Step, field: &str) -> Result<&'a str, String> {
    step.args
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("action needs args.{field}"))
}

/// Resolve `@name` and `{{name}}` references in one JSON value.
pub fn resolve(value: &Value, vars: &Vars) -> Value {
    match value {
        Value::String(text) => {
            if let Some(name) = text.strip_prefix('@') {
                if let Some(value) = vars.get(name) {
                    return value.clone();
                }
            }
            Value::String(substitute_text(text, vars))
        }
        Value::Array(items) => Value::Array(items.iter().map(|item| resolve(item, vars)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), resolve(value, vars)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Resolve one audit needle: an exact `@name` becomes the captured value,
/// `{{name}}` substitutes inside a longer string.
fn resolve_needle(text: &str, vars: &Vars) -> String {
    if let Some(name) = text.strip_prefix('@') {
        if let Some(value) = vars.get(name).and_then(Value::as_str) {
            return value.to_owned();
        }
    }
    substitute_text(text, vars)
}

fn substitute_text(text: &str, vars: &Vars) -> String {
    let mut output = text.to_owned();
    for (name, value) in vars {
        let Some(text) = value.as_str() else {
            continue;
        };
        output = output.replace(&format!("{{{{{name}}}}}"), text);
    }
    output
}

/// Read a dotted path from a JSON value.
pub fn path_get<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}
