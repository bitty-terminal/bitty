// Tests for the restricted configuration module loader (`CTX-1683`, bitty#2869).
//
// Each test maps to a contract rule; a test that would still pass with the
// rule removed is written as a negative control instead (the final- and
// intermediate-component swaps are the two the contract names).

use super::*;
use crate::ConfigOutcome;
use crate::config::ConfigEval;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

/// The test-only swap action: one closure over the canonical module path.
type SwapHook = Box<dyn Fn(&Path)>;

thread_local! {
    static SWAP_HOOK: RefCell<Option<SwapHook>> = const { RefCell::new(None) };
}

/// Called between canonicalization and the anchored open (test builds only).
pub(crate) fn test_swap_hook(path: &Path) {
    SWAP_HOOK.with(|hook| {
        if let Some(swap) = hook.borrow().as_ref() {
            swap(path);
        }
    });
}

fn set_swap_hook(hook: Option<SwapHook>) {
    SWAP_HOOK.with(|cell| *cell.borrow_mut() = hook);
}

thread_local! {
    static FSTAT_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the test-only `fstat` fault is armed on this thread.
pub(crate) fn fstat_fault_armed() -> bool {
    FSTAT_FAULT.with(std::cell::Cell::get)
}

/// Arm or disarm the test-only `fstat` fault for the current thread.
fn arm_fstat_fault(armed: bool) {
    FSTAT_FAULT.with(|flag| flag.set(armed));
}

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bitty-require-{tag}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// Create `<root>/<relative>` (creating parents) with `body`.
    fn module(&self, relative: &str, body: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("module parent");
        }
        std::fs::write(&path, body).expect("write module");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The module root is `<config-dir>/lua`; every test writes its modules below
/// `lua/` under the scratch root and hands that directory in.
fn module_root(root: &Path) -> PathBuf {
    root.join(MODULE_DIR_NAME)
}

fn eval_with_root(root: &Path, code: &str) -> (LuaVm, ConfigOutcome) {
    let mut vm = LuaVm::with_config_budgets(
        "test.config",
        crate::RC1_INSTRUCTION_BUDGET,
        crate::RC1_WALL_CLOCK_BUDGET_MS,
        crate::RC1_WARNING_MS,
        crate::RC2_MEMORY_PER_PLUGIN_BYTES,
    );
    vm.install_config_require(module_root(root), "init.lua")
        .expect("install loader");
    let outcome = vm.eval_config(code).expect("no pre-existing refusal");
    (vm, outcome)
}

/// Valid Lua source of exactly `total` bytes: a `return {}` plus comment
/// padding, so a size-boundary test can land on the ceiling itself.
fn padded_module_exact(total: usize) -> String {
    let mut body = String::from("return {}\n");
    while total - body.len() > 120 {
        body.push_str(&format!("-- {}\n", "x".repeat(100)));
    }
    let rest = total - body.len();
    assert!(rest >= 4, "target too small for the padding line");
    body.push_str(&format!("-- {}\n", "x".repeat(rest - 4)));
    assert_eq!(body.len(), total);
    body
}

fn refusal(root: &Path, code: &str) -> ConfigModuleDiagnostic {
    let (_vm, outcome) = eval_with_root(root, code);
    match outcome {
        ConfigOutcome::ModuleRefused { diagnostic } => *diagnostic,
        other => panic!("expected a module refusal, got {other:?}"),
    }
}

fn assert_code(diagnostic: &ConfigModuleDiagnostic, code: ConfigModuleCode) {
    assert_eq!(diagnostic.code(), code, "expected {code}, got {diagnostic}");
}

// ── name grammar ─────────────────────────────────────────────────────────────

#[test]
fn grammar_accepts_the_admitted_forms() {
    for spec in ["a", "config.options", "a-b_C9.x1", "a.b.c.d.e.f.g.h"] {
        let parsed = ModuleSpec::parse(spec.as_bytes()).expect(spec);
        assert_eq!(parsed.as_str(), spec);
    }
}

#[test]
fn grammar_rejects_every_refused_form() {
    let too_many_segments = (0..17)
        .map(|index| format!("s{index}"))
        .collect::<Vec<_>>()
        .join(".");
    let long_segment = "a".repeat(65);
    let long_spec = ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; 5].join(".");
    let cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        b".".to_vec(),
        b"a.".to_vec(),
        b"a..b".to_vec(),
        b"..".to_vec(),
        b"../etc/passwd".to_vec(),
        b"a/b".to_vec(),
        b"a\\\\b".to_vec(),
        b"C:windows".to_vec(),
        b"a\0b".to_vec(),
        b"caf\xc3\xa9".to_vec(),
        b"bitty".to_vec(),
        b"bitty.foo".to_vec(),
        b"BITTY.foo".to_vec(),
        b"-leading".to_vec(),
        b"a.b c".to_vec(),
        too_many_segments.into_bytes(),
        long_segment.into_bytes(),
        long_spec.into_bytes(),
    ];
    for raw in cases {
        let error = ModuleSpec::parse(&raw).expect_err(&format!("{raw:?} must be refused"));
        assert_code(&error, ConfigModuleCode::Name);
        // The refused spec is echoed, escaped, and bounded.
        assert!(error.render().contains("module_spec="));
    }
}

#[test]
fn non_string_argument_is_a_name_refusal() {
    let dir = TempDir::new("non-string");
    let diagnostic = refusal(dir.path(), "return require(42)");
    assert_code(&diagnostic, ConfigModuleCode::Name);
}

// ── resolution order ─────────────────────────────────────────────────────────

#[test]
fn flat_and_init_resolutions_load() {
    let dir = TempDir::new("resolve");
    dir.module("lua/opts.lua", "return { value = 'dark' }");
    dir.module("lua/pack/init.lua", "return { value = 'light' }");
    let (_vm, outcome) = eval_with_root(dir.path(), "return { theme = require('opts').value }");
    match outcome {
        ConfigOutcome::Completed { data, .. } => {
            assert!(data.undeclared.is_empty(), "{data:?}");
            assert_eq!(data.theme.as_deref(), Some("dark"));
        }
        other => panic!("expected completion, got {other:?}"),
    }
}

#[test]
fn both_resolutions_refuse_as_ambiguous() {
    let dir = TempDir::new("ambiguous");
    dir.module("lua/dup.lua", "return {}");
    dir.module("lua/dup/init.lua", "return {}");
    let diagnostic = refusal(dir.path(), "return require('dup')");
    assert_code(&diagnostic, ConfigModuleCode::Ambiguous);
}

#[test]
fn missing_module_refuses_as_not_found_without_a_path() {
    let dir = TempDir::new("notfound");
    std::fs::create_dir_all(dir.path().join("lua")).expect("root");
    let diagnostic = refusal(dir.path(), "return require('nope.missing')");
    assert_code(&diagnostic, ConfigModuleCode::NotFound);
    let rendered = diagnostic.render();
    assert!(rendered.contains("module_spec=\"nope.missing\""));
    // No absolute path, no canonical root, no identity tuple.
    assert!(!rendered.contains(dir.path().to_string_lossy().as_ref()));
    assert!(!rendered.contains("dev="));
}

// ── cache, duplicates, cycles ────────────────────────────────────────────────

#[test]
fn duplicate_require_returns_the_same_table() {
    let dir = TempDir::new("duplicate");
    dir.module("lua/shared.lua", "return { n = 1 }");
    let (vm, outcome) = eval_with_root(
        dir.path(),
        "local a = require('shared')\nlocal b = require('shared')\n\
         return { theme = tostring(a == b), close_confirm = tostring(b.n) }",
    );
    match outcome {
        ConfigOutcome::Completed { data, .. } => {
            assert!(data.undeclared.is_empty(), "{data:?}");
            assert_eq!(
                data.theme.as_deref(),
                Some("true"),
                "the second require returns the first table"
            );
            assert_eq!(
                data.close_confirm.as_deref(),
                Some("1"),
                "the chunk ran once"
            );
        }
        other => panic!("expected completion, got {other:?}"),
    }
    let stats = vm.config_module_stats().expect("stats");
    assert_eq!(stats.modules, 1, "one identity, one evaluation");
}

// Windows keys the cache by the case-folded canonical path, so a hardlink
// alias is two identities there — the documented deviation, not this rule.
#[cfg(unix)]
#[test]
fn hardlink_alias_collapses_to_one_evaluation() {
    let dir = TempDir::new("hardlink");
    let first = dir.module(
        "lua/first.lua",
        "counter = (counter or 0) + 1\nreturn { n = counter }",
    );
    std::fs::hard_link(&first, dir.path().join("lua/second.lua")).expect("hardlink");
    let (vm, outcome) = eval_with_root(
        dir.path(),
        "local a = require('first')\nlocal b = require('second')\n\
         return { theme = tostring(a == b), close_confirm = tostring(b.n) }",
    );
    match outcome {
        ConfigOutcome::Completed { data, .. } => {
            assert!(data.undeclared.is_empty(), "{data:?}");
            assert_eq!(
                data.theme.as_deref(),
                Some("true"),
                "one file identity is one cached table"
            );
            assert_eq!(
                data.close_confirm.as_deref(),
                Some("1"),
                "one file identity is one evaluation"
            );
        }
        other => panic!("expected completion, got {other:?}"),
    }
    let stats = vm.config_module_stats().expect("stats");
    assert_eq!(stats.modules, 1, "one file identity, one slot");
}

#[test]
fn self_require_is_a_cycle() {
    let dir = TempDir::new("cycle-self");
    dir.module("lua/self.lua", "return require('self')");
    let diagnostic = refusal(dir.path(), "return require('self')");
    assert_code(&diagnostic, ConfigModuleCode::Cycle);
    assert!(diagnostic.render().contains("self"));
}

#[test]
fn two_module_ring_is_a_cycle_naming_the_chain() {
    let dir = TempDir::new("cycle-ring");
    dir.module("lua/a.lua", "return require('b')");
    dir.module("lua/b.lua", "return require('a')");
    let diagnostic = refusal(dir.path(), "return require('a')");
    assert_code(&diagnostic, ConfigModuleCode::Cycle);
    let rendered = diagnostic.render();
    assert!(rendered.contains("a -> b -> a"), "{rendered}");
}

#[test]
fn longer_ring_under_the_depth_cap_is_the_same_code() {
    let dir = TempDir::new("cycle-long");
    dir.module("lua/m1.lua", "return require('m2')");
    dir.module("lua/m2.lua", "return require('m3')");
    dir.module("lua/m3.lua", "return require('m1')");
    let diagnostic = refusal(dir.path(), "return require('m1')");
    assert_code(&diagnostic, ConfigModuleCode::Cycle);
}

#[test]
fn require_never_returns_a_partial_table() {
    let dir = TempDir::new("partial");
    dir.module("lua/p.lua", "return require('p')");
    let (_vm, outcome) = eval_with_root(
        dir.path(),
        "local ok, value = pcall(require, 'p')\nreturn { ok = ok, value = tostring(value) }",
    );
    // `ok` is false: the cycle is a refusal, never a sentinel value.
    match outcome {
        ConfigOutcome::ModuleRefused { diagnostic } => {
            assert_code(&diagnostic, ConfigModuleCode::Cycle);
        }
        other => panic!("a caught cycle must still refuse the generation: {other:?}"),
    }
}

#[test]
fn depth_beyond_sixteen_refuses_before_the_seventeenth() {
    let dir = TempDir::new("depth");
    for index in 1..=17 {
        let body = if index == 17 {
            "return { leaf = true }".to_string()
        } else {
            format!("return require('m{}')", index + 1)
        };
        dir.module(&format!("lua/m{index}.lua"), &body);
    }
    let diagnostic = refusal(dir.path(), "return require('m1')");
    assert_code(&diagnostic, ConfigModuleCode::Depth);
    assert_eq!(
        diagnostic.aggregate,
        Some(ConfigModuleAggregate::Depth),
        "the depth counter is named"
    );
}

#[test]
fn depth_of_sixteen_is_admitted() {
    let dir = TempDir::new("depth-ok");
    for index in 1..=16 {
        let body = if index == 16 {
            "return { leaf = true }".to_string()
        } else {
            format!("return require('m{}')", index + 1)
        };
        dir.module(&format!("lua/m{index}.lua"), &body);
    }
    let (_vm, outcome) = eval_with_root(dir.path(), "return require('m1')");
    assert!(
        matches!(outcome, ConfigOutcome::Completed { .. }),
        "{outcome:?}"
    );
}

#[test]
fn count_beyond_sixty_four_identities_refuses() {
    let dir = TempDir::new("count");
    let mut body = String::new();
    for index in 1..=65 {
        dir.module(&format!("lua/c{index}.lua"), "return {}");
        body.push_str(&format!("require('c{index}')\n"));
    }
    body.push_str("return {}\n");
    let diagnostic = refusal(dir.path(), &body);
    assert_code(&diagnostic, ConfigModuleCode::Count);
    assert_eq!(
        diagnostic.aggregate,
        Some(ConfigModuleAggregate::Count),
        "the count counter is named"
    );
}

// ── per-file and aggregate bounds ────────────────────────────────────────────

#[test]
fn file_over_the_byte_ceiling_refuses_as_size() {
    let dir = TempDir::new("size-bytes");
    let body = format!("-- {}\nreturn {{}}", "x".repeat(MAX_MODULE_FILE_BYTES));
    dir.module("lua/big.lua", &body);
    let diagnostic = refusal(dir.path(), "return require('big')");
    assert_code(&diagnostic, ConfigModuleCode::Size);
}

#[test]
fn file_at_exactly_the_byte_ceiling_loads() {
    let dir = TempDir::new("size-exact");
    // The file lands on exactly the ceiling: the check must not reject the cap
    // itself, and the padding rides on short comment lines so the per-line
    // bound is not what refuses.
    let source = padded_module_exact(MAX_MODULE_FILE_BYTES);
    dir.module("lua/exact.lua", &source);
    let (_vm, outcome) = eval_with_root(dir.path(), "return require('exact')");
    assert!(
        matches!(outcome, ConfigOutcome::Completed { .. }),
        "{outcome:?}"
    );
}

#[test]
fn file_over_the_line_ceiling_refuses_as_size() {
    let dir = TempDir::new("size-lines");
    let mut source = String::from("return {}\n");
    for index in 0..MAX_MODULE_FILE_LINES {
        source.push_str(&format!("-- line {index}\n"));
    }
    dir.module("lua/lines.lua", &source);
    let diagnostic = refusal(dir.path(), "return require('lines')");
    assert_code(&diagnostic, ConfigModuleCode::Size);
}

#[test]
fn line_over_the_length_ceiling_refuses_as_size() {
    let dir = TempDir::new("size-line-length");
    let source = format!("-- {}\nreturn {{}}", "x".repeat(MAX_MODULE_LINE_BYTES));
    dir.module("lua/wide.lua", &source);
    let diagnostic = refusal(dir.path(), "return require('wide')");
    assert_code(&diagnostic, ConfigModuleCode::Size);
}

#[test]
fn aggregate_over_256_kib_refuses_as_aggregate() {
    let dir = TempDir::new("aggregate");
    // 60 KB per module: under the per-file byte cap (64 KiB) and the per-line
    // cap, over the 256 KiB aggregate at the fifth module.
    let chunk = padded_module_exact(60 * 1000);
    let mut body = String::new();
    for index in 1..=5 {
        dir.module(&format!("lua/a{index}.lua"), &chunk);
        body.push_str(&format!("require('a{index}')\n"));
    }
    body.push_str("return {}\n");
    let diagnostic = refusal(dir.path(), &body);
    assert_code(&diagnostic, ConfigModuleCode::Aggregate);
    assert_eq!(
        diagnostic.aggregate,
        Some(ConfigModuleAggregate::Bytes),
        "the byte counter is named"
    );
}

#[test]
fn stats_report_modules_and_module_bytes() {
    let dir = TempDir::new("stats");
    dir.module("lua/one.lua", "return {}");
    dir.module("lua/two.lua", "return {}");
    let (vm, outcome) = eval_with_root(dir.path(), "require('one')\nrequire('two')\nreturn {}");
    assert!(matches!(outcome, ConfigOutcome::Completed { .. }));
    let stats = vm.config_module_stats().expect("stats");
    assert_eq!(stats.modules, 2);
    assert!(stats.module_bytes > 0);
}

// ── containment ──────────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn symlink_out_of_the_root_refuses_as_escape() {
    let dir = TempDir::new("escape-symlink");
    let outside = dir.path().join("outside.lua");
    std::fs::write(&outside, "return { stolen = true }").expect("outside module");
    std::fs::create_dir_all(dir.path().join("lua")).expect("root");
    std::os::unix::fs::symlink(&outside, dir.path().join("lua/leak.lua")).expect("symlink");
    let diagnostic = refusal(dir.path(), "return require('leak')");
    assert_code(&diagnostic, ConfigModuleCode::Escape);
}

#[cfg(unix)]
#[test]
fn symlink_loop_at_a_module_path_refuses_as_escape() {
    // A self-link resolves to `ELOOP`, which `Path::exists` swallowed as "not
    // found": the operator was told a file that exists is missing. It is a
    // resolution failure, and the contract names it as a containment refusal.
    let dir = TempDir::new("escape-loop");
    std::fs::create_dir_all(dir.path().join("lua")).expect("root");
    let path = dir.path().join("lua/loop.lua");
    std::os::unix::fs::symlink(&path, &path).expect("self symlink");
    let diagnostic = refusal(dir.path(), "return require('loop')");
    assert_code(&diagnostic, ConfigModuleCode::Escape);
    let rendered = diagnostic.render();
    assert!(rendered.contains("link resolution loop"), "{rendered}");
}

/// The loop class is named by the host's own errno, so the portable constant
/// must equal what this platform reports for a self-symlink. A Linux-only
/// literal compiled everywhere and left macOS answering the generic
/// "could not be canonicalized" detail (the macOS leg of bitty#2869 caught it).
#[cfg(unix)]
#[test]
fn the_link_loop_class_matches_the_host_errno() {
    let dir = TempDir::new("loop-errno");
    let path = dir.path().join("self.lua");
    std::os::unix::fs::symlink(&path, &path).expect("self symlink");
    let error = std::fs::metadata(&path).expect_err("a self symlink cannot resolve");
    assert_eq!(
        error.raw_os_error(),
        Some(errno::LOOP),
        "the portable ELOOP constant must match the host errno"
    );
}

#[cfg(unix)]
#[test]
fn unreadable_module_directory_refuses_as_escape() {
    // `EACCES` on a component is a refusal, not a missing module. Skipped when
    // the host ignores mode 000 for this user (root), where the path is
    // readable and there is nothing to prove.
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("escape-eacces");
    dir.module("lua/locked/m.lua", "return { value = 'hidden' }");
    let locked = dir.path().join("lua/locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
        .expect("lock the directory");
    let readable = std::fs::metadata(locked.join("m.lua")).is_ok();
    let diagnostic = (!readable).then(|| refusal(dir.path(), "return require('locked.m')"));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))
        .expect("unlock the directory");
    let Some(diagnostic) = diagnostic else {
        eprintln!("skipping: mode 000 is not enforced for this user");
        return;
    };
    assert_code(&diagnostic, ConfigModuleCode::Escape);
    let rendered = diagnostic.render();
    assert!(rendered.contains("component is unreadable"), "{rendered}");
}

#[cfg(unix)]
#[test]
fn in_root_symlinked_layout_loads_through_its_canonical_target() {
    let dir = TempDir::new("escape-inroot");
    // A dotfiles-style layout inside the root: `lua/linked` is a link whose
    // canonical target (`lua/real`) is still under the canonical root, which
    // step 2 admits and step 3 then walks as its symlink-free canonical form.
    dir.module("lua/real/opts.lua", "return { value = 'dark' }");
    std::os::unix::fs::symlink(dir.path().join("lua/real"), dir.path().join("lua/linked"))
        .expect("symlink");
    let (_vm, outcome) = eval_with_root(
        dir.path(),
        "return { theme = require('linked.opts').value }",
    );
    assert!(
        matches!(outcome, ConfigOutcome::Completed { .. }),
        "{outcome:?}"
    );
}

#[cfg(unix)]
#[test]
fn final_component_swapped_after_canonicalization_refuses_as_escape() {
    let dir = TempDir::new("swap-final");
    dir.module("lua/target.lua", "return { value = 'inside' }");
    let outside = dir.path().join("outside.lua");
    std::fs::write(&outside, "return { value = 'outside' }").expect("outside module");
    let canonical = dir
        .path()
        .join("lua/target.lua")
        .canonicalize()
        .expect("canonical target");
    let swap_target = outside.clone();
    set_swap_hook(Some(Box::new(move |path: &Path| {
        if path == canonical {
            let _ = std::fs::remove_file(path);
            let _ = std::os::unix::fs::symlink(&swap_target, path);
        }
    })));
    let diagnostic = refusal(dir.path(), "return require('target')");
    set_swap_hook(None);
    assert_code(&diagnostic, ConfigModuleCode::Escape);
}

#[cfg(unix)]
#[test]
fn intermediate_component_swapped_after_canonicalization_refuses_as_escape() {
    // The contract's declared control: a loader that opened the canonical
    // path by name (then compared identities) passes the final-component test
    // and fails this one, so this asserts the walk rather than restating it.
    let dir = TempDir::new("swap-intermediate");
    dir.module("lua/pkg/mod.lua", "return { value = 'inside' }");
    let outside = TempDir::new("swap-intermediate-outside");
    std::fs::write(
        outside.path().join("mod.lua"),
        "return { value = 'outside' }",
    )
    .expect("outside module");
    let package_dir = dir.path().join("lua/pkg");
    let swap_target = outside.path().to_path_buf();
    set_swap_hook(Some(Box::new(move |path: &Path| {
        if path.ends_with("pkg/mod.lua") {
            let _ = std::fs::remove_dir_all(&package_dir);
            let _ = std::os::unix::fs::symlink(&swap_target, &package_dir);
        }
    })));
    let diagnostic = refusal(dir.path(), "return require('pkg.mod')");
    set_swap_hook(None);
    assert_code(&diagnostic, ConfigModuleCode::Escape);
}

#[cfg(target_os = "linux")]
#[test]
fn absolute_and_parent_remainders_are_refused_beneath_the_root() {
    // The branch complement the contract asks for: `RESOLVE_BENEATH` measures
    // containment from the descriptor, so `EXDEV` is the answer for both.
    let dir = TempDir::new("beneath");
    std::fs::create_dir_all(dir.path().join("lua/a")).expect("root");
    let root_fd = unix_open_root(&dir.path().join("lua")).expect("open root");
    for remainder in ["/etc/hostname", "../outside.lua", "a/../../outside.lua"] {
        let error = unix_open_beneath(&root_fd, remainder).expect_err(remainder);
        assert_eq!(
            error.raw_os_error(),
            Some(18),
            "{remainder} must refuse with EXDEV, got {error:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlinked_root_final_component_refuses_as_root() {
    let dir = TempDir::new("root-symlink");
    dir.module("real/keep.txt", "");
    std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("lua")).expect("symlink");
    let diagnostic = refusal(dir.path(), "return require('anything')");
    assert_code(&diagnostic, ConfigModuleCode::Root);
}

#[test]
fn missing_root_refuses_as_root_and_never_as_not_found() {
    let dir = TempDir::new("root-missing");
    let diagnostic = refusal(dir.path(), "return require('anything')");
    assert_code(&diagnostic, ConfigModuleCode::Root);
    // N-11: the record names the layout kind, never the canonical root.
    let rendered = diagnostic.render();
    assert!(rendered.contains("phase=root"));
    assert!(!rendered.contains(dir.path().to_string_lossy().as_ref()));
    assert!(rendered.contains("source=\"init.lua\""));
    assert!(!rendered.contains("module_spec="), "{rendered}");
}

#[test]
fn root_refusal_is_reported_once_per_generation() {
    let dir = TempDir::new("root-once");
    let (vm, outcome) = eval_with_root(dir.path(), "require('one')\nrequire('two')\nreturn {}");
    match outcome {
        ConfigOutcome::ModuleRefused { diagnostic } => {
            assert_code(&diagnostic, ConfigModuleCode::Root);
        }
        other => panic!("expected a root refusal, got {other:?}"),
    }
    // The generation is dead after the first refusal: the second call did no
    // work and did not read anything.
    assert_eq!(vm.config_module_stats().expect("stats").modules, 0);
}

// ── special files and the read boundary ──────────────────────────────────────

#[cfg(target_os = "linux")]
#[test]
fn fifo_at_a_module_path_refuses_without_waiting() {
    use rustix::fs::{FileType, Mode};
    let dir = TempDir::new("fifo");
    std::fs::create_dir_all(dir.path().join("lua")).expect("root");
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        dir.path().join("lua/pipe.lua"),
        Mode::from_bits_truncate(0o600),
    )
    .expect("mkfifo");
    // A blocking open would wait for a writer; the non-blocking open plus the
    // type check is the guarantee this test pins.
    let diagnostic = refusal(dir.path(), "return require('pipe')");
    assert_code(&diagnostic, ConfigModuleCode::NotRegular);
    assert_eq!(FileType::Fifo, FileType::Fifo);
}

#[cfg(unix)]
#[test]
fn socket_at_a_module_path_refuses_as_not_regular() {
    let dir = TempDir::new("socket");
    std::fs::create_dir_all(dir.path().join("lua")).expect("root");
    let _listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("lua/sock.lua")).expect("socket");
    let diagnostic = refusal(dir.path(), "return require('sock')");
    assert_code(&diagnostic, ConfigModuleCode::NotRegular);
}

#[test]
fn directory_at_the_init_resolution_refuses_as_not_regular() {
    let dir = TempDir::new("dir-init");
    std::fs::create_dir_all(dir.path().join("lua/pkg/init.lua/sub")).expect("directory");
    let diagnostic = refusal(dir.path(), "return require('pkg')");
    assert_code(&diagnostic, ConfigModuleCode::NotRegular);
}

#[test]
fn a_short_read_refuses_and_yields_no_prefix() {
    let dir = TempDir::new("read-short");
    let path = dir.module("lua/short.lua", "return { value = 1 }\n");
    let file = std::fs::File::open(&path).expect("open");
    // Expected length deliberately larger than the handle can supply: this is
    // the "file shrank between fstat and read" case, driven without a race.
    let error = read_bounded(file, 4096, MAX_MODULE_FILE_BYTES).expect_err("short read");
    assert_eq!(error.0, ConfigModuleCode::Read);
}

#[test]
fn a_read_past_the_ceiling_refuses_instead_of_truncating() {
    let dir = TempDir::new("read-over");
    let body = format!("-- {}\nreturn {{}}", "x".repeat(MAX_MODULE_FILE_BYTES));
    let path = dir.module("lua/over.lua", &body);
    let file = std::fs::File::open(&path).expect("open");
    let error = read_bounded(file, 64, MAX_MODULE_FILE_BYTES).expect_err("over cap");
    assert_eq!(error.0, ConfigModuleCode::Read);
}

#[test]
fn a_failed_fstat_refuses_as_read() {
    // No handle this suite can build fails `File::metadata`, so the loader's
    // own `fstat` branch is driven through the fault seam: this test fails if
    // that branch's code changes (for example Read -> Syntax), which is what
    // makes it evidence for the N-10 rule rather than a restatement of it.
    let dir = TempDir::new("fstat");
    dir.module("lua/ok.lua", "return {}");
    arm_fstat_fault(true);
    let (_vm, outcome) = eval_with_root(dir.path(), "return require('ok')");
    arm_fstat_fault(false);
    match outcome {
        ConfigOutcome::ModuleRefused { diagnostic } => {
            assert_code(&diagnostic, ConfigModuleCode::Read);
            let rendered = diagnostic.render();
            assert!(rendered.contains("fstat"), "{rendered}");
        }
        other => panic!("expected a read refusal, got {other:?}"),
    }
}

// ── parse, evaluation, shape ─────────────────────────────────────────────────

#[test]
fn binary_chunk_header_is_a_syntax_refusal() {
    let dir = TempDir::new("binary");
    let path = dir.path().join("lua/byte.lua");
    std::fs::create_dir_all(dir.path().join("lua")).expect("root");
    std::fs::write(&path, b"\x1bLua\x53\x00\x19\x93\r\n\x1a\n").expect("write chunk");
    let diagnostic = refusal(dir.path(), "return require('byte')");
    assert_code(&diagnostic, ConfigModuleCode::Syntax);
    assert!(diagnostic.render().contains("binary chunk"));
}

#[test]
fn syntax_error_carries_the_parser_detail_and_line() {
    let dir = TempDir::new("syntax");
    dir.module("lua/broken.lua", "return {\n");
    let diagnostic = refusal(dir.path(), "return require('broken')");
    assert_code(&diagnostic, ConfigModuleCode::Syntax);
    let rendered = diagnostic.render();
    assert!(rendered.contains("detail=\""), "{rendered}");
    assert!(rendered.contains("source=\"broken"), "{rendered}");
}

#[test]
fn runtime_error_in_a_module_is_an_eval_refusal() {
    let dir = TempDir::new("eval");
    dir.module("lua/boom.lua", "error('module exploded')");
    let (_vm, outcome) = eval_with_root(dir.path(), "return require('boom')");
    let diagnostic = match outcome {
        ConfigOutcome::ModuleRefused { diagnostic } => *diagnostic,
        other => panic!("expected a module refusal, got {other:?}"),
    };
    assert_code(&diagnostic, ConfigModuleCode::Eval);
    assert!(
        diagnostic.render().contains("module_spec=\"boom\""),
        "the record names the module that raised: {diagnostic}"
    );
    assert!(
        diagnostic.render().contains("module exploded"),
        "the module-authored message must reach `detail`: {diagnostic}"
    );
}

#[test]
fn module_must_return_exactly_one_table() {
    let dir = TempDir::new("shape");
    for (name, body) in [
        ("none", ""),
        ("scalar", "return 42"),
        ("two", "return {}, {}"),
    ] {
        dir.module(&format!("lua/{name}.lua"), body);
        let diagnostic = refusal(dir.path(), &format!("return require('{name}')"));
        assert_code(&diagnostic, ConfigModuleCode::Eval);
    }
}

#[test]
fn pcall_cannot_swallow_a_loader_refusal() {
    let dir = TempDir::new("pcall");
    std::fs::create_dir_all(dir.path().join("lua")).expect("root");
    let (_vm, outcome) = eval_with_root(
        dir.path(),
        "local ok = pcall(require, 'missing')\nreturn { ok = ok }",
    );
    match outcome {
        ConfigOutcome::ModuleRefused { diagnostic } => {
            assert_code(&diagnostic, ConfigModuleCode::NotFound);
        }
        other => panic!("a caught refusal must still end the generation: {other:?}"),
    }
}

#[test]
fn entry_chunk_budget_is_not_re_coded_as_a_module_budget() {
    let dir = TempDir::new("entry-budget");
    let (_vm, outcome) = eval_with_root(dir.path(), "while true do end");
    assert!(
        matches!(outcome, ConfigOutcome::Suspended { .. }),
        "the entry chunk keeps its shipped budget meaning: {outcome:?}"
    );
}

#[test]
fn entry_budget_after_a_module_load_keeps_its_shipped_meaning() {
    // The budget tripped in the entry chunk, after the module had already
    // returned: a module having been loaded is not evidence that the module
    // was evaluating when the budget ran out.
    let dir = TempDir::new("entry-budget-after-module");
    dir.module("lua/ok.lua", "return { ready = true }");
    let (_vm, outcome) = eval_with_root(dir.path(), "local m = require('ok')\nwhile true do end");
    assert!(
        matches!(outcome, ConfigOutcome::Suspended { .. }),
        "an entry-chunk budget keeps its meaning after a module load: {outcome:?}"
    );
}

#[test]
fn a_module_raise_names_the_module_in_its_attribution() {
    let dir = TempDir::new("raise-nested");
    dir.module("lua/outer.lua", "return require('inner')");
    dir.module("lua/inner.lua", "error('inner exploded')");
    // The error passes through `outer` on its way out: the record names the
    // innermost chunk that raised, not the one it unwound through.
    let diagnostic = refusal(dir.path(), "return require('outer')");
    assert_code(&diagnostic, ConfigModuleCode::Eval);
    let rendered = diagnostic.render();
    assert!(rendered.contains("module_spec=\"inner\""), "{rendered}");
    assert!(rendered.contains("inner exploded"), "{rendered}");
}

#[test]
fn a_caught_module_raise_does_not_spend_a_depth_slot() {
    // A raised module chunk still pops its in-flight frame, so the frame it
    // would have left behind cannot turn the sixteenth legal nest into a
    // depth refusal.
    let dir = TempDir::new("raise-depth");
    dir.module("lua/boom.lua", "error('module exploded')");
    for index in 1..=16 {
        let body = if index == 16 {
            "return { leaf = true }".to_string()
        } else {
            format!("return require('m{}')", index + 1)
        };
        dir.module(&format!("lua/m{index}.lua"), &body);
    }
    let (vm, outcome) = eval_with_root(
        dir.path(),
        "local ok = pcall(require, 'boom')\nreturn require('m1')",
    );
    assert!(
        matches!(outcome, ConfigOutcome::Completed { .. }),
        "a caught raise must not occupy a depth slot: {outcome:?}"
    );
    assert!(
        vm.config_module_in_flight().is_none(),
        "the frame is popped on the raise path"
    );
}

#[test]
fn an_entry_error_after_a_caught_module_raise_keeps_its_own_attribution() {
    let dir = TempDir::new("raise-attribution");
    dir.module("lua/boom.lua", "error('module exploded')");
    let (_vm, outcome) = eval_with_root(
        dir.path(),
        "local ok = pcall(require, 'boom')\nerror('entry failure')",
    );
    match outcome {
        ConfigOutcome::LuaError { message } => {
            assert!(message.contains("entry failure"), "{message}");
        }
        other => panic!("an entry failure keeps its own attribution: {other:?}"),
    }
}

#[test]
fn module_budget_is_reported_as_a_module_budget() {
    let dir = TempDir::new("module-budget");
    dir.module("lua/spin.lua", "while true do end");
    let diagnostic = refusal(dir.path(), "return require('spin')");
    assert_code(&diagnostic, ConfigModuleCode::Budget);
    assert_eq!(diagnostic.aggregate, Some(ConfigModuleAggregate::Budget));
}

// ── lexical re-check and the Windows forms ──────────────────────────────────

#[test]
fn the_lexical_check_refuses_every_refused_join() {
    // The grammar already makes these unrepresentable; the check runs anyway,
    // so a future grammar edit cannot silently reintroduce a traversal path.
    for relative in [
        "",
        "/etc/passwd",
        "\\windows\\system32",
        "a/../b.lua",
        "a/./b.lua",
        "a//b.lua",
        "a/b:c.lua",
        "a/b\0.lua",
        "C:module.lua",
    ] {
        assert!(
            lexical_containment(relative).is_err(),
            "{relative:?} must be refused"
        );
    }
    for relative in ["a/b/c.lua", "a-b_c9/init.lua", "top.lua"] {
        assert!(
            lexical_containment(relative).is_ok(),
            "{relative:?} is a normal join"
        );
    }
}

#[test]
fn windows_reserved_forms_are_recognised() {
    for name in [
        "CON", "con", "Con", "NUL", "nul.txt", "PRN", "AUX", "COM1", "com9", "LPT1", "lpt9",
        "CON.lua", "COM1.lua",
    ] {
        assert!(windows_reserved_name(name), "{name} is reserved");
    }
    for name in [
        "CONSOLE", "COM0", "COM10", "LPT0", "compile", "com9x", "module",
    ] {
        assert!(!windows_reserved_name(name), "{name} is an ordinary name");
    }
    assert!(windows_trailing_dot_or_space("module."));
    assert!(windows_trailing_dot_or_space("module "));
    assert!(!windows_trailing_dot_or_space("module"));
    assert!(!windows_trailing_dot_or_space("module.lua"));
}

// ── diagnostics: escaping, truncation, redaction ─────────────────────────────

/// The rendered bytes are the property: `0x20..=0x7E` plus tab and nothing
/// else, generated from the complement of the allowlist (every byte).
#[test]
fn rendered_fields_are_the_allowlist_complement_for_every_byte() {
    fn assert_allowed(text: &str, what: &str) {
        for byte in text.bytes() {
            assert!(
                byte == b'\t' || (0x20..=0x7E).contains(&byte),
                "{what} emitted a disallowed byte {byte:#04x} in {text:?}"
            );
        }
    }
    for byte in 0u8..=0xFF {
        let spec = vec![b'a', byte, b'b'];
        let diagnostic =
            ConfigModuleDiagnostic::new(ConfigModuleCode::Name, ConfigModulePhase::Name)
                .with_spec(spec)
                .with_source(String::from_utf8_lossy(&[byte, b'x']).into_owned())
                .with_detail(String::from_utf8_lossy(&[byte]).into_owned());
        assert_allowed(&diagnostic.render(), &format!("byte {byte:#04x}"));
    }
}

/// Independent hostile corpus, not derived from the allowlist: ANSI/CSI/OSC 8,
/// DCS, C1 as UTF-8, zero-width and combining marks, bidi isolates, U+2028,
/// U+2029, NEL, tabs.
#[test]
fn hostile_control_corpus_is_escaped() {
    let corpus = [
        "\x1b[2J\x1b[31m",
        "\x1b]8;;https://example.invalid\x07x\x1b]8;;\x07",
        "\x1bP1;2|dcs\x1b\\",
        "\u{80}\u{85}\u{9b}",
        "\u{200b}\u{200d}\u{0301}\u{fe0f}",
        "\u{202e}\u{2066}\u{2069}",
        "\u{2028}\u{2029}",
        "tab\there",
    ];
    for text in corpus {
        let diagnostic =
            ConfigModuleDiagnostic::new(ConfigModuleCode::Name, ConfigModulePhase::Name)
                .with_spec(text.as_bytes().to_vec())
                .with_detail(text.to_string());
        let rendered = diagnostic.render();
        for byte in rendered.bytes() {
            assert!(
                byte == b'\t' || (0x20..=0x7E).contains(&byte),
                "{text:?} leaked {byte:#04x} in {rendered:?}"
            );
        }
    }
}

#[test]
fn truncation_happens_before_escaping_and_marks_the_field() {
    let long = "é".repeat(200);
    let escaped = escape_rendered(long.as_bytes(), 10);
    assert!(escaped.ends_with("..."), "{escaped}");
    assert!(
        !escaped.ends_with("\\x"),
        "no half escape sequence: {escaped}"
    );
    // Decoding the emitted escapes reproduces whole scalars only: a pair of
    // bytes cut through the middle would not decode as UTF-8.
    let mut decoded = Vec::new();
    let bytes = escaped.trim_end_matches("...").as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 2..index + 4]).expect("hex");
            decoded.push(u8::from_str_radix(hex, 16).expect("hex byte"));
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let text = String::from_utf8(decoded).expect("whole scalars only");
    assert_eq!(text, "ééééé");
    // The detail field caps at 512 bytes, then escapes, so the marker is the
    // last thing in the field and is itself inside the allowlist.
    let diagnostic = ConfigModuleDiagnostic::new(ConfigModuleCode::Name, ConfigModulePhase::Name)
        .with_detail("a".repeat(MAX_MODULE_DETAIL_BYTES * 2));
    let rendered = diagnostic.render();
    assert!(rendered.contains("...\""));
    assert!(rendered.len() < MAX_MODULE_DETAIL_BYTES * 2);
}

#[test]
fn a_tab_cannot_forge_a_second_field() {
    let diagnostic = ConfigModuleDiagnostic::new(ConfigModuleCode::Name, ConfigModulePhase::Name)
        .with_detail("x\tcode=E_CONFIG_MODULE_ROOT");
    let rendered = diagnostic.render();
    // A tab cannot open a new `name=value` field: the forged text stays
    // inside the quoted `detail` field, so there is exactly one
    // space-delimited `code=` field and no space-delimited forgery.
    assert_eq!(rendered.matches(" code=").count(), 0, "{rendered}");
    assert!(rendered.starts_with("code=E_CONFIG_MODULE_NAME phase=name detail=\""));
}

#[test]
fn every_taxonomy_code_names_its_wire_spelling() {
    let codes = [
        ConfigModuleCode::Name,
        ConfigModuleCode::Root,
        ConfigModuleCode::NotFound,
        ConfigModuleCode::Ambiguous,
        ConfigModuleCode::Cycle,
        ConfigModuleCode::Depth,
        ConfigModuleCode::Count,
        ConfigModuleCode::Size,
        ConfigModuleCode::Aggregate,
        ConfigModuleCode::Escape,
        ConfigModuleCode::NotRegular,
        ConfigModuleCode::Read,
        ConfigModuleCode::Syntax,
        ConfigModuleCode::Eval,
        ConfigModuleCode::Budget,
    ];
    for code in codes {
        assert!(code.as_str().starts_with("E_CONFIG_MODULE_"));
    }
}

// ── isolation: no loader, no dynamic loading ─────────────────────────────────

#[test]
fn the_config_vm_denies_dynamic_loading_even_with_a_loader() {
    let dir = TempDir::new("deny");
    dir.module("lua/ok.lua", "return {}");
    let mut vm = LuaVm::with_config_budgets(
        "test.config",
        crate::RC1_INSTRUCTION_BUDGET,
        crate::RC1_WALL_CLOCK_BUDGET_MS,
        crate::RC1_WARNING_MS,
        crate::RC2_MEMORY_PER_PLUGIN_BYTES,
    );
    vm.install_config_require(dir.path(), "init.lua")
        .expect("install loader");
    let outcome = vm
        .eval_config(
            "assert(type(require) == 'function')\n\
             assert(load == nil and loadfile == nil and dofile == nil)\n\
             assert(package == nil and io == nil and os == nil and debug == nil)\n\
             assert(_G == nil and tonumber == nil and coroutine == nil)\n\
             return { theme = 'dark' }",
        )
        .expect("eval");
    assert!(
        matches!(outcome, ConfigOutcome::Completed { .. }),
        "the deny list must survive the loader: {outcome:?}"
    );
}

#[test]
fn a_config_with_no_require_never_touches_a_missing_root() {
    let dir = TempDir::new("no-require");
    let (vm, outcome) = eval_with_root(dir.path(), "return { theme = 'dark' }");
    assert!(
        matches!(outcome, ConfigOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert_eq!(vm.config_module_stats().expect("stats").modules, 0);
}

#[test]
fn the_loader_is_installed_once_per_generation() {
    let dir = TempDir::new("install-once");
    let mut vm = LuaVm::with_config_budgets(
        "test.config",
        crate::RC1_INSTRUCTION_BUDGET,
        crate::RC1_WALL_CLOCK_BUDGET_MS,
        crate::RC1_WARNING_MS,
        crate::RC2_MEMORY_PER_PLUGIN_BYTES,
    );
    vm.install_config_require(dir.path(), "init.lua")
        .expect("first install");
    assert!(vm.install_config_require(dir.path(), "init.lua").is_err());
}

#[test]
fn entry_name_is_used_for_root_attribution() {
    let dir = TempDir::new("entry-name");
    let mut vm = LuaVm::with_config_budgets(
        "test.config",
        crate::RC1_INSTRUCTION_BUDGET,
        crate::RC1_WALL_CLOCK_BUDGET_MS,
        crate::RC1_WARNING_MS,
        crate::RC2_MEMORY_PER_PLUGIN_BYTES,
    );
    vm.install_config_require(dir.path().join(MODULE_DIR_NAME), "profiles/work.lua")
        .expect("install");
    match vm.eval_config("return require('x')").expect("eval") {
        ConfigOutcome::ModuleRefused { diagnostic } => {
            assert_eq!(diagnostic.source.as_deref(), Some("profiles/work.lua"));
        }
        other => panic!("expected a root refusal, got {other:?}"),
    }
}

// ── Windows path security (runs on the Windows CI leg) ──────────────────────
//
// The Windows leg is not a prose claim: every test in this section is
// compiled and executed by `cargo nextest run --workspace` on
// `windows-latest`. Junctions are created through `mklink /J`, which needs no
// elevated token (unlike a directory symlink), so the reparse rules are
// exercised on a stock runner.

/// Create a directory junction at `link` pointing at `target`.
#[cfg(windows)]
fn junction(link: &Path, target: &Path) {
    use std::os::windows::process::CommandExt;
    assert!(target.is_dir(), "junction target must be a directory");
    let target = std::fs::canonicalize(target).expect("canonical target");
    // `mklink` is a cmd builtin, so it is reached through `cmd /C`. `raw_arg`
    // appends the command line verbatim (the MSVC argument quoter is meant for
    // the C runtime, not cmd.exe), so a temp path containing spaces stays one
    // argument.
    let script = format!("mklink /J \"{}\" \"{}\"", link.display(), target.display());
    let status = std::process::Command::new("cmd")
        .arg("/C")
        .raw_arg(script)
        .status()
        .expect("spawn cmd /C mklink /J");
    assert!(
        status.success(),
        "mklink /J {} -> {} failed",
        link.display(),
        target.display()
    );
}

#[cfg(windows)]
#[test]
fn windows_out_of_root_junction_refuses_as_escape() {
    let dir = TempDir::new("win-escape-out");
    dir.module("lua/keep.lua", "return { value = 'inside' }");
    let outside = TempDir::new("win-escape-outside");
    std::fs::write(
        outside.path().join("leak.lua"),
        "return { value = 'outside' }",
    )
    .expect("outside module");
    junction(&dir.path().join("lua/linked"), outside.path());
    // Canonical containment refuses the target before the walk starts.
    let diagnostic = refusal(dir.path(), "return require('linked.leak')");
    assert_code(&diagnostic, ConfigModuleCode::Escape);
}

#[cfg(windows)]
#[test]
fn windows_in_root_junction_is_refused_below_the_canonical_root() {
    // The contract does not carry the Unix "in-root symlinked layout" allowance
    // to Windows: a reparse component below the canonical root is refused even
    // when its target stays inside the root. Canonicalizing first would turn
    // this junction into its real target and silently admit it, so this test
    // pins the spec-derived walk.
    let dir = TempDir::new("win-escape-in");
    dir.module("lua/real/opts.lua", "return { value = 'inside' }");
    junction(&dir.path().join("lua/linked"), &dir.path().join("lua/real"));
    let diagnostic = refusal(dir.path(), "return require('linked.opts')");
    assert_code(&diagnostic, ConfigModuleCode::Escape);
}

#[cfg(windows)]
#[test]
fn windows_junction_swapped_before_the_walk_is_refused() {
    // NOT a mid-walk swap. `set_swap_hook` runs once, between
    // canonicalization and the walk, so `lua/pkg` is already a junction by the
    // time the first open runs and the ordinary per-component reparse check
    // refuses it — the same check the in-root-junction test pins. That makes
    // this a regression test for "a junction present at walk start is refused",
    // and it cannot distinguish a canonical-remainder walk from a
    // spec-remainder walk.
    //
    // The genuine mid-walk window (a real directory at step n replaced before
    // the open at step n + 1) is the admitted Windows residual documented on
    // `windows_open_verified`; it is not exercised here because the swap
    // cannot be driven deterministically without a test seam inside the walk.
    let dir = TempDir::new("win-swap-intermediate");
    dir.module("lua/pkg/mod.lua", "return { value = 'inside' }");
    let outside = TempDir::new("win-swap-intermediate-outside");
    std::fs::write(
        outside.path().join("mod.lua"),
        "return { value = 'outside' }",
    )
    .expect("outside module");
    let package_dir = dir.path().join("lua/pkg");
    let swap_target = outside.path().to_path_buf();
    set_swap_hook(Some(Box::new(move |path: &Path| {
        if path.ends_with("pkg/mod.lua") {
            let _ = std::fs::remove_dir_all(&package_dir);
            junction(&package_dir, &swap_target);
        }
    })));
    let diagnostic = refusal(dir.path(), "return require('pkg.mod')");
    set_swap_hook(None);
    assert_code(&diagnostic, ConfigModuleCode::Escape);
}

#[cfg(windows)]
#[test]
fn windows_final_component_swapped_to_a_junction_refuses_as_escape() {
    let dir = TempDir::new("win-swap-final");
    dir.module("lua/target.lua", "return { value = 'inside' }");
    let outside = TempDir::new("win-swap-final-outside");
    std::fs::write(
        outside.path().join("payload.lua"),
        "return { value = 'outside' }",
    )
    .expect("outside module");
    let swap_target = outside.path().to_path_buf();
    set_swap_hook(Some(Box::new(move |path: &Path| {
        // The hook is handed the canonical target, which on Windows carries a
        // verbatim prefix and normalized case, so compare the trailing
        // components instead of the literal non-canonical path.
        if path.ends_with("lua/target.lua") {
            let _ = std::fs::remove_file(path);
            junction(path, &swap_target);
        }
    })));
    let diagnostic = refusal(dir.path(), "return require('target')");
    set_swap_hook(None);
    // `FILE_FLAG_OPEN_REPARSE_POINT` refuses the object at the final
    // component; following it would have produced a directory and a
    // `NOT_REGULAR` answer instead.
    assert_code(&diagnostic, ConfigModuleCode::Escape);
}

#[cfg(windows)]
#[test]
fn windows_reserved_device_names_refuse_before_any_open() {
    let dir = TempDir::new("win-reserved");
    std::fs::create_dir_all(dir.path().join("lua")).expect("root");
    for spec in ["con", "nul", "aux", "com1", "lpt9"] {
        let diagnostic = refusal(dir.path(), &format!("return require('{spec}')"));
        assert_code(&diagnostic, ConfigModuleCode::Name);
        assert!(
            diagnostic.render().contains("reserved Windows device"),
            "{spec}: {diagnostic}"
        );
    }
    assert!(windows_component_refusal("a/init.lua").is_ok());
    assert!(windows_component_refusal("con.lua").is_err());
    assert!(windows_component_refusal("fine/COM1.lua").is_err());
    assert!(windows_component_refusal("trailing.").is_err());
    assert!(windows_component_refusal("trailing ").is_err());
}

#[cfg(windows)]
#[test]
fn windows_redirected_config_directory_still_loads() {
    // The enterprise folder-redirection layout: the configuration directory
    // itself is a junction. Root ancestors are resolved once, so the canonical
    // root works and no ancestor takes part in containment.
    let dir = TempDir::new("win-redirected");
    dir.module("actual/lua/opts.lua", "return { value = 'dark' }");
    junction(&dir.path().join("config"), &dir.path().join("actual"));
    let mut vm = LuaVm::with_config_budgets(
        "test.config",
        crate::RC1_INSTRUCTION_BUDGET,
        crate::RC1_WALL_CLOCK_BUDGET_MS,
        crate::RC1_WARNING_MS,
        crate::RC2_MEMORY_PER_PLUGIN_BYTES,
    );
    vm.install_config_require(dir.path().join("config/lua"), "init.lua")
        .expect("install loader");
    let outcome = vm
        .eval_config("return { theme = require('opts').value }")
        .expect("eval");
    match outcome {
        ConfigOutcome::Completed { data, .. } => {
            assert_eq!(data.theme.as_deref(), Some("dark"));
        }
        other => panic!("a redirected configuration directory must load: {other:?}"),
    }
}

#[cfg(windows)]
#[test]
fn windows_case_variant_specs_collapse_to_one_identity() {
    // NTFS is case-insensitive, so both spellings reach one file and the
    // case-folded canonical cache key collapses them to one evaluation.
    let dir = TempDir::new("win-case");
    dir.module("lua/Config/Options.lua", "return { value = 'dark' }");
    let (vm, outcome) = eval_with_root(
        dir.path(),
        "local a = require('config.options')\n\
         local b = require('Config.Options')\n\
         return { theme = tostring(a == b) }",
    );
    match outcome {
        ConfigOutcome::Completed { data, .. } => {
            assert_eq!(data.theme.as_deref(), Some("true"));
        }
        other => panic!("case variants must resolve on NTFS: {other:?}"),
    }
    assert_eq!(vm.config_module_stats().expect("stats").modules, 1);
}
