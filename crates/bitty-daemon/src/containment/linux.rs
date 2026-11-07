//! Linux probe: delegated cgroup v2, falling back to the subreaper plan.
//!
//! The cgroup probe creates and removes a probe cgroup under the delegated
//! root, so the answer is a write that proves delegation rather than an
//! assumption. A cgroup failure falls back to the subreaper only when that
//! mechanism is actually available; otherwise the report is
//! [`ContainmentKind::Refused`] with both reasons recorded.

use crate::platform::PlatformCell;

use super::cgroup::CgroupRoot;
use super::subreaper::SubreaperGuard;
use super::{ContainmentKind, ContainmentRefusal, ContainmentReport};

pub fn probe() -> ContainmentReport {
    let cell = PlatformCell::current();
    match CgroupRoot::detect() {
        Ok(root) => ContainmentReport {
            cell,
            kind: ContainmentKind::CgroupV2,
            refusal: None,
            fallback_reason: None,
            cgroup_root: Some(root.path().to_path_buf()),
        },
        Err(cgroup_refusal) => match SubreaperGuard::enable() {
            Ok(_) => ContainmentReport {
                cell,
                kind: ContainmentKind::Subreaper,
                refusal: None,
                fallback_reason: Some(cgroup_refusal.to_string()),
                cgroup_root: None,
            },
            Err(subreaper_refusal) => ContainmentReport {
                cell,
                kind: ContainmentKind::Refused,
                refusal: Some(ContainmentRefusal::NoMechanism {
                    cgroup: cgroup_refusal.to_string(),
                    subreaper: subreaper_refusal.to_string(),
                }),
                fallback_reason: None,
                cgroup_root: None,
            },
        },
    }
}
