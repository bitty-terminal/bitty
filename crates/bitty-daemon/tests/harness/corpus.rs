//! Typed corpus model and the strict JSON parser.
//!
//! `corpus.json` is the single shared vector set. Every field is required
//! unless the schema says otherwise; an unknown `schema`, an empty
//! `forbidden`, or a vector without a typed expectation is a hard error.

use std::collections::BTreeMap;

use serde_json::Value;

/// The only accepted corpus schema.
pub const SCHEMA: &str = "bittyd-conformance-corpus/1";

/// Which driver runs a vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverKind {
    Fake,
    Real,
}

impl DriverKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fake => "fake",
            Self::Real => "real",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "fake" => Ok(Self::Fake),
            "real" => Ok(Self::Real),
            other => Err(format!("unknown driver {other:?}")),
        }
    }
}

/// A tracked defect that makes a contract-correct vector fail today.
#[derive(Debug, Clone)]
pub struct KnownDefect {
    pub issue: String,
    pub reason: String,
    /// Drivers whose current behavior still carries the defect.
    pub drivers: Vec<DriverKind>,
    /// Zero-based step index (across `steps` then `state`) that must fail.
    pub step: usize,
    /// Refusal code the failing step must report, when the defect is a refusal.
    pub code: Option<String>,
    /// Result path the failing step must name, when the defect is a value.
    pub path: Option<String>,
}

/// One vector.
#[derive(Debug, Clone)]
pub struct Vector {
    pub id: String,
    pub title: String,
    pub area: String,
    pub decisions: Vec<String>,
    pub drivers: Vec<DriverKind>,
    pub host_grant: bool,
    pub injected: Injected,
    pub steps: Vec<Step>,
    pub state: Vec<Step>,
    pub forbidden: Vec<String>,
    pub real_skip: Option<String>,
    pub fake_skip: Option<String>,
    pub deferred: Option<String>,
    pub known_defect: Option<KnownDefect>,
}

impl Vector {
    /// The typed skip reason for one driver, if any.
    pub fn skip_reason(&self, driver: DriverKind) -> Option<&str> {
        match driver {
            DriverKind::Fake => self.fake_skip.as_deref(),
            DriverKind::Real => self.real_skip.as_deref(),
        }
    }

    pub fn runs_on(&self, driver: DriverKind) -> bool {
        self.drivers.contains(&driver)
    }
}

/// Injected time and limits.
#[derive(Debug, Clone, Default)]
pub struct Injected {
    pub clock_ms: u64,
    pub limits: BTreeMap<String, u64>,
}

/// One corpus step: a call or a driver action.
#[derive(Debug, Clone)]
pub struct Step {
    pub connection: Option<String>,
    pub method: Option<String>,
    pub params: Value,
    pub capture: BTreeMap<String, String>,
    pub expect: Expect,
    pub fault: Option<String>,
    pub action: Option<String>,
    pub args: Value,
}

/// Typed expected result and state assertions.
#[derive(Debug, Clone, Default)]
pub struct Expect {
    pub ok: Option<bool>,
    pub error_class: Option<String>,
    pub error_code: Option<String>,
    pub error_counts: Option<(u64, u64)>,
    pub retry_ms: u64,
    pub closed: bool,
    pub transport_closed: bool,
    pub result: BTreeMap<String, Value>,
    pub result_min: BTreeMap<String, u64>,
    pub present: Vec<String>,
    pub absent: Vec<String>,
    pub array: BTreeMap<String, Value>,
    pub array_len: BTreeMap<String, usize>,
    pub min_array_len: BTreeMap<String, usize>,
    pub hex_len: BTreeMap<String, usize>,
    pub distinct: Vec<String>,
    pub not_equal: BTreeMap<String, String>,
    pub not_equal_any: BTreeMap<String, Vec<String>>,
    pub request_log: Option<RequestLogExpect>,
    pub exit_code: Option<i32>,
    pub stdout_contains: Vec<String>,
    pub file_mode: Option<u32>,
    pub audit: Option<AuditExpect>,
}

impl Expect {
    /// Whether this step asserts a typed outcome, not just transport liveness.
    fn has_typed_assertion(&self) -> bool {
        self.ok.is_some()
            || self.error_code.is_some()
            || self.transport_closed
            || !self.result.is_empty()
            || !self.present.is_empty()
            || !self.array.is_empty()
            || !self.array_len.is_empty()
            || !self.hex_len.is_empty()
            || !self.distinct.is_empty()
            || !self.not_equal.is_empty()
            || !self.not_equal_any.is_empty()
            || self.request_log.is_some()
            || self.exit_code.is_some()
            || self.file_mode.is_some()
            || self.audit.is_some()
    }
}

/// Request-log assertions (V-32: no out-of-band pid, one abort).
#[derive(Debug, Clone, Default)]
pub struct RequestLogExpect {
    pub no_fields: Vec<String>,
    pub method_counts: BTreeMap<String, usize>,
}

/// Audit-file assertions.
#[derive(Debug, Clone, Default)]
pub struct AuditExpect {
    pub contains: Vec<String>,
    pub not_contains: Vec<String>,
    pub line_bounds: Vec<LineBound>,
}

/// A bounded occurrence count for one audit needle.
#[derive(Debug, Clone)]
pub struct LineBound {
    pub needle: String,
    pub min: usize,
    pub max: usize,
}

/// The parsed corpus plus its provenance fields.
#[derive(Debug, Clone)]
pub struct Corpus {
    pub contract: String,
    pub frozen: BTreeMap<String, String>,
    pub vectors: Vec<Vector>,
}

impl Corpus {
    /// Load the embedded corpus and validate it.
    ///
    /// # Errors
    ///
    /// Returns a static parse error naming the vector and field; the message
    /// never echoes a host path.
    pub fn load() -> Result<Self, String> {
        Self::parse(include_str!("corpus.json"))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let root: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
        let schema = string(&root, "schema")?;
        if schema != SCHEMA {
            return Err(format!("unknown corpus schema {schema:?}"));
        }
        let contract = string(&root, "contract")?;
        let mut frozen = BTreeMap::new();
        if let Some(map) = root.get("frozen").and_then(Value::as_object) {
            for (key, value) in map {
                frozen.insert(key.clone(), as_string(value, "frozen value")?);
            }
        }
        if frozen
            .get("session.attach.request_role")
            .map(String::as_str)
            != Some("attach_role")
        {
            return Err(
                "the corpus must freeze session.attach.request_role=attach_role".to_owned(),
            );
        }
        let vectors = array(&root, "vectors")?
            .iter()
            .map(parse_vector)
            .collect::<Result<Vec<_>, _>>()?;
        if vectors.is_empty() {
            return Err("the corpus has no vectors".to_owned());
        }
        let mut ids = std::collections::BTreeSet::new();
        for vector in &vectors {
            if !ids.insert(vector.id.clone()) {
                return Err(format!("duplicate vector id {}", vector.id));
            }
        }
        Ok(Self {
            contract,
            frozen,
            vectors,
        })
    }

    pub fn vector(&self, id: &str) -> Option<&Vector> {
        self.vectors.iter().find(|vector| vector.id == id)
    }
}

fn parse_vector(value: &Value) -> Result<Vector, String> {
    let id = string(value, "id")?;
    let fail = |message: String| format!("{id}: {message}");
    let title = string(value, "title").map_err(fail)?;
    let area = string(value, "area").map_err(fail)?;
    let decisions = string_array(value, "decisions").map_err(fail)?;
    let drivers = string_array(value, "drivers")
        .map_err(fail)?
        .iter()
        .map(|driver| DriverKind::parse(driver).map_err(fail))
        .collect::<Result<Vec<_>, _>>()?;
    if drivers.is_empty() {
        return Err(fail("no driver".to_owned()));
    }
    let setup = object(value, "setup").map_err(fail)?;
    let host_grant = match setup.get("host_grant") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(fail("setup.host_grant is not a boolean".to_owned())),
    };
    let injected = match value.get("injected") {
        None | Some(Value::Null) => Injected::default(),
        Some(injected) => {
            let clock_ms = optional_u64(injected, "clock_ms")
                .map_err(fail)?
                .unwrap_or(0);
            let mut limits = BTreeMap::new();
            if let Some(map) = injected.get("limits").and_then(Value::as_object) {
                for (name, limit) in map {
                    let limit = limit
                        .as_u64()
                        .ok_or_else(|| fail(format!("limit {name} is not an integer")))?;
                    limits.insert(name.clone(), limit);
                }
            }
            Injected { clock_ms, limits }
        }
    };
    let steps = array(value, "steps")
        .map_err(fail)?
        .iter()
        .map(|step| parse_step(step).map_err(fail))
        .collect::<Result<Vec<_>, _>>()?;
    if steps.is_empty() {
        return Err(fail("no steps".to_owned()));
    }
    let state = match value.get("state") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|step| parse_step(step).map_err(fail))
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(fail("state is not an array".to_owned())),
    };
    let forbidden = string_array(value, "forbidden").map_err(fail)?;
    if forbidden.is_empty() {
        return Err(fail("no forbidden observations".to_owned()));
    }
    for step in steps.iter().chain(state.iter()) {
        let setup_action = matches!(
            step.action.as_deref(),
            Some("advance_clock" | "restart" | "crash")
        );
        if !setup_action && !step.expect.has_typed_assertion() {
            return Err(fail(format!(
                "step {:?} has no typed expectation",
                step.method.as_deref().or(step.action.as_deref())
            )));
        }
    }
    let known_defect = match value.get("known_defect") {
        None | Some(Value::Null) => None,
        Some(defect) => {
            let drivers = match defect.get("drivers") {
                None | Some(Value::Null) => vec![DriverKind::Fake, DriverKind::Real],
                Some(Value::Array(_)) => string_array(defect, "drivers")
                    .map_err(fail)?
                    .iter()
                    .map(|driver| DriverKind::parse(driver).map_err(fail))
                    .collect::<Result<Vec<_>, _>>()?,
                Some(_) => return Err(fail("known_defect.drivers is not an array".to_owned())),
            };
            if drivers.is_empty() {
                return Err(fail("known_defect.drivers is empty".to_owned()));
            }
            let step = optional_u64(defect, "step")
                .map_err(fail)?
                .ok_or_else(|| fail("known_defect.step is required".to_owned()))?
                as usize;
            if step >= steps.len() + state.len() {
                return Err(fail(format!(
                    "known_defect.step {step} is outside the {} vector steps",
                    steps.len() + state.len()
                )));
            }
            let code = optional_string(defect, "code").map_err(fail)?;
            let path = optional_string(defect, "path").map_err(fail)?;
            if code.is_none() && path.is_none() {
                return Err(fail(
                    "known_defect needs a code or a path for the expected failure".to_owned(),
                ));
            }
            Some(KnownDefect {
                issue: string(defect, "issue").map_err(fail)?,
                reason: string(defect, "reason").map_err(fail)?,
                drivers,
                step,
                code,
                path,
            })
        }
    };
    Ok(Vector {
        id: id.clone(),
        title,
        area,
        decisions,
        drivers,
        host_grant,
        injected,
        steps,
        state,
        forbidden,
        real_skip: optional_string(value, "real_skip").map_err(fail)?,
        fake_skip: optional_string(value, "fake_skip").map_err(fail)?,
        deferred: optional_string(value, "deferred").map_err(fail)?,
        known_defect,
    })
}

fn parse_step(value: &Value) -> Result<Step, String> {
    let action = optional_string(value, "action")?;
    let method = optional_string(value, "method")?;
    let connection = optional_string(value, "connection")?;
    if action.is_some() == method.is_some() {
        return Err("a step has exactly one of action or method".to_owned());
    }
    if method.is_some() && connection.is_none() {
        return Err("a method step needs a connection".to_owned());
    }
    let mut capture = BTreeMap::new();
    if let Some(map) = value.get("capture").and_then(Value::as_object) {
        for (name, path) in map {
            capture.insert(name.clone(), as_string(path, "capture path")?);
        }
    }
    let fault = optional_string(value, "fault")?;
    let args = value.get("args").cloned().unwrap_or(Value::Null);
    Ok(Step {
        connection,
        method,
        params: value.get("params").cloned().unwrap_or(Value::Null),
        capture,
        expect: match value.get("expect") {
            None => Expect::default(),
            Some(expect) => parse_expect(expect)?,
        },
        fault,
        action,
        args,
    })
}

fn parse_expect(value: &Value) -> Result<Expect, String> {
    if !value.is_object() {
        return Err("expect is not an object".to_owned());
    }
    let mut expect = Expect {
        ok: optional_bool(value, "ok")?,
        error_class: optional_string(value, "error_class")?,
        error_code: optional_string(value, "error_code")?,
        error_counts: match value.get("counts") {
            None | Some(Value::Null) => None,
            Some(counts) => Some((
                optional_u64(counts, "actual")?
                    .ok_or_else(|| "counts.actual is required".to_owned())?,
                optional_u64(counts, "limit")?
                    .ok_or_else(|| "counts.limit is required".to_owned())?,
            )),
        },
        retry_ms: optional_u64(value, "retry_ms")?.unwrap_or(0),
        closed: optional_bool(value, "closed")?.unwrap_or(false),
        transport_closed: optional_string(value, "transport")?.as_deref() == Some("closed"),
        result: value_map(value, "result")?,
        result_min: u64_map(value, "result_min")?,
        present: optional_string_array(value, "present")?,
        absent: optional_string_array(value, "absent")?,
        array: value_map(value, "array")?,
        array_len: usize_map(value, "array_len")?,
        min_array_len: usize_map(value, "min_array_len")?,
        hex_len: usize_map(value, "hex_len")?,
        distinct: optional_string_array(value, "distinct")?,
        not_equal: string_map(value, "not_equal")?,
        not_equal_any: string_list_map(value, "not_equal_any")?,
        request_log: None,
        exit_code: optional_i32(value, "exit_code")?,
        stdout_contains: optional_string_array(value, "stdout_contains")?,
        file_mode: optional_u64(value, "file_mode")?.map(|mode| mode as u32),
        audit: None,
    };
    if let Some(log) = value.get("request_log") {
        let log = RequestLogExpect {
            no_fields: optional_string_array(log, "no_fields")?,
            method_counts: usize_map(log, "method_counts")?,
        };
        if log.no_fields.is_empty() && log.method_counts.is_empty() {
            return Err("request_log expectation is empty".to_owned());
        }
        expect.request_log = Some(log);
    }
    expect.result = normalize_values(expect.result);
    expect.result_min = normalize_values(expect.result_min);
    expect.array = normalize_values(expect.array);
    expect.array_len = normalize_values(expect.array_len);
    expect.min_array_len = normalize_values(expect.min_array_len);
    expect.hex_len = normalize_values(expect.hex_len);
    expect.not_equal = normalize_values(expect.not_equal);
    expect.not_equal_any = normalize_values(expect.not_equal_any);
    expect.present = expect
        .present
        .iter()
        .map(|path| normalize_path(path))
        .collect();
    expect.absent = expect
        .absent
        .iter()
        .map(|path| normalize_path(path))
        .collect();
    expect.distinct = expect
        .distinct
        .iter()
        .map(|path| normalize_path(path))
        .collect();
    if expect.error_class.is_some() && expect.error_code.is_none() {
        return Err("error_class without error_code".to_owned());
    }
    if let Some(audit) = value.get("audit") {
        let mut line_bounds = Vec::new();
        if let Some(items) = audit.get("line_bounds").and_then(Value::as_array) {
            for item in items {
                line_bounds.push(LineBound {
                    needle: string(item, "needle").map_err(|_| "audit needle".to_owned())?,
                    min: optional_u64(item, "min")?.unwrap_or(0) as usize,
                    max: optional_u64(item, "max")?.unwrap_or(usize::MAX as u64) as usize,
                });
            }
        }
        let audit = AuditExpect {
            contains: optional_string_array(audit, "contains")?,
            not_contains: optional_string_array(audit, "not_contains")?,
            line_bounds,
        };
        if audit.contains.is_empty()
            && audit.not_contains.is_empty()
            && audit.line_bounds.is_empty()
        {
            return Err("audit expectation is empty".to_owned());
        }
        expect.audit = Some(audit);
    }
    Ok(expect)
}

/// Strip the optional `result.` prefix so every expectation path is relative
/// to the response result.
fn normalize_path(path: &str) -> String {
    path.strip_prefix("result.").unwrap_or(path).to_owned()
}

fn normalize_values<T>(map: BTreeMap<String, T>) -> BTreeMap<String, T> {
    map.into_iter()
        .map(|(key, value)| (normalize_path(&key), value))
        .collect()
}

fn object<'a>(value: &'a Value, field: &str) -> Result<&'a serde_json::Map<String, Value>, String> {
    value
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| format!("missing object {field}"))
}

fn array<'a>(value: &'a Value, field: &str) -> Result<&'a [Value], String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("missing array {field}"))
}

fn string(value: &Value, field: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("missing string {field}"))
}

fn optional_string(value: &Value, field: &str) -> Result<Option<String>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("{field} is not a string")),
    }
}

fn optional_bool(value: &Value, field: &str) -> Result<Option<bool>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(format!("{field} is not a boolean")),
    }
}

fn optional_u64(value: &Value, field: &str) -> Result<Option<u64>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(number) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{field} is not an unsigned integer")),
    }
}

fn optional_i32(value: &Value, field: &str) -> Result<Option<i32>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(number) => number
            .as_i64()
            .and_then(|number| i32::try_from(number).ok())
            .map(Some)
            .ok_or_else(|| format!("{field} is not an integer")),
    }
}

fn string_array(value: &Value, field: &str) -> Result<Vec<String>, String> {
    array(value, field)?
        .iter()
        .map(|item| as_string(item, field))
        .collect()
}

fn optional_string_array(value: &Value, field: &str) -> Result<Vec<String>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(_)) => string_array(value, field),
        Some(_) => Err(format!("{field} is not an array")),
    }
}

fn as_string(value: &Value, field: &str) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{field} is not a string"))
}

fn value_map(value: &Value, field: &str) -> Result<BTreeMap<String, Value>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(map)) => Ok(map
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()),
        Some(_) => Err(format!("{field} is not an object")),
    }
}

fn u64_map(value: &Value, field: &str) -> Result<BTreeMap<String, u64>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, value)| {
                value
                    .as_u64()
                    .map(|number| (key.clone(), number))
                    .ok_or_else(|| format!("{field}.{key} is not an unsigned integer"))
            })
            .collect(),
        Some(_) => Err(format!("{field} is not an object")),
    }
}

fn usize_map(value: &Value, field: &str) -> Result<BTreeMap<String, usize>, String> {
    u64_map(value, field).map(|map| {
        map.into_iter()
            .map(|(key, value)| (key, value as usize))
            .collect()
    })
}

fn string_list_map(value: &Value, field: &str) -> Result<BTreeMap<String, Vec<String>>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, value)| {
                let values = value
                    .as_array()
                    .ok_or_else(|| format!("{field}.{key} is not an array"))?
                    .iter()
                    .map(|item| as_string(item, field))
                    .collect::<Result<Vec<_>, _>>()?;
                if values.is_empty() {
                    return Err(format!("{field}.{key} is empty"));
                }
                Ok((key.clone(), values))
            })
            .collect(),
        Some(_) => Err(format!("{field} is not an object")),
    }
}

fn string_map(value: &Value, field: &str) -> Result<BTreeMap<String, String>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, value)| {
                value
                    .as_str()
                    .map(|text| (key.clone(), text.to_owned()))
                    .ok_or_else(|| format!("{field}.{key} is not a string"))
            })
            .collect(),
        Some(_) => Err(format!("{field} is not an object")),
    }
}
