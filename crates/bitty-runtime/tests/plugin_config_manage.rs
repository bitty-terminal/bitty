//! `bitty.config.preview`/`bitty.config.set` over real plugin VMs
//! (CTX-1613, bitty#2393).
//!
//! The write-only `config.manage` head is exercised through the real Lua
//! bridge: deny-by-default without the grant, dev-root packages that merely
//! declare the head do not receive it, a preview/apply pair bound to the
//! rendered candidate, one-shot approval tokens, and shape refusals for paths,
//! tables, and extra arguments. The durable writer is a recording backend
//! here; the atomic user-layer writer is covered by the application's unit
//! tests. Headless and Windows-safe (temp dirs only).

mod common;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    ConfigApplyOutcome, ConfigChange, ConfigPreviewRendering, ConfigReadFailure, ConfigReadValue,
    ConfigSourceLabel, ConfigSourceList, ConfigSourceRow, ConfigValueRow, ConfigWriteBackend,
    ConfigWriteFailure, EmptySettings, PluginEnabledRendering, PluginList, PluginListRow,
    PluginRecord, PluginRuntime, PluginRuntimeConfig, ProfileCloneOptions, ProfileCloneOutcome,
    ProfileCreateOptions, ProfileCreateOutcome, ProfileDeleteOptions, ProfileDeleteOutcome,
    ProfileFailureKind, ProfileLifecycleRendering, ProfileList, ProfileListRow,
    ProfileRenameOptions, ProfileRenameOutcome, ProfileUseRendering, ProfileWriteFailure,
    SnapshotSource, SourceClass, candidate_digest, content_digest, manifest_toml, write_index,
};

struct NoSnapshot;

impl SnapshotSource for NoSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }
}

/// Recording backend: the service must hand it the exact rendered change.
#[derive(Default)]
struct RecordingBackend {
    calls: RefCell<Vec<ConfigChange>>,
    failure: Option<ConfigWriteFailure>,
    stale: bool,
}

impl ConfigWriteBackend for RecordingBackend {
    fn config_get(&self, _plugin_id: &str, key: &str) -> Result<ConfigValueRow, ConfigReadFailure> {
        Ok(ConfigValueRow {
            key: key.to_string(),
            value: ConfigReadValue::Text(String::from("catppuccin")),
            source: ConfigSourceLabel::User,
            redacted: false,
        })
    }

    fn config_sources(&self, _plugin_id: &str) -> Result<ConfigSourceList, ConfigReadFailure> {
        Ok(ConfigSourceList {
            rows: vec![
                ConfigSourceRow {
                    key: String::from("appearance.theme"),
                    source: ConfigSourceLabel::User,
                },
                ConfigSourceRow {
                    key: String::from("window.padding"),
                    source: ConfigSourceLabel::Default,
                },
            ],
            truncated: false,
        })
    }

    fn plugins_list(&self, _plugin_id: &str) -> Result<PluginList, ConfigReadFailure> {
        Ok(PluginList {
            rows: vec![PluginListRow {
                id: String::from("owner.plugin"),
                enabled: true,
                managed: true,
                source: String::from("bundled"),
                version: String::from("1.0.0"),
            }],
            truncated: false,
        })
    }

    fn profiles_list(&self, _plugin_id: &str) -> Result<ProfileList, ConfigReadFailure> {
        Ok(ProfileList {
            rows: vec![
                ProfileListRow {
                    name: String::from("default"),
                    active: true,
                },
                ProfileListRow {
                    name: String::from("work"),
                    active: false,
                },
            ],
            active: Some(String::from("default")),
            truncated: false,
        })
    }

    fn preview_plugin_enabled(
        &self,
        _plugin_id: &str,
        id: &str,
        enabled: bool,
    ) -> Result<PluginEnabledRendering, ConfigWriteFailure> {
        let rendered = format!("plugins.{id}.enabled = {enabled}");
        Ok(PluginEnabledRendering {
            changed_keys: vec![format!("plugins.{id}.enabled")],
            candidate_digest: candidate_digest(rendered.as_bytes()),
            rendered,
        })
    }

    fn apply_plugin_enabled(
        &self,
        _plugin_id: &str,
        id: &str,
        enabled: bool,
        expected_candidate_digest: &str,
    ) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
        let rendered = format!("plugins.{id}.enabled = {enabled}");
        if candidate_digest(rendered.as_bytes()) != expected_candidate_digest {
            return Err(ConfigWriteFailure::Stale);
        }
        // The ledger is only read at activation, so an applied toggle always
        // needs a restart.
        Ok(ConfigApplyOutcome::Applied {
            pending_restart: true,
        })
    }

    fn preview_profile_use(
        &self,
        _plugin_id: &str,
        name: &str,
    ) -> Result<ProfileUseRendering, ConfigWriteFailure> {
        let rendered = format!("profile = \"{name}\"");
        Ok(ProfileUseRendering {
            changed_keys: vec![String::from("profile")],
            candidate_digest: candidate_digest(rendered.as_bytes()),
            rendered,
            changed: true,
        })
    }

    fn apply_profile_use(
        &self,
        _plugin_id: &str,
        name: &str,
        expected_candidate_digest: &str,
    ) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
        let rendered = format!("profile = \"{name}\"");
        if candidate_digest(rendered.as_bytes()) != expected_candidate_digest {
            return Err(ConfigWriteFailure::Stale);
        }
        // bitty#2814: a live-only switch reports no pending restart.
        Ok(ConfigApplyOutcome::Applied {
            pending_restart: false,
        })
    }

    fn preview_profile_create(
        &self,
        _plugin_id: &str,
        name: &str,
        options: &ProfileCreateOptions,
    ) -> Result<ProfileLifecycleRendering, ProfileWriteFailure> {
        let rendered = format!(
            "+return {{ -- {name} force={} from={:?} preset={:?} }}",
            options.force, options.from, options.preset
        );
        Ok(ProfileLifecycleRendering {
            changed_keys: vec![format!("profiles.{name}")],
            candidate_digest: candidate_digest(rendered.as_bytes()),
            rendered,
            changed: true,
            replaces_existing: options.force,
            selector_updated: false,
            active: false,
        })
    }

    fn apply_profile_create(
        &self,
        _plugin_id: &str,
        name: &str,
        options: &ProfileCreateOptions,
        expected_candidate_digest: &str,
    ) -> Result<ProfileCreateOutcome, ProfileWriteFailure> {
        let rendered = format!(
            "+return {{ -- {name} force={} from={:?} preset={:?} }}",
            options.force, options.from, options.preset
        );
        if candidate_digest(rendered.as_bytes()) != expected_candidate_digest {
            return Err(ProfileWriteFailure::new(
                ProfileFailureKind::Stale,
                "the profile candidate changed since it was previewed",
            ));
        }
        Ok(ProfileCreateOutcome {
            name: name.to_string(),
            replaced: options.force,
            backed_up: options.force,
            extends: options.from.clone(),
            preset: options.preset.clone(),
        })
    }

    fn preview_profile_clone(
        &self,
        _plugin_id: &str,
        src: &str,
        dst: &str,
        options: &ProfileCloneOptions,
    ) -> Result<ProfileLifecycleRendering, ProfileWriteFailure> {
        let rendered = format!("+clone {src} -> {dst} (force={})", options.force);
        Ok(ProfileLifecycleRendering {
            changed_keys: vec![format!("profiles.{dst}")],
            candidate_digest: candidate_digest(rendered.as_bytes()),
            rendered,
            changed: true,
            replaces_existing: options.force,
            selector_updated: false,
            active: false,
        })
    }

    fn apply_profile_clone(
        &self,
        _plugin_id: &str,
        src: &str,
        dst: &str,
        options: &ProfileCloneOptions,
        expected_candidate_digest: &str,
    ) -> Result<ProfileCloneOutcome, ProfileWriteFailure> {
        let rendered = format!("+clone {src} -> {dst} (force={})", options.force);
        if candidate_digest(rendered.as_bytes()) != expected_candidate_digest {
            return Err(ProfileWriteFailure::new(
                ProfileFailureKind::Stale,
                "the profile candidate changed since it was previewed",
            ));
        }
        Ok(ProfileCloneOutcome {
            source: src.to_string(),
            target: dst.to_string(),
            replaced: options.force,
            backed_up: options.force,
            extends: None,
        })
    }

    fn preview_profile_rename(
        &self,
        _plugin_id: &str,
        old: &str,
        new: &str,
        options: &ProfileRenameOptions,
    ) -> Result<ProfileLifecycleRendering, ProfileWriteFailure> {
        let rendered = format!(
            "-profile = \"{old}\"\n+profile = \"{new}\" (force={})",
            options.force
        );
        Ok(ProfileLifecycleRendering {
            changed_keys: vec![
                format!("profiles.{old}"),
                format!("profiles.{new}"),
                String::from("profile"),
            ],
            candidate_digest: candidate_digest(rendered.as_bytes()),
            rendered,
            changed: true,
            replaces_existing: options.force,
            selector_updated: true,
            active: true,
        })
    }

    fn apply_profile_rename(
        &self,
        _plugin_id: &str,
        old: &str,
        new: &str,
        options: &ProfileRenameOptions,
        expected_candidate_digest: &str,
    ) -> Result<ProfileRenameOutcome, ProfileWriteFailure> {
        let rendered = format!(
            "-profile = \"{old}\"\n+profile = \"{new}\" (force={})",
            options.force
        );
        if candidate_digest(rendered.as_bytes()) != expected_candidate_digest {
            return Err(ProfileWriteFailure::new(
                ProfileFailureKind::Stale,
                "the profile candidate changed since it was previewed",
            ));
        }
        Ok(ProfileRenameOutcome {
            old_name: old.to_string(),
            new_name: new.to_string(),
            replaced: options.force,
            backed_up: true,
            selector_updated: true,
        })
    }

    fn preview_profile_delete(
        &self,
        _plugin_id: &str,
        name: &str,
        options: &ProfileDeleteOptions,
    ) -> Result<ProfileLifecycleRendering, ProfileWriteFailure> {
        if name == "extended" {
            return Err(ProfileWriteFailure::new(
                ProfileFailureKind::Conflict,
                "cannot delete 'extended': extended by 'child'",
            ));
        }
        let rendered = format!("-return {{ -- {name} (force={}) }}", options.force);
        Ok(ProfileLifecycleRendering {
            changed_keys: vec![format!("profiles.{name}")],
            candidate_digest: candidate_digest(rendered.as_bytes()),
            rendered,
            changed: true,
            replaces_existing: false,
            selector_updated: false,
            active: false,
        })
    }

    fn apply_profile_delete(
        &self,
        _plugin_id: &str,
        name: &str,
        options: &ProfileDeleteOptions,
        expected_candidate_digest: &str,
    ) -> Result<ProfileDeleteOutcome, ProfileWriteFailure> {
        let rendered = format!("-return {{ -- {name} (force={}) }}", options.force);
        if candidate_digest(rendered.as_bytes()) != expected_candidate_digest {
            return Err(ProfileWriteFailure::new(
                ProfileFailureKind::Stale,
                "the profile candidate changed since it was previewed",
            ));
        }
        Ok(ProfileDeleteOutcome {
            name: name.to_string(),
            backed_up: true,
            was_active: false,
        })
    }

    fn preview(
        &self,
        _plugin_id: &str,
        change: &ConfigChange,
    ) -> Result<ConfigPreviewRendering, ConfigWriteFailure> {
        Ok(ConfigPreviewRendering {
            changed_keys: vec![change.key()],
            rendered: change.rendered().to_string(),
            candidate_digest: candidate_digest(change.rendered().as_bytes()),
        })
    }

    fn apply(
        &self,
        _plugin_id: &str,
        change: &ConfigChange,
        expected_candidate_digest: &str,
    ) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
        self.calls.borrow_mut().push(change.clone());
        if self.stale {
            return Err(ConfigWriteFailure::Stale);
        }
        if candidate_digest(change.rendered().as_bytes()) != expected_candidate_digest {
            return Err(ConfigWriteFailure::Stale);
        }
        match self.failure {
            Some(failure) => Err(failure),
            // A backend that cannot classify the change reports
            // `pending_restart`, so the reply is `restart` (bitty#2972).
            None => Ok(ConfigApplyOutcome::Applied {
                pending_restart: true,
            }),
        }
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-config-manage-{tag}-{}-{counter}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(
    store_root: Option<PathBuf>,
    dev_root: Option<PathBuf>,
    backend: Option<Rc<RecordingBackend>>,
) -> PluginRuntime {
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(temp_dir("state")),
        store_root,
        bundled_roots: Vec::new(),
        third_party_roots: dev_root.into_iter().collect(),
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(NoSnapshot),
    });
    if let Some(backend) = backend {
        rt.set_config_write_backend(Some(backend));
    }
    // bitty#2781/bitty#2800: probe the config contract, not the wall clock.
    common::install_harness_seams(&mut rt);
    rt
}

/// Write a plugin package under `root/<id>/`.
fn write_plugin(root: &Path, id: &str, capabilities: &[&str], commands: &[&str], init_src: &str) {
    write_package_at(&root.join(id), id, capabilities, commands, init_src);
}

/// Write one plugin package into exactly `plugin`.
fn write_package_at(
    plugin: &Path,
    id: &str,
    capabilities: &[&str],
    commands: &[&str],
    init_src: &str,
) {
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let capabilities_toml = capabilities
        .iter()
        .map(|capability| format!("{capability} = true\n"))
        .collect::<String>();
    let qualified: Vec<String> = commands
        .iter()
        .map(|command| format!("\"{id}:{command}\""))
        .collect();
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Test"
version = "0.1.0"
description = "test"

[compat]
plugin-api = "^1.0"

[capabilities]
{capabilities_toml}
[lazy]
commands = [{commands}]
"#,
            commands = qualified.join(", "),
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");
}

/// Build an installed (`Registry`) store record whose recorded grant carries
/// `config.manage`, then write the store index.
fn write_store_package(
    store: &Path,
    id: &str,
    capabilities: &[&str],
    commands: &[&str],
    init_src: &str,
) -> PluginRecord {
    let version = "0.1.0";
    let relative = format!("packages/{id}/{version}");
    let package_root = store.join(&relative);
    write_package_at(&package_root, id, capabilities, commands, init_src);
    let manifest_bytes =
        std::fs::read(package_root.join("bitty-plugin.toml")).expect("manifest bytes");
    let manifest = manifest_toml::parse_manifest(&manifest_bytes).expect("manifest parses");
    PluginRecord {
        source_class: SourceClass::Registry,
        plugin_id: id.to_string(),
        version: version.to_string(),
        root: relative,
        manifest_hash: manifest.manifest_hash(),
        content_digest: content_digest(&package_root).expect("content digest"),
        enabled: true,
        granted: vec!["config.manage".to_string()],
    }
}

fn pid(id: &str) -> PluginId {
    PluginId::new(id).expect("id")
}

const PROBE: &str = r##"
local function errcode(f)
  local ok, result = pcall(f)
  return ok and "OK" or "ERR:" .. result.code
end
local function preview()
  local ok, p = pcall(bitty.config.preview, "appearance.theme", "catppuccin")
  if not ok then return nil, "ERR:" .. p.code end
  return p
end
bitty.commands.register({ id = "preview", title = "Preview", run = function()
  local p, err = preview()
  if not p then return err end
  return p.rendered .. "|" .. table.concat(p.keys, ",")
end })
bitty.commands.register({ id = "apply", title = "Apply", run = function()
  local p, err = preview()
  if not p then return err end
  local ok, out = pcall(bitty.config.set, "appearance.theme", "catppuccin", p.digest)
  if not ok then return "ERR:" .. out.code end
  return tostring(out.applied) .. "|" .. out.reload
end })
bitty.commands.register({ id = "replay", title = "Replay", run = function()
  local p, err = preview()
  if not p then return err end
  local ok, out = pcall(bitty.config.set, "appearance.theme", "catppuccin", p.digest)
  if not ok then return "ERR:" .. out.code end
  local again_ok, again = pcall(bitty.config.set, "appearance.theme", "catppuccin", p.digest)
  if again_ok then return "REPLAYED" end
  return again.code
end })
bitty.commands.register({ id = "apply_digest", title = "Apply digest", run = function(digest)
  local ok, out = pcall(bitty.config.set, "appearance.theme", "catppuccin", digest)
  if not ok then return "ERR:" .. out.code end
  return tostring(out.applied) .. "|" .. out.reload
end })
bitty.commands.register({ id = "digest", title = "Digest", run = function()
  local p, err = preview()
  if not p then return err end
  return p.digest
end })
bitty.commands.register({ id = "changed", title = "Changed", run = function()
  local p, err = preview()
  if not p then return err end
  local ok, out = pcall(bitty.config.set, "appearance.theme", "dracula", p.digest)
  if ok then return "APPLIED" end
  return out.code
end })
bitty.commands.register({ id = "bad", title = "Bad shapes", run = function()
  return table.concat({
    errcode(function() return bitty.config.preview("appearance.theme", "x", "extra") end),
    errcode(function() return bitty.config.set("appearance.theme", "x") end),
    errcode(function() return bitty.config.set("appearance.theme", "x", "d", "extra") end),
    errcode(function() return bitty.config.preview(1, "x") end),
    errcode(function() return bitty.config.preview("appearance.theme", {}) end),
    errcode(function() return bitty.config.preview("appearance.theme", true) end),
    errcode(function() return bitty.config.preview("appearance.theme", "line\nbreak") end),
    errcode(function() return bitty.config.set("appearance.theme", "catppuccin", "deadbeef") end),
  }, ",")
end })

bitty.commands.register({ id = "get", title = "Get", run = function()
  local ok, row = pcall(bitty.config.get, "appearance.theme")
  if not ok then return "ERR:" .. row.code end
  return row.key .. "|" .. row.value .. "|" .. row.kind .. "|" .. row.source .. "|" .. tostring(row.redacted)
end })
bitty.commands.register({ id = "sources", title = "Sources", run = function()
  local ok, rows = pcall(bitty.config.sources)
  if not ok then return "ERR:" .. rows.code end
  return rows.count .. "|" .. rows.rows[1].key .. "|" .. rows.rows[1].source .. "|" .. tostring(rows.truncated)
end })
bitty.commands.register({ id = "plugins", title = "Plugins", run = function()
  local ok, rows = pcall(bitty.plugins.list)
  if not ok then return "ERR:" .. rows.code end
  return rows.count .. "|" .. rows.rows[1].id .. "|" .. tostring(rows.rows[1].enabled) .. "|" .. tostring(rows.rows[1].managed) .. "|" .. rows.rows[1].source .. "|" .. rows.rows[1].version
end })
bitty.commands.register({ id = "profiles", title = "Profiles", run = function()
  local ok, rows = pcall(bitty.profiles.list)
  if not ok then return "ERR:" .. rows.code end
  return rows.active .. "|" .. rows.count .. "|" .. rows.rows[1].name .. "|" .. tostring(rows.rows[1].active)
end })
bitty.commands.register({ id = "enable", title = "Enable", run = function()
  local ok, p = pcall(bitty.config.preview_enabled, "owner.plugin", true)
  if not ok then return "ERR:" .. p.code end
  local applied, out = pcall(bitty.config.set_enabled, "owner.plugin", true, p.digest)
  if not applied then return "ERR:" .. out.code end
  return p.rendered .. "|" .. p.keys[1] .. "|" .. tostring(p.changed) .. "|" .. tostring(out.changed) .. "|" .. out.reload
end })
bitty.commands.register({ id = "enable_replay", title = "Enable replay", run = function()
  local ok, p = pcall(bitty.config.preview_enabled, "owner.plugin", true)
  if not ok then return "ERR:" .. p.code end
  pcall(bitty.config.set_enabled, "owner.plugin", true, p.digest)
  local again_ok, again = pcall(bitty.config.set_enabled, "owner.plugin", true, p.digest)
  if again_ok then return "REPLAYED" end
  return again.code
end })
bitty.commands.register({ id = "use", title = "Use", run = function()
  local ok, p = pcall(bitty.profiles.preview_use, "work")
  if not ok then return "ERR:" .. p.code end
  local applied, out = pcall(bitty.profiles.use, "work", p.digest)
  if not applied then return "ERR:" .. out.code end
  return p.rendered .. "|" .. p.keys[1] .. "|" .. out.active .. "|" .. tostring(out.changed) .. "|" .. tostring(out.pending_restart)
end })
bitty.commands.register({ id = "use_replay", title = "Use replay", run = function()
  local ok, p = pcall(bitty.profiles.preview_use, "work")
  if not ok then return "ERR:" .. p.code end
  pcall(bitty.profiles.use, "work", p.digest)
  local again_ok, again = pcall(bitty.profiles.use, "work", p.digest)
  if again_ok then return "REPLAYED" end
  return again.code
end })
bitty.commands.register({ id = "deny", title = "Deny", run = function()
  return table.concat({
    errcode(function() return bitty.config.get("appearance.theme") end),
    errcode(function() return bitty.config.sources() end),
    errcode(function() return bitty.plugins.list() end),
    errcode(function() return bitty.profiles.list() end),
    errcode(function() return bitty.config.preview_enabled("owner.plugin", true) end),
    errcode(function() return bitty.profiles.preview_use("work") end),
    errcode(function() return bitty.profiles.preview_create("fresh") end),
    errcode(function() return bitty.profiles.create("fresh", string.rep("0", 64)) end),
    errcode(function() return bitty.profiles.preview_clone("work", "copy") end),
    errcode(function() return bitty.profiles.clone("work", "copy", string.rep("0", 64)) end),
    errcode(function() return bitty.profiles.preview_rename("work", "work2") end),
    errcode(function() return bitty.profiles.rename("work", "work2", string.rep("0", 64)) end),
    errcode(function() return bitty.profiles.preview_delete("work") end),
    errcode(function() return bitty.profiles.delete("work", string.rep("0", 64)) end),
  }, ",")
end })

bitty.commands.register({ id = "profile_lifecycle", title = "Profile lifecycle", run = function()
  assert(bitty.config.profiles == bitty.profiles, "bitty.config.profiles must alias bitty.profiles")
  local listed = bitty.config.profiles.list()
  assert(#listed.rows == 2 and #listed.profiles == 2, "list exposes both rows and profiles")

  -- 1. Create with preset + embedded opts.digest
  local pc = bitty.profiles.preview_create("fresh", { preset = "dark", force = true })
  assert(#pc.digest == 64 and pc.changed and pc.replaces_existing)
  local c = bitty.profiles.create("fresh", { preset = "dark", force = true, digest = pc.digest })
  assert(c.name == "fresh" and c.created and c.replaced and c.backed_up and c.preset == "dark")

  -- 2. Clone with trailing digest
  local pcl = bitty.config.profiles.preview_clone("work", "work-copy")
  local cl = bitty.config.profiles.clone("work", "work-copy", pcl.digest)
  assert(cl.source == "work" and cl.name == "work-copy" and cl.cloned)

  -- 3. Rename with trailing digest
  local prn = bitty.profiles.preview_rename("work", "work-next", { force = true })
  assert(prn.selector_updated)
  local rn = bitty.profiles.rename("work", "work-next", { force = true }, prn.digest)
  assert(rn.old_name == "work" and rn.new_name == "work-next" and rn.selector_updated and rn.reload == "auto")

  -- 4. Delete with trailing digest
  local pdl = bitty.profiles.preview_delete("work-next")
  local dl = bitty.profiles.delete("work-next", pdl.digest)
  assert(dl.name == "work-next" and dl.deleted and dl.backed_up)

  return table.concat({ c.name, cl.name, rn.new_name, dl.name }, "|")
end })

bitty.commands.register({ id = "profile_refusals", title = "Profile refusals", run = function()
  local pc = bitty.profiles.preview_create("once")
  local first_ok = pcall(bitty.profiles.create, "once", pc.digest)
  assert(first_ok, "first create succeeds")
  local replay_code = errcode(function() return bitty.profiles.create("once", pc.digest) end)

  local pc2 = bitty.profiles.preview_create("mismatch")
  local mismatch_code = errcode(function()
    return bitty.profiles.create("mismatch", { force = true }, pc2.digest)
  end)

  local no_digest = errcode(function() return bitty.profiles.create("nodigest") end)
  local reserved_create = errcode(function() return bitty.profiles.preview_create("default") end)
  local reserved_clone = errcode(function() return bitty.profiles.preview_clone("work", "default") end)
  local reserved_rename = errcode(function() return bitty.profiles.preview_rename("default", "other") end)
  local reserved_delete = errcode(function() return bitty.profiles.preview_delete("default") end)
  local extended_delete = errcode(function() return bitty.profiles.preview_delete("extended") end)

  -- Path traversal and invalid profile names must fail closed without leaking paths
  for _, bad in ipairs({ "../evil", "../../etc/passwd", "a/b", "a\\b", "UpperCase", "-dash" }) do
    local ok, err = pcall(bitty.profiles.preview_create, bad)
    assert(not ok and err.code == "E_PROFILE_INVALID", "bad create name: " .. bad)
    assert(not err.message:find("/", 1, true) and not err.message:find("\\", 1, true), "path leak: " .. err.message)
  end
  for _, bad in ipairs({ "../evil", "a/b", "a\\b" }) do
    local ok, err = pcall(bitty.profiles.preview_delete, bad)
    assert(not ok and err.code == "E_PROFILE_INVALID", "bad delete name: " .. bad)
    assert(not err.message:find("/", 1, true) and not err.message:find("\\", 1, true), "path leak: " .. err.message)
  end

  return table.concat({
    replay_code,
    mismatch_code,
    no_digest,
    reserved_create,
    reserved_clone,
    reserved_rename,
    reserved_delete,
    extended_delete,
  }, ",")
end })

bitty.commands.register({ id = "keys", title = "Bad keys", run = function()
  return table.concat({
    errcode(function() return bitty.config.preview("terminal.shell", "/bin/sh") end),
    errcode(function() return bitty.config.preview("/etc/bitty/init.lua", "x") end),
    errcode(function() return bitty.config.preview("../../init", "x") end),
    errcode(function() return bitty.config.preview("appearance", "x") end),
    errcode(function() return bitty.config.preview("history.capture.transcript", true) end),
    errcode(function() return bitty.config.get("history.capture.transcript") end),
    errcode(function() return bitty.config.get("history.capture.commands") end),
  }, ",")
end })
"##;

const PROBE_COMMANDS: &[&str] = &[
    "preview",
    "apply",
    "replay",
    "changed",
    "bad",
    "keys",
    "digest",
    "apply_digest",
    "get",
    "sources",
    "plugins",
    "profiles",
    "enable",
    "enable_replay",
    "use",
    "use_replay",
    "deny",
    "profile_lifecycle",
    "profile_refusals",
];

/// Granted store package: a recorded grant is the only way a plugin receives
/// `config.manage` (review B2).
fn granted_runtime() -> (PluginRuntime, Rc<RecordingBackend>) {
    let store = temp_dir("store");
    let record = write_store_package(
        &store,
        "xuepoo.configprobe",
        &["config.manage"],
        PROBE_COMMANDS,
        PROBE,
    );
    write_index(&store, &[record]).expect("store index");
    let backend = Rc::new(RecordingBackend::default());
    let mut rt = runtime(Some(store), None, Some(Rc::clone(&backend)));
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    (rt, backend)
}

/// Development package that declares the write head but has no recorded
/// grant: the head must be withheld (review B2).
fn dev_runtime_without_record() -> (PluginRuntime, PathBuf) {
    let root = temp_dir("dev");
    write_plugin(
        &root,
        "xuepoo.configprobe",
        &["config.manage"],
        PROBE_COMMANDS,
        PROBE,
    );
    let mut rt = runtime(None, Some(root.clone()), None);
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    (rt, root)
}

fn run(rt: &mut PluginRuntime, command: &str) -> String {
    // bitty#2800: settle a documented RC-1 park instead of requiring the
    // command to finish inside one wall window.
    common::settle_dispatch(rt, &pid("xuepoo.configprobe"), command, &[]).expect_string(command)
}

#[test]
fn dev_root_declaration_without_a_record_gets_no_write_grant() {
    // A `BITTY_PLUGIN_DIR` package declaring `config.manage` still activates,
    // but the head is withheld and every write fails closed.
    let (mut rt, root) = dev_runtime_without_record();
    assert_eq!(run(&mut rt, "preview"), "ERR:E_CAPABILITY_DENIED");
    assert_eq!(run(&mut rt, "apply"), "ERR:E_CAPABILITY_DENIED");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn writes_are_denied_without_the_config_manage_grant() {
    // A package with no declared capability at all: the namespace is present
    // but every call fails closed, so a read-only dashboard cannot write.
    let root = temp_dir("dev-none");
    write_plugin(&root, "xuepoo.configprobe", &[], PROBE_COMMANDS, PROBE);
    let mut rt = runtime(None, Some(root.clone()), None);
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    assert_eq!(run(&mut rt, "preview"), "ERR:E_CAPABILITY_DENIED");
    assert_eq!(run(&mut rt, "apply"), "ERR:E_CAPABILITY_DENIED");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn preview_and_apply_carry_the_exact_rendered_change() {
    let (mut rt, backend) = granted_runtime();
    assert_eq!(
        run(&mut rt, "preview"),
        "appearance.theme = \"catppuccin\"|appearance.theme",
        "the preview renders the candidate and lists the changed keys"
    );
    assert!(backend.calls.borrow().is_empty(), "preview never writes");
    assert_eq!(
        run(&mut rt, "apply"),
        "true|restart",
        "an unclassified backend never claims a live adoption"
    );
    let calls = backend.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].key(), "appearance.theme");
    assert_eq!(calls[0].rendered(), "appearance.theme = \"catppuccin\"");
}

#[test]
fn approval_tokens_are_one_shot_and_value_bound() {
    let (mut rt, backend) = granted_runtime();
    assert_eq!(run(&mut rt, "replay"), "E_CONFIG_APPROVAL_REQUIRED");
    assert_eq!(run(&mut rt, "changed"), "E_CONFIG_APPROVAL_REQUIRED");
    assert_eq!(
        backend.calls.borrow().len(),
        1,
        "only the first apply wrote"
    );
}

#[test]
fn approval_tokens_are_generation_bound() {
    // CTX-1613: the approval table lives on the generation. A reload drops
    // the old table, so a token minted before it can never authorize a write
    // in the new generation even though the rendered change is identical.
    let (mut rt, backend) = granted_runtime();
    let id = pid("xuepoo.configprobe");
    let digest = run(&mut rt, "digest");
    assert_eq!(digest.len(), 64, "preview issued a token: {digest}");
    assert!(backend.calls.borrow().is_empty(), "no write yet");

    rt.reload(&id).expect("reload the generation");
    let replay = common::settle_dispatch(&mut rt, &id, "apply_digest", &[LuaValue::String(digest)])
        .expect_string("apply_digest");
    assert_eq!(
        replay, "ERR:E_CONFIG_APPROVAL_REQUIRED",
        "a pre-reload token never authorizes the new generation"
    );
    assert!(
        backend.calls.borrow().is_empty(),
        "the stale token reached no backend"
    );

    // The reloaded generation mints and applies its own token normally.
    assert_eq!(run(&mut rt, "apply"), "true|restart");
    assert_eq!(backend.calls.borrow().len(), 1);
}

#[test]
fn bridge_rejects_extra_arguments_and_non_scalar_values() {
    let (mut rt, backend) = granted_runtime();
    assert_eq!(
        run(&mut rt, "bad"),
        "ERR:E_DEF_INVALID,ERR:E_CONFIG_APPROVAL_MISMATCH,ERR:E_DEF_INVALID,ERR:E_CONFIG_KEY,\
ERR:E_CONFIG_VALUE,ERR:E_CONFIG_VALUE,ERR:E_CONFIG_VALUE,ERR:E_CONFIG_APPROVAL_MISMATCH"
    );
    assert!(backend.calls.borrow().is_empty(), "no malformed call wrote");
}

#[test]
fn no_path_or_command_key_is_addressable() {
    let (mut rt, backend) = granted_runtime();
    assert_eq!(
        run(&mut rt, "keys"),
        "ERR:E_CONFIG_KEY,ERR:E_CONFIG_KEY,ERR:E_CONFIG_KEY,ERR:E_CONFIG_KEY,ERR:E_CONFIG_KEY,\
ERR:E_CONFIG_KEY,ERR:E_CONFIG_KEY"
    );
    assert!(
        backend.calls.borrow().is_empty(),
        "no path-shaped key wrote"
    );
}

#[test]
fn granted_generation_without_a_backend_fails_closed() {
    let store = temp_dir("unwired");
    let record = write_store_package(
        &store,
        "xuepoo.configprobe",
        &["config.manage"],
        PROBE_COMMANDS,
        PROBE,
    );
    write_index(&store, &[record]).expect("store index");
    let mut rt = runtime(Some(store), None, None);
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    assert_eq!(
        run(&mut rt, "preview"),
        "ERR:E_CONFIG_UNAVAILABLE",
        "no writable layer is a typed unavailable, not a generic not-implemented"
    );
    assert_eq!(run(&mut rt, "apply"), "ERR:E_CONFIG_UNAVAILABLE");
}

#[test]
fn read_verbs_cross_the_wire_with_logical_sources() {
    let (mut rt, _backend) = granted_runtime();
    assert_eq!(
        run(&mut rt, "get"),
        "appearance.theme|catppuccin|text|user|false"
    );
    assert_eq!(run(&mut rt, "sources"), "2|appearance.theme|user|false");
    assert_eq!(
        run(&mut rt, "plugins"),
        "1|owner.plugin|true|true|bundled|1.0.0"
    );
    assert_eq!(run(&mut rt, "profiles"), "default|2|default|true");
}

#[test]
fn enable_and_profile_use_are_token_bound_over_the_wire() {
    let (mut rt, _backend) = granted_runtime();
    assert_eq!(
        run(&mut rt, "enable"),
        "plugins.owner.plugin.enabled = true|plugins.owner.plugin.enabled|true|true|restart"
    );
    assert_eq!(run(&mut rt, "enable_replay"), "E_CONFIG_APPROVAL_REQUIRED");
    assert_eq!(
        run(&mut rt, "use"),
        "profile = \"work\"|profile|work|true|false",
        "bitty#2814: a live-class switch reports no pending restart"
    );
    assert_eq!(run(&mut rt, "use_replay"), "E_CONFIG_APPROVAL_REQUIRED");
}

#[test]
fn ungranted_generation_denies_every_new_verb() {
    let root = temp_dir("dev-none-manage");
    write_plugin(&root, "xuepoo.configprobe", &[], PROBE_COMMANDS, PROBE);
    let mut rt = runtime(None, Some(root.clone()), None);
    rt.discover();
    for (id, result) in rt.activate_discovered() {
        result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
    }
    assert_eq!(
        run(&mut rt, "deny"),
        "ERR:E_CAPABILITY_DENIED,ERR:E_CAPABILITY_DENIED,ERR:E_CAPABILITY_DENIED,\
ERR:E_CAPABILITY_DENIED,ERR:E_CAPABILITY_DENIED,ERR:E_CAPABILITY_DENIED,\
ERR:E_CAPABILITY_DENIED,ERR:E_CAPABILITY_DENIED,ERR:E_CAPABILITY_DENIED,\
ERR:E_CAPABILITY_DENIED,ERR:E_CAPABILITY_DENIED,ERR:E_CAPABILITY_DENIED,\
ERR:E_CAPABILITY_DENIED,ERR:E_CAPABILITY_DENIED"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn profile_lifecycle_verbs_are_token_bound_and_refuse_invalid_or_reserved_targets() {
    let (mut rt, _backend) = granted_runtime();
    assert_eq!(
        run(&mut rt, "profile_lifecycle"),
        "fresh|work-copy|work-next|work-next"
    );
    assert_eq!(
        run(&mut rt, "profile_refusals"),
        "ERR:E_CONFIG_APPROVAL_REQUIRED,ERR:E_CONFIG_APPROVAL_REQUIRED,\
ERR:E_CONFIG_APPROVAL_REQUIRED,ERR:E_PROFILE_CONFLICT,ERR:E_PROFILE_CONFLICT,\
ERR:E_PROFILE_CONFLICT,ERR:E_PROFILE_CONFLICT,ERR:E_PROFILE_CONFLICT"
    );
}

/// bitty#2972: a committed `config.manage` write is host I/O the plugin neither
/// performs nor controls, so its latency is credited against the calling
/// callback's RC-1 wall limit (see `CONFIG_COMMIT_CREDIT_MAX_MS`).
///
/// Before the credit, a write slower than the per-tick budget plus any
/// post-apply Lua work suspended the generation at the next slice boundary.
/// The next cold timer drive then terminally failed it (`fail_runtime` →
/// `rollback`), which clears its UI blocks and releases its overlay capture:
/// the panel that made the write vanished and the confirmation row was never
/// shown. These tests pin the credit, the restart classification, and the
/// refusal path against a real overlay-holding generation.
mod overlay_write_credit {
    use super::*;
    use bitty_runtime::plugin_runtime::{
        ConfigReadFailure, ConfigSourceLabel, ConfigValueRow, LifecycleState,
    };

    /// Write backend whose durable replace is slow enough to exceed the RC-1
    /// per-tick wall budget, like an fsync on a busy disk.
    struct SlowWriteBackend {
        delay_ms: u64,
        pending_restart: bool,
        stale: bool,
        calls: RefCell<Vec<ConfigChange>>,
    }

    impl SlowWriteBackend {
        fn new(delay_ms: u64, pending_restart: bool, stale: bool) -> Self {
            Self {
                delay_ms,
                pending_restart,
                stale,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl ConfigWriteBackend for SlowWriteBackend {
        fn config_get(
            &self,
            _plugin_id: &str,
            key: &str,
        ) -> Result<ConfigValueRow, ConfigReadFailure> {
            Ok(ConfigValueRow {
                key: key.to_string(),
                value: ConfigReadValue::Text(String::from("catppuccin")),
                source: ConfigSourceLabel::User,
                redacted: false,
            })
        }

        fn preview(
            &self,
            _plugin_id: &str,
            change: &ConfigChange,
        ) -> Result<ConfigPreviewRendering, ConfigWriteFailure> {
            Ok(ConfigPreviewRendering {
                changed_keys: vec![change.key()],
                rendered: change.rendered().to_string(),
                candidate_digest: candidate_digest(change.rendered().as_bytes()),
            })
        }

        fn apply(
            &self,
            _plugin_id: &str,
            change: &ConfigChange,
            expected_candidate_digest: &str,
        ) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
            self.calls.borrow_mut().push(change.clone());
            if self.stale {
                return Err(ConfigWriteFailure::Stale);
            }
            if candidate_digest(change.rendered().as_bytes()) != expected_candidate_digest {
                return Err(ConfigWriteFailure::Stale);
            }
            Ok(ConfigApplyOutcome::Applied {
                pending_restart: self.pending_restart,
            })
        }

        fn preview_plugin_enabled(
            &self,
            _plugin_id: &str,
            id: &str,
            enabled: bool,
        ) -> Result<PluginEnabledRendering, ConfigWriteFailure> {
            let rendered = format!("plugins.{id}.enabled = {enabled}");
            Ok(PluginEnabledRendering {
                changed_keys: vec![format!("plugins.{id}.enabled")],
                candidate_digest: candidate_digest(rendered.as_bytes()),
                rendered,
            })
        }

        fn apply_plugin_enabled(
            &self,
            _plugin_id: &str,
            id: &str,
            enabled: bool,
            expected_candidate_digest: &str,
        ) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
            let rendered = format!("plugins.{id}.enabled = {enabled}");
            if candidate_digest(rendered.as_bytes()) != expected_candidate_digest {
                return Err(ConfigWriteFailure::Stale);
            }
            Ok(ConfigApplyOutcome::Applied {
                pending_restart: true,
            })
        }
    }

    const OVERLAY_ID: &str = "xuepoo.overlaywriter";

    /// Dashboard-shaped probe: hold a focusable overlay, apply a change from
    /// the capture-input handler, then do post-apply Lua work and keep a
    /// one-shot timer armed (the next cold timer drive is what turns a
    /// suspended VM into a failed generation).
    const OVERLAY_PROBE: &str = r##"
    local handle = nil
    local open_ok = pcall(function()
      handle = bitty.ui.overlay.acquire({ title = "Settings", placeholder = "Type" })
    end)
    bitty.store.set("open_ok", open_ok)
    bitty.store.set("handle", handle or -1)

    local function apply()
      local ok, p = pcall(bitty.config.preview, "appearance.theme", "catppuccin")
      if not ok then return "ERR:" .. p.code end
      local applied, out = pcall(bitty.config.set, "appearance.theme", "catppuccin", p.digest)
      if not applied then return "ERR:" .. out.code end
      return tostring(out.applied) .. "|" .. out.reload
    end

    bitty.commands.register({ id = "apply", title = "Apply", run = apply })
    bitty.commands.register({ id = "enable", title = "Enable", run = function()
      local ok, p = pcall(bitty.config.preview_enabled, "owner.plugin", true)
      if not ok then return "ERR:" .. p.code end
      local applied, out = pcall(bitty.config.set_enabled, "owner.plugin", true, p.digest)
      if not applied then return "ERR:" .. out.code end
      return tostring(out.changed) .. "|" .. out.reload
    end })
    bitty.commands.register({ id = "replay", title = "Replay", run = function()
      local ok, p = pcall(bitty.config.preview, "appearance.theme", "catppuccin")
      if not ok then return "ERR:" .. p.code end
      local first, out = pcall(bitty.config.set, "appearance.theme", "catppuccin", p.digest)
      if not first then return "ERR:" .. out.code end
      local again_ok, again = pcall(bitty.config.set, "appearance.theme", "catppuccin", p.digest)
      if again_ok then return "REPLAYED" end
      return again.code
    end })

    bitty.events.subscribe("overlay.input.available", function(_event)
      bitty.store.set("event_result", apply())
      -- Post-apply work the dashboard also does (rebuild rows, render, paint):
      -- this is what reaches a slice boundary after the write.
      local total = 0
      for i = 1, 20000 do
        total = total + i
      end
      bitty.store.set("post_work", total)
    end)

    bitty.timers.create(10, function()
      bitty.store.set("timer_fired", true)
    end)
    return {}
    "##;

    const OVERLAY_COMMANDS: &[&str] = &["apply", "enable", "replay"];

    fn write_overlay_package(store: &Path, id: &str) -> PluginRecord {
        let version = "0.1.0";
        let relative = format!("packages/{id}/{version}");
        let package_root = store.join(&relative);
        write_package_at(
            &package_root,
            id,
            &["config.manage", "ui.overlay.focus"],
            OVERLAY_COMMANDS,
            OVERLAY_PROBE,
        );
        // The probe subscribes to the capture nudge during activation.
        let manifest_path = package_root.join("bitty-plugin.toml");
        let manifest = std::fs::read_to_string(&manifest_path).expect("manifest");
        std::fs::write(
            &manifest_path,
            format!("{manifest}events = [\"overlay.input.available\"]\n"),
        )
        .expect("manifest events");
        let manifest_bytes = std::fs::read(&manifest_path).expect("manifest bytes");
        let manifest = manifest_toml::parse_manifest(&manifest_bytes).expect("manifest parses");
        PluginRecord {
            source_class: SourceClass::Registry,
            plugin_id: id.to_string(),
            version: version.to_string(),
            root: relative,
            manifest_hash: manifest.manifest_hash(),
            content_digest: content_digest(&package_root).expect("content digest"),
            enabled: true,
            granted: vec![
                String::from("config.manage"),
                String::from("ui.overlay.focus"),
            ],
        }
    }

    /// Production RC-1 budgets: the regression is about the real per-tick
    /// wall limit, so the harness seams (which widen it to the callback
    /// ceiling) must not be installed.
    fn overlay_runtime(backend: Rc<SlowWriteBackend>) -> (PluginRuntime, PluginId) {
        let store = temp_dir("overlay-credit-store");
        let record = write_overlay_package(&store, OVERLAY_ID);
        write_index(&store, &[record]).expect("store index");
        let mut rt = PluginRuntime::new(PluginRuntimeConfig {
            safe_mode: false,
            data_dir: Some(temp_dir("overlay-credit-state")),
            store_root: Some(store),
            bundled_roots: Vec::new(),
            third_party_roots: Vec::new(),
            settings: Rc::new(EmptySettings),
            snapshot: Rc::new(NoSnapshot),
        });
        rt.set_config_write_backend(Some(backend));
        common::install_stub_backend(&mut rt);
        rt.discover();
        for (id, result) in rt.activate_discovered() {
            result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
        }
        let id = pid(OVERLAY_ID);
        assert_eq!(
            rt.state(&id),
            Some(&LifecycleState::Active),
            "the probe must activate"
        );
        (rt, id)
    }

    fn store_string(rt: &PluginRuntime, id: &PluginId, key: &str) -> Option<String> {
        rt.services(id)
            .and_then(|services| services.with_store(|store| store.get(key)))
            .and_then(|value| match value {
                LuaValue::String(text) => Some(text),
                _ => None,
            })
    }

    #[test]
    fn a_slow_value_write_from_an_overlay_keeps_the_overlay_and_the_generation() {
        // The durable write takes 60 ms, past the 50 ms per-tick budget, and
        // the handler then does dashboard-shaped post-work. Before the
        // config-write credit this suspended the VM; the timer pump below
        // then terminally failed the generation and released the capture.
        let backend = Rc::new(SlowWriteBackend::new(60, false, false));
        let (mut rt, id) = overlay_runtime(Rc::clone(&backend));
        assert!(
            rt.overlay_capture().borrow().is_active(),
            "init.lua holds the capture"
        );

        let delivered = rt.deliver_event(
            "overlay.input.available",
            &LuaValue::table([("owner", LuaValue::String(OVERLAY_ID.to_string()))]),
        );
        assert_eq!(delivered, 1, "the capture handler ran");
        assert_eq!(
            store_string(&rt, &id, "event_result").as_deref(),
            Some("true|auto"),
            "a live-adoptable value reports the automatic reload"
        );
        assert!(
            rt.overlay_capture().borrow().is_active(),
            "the write must not release the overlay capture"
        );
        assert_eq!(
            rt.state(&id),
            Some(&LifecycleState::Active),
            "the committed write must not suspend the generation"
        );

        // The dashboard keeps a re-arm timer; its next cold drive is the
        // place a suspended VM would be terminally failed.
        let fired =
            rt.pump_due_timers_at(std::time::Instant::now() + std::time::Duration::from_millis(50));
        assert_eq!(fired, 1, "the keep-alive timer fired");
        assert_eq!(
            rt.state(&id),
            Some(&LifecycleState::Active),
            "the generation survives the timer drive"
        );
        assert!(
            rt.overlay_capture().borrow().is_active(),
            "the overlay still owns the capture after the reload tick"
        );
        assert_eq!(backend.calls.borrow().len(), 1, "exactly one write landed");
    }

    #[test]
    fn a_value_the_running_plan_cannot_adopt_reports_restart() {
        // bitty#2972: a persisted value with no live adopter must not be
        // reported as an `auto` adoption.
        let backend = Rc::new(SlowWriteBackend::new(0, true, false));
        let (mut rt, id) = overlay_runtime(Rc::clone(&backend));
        let out = common::settle_dispatch(&mut rt, &id, "apply", &[]).expect_string("apply");
        assert_eq!(out, "true|restart");
        assert!(
            rt.overlay_capture().borrow().is_active(),
            "a restart-required write still leaves the panel open"
        );
        assert_eq!(rt.state(&id), Some(&LifecycleState::Active));
    }

    #[test]
    fn a_plugin_set_write_reports_restart_and_keeps_the_overlay() {
        let backend = Rc::new(SlowWriteBackend::new(0, false, false));
        let (mut rt, id) = overlay_runtime(Rc::clone(&backend));
        let out = common::settle_dispatch(&mut rt, &id, "enable", &[]).expect_string("enable");
        assert_eq!(
            out, "true|restart",
            "the ledger is read at activation, so an applied toggle needs a restart"
        );
        assert!(
            rt.overlay_capture().borrow().is_active(),
            "the panel survives a plugin-set write"
        );
    }

    #[test]
    fn a_refused_write_and_a_replayed_digest_keep_the_overlay_open() {
        // A stale candidate refuses with the typed error and no write.
        let backend = Rc::new(SlowWriteBackend::new(0, false, true));
        let (mut rt, id) = overlay_runtime(Rc::clone(&backend));
        let out = common::settle_dispatch(&mut rt, &id, "apply", &[]).expect_string("apply");
        assert_eq!(out, "ERR:E_CONFIG_STALE");
        assert!(
            rt.overlay_capture().borrow().is_active(),
            "a refused write must leave the overlay open with the typed error"
        );
        assert_eq!(rt.state(&id), Some(&LifecycleState::Active));

        // A replayed one-shot digest refuses with the approval error.
        let backend = Rc::new(SlowWriteBackend::new(0, false, false));
        let (mut rt, id) = overlay_runtime(Rc::clone(&backend));
        let out = common::settle_dispatch(&mut rt, &id, "replay", &[]).expect_string("replay");
        assert_eq!(out, "E_CONFIG_APPROVAL_REQUIRED");
        assert!(
            rt.overlay_capture().borrow().is_active(),
            "a replayed digest must leave the overlay open"
        );
        assert_eq!(rt.state(&id), Some(&LifecycleState::Active));
    }
}
