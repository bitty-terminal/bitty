//! Overlay lifecycle: create the per-run qcow2 overlay and read back its
//! backing file. Creation is refused unless the plan passes policy
//! validation, the base image exists, and no overlay with the same run id
//! already exists (a run id is single-use; reruns get a new id).

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::policy::RunPlan;

/// Run `qemu-img create` for the plan's overlay. Requires the base image to
/// exist; never touches the base image itself.
pub fn create_overlay(qemu_img: &Path, plan: &RunPlan) -> io::Result<PathBuf> {
    if let Err(violations) = plan.validate() {
        let detail: Vec<String> = violations.iter().map(ToString::to_string).collect();
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("run plan violates policy: {}", detail.join("; ")),
        ));
    }

    let base = plan.base_image();
    if !base.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "base image {} is not prepared (manual, far-future step; see specifications/vm-tier-policy.md)",
                base.display()
            ),
        ));
    }

    let overlay = plan.overlay_image();
    if overlay.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "overlay {} already exists; run ids are single-use",
                overlay.display()
            ),
        ));
    }

    std::fs::create_dir_all(plan.run_dir())?;

    let output = Command::new(qemu_img)
        .args(plan.qemu_img_create_args())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "qemu-img create failed with {}: {}",
            output.status,
            first_non_empty_line(&String::from_utf8_lossy(&output.stderr)).unwrap_or("no output")
        )));
    }
    Ok(overlay)
}

/// Read the backing file name of `overlay` via `qemu-img info --output=json`.
pub fn overlay_backing_file(qemu_img: &Path, overlay: &Path) -> io::Result<String> {
    let output = Command::new(qemu_img)
        .arg("info")
        .arg("--output=json")
        .arg(overlay)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "qemu-img info failed with {}: {}",
            output.status,
            first_non_empty_line(&String::from_utf8_lossy(&output.stderr)).unwrap_or("no output")
        )));
    }
    let json = String::from_utf8_lossy(&output.stdout);
    parse_backing_filename(&json).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "qemu-img info output has no backing-filename",
        )
    })
}

/// Extract `"backing-filename": "..."` from `qemu-img info --output=json`.
/// Handles `\"` escapes; returns `None` when the key is absent.
pub fn parse_backing_filename(json: &str) -> Option<String> {
    const KEY: &str = "\"backing-filename\": \"";
    let start = json.find(KEY)? + KEY.len();
    let mut out = String::new();
    let mut chars = json[start..].chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '"' => return Some(out),
            _ => out.push(ch),
        }
    }
    None
}

/// First non-empty line, for compact error details.
pub fn first_non_empty_line(raw: &str) -> Option<&str> {
    raw.lines().find(|line| !line.trim().is_empty())
}

/// Single-quote `raw` unless it is already shell-safe. Used only to render
/// dry-run commands; nothing here executes a shell.
pub fn shell_quote(raw: &str) -> String {
    let safe = !raw.is_empty()
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._+-=:@,".contains(&b));
    if safe {
        raw.to_string()
    } else {
        format!("'{}'", raw.replace('\'', "'\\''"))
    }
}

/// Render a program plus arguments as one shell-safe command line.
pub fn render_command(program: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(shell_quote(program));
    parts.extend(args.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Cadence, RunPlan, guest};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique scratch root per hermetic test (parallel-safe); removed on drop.
    struct HermeticRoot(std::path::PathBuf);

    impl HermeticRoot {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "bitty-test-vm-hermetic-{tag}-{}-{n}",
                std::process::id()
            )))
        }
    }

    impl Drop for HermeticRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A qemu-img path that cannot exist, proving the guard under test fires
    /// before any subprocess is spawned.
    fn missing_qemu_img(root: &HermeticRoot) -> std::path::PathBuf {
        root.0.join("no-such-qemu-img")
    }

    fn hermetic_plan(root: &HermeticRoot, run_id: &str) -> RunPlan {
        let guest = guest("arch").expect("arch guest exists");
        RunPlan::new(guest, Cadence::Pr, run_id, &root.0).expect("plan builds")
    }

    #[test]
    fn refuses_reused_run_id_before_touching_qemu_img() {
        // Exactly-once proof in the fast tier: run ids are single-use and the
        // refusal must not depend on qemu-img being installed. The pre-existing
        // overlay plus a nonexistent binary isolates the guard itself.
        let root = HermeticRoot::new("reuse");
        let plan = hermetic_plan(&root, "reuse-1");
        let base = plan.base_image();
        std::fs::create_dir_all(base.parent().expect("base dir")).expect("base dir");
        std::fs::write(&base, []).expect("fixture base marker");
        let overlay = plan.overlay_image();
        std::fs::create_dir_all(overlay.parent().expect("run dir")).expect("run dir");
        std::fs::write(&overlay, []).expect("pre-existing overlay marker");

        let error = create_overlay(&missing_qemu_img(&root), &plan)
            .expect_err("reused run id must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
    }

    #[test]
    fn refuses_missing_base_before_touching_qemu_img() {
        // Same isolation for the missing-base guard: NotFound without ever
        // spawning a subprocess.
        let root = HermeticRoot::new("missing");
        let plan = hermetic_plan(&root, "missing-1");

        let error = create_overlay(&missing_qemu_img(&root), &plan)
            .expect_err("missing base must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
    }

    #[test]
    fn parses_backing_filename_from_qemu_json() {
        let json = r#"{
    "filename": "/vm-root/runs/arch-1/arch-overlay.qcow2",
    "format": "qcow2",
    "backing-filename": "/vm-root/images/base/arch.qcow2",
    "virtual-size": 8589934592
}"#;
        assert_eq!(
            parse_backing_filename(json),
            Some("/vm-root/images/base/arch.qcow2".to_string())
        );
    }

    #[test]
    fn parses_escaped_backing_filename_and_rejects_missing_key() {
        let json = r#"{"backing-filename": "/vm-root/images/base/a\"b.qcow2"}"#;
        assert_eq!(
            parse_backing_filename(json),
            Some("/vm-root/images/base/a\"b.qcow2".to_string())
        );
        assert_eq!(parse_backing_filename(r#"{"format": "qcow2"}"#), None);
    }

    #[test]
    fn first_non_empty_line_skips_blank_output() {
        assert_eq!(first_non_empty_line(""), None);
        assert_eq!(first_non_empty_line("\n  \nboom\nmore"), Some("boom"));
    }

    #[test]
    fn shell_quoting_keeps_safe_paths_and_quotes_hostile_ones() {
        assert_eq!(
            shell_quote("/vm-root/runs/a/overlay.qcow2"),
            "/vm-root/runs/a/overlay.qcow2"
        );
        assert_eq!(shell_quote("/vm root/x"), "'/vm root/x'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");

        let rendered = render_command("qemu-img", &["create".to_string(), "a b".to_string()]);
        assert_eq!(rendered, "qemu-img create 'a b'");
    }
}
