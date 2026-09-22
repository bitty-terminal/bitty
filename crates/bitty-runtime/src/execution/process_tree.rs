//! Owned-process-tree kill backends (RUN-17, #1048).
//!
//! Cancel must terminate the owned process tree, never a single PID: a job
//! that spawned grandchildren must not leave them behind. The mechanism is
//! platform-split and confined here so the supervisor never branches on
//! `target_os` itself:
//!
//! - Linux: process groups plus pidfd;
//! - macOS: process groups plus kqueue/process wait;
//! - Windows: Job Objects plus ConPTY.
//!
//! This module selects the backend and the kill scope it can honor. The
//! actual kill, typed cancel, and generation fencing stay CTX-0512: nothing
//! here signals a process, opens a handle, or uses `unsafe`.

use std::fmt;

/// Backend that can terminate a job's owned process tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProcessTreeBackend {
    /// Linux process groups plus pidfd.
    LinuxProcessGroup,
    /// macOS process groups plus kqueue/process wait.
    MacosProcessGroup,
    /// Windows Job Objects plus ConPTY.
    WindowsJobObject,
    /// No owned-tree backend on this platform: only the direct child can be
    /// terminated. Callers must surface the gap, never silently single-kill
    /// while claiming tree cleanup.
    Unsupported,
}

impl ProcessTreeBackend {
    /// Backend for the compiling platform.
    #[must_use]
    pub const fn detect() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self::WindowsJobObject
        }
        #[cfg(target_os = "macos")]
        {
            Self::MacosProcessGroup
        }
        #[cfg(target_os = "linux")]
        {
            Self::LinuxProcessGroup
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        {
            Self::Unsupported
        }
    }

    /// Stable lowercase wire/display name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LinuxProcessGroup => "linux_process_group",
            Self::MacosProcessGroup => "macos_process_group",
            Self::WindowsJobObject => "windows_job_object",
            Self::Unsupported => "unsupported",
        }
    }

    /// Whether this backend can terminate the owned tree (as opposed to the
    /// direct child only).
    #[must_use]
    pub const fn kills_owned_tree(self) -> bool {
        !matches!(self, Self::Unsupported)
    }
}

impl fmt::Display for ProcessTreeBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Kill scope a cancel may request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KillScope {
    /// Terminate the direct child only (fallback where no tree backend
    /// exists; the caller owns the orphan gap).
    DirectChild,
    /// Terminate the whole owned process tree.
    OwnedTree,
}

impl KillScope {
    /// Scope the backend can honor: tree backends take [`KillScope::OwnedTree`],
    /// [`ProcessTreeBackend::Unsupported`] degrades to
    /// [`KillScope::DirectChild`].
    #[must_use]
    pub const fn for_backend(backend: ProcessTreeBackend) -> Self {
        match backend {
            ProcessTreeBackend::Unsupported => Self::DirectChild,
            ProcessTreeBackend::LinuxProcessGroup
            | ProcessTreeBackend::MacosProcessGroup
            | ProcessTreeBackend::WindowsJobObject => Self::OwnedTree,
        }
    }

    /// Stable lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectChild => "direct_child",
            Self::OwnedTree => "owned_tree",
        }
    }
}

impl fmt::Display for KillScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_backend_maps_to_exactly_one_scope() {
        assert_eq!(
            KillScope::for_backend(ProcessTreeBackend::LinuxProcessGroup),
            KillScope::OwnedTree
        );
        assert_eq!(
            KillScope::for_backend(ProcessTreeBackend::MacosProcessGroup),
            KillScope::OwnedTree
        );
        assert_eq!(
            KillScope::for_backend(ProcessTreeBackend::WindowsJobObject),
            KillScope::OwnedTree
        );
        assert_eq!(
            KillScope::for_backend(ProcessTreeBackend::Unsupported),
            KillScope::DirectChild
        );
    }

    #[test]
    fn tree_capability_matches_the_scope_mapping() {
        for backend in [
            ProcessTreeBackend::LinuxProcessGroup,
            ProcessTreeBackend::MacosProcessGroup,
            ProcessTreeBackend::WindowsJobObject,
            ProcessTreeBackend::Unsupported,
        ] {
            assert_eq!(
                backend.kills_owned_tree(),
                KillScope::for_backend(backend) == KillScope::OwnedTree,
                "{backend} capability must agree with its kill scope"
            );
        }
    }

    #[test]
    fn detect_matches_the_compiling_platform() {
        let backend = ProcessTreeBackend::detect();
        #[cfg(target_os = "linux")]
        assert_eq!(backend, ProcessTreeBackend::LinuxProcessGroup);
        #[cfg(target_os = "macos")]
        assert_eq!(backend, ProcessTreeBackend::MacosProcessGroup);
        #[cfg(target_os = "windows")]
        assert_eq!(backend, ProcessTreeBackend::WindowsJobObject);
    }

    #[test]
    fn names_are_stable() {
        assert_eq!(
            ProcessTreeBackend::LinuxProcessGroup.as_str(),
            "linux_process_group"
        );
        assert_eq!(
            ProcessTreeBackend::MacosProcessGroup.as_str(),
            "macos_process_group"
        );
        assert_eq!(
            ProcessTreeBackend::WindowsJobObject.as_str(),
            "windows_job_object"
        );
        assert_eq!(ProcessTreeBackend::Unsupported.as_str(), "unsupported");
        assert_eq!(KillScope::DirectChild.as_str(), "direct_child");
        assert_eq!(KillScope::OwnedTree.as_str(), "owned_tree");
    }
}
