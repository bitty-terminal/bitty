-- Entry point for the Composer (bitty-terminal.composer): modal command-line
-- policy over the PUBLIC host API only.
--
-- Accepted contracts: W-73 (composer boundary, bitty-docs) and W-82 (composer
-- architecture, bitty-terminal-docs). This file re-expresses the reviewed
-- Core-internal policy (`bitty-rich` composer.rs: CommandBuffer, key-chord
-- vocabulary, modal session) as extension-side presentation state. It never
-- mutates Terminal Truth: the buffer lives here, submission goes through
-- `bitty.terminal.submit` (the host frames the bracketed-paste bytes), the
-- editor round trip goes through `bitty.process.editor.start` (Core owns the
-- temp file and the child), and input arrives through the bounded transient
-- capture behind `bitty.ui.overlay.*` (the host owns acquisition, release,
-- and the input pipeline). No filesystem, process-spawn, network, clipboard,
-- or raw terminal-input authority is used; the editor temp path never leaves
-- Core.
--
-- The host evaluates this file once per plugin activation and owns every
-- resource created here for the lifetime of that generation. The code stays
-- inside the Lua 5.1 grammar (the `just lua` gate) and uses no `utf8` library:
-- all bounds are byte bounds and `#s` is a byte count.
--
-- Single documented over-limit rule (see README): over-limit input on ANY
-- path (typing, paste, editor content, submit framing) is rejected with the
-- previous buffer intact. Nothing is truncated, nothing is emitted.
--
-- The host never calls into the plugin on the input path (P0-AC-015). The
-- plugin drains already-queued capture events through `composer.pump()`, an
-- owner-initiated synchronous read the host tick (or the tests) invoke. Feed
-- while closed fails closed with NOT_OPEN: those bytes belong to the PTY and
-- the plugin touches nothing.

local M = {}

M.PLUGIN_ID = "bitty-terminal.composer"

-- Bounds mirrored from the W-82 limits table (Core `bitty-rich` composer.rs
-- evidence: COMPOSER_MAX_BYTES, EDITOR_TIMEOUT_DEFAULT/MAX). The numeric
-- enforcement lives in the host operations; the values are re-declared here
-- so the extension-side presentation state honors the same ceiling.
M.MAX_BYTES = 64 * 1024 -- edit-buffer and temp-file byte cap
M.SUBMIT_FRAME_OVERHEAD = 13 -- informational: ESC[200~ + ESC[201~ + CR
M.EDITOR_TIMEOUT_DEFAULT_MS = 120000 -- editor wait default (120 s)
M.EDITOR_TIMEOUT_MAX_MS = 300000 -- editor wait hard ceiling (300 s)
M.REQUIRED_API_MAJOR = 1 -- manifest declares plugin-api ^1.0; same gate here

-- In-modal key roles and their defaults (composer.rs ComposerKeys::default:
-- Enter newline, Ctrl+Enter submit, Esc close, Alt+E external editor).
M.DEFAULT_KEYS = {
  newline = "enter",
  submit = "ctrl+enter",
  close = "escape",
  external_editor = "alt+e",
}

-- Manual-open chord default (composer.rs COMPOSER_OPEN_CHORD). The open chord
-- lives on the host keymap (it targets the `open` command while the modal is
-- closed); the in-modal Alt+E role above applies only while the modal holds
-- capture, so the two never compete.
M.DEFAULT_OPEN_CHORD = "alt+e"

-- Not-open and disabled sentinels surfaced through the scalar queries below.
M.CODE_NOT_OPEN = "NOT_OPEN"
M.CODE_DISABLED = "E_DISABLED"

-- ---------------------------------------------------------------------------
-- Chord vocabulary (re-expressed from composer.rs ComposerChord)
-- ---------------------------------------------------------------------------

local MOD_ALIASES = {
  ctrl = "ctrl",
  control = "ctrl",
  alt = "alt",
  opt = "alt",
  option = "alt",
  shift = "shift",
  super = "super",
  meta = "super",
  cmd = "super",
  command = "super",
  win = "super",
  windows = "super",
}

local KEY_ALIASES = {
  enter = "enter",
  ["return"] = "enter",
  escape = "escape",
  esc = "escape",
}

local MOD_ORDER = { "ctrl", "alt", "shift", "super" }

local function trim(s)
  return string.match(s, "^%s*(.-)%s*$")
end

local function is_single_graphic(token)
  return #token == 1 and string.byte(token) >= 33 and string.byte(token) <= 126
end

-- Parses one in-modal role chord: `Enter`, `Ctrl+Enter`, `Shift+Enter`,
-- `Esc`/`Escape`, `Alt+E`, or `<mod>+<char>`. Bare single characters are
-- rejected (they would shadow shell typing; typing arrives as text events,
-- never as chords). Returns a chord table or nil plus a reason.
function M.parse_chord(raw)
  if type(raw) ~= "string" then
    return nil, "chord must be a string"
  end
  local text = trim(string.lower(raw))
  if text == "" then
    return nil, "chord must not be empty"
  end
  if #text > 64 then
    return nil, "chord exceeds 64 bytes"
  end
  local mods = {}
  local key = nil
  -- Split on '+'; string.gmatch with "[^+]+" skips empty runs, so validate
  -- the join round-trips (rejects "ctrl++e", "+e", "e+").
  local parts = {}
  for part in string.gmatch(text, "([^%+]+)") do
    parts[#parts + 1] = trim(part)
  end
  if #parts == 0 then
    return nil, "unknown chord '" .. raw .. "'"
  end
  local rejoined = table.concat(parts, "+")
  -- Allow whitespace around '+' (trimmed above); anything else is malformed.
  local nospace = string.gsub(text, "%s+", "")
  if rejoined ~= nospace then
    return nil, "unknown chord '" .. raw .. "'"
  end
  for _, token in ipairs(parts) do
    if token == "" then
      return nil, "unknown chord '" .. raw .. "'"
    end
    local mod = MOD_ALIASES[token]
    if mod ~= nil then
      if mods[mod] then
        return nil, "unknown chord '" .. raw .. "'"
      end
      mods[mod] = true
    else
      if key ~= nil then
        return nil, "unknown chord '" .. raw .. "'"
      end
      if KEY_ALIASES[token] ~= nil then
        key = KEY_ALIASES[token]
      elseif is_single_graphic(token) then
        key = token
      else
        return nil, "unknown chord '" .. raw .. "'"
      end
    end
  end
  if key == nil then
    return nil, "unknown chord '" .. raw .. "'"
  end
  local bare = mods.ctrl == nil and mods.alt == nil
    and mods.shift == nil and mods.super == nil
  if bare and key ~= "enter" and key ~= "escape" then
    return nil, "chord '" .. raw .. "' needs a modifier; bare keys go to the shell"
  end
  return { key = key, ctrl = mods.ctrl == true, alt = mods.alt == true,
    shift = mods.shift == true, super = mods.super == true }
end

-- Canonical spelling for distinctness checks: modifiers in fixed order.
function M.chord_canonical(chord)
  local out = {}
  for _, mod in ipairs(MOD_ORDER) do
    if chord[mod] then
      out[#out + 1] = mod
    end
  end
  out[#out + 1] = chord.key
  return table.concat(out, "+")
end

-- Builds the four in-modal roles from chord strings (ComposerKeys::from_chords:
-- the roles must be pairwise distinct so one press can never mean two
-- things). Returns the keys table or nil plus a reason; fail-closed.
function M.parse_keys(spec)
  if type(spec) ~= "table" then
    return nil, "keys spec must be a table"
  end
  local keys = {}
  for _, role in ipairs({ "newline", "submit", "close", "external_editor" }) do
    local chord, err = M.parse_chord(spec[role])
    if chord == nil then
      return nil, "invalid " .. role .. " chord: " .. err
    end
    keys[role] = chord
  end
  local seen = {}
  local dups = {}
  for _, role in ipairs({ "newline", "submit", "close", "external_editor" }) do
    local canon = M.chord_canonical(keys[role])
    if seen[canon] then
      dups[#dups + 1] = "'" .. canon .. "'"
    else
      seen[canon] = true
    end
  end
  if #dups > 0 then
    return nil, "keys must be distinct; duplicates: " .. table.concat(dups, ", ")
  end
  return keys
end

-- Validates the manual-open chord a keymap would bind (composer.rs
-- validate_open_chord): exactly `<mod>+...+<single-char>`, any order,
-- case-insensitive; named keys are rejected and bare single characters fail
-- closed so registration can never shadow shell typing.
function M.parse_open_chord(raw)
  if type(raw) ~= "string" then
    return nil, "open chord must be a string"
  end
  local text = trim(string.lower(raw))
  if text == "" then
    return nil, "open chord must not be empty"
  end
  if #text > 64 then
    return nil, "unknown open chord '" .. raw .. "'"
  end
  local mods = {}
  local key = nil
  local parts = {}
  for part in string.gmatch(text, "([^%+]+)") do
    parts[#parts + 1] = trim(part)
  end
  local nospace = string.gsub(text, "%s+", "")
  if #parts == 0 or table.concat(parts, "+") ~= nospace then
    return nil, "unknown open chord '" .. raw .. "'"
  end
  for _, token in ipairs(parts) do
    if token == "" then
      return nil, "unknown open chord '" .. raw .. "'"
    end
    local mod = MOD_ALIASES[token]
    if mod ~= nil then
      if mods[mod] then
        return nil, "unknown open chord '" .. raw .. "'"
      end
      mods[mod] = true
    else
      if key ~= nil or not is_single_graphic(token) then
        return nil, "unknown open chord '" .. raw .. "'"
      end
      key = token
    end
  end
  if key == nil then
    return nil, "unknown open chord '" .. raw .. "'"
  end
  if mods.ctrl == nil and mods.alt == nil and mods.shift == nil and mods.super == nil then
    return nil, "open chord '" .. key .. "' needs a modifier (e.g. 'alt+" .. key .. "')"
  end
  return { key = key, ctrl = mods.ctrl == true, alt = mods.alt == true,
    shift = mods.shift == true, super = mods.super == true }
end

-- ---------------------------------------------------------------------------
-- CommandBuffer: bounded extension-side presentation state
-- (re-expressed from composer.rs CommandBuffer; never Terminal Truth)
-- ---------------------------------------------------------------------------

function M.buffer_new()
  return { text = "" }
end

function M.buffer_bytes(buf)
  return #buf.text
end

-- Appends `s`; over the cap fails closed with the buffer kept as-is.
function M.buffer_insert(buf, s)
  s = s or ""
  if type(s) ~= "string" then
    return false, "not a string"
  end
  local wanted = #buf.text + #s
  if wanted > M.MAX_BYTES then
    return false, wanted
  end
  buf.text = buf.text .. s
  return true
end

-- Replaces the content (external-editor result); over the cap fails closed
-- with the old content kept.
function M.buffer_set(buf, s)
  if type(s) ~= "string" then
    return false, "not a string"
  end
  if #s > M.MAX_BYTES then
    return false, #s
  end
  buf.text = s
  return true
end

function M.buffer_clear(buf)
  buf.text = ""
end

-- ---------------------------------------------------------------------------
-- Compatibility gate: no partial activation on version mismatch
-- ---------------------------------------------------------------------------

-- The manifest declares plugin-api ^1.0 and the host validates before
-- activation; this is the defense-in-depth twin inside the plugin. When the
-- check fails the module registers nothing (no commands, no keymap, no
-- events, no overlay) and only records the diagnostic.
function M.check_compat(api_version)
  if type(api_version) ~= "string" then
    return false, "plugin API version is not a string"
  end
  local major = string.match(api_version, "^(%d+)%.")
  if major == nil then
    return false, "unparsable plugin API version '" .. api_version .. "'"
  end
  if tonumber(major) ~= M.REQUIRED_API_MAJOR then
    return false, "requires Plugin API ^1.0, host provides '" .. api_version .. "'"
  end
  return true
end

-- ---------------------------------------------------------------------------
-- Modal session (re-expressed from composer.rs ComposerSession)
-- ---------------------------------------------------------------------------

M.disabled = false
M.disabled_reason = ""

local session = {
  open = false,
  handle = nil,
  buffer = M.buffer_new(),
  keys = nil,
  last_code = "ok",
  last_detail = "",
  overflowed = false,
}

local function note(code, detail)
  session.last_code = code
  session.last_detail = detail or ""
end

-- Host-dependent entries below require the injected `bitty` table; the pure
-- policy above (chords, buffer, compat gate) works without it. The injected
-- host value arrives as engine userdata, so presence is tested with a nil
-- comparison rather than a type check.
local function has_host()
  return bitty ~= nil
end

local NO_HOST = "NO_HOST"

-- Extracts the E_* host code from a pcall error for typed diagnostics.
local function host_code(err)
  local text = tostring(err)
  local code = string.match(text, "(E_[A-Z_]+)")
  if code ~= nil then
    return code
  end
  return "E_UNKNOWN"
end

-- Host calls return indexable engine values (tables or bridged objects);
-- only nil means "no value". Never gate host-provided values on
-- type() == "table".
local function submit_code(outcome)
  if outcome == nil then
    return "E_UNKNOWN", ""
  end
  if outcome.status == "accepted" then
    return "accepted", ""
  end
  if outcome.status == "denied" then
    return "denied:" .. tostring(outcome.deny), ""
  end
  if outcome.status == "unavailable" then
    return "unavailable:" .. tostring(outcome.reason), ""
  end
  return "E_UNKNOWN", ""
end

local function editor_code(outcome)
  if outcome == nil then
    return "E_UNKNOWN", ""
  end
  local status = tostring(outcome.status)
  if status == "edited" then
    return "edited", ""
  end
  if status == "denied" then
    return "denied:" .. tostring(outcome.deny), ""
  end
  if status == "unavailable" then
    return "unavailable:" .. tostring(outcome.reason), ""
  end
  return status, ""
end

-- Overlay scene budget: one update carries at most ~4 KiB (host per-call
-- ceiling), while the buffer may hold 64 KiB. The overlay therefore shows a
-- bounded tail preview; the buffer itself is never truncated. Presentation
-- only, documented in the README.
M.PREVIEW_MAX_BYTES = 4000
M.PREVIEW_MARKER = "[...]"

local function preview_text()
  local text = session.buffer.text
  local tail_keep = M.PREVIEW_MAX_BYTES - #M.PREVIEW_MARKER
  if #text > tail_keep then
    return M.PREVIEW_MARKER .. string.sub(text, #text - tail_keep + 1)
  end
  return text
end

local function release_session(reason)
  local handle = session.handle
  session.open = false
  session.handle = nil
  if handle ~= nil then
    -- Idempotent on an already-released handle; Core-reported reasons
    -- overwrite on involuntary terminal causes. Failure here cannot strand
    -- capture: release is Core-guaranteed.
    pcall(bitty.ui.overlay.release, handle, reason)
  end
end

local function paint()
  local scene = { kind = "Text", text = preview_text() }
  local ok, err = pcall(bitty.ui.overlay.update, session.handle, scene)
  if not ok then
    -- Fail closed: never leave a half-presented modal behind.
    release_session("cancelled")
    note(host_code(err), "overlay update failed; session closed")
    return false
  end
  return true
end

-- Scalar state queries for tests and diagnostics (plain values only).
function M.is_open()
  return session.open
end

function M.buffer_text()
  return session.buffer.text
end

function M.buffer_byte_count()
  return #session.buffer.text
end

function M.last_code()
  return session.last_code
end

function M.last_detail()
  return session.last_detail
end

function M.capture_overflowed()
  return session.overflowed
end

-- Manual open only: acquires the focusable overlay + transient capture. There
-- is no auto-enter path; while closed, input bytes flow to the PTY and the
-- plugin touches nothing.
function M.open_now()
  if not has_host() then
    return false, NO_HOST
  end
  if M.disabled then
    return false, M.CODE_DISABLED
  end
  if session.open then
    note("ALREADY_OPEN", "session already holds capture")
    return false, "ALREADY_OPEN"
  end
  local submit_canon = M.chord_canonical(session.keys.submit)
  local ok, handle = pcall(bitty.ui.overlay.acquire,
    { title = "Composer", placeholder = "Type a command (" .. submit_canon .. " submits)" })
  if not ok then
    note(host_code(handle), "overlay acquire failed; session stays closed")
    return false, session.last_code
  end
  session.open = true
  session.handle = handle
  session.overflowed = false
  if not paint() then
    return false, session.last_code
  end
  note("OPENED", "")
  return true
end

-- Submit through the controlled PTY path. Clears the buffer only after the
-- host reports accepted; any denial or unavailability leaves the buffer and
-- the open session intact and emits nothing.
function M.submit_now()
  if not has_host() then
    return false, NO_HOST
  end
  if M.disabled then
    return false, M.CODE_DISABLED
  end
  if not session.open then
    note(M.CODE_NOT_OPEN, "no session; bytes belong to the PTY")
    return false, M.CODE_NOT_OPEN
  end
  local ok, outcome = pcall(bitty.terminal.submit, session.buffer.text)
  if not ok then
    note(host_code(outcome), "submit call failed; buffer kept")
    return false, session.last_code
  end
  local code = submit_code(outcome)
  if code == "accepted" then
    M.buffer_clear(session.buffer)
    release_session("submitted")
    note("SUBMITTED", "")
    return true
  end
  note(code, "submit refused; buffer kept, session stays open")
  return false, code
end

-- Explicit cancel: discards the unsubmitted buffer and writes nothing.
function M.cancel_now()
  if not has_host() then
    return false, NO_HOST
  end
  if M.disabled then
    return false, M.CODE_DISABLED
  end
  if not session.open then
    note(M.CODE_NOT_OPEN, "no session; bytes belong to the PTY")
    return false, M.CODE_NOT_OPEN
  end
  M.buffer_clear(session.buffer)
  release_session("cancelled")
  note("CANCELLED", "")
  return true
end

-- Close (Esc): ends the modal, draft preserved for reopen. Like cancel it
-- writes nothing to the PTY; unlike cancel it keeps the buffer.
function M.close_now()
  if not has_host() then
    return false, NO_HOST
  end
  if M.disabled then
    return false, M.CODE_DISABLED
  end
  if not session.open then
    note(M.CODE_NOT_OPEN, "no session; bytes belong to the PTY")
    return false, M.CODE_NOT_OPEN
  end
  release_session("cancelled")
  note("CLOSED", "draft preserved")
  return true
end

-- External-editor detour: the host resolves the allowlisted program, owns the
-- temp file, and returns a typed outcome. Edited content installs bounded
-- (over-cap keeps the old draft); every other outcome preserves the draft.
-- The session stays open throughout: the editor is a detour, not an exit.
function M.editor_now(timeout_ms)
  if not has_host() then
    return false, NO_HOST
  end
  if M.disabled then
    return false, M.CODE_DISABLED
  end
  if not session.open then
    note(M.CODE_NOT_OPEN, "no session; bytes belong to the PTY")
    return false, M.CODE_NOT_OPEN
  end
  local timeout = M.EDITOR_TIMEOUT_DEFAULT_MS
  if type(timeout_ms) == "number" and timeout_ms >= 1 then
    timeout = math.floor(timeout_ms)
    if timeout > M.EDITOR_TIMEOUT_MAX_MS then
      timeout = M.EDITOR_TIMEOUT_MAX_MS
    end
  end
  local ok, outcome = pcall(bitty.process.editor.start,
    { draft = session.buffer.text, timeout_ms = timeout })
  if not ok then
    note(host_code(outcome), "editor call failed; draft kept")
    return false, session.last_code
  end
  local code = editor_code(outcome)
  if code == "edited" then
    local set_ok = M.buffer_set(session.buffer, outcome.content or "")
    if not set_ok then
      -- Over-cap editor content: the one over-limit rule, draft kept.
      note("unavailable:too-large", "editor content over cap; draft kept")
      paint()
      return false, session.last_code
    end
    paint()
    note("EDITED", "")
    return true
  end
  note(code, "editor round trip ended; draft kept")
  paint()
  return false, code
end

-- Normalizes one captured key payload to a chord for role matching.
local function key_payload_chord(data)
  if data == nil or type(data.key) ~= "string" then
    return nil
  end
  local name = string.lower(data.key)
  name = KEY_ALIASES[name] or name
  if not (name == "enter" or name == "escape" or is_single_graphic(name)) then
    return nil
  end
  return {
    key = name,
    ctrl = data.ctrl == true,
    alt = data.alt == true,
    shift = data.shift == true,
    super = data.super == true,
  }
end

-- Feeds one captured key press while open. Role precedence is submit-first
-- (composer.rs ComposerSession::feed: submit, close, external-editor,
-- newline, then text); distinctness validation at activation makes a double
-- match impossible by construction. Unmatched presses are swallowed: the
-- buffer is untouched and the session stays open, and nothing reaches the
-- PTY through this plugin.
local function feed_key(data)
  local chord = key_payload_chord(data)
  if chord == nil then
    return "ignored"
  end
  local canon = M.chord_canonical(chord)
  if canon == M.chord_canonical(session.keys.submit) then
    local ok = M.submit_now()
    if ok then
      return "submitted"
    end
    return "submit-refused"
  end
  if canon == M.chord_canonical(session.keys.close) then
    M.close_now()
    return "closed"
  end
  if canon == M.chord_canonical(session.keys.external_editor) then
    M.editor_now(nil)
    return "editor"
  end
  if canon == M.chord_canonical(session.keys.newline) then
    local ok = M.buffer_insert(session.buffer, "\n")
    if not ok then
      note("rejected:too-large", "newline over cap; buffer kept")
      return "too-large"
    end
    paint()
    return "newline"
  end
  return "ignored"
end

-- Feeds one captured event while open. Typing and paste share the single
-- over-limit rule: rejection keeps the prior buffer intact.
local function feed_event(ev)
  if ev == nil then
    return "ignored"
  end
  if ev.type == "key" then
    return feed_key(ev.data)
  end
  if ev.type == "text" or ev.type == "paste" then
    local data = ev.data
    if data == nil or type(data.text) ~= "string" then
      return "ignored"
    end
    local ok = M.buffer_insert(session.buffer, data.text)
    if not ok then
      note("rejected:too-large", ev.type .. " over cap; buffer kept")
      return "too-large"
    end
    paint()
    return "inserted"
  end
  return "ignored"
end

-- Owner-initiated drain of already-queued capture events. Never a Core-driven
-- callback and never on the input hot path: the host tick invokes this, and
-- each queued event routes through the modal feed above. Ends early when the
-- session closes mid-drain (submit/close); leftover queue entries belong to
-- a released session and are never delivered to the terminal by this plugin.
function M.pump()
  if not has_host() then
    return false, NO_HOST
  end
  if M.disabled then
    return false, M.CODE_DISABLED
  end
  if not session.open then
    -- Passive drain while closed: no diagnostic overwrite, so an
    -- observer-recorded release reason survives a trailing pump.
    return false, M.CODE_NOT_OPEN
  end
  local ok, poll = pcall(bitty.ui.overlay.poll, session.handle)
  if not ok then
    -- Stale or foreign handle (e.g. new generation): drop the session,
    -- keep the draft, never touch the PTY.
    session.open = false
    session.handle = nil
    note(host_code(poll), "poll failed; session dropped, draft kept")
    return false, session.last_code
  end
  if poll == nil then
    note("E_UNKNOWN", "poll returned no value")
    return false, session.last_code
  end
  if poll.overflowed == true then
    session.overflowed = true
  end
  if poll.status == "released" then
    -- Involuntary terminal cause (timeout, focus switch, unload, crash):
    -- Core already released capture; keep the draft, mark closed.
    session.open = false
    session.handle = nil
    note("RELEASED", tostring(poll.reason or ""))
    return true, "released"
  end
  local events = poll.events
  if events == nil then
    events = {}
  end
  local fed = 0
  for _, ev in ipairs(events) do
    feed_event(ev)
    fed = fed + 1
    if not session.open then
      break
    end
  end
  if session.open then
    paint()
  end
  return true, fed
end

-- ---------------------------------------------------------------------------
-- Host wiring. Skipped when the host table is absent so the pure policy
-- above (chords, buffer, compat gate) stays loadable on its own.
-- ---------------------------------------------------------------------------

-- Diagnostic entry point for tests and operators; also the pump driver the
-- host tick invokes. Set before the host guard so it exists in every mode.
if composer == nil then
  composer = M
end

if bitty == nil then
  return M
end

do
  local compat_ok, compat_err = M.check_compat(bitty.api_version)
  if not compat_ok then
    -- Version/capability mismatch disables with a diagnostic and no partial
    -- activation: nothing below runs, so no command, keymap, event, or
    -- overlay exists for this generation.
    M.disabled = true
    M.disabled_reason = compat_err
    note(M.CODE_DISABLED, compat_err)
    return M
  end

  local function setting(key, default)
    local value = bitty.settings.get(key)
    if value == nil then
      return default
    end
    return value
  end

  -- In-modal roles: validated distinct, fail-closed to defaults. A bad
  -- configuration never breaks the modal and never widens authority.
  local spec = {
    newline = setting("newline", M.DEFAULT_KEYS.newline),
    submit = setting("submit", M.DEFAULT_KEYS.submit),
    close = setting("close", M.DEFAULT_KEYS.close),
    external_editor = setting("external_editor", M.DEFAULT_KEYS.external_editor),
  }
  local keys, keys_err = M.parse_keys(spec)
  if keys == nil then
    keys = M.parse_keys(M.DEFAULT_KEYS)
    note("KEYS_FALLBACK", tostring(keys_err))
  end
  session.keys = keys

  local open_raw = setting("open_chord", M.DEFAULT_OPEN_CHORD)
  local open_chord, open_err = M.parse_open_chord(open_raw)
  if open_chord == nil then
    open_chord, open_err = M.parse_open_chord(M.DEFAULT_OPEN_CHORD)
  end
  -- Recompute the canonical open spelling for the suggestion below.
  local open_canon = M.DEFAULT_OPEN_CHORD
  if open_chord ~= nil then
    local parts = {}
    if open_chord.ctrl then parts[#parts + 1] = "ctrl" end
    if open_chord.alt then parts[#parts + 1] = "alt" end
    if open_chord.shift then parts[#parts + 1] = "shift" end
    if open_chord.super then parts[#parts + 1] = "super" end
    parts[#parts + 1] = open_chord.key
    open_canon = table.concat(parts, "+")
  end
  if open_err ~= nil and session.last_code == "ok" then
    note("KEYS_FALLBACK", tostring(open_err))
  end

  local empty_schema = {
    type = "object",
    properties = {},
    additionalProperties = false,
  }

  bitty.commands.register({
    id = "open",
    title = "Composer: open",
    description = "Manually open the composer modal (no auto-enter).",
    args_schema = empty_schema,
    run = function(_args)
      local ok, code = M.open_now()
      return { ok = ok, code = code }
    end,
  })

  bitty.commands.register({
    id = "submit",
    title = "Composer: submit",
    description = "Submit the buffer through the paste pipeline; clears only on accepted.",
    args_schema = empty_schema,
    run = function(_args)
      local ok, code = M.submit_now()
      return { ok = ok, code = code }
    end,
  })

  bitty.commands.register({
    id = "cancel",
    title = "Composer: cancel",
    description = "Discard the unsubmitted buffer; writes nothing.",
    args_schema = empty_schema,
    run = function(_args)
      local ok, code = M.cancel_now()
      return { ok = ok, code = code }
    end,
  })

  bitty.commands.register({
    id = "close",
    title = "Composer: close",
    description = "Close the modal, draft preserved for reopen; writes nothing.",
    args_schema = empty_schema,
    run = function(_args)
      local ok, code = M.close_now()
      return { ok = ok, code = code }
    end,
  })

  bitty.commands.register({
    id = "editor",
    title = "Composer: external editor",
    description = "Round-trip the draft through the allowlisted editor; failures keep the draft.",
    args_schema = {
      type = "object",
      properties = {
        timeout_ms = { type = "number", minimum = 1 },
      },
      additionalProperties = false,
    },
    run = function(args)
      local timeout = nil
      if args ~= nil then
        timeout = args.timeout_ms
      end
      local ok, code = M.editor_now(timeout)
      return { ok = ok, code = code }
    end,
  })

  -- Key suggestion never overrides user or workspace mappings; conflicts
  -- produce host diagnostics. A failed suggestion is non-fatal: the `open`
  -- command above still works.
  local suggest_ok, suggest_err = pcall(bitty.keymaps.suggest, {
    chord = open_canon,
    command = M.PLUGIN_ID .. ":open",
  })
  if not suggest_ok then
    if session.last_code == "ok" or session.last_code == "KEYS_FALLBACK" then
      note(session.last_code, "keymap suggest failed: " .. host_code(suggest_err))
    end
  end

  -- Observation only: Core already released capture on any involuntary cause.
  -- The handler performs no host calls, so it cannot throw.
  bitty.events.subscribe("overlay.released", function(event)
    local reason = ""
    if event ~= nil and event.payload ~= nil then
      reason = tostring(event.payload.reason or "")
    end
    session.open = false
    session.handle = nil
    note("RELEASED", reason)
  end)
end

return M
