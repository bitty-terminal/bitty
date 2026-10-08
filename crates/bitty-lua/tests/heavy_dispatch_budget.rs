#![forbid(unsafe_code)]

//! RC-1 heavy-dispatch budget regression (bitty#1825).
//!
//! Follow-up triage for the git-panel headless report: `open`/`branch`
//! burned `~10.08M` instructions on trivially-small input against the real
//! host while light verbs fit under the `10M` per-callback budget.
//!
//! Host-side line-level tracing for this issue established:
//!
//! - The budget is per-callback, not cumulative: `drive_stashed` resets
//!   `instructions_used` on every `call_function`, and
//!   `PluginRuntime::dispatch_command` is a thin wrapper over it. The
//!   `+10/+88` drift across runs is fixture noise.
//! - There is no `~10M` fixed per-dispatch bridge overhead on current
//!   `main`: snapshot marshal, spawn delivery, and result marshal are host
//!   (Rust) work and consume no VM fuel. Measured verb-shaped costs on the
//!   default RC-1 budgets are `400-683` fuel on the empty store and
//!   `1515-9638` on small realistic outputs (heaviest: `open` at `9638`,
//!   i.e. under `0.1%` of budget).
//! - The `10.08M` burn does not reproduce on current `main` with any
//!   trivially-small input (empty, `fatal: not a git repository`, realistic
//!   multi-branch listings, lines up to 2000 chars). Pattern fuel scales
//!   linearly (`~18` fuel per scanned char for a failed search), so even an
//!   `8 KiB` worst-case bounded output stays under `~200k`.
//! - The current blocker for the real `open`/`branch` verbs is the
//!   string-pattern parity defect tracked by bitty#1826 (lazy quantifier
//!   plus `$` anchor, e.g. `^%s*(.-)%s*$`, yields `nil` under phodopus while
//!   PUC Lua trims). The workload below therefore uses only constructs
//!   verified against the current engine (greedy captures, `gsub` trims,
//!   plain finds, `gmatch` line iteration, closure sort) until #1826 lands.
//!
//! What this suite pins: the accepted bounded workload for spawn-bearing
//! dispatches (one terminal snapshot, at most two allowlisted `git` spawns,
//! `8 KiB` output cap with half-line drop, `MAX_BRANCHES = 32` /
//! `MAX_COMMITS = 64` / `MAX_ENTRIES = 128` bounds) completes within the
//! unchanged `RC1_INSTRUCTION_BUDGET` with at least `100x` margin, on a fresh
//! generation, in both heavy-first dispatch orders (runs A/C shape) and on
//! both the empty store and small realistic outputs. Any future fixed
//! per-dispatch overhead regression trips the ceiling long before it
//! threatens the real budget.

use std::collections::HashMap;
use std::rc::Rc;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{BridgeError, ExecuteOutcome, HostServices, LuaValue, LuaVm, MarshallingLimits};

/// Fuel ceiling for one heavy-verb dispatch: `100x` under the RC-1 budget.
///
/// Measured workload costs are `400-9638`; the ceiling leaves two orders of
/// magnitude of headroom for host variance while catching any `10M`-class
/// per-dispatch overhead regression with wide margin.
const HEAVY_VERB_FUEL_CEILING: u64 = 1_000_000;

/// Spawn-bearing heavy-verb workload shaped like the git-panel verbs.
///
/// One snapshot, at most two spawns, bounded parse/sort/truncate. Uses only
/// engine-verified constructs (see module docs re bitty#1826).
const HEAVY_WORKLOAD_INIT: &str = r#"
local MAX_BRANCHES = 32
local MAX_COMMITS = 64
local MAX_ENTRIES = 128
local MAX_SPAWN_OUTPUT_BYTES = 8192

local function trim(text)
  return string.gsub(string.gsub(text, "^%s+", ""), "%s+$", "")
end

local function snapshot_cwd(snap)
  local zones = snap.zones
  if type(zones) ~= "table" then
    return nil
  end
  for index = #zones, 1, -1 do
    local zone = zones[index]
    if type(zone) == "table" and type(zone.metadata) == "table" then
      local cwd = zone.metadata.cwd
      if type(cwd) == "string" and cwd ~= "" then
        return cwd
      end
    end
  end
  return nil
end

local function refresh_cache()
  local snap = bitty.terminal.snapshot({ scope = "semantic" })
  return snapshot_cwd(snap)
end

local function spawn_git(args)
  local result = bitty.process.spawn(args)
  local output = result.output
  if #output > MAX_SPAWN_OUTPUT_BYTES then
    output = string.sub(output, 1, MAX_SPAWN_OUTPUT_BYTES)
    local last_newline = string.find(output, "\n[^\n]*$")
    if last_newline == nil then
      output = ""
    else
      output = string.sub(output, 1, last_newline)
    end
  end
  return output
end

local function branch_list_from_output(output)
  local branches = {}
  local by_name = {}
  for line in string.gmatch(output or "", "[^\n]+") do
    local trimmed = trim(line)
    local is_current = string.sub(trimmed, 1, 1) == "*"
    local name = trimmed
    if is_current then
      name = trim(string.sub(trimmed, 2))
    end
    if type(name) == "string" and #name > 0 and #name <= 128 then
      if by_name[name] == nil then
        local entry = { name = name, is_current = is_current }
        by_name[name] = entry
        branches[#branches + 1] = entry
      end
    end
  end
  table.sort(branches, function(a, b)
    if a.name ~= b.name then
      return a.name < b.name
    end
    if a.is_current ~= b.is_current then
      return a.is_current
    end
    return false
  end)
  while #branches > MAX_BRANCHES do
    branches[#branches] = nil
  end
  return branches
end

local function status_entries_from_output(output)
  local entries = {}
  for line in string.gmatch(output or "", "[^\n]+") do
    local rest = string.gsub(string.match(line, "^..(.*)$") or "", "%s+$", "")
    if rest ~= nil and rest ~= "" then
      entries[#entries + 1] = { path = rest }
      if #entries >= MAX_ENTRIES then
        break
      end
    end
  end
  table.sort(entries, function(a, b)
    return a.path < b.path
  end)
  return entries
end

local function commit_list_from_output(output)
  local commits = {}
  for line in string.gmatch(output or "", "[^\n]+") do
    local hash = string.match(line, "^(%S+)%s*$")
    if hash == nil then
      hash = string.match(line, "^(%S+)%s")
    end
    if hash ~= nil then
      commits[#commits + 1] = { hash = hash }
      if #commits >= MAX_COMMITS then
        break
      end
    end
  end
  return commits
end

bitty.commands.register({
  id = "open",
  title = "heavy open",
  description = "two spawns plus both parses",
  run = function(_args)
    refresh_cache()
    local branches = branch_list_from_output(spawn_git({ "branch", "-a" }))
    local entries = status_entries_from_output(spawn_git({ "status", "--porcelain" }))
    return { branches = #branches, entries = #entries }
  end,
})

bitty.commands.register({
  id = "status",
  title = "heavy status",
  description = "one spawn plus status parse",
  run = function(_args)
    refresh_cache()
    return status_entries_from_output(spawn_git({ "status", "--porcelain" }))
  end,
})

bitty.commands.register({
  id = "diff",
  title = "heavy diff",
  description = "one spawn plus bounded line collect",
  run = function(_args)
    refresh_cache()
    local output = spawn_git({ "diff", "--stat" })
    local lines = {}
    for line in string.gmatch(output or "", "[^\n]+") do
      lines[#lines + 1] = line
      if #lines >= MAX_ENTRIES then
        break
      end
    end
    return lines
  end,
})

bitty.commands.register({
  id = "log",
  title = "heavy log",
  description = "one spawn plus commit parse",
  run = function(_args)
    refresh_cache()
    return commit_list_from_output(spawn_git({ "log", "--oneline", "-n", "10" }))
  end,
})

bitty.commands.register({
  id = "branch",
  title = "heavy branch",
  description = "one spawn plus branch parse and sort",
  run = function(_args)
    refresh_cache()
    return branch_list_from_output(spawn_git({ "branch", "-a" }))
  end,
})
"#;

/// Stub host services mimicking the headless self-test: stub snapshot with
/// one cwd zone, empty store, canned small spawn outputs keyed by argv.
struct HeavyStub {
    outputs: HashMap<String, String>,
}

impl HostServices for HeavyStub {
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
        Ok(LuaValue::table([
            ("title", LuaValue::String("scratch".to_string())),
            (
                "zones",
                LuaValue::array(vec![LuaValue::table([(
                    "metadata",
                    LuaValue::table([("cwd", LuaValue::String("/tmp/bitty/scratch".to_string()))]),
                )])]),
            ),
        ]))
    }

    fn notify_show(&self, _payload: &LuaValue) -> Result<bool, BridgeError> {
        Ok(true)
    }

    fn process_spawn(&self, args: &[String]) -> Result<LuaValue, BridgeError> {
        let key = args.join("\0");
        let output = self.outputs.get(&key).cloned().unwrap_or_default();
        Ok(LuaValue::table([
            ("output", LuaValue::String(output)),
            ("stderr", LuaValue::String(String::new())),
            ("truncated", LuaValue::Bool(false)),
            ("exit_code", LuaValue::Integer(128)),
            ("execution_id", LuaValue::Integer(1)),
            ("untrusted", LuaValue::Bool(true)),
        ]))
    }
}

fn heavy_vm(outputs: HashMap<String, String>) -> LuaVm {
    let mut vm =
        build_plugin_vm("xuepoo.heavy-1825", Some(VmBudgets::default())).expect("default budgets");
    let services: Rc<dyn HostServices> = Rc::new(HeavyStub { outputs });
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install");
    let outcome = vm.execute(HEAVY_WORKLOAD_INIT).expect("load workload");
    assert!(
        matches!(outcome, ExecuteOutcome::Completed { .. }),
        "workload activation must complete: {outcome:?}"
    );
    vm
}

fn dispatch_all(vm: &mut LuaVm, order: &[&str]) {
    let regs = vm.take_registrations();
    assert_eq!(regs.commands.len(), 5, "five heavy verbs registered");
    for id in order {
        let cmd = regs
            .commands
            .iter()
            .find(|command| command.id == *id)
            .unwrap_or_else(|| panic!("command {id} registered"));
        let value = vm
            .call_function(&cmd.run, &[])
            .unwrap_or_else(|error| panic!("verb {id} must complete: {error:?}"));
        let _ = value;
        assert!(
            !vm.is_suspended(),
            "verb {id} must not suspend the VM (used {})",
            vm.instructions_used()
        );
        assert!(
            vm.instructions_used() < HEAVY_VERB_FUEL_CEILING,
            "verb {id} used {} fuel, ceiling is {HEAVY_VERB_FUEL_CEILING}",
            vm.instructions_used()
        );
    }
}

fn empty_outputs() -> HashMap<String, String> {
    HashMap::new()
}

fn small_outputs() -> HashMap<String, String> {
    let mut outputs = HashMap::new();
    outputs.insert(
        "branch\0-a".to_string(),
        "* main\n  dev\n  feature/panel\n  release/0.1".to_string(),
    );
    outputs.insert(
        "status\0--porcelain".to_string(),
        "M  staged.lua\n M worktree.lua\n?? untracked.lua".to_string(),
    );
    outputs.insert(
        ["log", "--oneline", "-n", "10"].join("\0"),
        "abc1234 first subject\ndef5678 second subject".to_string(),
    );
    outputs.insert(
        "diff\0--stat".to_string(),
        " staged.lua | 2 +-\n worktree.lua | 1 +".to_string(),
    );
    outputs
}

#[test]
fn heavy_verbs_branch_first_complete_within_budget_on_empty_store() {
    // Run-C shape: branch first on a fresh generation, empty store.
    let mut vm = heavy_vm(empty_outputs());
    dispatch_all(&mut vm, &["branch", "open", "log", "status", "diff"]);
}

#[test]
fn heavy_verbs_open_first_complete_within_budget_on_empty_store() {
    // Run-A shape: open (two spawns) first on a fresh generation.
    let mut vm = heavy_vm(empty_outputs());
    dispatch_all(&mut vm, &["open", "branch", "status", "log", "diff"]);
}

#[test]
fn heavy_verbs_complete_within_budget_on_small_outputs() {
    // Same bounded workload with small realistic outputs (non-empty repo).
    let mut vm = heavy_vm(small_outputs());
    dispatch_all(&mut vm, &["branch", "open", "log", "status", "diff"]);
    let mut vm = heavy_vm(small_outputs());
    dispatch_all(&mut vm, &["open", "branch", "status", "log", "diff"]);
}
