//! `bitty shell-init`: emit shell integration (CTX-1054, issue #1813;
//! completed for all five shells by issue #2372).
//!
//! Canonical: issue #1813 (`bitty completion <shell>` emits completion
//! scripts) and issue #2372 (nested completion generated from the CLI
//! grammar, single registration, OSC 7 + OSC 133 on every prompt path,
//! strict-shell safety, idempotent double-source, and tolerance for an
//! absent or older `bitty` binary).
//!
//! # Contract (implemented)
//!
//! - Shape: `bitty shell-init <shell>`, where `<shell>` is one of
//!   `bash|zsh|fish|powershell|nushell` (`pwsh` accepted for PowerShell,
//!   `nu` for Nushell — the same [`CompletionShell`] table as
//!   `bitty completion`, so the two commands can never disagree on names).
//!   The script prints to stdout, exit 0.
//! - Class: local (no instance, no config load, no plugin VM, safe-mode
//!   clean). The script is static: per-shell prompt hooks (OSC 7 cwd report
//!   plus OSC 133 prompt-start `A`, command-start `B`, and
//!   command-done-with-status `D` marks, observation only) followed by the
//!   matching `bitty completion <shell>` definitions inlined from the CLI
//!   grammar. The script never invokes `bitty` at startup, so a machine
//!   without the binary (or with an older one) still sources cleanly.
//! - `--format` is accepted and ignored: shell-init emits a script, never the
//!   output envelope. `--no-color` is accepted and ignored (scripts carry no
//!   color). `--socket`/`--instance` combined with `shell-init` fail closed
//!   (exit 2): they select a runtime target this command never uses.
//! - Missing/unknown shell, extra positionals, unknown flags, and stray `--`
//!   fail closed (exit 2, stderr only, no stdout script).
//!
//! # Wiring (the #1813/#2372 acceptance)
//!
//! - bash: `eval "$(bitty shell-init bash)"` in `~/.bashrc`.
//! - zsh: `eval "$(bitty shell-init zsh)"` in `~/.zshrc`.
//! - fish: `bitty shell-init fish | source` in `config.fish`.
//! - powershell: `bitty shell-init powershell | Out-String | Invoke-Expression`
//!   in `$PROFILE`.
//! - nushell: save once and `source` it from `config.nu` (see the script
//!   header).
//!
//! [`install_line`] is the single source for these lines: `bitty init` and
//! the help text both render it, so a fresh shell gets the same working
//! command everywhere.
//!
//! # Exit codes (stable taxonomy)
//!
//! - `0` success.
//! - `2` usage error (missing/unknown shell, extra positional, unknown flag,
//!   bad `--socket`/`--instance` shape, stray `--`).

#![forbid(unsafe_code)]

use std::borrow::Cow;

use crate::cli::Args;
use crate::completion::{CompletionShell, completion_script, shell_list};

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success.
pub const EXIT_OK: i32 = 0;
/// CLI usage error.
pub const EXIT_USAGE: i32 = 2;

// ---------------------------------------------------------------------------
// Install lines (single source for shell-init help and `bitty init`)
// ---------------------------------------------------------------------------

/// Exact startup-file line that enables this shell's integration.
#[must_use]
pub fn install_line(shell: CompletionShell) -> &'static str {
    match shell {
        CompletionShell::Bash => "eval \"$(bitty shell-init bash)\"",
        CompletionShell::Zsh => "eval \"$(bitty shell-init zsh)\"",
        CompletionShell::Fish => "bitty shell-init fish | source",
        CompletionShell::Powershell => {
            "bitty shell-init powershell | Out-String | Invoke-Expression"
        }
        CompletionShell::Nushell => {
            "bitty shell-init nushell | save -f ~/.config/nushell/bitty-shell-init.nu"
        }
    }
}

/// Extra follow-up line for shells whose install is two steps (Nushell).
#[must_use]
pub fn install_follow_up(shell: CompletionShell) -> &'static str {
    match shell {
        CompletionShell::Nushell => {
            " Then add `source ~/.config/nushell/bitty-shell-init.nu` to config.nu."
        }
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Static scripts (prompt hooks; the matching completion definitions follow)
// ---------------------------------------------------------------------------

/// Bash hooks: `PROMPT_COMMAND` (OSC 133 `D` status, OSC 7 cwd, OSC 133 `A`
/// prompt-start) plus `PS0` (OSC 133 `B` command-start).
const BASH_HOOKS: &str = r#"# bitty shell integration for Bash (static; generated, do not edit).
# Enable with: eval "$(bitty shell-init bash)" in ~/.bashrc
# - Tab completion follows this block (generated from the CLI grammar).
#   The script never invokes `bitty` at startup, so an absent or older
#   binary cannot break shell startup.
# - Prompt hooks: OSC 7 cwd report plus OSC 133 prompt-start (A),
#   command-start (B via PS0), and command-done-with-status (D) marks
#   (observation only; ignored by terminals without support).
# - Runs on every prompt path: PROMPT_COMMAND fires for multiline edits,
#   vi-mode (set -o vi), and transient-prompt redraws alike.
# - OSC 7 cwd is percent-encoded (RFC 3986 unreserved plus slash kept).
#   Hostname passes through verbatim (DNS-safe, authority must match).
# - Safe under `set -euo pipefail`; re-sourcing is a no-op.
if [ -z "${_BITTY_SHELL_INIT:-}" ]; then
    _BITTY_SHELL_INIT=1
    # Encode a path for OSC 7: keep unreserved and slash, encode the rest.
    # Drive-letter colon (C:/) survives for the file URI convention.
    _bitty_urlencode() {
        local LC_ALL=C
        local _bitty_input
        _bitty_input="${1:-}"
        local _bitty_len
        _bitty_len=${#_bitty_input}
        local _bitty_i
        local _bitty_c
        local _bitty_o
        local _bitty_out=""
        for (( _bitty_i = 0; _bitty_i < _bitty_len; _bitty_i++ )); do
            _bitty_c="${_bitty_input:_bitty_i:1}"
            case "$_bitty_c" in
                [A-Za-z0-9.~_/-])
                    _bitty_o="$_bitty_c"
                    ;;
                :)
                    if [[ $_bitty_i -eq 1 && ${_bitty_input:0:1} == [A-Za-z] && ${_bitty_input:2:1} == "/" ]]; then
                        _bitty_o=":"
                    else
                        _bitty_o="%3A"
                    fi
                    ;;
                *)
                    printf -v _bitty_o '%%%02X' "'$_bitty_c"
                    ;;
            esac
            _bitty_out+="$_bitty_o"
        done
        printf '%s' "$_bitty_out"
    }
    _bitty_osc7() {
        printf '\e]7;file://%s%s\e\\' "${HOSTNAME:-$(uname -n 2>/dev/null)}" "$(_bitty_urlencode "${PWD:-.}")"
    }
    _bitty_prompt_hook() {
        local _bitty_status=$?
        printf '\e]133;D;%s\e\\' "$_bitty_status"
        _bitty_osc7
        printf '\e]133;A\e\\'
    }
    # The hook runs first so it captures the previous command's status
    # before existing prompt commands can overwrite $?. Both string- and
    # array-valued PROMPT_COMMAND are preserved.
    if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a "* ]]; then
        if [[ " ${PROMPT_COMMAND[*]} " != *" _bitty_prompt_hook "* ]]; then
            PROMPT_COMMAND=(_bitty_prompt_hook "${PROMPT_COMMAND[@]}")
        fi
    elif [[ "${PROMPT_COMMAND:-}" != *"_bitty_prompt_hook"* ]]; then
        PROMPT_COMMAND="_bitty_prompt_hook${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
    fi
    # Command-start (B): PS0 expands after a command line is read and before
    # it executes, for single-line, multiline, and vi-mode edits alike.
    if [[ "${PS0:-}" != *"]133;B"* ]]; then
        PS0="${PS0:-}\[\e]133;B\e\\\]"
    fi
fi
"#;

/// Zsh hooks: `precmd` (OSC 133 `D` status, OSC 7 cwd, OSC 133 `A`
/// prompt-start) plus `preexec` (OSC 133 `B` command-start).
const ZSH_HOOKS: &str = r#"# bitty shell integration for Zsh (static; generated, do not edit).
# Enable with: eval "$(bitty shell-init zsh)" in ~/.zshrc (after compinit,
# so compdef can register Tab completion).
# - Tab completion follows this block (generated from the CLI grammar).
#   The script never invokes `bitty` at startup, so an absent or older
#   binary cannot break shell startup.
# - Prompt hooks: OSC 7 cwd report plus OSC 133 prompt-start (A),
#   command-start (B via preexec), and command-done-with-status (D) marks
#   (observation only; ignored by terminals without support).
# - Runs on every prompt path: precmd/preexec fire for multiline edits,
#   vi-mode (bindkey -v), and transient-prompt redraws alike.
# - OSC 7 cwd is percent-encoded (RFC 3986 unreserved plus slash kept).
#   Hostname passes through verbatim (DNS-safe, authority must match).
# - Safe under `setopt no_unset`; re-sourcing is a no-op.
if (( ! ${+_BITTY_SHELL_INIT} )); then
    typeset -g _BITTY_SHELL_INIT=1
    # Encode a path for OSC 7: keep unreserved and slash, encode the rest.
    # Drive-letter colon (C:/) survives for the file URI convention.
    _bitty_urlencode() {
        local LC_ALL=C
        local _bitty_input
        _bitty_input="${1:-}"
        local _bitty_len
        _bitty_len=${#_bitty_input}
        local _bitty_i
        local _bitty_c
        local _bitty_o
        local _bitty_out=""
        for (( _bitty_i = 0; _bitty_i < _bitty_len; _bitty_i++ )); do
            _bitty_c="${_bitty_input:_bitty_i:1}"
            case "$_bitty_c" in
                [A-Za-z0-9.~_/-])
                    _bitty_o="$_bitty_c"
                    ;;
                :)
                    if [[ $_bitty_i -eq 1 && ${_bitty_input:0:1} == [A-Za-z] && ${_bitty_input:2:1} == "/" ]]; then
                        _bitty_o=":"
                    else
                        _bitty_o="%3A"
                    fi
                    ;;
                *)
                    printf -v _bitty_o '%%%02X' "'$_bitty_c"
                    ;;
            esac
            _bitty_out+="$_bitty_o"
        done
        printf '%s' "$_bitty_out"
    }
    _bitty_osc7() {
        printf '\e]7;file://%s%s\e\\' "${HOST:-localhost}" "$(_bitty_urlencode "${PWD:-.}")"
    }
    _bitty_prompt_hook() {
        local _bitty_status=$?
        printf '\e]133;D;%s\e\\' "$_bitty_status"
        _bitty_osc7
        printf '\e]133;A\e\\'
    }
    _bitty_preexec_hook() {
        printf '\e]133;B\e\\'
    }
    typeset -ga precmd_functions preexec_functions
    if (( ! ${precmd_functions[(Ie)_bitty_prompt_hook]} )); then
        precmd_functions+=(_bitty_prompt_hook)
    fi
    if (( ! ${preexec_functions[(Ie)_bitty_preexec_hook]} )); then
        preexec_functions+=(_bitty_preexec_hook)
    fi
fi
"#;

/// Fish hooks: `fish_prompt` event (OSC 133 `D` status, OSC 7 cwd, OSC 133
/// `A` prompt-start) plus `fish_preexec` event (OSC 133 `B` command-start).
const FISH_HOOKS: &str = r#"# bitty shell integration for Fish (static; generated, do not edit).
# Enable with: bitty shell-init fish | source in config.fish
# - Tab completion follows this block (generated from the CLI grammar).
#   The script never invokes `bitty` at startup, so an absent or older
#   binary cannot break shell startup.
# - Prompt hooks: OSC 7 cwd report plus OSC 133 prompt-start (A),
#   command-start (B via fish_preexec), and command-done-with-status (D)
#   marks (observation only; ignored by terminals without support).
# - Runs on every prompt path: the fish_prompt event fires for multiline
#   edits and vi-mode (fish_vi_key_bindings) alike.
# - Safe in non-interactive fish (`status is-interactive` false): no hook
#   fires and no completion is queried, so sourcing stays silent.
# - OSC 7 cwd is percent-encoded (RFC 3986 unreserved plus slash kept).
#   Hostname passes through verbatim (DNS-safe, authority must match).
# - Re-sourcing is a no-op.
if not set -q _BITTY_SHELL_INIT
    set -g _BITTY_SHELL_INIT 1
    # Encode a path for OSC 7: keep unreserved and slash, encode the rest.
    # Drive-letter colon (C:/) survives for the file URI convention.
    function _bitty_urlencode
        set -l _bitty_input $argv[1]
        set -l _bitty_encoded (string escape --style=url -- "$_bitty_input")
        string replace -r '^([A-Za-z])%3[Aa]/' '$1:/' -- "$_bitty_encoded"
        # Mask replace status: no drive match still prints input with status 1.
        true
    end
    function _bitty_prompt_hook --on-event fish_prompt
        set -l _bitty_status $status
        printf '\e]133;D;%s\e\\' $_bitty_status
        printf '\e]7;file://%s%s\e\\' (hostname 2>/dev/null) (_bitty_urlencode (pwd))
        printf '\e]133;A\e\\'
    end
    function _bitty_preexec_hook --on-event fish_preexec
        printf '\e]133;B\e\\'
    end
end
"#;

/// PowerShell hooks: wrapped `prompt` (OSC 133 `D` status, OSC 7 cwd, OSC 133
/// `A` prompt-start; the previous prompt is chained, never dropped) plus a
/// guarded PSReadLine Enter handler (OSC 133 `B` command-start).
const POWERSHELL_HOOKS: &str = r#"# bitty shell integration for PowerShell (static; generated, do not edit).
# Enable with: bitty shell-init powershell | Out-String | Invoke-Expression in $PROFILE
# - Tab completion follows this block (generated from the CLI grammar and
#   registered once). The script never invokes `bitty` at startup, so an
#   absent or older binary cannot break shell startup.
# - Prompt hooks: OSC 7 cwd report plus OSC 133 prompt-start (A),
#   command-start (B via PSReadLine Enter), and command-done-with-status (D)
#   marks (observation only; ignored by terminals without support).
# - Safe under `Set-StrictMode -Version Latest`; re-sourcing is a no-op.
if (-not (Test-Path variable:global:_BittyShellInit)) {
    $global:_BittyShellInit = $true
    $bitty_previous_prompt = Get-Command prompt -CommandType Function -ErrorAction SilentlyContinue | Select-Object -First 1
    $global:_BittyPromptChain = if ($bitty_previous_prompt) { $bitty_previous_prompt.ScriptBlock } else { $null }
    # Encode a path for OSC 7: backslashes become slashes, then each
    # segment is escaped. Drive-letter colon (C:) survives for file URIs.
    function Global:_BittyUrlEncode([string]$Path) {
        $normalized = $Path -replace '\\', '/'
        $parts = $normalized.Split('/')
        for ($i = 0; $i -lt $parts.Length; $i++) {
            if ($i -eq 0 -and $parts[$i] -match '^[A-Za-z]:$') {
                continue
            }
            $parts[$i] = [System.Uri]::EscapeDataString($parts[$i])
        }
        return ($parts -join '/')
    }
    function global:prompt {
        # Prefer the native exit code: $? alone collapses every native
        # failure to 1 (a native command exiting 42 must report D;42).
        # Test-Path guards StrictMode when no native command ran yet.
        $status = if ($?) { 0 } elseif (Test-Path variable:global:LASTEXITCODE) { $global:LASTEXITCODE } else { 1 }
        $host_name = [System.Net.Dns]::GetHostName()
        $uri_path = _BittyUrlEncode (Get-Location).Path
        if ($uri_path -notmatch '^/') { $uri_path = '/' + $uri_path }
        $esc = [char]27
        Write-Host -NoNewline "$esc]133;D;$status$esc\"
        Write-Host -NoNewline "$esc]7;file://$host_name$uri_path$esc\"
        Write-Host -NoNewline "$esc]133;A$esc\"
        if ($global:_BittyPromptChain) { & $global:_BittyPromptChain } else { "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) " }
    }
    if (Get-Module -ListAvailable -Name PSReadLine) {
        try {
            # Command-start mark from the validation hook: PSReadLine calls
            # it before execution for every accepted line, including commands
            # dropped from history as duplicates. Multiline Enter behavior is
            # untouched (unlike replacing the Enter key handler).
            $global:_BittyCommandValidationHandler = (Get-PSReadLineOption).CommandValidationHandler
            Set-PSReadLineOption -CommandValidationHandler {
                param($commandAst)
                if ($global:_BittyCommandValidationHandler) {
                    & $global:_BittyCommandValidationHandler $commandAst
                }
                $esc = [char]27
                [Console]::Write("$esc]133;B$esc\")
            }
        } catch {
            # A host without a usable PSReadLine keeps prompt marks only.
        }
    }
}
"#;

/// Nushell hooks: `pre_prompt` (OSC 133 `D` status, OSC 7 cwd, OSC 133 `A`
/// prompt-start) plus an optional `pre_execution` (OSC 133 `B`
/// command-start). Existing hooks are kept.
///
/// The completion definitions are inlined (shared with the
/// `bitty completion nushell` output) because Nushell resolves `source`
/// paths at parse time: a `save -f` then `source` roundtrip of a just-written
/// file cannot work on first setup — the file does not exist yet when the
/// `source` line is parsed.
///
/// The hook guard probes the session's own `$env.config.hooks.pre_prompt`
/// for the `bitty-shell-init` marker closure below, never `$env`: `$env.*`
/// entries are real OS environment variables, so a child `nu` inherits them
/// while starting with a fresh `$env.config` — an env-var guard would skip
/// registration in the child and it would emit no marks (#1856). Probing
/// actual hook state keeps re-sourcing exactly-once per session while child
/// sessions self-configure.
const NUSHELL_HOOKS: &str = r##"# bitty shell integration for Nushell (static; generated, do not edit).
# Enable with: bitty shell-init nushell | save -f ~/.config/nushell/bitty-shell-init.nu
# then add `source ~/.config/nushell/bitty-shell-init.nu` to config.nu
# - Tab completion inlined below (same output as `bitty completion nushell`).
#   Inlined because Nushell parses `source` paths before running `save -f`,
#   so a save-then-source roundtrip of a just-written file cannot load.
#   The script never invokes `bitty` at startup.
# - Prompt hooks: OSC 7 cwd report plus OSC 133 prompt-start (A),
#   command-start (B via pre_execution when the session supports it), and
#   command-done-with-status (D) marks (observation only; ignored by
#   terminals without support).
# - OSC 7 cwd is percent-encoded (RFC 3986 unreserved plus slash kept).
#   Hostname passes through verbatim (DNS-safe, authority must match).
#   Drive-letter colon (C:/) survives for the file URI convention.
# - Guard: this session's own hooks list is probed for the marker closure
#   below (an $env guard would leak into child sessions via inheritance
#   while each child starts with a fresh $env.config: #1856).
if not (try { $env.config.hooks.pre_prompt? | default [] | any {|h| try { $h | to nuon --serialize | str contains "# bitty-shell-init" } catch { false } } } catch { false }) {
    $env.config = ($env.config | default {} hooks | upsert hooks.pre_prompt ((try { $env.config.hooks.pre_prompt } catch { [] }) ++ [{||
        # bitty-shell-init
        print -n $"\e]133;D;($env.LAST_EXIT_CODE? | default 0)\e\\"
        print -n $"\e]7;file://(sys host | get hostname)(pwd | str replace --all "\\" "/" | url encode | str replace --all ':' '%3A' | str replace --regex '^([A-Za-z])%3A/' '$1:/')\e\\"
        print -n "\e]133;A\e\\"
    }]))
}
# Command-start (B) is separate and optional: older Nushell versions have no
# pre_execution hook key, so an unsupported session simply skips this mark.
try {
    if not ($env.config.hooks.pre_execution? | default [] | any {|h| try { $h | to nuon --serialize | str contains "# bitty-shell-init-command" } catch { false } }) {
        $env.config = ($env.config | upsert hooks.pre_execution ((try { $env.config.hooks.pre_execution } catch { [] }) ++ [{||
            # bitty-shell-init-command
            print -n "\e]133;B\e\\"
        }]))
    }
} catch {
}
"##;

/// Static shell-integration script for a shell (hooks + inlined completion).
///
/// The completion definitions are generated from [`crate::grammar`] and
/// inlined rather than evaled from `bitty completion <shell>`, so sourcing
/// never runs the binary (absent/older binaries stay harmless).
#[must_use]
pub fn shell_init_script(shell: CompletionShell) -> Cow<'static, str> {
    let hooks = match shell {
        CompletionShell::Bash => BASH_HOOKS,
        CompletionShell::Zsh => ZSH_HOOKS,
        CompletionShell::Fish => FISH_HOOKS,
        CompletionShell::Powershell => POWERSHELL_HOOKS,
        CompletionShell::Nushell => NUSHELL_HOOKS,
    };
    Cow::Owned(format!("{hooks}\n{}", completion_script(shell)))
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// Validated `bitty shell-init` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellInitRequest {
    /// Target shell.
    pub shell: CompletionShell,
}

/// Parse failure: help vs usage error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellInitParseError {
    /// `--help` requested: print [`shell_init_help_text`] to stdout, exit 0.
    Help,
    /// Usage error: print the message (it already ends with usage) to stderr,
    /// exit 2.
    Usage(String),
}

impl ShellInitParseError {
    /// Render the stderr message for [`ShellInitParseError::Usage`].
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::Help => String::from("bitty shell-init: help requested"),
            Self::Usage(message) => message,
        }
    }
}

/// Usage line for `bitty shell-init`.
#[must_use]
pub fn shell_init_usage() -> String {
    format!(
        "Usage: bitty shell-init <shell>\nSupported shells: {}",
        shell_list()
    )
}

/// Help text for `bitty shell-init` (`--help` never needs an instance or VM).
#[must_use]
pub fn shell_init_help_text() -> String {
    format!(
        "bitty shell-init — emit shell integration (local, no instance)\n\
         \n\
         Usage: bitty shell-init <shell>\n\
         \n\
         Prints a static shell-integration script for <shell> (one of {}) to stdout.\n\
         The script inlines Tab completion for the full CLI grammar (first-level\n\
         words, nested verbs, and flags) and installs prompt hooks that report\n\
         cwd (OSC 7) plus prompt-start, command-start, and exit-status marks\n\
         (OSC 133). It never invokes `bitty` while sourcing, so an absent or\n\
         older binary cannot break shell startup.\n\
         \n\
         Enable it from your shell startup file:\n\
         \x20 bash:       {}  (~/.bashrc)\n\
         \x20 zsh:        {}  (~/.zshrc)\n\
         \x20 fish:       {}  (config.fish)\n\
         \x20 powershell: {}  ($PROFILE)\n\
         \x20 nushell:    {}  (then `source` it from config.nu)\n",
        shell_list(),
        install_line(CompletionShell::Bash),
        install_line(CompletionShell::Zsh),
        install_line(CompletionShell::Fish),
        install_line(CompletionShell::Powershell),
        install_line(CompletionShell::Nushell),
    )
}

/// Validate post-`shell-init` tokens.
///
/// `--format` (both spellings) and `--no-color` are consumed and ignored:
/// shell-init emits a script, never the output envelope and never color.
pub fn parse_shell_init_request(
    tokens: &[String],
    invoked_as: &str,
) -> Result<ShellInitRequest, ShellInitParseError> {
    let mut shell: Option<CompletionShell> = None;
    let mut i = 0usize;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "-h" || token == "--help" {
            return Err(ShellInitParseError::Help);
        }
        if token == "--no-color" || token.starts_with("--format=") {
            i += 1;
            continue;
        }
        if token == "--format" {
            if i + 1 < tokens.len() {
                i += 2;
                continue;
            }
            return Err(ShellInitParseError::Usage(format!(
                "bitty {invoked_as}: --format needs a value (table|json|jsonl)\n{}",
                shell_init_usage()
            )));
        }
        if shell.is_none() {
            match CompletionShell::parse(token) {
                Some(parsed) => shell = Some(parsed),
                None => {
                    return Err(ShellInitParseError::Usage(format!(
                        "bitty {invoked_as}: unknown shell {token:?} (want bash|zsh|fish|powershell|nushell)\n{}",
                        shell_init_usage()
                    )));
                }
            }
        } else {
            return Err(ShellInitParseError::Usage(format!(
                "bitty {invoked_as}: unexpected argument {token:?}\n{}",
                shell_init_usage()
            )));
        }
        i += 1;
    }
    match shell {
        Some(shell) => Ok(ShellInitRequest { shell }),
        None => Err(ShellInitParseError::Usage(format!(
            "bitty {invoked_as}: missing <shell>\n{}",
            shell_init_usage()
        ))),
    }
}

/// Execute a validated request; returns the process exit code.
pub fn run_shell_init(request: &ShellInitRequest) -> i32 {
    print!("{}", shell_init_script(request.shell));
    EXIT_OK
}

/// Runs `bitty shell-init`; returns the process exit code.
///
/// - `--help` prints help to stdout, exit 0, and never needs an instance.
/// - Extra positionals, unknown shells/flags, and stray `--` fail closed
///   (exit 2, stderr only, no stdout script).
/// - `--socket`/`--instance` combined with `shell-init` are usage errors.
pub(crate) fn run_cli(args: &Args) -> i32 {
    if args.ctl_socket_pre.is_some()
        || args.list_socket.is_some()
        || args.dev_socket_pre.is_some()
        || args.ctl_instance_pre.is_some()
        || args.list_instance.is_some()
        || args.dev_instance_pre.is_some()
    {
        eprintln!(
            "bitty shell-init: --socket/--instance need a runtime command (shell-init is local)\n{}",
            shell_init_usage()
        );
        return EXIT_USAGE;
    }
    match parse_shell_init_request(&args.shell_init_raw, "shell-init") {
        Err(ShellInitParseError::Help) => {
            print!("{}", shell_init_help_text());
            EXIT_OK
        }
        Err(err) => {
            eprintln!("{}", err.message());
            EXIT_USAGE
        }
        Ok(request) => run_shell_init(&request),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    const SHELLS: [CompletionShell; 5] = [
        CompletionShell::Bash,
        CompletionShell::Zsh,
        CompletionShell::Fish,
        CompletionShell::Powershell,
        CompletionShell::Nushell,
    ];

    #[test]
    fn shells_parse_case_insensitive_with_aliases() {
        assert_eq!(
            parse_shell_init_request(&tokens(&["Bash"]), "shell-init").unwrap(),
            ShellInitRequest {
                shell: CompletionShell::Bash
            }
        );
        assert_eq!(
            parse_shell_init_request(&tokens(&["pwsh"]), "shell-init")
                .unwrap()
                .shell,
            CompletionShell::Powershell
        );
        assert_eq!(
            parse_shell_init_request(&tokens(&["nu"]), "shell-init")
                .unwrap()
                .shell,
            CompletionShell::Nushell
        );
    }

    #[test]
    fn scripts_carry_hooks_and_inlined_completion() {
        for shell in SHELLS {
            let script = shell_init_script(shell);
            assert!(script.contains("bitty"), "shell: {shell:?}");
            for mark in ["]133;A", "]133;B", "]133;D;"] {
                assert!(
                    script.contains(mark),
                    "shell {shell:?} emits OSC 133 {mark:?}"
                );
            }
            assert!(script.contains("]7;"), "shell {shell:?} reports OSC 7 cwd");
            assert!(
                script.contains(completion_script(shell).trim()),
                "shell {shell:?} inlines the matching completion script"
            );
        }
    }

    #[test]
    fn scripts_never_invoke_bitty_at_startup() {
        // #2372: sourcing must not fail when `bitty` is absent or older.
        // Only comment lines may mention `bitty completion` / `bitty shell-init`.
        for shell in SHELLS {
            let script = shell_init_script(shell);
            for line in script.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with('#') || trimmed.starts_with("extern \"bitty") {
                    continue;
                }
                assert!(
                    !trimmed.contains("bitty completion") && !trimmed.contains("bitty shell-init"),
                    "shell {shell:?} invokes bitty while sourcing: {line:?}"
                );
            }
        }
    }

    #[test]
    fn every_install_line_is_emitted_verbatim() {
        for shell in SHELLS {
            assert!(
                shell_init_script(shell).contains(install_line(shell)),
                "shell {shell:?} header/help names its install line"
            );
        }
    }

    #[test]
    fn bash_hook_runs_first_and_keeps_array_prompt_command() {
        let script = shell_init_script(CompletionShell::Bash);
        assert!(
            script.contains(
                r#"PROMPT_COMMAND="_bitty_prompt_hook${PROMPT_COMMAND:+; $PROMPT_COMMAND}""#
            ),
            "bash prepends the hook before existing PROMPT_COMMAND"
        );
        assert!(
            !script.contains(
                r#"PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND; }_bitty_prompt_hook""#
            ),
            "bash no longer appends the hook after existing PROMPT_COMMAND"
        );
        assert!(
            script.contains("declare -a "),
            "bash detects array-valued PROMPT_COMMAND"
        );
        assert!(
            script.contains("PROMPT_COMMAND=(_bitty_prompt_hook"),
            "bash prepends the hook to array-valued PROMPT_COMMAND"
        );
    }

    #[test]
    fn zsh_registers_completion_and_preexec_once() {
        let script = shell_init_script(CompletionShell::Zsh);
        assert!(
            script.contains("compdef _bitty bitty"),
            "zsh registers _bitty via compdef"
        );
        assert!(
            script.contains("$+functions[compdef]"),
            "zsh only calls compdef when the completion system is loaded"
        );
        assert!(
            script.contains("preexec_functions+=(_bitty_preexec_hook)"),
            "zsh registers the command-start hook"
        );
        assert!(
            script.contains("precmd_functions[(Ie)_bitty_prompt_hook]"),
            "zsh registers the prompt hook exactly once"
        );
    }

    #[test]
    fn fish_registers_prompt_preexec_and_completion_once() {
        let script = shell_init_script(CompletionShell::Fish);
        assert!(
            script.contains("--on-event fish_prompt"),
            "fish hooks every prompt"
        );
        assert!(
            script.contains("--on-event fish_preexec"),
            "fish marks command start"
        );
        assert!(
            script.contains("if not set -q _BITTY_COMPLETION_FISH"),
            "fish registers completion exactly once"
        );
    }

    #[test]
    fn powershell_is_strict_mode_safe() {
        let script = shell_init_script(CompletionShell::Powershell);
        // StrictMode throws on an unset variable; LASTEXITCODE is only
        // defined after the first native command.
        assert!(
            script.contains("Test-Path variable:global:LASTEXITCODE"),
            "powershell guards LASTEXITCODE under StrictMode"
        );
        assert!(
            script.contains("_BittyCompletionRegistered"),
            "powershell registers completion exactly once"
        );
        assert!(
            script.contains("PSReadLine"),
            "powershell marks command start via PSReadLine"
        );
        assert!(
            script.contains("CommandValidationHandler"),
            "powershell marks every accepted line, including history duplicates"
        );
    }

    #[test]
    fn bash_ps0_emits_command_start_mark() {
        let script = shell_init_script(CompletionShell::Bash);
        assert!(script.contains("PS0="), "bash sets PS0 for command start");
        assert!(script.contains("]133;B"), "PS0 carries the B mark");
    }

    #[test]
    fn osc7_cwd_is_percent_encoded() {
        let bash = shell_init_script(CompletionShell::Bash);
        assert!(bash.contains("_bitty_urlencode"), "bash defines encoder");
        assert!(
            bash.contains(r#"$(_bitty_urlencode "${PWD:-.}")"#),
            "bash encodes PWD for OSC 7"
        );
        assert!(bash.contains("%%%02X"), "bash pct-encodes via printf");
        assert!(bash.contains("%3A"), "bash encodes non-drive colons");
        assert!(
            bash.contains(r#""${HOSTNAME:-$(uname -n 2>/dev/null)}""#),
            "bash passes hostname through verbatim"
        );

        let zsh = shell_init_script(CompletionShell::Zsh);
        assert!(zsh.contains("_bitty_urlencode"), "zsh defines encoder");
        assert!(
            zsh.contains(r#"$(_bitty_urlencode "${PWD:-.}")"#),
            "zsh encodes PWD for OSC 7"
        );
        assert!(zsh.contains("%%%02X"), "zsh pct-encodes via printf");
        assert!(zsh.contains("%3A"), "zsh encodes non-drive colons");

        let fish = shell_init_script(CompletionShell::Fish);
        assert!(
            fish.contains("string escape --style=url"),
            "fish encodes via string escape"
        );
        assert!(
            fish.contains("(_bitty_urlencode (pwd))"),
            "fish encodes pwd for OSC 7"
        );
        assert!(
            fish.contains("(hostname 2>/dev/null)"),
            "fish passes hostname through verbatim"
        );
        assert!(fish.contains("%3[Aa]"), "fish preserves drive-letter colon");

        let pwsh = shell_init_script(CompletionShell::Powershell);
        assert!(
            pwsh.contains("_BittyUrlEncode"),
            "powershell defines encoder"
        );
        assert!(
            pwsh.contains("EscapeDataString"),
            "powershell pct-encodes via EscapeDataString"
        );
        assert!(
            pwsh.contains(r#"-replace '\\', '/'"#),
            "powershell normalizes backslashes"
        );
        assert!(
            pwsh.contains("^[A-Za-z]:$"),
            "powershell preserves drive-letter colon"
        );
        assert!(
            pwsh.contains("$i -eq 0 -and $parts[$i] -match '^[A-Za-z]:$'"),
            "powershell restricts drive colon to the first segment (/tmp/C:/x keeps %3A)"
        );
        assert!(
            pwsh.contains("$host_name$uri_path"),
            "powershell emits encoded path without raw cwd"
        );

        let nu = shell_init_script(CompletionShell::Nushell);
        assert!(nu.contains("url encode"), "nushell encodes via url encode");
        assert!(
            nu.contains("str replace --all ':' '%3A'"),
            "nushell encodes non-drive colons"
        );
        assert!(
            nu.contains("^([A-Za-z])%3A/"),
            "nushell preserves drive-letter colon without sentinel"
        );
        assert!(
            !nu.contains("__BITTY_DRIVE__"),
            "nushell avoids filename-colliding sentinel"
        );
        assert!(
            nu.contains("sys host | get hostname"),
            "nushell passes hostname through verbatim"
        );
    }

    #[test]
    fn nushell_inlines_completion_without_save_then_source() {
        let script = shell_init_script(CompletionShell::Nushell);
        let code_lines: Vec<&str> = script
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect();
        assert!(
            !code_lines.iter().any(|line| line.contains("save -f")),
            "nushell init must not save a completion file at runtime"
        );
        assert!(
            !code_lines
                .iter()
                .any(|line| line.split_whitespace().any(|word| word == "source")),
            "nushell init must not source a just-written file at runtime"
        );
        for marker in ["extern \"bitty\"", "extern \"bitty ctl\""] {
            assert!(
                script.contains(marker),
                "nushell init inlines completion ({marker:?} missing)"
            );
        }
        assert!(
            script.contains(completion_script(CompletionShell::Nushell).trim()),
            "nushell init shares the bitty completion nushell output"
        );
    }

    #[test]
    fn nushell_guard_probes_hooks_not_env() {
        let script = shell_init_script(CompletionShell::Nushell);
        assert!(
            !script.contains("BITTY_SHELL_INIT"),
            "nushell init must not key hook registration on an inherited $env var"
        );
        assert!(
            script.contains(r#"if not (try { $env.config.hooks.pre_prompt?"#),
            "nushell guard probes the session's own pre_prompt hooks"
        );
        assert!(
            script.contains(r##"str contains "# bitty-shell-init""##),
            "nushell guard looks for the full marker, not the bare filename stem"
        );
        assert!(
            script.contains("any {|h| try { $h | to nuon --serialize"),
            "nushell guard isolates per-hook serialization failures"
        );
        let register_pos = script.find("upsert hooks.pre_prompt").unwrap_or(usize::MAX);
        assert!(
            register_pos < script.len() && script[register_pos..].contains("# bitty-shell-init"),
            "nushell marker sits inside the registered hook closure"
        );
    }

    #[test]
    fn shell_positional_parses_format_ignored() {
        let req = parse_shell_init_request(&tokens(&["bash"]), "shell-init").unwrap();
        assert_eq!(req.shell, CompletionShell::Bash);
        let req =
            parse_shell_init_request(&tokens(&["--format", "json", "fish"]), "shell-init").unwrap();
        assert_eq!(req.shell, CompletionShell::Fish);
        let req = parse_shell_init_request(&tokens(&["zsh", "--no-color"]), "shell-init").unwrap();
        assert_eq!(req.shell, CompletionShell::Zsh);
    }

    #[test]
    fn missing_unknown_extra_and_separator_fail() {
        for words in [
            vec![],
            vec!["tcl"],
            vec!["bash", "zsh"],
            vec!["--"],
            vec!["--bogus"],
            vec!["bash", "--bogus"],
        ] {
            assert!(
                matches!(
                    parse_shell_init_request(&tokens(&words), "shell-init"),
                    Err(ShellInitParseError::Usage(_))
                ),
                "words: {words:?}"
            );
        }
        assert_eq!(
            parse_shell_init_request(&tokens(&["--help"]), "shell-init"),
            Err(ShellInitParseError::Help)
        );
    }

    #[test]
    fn help_names_shells_and_wiring() {
        let text = shell_init_help_text();
        for shell in ["bash", "zsh", "fish", "powershell", "nushell"] {
            assert!(text.contains(shell), "help names {shell}");
        }
        for line in [
            install_line(CompletionShell::Bash),
            install_line(CompletionShell::Zsh),
            install_line(CompletionShell::Fish),
            install_line(CompletionShell::Powershell),
            install_line(CompletionShell::Nushell),
        ] {
            assert!(
                text.contains(line),
                "help names working install line {line:?}"
            );
        }
        assert!(text.contains("OSC 7"), "help names cwd report");
        assert!(text.contains("OSC 133"), "help names status marks");
    }
}
