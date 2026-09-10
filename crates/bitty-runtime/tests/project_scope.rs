#![forbid(unsafe_code)]
//! Single-source project filesystem scope — grant/gate agreement pins (CTX-0300).
//!
//! The runtime scope constants are the single source of truth for the
//! `fs.read:~/projects/**` grant consumed by project and browser surfaces.
//! These tests pin the exact scope value, pin that the first-party
//! `bitty-terminal.project` manifest grants exactly that pattern, and pin
//! that the shared pure gate accepts the granted subtree and fails closed on
//! escapes (no traversal, no prefix siblings, no outside paths).

use std::collections::BTreeSet;

use bitty_plugin_host::{
    CapabilityFamily, CapabilityId, FsAccess, PluginManifest, bundled::project_manifest,
};
use bitty_runtime::{
    browser_panel::BrowserIntegration,
    project::ProjectIntegration,
    project_scope::{
        PROJECT_FS_PATTERN, PROJECT_PATH_MAX_BYTES, PROJECT_ROOT, is_within_project_scope,
    },
};

fn granted_set_for(manifest: &PluginManifest) -> BTreeSet<CapabilityId> {
    let mut set = manifest.capabilities.ids.clone();
    for req in &manifest.capabilities.filesystem {
        for pat in &req.paths {
            let s = match req.access {
                FsAccess::Read => format!("fs.read:{pat}"),
                FsAccess::Write => format!("fs.write:{pat}"),
            };
            set.insert(CapabilityId::parse(&s).unwrap());
        }
    }
    set
}

#[test]
fn scope_constants_are_single_source_and_pinned() {
    assert_eq!(PROJECT_ROOT, "~/projects");
    assert_eq!(PROJECT_FS_PATTERN, "~/projects/**");
    assert_eq!(PROJECT_PATH_MAX_BYTES, 4096);
    assert_eq!(PROJECT_FS_PATTERN.strip_suffix("/**"), Some(PROJECT_ROOT));
}

#[test]
fn project_manifest_grants_the_single_source_pattern() {
    let manifest = project_manifest();
    assert_eq!(manifest.capabilities.filesystem.len(), 1);
    let req = &manifest.capabilities.filesystem[0];
    assert_eq!(req.access, FsAccess::Read);
    assert_eq!(req.paths, vec![PROJECT_FS_PATTERN.to_string()]);
    let granted = granted_set_for(&manifest);
    let expected = CapabilityId::parse(&format!("fs.read:{PROJECT_FS_PATTERN}")).unwrap();
    assert!(granted.contains(&expected));
    assert_eq!(expected.as_str(), "fs.read:~/projects/**");
    assert_eq!(expected.family(), CapabilityFamily::Fs);
    assert!(!granted.contains(&CapabilityId::parse("fs.read:/etc/passwd").unwrap()));
}

#[test]
fn grant_and_gate_agree_on_allowed_paths() {
    for allowed in [
        "~/projects",
        "~/projects/",
        "~/projects/foo",
        "~/projects/foo/bar",
    ] {
        assert!(is_within_project_scope(allowed));
        assert!(ProjectIntegration::is_within_projects(allowed));
        assert!(ProjectIntegration::is_fs_allowed(allowed));
        assert!(BrowserIntegration::is_within_file_scope(&format!(
            "file://{allowed}"
        )));
    }
}

#[test]
fn grant_and_gate_fail_closed_on_escapes() {
    for denied in [
        "",
        "/etc/passwd",
        "/home/user/projects/foo",
        "~/projectsx",
        "~/projects/../etc/passwd",
        "~/projects/foo/../../etc",
        "~/Documents/foo",
    ] {
        assert!(!is_within_project_scope(denied), "{denied}");
        assert!(!ProjectIntegration::is_within_projects(denied), "{denied}");
        assert!(!ProjectIntegration::is_fs_allowed(denied), "{denied}");
    }
    assert!(!BrowserIntegration::is_within_file_scope(
        "file:///etc/passwd"
    ));
    assert!(!BrowserIntegration::is_within_file_scope(
        "file://~/projectsx"
    ));
    assert!(!BrowserIntegration::is_within_file_scope(
        "file://~/projects/../etc"
    ));
}
