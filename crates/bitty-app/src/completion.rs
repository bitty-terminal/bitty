//! `bitty completion`: emit shell completion (CTX-0763, issue #1375).
//!
//! Canonical: `docs/specifications/cli-contract-rfc.md` (`bitty completion`,
//! local class, Bash/Zsh/Fish/PowerShell/Nushell) with the stable `comp`
//! alias.
//!
//! # Contract (implemented)
//!
//! - Shape: `bitty completion <shell>` (alias `bitty comp <shell>`), where
//!   `<shell>` is one of `bash|zsh|fish|powershell|nushell` (`pwsh` accepted
//!   for PowerShell). The script prints to stdout, exit 0.
//! - Class: local (no instance, no config load, no plugin VM, safe-mode
//!   clean). Completion is static: core tokens and global flags from the v1
//!   tree. Dynamic plugin-subtree entries derived from installed manifests
//!   (regenerated when the plugin set changes, no VM load) are a follow-up.
//! - `--format` is accepted and ignored: completion emits a script, never the
//!   output envelope. `--no-color` is accepted and ignored (scripts carry no
//!   color). `--socket`/`--instance` combined with `completion` fail closed
//!   (exit 2): they select a runtime target this command never uses.
//! - Missing/unknown shell, extra positionals, unknown flags, and stray `--`
//!   fail closed (exit 2, stderr only, no stdout script).
//!
//! # Exit codes (stable taxonomy)
//!
//! - `0` success.
//! - `2` usage error (missing/unknown shell, extra positional, unknown flag,
//!   bad `--socket`/`--instance` shape, stray `--`).

#![forbid(unsafe_code)]

use crate::cli::Args;

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success.
pub const EXIT_OK: i32 = 0;
/// CLI usage error.
pub const EXIT_USAGE: i32 = 2;

// ---------------------------------------------------------------------------
// Shells
// ---------------------------------------------------------------------------

/// Shells with static completion scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionShell {
    /// Bourne-again shell.
    Bash,
    /// Z shell.
    Zsh,
    /// Friendly interactive shell.
    Fish,
    /// PowerShell (`powershell` or `pwsh`).
    Powershell,
    /// Nushell.
    Nushell,
}

impl CompletionShell {
    /// Parse a shell token (case-insensitive, trimmed).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            "powershell" | "pwsh" => Some(Self::Powershell),
            "nushell" | "nu" => Some(Self::Nushell),
            _ => None,
        }
    }

    /// Supported shells in help order.
    #[must_use]
    pub fn all() -> &'static [&'static str] {
        &["bash", "zsh", "fish", "powershell", "nushell"]
    }
}

/// Supported shells in help order, comma-joined for usage/help text.
#[must_use]
pub fn shell_list() -> String {
    CompletionShell::all().join(", ")
}

/// Static core subcommand tokens completed by every script (v1 tree plus the
/// stable `ls`/`cfg`/`comp` aliases and the extra `init` wizard word).
/// Test-only single source: [`completion_commands`] plus the per-script test
/// keep every static script in sync with this table.
#[cfg(test)]
const COMPLETION_COMMANDS: &str =
    "run ctl config cfg plugin list ls inspect dev doctor cmd x completion comp version init";

/// Static global flags completed by every script.
#[cfg(test)]
const COMPLETION_FLAGS: &str = "--help --version --verbose --log-level --headless --test-mode \
    --safe --fail-loud --mascot --no-splash --split --split-ratio --stack --overlay \
    --layout --focus --config --profile --theme --font-family --font-size --opacity \
    --format --socket --instance --no-color --yes --force --scrollback --close-confirm \
    --gaps-in --gaps-out --border --radius";

// ---------------------------------------------------------------------------
// Static scripts (core tree; plugin subtrees are a follow-up)
// ---------------------------------------------------------------------------

/// Bash completion script (static core tree).
const BASH_SCRIPT: &str = r#"# bitty completion for Bash (static v1 core tree; generated, do not edit).
# Install: bitty completion bash >> ~/.bash_completion
_bitty_complete() {
    local cur cmds flags
    cmds="run ctl config cfg plugin list ls inspect dev doctor cmd x completion comp version init"
    flags="--help --version --verbose --log-level --headless --test-mode --safe --fail-loud --mascot --no-splash --split --split-ratio --stack --overlay --layout --focus --config --profile --theme --font-family --font-size --opacity --format --socket --instance --no-color --yes --force --scrollback --close-confirm --gaps-in --gaps-out --border --radius"
    cur="${COMP_WORDS[COMP_CWORD]}"
    if [[ "$cur" == -* ]]; then
        COMPREPLY=($(compgen -W "$flags" -- "$cur"))
    else
        COMPREPLY=($(compgen -W "$cmds" -- "$cur"))
    fi
}
complete -F _bitty_complete bitty
"#;

/// Zsh completion script (static core tree).
const ZSH_SCRIPT: &str = r#"#compdef bitty
# bitty completion for Zsh (static v1 core tree; generated, do not edit).
# Install: bitty completion zsh > ~/.zsh/completions/_bitty
_bitty() {
    local -a cmds flags
    cmds=(run ctl config cfg plugin list ls inspect dev doctor cmd x completion comp version init)
    flags=(--help --version --verbose --log-level --headless --test-mode --safe --fail-loud --mascot --no-splash --split --split-ratio --stack --overlay --layout --focus --config --profile --theme --font-family --font-size --opacity --format --socket --instance --no-color --yes --force --scrollback --close-confirm --gaps-in --gaps-out --border --radius)
    _arguments -C '1: :->cmd' '*: :->args'
    case "$state" in
        cmd) _describe 'bitty command' cmds ;;
        args) _arguments '*: :($flags)' ;;
    esac
}
_bitty "$@"
"#;

/// Fish completion script (static core tree).
const FISH_SCRIPT: &str = r#"# bitty completion for Fish (static v1 core tree; generated, do not edit).
# Install: bitty completion fish > ~/.config/fish/completions/bitty.fish
for cmd in run ctl config cfg plugin list ls inspect dev doctor cmd x completion comp version init
    complete -c bitty -f -n __fish_use_subcommand -a $cmd
end
for flag in --help --version --verbose --log-level --headless --test-mode --safe --fail-loud --mascot --no-splash --split --split-ratio --stack --overlay --layout --focus --config --profile --theme --font-family --font-size --opacity --format --socket --instance --no-color --yes --force --scrollback --close-confirm --gaps-in --gaps-out --border --radius
    complete -c bitty -f -l (string replace -- '--' '' $flag)
end
"#;

/// PowerShell completion script (static core tree).
const POWERSHELL_SCRIPT: &str = r#"# bitty completion for PowerShell (static v1 core tree; generated, do not edit).
# Install: bitty completion powershell | Out-String | Invoke-Expression
Register-ArgumentCompleter -Native -CommandName @('bitty') -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)
    $cmds = @('run','ctl','config','cfg','plugin','list','ls','inspect','dev','doctor','cmd','x','completion','comp','version','init')
    $flags = @('--help','--version','--verbose','--log-level','--headless','--test-mode','--safe','--fail-loud','--mascot','--no-splash','--split','--split-ratio','--stack','--overlay','--layout','--focus','--config','--profile','--theme','--font-family','--font-size','--opacity','--format','--socket','--instance','--no-color','--yes','--force','--scrollback','--close-confirm','--gaps-in','--gaps-out','--border','--radius')
    $pool = if ($wordToComplete.StartsWith('-')) { $flags } else { $cmds }
    $pool | Where-Object { $_ -like "$wordToComplete*" } | ForEach-Object {
        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
    }
}
"#;

/// Nushell completion script (static core tree).
const NUSHELL_SCRIPT: &str = r#"# bitty completion for Nushell (static v1 core tree; generated, do not edit).
# Install: bitty completion nushell | save -f ~/.config/nushell/completions-bitty.nu
def "nu-complete bitty commands" [] {
    [run ctl config cfg plugin list ls inspect dev doctor cmd x completion comp version init]
}
def "nu-complete bitty flags" [] {
    [--help --version --verbose --log-level --headless --test-mode --safe --fail-loud --mascot --no-splash --split --split-ratio --stack --overlay --layout --focus --config --profile --theme --font-family --font-size --opacity --format --socket --instance --no-color --yes --force --scrollback --close-confirm --gaps-in --gaps-out --border --radius]
}
extern "bitty" [
    command?: string@"nu-complete bitty commands",
    ...rest: string@"nu-complete bitty flags",
]
"#;

/// Static completion script for a shell (core v1 tree).
#[must_use]
pub fn completion_script(shell: CompletionShell) -> &'static str {
    match shell {
        CompletionShell::Bash => BASH_SCRIPT,
        CompletionShell::Zsh => ZSH_SCRIPT,
        CompletionShell::Fish => FISH_SCRIPT,
        CompletionShell::Powershell => POWERSHELL_SCRIPT,
        CompletionShell::Nushell => NUSHELL_SCRIPT,
    }
}

/// The static core tokens every script completes (tested to stay in sync).
#[cfg(test)]
#[must_use]
pub fn completion_commands() -> &'static str {
    COMPLETION_COMMANDS
}

/// The static global flags every script completes (tested to stay in sync).
#[cfg(test)]
#[must_use]
pub fn completion_flags() -> &'static str {
    COMPLETION_FLAGS
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// Validated `bitty completion` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionRequest {
    /// Target shell.
    pub shell: CompletionShell,
}

/// Parse failure: help vs usage error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionParseError {
    /// `--help` requested: print [`completion_help_text`] to stdout, exit 0.
    Help,
    /// Usage error: print the message (it already ends with usage) to stderr,
    /// exit 2.
    Usage(String),
}

impl CompletionParseError {
    /// Render the stderr message for [`CompletionParseError::Usage`].
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::Help => String::from("bitty completion: help requested"),
            Self::Usage(message) => message,
        }
    }
}

/// Usage line for `bitty completion` (names the invoked spelling).
#[must_use]
pub fn completion_usage(invoked_as: &str) -> String {
    format!(
        "Usage: bitty {invoked_as} <shell>\nSupported shells: {}",
        shell_list()
    )
}

/// Help text for `bitty completion` (`--help` never needs an instance or VM).
#[must_use]
pub fn completion_help_text(invoked_as: &str) -> String {
    format!(
        "bitty {invoked_as} — emit shell completion (local, no instance)\n\
         \n\
         Usage: bitty {invoked_as} <shell>\n\
         \n\
         Prints a static completion script for <shell> (one of {}) to stdout. Install it with, for example:\n\
         `bitty completion bash >> ~/.bash_completion`. The script covers the\n\
         v1 core tree and global flags; plugin-subtree entries derived from\n\
         installed manifests are a follow-up. Alias `comp` behaves identically.\n",
        shell_list()
    )
}

/// Validate post-`completion` tokens.
///
/// `invoked_as` is `completion` or the `comp` alias (usage/help name it).
/// `--format` (both spellings) is consumed and ignored: completion emits a
/// script, never the output envelope.
pub fn parse_completion_request(
    tokens: &[String],
    invoked_as: &str,
) -> Result<CompletionRequest, CompletionParseError> {
    let mut shell: Option<CompletionShell> = None;
    let mut i = 0usize;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "-h" || token == "--help" {
            return Err(CompletionParseError::Help);
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
            return Err(CompletionParseError::Usage(format!(
                "bitty {invoked_as}: --format needs a value (table|json|jsonl)\n{}",
                completion_usage(invoked_as)
            )));
        }
        if shell.is_none() {
            match CompletionShell::parse(token) {
                Some(parsed) => shell = Some(parsed),
                None => {
                    return Err(CompletionParseError::Usage(format!(
                        "bitty {invoked_as}: unknown shell {token:?} (want bash|zsh|fish|powershell|nushell)\n{}",
                        completion_usage(invoked_as)
                    )));
                }
            }
        } else {
            return Err(CompletionParseError::Usage(format!(
                "bitty {invoked_as}: unexpected argument {token:?}\n{}",
                completion_usage(invoked_as)
            )));
        }
        i += 1;
    }
    match shell {
        Some(shell) => Ok(CompletionRequest { shell }),
        None => Err(CompletionParseError::Usage(format!(
            "bitty {invoked_as}: missing <shell>\n{}",
            completion_usage(invoked_as)
        ))),
    }
}

/// Execute a validated request; returns the process exit code.
pub fn run_completion(request: &CompletionRequest) -> i32 {
    print!("{}", completion_script(request.shell));
    EXIT_OK
}

/// Runs `bitty completion`; returns the process exit code.
///
/// - `--help` prints help to stdout, exit 0, and never needs an instance.
/// - Extra positionals, unknown shells/flags, and stray `--` fail closed
///   (exit 2, stderr only, no stdout script).
/// - `--socket`/`--instance` combined with `completion` are usage errors.
pub(crate) fn run_cli(args: &Args) -> i32 {
    if args.ctl_socket_pre.is_some()
        || args.list_socket.is_some()
        || args.dev_socket_pre.is_some()
        || args.ctl_instance_pre.is_some()
        || args.list_instance.is_some()
        || args.dev_instance_pre.is_some()
    {
        eprintln!(
            "bitty {}: --socket/--instance need a runtime command (completion is local)\n{}",
            args.completion_spelling,
            completion_usage(&args.completion_spelling)
        );
        return EXIT_USAGE;
    }
    match parse_completion_request(&args.completion_raw, &args.completion_spelling) {
        Err(CompletionParseError::Help) => {
            print!("{}", completion_help_text(&args.completion_spelling));
            EXIT_OK
        }
        Err(err) => {
            eprintln!("{}", err.message());
            EXIT_USAGE
        }
        Ok(request) => run_completion(&request),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn shells_parse_case_insensitive_with_pwsh_alias() {
        assert_eq!(CompletionShell::parse("Bash"), Some(CompletionShell::Bash));
        assert_eq!(
            CompletionShell::parse("pwsh"),
            Some(CompletionShell::Powershell)
        );
        assert_eq!(CompletionShell::parse("nu"), Some(CompletionShell::Nushell));
        assert_eq!(CompletionShell::parse("tcl"), None);
        assert_eq!(CompletionShell::all().len(), 5);
    }

    #[test]
    fn scripts_mention_bitty_and_core_tree() {
        for shell in [
            CompletionShell::Bash,
            CompletionShell::Zsh,
            CompletionShell::Fish,
            CompletionShell::Powershell,
            CompletionShell::Nushell,
        ] {
            let script = completion_script(shell);
            assert!(script.contains("bitty"), "shell: {shell:?}");
            for word in ["run", "ctl", "version", "completion", "comp"] {
                assert!(script.contains(word), "shell {shell:?} missing {word}");
            }
        }
        // The shared token tables stay in sync with every script.
        for word in completion_commands().split_whitespace() {
            for shell in [
                CompletionShell::Bash,
                CompletionShell::Zsh,
                CompletionShell::Fish,
                CompletionShell::Powershell,
                CompletionShell::Nushell,
            ] {
                assert!(
                    completion_script(shell).contains(word),
                    "shell {shell:?} missing {word}"
                );
            }
        }
        for flag in ["--format", "--socket", "--no-color"] {
            assert!(completion_flags().contains(flag), "flags missing {flag}");
        }
    }

    #[test]
    fn shell_positional_parses_format_ignored() {
        let req = parse_completion_request(&tokens(&["bash"]), "completion").unwrap();
        assert_eq!(req.shell, CompletionShell::Bash);
        let req = parse_completion_request(&tokens(&["--format", "json", "fish"]), "comp").unwrap();
        assert_eq!(req.shell, CompletionShell::Fish);
        let req = parse_completion_request(&tokens(&["zsh", "--no-color"]), "completion").unwrap();
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
                    parse_completion_request(&tokens(&words), "completion"),
                    Err(CompletionParseError::Usage(_))
                ),
                "words: {words:?}"
            );
        }
        assert_eq!(
            parse_completion_request(&tokens(&["--help"]), "completion"),
            Err(CompletionParseError::Help)
        );
    }
}
