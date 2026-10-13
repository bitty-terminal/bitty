//! Colorscheme bridge tests (CTX-1169, issue #1976).
//!
//! Pins the `bitty.image.read` and `bitty.theme.preview`/`confirm` spellings
//! against stub hosts: namespace shape and read-only tables, bridge-side
//! path/table validation (no host contact on malformed input), descriptor
//! delivery, persist-flag recording, and typed grant denials.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{
    BoundedExecution, BridgeError, HostServices, LuaValue, LuaVm, MarshallingLimits, ThemeTable,
};

fn gate_vm(id: impl Into<String>) -> LuaVm {
    build_plugin_vm(id, Some(VmBudgets::default())).expect("default budgets are valid")
}

#[derive(Default)]
struct FakeColorscheme {
    store: RefCell<BTreeMap<String, LuaValue>>,
    image_granted: bool,
    theme_granted: bool,
    previewed: RefCell<Vec<ThemeTable>>,
    confirmed: RefCell<Vec<ThemeTable>>,
}

impl HostServices for FakeColorscheme {
    fn store_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(self.store.borrow().get(key).cloned())
    }

    fn store_set(&self, key: &str, value: LuaValue) -> Result<(), BridgeError> {
        if matches!(value, LuaValue::Nil) {
            self.store.borrow_mut().remove(key);
        } else {
            self.store.borrow_mut().insert(key.to_string(), value);
        }
        Ok(())
    }

    fn settings_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        let _ = key;
        Ok(None)
    }

    fn terminal_snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }

    fn notify_show(&self, _payload: &LuaValue) -> Result<bool, BridgeError> {
        Err(BridgeError::capability_denied("platform.notify"))
    }

    fn image_read(&self, path: &str) -> Result<LuaValue, BridgeError> {
        if !self.image_granted {
            return Err(BridgeError::capability_denied("platform.image-file"));
        }
        Ok(LuaValue::table([
            ("path", LuaValue::String(format!("/approved/{path}"))),
            ("format", LuaValue::String("png".to_string())),
            ("width", LuaValue::Integer(3)),
            ("height", LuaValue::Integer(2)),
            ("size", LuaValue::Integer(70)),
        ]))
    }

    fn theme_preview(&self, theme: &ThemeTable) -> Result<bool, BridgeError> {
        if !self.theme_granted {
            return Err(BridgeError::capability_denied("runtime.configure"));
        }
        self.previewed.borrow_mut().push(theme.clone());
        Ok(true)
    }

    fn theme_confirm(&self, theme: &ThemeTable) -> Result<bool, BridgeError> {
        if !self.theme_granted {
            return Err(BridgeError::capability_denied("runtime.configure"));
        }
        self.confirmed.borrow_mut().push(theme.clone());
        Ok(true)
    }
}

fn install(vm: &mut LuaVm, services: Rc<FakeColorscheme>) {
    let services: Rc<dyn HostServices> = services;
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install");
}

fn run(vm: &mut LuaVm, code: &str) -> BoundedExecution {
    vm.execute_bounded(code).expect("execute")
}

fn theme_lua() -> &'static str {
    r##"
    theme_arg = {
        background = "#1e1e2e", foreground = "#cdd6f4",
        cursor = "#f5e0dc", selection = "#313244",
        ansi = {
            "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af",
            "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
            "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af",
            "#89b4fa", "#f5c2e7", "#94e2d5", "#cdd6f4",
        },
    }
    "##
}

#[test]
fn colorscheme_namespaces_present_and_read_only() {
    let mut vm = gate_vm("colorscheme-shape");
    install(&mut vm, Rc::new(FakeColorscheme::default()));
    let outcome = run(
        &mut vm,
        r#"
        assert(type(bitty.image) == "table", "bitty.image must exist")
        assert(type(bitty.image.read) == "function", "image.read must exist")
        assert(bitty.image.write == nil, "no image write verb")
        assert(type(bitty.theme) == "table", "bitty.theme must exist")
        assert(type(bitty.theme.preview) == "function", "preview must exist")
        assert(type(bitty.theme.confirm) == "function", "confirm must exist")
        assert(bitty.theme.apply == nil, "no silent apply verb")
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    for chunk in [
        "bitty.image = {}",
        "bitty.image.read = 1",
        "bitty.theme = {}",
        "bitty.theme.preview = 1",
        "bitty.theme.confirm = 1",
    ] {
        let mut vm = gate_vm("colorscheme-readonly");
        install(&mut vm, Rc::new(FakeColorscheme::default()));
        let outcome = run(&mut vm, chunk);
        assert!(
            matches!(outcome, BoundedExecution::RuntimeError(_)),
            "{chunk} must fail: {outcome:?}"
        );
    }
}

#[test]
fn image_read_validates_path_before_host_contact() {
    let mut vm = gate_vm("colorscheme-image-shape");
    install(&mut vm, Rc::new(FakeColorscheme::default()));
    let outcome = run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.image.read, "")
        assert(ok == false, "empty path fails")
        assert(err.code == "E_DEF_INVALID", "typed invalid, got " .. tostring(err.code))
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn image_read_delivers_descriptor_and_denies_without_grant() {
    let services = Rc::new(FakeColorscheme {
        image_granted: true,
        ..FakeColorscheme::default()
    });
    let mut vm = gate_vm("colorscheme-image-ok");
    install(&mut vm, services.clone());
    let outcome = run(
        &mut vm,
        r#"
        local descriptor = bitty.image.read("paper.png")
        assert(descriptor.format == "png", "format crosses")
        assert(descriptor.width == 3, "width crosses")
        assert(descriptor.height == 2, "height crosses")
        assert(descriptor.size == 70, "size crosses")
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    let mut vm = gate_vm("colorscheme-image-denied");
    install(&mut vm, Rc::new(FakeColorscheme::default()));
    let outcome = run(
        &mut vm,
        r#"
        local ok, err = pcall(bitty.image.read, "paper.png")
        assert(ok == false, "ungranted read fails")
        assert(err.code == "E_CAPABILITY_DENIED", "typed denial, got " .. tostring(err.code))
        "#,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}

#[test]
fn theme_preview_and_confirm_record_persist_flags() {
    let services = Rc::new(FakeColorscheme {
        theme_granted: true,
        ..FakeColorscheme::default()
    });
    let mut vm = gate_vm("colorscheme-theme-ok");
    install(&mut vm, services.clone());
    let outcome = run(
        &mut vm,
        &format!(
            "{} assert(bitty.theme.preview(theme_arg) == true, \"preview queues\") assert(bitty.theme.confirm(theme_arg) == true, \"confirm queues\")",
            theme_lua()
        ),
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(services.previewed.borrow().len(), 1);
    assert_eq!(services.confirmed.borrow().len(), 1);
    assert_eq!(
        services.previewed.borrow()[0],
        services.confirmed.borrow()[0]
    );
    let mut vm = gate_vm("colorscheme-theme-shape");
    install(&mut vm, services.clone());
    let outcome = run(
        &mut vm,
        r##"
        local bad = { background = "#1e1e2e" }
        local ok, err = pcall(bitty.theme.preview, bad)
        assert(ok == false, "ragged table fails")
        assert(err.code == "E_DEF_INVALID", "typed invalid, got " .. tostring(err.code))
        "##,
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
    assert_eq!(
        services.previewed.borrow().len(),
        1,
        "malformed tables never reach the host"
    );
    let mut vm = gate_vm("colorscheme-theme-denied");
    install(&mut vm, Rc::new(FakeColorscheme::default()));
    let outcome = run(
        &mut vm,
        &format!(
            "{} local ok, err = pcall(bitty.theme.confirm, theme_arg) assert(ok == false, \"ungranted confirm fails\") assert(err.code == \"E_CAPABILITY_DENIED\", \"typed denial, got \" .. tostring(err.code))",
            theme_lua()
        ),
    );
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "{outcome:?}"
    );
}
