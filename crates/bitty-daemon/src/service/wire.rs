//! The bounded `bitty-ipc` v1 envelopes and the typed parameter helpers.
//!
//! The daemon serves the accepted `bitty-ipc` v1 wire envelope: a JSON object
//! with `v`, `id`, `method`, and `params` on the request side, and `v`, `id`,
//! and `ok` plus `result` or `error` on the response side. The framing (a
//! 4-byte big-endian length prefix) and the length bound come from
//! `bitty-ipc-api`; this module only parses and renders the envelope and never
//! invents a second wire format.
//!
//! Rules enforced here:
//!
//! - `v` must be a supported wire version, refused whole before the service
//!   sees the request;
//! - `id` is a bounded correlation value, validated by `bitty-ipc-api`;
//! - `method` is validated by the `bitty-ipc` grammar, and a grammar violation
//!   is `InvalidMethod` (a well-formed unknown method is `NotFound` later);
//! - `params` is bounded and depth-checked, and an ambient `auth`, `scope`, or
//!   `role` field is rejected rather than ignored;
//! - an error response carries only the static class, code, message, and
//!   numeric counts from the refusal. No caller bytes, path, token, bearer, or
//!   another session's details can be rendered.

use serde_json::{Map, Value, json};

use bitty_ipc_api::error::ErrorClass;
use bitty_ipc_api::frame::encode_frame;
use bitty_ipc_api::wire::{
    MAX_ID_BYTES, MAX_JSON_DEPTH, validate_id, validate_json_depth, validate_method,
    validate_no_ambient_auth, validate_response_envelope, validate_wire_version,
};

use crate::contract::ClientVersion;
use crate::events::{Event, EventDetail};
use crate::handshake::{
    ACCEPTED_WIRE_VERSION, ClientCorrelation, HandshakeResponse, NegotiateRequest, SecretBearer,
};
use crate::registry::DaemonInstanceId;
use crate::service::dispatch::{Refusal, RefusalKind};

/// A parsed `bitty-ipc` v1 request envelope.
#[derive(Debug, Clone)]
pub struct RequestEnvelope {
    /// The correlation id echoed on the response.
    pub id: String,
    /// The validated method name.
    pub method: String,
    /// The request parameters (`Null` when absent).
    pub params: Value,
}

impl RequestEnvelope {
    /// Parse one frame payload.
    ///
    /// # Errors
    ///
    /// Returns a static [`Refusal`]: `InvalidRequest` for a malformed payload,
    /// `InvalidMethod` for a grammar-invalid method, `VersionMismatch` for an
    /// unsupported `v`, and `LimitExceeded` for an over-long `id`.
    pub fn parse(payload: &[u8]) -> Result<Self, Refusal> {
        let value: Value =
            serde_json::from_slice(payload).map_err(|_| Refusal::invalid_request())?;
        let object = value.as_object().ok_or_else(Refusal::invalid_request)?;
        let version = object
            .get("v")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .ok_or_else(Refusal::invalid_request)?;
        validate_wire_version(version).map_err(|_| Refusal::version_mismatch())?;

        let id = object
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(Refusal::invalid_request)?;
        validate_id(id).map_err(|error| match error {
            bitty_ipc_api::error::IpcError::LimitExceeded { limit, actual, .. } => {
                Refusal::limit_exceeded(
                    u64::try_from(limit).unwrap_or(u64::MAX),
                    u64::try_from(actual).unwrap_or(u64::MAX),
                )
            }
            _ => Refusal::invalid_request(),
        })?;
        if id.len() > MAX_ID_BYTES {
            return Err(Refusal::invalid_request());
        }

        let method = object
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(Refusal::invalid_request)?;
        validate_method(method).map_err(|_| Refusal::invalid_method())?;

        let params = object.get("params").cloned().unwrap_or(Value::Null);
        if !params.is_null() {
            let encoded = serde_json::to_vec(&params).map_err(|_| Refusal::invalid_request())?;
            validate_json_depth(&encoded, MAX_JSON_DEPTH)
                .map_err(|_| Refusal::invalid_request())?;
            validate_no_ambient_auth(&encoded).map_err(|_| Refusal::invalid_request())?;
        }
        validate_no_ambient_auth(payload).map_err(|_| Refusal::invalid_request())?;

        Ok(Self {
            id: id.to_owned(),
            method: method.to_owned(),
            params,
        })
    }
}

/// Render a success response frame payload.
///
/// # Errors
///
/// Returns [`Refusal::internal`] when the result cannot be encoded or violates
/// the response envelope bound.
pub fn render_success(id: &str, result: &Value) -> Result<Vec<u8>, Refusal> {
    let payload = serde_json::to_vec(&json!({
        "v": ACCEPTED_WIRE_VERSION,
        "id": id,
        "ok": true,
        "result": result,
    }))
    .map_err(|_| Refusal::internal())?;
    validate_response_envelope(ACCEPTED_WIRE_VERSION, id, &payload)
        .map_err(|_| Refusal::internal())?;
    Ok(payload)
}

/// Render an error response frame payload.
///
/// # Errors
///
/// Returns [`Refusal::internal`] when the error cannot be encoded.
pub fn render_error(id: &str, refusal: Refusal) -> Result<Vec<u8>, Refusal> {
    let error = error_object(refusal);
    let payload = serde_json::to_vec(&json!({
        "v": ACCEPTED_WIRE_VERSION,
        "id": id,
        "ok": false,
        "error": error,
    }))
    .map_err(|_| Refusal::internal())?;
    validate_response_envelope(ACCEPTED_WIRE_VERSION, id, &payload)
        .map_err(|_| Refusal::internal())?;
    Ok(payload)
}

/// Render a refusal whose request `id` cannot be echoed.
///
/// A complete frame with an over-long `id` still gets a typed refusal, but the
/// response omits `id`: the value failed the bound, so echoing it would repeat
/// the violation and the client cannot correlate it anyway. The connection
/// retires immediately after this frame drains.
///
/// # Errors
///
/// Returns [`Refusal::internal`] when the error cannot be encoded or the
/// response envelope exceeds the frame/depth bound.
pub fn render_error_without_id(refusal: Refusal) -> Result<Vec<u8>, Refusal> {
    let error = error_object(refusal);
    let payload = serde_json::to_vec(&json!({
        "v": ACCEPTED_WIRE_VERSION,
        "ok": false,
        "error": error,
    }))
    .map_err(|_| Refusal::internal())?;
    validate_wire_version(ACCEPTED_WIRE_VERSION).map_err(|_| Refusal::internal())?;
    validate_json_depth(&payload, MAX_JSON_DEPTH).map_err(|_| Refusal::internal())?;
    if payload.len() > bitty_ipc_api::frame::MAX_FRAME_BYTES {
        return Err(Refusal::internal());
    }
    Ok(payload)
}

/// The static error object shared by both response shapes.
fn error_object(refusal: Refusal) -> Value {
    let mut error = Map::new();
    error.insert(
        "class".to_owned(),
        Value::String(class_name(refusal.class()).to_owned()),
    );
    error.insert("code".to_owned(), Value::String(refusal.code().to_owned()));
    error.insert(
        "message".to_owned(),
        Value::String(refusal.message().to_owned()),
    );
    if let Some((actual, limit)) = refusal.counts() {
        error.insert(
            "counts".to_owned(),
            json!({"actual": actual, "limit": limit}),
        );
    }
    if let RefusalKind::Timeout {
        request_id,
        timeout_ms,
    } = refusal.kind()
    {
        error.insert("request_id".to_owned(), json!(request_id));
        error.insert("timeout_ms".to_owned(), json!(timeout_ms));
    }
    Value::Object(error)
}

/// Encode one payload as a length-prefixed frame.
///
/// # Errors
///
/// Returns [`Refusal::internal`] when the payload exceeds the frame bound.
pub fn frame(payload: &[u8]) -> Result<Vec<u8>, Refusal> {
    encode_frame(payload).map_err(|_| Refusal::internal())
}

/// Parse the `daemon.negotiate` request parameters.
///
/// # Errors
///
/// `InvalidRequest` for a malformed `client_version`, `client_correlation`, or
/// `host_grant`; `Unauthenticated` for a present but malformed
/// `operator_capability`, so the response never distinguishes a bad bearer from
/// a missing capability file.
pub fn parse_negotiate(params: &Value) -> Result<NegotiateRequest, Refusal> {
    let map = params.as_object().ok_or_else(Refusal::invalid_request)?;
    let version = map
        .get("client_version")
        .and_then(Value::as_object)
        .ok_or_else(Refusal::invalid_request)?;
    let major = version
        .get("major")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(Refusal::invalid_request)?;
    let minor = version
        .get("minor")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(Refusal::invalid_request)?;
    let mut request = NegotiateRequest::new(ClientVersion::new(major, minor));
    if let Some(value) = map.get("client_correlation") {
        let correlation = value.as_str().ok_or_else(Refusal::invalid_request)?;
        request = request.with_client_correlation(
            ClientCorrelation::parse(correlation).map_err(|_| Refusal::invalid_request())?,
        );
    }
    if let Some(value) = map.get("operator_capability") {
        let raw = value.as_str().ok_or_else(Refusal::unauthenticated)?;
        let capability = SecretBearer::parse(raw).map_err(|_| Refusal::unauthenticated())?;
        request = request.with_operator_capability(capability);
    }
    if let Some(value) = map.get("host_grant") {
        let raw = value.as_str().ok_or_else(Refusal::invalid_request)?;
        request = request
            .with_host_grant(SecretBearer::parse(raw).map_err(|_| Refusal::invalid_request())?);
    }
    if let Some(value) = map.get("events") {
        let requested = value.as_bool().ok_or_else(Refusal::invalid_request)?;
        request = request.with_events(requested);
    }
    Ok(request)
}

/// Render one pushed session event as the `event` object of a wire frame.
///
/// The frozen envelope fields (`D-10`) are always present. `detail` is added
/// only when the kind carries a payload, so `control_transfer` names the two
/// leases and `daemon_stopping` carries its deadline; a kind with no payload
/// has no `detail` key at all.
pub fn event_value(event: &Event) -> Value {
    let mut value = json!({
        "kind": event.kind.as_str(),
        "instance_id": event.instance_id.as_hex(),
        "session_id": event.session_id.as_hex(),
        "attach_id": event.attach_id.map(|attach_id| attach_id.as_hex()),
        "revision": event.revision,
        "terminal": event.is_terminal(),
    });
    if let Value::Object(object) = &mut value {
        match event.detail {
            EventDetail::None => {}
            EventDetail::ControlTransfer { from, to } => {
                object.insert(
                    "detail".to_owned(),
                    json!({"from": from.as_hex(), "to": to.as_hex()}),
                );
            }
            EventDetail::DaemonStopping { deadline_ms } => {
                object.insert("detail".to_owned(), json!({"deadline_ms": deadline_ms}));
            }
        }
        if let Some(marker) = event.terminal {
            object.insert(
                "terminal_marker".to_owned(),
                json!({
                    "state": marker.state.as_str(),
                    "reason": marker.reason.as_str(),
                    "cleanup": marker.cleanup.as_str(),
                }),
            );
        }
    }
    value
}

/// Render one pushed session event as a complete wire frame.
///
/// The frame is `{"v": 1, "id": <server correlation>, "event": {...}}`. The
/// correlation id is chosen by the server per connection and is never a client
/// value (`D-10`).
///
/// # Errors
///
/// Returns [`Refusal::internal`] when the payload exceeds the frame bound.
pub fn render_event(id: &str, event: &Event) -> Result<Vec<u8>, Refusal> {
    let payload = json!({
        "v": ACCEPTED_WIRE_VERSION,
        "id": id,
        "event": event_value(event),
    });
    let bytes = serde_json::to_vec(&payload).map_err(|_| Refusal::internal())?;
    frame(&bytes)
}

/// Render the complete handshake response as a JSON value.
///
/// # Errors
///
/// Always `Ok`; the signature stays `Result` so a future field with a fallible
/// encoding does not change the server call site.
pub fn handshake_value(response: &HandshakeResponse) -> Result<Value, Refusal> {
    let capabilities: Map<String, Value> = response
        .capabilities
        .all()
        .into_iter()
        .map(|(name, enabled)| (name.to_owned(), Value::Bool(enabled)))
        .collect();
    let limits: Vec<Value> = response
        .limits
        .advertised()
        .map(|entry| {
            json!({
                "name": entry.name,
                "effective": entry.value.effective(),
                "default": entry.value.default_value(),
                "system_max": entry.value.system_max(),
            })
        })
        .collect();
    Ok(json!({
        "wire_version": response.wire_version,
        "service_api_version": {
            "major": response.service_api_version.major,
            "minor": response.service_api_version.minor,
            "min_client_minor": response.service_api_version.min_client_minor,
        },
        "daemon_instance_id": response.daemon_instance_id.as_hex(),
        "daemon_pid": response.daemon_pid,
        "server_identity": {
            "uid": response.server_identity.uid,
            "endpoint_kind": response.server_identity.endpoint_kind.as_str(),
            "endpoint_owner": response.server_identity.endpoint_owner,
            "endpoint_mode": response.server_identity.endpoint_mode,
        },
        "peer_identity": {
            "uid": response.peer_identity.uid,
            "pid": response.peer_identity.pid,
            "start_time": response.peer_identity.start_time,
        },
        "client_id": response.client_id.as_hex(),
        "scopes": response.scopes.names(),
        "capabilities": Value::Object(capabilities),
        "limits": limits,
        "server_monotonic_ms": response.server_monotonic_ms,
        "daemon_state": response.daemon_state.as_str(),
    }))
}

fn class_name(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Framing => "Framing",
        ErrorClass::Channel => "Channel",
        ErrorClass::Transport => "Transport",
        ErrorClass::Timeout => "Timeout",
        ErrorClass::Validation => "Validation",
        ErrorClass::Scope => "Scope",
        ErrorClass::Unauthenticated => "Unauthenticated",
        ErrorClass::Denied => "Denied",
        ErrorClass::NotFound => "NotFound",
        ErrorClass::Unavailable => "Unavailable",
        ErrorClass::Internal => "Internal",
    }
}

/// Typed parameter extraction.
///
/// Every failure is a static `InvalidRequest`; no helper echoes the offending
/// value or a field name from the caller.
pub mod params {
    use super::{DaemonInstanceId, Map, Refusal, Value};

    /// The parameter object, or `None` when the request carries no parameters.
    ///
    /// # Errors
    ///
    /// `InvalidRequest` when the parameters are present but not a JSON object.
    pub fn object(params: &Value) -> Result<Option<&Map<String, Value>>, Refusal> {
        match params {
            Value::Null => Ok(None),
            Value::Object(map) => Ok(Some(map)),
            _ => Err(Refusal::invalid_request()),
        }
    }

    /// A required string field.
    ///
    /// # Errors
    ///
    /// `InvalidRequest` when the field is missing or not a string.
    pub fn required_string<'a>(
        fields: Option<&'a Map<String, Value>>,
        key: &str,
    ) -> Result<&'a str, Refusal> {
        string_field(fields, key)?.ok_or_else(Refusal::invalid_request)
    }

    /// An optional string field.
    ///
    /// # Errors
    ///
    /// `InvalidRequest` when the field is present but not a string.
    pub fn string_field<'a>(
        fields: Option<&'a Map<String, Value>>,
        key: &str,
    ) -> Result<Option<&'a str>, Refusal> {
        match fields.and_then(|map| map.get(key)) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value)),
            Some(_) => Err(Refusal::invalid_request()),
        }
    }

    /// An optional unsigned integer field.
    ///
    /// # Errors
    ///
    /// `InvalidRequest` when the field is present but not a `u64`.
    pub fn u64_field(
        fields: Option<&Map<String, Value>>,
        key: &str,
    ) -> Result<Option<u64>, Refusal> {
        match fields.and_then(|map| map.get(key)) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(number)) => number
                .as_u64()
                .map(Some)
                .ok_or_else(Refusal::invalid_request),
            Some(_) => Err(Refusal::invalid_request()),
        }
    }

    /// A required unsigned integer field.
    ///
    /// # Errors
    ///
    /// `InvalidRequest` when the field is missing or not a `u64`.
    pub fn required_u64(fields: Option<&Map<String, Value>>, key: &str) -> Result<u64, Refusal> {
        u64_field(fields, key)?.ok_or_else(Refusal::invalid_request)
    }

    /// An optional boolean field.
    ///
    /// # Errors
    ///
    /// `InvalidRequest` when the field is present but not a boolean.
    pub fn bool_field(
        fields: Option<&Map<String, Value>>,
        key: &str,
    ) -> Result<Option<bool>, Refusal> {
        match fields.and_then(|map| map.get(key)) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(_) => Err(Refusal::invalid_request()),
        }
    }

    /// An optional daemon-instance field.
    ///
    /// # Errors
    ///
    /// `InvalidRequest` when the field is not a 32-character lowercase hex
    /// instance id.
    pub fn instance_field(
        fields: Option<&Map<String, Value>>,
        key: &str,
    ) -> Result<Option<DaemonInstanceId>, Refusal> {
        let Some(value) = string_field(fields, key)? else {
            return Ok(None);
        };
        value
            .parse::<DaemonInstanceId>()
            .map(Some)
            .map_err(|_| Refusal::invalid_request())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(method: &str, params: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "v": 1,
            "id": "1",
            "method": method,
            "params": params,
        }))
        .expect("encode request")
    }

    #[test]
    fn a_controller_claim_passes_the_wire_validator() {
        // `session.attach` carries the role as `attach_role`: a bare `role` key
        // is an ambient-authority probe and stays refused by the transport
        // validator (it is not weakened for the daemon).
        let claim = envelope(
            "session.attach",
            json!({
                "attach_role": "controller",
                "session_id": "a".repeat(32),
                "expected_generation": 0,
                "control_token": "b".repeat(64),
                "idempotency_key": "claim",
            }),
        );
        let parsed = RequestEnvelope::parse(&claim).expect("attach_role is a service field");
        assert_eq!(parsed.method, "session.attach");
        assert_eq!(parsed.params["attach_role"], "controller");
    }

    #[test]
    fn an_observer_attach_passes_the_wire_validator() {
        let observer = envelope(
            "session.attach",
            json!({
                "attach_role": "observer",
                "session_id": "a".repeat(32),
                "observer_grant": "c".repeat(64),
            }),
        );
        let parsed = RequestEnvelope::parse(&observer).expect("attach_role is a service field");
        assert_eq!(parsed.params["attach_role"], "observer");
    }

    #[test]
    fn ambient_authority_fields_stay_refused() {
        for forbidden in ["role", "auth", "scope"] {
            let legacy = envelope(
                "session.attach",
                json!({(forbidden): "controller", "session_id": "a".repeat(32)}),
            );
            let error = RequestEnvelope::parse(&legacy).expect_err("ambient field");
            assert_eq!(error.kind(), RefusalKind::InvalidRequest, "{forbidden}");
        }
    }
}
