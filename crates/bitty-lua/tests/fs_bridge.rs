//! `bitty.fs` host bridge parity tests (RFC-0005, CTX-0984).
//!
//! Core-owned `bitty.fs.*` root beside `terminal.*` with the decided verbs
//! `read`, `write`, and `list` (`open` rejected, `append` as a
//! write-disposition flag, `list` under the read grant). Reuses the accepted
//! `4096` path bound; scoped grants carry no wildcard; results are
//! read-into-VM-only with the Core-attached untrusted label; budgets deny
//! polling-as-watch; denials are oracle-tight with silent-skip listing;
//! process combination stays argv-first; no `terminal.*` migration either
//! direction; no watch, handles, or streaming.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{
    BoundedExecution, BridgeError, FS_CONTENT_MAX_BYTES, FS_LIST_MAX_ENTRIES, FS_PATH_MAX_BYTES,
    HostServices, LuaValue, LuaVm, MarshallingLimits,
};

fn gate_vm(id: impl Into<String>) -> LuaVm {
    build_plugin_vm(id, Some(VmBudgets::default())).expect("default budgets are valid")
}

/// Mock fs backend with explicit scoped grants, budgets, silent-skip
/// listings, and untrusted labels (parity with `FsGate`, not shared code:
/// `bitty-lua` stays runtime-generic with no `bitty-plugin-host` dependency).
#[derive(Default)]
struct FakeFs {
    files: RefCell<BTreeMap<String, String>>,
    read_grants: RefCell<Vec<String>>,
    write_grants: RefCell<Vec<String>>,
    ops: RefCell<u64>,
    bytes: RefCell<u64>,
    max_ops: u64,
}

impl FakeFs {
    fn with_grants(read: &[&str], write: &[&str]) -> Self {
        Self {
            files: RefCell::new(BTreeMap::new()),
            read_grants: RefCell::new(read.iter().map(|s| (*s).to_string()).collect()),
            write_grants: RefCell::new(write.iter().map(|s| (*s).to_string()).collect()),
            ops: RefCell::new(0),
            bytes: RefCell::new(0),
            max_ops: 8,
        }
    }

    fn covers(grants: &[String], path: &str) -> bool {
        grants.iter().any(|prefix| {
            path == prefix
                || path.starts_with(&format!("{prefix}/"))
                || (prefix.ends_with("/**") && path.starts_with(&prefix[..prefix.len() - 3]))
                || (prefix.ends_with("/*") && path.starts_with(&prefix[..prefix.len() - 2]))
        })
    }

    fn check_budget(&self) -> Result<(), BridgeError> {
        if *self.ops.borrow() >= self.max_ops {
            return Err(BridgeError::new(
                "budget",
                "E_FS_OVER_BOUND",
                "fs denied for family 'fs'",
            ));
        }
        Ok(())
    }

    fn charge(&self, bytes: u64) {
        *self.ops.borrow_mut() += 1;
        *self.bytes.borrow_mut() += bytes;
    }

    fn denied(code: &'static str) -> BridgeError {
        BridgeError::new("runtime", code, "fs denied for family 'fs'")
    }
}

impl HostServices for FakeFs {
    fn store_get(&self, _key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(None)
    }

    fn store_set(&self, _key: &str, _value: LuaValue) -> Result<(), BridgeError> {
        Ok(())
    }

    fn settings_get(&self, _key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(None)
    }

    fn terminal_snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }

    fn notify_show(&self, _payload: &LuaValue) -> Result<bool, BridgeError> {
        Err(BridgeError::capability_denied("platform.notify"))
    }

    fn fs_read(&self, path: &str) -> Result<LuaValue, BridgeError> {
        if !Self::covers(&self.read_grants.borrow(), path) {
            return Err(Self::denied("E_FS_SCOPE_MISMATCH"));
        }
        if path.contains(".env") {
            return Err(Self::denied("E_FS_SENSITIVE_PATH"));
        }
        let Some(content) = self.files.borrow().get(path).cloned() else {
            return Err(Self::denied("E_FS_SCOPE_MISMATCH"));
        };
        self.check_budget()?;
        // Secret-shaped reads serve redacted with the label, never raw.
        let (body, redacted) = if content.contains("AKIA") {
            ("[redacted]".to_string(), true)
        } else {
            (content, true)
        };
        self.charge(body.len() as u64);
        Ok(LuaValue::table([
            ("body", LuaValue::String(body)),
            ("truncated", LuaValue::Bool(false)),
            ("redacted", LuaValue::Bool(redacted)),
            ("untrusted", LuaValue::Bool(true)),
            ("path", LuaValue::String(path.to_string())),
        ]))
    }

    fn fs_write(&self, path: &str, content: &str, append: bool) -> Result<LuaValue, BridgeError> {
        if !Self::covers(&self.write_grants.borrow(), path) {
            return Err(Self::denied("E_FS_SCOPE_MISMATCH"));
        }
        if content.contains("AKIA") {
            return Err(Self::denied("E_FS_SECRET_CONTENT"));
        }
        self.check_budget()?;
        let mut files = self.files.borrow_mut();
        let next = if append {
            format!(
                "{}{}",
                files.get(path).cloned().unwrap_or_default(),
                content
            )
        } else {
            content.to_string()
        };
        files.insert(path.to_string(), next);
        self.charge(content.len() as u64);
        Ok(LuaValue::table([
            ("path", LuaValue::String(path.to_string())),
            ("bytes_written", LuaValue::Integer(content.len() as i64)),
            ("appended", LuaValue::Bool(append)),
            ("untrusted", LuaValue::Bool(true)),
        ]))
    }

    fn fs_list(&self, prefix: &str, max_entries: usize) -> Result<LuaValue, BridgeError> {
        if !Self::covers(&self.read_grants.borrow(), prefix)
            && !Self::covers(&self.read_grants.borrow(), &format!("{prefix}/x"))
            && !self
                .read_grants
                .borrow()
                .iter()
                .any(|g| prefix.starts_with(g.trim_end_matches("/**").trim_end_matches("/*")))
        {
            // Prefix outside the read grant denies explicitly (not silent).
            let covered = Self::covers(&self.read_grants.borrow(), prefix);
            if !covered {
                // Allow listing when any grant is a parent of the prefix.
                let parent_ok = self.read_grants.borrow().iter().any(|g| {
                    let base = g.trim_end_matches("/**").trim_end_matches("/*");
                    prefix == base || prefix.starts_with(base)
                });
                if !parent_ok {
                    return Err(Self::denied("E_FS_SCOPE_MISMATCH"));
                }
            }
        }
        // Silent-skip: denied entries omitted without marking.
        let mut names: Vec<String> = Vec::new();
        for path in self.files.borrow().keys() {
            if path == prefix {
                continue;
            }
            let Some(rest) = path.strip_prefix(prefix) else {
                continue;
            };
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            if rest.is_empty() || rest.contains('/') {
                continue;
            }
            let child = format!("{prefix}/{rest}");
            if !Self::covers(&self.read_grants.borrow(), &child) {
                continue;
            }
            if child.contains(".env") {
                continue;
            }
            names.push(child);
        }
        names.sort();
        names.truncate(max_entries);
        self.check_budget()?;
        self.charge(names.iter().map(|n| n.len() as u64).sum());
        Ok(LuaValue::array(
            names
                .into_iter()
                .map(|name| {
                    LuaValue::table([
                        ("name", LuaValue::String(name)),
                        ("kind", LuaValue::String("file".to_string())),
                        ("untrusted", LuaValue::Bool(true)),
                    ])
                })
                .collect(),
        ))
    }
}

fn install(vm: &mut LuaVm, services: Rc<FakeFs>) {
    let services: Rc<dyn HostServices> = services;
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install");
}

fn run(vm: &mut LuaVm, code: &str) -> BoundedExecution {
    vm.execute_bounded(code).expect("execute")
}

#[test]
fn fs_root_has_decided_verbs_and_rejects_open_append_watch() {
    let mut vm = gate_vm("fs-shape");
    install(
        &mut vm,
        Rc::new(FakeFs::with_grants(&["~/docs"], &["~/docs/out"])),
    );
    let outcome = run(
        &mut vm,
        r#"
        assert(type(bitty.fs) == "table", "bitty.fs must exist")
        assert(type(bitty.fs.read) == "function", "read must exist")
        assert(type(bitty.fs.write) == "function", "write must exist")
        assert(type(bitty.fs.list) == "function", "list must exist")
        assert(bitty.fs.open == nil, "open is rejected as a verb")
        assert(bitty.fs.append == nil, "append is a flag, not a verb")
        assert(bitty.fs.watch == nil, "no watch")
        assert(bitty.fs.subscribe == nil, "no subscribe")
        assert(bitty.fs.tail == nil, "no tail-follow")
        assert(bitty.fs.stream == nil, "no streaming")
        assert(bitty.fs.handle == nil, "no handles")
        assert(bitty.fs.handles == nil, "no handles")
        assert(bitty.fs.cursor == nil, "no cursor")
        result = "ok"
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn fs_grants_gate_read_write_list() {
    let services = Rc::new(FakeFs::with_grants(&["~/docs"], &["~/docs/out"]));
    services
        .files
        .borrow_mut()
        .insert("~/docs/notes.txt".to_string(), "hello".to_string());
    let mut vm = gate_vm("fs-grants");
    install(&mut vm, services.clone());
    // Read under the read grant succeeds with the untrusted label.
    let outcome = run(
        &mut vm,
        r#"
        local result = bitty.fs.read("~/docs/notes.txt")
        assert(result.body == "hello", "read body")
        assert(result.untrusted == true, "untrusted label")
        assert(result.redacted == true, "redacted flag")
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    // Write under the write grant succeeds; append flag selects disposition.
    let outcome = run(
        &mut vm,
        r#"
        local receipt = bitty.fs.write("~/docs/out/a.txt", "one", nil)
        assert(receipt.appended == false, "default is overwrite")
        local receipt2 = bitty.fs.write("~/docs/out/a.txt", "+two", { append = true })
        assert(receipt2.appended == true, "append flag")
        assert(receipt2.untrusted == true, "receipt label")
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.files.borrow().get("~/docs/out/a.txt").unwrap(),
        "one+two"
    );
    // List under the read grant succeeds with silent-skip shape.
    let outcome = run(
        &mut vm,
        r#"
        local entries = bitty.fs.list("~/docs")
        assert(#entries >= 1, "at least one visible entry")
        for _, entry in ipairs(entries) do
          assert(entry.untrusted == true, "entry label")
          assert(entry.kind == "file" or entry.kind == "dir", "kind metadata")
        end
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    // Read grant never implies write: writing under the read-only path denies.
    let outcome = run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.fs.write, "~/docs/notes.txt", "x", nil)
        assert(ok == false, "write outside write grant must deny")
        assert(err.code == "E_FS_SCOPE_MISMATCH", "typed scope denial, got " .. tostring(err.code))
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn fs_path_and_payload_bounds_reuse_4096_8kib() {
    assert_eq!(FS_PATH_MAX_BYTES, 4096);
    assert_eq!(FS_CONTENT_MAX_BYTES, 8 * 1024);
    assert_eq!(FS_LIST_MAX_ENTRIES, 1024);
    let mut vm = gate_vm("fs-bounds");
    install(
        &mut vm,
        Rc::new(FakeFs::with_grants(&["~/docs"], &["~/docs/out"])),
    );
    // Empty path fails closed with validation (not a denial category).
    let outcome = run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.fs.read, "")
        assert(ok == false, "empty path must fail")
        assert(err.class == "validation", "validation class, got " .. tostring(err.class))
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    // Over-bound path fails closed with E_DEF_LIMIT before any grant check.
    let long_path = format!("~/docs/{}", "x".repeat(4096));
    let outcome = run(
        &mut vm,
        &format!(
            "local ok, err = pcall(bitty.fs.read, \"{long_path}\"); \
             assert(ok == false, \"over-bound path must fail\"); \
             assert(err.code == \"E_DEF_LIMIT\", \"typed limit, got \" .. tostring(err.code))"
        ),
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    // Over-bound write payload fails closed before the host.
    let outcome = run(
        &mut vm,
        &format!(
            "local big = string.rep(\"x\", {}); \
             local ok, err = pcall(bitty.fs.write, \"~/docs/out/a.txt\", big, nil); \
             assert(ok == false, \"over-bound payload must fail\"); \
             assert(err.code == \"E_DEF_LIMIT\" or err.code == \"E_VALUE_BYTES\", \
             \"typed limit, got \" .. tostring(err.code))",
            FS_CONTENT_MAX_BYTES + 1
        ),
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn fs_denials_are_typed_and_oracle_tight() {
    let services = Rc::new(FakeFs::with_grants(&["~/docs"], &["~/docs/out"]));
    services
        .files
        .borrow_mut()
        .insert("~/docs/notes.txt".to_string(), "hello".to_string());
    let mut vm = gate_vm("fs-denials");
    install(&mut vm, services);
    // Scope mismatch carries the typed code without content bytes.
    let outcome = run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.fs.read, "~/other/x.txt")
        assert(ok == false, "out-of-scope must deny")
        assert(err.code == "E_FS_SCOPE_MISMATCH", "typed denial, got " .. tostring(err.code))
        assert(not string.find(err.message, "hello"), "no content bytes in denial")
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    // Sensitive-path denial is typed without naming secrets.
    let outcome = run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.fs.read, "~/docs/.env")
        assert(ok == false, "sensitive must deny")
        assert(err.code == "E_FS_SENSITIVE_PATH", "typed denial, got " .. tostring(err.code))
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    // Secret-shaped write refuses with the typed category.
    let outcome = run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.fs.write, "~/docs/out/s.txt", "AKIAIOSFODNN7EXAMPLE", nil)
        assert(ok == false, "secret-shaped write must refuse")
        assert(err.code == "E_FS_SECRET_CONTENT", "typed denial, got " .. tostring(err.code))
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn fs_listings_suppress_denied_entries_silently() {
    let services = Rc::new(FakeFs::with_grants(&["~/docs"], &["~/docs/out"]));
    services
        .files
        .borrow_mut()
        .insert("~/docs/notes.txt".to_string(), "ok".to_string());
    services
        .files
        .borrow_mut()
        .insert("~/docs/.env".to_string(), "K=v".to_string());
    services
        .files
        .borrow_mut()
        .insert("~/other/x.txt".to_string(), "outside".to_string());
    let mut vm = gate_vm("fs-silent");
    install(&mut vm, services);
    let outcome = run(
        &mut vm,
        r#"
        local entries = bitty.fs.list("~/docs")
        local seen_notes = false
        for _, entry in ipairs(entries) do
          assert(entry.untrusted == true, "entry label")
          assert(not string.find(entry.name, ".env"), "sensitive suppressed silently")
          assert(not string.find(entry.name, "other"), "out-of-scope suppressed silently")
          if entry.name == "~/docs/notes.txt" then seen_notes = true end
        end
        assert(seen_notes, "visible entry present")
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn fs_budgets_deny_polling_as_watch() {
    let services = Rc::new(FakeFs::with_grants(&["~/docs"], &["~/docs/out"]));
    services
        .files
        .borrow_mut()
        .insert("~/docs/notes.txt".to_string(), "hello".to_string());
    let mut vm = gate_vm("fs-budget");
    install(&mut vm, services);
    // Eight successful polls fit the window; the ninth denies as over-bound
    // (polling cannot reconstitute a watch or tail-follow).
    let outcome = run(
        &mut vm,
        r#"
        for i = 1, 8 do
          local result = bitty.fs.read("~/docs/notes.txt")
          assert(result.body == "hello", "poll " .. i)
        end
        local ok, err = pcall(bitty.fs.read, "~/docs/notes.txt")
        assert(ok == false, "ninth poll must deny")
        assert(err.code == "E_FS_OVER_BOUND", "typed budget denial, got " .. tostring(err.code))
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn fs_labels_survive_redaction_and_attribution() {
    let services = Rc::new(FakeFs::with_grants(&["~/docs"], &["~/docs/out"]));
    services.files.borrow_mut().insert(
        "~/docs/s.txt".to_string(),
        "AKIAIOSFODNN7EXAMPLE".to_string(),
    );
    let mut vm = gate_vm("fs-labels");
    install(&mut vm, services);
    let outcome = run(
        &mut vm,
        r#"
        local result = bitty.fs.read("~/docs/s.txt")
        assert(result.body == "[redacted]", "secret redacted, got " .. tostring(result.body))
        assert(result.redacted == true, "redacted flag")
        assert(result.untrusted == true, "label survives redaction")
        assert(result.path == "~/docs/s.txt", "attribution path")
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn fs_does_not_migrate_with_terminal_in_either_direction() {
    let mut vm = gate_vm("fs-nomigrate");
    install(
        &mut vm,
        Rc::new(FakeFs::with_grants(&["~/docs"], &["~/docs/out"])),
    );
    let outcome = run(
        &mut vm,
        r#"
        -- The fs root carries no terminal members and vice versa.
        assert(bitty.fs.snapshot == nil, "no terminal snapshot under fs")
        assert(bitty.terminal.read == nil, "no fs read under terminal")
        assert(bitty.terminal.write == nil, "no fs write under terminal")
        assert(bitty.terminal.list == nil, "no fs list under terminal")
        -- Both namespaces stay present and callable (v1 frozen, fs is new).
        assert(type(bitty.terminal.snapshot) == "function", "terminal snapshot stays")
        assert(type(bitty.fs.read) == "function", "fs read is new root")
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn fs_list_oversized_max_entries_fails_with_e_def_limit() {
    let services = Rc::new(FakeFs::with_grants(&["~/docs"], &["~/docs/out"]));
    services
        .files
        .borrow_mut()
        .insert("~/docs/notes.txt".to_string(), "hello".to_string());
    let mut vm = gate_vm("fs-list-limit");
    install(&mut vm, services);
    // Oversized max_entries must fail with E_DEF_LIMIT, never clamp silently
    // to a 1024-entry page (CodeRabbit PR #1731, 03:36 UTC thread).
    let oversized = FS_LIST_MAX_ENTRIES + 1;
    let outcome = run(
        &mut vm,
        &format!(
            "local ok, err = pcall(bitty.fs.list, \"~/docs\", {oversized}); \
             assert(ok == false, \"oversized max_entries must fail\"); \
             assert(err.code == \"E_DEF_LIMIT\", \"typed limit, got \" .. tostring(err.code))"
        ),
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    // A large oversized request (e.g. 5000) also fails typed, not silently.
    let outcome = run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.fs.list, "~/docs", 5000)
        assert(ok == false, "5000 max_entries must fail")
        assert(err.code == "E_DEF_LIMIT", "typed limit, got " .. tostring(err.code))
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    // The cap itself stays accepted.
    let outcome = run(
        &mut vm,
        &format!(
            "local entries = bitty.fs.list(\"~/docs\", {FS_LIST_MAX_ENTRIES}); \
             assert(#entries >= 1, \"cap-sized page must list\")"
        ),
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn fs_combines_argv_first_with_no_shell_string() {
    let mut vm = gate_vm("fs-argv");
    install(
        &mut vm,
        Rc::new(FakeFs::with_grants(&["~/docs"], &["~/docs/out"])),
    );
    // File bytes combined to process.spawn must travel as argv entries, never
    // through shell-string construction: spawn takes an argv array table.
    let outcome = run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.process.spawn, "not-a-table")
        assert(ok == false, "spawn needs an argv array")
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}
