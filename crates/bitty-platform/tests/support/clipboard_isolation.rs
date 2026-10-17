//! Test-only refusal gate. No display connections or clipboard content reads.
//!
//! Proc observations are diagnostics, never native authorization. Even stable
//! namespace, PID/start-time and mount identities cannot bind the subsequent
//! independent Clipboard::new connection to that observed server. Refuse until
//! a reviewed connection-bound runner contract exists (PX-6898).

pub const BLOCKED: &str =
    "native access not executed: connection-bound disposable runner unavailable";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start: u64,
    pub parent: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    // mnt, net, pid; namespace identifiers are opaque kernel identities.
    pub namespaces: Option<[String; 3]>,
    pub parent_namespaces: Option<[String; 3]>,
    pub runner: Option<ProcessIdentity>,
    pub server: Option<ProcessIdentity>,
    // Full mountinfo rows preserve mount IDs and devices, not just tmpfs type.
    pub mounts: Vec<String>,
}

#[derive(Debug)]
pub struct IsolationEvidence {
    // Private: callers cannot manufacture an authorized token from booleans.
    _observed: Observation,
}

/// The collector deliberately issues no positive authorization, including for
/// consistent synthetic evidence. A second observation is not a TOCTOU fix.
pub fn collect(mut observe: impl FnMut() -> Observation) -> IsolationEvidence {
    let _before = observe();
    IsolationEvidence {
        _observed: observe(),
    }
}

pub fn run_if_isolated(
    _evidence: &IsolationEvidence,
    _native: impl FnOnce(),
) -> Result<(), &'static str> {
    Err(BLOCKED)
}

#[cfg(target_os = "linux")]
pub fn current() -> IsolationEvidence {
    use std::{env, fs};

    fn identity(pid: u32) -> Option<ProcessIdentity> {
        // comm can contain spaces and parentheses; fields after the final ')'
        // start at stat field 3. Parent is field 4 and starttime is field 22.
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, tail) = stat.rsplit_once(')')?;
        let fields: Vec<_> = tail.split_whitespace().collect();
        Some(ProcessIdentity {
            pid,
            parent: fields.get(1)?.parse().ok()?,
            start: fields.get(19)?.parse().ok()?,
        })
    }

    fn namespaces(pid: u32) -> Option<[String; 3]> {
        let read = |kind| {
            fs::read_link(format!("/proc/{pid}/ns/{kind}"))
                .ok()
                .map(|value| value.to_string_lossy().into_owned())
        };
        Some([read("mnt")?, read("net")?, read("pid")?])
    }

    collect(|| {
        let pid = std::process::id();
        let runner = identity(pid).and_then(|process| identity(process.parent));
        // An inherited PID is only a diagnostic hint, never authority. Do not
        // open the display socket, display lock or clipboard to inspect it.
        let server = env::var("BITTY_CLIPBOARD_XVFB_PID")
            .ok()
            .and_then(|value| value.parse().ok())
            .and_then(identity);
        Observation {
            namespaces: namespaces(pid),
            parent_namespaces: runner.as_ref().and_then(|runner| namespaces(runner.pid)),
            runner,
            server,
            mounts: fs::read_to_string("/proc/self/mountinfo")
                .unwrap_or_default()
                .lines()
                .filter(|line| matches!(line.split_whitespace().nth(4), Some("/tmp" | "/run")))
                .map(str::to_owned)
                .collect(),
        }
    })
}

#[cfg(not(target_os = "linux"))]
pub fn current() -> IsolationEvidence {
    collect(Observation::default)
}
