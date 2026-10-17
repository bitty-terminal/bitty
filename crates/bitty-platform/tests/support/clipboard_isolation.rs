//! Test-only isolation gate. No clipboard calls, display connections or content reads.

#[derive(Debug, Default)]
pub struct IsolationEvidence {
    pub linux: bool,
    pub display_number: Option<u32>,
    pub wayland_present: bool,
    pub server_pid: Option<u32>,
    pub display_lock_pid: Option<u32>,
    pub server_parent: Option<u32>,
    pub test_parent: Option<u32>,
    pub server_is_xvfb: bool,
    pub server_uses_displayfd: bool,
    pub server_disables_tcp: bool,
    pub private_tmp: bool,
    pub private_run: bool,
    pub server_owns_display_socket: bool,
}

pub fn validate(e: &IsolationEvidence) -> Result<(), &'static str> {
    if !e.linux
        || e.display_number.is_none()
        || e.wayland_present
        || e.server_pid.is_none()
        || e.server_pid != e.display_lock_pid
        || e.server_parent.is_none()
        || e.server_parent != e.test_parent
        || !e.server_is_xvfb
        || !e.server_uses_displayfd
        || !e.server_disables_tcp
        || !e.private_tmp
        || !e.private_run
        || !e.server_owns_display_socket
    {
        return Err("requires the disposable Xvfb namespace runner; native access not executed");
    }
    Ok(())
}

pub fn run_if_isolated(e: &IsolationEvidence, run: impl FnOnce()) -> Result<(), &'static str> {
    validate(e)?;
    run();
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn current() -> IsolationEvidence {
    use std::{env, fs, os::unix::fs::FileTypeExt, path::Path};

    fn parent(pid: u32) -> Option<u32> {
        fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("PPid:")?.trim().parse().ok())
    }

    // The marker is a discovery hint only. Every independent fact below is
    // required before a native constructor may be called.
    let Some(server_pid) = env::var("BITTY_CLIPBOARD_XVFB_PID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid > 1)
    else {
        return IsolationEvidence::default();
    };
    let display_number = env::var("DISPLAY").ok().and_then(|value| {
        let number = value.strip_prefix(':')?;
        number
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| number.parse().ok())
            .flatten()
    });
    let args = fs::read(format!("/proc/{server_pid}/cmdline")).unwrap_or_default();
    let args: Vec<_> = args.split(|byte| *byte == 0).collect();
    let mounts = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let private_mount = |path: &str| {
        mounts.lines().any(|line| {
            let Some((fields, kind)) = line.split_once(" - ") else {
                return false;
            };
            let fields: Vec<_> = fields.split_whitespace().collect();
            fields.get(3) == Some(&"/")
                && fields.get(4) == Some(&path)
                && kind.starts_with("tmpfs ")
        })
    };
    IsolationEvidence {
        linux: true,
        display_number,
        wayland_present: env::var_os("WAYLAND_DISPLAY").is_some()
            || env::var_os("WAYLAND_SOCKET").is_some(),
        server_pid: Some(server_pid),
        display_lock_pid: display_number.and_then(|number| {
            fs::read_to_string(format!("/tmp/.X{number}-lock"))
                .ok()?
                .trim()
                .parse()
                .ok()
        }),
        server_parent: parent(server_pid),
        test_parent: parent(std::process::id()),
        server_is_xvfb: fs::read_link(format!("/proc/{server_pid}/exe"))
            .ok()
            .and_then(|path| path.file_name().map(|name| name == Path::new("Xvfb")))
            .unwrap_or(false),
        server_uses_displayfd: args.contains(&b"-displayfd".as_slice()),
        server_disables_tcp: args
            .windows(2)
            .any(|pair| pair == [b"-nolisten".as_slice(), b"tcp".as_slice()]),
        private_tmp: private_mount("/tmp"),
        private_run: private_mount("/run"),
        server_owns_display_socket: display_number.is_some_and(|number| {
            let socket_path = format!("/tmp/.X11-unix/X{number}");
            if !fs::symlink_metadata(&socket_path)
                .is_ok_and(|metadata| metadata.file_type().is_socket())
            {
                return false;
            }
            let sockets = fs::read_to_string("/proc/net/unix").unwrap_or_default();
            let Ok(descriptors) = fs::read_dir(format!("/proc/{server_pid}/fd")) else {
                return false;
            };
            let owned: Vec<_> = descriptors
                .flatten()
                .filter_map(|entry| fs::read_link(entry.path()).ok())
                .collect();
            sockets.lines().any(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                fields.get(7) == Some(&socket_path.as_str())
                    && fields
                        .get(6)
                        .is_some_and(|inode| owned.contains(&format!("socket:[{inode}]").into()))
            })
        }),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn current() -> IsolationEvidence {
    // No disposable native backend runner has been established here yet.
    IsolationEvidence::default()
}
