//! Closed static refusal codes from the accepted error taxonomy (`D-14`,
//! `D-16`, "Error taxonomy mapping").
//!
//! The daemon never stores caller text as a result code. This enum is the
//! complete set of refusals and failures an audit record or an idempotent
//! result descriptor can carry; unknown conditions map to [`RefusalCode::Internal`]
//! with a static spelling, never to a parsed string.
//!
//! The wire spelling remains open until the conformance corpus freezes JSON
//! key spellings (bitty#2664); [`RefusalCode::as_str`] is the stable internal
//! spelling the corpus can pin.

use std::fmt;

macro_rules! define_refusal_codes {
    ($( $variant:ident => $spelling:literal, )*) => {
        /// A static refusal or failure code.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum RefusalCode {
            $(
                #[doc = $spelling]
                $variant,
            )*
        }

        impl RefusalCode {
            /// Every code, in contract order.
            pub const ALL: [Self; [$(stringify!($variant)),*].len()] = [
                $(Self::$variant,)*
            ];

            /// The stable static audit spelling (snake case).
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(
                        Self::$variant => $spelling,
                    )*
                }
            }

            /// The frozen wire spelling (`Denied.code`), `PascalCase` per the
            /// service refusal taxonomy (`bitty#2664`).
            #[must_use]
            pub const fn wire_name(self) -> &'static str {
                match self {
                    $(
                        Self::$variant => stringify!($variant),
                    )*
                }
            }
        }
    };
}

define_refusal_codes! {
    VersionMismatch => "version_mismatch",
    Unauthenticated => "unauthenticated",
    FrameTooLarge => "frame_too_large",
    PayloadTooLarge => "payload_too_large",
    LimitExceeded => "limit_exceeded",
    PendingLimitExceeded => "pending_limit_exceeded",
    ChannelFull => "channel_full",
    RateLimited => "rate_limited",
    ScopeViolation => "scope_violation",
    ConsentRequired => "consent_required",
    ControllerRequired => "controller_required",
    InvalidControlToken => "invalid_control_token",
    ObserverGrantRequired => "observer_grant_required",
    AlreadyNegotiated => "already_negotiated",
    UnsupportedCapability => "unsupported_capability",
    UnsupportedPlatform => "unsupported_platform",
    StaleInstance => "stale_instance",
    StaleGeneration => "stale_generation",
    NotFound => "not_found",
    ControlHeld => "control_held",
    SessionStopping => "session_stopping",
    NotRunning => "not_running",
    TerminalAlreadyHosted => "terminal_already_hosted",
    ActiveSessions => "active_sessions",
    DaemonStopping => "daemon_stopping",
    IdempotencyConflict => "idempotency_conflict",
    IdempotencyCapacity => "idempotency_capacity",
    TerminalEventLost => "terminal_event_lost",
    InvalidMethod => "invalid_method",
    InvalidRequest => "invalid_request",
    Timeout => "timeout",
    Internal => "internal",
}

impl fmt::Display for RefusalCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_code_has_a_unique_static_spelling() {
        let spellings: BTreeSet<_> = RefusalCode::ALL.iter().map(|code| code.as_str()).collect();
        assert_eq!(spellings.len(), RefusalCode::ALL.len());
        assert_eq!(RefusalCode::ALL.len(), 32);
        for code in RefusalCode::ALL {
            assert!(code.as_str().bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
            }));
            assert_eq!(code.to_string(), code.as_str());
        }
    }

    #[test]
    fn every_code_has_a_unique_wire_spelling() {
        let wire: BTreeSet<_> = RefusalCode::ALL
            .iter()
            .map(|code| code.wire_name())
            .collect();
        assert_eq!(wire.len(), RefusalCode::ALL.len());
        for code in RefusalCode::ALL {
            assert!(
                code.wire_name()
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric()),
                "{} is not a wire word",
                code.wire_name()
            );
            assert_eq!(code.wire_name(), stringify_code(code));
        }
    }

    fn stringify_code(code: RefusalCode) -> &'static str {
        match code {
            RefusalCode::VersionMismatch => "VersionMismatch",
            RefusalCode::Unauthenticated => "Unauthenticated",
            RefusalCode::FrameTooLarge => "FrameTooLarge",
            RefusalCode::PayloadTooLarge => "PayloadTooLarge",
            RefusalCode::LimitExceeded => "LimitExceeded",
            RefusalCode::PendingLimitExceeded => "PendingLimitExceeded",
            RefusalCode::ChannelFull => "ChannelFull",
            RefusalCode::RateLimited => "RateLimited",
            RefusalCode::ScopeViolation => "ScopeViolation",
            RefusalCode::ConsentRequired => "ConsentRequired",
            RefusalCode::ControllerRequired => "ControllerRequired",
            RefusalCode::InvalidControlToken => "InvalidControlToken",
            RefusalCode::ObserverGrantRequired => "ObserverGrantRequired",
            RefusalCode::AlreadyNegotiated => "AlreadyNegotiated",
            RefusalCode::UnsupportedCapability => "UnsupportedCapability",
            RefusalCode::UnsupportedPlatform => "UnsupportedPlatform",
            RefusalCode::StaleInstance => "StaleInstance",
            RefusalCode::StaleGeneration => "StaleGeneration",
            RefusalCode::NotFound => "NotFound",
            RefusalCode::ControlHeld => "ControlHeld",
            RefusalCode::SessionStopping => "SessionStopping",
            RefusalCode::NotRunning => "NotRunning",
            RefusalCode::TerminalAlreadyHosted => "TerminalAlreadyHosted",
            RefusalCode::ActiveSessions => "ActiveSessions",
            RefusalCode::DaemonStopping => "DaemonStopping",
            RefusalCode::IdempotencyConflict => "IdempotencyConflict",
            RefusalCode::IdempotencyCapacity => "IdempotencyCapacity",
            RefusalCode::TerminalEventLost => "TerminalEventLost",
            RefusalCode::InvalidMethod => "InvalidMethod",
            RefusalCode::InvalidRequest => "InvalidRequest",
            RefusalCode::Timeout => "Timeout",
            RefusalCode::Internal => "Internal",
        }
    }

    #[test]
    fn taxonomy_codes_keep_their_contract_names() {
        assert_eq!(RefusalCode::ScopeViolation.as_str(), "scope_violation");
        assert_eq!(
            RefusalCode::IdempotencyCapacity.as_str(),
            "idempotency_capacity"
        );
        assert_eq!(
            RefusalCode::TerminalAlreadyHosted.as_str(),
            "terminal_already_hosted"
        );
        assert_eq!(RefusalCode::Internal.as_str(), "internal");
    }
}
