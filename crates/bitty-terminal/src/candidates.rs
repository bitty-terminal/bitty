//! Bounded local completion candidates (CTX-1486, issue #2575).
//!
//! # Hidden helpers (not public commands)
//!
//! Generated completion scripts call two local helpers when the shell offers
//! dynamic values:
//!
//! - `bitty +list-profiles [--json]` — profile names under
//!   `$XDG_CONFIG_HOME/bitty/profiles/`.
//! - `bitty +list-plugins --installed [--json]` — ids of plugins present in
//!   the managed `bitty-plugins.lua` ledger or the installed XDG plugin
//!   store. The plain `+list-plugins`/`list plugins` alias keeps its existing
//!   static-catalog contract; `--installed` is the candidate-only view.
//!
//! Both helpers are hidden: they are absent from `bitty --help`, the static
//! grammar, and completion candidate lists. They exist so the Bash, Zsh,
//! Fish, and PowerShell scripts share one implementation instead of
//! re-deriving local state.
//!
//! # Contract (implemented)
//!
//! - Class: local and read-only. No runtime instance, no plugin VM, no
//!   component host, no network I/O, no consent prompt, no shell spawn.
//! - Input: local configuration/store state only. The helpers never inspect
//!   the terminal environment beyond the XDG/HOME variables used for path
//!   resolution, and never accept a `--socket`/`--instance` target.
//! - Output: one candidate per line, sorted bytewise and de-duplicated.
//!   `--json` emits one `{"v":1,"kind":"<kind>","candidates":[...]}` object.
//!   Candidates never carry diagnostics, host paths, or ANSI escapes.
//! - Bounds: at most [`MAX_CANDIDATES`] candidates, each at most
//!   [`MAX_CANDIDATE_BYTES`] bytes; the underlying state/store reads keep
//!   their own byte ceilings (`bitty-plugins.lua` 64 KiB, `current.json`
//!   bounded by the runtime index ceiling).
//! - Failure: missing or malformed state yields an empty candidate set with
//!   exit 0 and no stderr, so a broken store can never break the shell.
//! - Usage errors (unknown flag) print usage to stderr and exit 2; the shells
//!   redirect that stream away from the candidate list.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

/// Maximum candidates emitted per helper call (deterministic bound).
pub(crate) const MAX_CANDIDATES: usize = 256;

/// Maximum bytes for one candidate (matches the plugin-id ceiling).
pub(crate) const MAX_CANDIDATE_BYTES: usize = 128;

/// Stable taxonomy for the hidden helpers.
pub const EXIT_OK: i32 = 0;
/// CLI usage error.
pub const EXIT_USAGE: i32 = 2;

/// Which local candidate set a hidden helper emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateKind {
    /// Profile names (`bitty config profile ...`, `--profile`).
    Profiles,
    /// Installed plugin ids (`bitty plugin enable|disable ...`).
    Plugins,
}

impl CandidateKind {
    /// Stable JSON `kind` label.
    fn label(self) -> &'static str {
        match self {
            Self::Profiles => "profiles",
            Self::Plugins => "plugins",
        }
    }

    /// Usage line for one helper.
    fn usage(self) -> &'static str {
        match self {
            Self::Profiles => "Usage: bitty +list-profiles [--json]",
            Self::Plugins => "Usage: bitty +list-plugins --installed [--json]",
        }
    }
}

/// Parsed hidden-helper invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HiddenRequest {
    /// Candidate set to emit.
    pub kind: CandidateKind,
    /// Emit the JSON envelope instead of one candidate per line.
    pub json: bool,
}

/// Match the hidden helpers at `argv[1]`; `None` means "not a helper".
///
/// `raw` is the complete process argv, so callers can pass
/// `std::env::args()` directly. Only a leading helper token is recognized:
/// global flags before the token keep their normal parser path.
pub(crate) fn parse_hidden(raw: &[String]) -> Option<Result<HiddenRequest, String>> {
    let first = raw.get(1)?.as_str();
    let (kind, rest) = match first {
        "+list-profiles" => (CandidateKind::Profiles, &raw[2..]),
        "+list-plugins" => {
            // `+list-plugins` without `--installed` stays the existing
            // static-catalog alias, so only intercept the explicit view.
            if !raw[2..].iter().any(|token| token == "--installed") {
                return None;
            }
            (CandidateKind::Plugins, &raw[2..])
        }
        _ => return None,
    };
    let mut json = false;
    let mut installed = false;
    for token in rest {
        match token.as_str() {
            "--json" if !json => json = true,
            "--json" => {
                return Some(Err(format!(
                    "bitty {first}: duplicate --json\n{}",
                    kind.usage()
                )));
            }
            "--installed" if kind == CandidateKind::Plugins && !installed => installed = true,
            "--installed" if kind == CandidateKind::Plugins => {
                return Some(Err(format!(
                    "bitty {first}: duplicate --installed\n{}",
                    kind.usage()
                )));
            }
            other => {
                return Some(Err(format!(
                    "bitty {first}: unexpected argument {other:?}\n{}",
                    kind.usage()
                )));
            }
        }
    }
    Some(Ok(HiddenRequest { kind, json }))
}

/// Sorted, validated, de-duplicated, bounded candidate list.
fn finalize(candidates: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    for name in candidates {
        if name.is_empty() || name.len() > MAX_CANDIDATE_BYTES || name.chars().any(char::is_control)
        {
            continue;
        }
        names.insert(name);
    }
    names.into_iter().take(MAX_CANDIDATES).collect()
}

/// Profile names under the resolved profile directory (empty on any failure).
fn profile_candidates() -> Vec<String> {
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    let Some(dir) = crate::profile::profile_dir_from(None, bitty_config_env.as_deref()).ok() else {
        return Vec::new();
    };
    crate::profile::candidate_names(&dir)
}

/// Installed plugin ids from the managed ledger plus the XDG store.
///
/// Either source may be absent or unreadable; a read failure contributes no
/// ids instead of surfacing an error (the helper must be fail-soft).
fn plugin_candidates() -> Vec<String> {
    let bitty_config_env = std::env::var("BITTY_CONFIG").ok();
    let xdg_config_home = std::env::var("XDG_CONFIG_HOME").ok();
    let xdg_data_home = std::env::var("XDG_DATA_HOME").ok();
    let home = std::env::var("HOME").ok();
    let state_path = crate::plugin::init_plugin_state_path(
        None,
        bitty_config_env.as_deref(),
        xdg_config_home.as_deref(),
        home.as_deref(),
    )
    .ok();
    let store_root =
        crate::plugin_runtime::store_root_for(xdg_data_home.as_deref(), home.as_deref());
    plugin_candidates_from(state_path.as_deref(), store_root.as_deref())
}

/// Superset extraction for the plugin candidates (state + store).
fn plugin_candidates_from(state_path: Option<&Path>, store_root: Option<&Path>) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    if let Some(path) = state_path {
        if let Ok(state) = crate::plugin::load_state_for_inspect(path) {
            ids.extend(state.ids().cloned());
        }
    }
    if let Some(root) = store_root {
        if let Ok(records) = bitty_runtime::plugin_runtime::resolution::load_index(root) {
            ids.extend(records.into_iter().map(|record| record.plugin_id));
        }
    }
    finalize(ids)
}

/// Render candidates to `out`; write failures are ignored (a closed pipe is
/// not a completion error).
fn write_candidates(
    out: &mut dyn Write,
    kind: CandidateKind,
    candidates: &[String],
    json: bool,
) -> i32 {
    if json {
        let mut body = String::from("{\"v\":1,\"kind\":\"");
        body.push_str(kind.label());
        body.push_str("\",\"candidates\":[");
        for (index, candidate) in candidates.iter().enumerate() {
            if index > 0 {
                body.push(',');
            }
            body.push('"');
            push_json_string(&mut body, candidate);
            body.push('"');
        }
        body.push_str("]}\n");
        let _ = out.write_all(body.as_bytes());
    } else {
        for candidate in candidates {
            let _ = writeln!(out, "{candidate}");
        }
    }
    EXIT_OK
}

/// JSON string escaping without pulling a dependency into this helper path.
fn push_json_string(out: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
}

/// Execute one parsed helper request; always exits 0.
fn run_request(request: HiddenRequest, out: &mut dyn Write) -> i32 {
    let candidates = match request.kind {
        CandidateKind::Profiles => profile_candidates(),
        CandidateKind::Plugins => plugin_candidates(),
    };
    write_candidates(out, request.kind, &candidates, request.json)
}

/// Runs a hidden helper when `raw` names one; `None` keeps the normal parser.
pub(crate) fn run_hidden(raw: &[String]) -> Option<i32> {
    let request = match parse_hidden(raw)? {
        Ok(request) => request,
        Err(message) => {
            eprintln!("{message}");
            return Some(EXIT_USAGE);
        }
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    Some(run_request(request, &mut out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("bitty-ctx1486-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn argv(words: &[&str]) -> Vec<String> {
        std::iter::once("bitty")
            .chain(words.iter().copied())
            .map(str::to_string)
            .collect()
    }

    fn write_profile(dir: &Path, name: &str) {
        std::fs::write(dir.join(format!("{name}.lua")), "return {}\n").expect("profile file");
    }

    fn write_state(path: &Path, ids: &[&str]) {
        let records: std::collections::BTreeMap<String, crate::plugin::PluginRecord> = ids
            .iter()
            .map(|id| {
                (
                    (*id).to_string(),
                    crate::plugin::PluginRecord {
                        source: "bundled".to_string(),
                        manifest_hash: "a".repeat(64),
                        enabled: true,
                        granted: std::collections::BTreeSet::new(),
                        denied: std::collections::BTreeSet::new(),
                    },
                )
            })
            .collect();
        let state = crate::plugin::PluginState::from_records(records);
        std::fs::write(path, state.render()).expect("state file");
    }

    fn write_legacy_state(path: &Path, ids: &[&str]) {
        let mut text = String::from("state_version = 1\n");
        for id in ids {
            text.push_str(&format!(
                "[plugins.\"{id}\"]\nsource = \"bundled\"\n\
                 manifest_hash = \"{}\"\nenabled = true\ngranted = []\n",
                "a".repeat(64)
            ));
        }
        std::fs::write(path, text).expect("legacy toml ledger");
    }

    fn store_record(id: &str) -> bitty_runtime::plugin_runtime::resolution::PluginRecord {
        bitty_runtime::plugin_runtime::resolution::PluginRecord {
            source_class: bitty_runtime::plugin_runtime::SourceClass::LocalPath,
            plugin_id: id.to_string(),
            version: "1.0.0".to_string(),
            root: format!("/store/{id}"),
            manifest_hash: "b".repeat(64),
            content_digest: "c".repeat(64),
            enabled: true,
            granted: Vec::new(),
        }
    }

    #[test]
    fn parse_hidden_recognizes_only_the_helper_forms() {
        assert_eq!(
            parse_hidden(&argv(&["+list-profiles"])),
            Some(Ok(HiddenRequest {
                kind: CandidateKind::Profiles,
                json: false
            }))
        );
        assert_eq!(
            parse_hidden(&argv(&["+list-plugins", "--installed", "--json"])),
            Some(Ok(HiddenRequest {
                kind: CandidateKind::Plugins,
                json: true
            }))
        );
        // The plain static-catalog alias stays on the normal parser path.
        assert_eq!(parse_hidden(&argv(&["+list-plugins"])), None);
        assert_eq!(parse_hidden(&argv(&["+list-themes"])), None);
        assert_eq!(parse_hidden(&argv(&["config", "profile", "list"])), None);
        assert_eq!(parse_hidden(&[]), None);
    }

    #[test]
    fn parse_hidden_rejects_unknown_and_duplicate_flags() {
        for words in [
            vec!["+list-profiles", "--bogus"],
            vec!["+list-profiles", "--json", "--json"],
            vec!["+list-plugins", "--installed", "--bogus"],
            vec!["+list-plugins", "--installed", "--installed"],
            vec!["+list-profiles", "extra"],
        ] {
            assert!(
                matches!(parse_hidden(&argv(&words)), Some(Err(_))),
                "words: {words:?}"
            );
        }
        // `--installed` only belongs to the plugin view.
        assert!(matches!(
            parse_hidden(&argv(&["+list-profiles", "--installed"])),
            Some(Err(_))
        ));
    }

    #[test]
    fn profiles_are_sorted_deduplicated_and_validated() {
        let dir = scratch_dir("profiles");
        write_profile(&dir, "work");
        write_profile(&dir, "alpha");
        std::fs::write(dir.join("not a profile.lua"), "return {}\n").expect("bad name");
        std::fs::write(dir.join("notes.txt"), "ignored\n").expect("ignored");
        std::fs::create_dir(dir.join("nested.lua")).expect("directory");
        assert_eq!(
            crate::profile::candidate_names(&dir),
            vec!["alpha".to_string(), "work".to_string()]
        );
    }

    #[test]
    fn profiles_missing_directory_is_empty() {
        let dir = scratch_dir("profiles-missing").join("absent");
        assert!(crate::profile::candidate_names(&dir).is_empty());
    }

    #[test]
    fn profiles_are_bounded() {
        let dir = scratch_dir("profiles-cap");
        for index in 0..(MAX_CANDIDATES + 25) {
            write_profile(&dir, &format!("p{index:04}"));
        }
        let names = crate::profile::candidate_names(&dir);
        assert_eq!(names.len(), MAX_CANDIDATES);
        assert_eq!(names.first().map(String::as_str), Some("p0000"));
        assert_eq!(
            names.last().map(String::as_str),
            Some("p0255"),
            "bounded list keeps the deterministic prefix"
        );
    }

    #[test]
    fn plugins_union_state_and_store_sorted() {
        let dir = scratch_dir("plugins");
        let state = dir.join("bitty-plugins.lua");
        write_state(&state, &["zeta.bundled", "alpha.bundled"]);
        let store = dir.join("store");
        std::fs::create_dir_all(&store).expect("store dir");
        bitty_runtime::plugin_runtime::resolution::write_index(
            &store,
            &[
                store_record("middle.installed"),
                store_record("aaa.installed"),
            ],
        )
        .expect("store index");
        assert_eq!(
            plugin_candidates_from(Some(&state), Some(&store)),
            vec![
                "aaa.installed".to_string(),
                "alpha.bundled".to_string(),
                "middle.installed".to_string(),
                "zeta.bundled".to_string()
            ]
        );
    }

    #[test]
    fn plugins_union_is_bounded_sorted_and_deduplicated() {
        let dir = scratch_dir("plugins-bounded");
        let state = dir.join("bitty-plugins.lua");
        let managed: Vec<String> = (0..200)
            .map(|index| format!("s{index:04}.bundled"))
            .collect();
        let managed_refs: Vec<&str> = managed.iter().map(String::as_str).collect();
        write_state(&state, &managed_refs);

        // One store id duplicates a managed id; the rest are store-only.
        let mut records = vec![store_record("s0000.bundled")];
        for index in 0..99 {
            records.push(store_record(&format!("t{index:04}.installed")));
        }
        let store = dir.join("store");
        std::fs::create_dir_all(&store).expect("store dir");
        bitty_runtime::plugin_runtime::resolution::write_index(&store, &records)
            .expect("store index");

        let candidates = plugin_candidates_from(Some(&state), Some(&store));
        assert_eq!(candidates.len(), MAX_CANDIDATES);
        assert!(
            candidates.windows(2).all(|pair| pair[0] < pair[1]),
            "union stays sorted and de-duplicated"
        );
        assert_eq!(
            candidates
                .iter()
                .filter(|id| id.as_str() == "s0000.bundled")
                .count(),
            1,
            "a managed/store duplicate appears once"
        );
    }

    #[test]
    fn plugins_legacy_toml_fallback_when_lua_absent() {
        let dir = scratch_dir("plugins-legacy");
        let lua = dir.join("bitty-plugins.lua");
        let legacy = dir.join("bitty-plugins.toml");
        write_legacy_state(&legacy, &["legacy.bundled", "alpha.bundled"]);
        assert_eq!(
            plugin_candidates_from(Some(&lua), None),
            vec!["alpha.bundled".to_string(), "legacy.bundled".to_string()]
        );
        assert!(
            lua.exists(),
            "the inspect loader migrates the legacy ledger once"
        );
    }

    #[test]
    fn plugins_malformed_lua_does_not_fall_back_to_legacy() {
        let dir = scratch_dir("plugins-malformed-lua");
        let lua = dir.join("bitty-plugins.lua");
        let legacy = dir.join("bitty-plugins.toml");
        write_legacy_state(&legacy, &["legacy.bundled"]);
        std::fs::write(&lua, "return { state_version = 1, plugins = 1 }\n")
            .expect("malformed lua ledger");
        assert!(
            plugin_candidates_from(Some(&lua), None).is_empty(),
            "a present-but-malformed Lua ledger never falls back"
        );
        assert!(legacy.exists(), "the legacy file stays untouched");
    }

    #[test]
    fn plugins_missing_state_and_store_are_empty() {
        let dir = scratch_dir("plugins-missing");
        assert!(
            plugin_candidates_from(
                Some(&dir.join("absent.toml")),
                Some(&dir.join("absent-store"))
            )
            .is_empty()
        );
        assert!(plugin_candidates_from(None, None).is_empty());
    }

    #[test]
    fn plugins_malformed_state_and_store_fail_soft() {
        let dir = scratch_dir("plugins-malformed");
        let state = dir.join("bitty-plugins.lua");
        std::fs::write(&state, "return { state_version = 2").expect("bad state");
        let store = dir.join("store");
        std::fs::create_dir_all(&store).expect("store dir");
        std::fs::write(store.join("current.json"), "{not json").expect("bad index");
        assert!(plugin_candidates_from(Some(&state), Some(&store)).is_empty());
    }

    #[test]
    fn plugins_oversized_state_fails_soft() {
        let dir = scratch_dir("plugins-oversized");
        let state = dir.join("bitty-plugins.lua");
        std::fs::write(&state, "x".repeat(bitty_plugin_state_ceiling() + 1)).expect("big state");
        assert!(plugin_candidates_from(Some(&state), None).is_empty());
    }

    /// The managed ledger ceiling, restated here so the test does not depend
    /// on private module constants.
    fn bitty_plugin_state_ceiling() -> usize {
        64 * 1024
    }

    #[test]
    fn plain_output_is_one_candidate_per_line() {
        let names = vec![
            "alpha".to_string(),
            "with space".to_string(),
            "日本".to_string(),
        ];
        let mut out = Vec::new();
        assert_eq!(
            write_candidates(&mut out, CandidateKind::Profiles, &names, false),
            EXIT_OK
        );
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "alpha\nwith space\n日本\n"
        );
    }

    #[test]
    fn json_output_is_stable_and_escaped() {
        let names = vec!["quote\"id".to_string(), "back\\slash".to_string()];
        let mut out = Vec::new();
        assert_eq!(
            write_candidates(&mut out, CandidateKind::Plugins, &names, true),
            EXIT_OK
        );
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "{\"v\":1,\"kind\":\"plugins\",\"candidates\":[\"quote\\\"id\",\"back\\\\slash\"]}\n"
        );
    }

    #[test]
    fn json_output_carries_no_host_paths() {
        let names = vec!["alpha".to_string()];
        let mut out = Vec::new();
        let _ = write_candidates(&mut out, CandidateKind::Profiles, &names, true);
        let text = String::from_utf8(out).expect("utf-8");
        assert!(!text.contains(std::env::temp_dir().to_string_lossy().as_ref()));
        assert!(!text.contains('/'));
    }

    #[test]
    fn finalize_drops_control_characters_and_overlong_names() {
        let long = "x".repeat(MAX_CANDIDATE_BYTES + 1);
        let names = finalize(vec![
            "ok".to_string(),
            "bad\nname".to_string(),
            long,
            String::new(),
        ]);
        assert_eq!(names, vec!["ok".to_string()]);
    }
}
