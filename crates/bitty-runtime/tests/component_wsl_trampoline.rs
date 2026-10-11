//! WSL2 trampoline gated integration (issue #1908).
//!
//! Windows CI-runnable: proves the live `wsl.exe --version` probe and the
//! fixed-argv trampoline wiring without ever failing the suite on runners
//! without WSL (skip with notice when WSL is absent). Unix legs prove the
//! no-op contract (native execution, zero spawn).

use bitty_runtime::component::{
    WslDistro, detect_wsl_context, select_wsl_distro, windows_path_to_guest, wsl_argv,
    wsl_trampoline_active,
};

#[test]
fn wsl_trampoline_gated_live_probe() {
    #[cfg(not(windows))]
    {
        // Unix behavior 100% unchanged: detection is a no-op with zero spawn,
        // trampoline never active even when a distro is selected.
        let detected = detect_wsl_context().expect("Unix detect is a no-op");
        assert_eq!(detected, None);
        let named = WslDistro::Named("Ubuntu".to_string());
        assert!(!wsl_trampoline_active(Some(&named)));
        assert!(!wsl_trampoline_active(None));
    }
    #[cfg(windows)]
    {
        // Windows: fail-closed when WSL is absent on the runner — skip with
        // notice instead of failing the suite. Only when WSL is present do we
        // assert the live probe plus the mapped argv wiring.
        match detect_wsl_context() {
            Ok(Some(version)) => {
                assert!(
                    !version.trim().is_empty(),
                    "version probe must name a version"
                );
                let selection = select_wsl_distro(Some("Ubuntu"), None)
                    .expect("valid distro")
                    .expect("trampoline selected");
                assert!(wsl_trampoline_active(Some(&selection)));
                let guest = windows_path_to_guest(
                    "C:\\Users\\ci\\bitty\\components\\net\\0.0.1\\bitty-net.exe",
                )
                .expect("mappable store path");
                assert_eq!(
                    guest,
                    "/mnt/c/Users/ci/bitty/components/net/0.0.1/bitty-net.exe"
                );
                let argv = wsl_argv(&selection, &guest, &[]).expect("fixed argv");
                assert_eq!(argv[0], "wsl.exe");
                assert!(argv.contains(&"--exec".to_string()));
                assert!(argv.contains(&guest));
            }
            Ok(None) => {
                // `Ok(None)` never happens on Windows (detection always probes);
                // treat as a skip with notice to keep the cell explicitly labeled.
                eprintln!("notice: WSL probe returned native on Windows, skipping live assertions");
            }
            Err(error) => {
                eprintln!(
                    "notice: WSL unavailable on this runner, skipping live WSL assertions ({})",
                    error.message()
                );
            }
        }
    }
}

#[test]
fn wsl_distro_selection_is_pure_on_every_platform() {
    // No spawn, no filesystem: explicit wins, `default` maps to the default
    // distro, invalid fails closed. Runs everywhere (unit counterpart lives in
    // `wsl.rs`; this proves the public crate surface).
    assert_eq!(select_wsl_distro(None, None).expect("none"), None);
    assert_eq!(
        select_wsl_distro(Some("default"), None).expect("explicit default"),
        Some(WslDistro::Default)
    );
    assert_eq!(
        select_wsl_distro(None, Some("Debian")).expect("env"),
        Some(WslDistro::Named("Debian".to_string()))
    );
    assert!(select_wsl_distro(Some("../evil"), None).is_err());
    // Pure mapping plus fixed argv also run everywhere (no WSL needed): the
    // primary Windows-to-guest direction and the discrete argv preservation.
    let guest =
        windows_path_to_guest("C:\\Users\\ci\\bitty\\components\\net\\0.0.1\\bitty-net.exe")
            .expect("mappable");
    assert_eq!(
        guest,
        "/mnt/c/Users/ci/bitty/components/net/0.0.1/bitty-net.exe"
    );
    let selection = WslDistro::Named("Ubuntu".to_string());
    let argv = wsl_argv(
        &selection,
        &guest,
        &["--flag".to_string(), "a b".to_string()],
    )
    .expect("argv");
    assert_eq!(argv[0], "wsl.exe");
    assert!(argv.contains(&"--exec".to_string()));
    assert!(argv.contains(&guest));
    assert!(argv.contains(&"a b".to_string()));
}
