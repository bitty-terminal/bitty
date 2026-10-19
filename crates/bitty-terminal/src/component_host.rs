//! Production native-component host construction (CTX-1361, bitty#2254).
//!
//! Resolves the same user/system component roots as `bitty extension`
//! (`$XDG_DATA_HOME`/`$HOME`, with the developer-only
//! `BITTY_COMPONENTS_DIR` / `BITTY_SYSTEM_COMPONENTS_DIR` overrides) and
//! installs one [`ComponentHost`] on the runtime. The host never consults
//! `PATH`; a missing install fails closed at request time.
//!
//! This module is shared by the binary and the composition-root library
//! target. Diagnostics are injected so the binary can route them through
//! `logging` while headless tests can stay silent.

use std::path::PathBuf;

use bitty_runtime::Runtime;
use bitty_runtime::component::{
    BrokerConfig, COMPONENTS_DIR_ENV, ComponentEnv, ComponentHost, ComponentWarnSink,
    SYSTEM_COMPONENTS_DIR_ENV, component_search_roots_for,
};

/// Resolve the production component roots from the process environment.
///
/// Returns `(user, system)`. The system tier always resolves (platform
/// default unless the developer override names one), so an unavailable user
/// data directory yields `(None, Some(...))`, not "no host".
#[must_use]
pub fn roots_from_env() -> (Option<PathBuf>, Option<PathBuf>) {
    let xdg_data_home = std::env::var("XDG_DATA_HOME").ok();
    let home = std::env::var("HOME").ok();
    let components_dir = std::env::var(COMPONENTS_DIR_ENV).ok();
    let system_components_dir = std::env::var(SYSTEM_COMPONENTS_DIR_ENV).ok();
    let data_home =
        bitty_runtime::component::data_home_for(xdg_data_home.as_deref(), home.as_deref());
    component_search_roots_for(
        components_dir.as_deref(),
        data_home.as_deref(),
        system_components_dir.as_deref(),
    )
}

/// Build a host for explicit roots (the environment-free production
/// constructor used by [`install`] and the integration slice).
#[must_use]
pub fn host_for_roots(
    user_root: Option<PathBuf>,
    system_root: Option<PathBuf>,
    warn: impl ComponentWarnSink + 'static,
) -> Option<ComponentHost> {
    if user_root.is_none() && system_root.is_none() {
        return None;
    }
    let config =
        BrokerConfig::new(user_root, ComponentEnv::from_process()).with_system_root(system_root);
    Some(ComponentHost::from_config(config).with_warn_sink(warn))
}

/// Install the production host on `runtime`; returns whether one was installed.
pub fn install(runtime: &mut Runtime, warn: impl ComponentWarnSink + 'static) -> bool {
    let (user_root, system_root) = roots_from_env();
    install_with_roots(runtime, user_root, system_root, warn)
}

/// Install a host for explicit roots; returns whether one was installed.
pub fn install_with_roots(
    runtime: &mut Runtime,
    user_root: Option<PathBuf>,
    system_root: Option<PathBuf>,
    warn: impl ComponentWarnSink + 'static,
) -> bool {
    match host_for_roots(user_root, system_root, warn) {
        Some(host) => {
            runtime.install_component_host(host);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_honor_component_override() {
        // Pure shape check: the resolver is environment-driven, so this test
        // only asserts that an explicit override path is preserved.
        let (user, system) = component_search_roots_for(
            Some("component-root-test"),
            None,
            Some("system-component-root-test"),
        );
        assert_eq!(user, Some(PathBuf::from("component-root-test")));
        assert_eq!(system, Some(PathBuf::from("system-component-root-test")));
    }
}
