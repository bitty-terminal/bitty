//! The operator CLI's authenticated endpoint client (`D-05`, `D-07`, `D-15`).
//!
//! `bittyd stop` is the only CLI invocation that speaks the daemon endpoint.
//! It resolves the same per-user Unix socket the service loop binds, presents
//! the owner-private operator capability at `daemon.negotiate`, and issues
//! `daemon.stop` on that connection. There is no second transport, no TCP
//! fallback, and no privileged shortcut: the request takes exactly the paths a
//! consumer takes, and the daemon's peer guard re-checks the connection on the
//! privileged boundary.
//!
//! The capability is loaded with [`crate::operator::OperatorStore::load_from`],
//! which refuses a symlink, a foreign owner, or a mode other than `0600`. The
//! client never reads the file on behalf of the daemon and never prints it.

use std::time::Duration;

use serde_json::{Value, json};

use crate::contract::{MIN_CLIENT_MINOR, SERVICE_API_MAJOR};
#[cfg(unix)]
use crate::endpoint::{EndpointEnv, EndpointError, EndpointPaths};
#[cfg(unix)]
use crate::operator::{OperatorError, OperatorStore};
use crate::service::dispatch::StopMode;
use crate::service::wire;
#[cfg(unix)]
use std::io::{Read, Write};

/// Bound on one CLI exchange with the daemon.
///
/// The daemon's own request bound is `request_timeout_ms`, and a `force` stop
/// also spends up to `abort_grace_ms` terminating and reaping the worker tree
/// before it replies. The CLI adds a wider local bound so a wedged endpoint
/// cannot hang an operator script while a legitimate force stop still fits.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// The accepted `daemon.stop` result the CLI needs for its exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopReply {
    /// The mode the daemon accepted.
    pub mode: StopMode,
    /// Sessions active when the stop was observed.
    pub active_sessions: u64,
    /// Sessions this stop terminated.
    pub terminated: u64,
    /// Bound workers the daemon could not verify gone (`D-05` exit `5`).
    pub unverified: u64,
    /// Whether a drain deadline forced the remainder. The accepted `1.3`
    /// result does not carry this field yet, so a served daemon reports
    /// `false`; the parser keeps the mapping ready for the additive key.
    pub drain_forced: bool,
}

/// A refused or failed CLI exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopError {
    /// No endpoint exists for this user, so no daemon is running.
    Absent,
    /// This cell has no accepted endpoint transport.
    UnsupportedPlatform,
    /// The endpoint path is unusable (not absolute, too long, or no file name).
    InvalidEndpoint,
    /// The capability file is unsafe or unreadable while an endpoint answers;
    /// this is an authentication invariant failure, never a weaker attempt.
    Authentication,
    /// The daemon refused the request with a typed code.
    Refused {
        /// The static refusal class.
        class: String,
        /// The static refusal code.
        code: String,
        /// Optional `(actual, limit)` counts.
        counts: Option<(u64, u64)>,
    },
    /// The endpoint did not answer, or the connection failed mid-exchange.
    Transport,
    /// The response was not a well-formed `bitty-ipc` v1 envelope.
    Protocol,
}

impl std::fmt::Display for StopError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent => formatter.write_str("no running daemon"),
            Self::UnsupportedPlatform => {
                formatter.write_str("no accepted bittyd endpoint transport on this platform")
            }
            Self::InvalidEndpoint => formatter.write_str("the bittyd endpoint path is unusable"),
            Self::Authentication => formatter.write_str("operator authentication failed"),
            Self::Refused {
                class,
                code,
                counts,
            } => match counts {
                Some((actual, limit)) => write!(
                    formatter,
                    "the daemon refused the stop: {class}/{code} (actual {actual}, limit {limit})"
                ),
                None => write!(formatter, "the daemon refused the stop: {class}/{code}"),
            },
            Self::Transport => formatter.write_str("the daemon endpoint did not answer"),
            Self::Protocol => formatter.write_str("the daemon sent a malformed response"),
        }
    }
}

impl std::error::Error for StopError {}

/// Issue one authenticated `daemon.stop` and return the accepted result.
///
/// # Errors
///
/// Returns [`StopError`] for an absent endpoint, an unsafe capability file, a
/// typed daemon refusal, or a transport/protocol failure. No caller input is
/// echoed in the error.
pub fn stop(mode: StopMode) -> Result<StopReply, StopError> {
    #[cfg(not(unix))]
    {
        let _ = mode;
        return Err(StopError::UnsupportedPlatform);
    }
    #[cfg(unix)]
    {
        stop_unix(mode)
    }
}

#[cfg(unix)]
fn stop_unix(mode: StopMode) -> Result<StopReply, StopError> {
    use std::os::unix::net::UnixStream;

    let env = EndpointEnv::from_process();
    let paths = match EndpointPaths::resolve(&env) {
        Ok(paths) => paths,
        Err(EndpointError::NoEndpoint { .. }) => return Err(StopError::Absent),
        Err(EndpointError::UnsupportedPlatform { .. }) => {
            return Err(StopError::UnsupportedPlatform);
        }
        Err(EndpointError::InvalidPath { .. } | EndpointError::PathTooLong { .. }) => {
            return Err(StopError::InvalidEndpoint);
        }
        Err(_) => return Err(StopError::InvalidEndpoint),
    };

    let mut stream = match UnixStream::connect(paths.socket()) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(StopError::Absent);
        }
        Err(_) => return Err(StopError::Transport),
    };
    stream
        .set_read_timeout(Some(STOP_TIMEOUT))
        .map_err(|_| StopError::Transport)?;
    stream
        .set_write_timeout(Some(STOP_TIMEOUT))
        .map_err(|_| StopError::Transport)?;
    // Linux attaches `SCM_CREDENTIALS` only when the sending socket opted in;
    // the daemon refuses an unauthenticated peer before parsing a frame.
    crate::service::frame::enable_credentials(&stream).map_err(|_| StopError::Transport)?;

    // The capability is read only after a live endpoint answered, so a missing
    // file with no daemon stays the supported absence path rather than a
    // tamper claim.
    let operator = OperatorStore::load_from(paths.runtime_dir()).map_err(|error| match error {
        OperatorError::UnsupportedPlatform => StopError::UnsupportedPlatform,
        _ => StopError::Authentication,
    })?;
    let capability = operator.capability().as_hex();

    let negotiate = json!({
        "v": 1,
        "id": "cli-negotiate",
        "method": "daemon.negotiate",
        "params": {
            "client_version": {
                "major": SERVICE_API_MAJOR,
                "minor": MIN_CLIENT_MINOR,
            },
            "operator_capability": capability,
        },
    });
    let handshake = exchange(&mut stream, &negotiate)?;
    if !handshake.ok {
        return Err(refusal_from(&handshake));
    }

    let request = json!({
        "v": 1,
        "id": "cli-stop",
        "method": "daemon.stop",
        "params": {"mode": mode.as_str()},
    });
    let reply = exchange(&mut stream, &request)?;
    if !reply.ok {
        return Err(refusal_from(&reply));
    }
    parse_stop_result(&reply.result)
}

#[cfg(unix)]
fn exchange(
    stream: &mut std::os::unix::net::UnixStream,
    request: &Value,
) -> Result<Reply, StopError> {
    let payload = serde_json::to_vec(request).map_err(|_| StopError::Protocol)?;
    let frame = wire::frame(&payload).map_err(|_| StopError::Protocol)?;
    stream.write_all(&frame).map_err(|_| StopError::Transport)?;
    stream.flush().map_err(|_| StopError::Transport)?;

    let mut header = [0u8; 4];
    stream
        .read_exact(&mut header)
        .map_err(|_| StopError::Transport)?;
    let length = usize::try_from(u32::from_be_bytes(header)).map_err(|_| StopError::Protocol)?;
    let bound = usize::try_from(crate::limits::LimitId::MaxFrameBytes.default_value())
        .map_err(|_| StopError::Protocol)?;
    if length > bound {
        return Err(StopError::Protocol);
    }
    let mut body = vec![0u8; length];
    stream
        .read_exact(&mut body)
        .map_err(|_| StopError::Transport)?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| StopError::Protocol)?;
    Reply::from_value(value).ok_or(StopError::Protocol)
}

/// One parsed response envelope.
struct Reply {
    ok: bool,
    result: Value,
    class: String,
    code: String,
    counts: Option<(u64, u64)>,
}

impl Reply {
    fn from_value(value: Value) -> Option<Self> {
        let object = value.as_object()?;
        object.get("v")?.as_u64()?;
        object.get("id")?.as_str()?;
        let ok = object.get("ok")?.as_bool()?;
        let result = object.get("result").cloned().unwrap_or(Value::Null);
        let (class, code, counts) = match object.get("error").and_then(Value::as_object) {
            Some(error) => (
                error
                    .get("class")
                    .and_then(Value::as_str)
                    .unwrap_or("Internal")
                    .to_owned(),
                error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("Internal")
                    .to_owned(),
                error.get("counts").and_then(|counts| {
                    Some((
                        counts.get("actual")?.as_u64()?,
                        counts.get("limit")?.as_u64()?,
                    ))
                }),
            ),
            None => (String::new(), String::new(), None),
        };
        Some(Self {
            ok,
            result,
            class,
            code,
            counts,
        })
    }
}

fn refusal_from(reply: &Reply) -> StopError {
    if reply.class == "Unauthenticated" || reply.code == "Unauthenticated" {
        return StopError::Authentication;
    }
    StopError::Refused {
        class: reply.class.clone(),
        code: reply.code.clone(),
        counts: reply.counts,
    }
}

fn parse_stop_result(result: &Value) -> Result<StopReply, StopError> {
    let mode = result
        .get("mode")
        .and_then(Value::as_str)
        .and_then(StopMode::parse)
        .ok_or(StopError::Protocol)?;
    let active_sessions = result
        .get("active_sessions")
        .and_then(Value::as_u64)
        .ok_or(StopError::Protocol)?;
    let terminated = result
        .get("terminated")
        .and_then(Value::as_u64)
        .ok_or(StopError::Protocol)?;
    Ok(StopReply {
        mode,
        active_sessions,
        terminated,
        unverified: result
            .get("unverified")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        drain_forced: result
            .get("drain_forced")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_result_parses_the_accepted_shape() {
        let reply = parse_stop_result(&json!({
            "accepted": true,
            "mode": "force",
            "active_sessions": 2,
            "terminated": 2,
            "unverified": 1,
            "drain_forced": true,
            "drain_deadline_ms": null,
        }))
        .expect("accepted");
        assert_eq!(reply.mode, StopMode::Force);
        assert_eq!(reply.active_sessions, 2);
        assert_eq!(reply.terminated, 2);
        assert_eq!(reply.unverified, 1);
        assert!(reply.drain_forced);
    }

    #[test]
    fn stop_result_requires_the_frozen_keys() {
        assert_eq!(
            parse_stop_result(&json!({"mode": "force"})),
            Err(StopError::Protocol)
        );
        assert_eq!(
            parse_stop_result(&json!({"mode": "force", "active_sessions": 1})),
            Err(StopError::Protocol)
        );
        assert_eq!(
            parse_stop_result(&json!({"mode": "unknown", "active_sessions": 1, "terminated": 0})),
            Err(StopError::Protocol)
        );
    }

    #[test]
    fn an_unauthenticated_refusal_stays_an_authentication_failure() {
        let reply = Reply {
            ok: false,
            result: Value::Null,
            class: "Unauthenticated".to_owned(),
            code: "Unauthenticated".to_owned(),
            counts: None,
        };
        assert_eq!(refusal_from(&reply), StopError::Authentication);
    }

    #[test]
    fn a_typed_refusal_keeps_its_static_code_and_counts() {
        let reply = Reply {
            ok: false,
            result: Value::Null,
            class: "Denied".to_owned(),
            code: "ActiveSessions".to_owned(),
            counts: Some((2, 16)),
        };
        assert_eq!(
            refusal_from(&reply),
            StopError::Refused {
                class: "Denied".to_owned(),
                code: "ActiveSessions".to_owned(),
                counts: Some((2, 16)),
            }
        );
    }
}
