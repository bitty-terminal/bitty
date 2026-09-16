//! CTX-0485 hostile probes: external-editor program allowlist + temp-file
//! posture at the public `bitty-rich` boundary.
//!
//! These are the reviewer-facing red/green probes for the composer
//! external-editor round trip (parent issue #748). Red run against the
//! pre-fix tree (recorded in the PR body):
//! `"sh"` resolved to `Some("sh")`, `"ed"` was accepted, and the temp file
//! was named `bitty-composer-<pid>-<nanos>-<seq>.sh`. They now stay as
//! regression evidence.

#[cfg(unix)]
use bitty_rich::composer::TempComposerFile;
use bitty_rich::composer::{EDITOR_ALLOWLIST, EditorError, resolve_editor};

#[cfg(unix)]
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx0485-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("probe dir");
    dir
}

#[test]
fn hostile_editor_values_never_resolve_to_a_spawnable_program() {
    for hostile in [
        "sh",
        "/bin/sh",
        "/usr/bin/env",
        "/tmp/evil/nvim",
        "nvim -u NONE",
        "vim;id",
        "NVIM",
        "./nvim",
    ] {
        let resolved = resolve_editor(Some(hostile), Some("vim"));
        assert!(
            matches!(resolved, Err(EditorError::NotAllowed)),
            "hostile editor resolved: {hostile:?} -> {resolved:?}"
        );
    }
}

#[test]
fn allowlisted_editors_resolve_and_nothing_else_does() {
    for name in EDITOR_ALLOWLIST {
        assert_eq!(resolve_editor(None, Some(name)), Ok((*name).to_string()));
    }
    assert_eq!(
        resolve_editor(None, Some("ed")),
        Err(EditorError::NotAllowed)
    );
}

#[test]
#[cfg(unix)]
fn composer_temp_file_is_owner_only_and_not_script_suffixed() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = temp_dir("temp");
    let temp: TempComposerFile = bitty_rich::write_composer_temp("echo hi", &dir).expect("temp");
    let name = temp
        .path()
        .file_name()
        .expect("name")
        .to_string_lossy()
        .into_owned();
    assert!(
        !name.to_ascii_lowercase().ends_with(".sh"),
        "script suffix on composer temp: {name}"
    );
    let mode = std::fs::metadata(temp.path())
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "temp file must stay owner-only: {mode:o}");
    let _ = std::fs::remove_dir_all(&dir);
}
