//! `bitty shell-init`: emit shell integration (CTX-1054, issue #1813).
//!
//! Canonical: issue #1813 (`bitty completion <shell>` already emits static
//! completion scripts, but nothing installs or wires them).
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
//!   plus OSC 133 prompt-start `A` and command-done-with-status `D` marks,
//!   observation only) followed by one eval line that wires the matching
//!   `bitty completion <shell>` output, so Tab completion works after init.
//! - `--format` is accepted and ignored: shell-init emits a script, never the
//!   output envelope. `--no-color` is accepted and ignored (scripts carry no
//!   color). `--socket`/`--instance` combined with `shell-init` fail closed
//!   (exit 2): they select a runtime target this command never uses.
//! - Missing/unknown shell, extra positionals, unknown flags, and stray `--`
//!   fail closed (exit 2, stderr only, no stdout script).
//!
//! # Wiring (the #1813 acceptance)
//!
//! - bash: `eval "$(bitty shell-init bash)"` in `~/.bashrc`.
//! - zsh: `eval "$(bitty shell-init zsh)"` in `~/.zshrc`.
//! - fish: `bitty shell-init fish | source` in `config.fish`.
//! - powershell: `bitty shell-init powershell | Out-String | Invoke-Expression`
//!   in `$PROFILE`.
//! - nushell: save once and source from `config.nu` (see the script header).
//!
//! `bitty init` prints this hint after writing the config file, so a fresh
//! shell gets working Tab completion plus cwd/status prompt marks after init.
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
// Static scripts (prompt hooks + completion wiring; plugin subtrees stay out)
// ---------------------------------------------------------------------------

/// Bash integration: `PROMPT_COMMAND` hook (OSC 133 `D` status, OSC 7 cwd,
/// OSC 133 `A` prompt-start) plus `bitty completion bash`.
const BASH_SCRIPT: &str = r#"# bitty shell integration for Bash (static; generated, do not edit).
# Enable with: eval "$(bitty shell-init bash)" in ~/.bashrc
# - Tab completion via `bitty completion bash` (evaled below).
# - Prompt hooks: OSC 7 cwd report plus OSC 133 prompt-start (A) and
#   command-done-with-status (D) marks (observation only; ignored by
#   terminals without support).
# - OSC 7 cwd is percent-encoded (RFC 3986 unreserved plus slash kept).
#   Hostname passes through verbatim (DNS-safe, authority must match).
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
        printf '\e]7;file://%s%s\e\\' "$HOSTNAME" "$(_bitty_urlencode "$PWD")"
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
fi
eval "$(bitty completion bash)"
"#;

/// Zsh integration: `precmd` hook (OSC 133 `D` status, OSC 7 cwd, OSC 133
/// `A` prompt-start) plus `bitty completion zsh`.
const ZSH_SCRIPT: &str = r#"# bitty shell integration for Zsh (static; generated, do not edit).
# Enable with: eval "$(bitty shell-init zsh)" in ~/.zshrc (after compinit,
# so compdef can register Tab completion).
# - Tab completion via `bitty completion zsh` (evaled below).
# - Prompt hooks: OSC 7 cwd report plus OSC 133 prompt-start (A) and
#   command-done-with-status (D) marks (observation only; ignored by
#   terminals without support).
# - OSC 7 cwd is percent-encoded (RFC 3986 unreserved plus slash kept).
#   Hostname passes through verbatim (DNS-safe, authority must match).
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
        printf '\e]7;file://%s%s\e\\' "$HOST" "$(_bitty_urlencode "$PWD")"
    }
    _bitty_prompt_hook() {
        local _bitty_status=$?
        printf '\e]133;D;%s\e\\' "$_bitty_status"
        _bitty_osc7
        printf '\e]133;A\e\\'
    }
    if (( ! ${precmd_functions[(Ie)_bitty_prompt_hook]} )); then
        precmd_functions+=(_bitty_prompt_hook)
    fi
fi
eval "$(bitty completion zsh)"
if (( $+functions[compdef] )); then
    compdef _bitty bitty
fi
"#;

/// Fish integration: `fish_prompt`-event hook (OSC 133 `D` status, OSC 7
/// cwd, OSC 133 `A` prompt-start) plus `bitty completion fish`.
const FISH_SCRIPT: &str = r#"# bitty shell integration for Fish (static; generated, do not edit).
# Enable with: bitty shell-init fish | source in config.fish
# - Tab completion via `bitty completion fish` (sourced below).
# - Prompt hooks: OSC 7 cwd report plus OSC 133 prompt-start (A) and
#   command-done-with-status (D) marks (observation only; ignored by
#   terminals without support).
# - OSC 7 cwd is percent-encoded (RFC 3986 unreserved plus slash kept).
#   Hostname passes through verbatim (DNS-safe, authority must match).
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
        printf '\e]7;file://%s%s\e\\' (hostname) (_bitty_urlencode (pwd))
        printf '\e]133;A\e\\'
    end
end
bitty completion fish | source
"#;

/// PowerShell integration: wrapped `prompt` (OSC 133 `D` status, OSC 7 cwd,
/// OSC 133 `A` prompt-start; the previous prompt is chained, never dropped)
/// plus `bitty completion powershell`.
const POWERSHELL_SCRIPT: &str = r#"# bitty shell integration for PowerShell (static; generated, do not edit).
# Enable with: bitty shell-init powershell | Out-String | Invoke-Expression in $PROFILE
# - Tab completion via `bitty completion powershell` (invoked below).
# - Prompt hooks: OSC 7 cwd report plus OSC 133 prompt-start (A) and
#   command-done-with-status (D) marks (observation only; ignored by
#   terminals without support).
# - OSC 7 cwd is percent-encoded (RFC 3986 unreserved plus slash kept).
#   Hostname passes through verbatim (DNS-safe, authority must match).
if (-not (Test-Path variable:global:_BittyShellInit)) {
    $global:_BittyShellInit = $true
    $global:_BittyPromptChain = (Get-Command prompt -CommandType Function -ErrorAction SilentlyContinue).ScriptBlock
    # Encode a path for OSC 7: backslashes become slashes, then each
    # segment is escaped. Drive-letter colon (C:) survives for file URIs.
    function Global:_BittyUrlEncode([string]$Path) {
        $normalized = $Path -replace '\\', '/'
        $parts = $normalized.Split('/')
        for ($i = 0; $i -lt $parts.Length; $i++) {
            if ($parts[$i] -match '^[A-Za-z]:$') {
                continue
            }
            $parts[$i] = [System.Uri]::EscapeDataString($parts[$i])
        }
        return ($parts -join '/')
    }
    function global:prompt {
        # Prefer the native exit code: $? alone collapses every native
        # failure to 1 (a native command exiting 42 must report D;42).
        $status = if ($?) { 0 } elseif ($LASTEXITCODE) { $LASTEXITCODE } else { 1 }
        $host_name = if ($env:COMPUTERNAME) { $env:COMPUTERNAME } else { hostname }
        $uri_path = _BittyUrlEncode (Get-Location).Path
        if ($uri_path -notmatch '^/') { $uri_path = '/' + $uri_path }
        $esc = [char]27
        Write-Host -NoNewline "$esc]133;D;$status$esc\"
        Write-Host -NoNewline "$esc]7;file://$host_name$uri_path$esc\"
        Write-Host -NoNewline "$esc]133;A$esc\"
        if ($global:_BittyPromptChain) { & $global:_BittyPromptChain } else { "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) " }
    }
}
bitty completion powershell | Out-String | Invoke-Expression
"#;

/// Nushell integration: `pre_prompt` hook (OSC 133 `D` status, OSC 7 cwd,
/// OSC 133 `A` prompt-start; existing hooks are kept) plus the inlined
/// `bitty completion nushell` definitions.
///
/// The completion definitions are inlined (shared with the
/// `bitty completion nushell` output) because Nushell resolves `source`
/// paths at parse time: a `save -f` then `source` roundtrip of a just-written
/// file cannot work on first setup — the file does not exist yet when the
/// `source` line is parsed.
const NUSHELL_HEADER: &str = r#"# bitty shell integration for Nushell (static; generated, do not edit).
# Enable with: bitty shell-init nushell | save -f ~/.config/nushell/bitty-shell-init.nu
# then add `source ~/.config/nushell/bitty-shell-init.nu` to config.nu
# - Tab completion inlined below (same output as `bitty completion nushell`).
#   Inlined because Nushell parses `source` paths before running `save -f`,
#   so a save-then-source roundtrip of a just-written file cannot load.
# - Prompt hooks: OSC 7 cwd report plus OSC 133 prompt-start (A) and
#   command-done-with-status (D) marks (observation only; ignored by
#   terminals without support).
# - OSC 7 cwd is percent-encoded (RFC 3986 unreserved plus slash kept).
#   Hostname passes through verbatim (DNS-safe, authority must match).
#   Drive-letter colon (C:/) survives for the file URI convention.
if "BITTY_SHELL_INIT" not-in $env {
    $env.BITTY_SHELL_INIT = "1"
    $env.config = ($env.config | default {} hooks | upsert hooks.pre_prompt ((try { $env.config.hooks.pre_prompt } catch { [] }) ++ [{||
        print -n $"\e]133;D;($env.LAST_EXIT_CODE)\e\\"
        print -n $"\e]7;file://(sys host | get hostname)(pwd | str replace --all "\\" "/" | url encode | str replace --regex '^([A-Za-z]):/' '$1__BITTY_DRIVE__/' | str replace --all ':' '%3A' | str replace --all '__BITTY_DRIVE__' ':')\e\\"
        print -n "\e]133;A\e\\"
    }]))
}
"#;

/// Static shell-integration script for a shell (hooks + completion wiring).
///
/// Nushell returns an owned script: the prompt-hook header plus the shared
/// `bitty completion nushell` definitions inlined, so no parse-time `source`
/// of a just-written file is needed.
#[must_use]
pub fn shell_init_script(shell: CompletionShell) -> Cow<'static, str> {
    match shell {
        CompletionShell::Bash => Cow::Borrowed(BASH_SCRIPT),
        CompletionShell::Zsh => Cow::Borrowed(ZSH_SCRIPT),
        CompletionShell::Fish => Cow::Borrowed(FISH_SCRIPT),
        CompletionShell::Powershell => Cow::Borrowed(POWERSHELL_SCRIPT),
        CompletionShell::Nushell => Cow::Owned(format!(
            "{NUSHELL_HEADER}{}",
            completion_script(CompletionShell::Nushell)
        )),
    }
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
         The script wires Tab completion (via the matching `bitty completion <shell>`\n\
         output) and installs prompt hooks that report cwd (OSC 7) plus\n\
         prompt-start and exit-status marks (OSC 133) for shell-integration parity.\n\
         \n\
         Enable it from your shell startup file:\n\
         \x20 bash:       eval \"$(bitty shell-init bash)\"  (~/.bashrc)\n\
         \x20 zsh:        eval \"$(bitty shell-init zsh)\"  (~/.zshrc)\n\
         \x20 fish:       bitty shell-init fish | source  (config.fish)\n\
         \x20 powershell: bitty shell-init powershell | Out-String | Invoke-Expression  ($PROFILE)\n\
         \x20 nushell:    save once, then `source` it from config.nu (see the script header).\n",
        shell_list()
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
    fn scripts_carry_hooks_and_completion_wiring() {
        let wiring = [
            (CompletionShell::Bash, "bitty completion bash"),
            (CompletionShell::Zsh, "bitty completion zsh"),
            (CompletionShell::Fish, "bitty completion fish"),
            (CompletionShell::Powershell, "bitty completion powershell"),
            (CompletionShell::Nushell, "bitty completion nushell"),
        ];
        for (shell, wire) in wiring {
            let script = shell_init_script(shell);
            assert!(script.contains("bitty"), "shell: {shell:?}");
            assert!(
                script.contains("]133;"),
                "shell {shell:?} marks OSC 133 prompt/status"
            );
            assert!(script.contains("]7;"), "shell {shell:?} reports OSC 7 cwd");
            assert!(
                script.contains(wire),
                "shell {shell:?} wires Tab completion via {wire:?}"
            );
        }
        // Every script guards its hook wiring with a shell-native
        // conditional, so re-sourcing never duplicates hooks. Each guard
        // must textually precede the hook it protects (marker presence
        // alone is not enough: an unguarded marker would pass that check
        // while still duplicating hooks on re-source).
        let guards = [
            (
                CompletionShell::Bash,
                r#"if [ -z "${_BITTY_SHELL_INIT:-}" ]"#,
                "_bitty_prompt_hook",
            ),
            (
                CompletionShell::Zsh,
                "if (( ! ${+_BITTY_SHELL_INIT} ))",
                "_bitty_prompt_hook",
            ),
            (
                CompletionShell::Fish,
                "if not set -q _BITTY_SHELL_INIT",
                "_bitty_prompt_hook",
            ),
            (
                CompletionShell::Powershell,
                "if (-not (Test-Path variable:global:_BittyShellInit))",
                "global:prompt",
            ),
            (
                CompletionShell::Nushell,
                r#"if "BITTY_SHELL_INIT" not-in $env"#,
                "pre_prompt",
            ),
        ];
        for (shell, guard, hook) in guards {
            let script = shell_init_script(shell);
            assert!(
                script.contains(guard),
                "shell {shell:?} guards double-sourcing with {guard:?}"
            );
            let guard_pos = script.find(guard).unwrap_or(usize::MAX);
            let hook_pos = script.find(hook).unwrap_or(0);
            assert!(
                guard_pos < hook_pos,
                "shell {shell:?} guard precedes hook wiring"
            );
        }
    }

    #[test]
    fn bash_hook_runs_first_and_keeps_array_prompt_command() {
        let script = shell_init_script(CompletionShell::Bash);
        // The hook must precede existing prompt commands: anything running
        // before it would overwrite $? and mask the previous command's
        // real status (verified: append form captures 0 after `false`
        // when a prior entry succeeds; prepend form captures 1).
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
        // Array-valued PROMPT_COMMAND keeps every existing entry.
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
    fn zsh_registers_completion_via_compdef() {
        let script = shell_init_script(CompletionShell::Zsh);
        // Evaluating the completion body only defines _bitty; Tab
        // completion needs compdef registration once compinit has run.
        assert!(
            script.contains("compdef _bitty bitty"),
            "zsh registers _bitty via compdef"
        );
        assert!(
            script.contains("$+functions[compdef]"),
            "zsh only calls compdef when the completion system is loaded"
        );
    }

    #[test]
    fn powershell_reports_native_exit_code() {
        let script = shell_init_script(CompletionShell::Powershell);
        // $? is boolean: a native command exiting 42 must report D;42,
        // not D;1. A zero/unset LASTEXITCODE still falls back to 1 so a
        // failed cmdlet after a successful native command is not masked.
        assert!(
            script.contains("$LASTEXITCODE"),
            "powershell prefers the native exit code"
        );
    }

    #[test]
    fn osc7_cwd_is_percent_encoded() {
        // CTX-1074: every hook must encode cwd before OSC 7 insertion.
        // Table: RFC 3986 unreserved plus slash kept, rest pct-encoded.
        // Hostname passes through verbatim. Drive colon survives.
        let bash = shell_init_script(CompletionShell::Bash);
        assert!(bash.contains("_bitty_urlencode"), "bash defines encoder");
        assert!(
            bash.contains(r#"$(_bitty_urlencode "$PWD")"#),
            "bash encodes PWD for OSC 7"
        );
        assert!(bash.contains("%%%02X"), "bash pct-encodes via printf");
        assert!(bash.contains("%3A"), "bash encodes non-drive colons");
        assert!(
            bash.contains(r#""$HOSTNAME""#),
            "bash passes hostname through verbatim"
        );

        let zsh = shell_init_script(CompletionShell::Zsh);
        assert!(zsh.contains("_bitty_urlencode"), "zsh defines encoder");
        assert!(
            zsh.contains(r#"$(_bitty_urlencode "$PWD")"#),
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
            fish.contains("(hostname)"),
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
            nu.contains("__BITTY_DRIVE__"),
            "nushell preserves drive-letter colon"
        );
        assert!(
            nu.contains("sys host | get hostname"),
            "nushell passes hostname through verbatim"
        );
    }

    #[test]
    fn nushell_inlines_completion_without_save_then_source() {
        // Nushell parses `source` paths before running `save -f`, so a
        // save-then-source roundtrip of a just-written file cannot load on
        // first setup. The init script inlines the shared completion
        // definitions instead (comment lines still name the install step).
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
        for marker in ["nu-complete bitty commands", "extern \"bitty\""] {
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
        assert!(text.contains("bitty completion"), "help names wiring");
        assert!(text.contains("OSC 7"), "help names cwd report");
        assert!(text.contains("OSC 133"), "help names status marks");
    }
}
