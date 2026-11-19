#![forbid(unsafe_code)]
//! The `disabled` arm of the panel lifecycle mapping, as a **decision record**
//! (bitty#2476 residual 4, ADR-0020 §3 rule 4).
//!
//! The arm is decided, not implemented: `PlaceholderReason::Disabled` has
//! exactly one production producer — the wall-recovery reconcile in
//! `PluginRuntime::recover_wall_failed_plugins` — and every user-facing disable
//! (`bitty plugin disable` in the CLI process, and the in-process
//! `config.set_enabled` writer the application answers) is next-start, so no
//! live generation and no live container is ever in the path. That decision is
//! written on the reason itself (`bitty-ui/src/panel_recovery.rs`) and on the
//! reconcile entry point (`src/plugin_runtime/mod.rs`).
//!
//! A decision record that nothing defends drifts back into folklore, and this
//! crate's placebo risk is specific: a second `disabled` producer is a decision
//! change, not a wiring change, because it would mean a live disable entry
//! exists — the very verb ADR-0020 §3 rule 4 describes and residual 4 did not
//! build. So this file gates the claim instead of restating it.
//!
//! What it pins, and the wrong implementation each check fails:
//!
//! - **One producer.** Exactly one `PlaceholderReason::Disabled` in the
//!   plugin runtime's code, and it is `dispose_inner` called with that reason.
//!   Fails for any implementation that adds a second topology (a live disable
//!   verb, a settle pass, a `present_placeholder` call on the disable edge)
//!   without changing the recorded decision.
//! - **The producer is still the wall-recovery arm.** That call sits inside
//!   `recover_wall_failed_plugins_inner`, which still selects on
//!   `FailureClass::WallSliceTimeout` above the store-index gate
//!   (`record.enabled`). Fails for a producer that fires on a plain crash, on a
//!   disable the user requested, or on any plugin whose index record the pass
//!   never read.
//! - **No live disable verb.** The plugin runtime exposes no `disable_generation`
//!   / `pub fn disable(`. Fails for the arm being "implemented" by adding the
//!   verb the decision says does not exist while this documentation still claims
//!   it does not.
//! - **The decision is still written down.** The reason carries its producer
//!   section, and the reconcile entry point carries its `disabled` section.
//!   Fails for a tree where the prose was deleted and only this test's premise
//!   survived.
//!
//! Every check reads the real files (`CARGO_MANIFEST_DIR`-relative, with a
//! sentinel that proves the scan is looking at the file it thinks it is), the
//! way `tests/panel_placeholder_bounds.rs` gates rule 6. The comment stripper is
//! deliberately coarse: the scanned files hold no `//` inside a string literal,
//! so a marker inside a doc sentence is not counted as code and a snippet quoted
//! in prose cannot satisfy a gate.

use std::path::PathBuf;

/// The plugin runtime module that owns the generation lifecycle verbs.
const RUNTIME: &str = "src/plugin_runtime/mod.rs";
/// The reason vocabulary, where the bounded decision is written.
const VOCABULARY: &str = "../bitty-ui/src/panel_recovery.rs";

/// One scanned source file, read and comment-stripped, with the sentinel that
/// proves the gate read the file it thinks it is.
struct Source {
    /// Path relative to the crate root, for messages.
    label: &'static str,
    /// The file exactly as it is on disk (comments kept).
    raw: String,
    /// The same file with `//` and `/* */` comments dropped.
    code: String,
}

impl Source {
    fn read(label: &'static str, sentinel: &str) -> Self {
        let path: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(label);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        assert!(
            raw.contains(sentinel),
            "{} must be the file this gate thinks it is (missing {sentinel})",
            path.display()
        );
        let code = strip_comments(&raw);
        Self { label, raw, code }
    }

    /// The body of the first item whose header line contains `header`, from that
    /// header to the impl-level closing brace that ends it.
    ///
    /// The two scanned functions are `impl` methods, so their body ends at the
    /// first line that is exactly four spaces and a closing brace. A line
    /// number is all that is returned: the callers assert on the text.
    fn item_body(&self, header: &str) -> (usize, usize) {
        let lines: Vec<&str> = self.code.lines().collect();
        let start = lines
            .iter()
            .position(|line| line.contains(header))
            .unwrap_or_else(|| panic!("{}: no `{header}`", self.label));
        let end = lines[start..]
            .iter()
            .position(|line| *line == "    }")
            .map(|offset| start + offset)
            .unwrap_or_else(|| panic!("{}: `{header}` has no impl-level close", self.label));
        (start, end)
    }
}

/// Drops `//` and `/* */` comments so a doc sentence may name a token the code
/// may not use, and so prose cannot satisfy a code gate.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut depth: usize = 0;
    while let Some(c) = chars.next() {
        if depth > 0 {
            if c == '/' && chars.peek() == Some(&'*') {
                chars.next();
                depth += 1;
            } else if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                depth -= 1;
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            depth = 1;
            continue;
        }
        if c == '/' && chars.peek() == Some(&'/') {
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The plugin runtime holds exactly one `disabled` producer and it is the
/// wall-recovery reconcile's `dispose_inner` call.
///
/// Fails for a second producer anywhere in the lifecycle module: that is a
/// decision change (a live disable entry now exists), so the recorded decision
/// on `PlaceholderReason::Disabled` has to change with it instead of being
/// contradicted by the tree.
#[test]
fn the_disabled_reason_has_one_producer_and_it_is_the_wall_recovery_reconcile() {
    let runtime = Source::read(RUNTIME, "fn recover_wall_failed_plugins_inner");

    let producers: Vec<(usize, &str)> = runtime
        .code
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("PlaceholderReason::Disabled"))
        .collect();
    assert_eq!(
        producers.len(),
        1,
        "{RUNTIME} names `PlaceholderReason::Disabled` in code {} times (lines {:?}); the arm is \
         decided with one producer, the wall-recovery reconcile. A second producer means a live \
         disable path landed: update the decision on the reason (bitty-ui/src/panel_recovery.rs) \
         and this gate with it",
        producers.len(),
        producers
            .iter()
            .map(|(line, _)| line + 1)
            .collect::<Vec<_>>()
    );
    let (producer_line, producer_text) = producers[0];
    assert_eq!(
        producer_text.trim(),
        "let _ = self.dispose_inner(&id, PlaceholderReason::Disabled);",
        "{RUNTIME}:{}: the one `disabled` producer must be the reconcile ending the generation \
         through `dispose_inner` (a release), not a placeholder write that leaves the binding in \
         force",
        producer_line + 1
    );

    // The producer is inside the reconcile, under the index gate and below the
    // class filter: neither the wall-failed class nor the disabled record may
    // move away from it.
    let (start, end) = runtime.item_body("fn recover_wall_failed_plugins_inner");
    assert!(
        (start..end).contains(&producer_line),
        "{RUNTIME}:{}: the `disabled` producer is outside `recover_wall_failed_plugins_inner` \
         ({}-{}), so the recorded decision (\"the wall-recovery reconcile is the sole producer\") \
         no longer describes the code",
        start + 1,
        start + 1,
        end + 1
    );
    let body = runtime
        .code
        .lines()
        .take(end)
        .skip(start)
        .collect::<Vec<_>>()
        .join("\n");
    for required in [
        "entry.failure_class == FailureClass::WallSliceTimeout",
        "if !record.enabled {",
    ] {
        assert!(
            body.contains(required),
            "`recover_wall_failed_plugins_inner` no longer contains {required:?}: the reason is \
             documented as the wall-failed-and-disabled arm (ADR-0020 §3 rule 4), so a candidate \
             set that moved off the wall class or off the index gate makes that documentation \
             false"
        );
    }
}

/// The plugin runtime exposes no live disable verb, which is why the arm is a
/// decision and not a mechanism.
///
/// Fails for the tree growing the verb the documentation says does not exist:
/// with a live disable entry, a user disable *can* present `disabled`, and the
/// bounded decision on the reason has to be rewritten rather than quietly
/// invalidated.
#[test]
fn the_plugin_runtime_has_no_live_disable_verb() {
    let runtime = Source::read(RUNTIME, "pub fn recover_wall_failed_plugins");
    for absent in ["fn disable_generation", "fn disable(", "fn set_disabled"] {
        assert!(
            !runtime.code.contains(absent),
            "{RUNTIME} declares `{absent}`: a live disable verb exists, so the recorded decision \
             on `PlaceholderReason::Disabled` (\"the wall-recovery reconcile is the sole producer; \
             every user disable is next-start\") is no longer true and has to be rewritten"
        );
    }
}

/// The decision is written where a reader will find it: on the reason, and on
/// the lifecycle entry point that is its only producer.
///
/// Fails for a tree that keeps the single-producer code and deletes the prose
/// that says why, which is the state this residual started from.
#[test]
fn the_bounded_decision_is_recorded_on_the_reason_and_on_the_reconcile() {
    let vocabulary = Source::read(VOCABULARY, "pub enum PlaceholderReason");
    for section in [
        "# Producer: a bounded decision (bitty#2476 residual 4)",
        "# Why a user disable does not present it, and why no call was added",
    ] {
        assert!(
            vocabulary.raw.contains(section),
            "{VOCABULARY}: the `PlaceholderReason::Disabled` documentation lost its {section:?} \
             section; a reader who meets a `disabled` container has to be able to tell a faulted- \
             then-disabled plugin from one the user just disabled"
        );
    }

    let runtime = Source::read(RUNTIME, "pub fn recover_wall_failed_plugins");
    let (start, end) = runtime.item_body("    pub fn recover_wall_failed_plugins(&mut self)");
    let lines: Vec<&str> = runtime.raw.lines().collect();
    let docs = lines[..start]
        .iter()
        .rev()
        .take_while(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("///") || trimmed.starts_with("#[")
        })
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        docs.contains("# The `disabled` arm (bitty#2476 residual 4)"),
        "{RUNTIME}: the `recover_wall_failed_plugins` documentation lost its `disabled` arm \
         section at lines {}-{}; the entry point is where a reader looks for what the pass does \
         to a disabled plugin",
        start + 1,
        end + 1
    );
}
