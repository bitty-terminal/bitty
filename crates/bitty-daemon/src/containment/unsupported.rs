//! The typed refusal for a cell with no accepted containment mechanism.
//!
//! A cell that compiles this module (for example a BSD other than FreeBSD)
//! makes no containment claim: the probe returns
//! [`ContainmentRefusal::UnsupportedPlatform`] and a spawn refuses before
//! creating a process. There is no fallback and no approximation.

use std::process::Command;

use crate::platform::PlatformCell;

use super::{
    ContainmentError, ContainmentKind, ContainmentLimits, ContainmentRefusal, ContainmentReport,
    SessionMode,
};

/// A contained child on a cell with no mechanism. The type is uninhabited:
/// [`spawn_with`] always refuses, so no value can exist.
#[derive(Debug)]
pub enum ContainedChild {}

pub fn probe() -> ContainmentReport {
    let cell = PlatformCell::current();
    ContainmentReport {
        cell,
        kind: ContainmentKind::Refused,
        refusal: Some(ContainmentRefusal::UnsupportedPlatform { cell }),
        fallback_reason: None,
        cgroup_root: None,
    }
}

pub fn spawn_with(
    report: &ContainmentReport,
    _name: &str,
    _command: &mut Command,
    _limits: ContainmentLimits,
    _mode: SessionMode,
) -> Result<ContainedChild, ContainmentError> {
    Err(ContainmentError::Refused(
        report
            .refusal()
            .cloned()
            .unwrap_or(ContainmentRefusal::UnsupportedPlatform {
                cell: report.cell(),
            }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unsupported_cell_refuses_and_creates_no_child() {
        let report = probe();
        assert_eq!(report.kind(), ContainmentKind::Refused);
        assert!(!report.is_enforced());
        assert!(matches!(
            report.refusal(),
            Some(ContainmentRefusal::UnsupportedPlatform { cell }) if *cell == report.cell()
        ));
        let mut command = Command::new("true");
        let error = spawn_with(
            &report,
            "bittyd-test-unsupported",
            &mut command,
            ContainmentLimits::default(),
            SessionMode::ProcessGroup,
        )
        .expect_err("no mechanism means no spawn");
        assert!(matches!(error, ContainmentError::Refused(_)));
    }
}
