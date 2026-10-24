//! `bitty completion`: emit shell completion (CTX-0763, issue #1375;
//! nested grammar from issue #2372).
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
//!   clean). Completion covers the full static CLI grammar (first-level
//!   words, nested verbs such as `config profile`, `ctl workspace`,
//!   `plugin config`, `extension`, `dev trace`, and per-command flags). The
//!   grammar tree lives in [`crate::grammar`] and is built from the parser's
//!   own tables, so a new verb reaches every shell at once. Dynamic
//!   plugin-subtree entries derived from installed manifests are a follow-up.
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
use crate::grammar;

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

// ---------------------------------------------------------------------------
// Script generation (single source: crate::grammar)
// ---------------------------------------------------------------------------

/// Bash `case` arms: `path` -> space-joined candidates.
fn bash_case_arms() -> String {
    let mut arms = String::new();
    for node in grammar::nodes() {
        let candidates = node.candidates.join(" ");
        arms.push_str(&format!(
            "        \"{}\") candidates=\"{}\" ;;\n",
            node.path, candidates
        ));
    }
    arms
}

/// Zsh `case` arms: `path` -> array literal.
fn zsh_case_arms() -> String {
    let mut arms = String::new();
    for node in grammar::nodes() {
        let candidates = node.candidates.join(" ");
        arms.push_str(&format!(
            "        \"{}\") candidates=({}) ;;\n",
            node.path, candidates
        ));
    }
    arms
}

/// Fish `switch` arms: `path` -> printf word list.
fn fish_case_arms() -> String {
    let mut arms = String::new();
    for node in grammar::nodes() {
        let candidates = node.candidates.join(" ");
        arms.push_str(&format!(
            "        case '{}'\n            printf '%s\\n' {}\n",
            node.path, candidates
        ));
    }
    arms
}

/// PowerShell ordered-dictionary entries: `'path' = @('a','b',...)`.
fn powershell_case_entries() -> String {
    let mut arms = String::new();
    for node in grammar::nodes() {
        let candidates = node
            .candidates
            .iter()
            .map(|word| format!("'{word}'"))
            .collect::<Vec<_>>()
            .join(",");
        arms.push_str(&format!(
            "            '{}' = @({})\n",
            node.path, candidates
        ));
    }
    arms
}

/// Nushell extern block for one grammar node.
fn nushell_extern(node: &grammar::Node) -> String {
    let mut block = String::new();
    if node.path.is_empty() {
        block.push_str("extern \"bitty\" [\n");
    } else if node.freeform {
        block.push_str(&format!(
            "extern \"bitty {}\" [\n    ...args: string\n",
            node.path
        ));
    } else {
        block.push_str(&format!("extern \"bitty {}\" [\n", node.path));
    }
    for flag in node.candidates.iter().filter(|word| word.starts_with("--")) {
        let value_takes_argument = grammar::value_flags().contains(flag);
        match (*flag, value_takes_argument) {
            ("--help", _) => block.push_str("    --help(-h)\n"),
            ("--version", _) => block.push_str("    --version(-V)\n"),
            (flag, true) => block.push_str(&format!("    {flag}: string\n")),
            (flag, false) => block.push_str(&format!("    {flag}\n")),
        }
    }
    block.push_str("]\n");
    block
}

fn nushell_externs() -> String {
    let nodes = grammar::nodes();
    let mut known: std::collections::HashSet<String> =
        nodes.iter().map(|node| node.path.clone()).collect();
    let mut out = String::new();
    for node in &nodes {
        // The root extern only carries flags: subcommand externs own the
        // positional words, which is how Nushell builds its command tree.
        if node.path.is_empty() || !node.candidates.is_empty() {
            out.push_str(&nushell_extern(node));
        }
    }
    // Every offered child word needs its own extern, otherwise Nushell has
    // no subcommand to suggest (`bitty config <TAB>`). Leaf words with no
    // flags of their own get an empty signature.
    for node in &nodes {
        for candidate in &node.candidates {
            if candidate.starts_with('-') {
                continue;
            }
            let path = if node.path.is_empty() {
                (*candidate).to_string()
            } else {
                format!("{} {}", node.path, candidate)
            };
            if known.insert(path.clone()) {
                out.push_str(&format!("extern \"bitty {path}\" []\n"));
            }
        }
    }
    out
}

fn value_flag_pattern(separator: &str) -> String {
    grammar::value_flags().join(separator)
}

/// Bash completion script (nested grammar, strict-mode safe, idempotent).
fn bash_script() -> String {
    format!(
        r#"# bitty completion for Bash (generated from the CLI grammar; do not edit).
# Install: bitty completion bash > ~/.local/share/bash-completion/completions/bitty
#      or: eval "$(bitty completion bash)" after the bitty binary is on PATH.
_bitty_flag_takes_value() {{
    case "$1" in
        {values}) return 0 ;;
    esac
    return 1
}}
_bitty_complete() {{
    local cur="${{COMP_WORDS[COMP_CWORD]:-}}"
    local path=""
    local word
    local i
    for (( i = 1; i < COMP_CWORD; i++ )); do
        word="${{COMP_WORDS[i]}}"
        case "$word" in
            --*=*) ;;
            -*)
                if _bitty_flag_takes_value "$word"; then
                    i=$((i + 1))
                fi
                ;;
            *)
                if [ -z "$path" ]; then
                    path="$word"
                else
                    path="$path $word"
                fi
                ;;
        esac
    done
    local candidates=""
    case "$path" in
{arms}        *) return 0 ;;
    esac
    # shellcheck disable=SC2207
    COMPREPLY=( $(compgen -W "$candidates" -- "$cur") )
    return 0
}}
complete -o default -F _bitty_complete bitty
"#,
        values = value_flag_pattern("|"),
        arms = bash_case_arms(),
    )
}

/// Zsh completion script (nested grammar, `no_unset` safe, idempotent).
fn zsh_script() -> String {
    format!(
        r#"#compdef bitty
# bitty completion for Zsh (generated from the CLI grammar; do not edit).
# Install: bitty completion zsh > ~/.zfunc/_bitty  (with ~/.zfunc on $fpath)
#      or: eval "$(bitty completion zsh)" after the bitty binary is on PATH.
_bitty_flag_takes_value() {{
    case "$1" in
        {values}) return 0 ;;
    esac
    return 1
}}
_bitty() {{
    local cur="${{words[CURRENT]}}"
    local path=""
    local word
    local i
    for (( i = 2; i < CURRENT; i++ )); do
        word="${{words[i]}}"
        case "$word" in
            --*=*) ;;
            -*)
                if _bitty_flag_takes_value "$word"; then
                    (( i++ ))
                fi
                ;;
            *)
                if [[ -z "$path" ]]; then
                    path="$word"
                else
                    path="$path $word"
                fi
                ;;
        esac
    done
    local -a candidates
    candidates=()
    case "$path" in
{arms}        *) return 1 ;;
    esac
    if (( ${{#candidates}} )); then
        compadd -- "${{candidates[@]}}"
    fi
    return 0
}}
if (( $+functions[compdef] )); then
    compdef _bitty bitty
fi
"#,
        values = value_flag_pattern("|"),
        arms = zsh_case_arms(),
    )
}

/// Fish completion script (nested grammar, non-interactive safe, idempotent).
fn fish_script() -> String {
    format!(
        r#"# bitty completion for Fish (generated from the CLI grammar; do not edit).
# Install: bitty completion fish > ~/.config/fish/completions/bitty.fish
#      or: bitty completion fish | source
function __bitty_flag_takes_value
    switch $argv[1]
        case {values}
            return 0
    end
    return 1
end
function __bitty_path
    set -l tokens (commandline -opc)
    if test (count $tokens) -gt 0
        set -e tokens[1]
    end
    set -l path
    set -l skip_next 0
    for token in $tokens
        if test $skip_next -eq 1
            set skip_next 0
            continue
        end
        switch $token
            case '--*=*'
                continue
            case '--*'
                if __bitty_flag_takes_value $token
                    set skip_next 1
                end
                continue
            case '-*'
                continue
            case '*'
                set -a path $token
        end
    end
    if test (count $path) -eq 0
        return 0
    end
    string join ' ' -- $path
end
function __bitty_complete
    set -l path (__bitty_path)
    switch "$path"
{arms}        case '*'
            return 0
    end
end
if not set -q _BITTY_COMPLETION_FISH
    set -g _BITTY_COMPLETION_FISH 1
    complete -c bitty -f -a '(__bitty_complete)'
end
"#,
        values = value_flag_pattern(" "),
        arms = fish_case_arms(),
    )
}

/// PowerShell completion script (nested grammar, strict-mode safe,
/// registration guarded so double-sourcing is a no-op).
fn powershell_script() -> String {
    format!(
        r#"# bitty completion for PowerShell (generated from the CLI grammar; do not edit).
# Install: bitty completion powershell | Out-String | Invoke-Expression
#      or save it to a file dot-sourced from $PROFILE.
if (-not (Test-Path variable:global:_BittyCompletionRegistered)) {{
    $global:_BittyCompletionRegistered = $true
    $global:_BittyValueFlags = @({values})
    $global:_BittyCompletionScript = {{
        param($wordToComplete, $commandAst, $cursorPosition)
        $paths = [ordered]@{{
{arms}        }}
        $tokens = New-Object System.Collections.Generic.List[string]
        foreach ($element in $commandAst.CommandElements) {{
            if ($element.Extent.EndOffset -lt $cursorPosition) {{
                $tokens.Add($element.Extent.Text.Trim([char]39, [char]34))
            }}
        }}
        if ($tokens.Count -gt 0) {{
            $tokens.RemoveAt(0)
        }}
        $pathTokens = New-Object System.Collections.Generic.List[string]
        for ($i = 0; $i -lt $tokens.Count; $i++) {{
            $token = $tokens[$i]
            if ($token.StartsWith('--')) {{
                if (-not $token.Contains('=') -and ($global:_BittyValueFlags -contains $token)) {{
                    $i++
                }}
                continue
            }}
            if ($token.StartsWith('-')) {{
                continue
            }}
            $pathTokens.Add($token)
        }}
        $path = ($pathTokens -join ' ')
        $candidates = @()
        if ($paths.Contains($path)) {{
            $candidates = $paths[$path]
        }}
        $candidates | Where-Object {{ $_ -like "$wordToComplete*" }} | ForEach-Object {{
            [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
        }}
    }}
    Register-ArgumentCompleter -Native -CommandName @('bitty') -ScriptBlock $global:_BittyCompletionScript
}}
"#,
        values = grammar::value_flags()
            .iter()
            .map(|flag| format!("'{flag}'"))
            .collect::<Vec<_>>()
            .join(","),
        arms = powershell_case_entries(),
    )
}

/// Nushell completion script (nested grammar via one extern per node).
fn nushell_script() -> String {
    format!(
        "# bitty completion for Nushell (generated from the CLI grammar; do not edit).\n\
         # Install: bitty completion nushell | save -f ~/.config/nushell/completions-bitty.nu\n\
         #      then add `source ~/.config/nushell/completions-bitty.nu` to config.nu.\n{}",
        nushell_externs()
    )
}

/// Completion script for a shell, generated from the CLI grammar tree.
#[must_use]
pub fn completion_script(shell: CompletionShell) -> String {
    match shell {
        CompletionShell::Bash => bash_script(),
        CompletionShell::Zsh => zsh_script(),
        CompletionShell::Fish => fish_script(),
        CompletionShell::Powershell => powershell_script(),
        CompletionShell::Nushell => nushell_script(),
    }
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
         Prints a completion script for <shell> (one of {}) to stdout. The script\n\
         completes the full CLI grammar: first-level words, nested verbs\n\
         (config, ctl resources, plugin, extension, dev, inspect, list), and\n\
         per-command flags. Install it with, for example:\n\
         `bitty completion bash > ~/.local/share/bash-completion/completions/bitty`.\n\
         Alias `comp` behaves identically.\n",
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
    use crate::cli::{ConfigCommand, SUBCOMMANDS};
    use crate::component::ComponentVerb;
    use crate::ctl::request::CTL_RESOURCES;
    use crate::dev::{DEV_DUMP_SUBVERBS, DEV_TRACE_SUBVERBS, DEV_VERBS};
    use crate::inspect::InspectTarget;
    use crate::list::ListKind;
    use crate::plugin::{ConfigAction, PluginVerb};

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
    fn every_cli_subcommand_appears_in_every_shell() {
        // Regression for #2372: a top-level word, nested verb, or shell name
        // missing from any emitter is a broken completion.
        for shell in SHELLS {
            let script = completion_script(shell);
            for (name, aliases) in SUBCOMMANDS {
                assert!(script.contains(name), "{shell:?} missing {name}");
                for alias in *aliases {
                    assert!(script.contains(alias), "{shell:?} missing {alias}");
                }
            }
            for (verb, _) in ConfigCommand::VERBS {
                assert!(script.contains(verb), "{shell:?} missing config {verb}");
            }
            for (resource, verbs) in CTL_RESOURCES {
                assert!(
                    script.contains(resource),
                    "{shell:?} missing ctl {resource}"
                );
                for verb in *verbs {
                    assert!(
                        script.contains(verb),
                        "{shell:?} missing ctl {resource} {verb}"
                    );
                }
            }
            for (verb, _) in PluginVerb::VERBS {
                assert!(script.contains(verb), "{shell:?} missing plugin {verb}");
            }
            for (action, _) in ConfigAction::ACTIONS {
                assert!(
                    script.contains(action),
                    "{shell:?} missing plugin config {action}"
                );
            }
            for (verb, _) in ComponentVerb::VERBS {
                assert!(script.contains(verb), "{shell:?} missing extension {verb}");
            }
            for target in InspectTarget::NAMES {
                assert!(
                    script.contains(target),
                    "{shell:?} missing inspect {target}"
                );
            }
            for kind in ListKind::NAMES {
                assert!(script.contains(kind), "{shell:?} missing list {kind}");
            }
            for verb in DEV_VERBS
                .iter()
                .chain(DEV_TRACE_SUBVERBS)
                .chain(DEV_DUMP_SUBVERBS)
            {
                assert!(script.contains(verb), "{shell:?} missing dev {verb}");
            }
            for name in CompletionShell::all() {
                assert!(script.contains(name), "{shell:?} missing shell name {name}");
            }
        }
    }

    #[test]
    fn scripts_offer_nested_paths_not_only_top_level() {
        let bash = completion_script(CompletionShell::Bash);
        assert!(
            bash.contains(r#""config") candidates="path check show validate edit profile"#)
                || bash.contains(r#""config") candidates="path check show validate edit profile "#),
            "bash offers config verbs: {bash}"
        );
        assert!(
            bash.contains(
                r#""ctl workspace") candidates="list new close focus move rename move-panel"#
            ) || bash.contains(
                r#""ctl workspace") candidates="list new close focus move rename move-panel "#
            ),
            "bash offers ctl workspace verbs: {bash}"
        );
        assert!(
            bash.contains(r#""dev trace") candidates="startup latency"#)
                || bash.contains(r#""dev trace") candidates="startup latency "#),
            "bash offers dev trace subverbs: {bash}"
        );
    }

    #[test]
    fn scripts_exclude_hidden_component_alias() {
        for shell in SHELLS {
            let script = completion_script(shell);
            let leaks_alias = script
                .split(|c: char| !c.is_alphanumeric())
                .any(|tok| tok == "component");
            assert!(!leaks_alias, "{shell:?} leaks hidden component alias");
        }
    }

    #[test]
    fn zsh_completion_defines_without_invoking() {
        let script = completion_script(CompletionShell::Zsh);
        assert!(script.contains("_bitty() {"), "zsh defines _bitty");
        assert!(
            script.contains("#compdef bitty"),
            "zsh keeps compdef header"
        );
        assert!(
            !script.contains("_bitty \"$@\""),
            "zsh must not invoke _bitty at eval time"
        );
        assert!(
            script.contains("compdef _bitty bitty"),
            "zsh registers when compinit has run"
        );
    }

    #[test]
    fn completion_registers_once_per_shell() {
        let fish = completion_script(CompletionShell::Fish);
        assert!(
            fish.contains("if not set -q _BITTY_COMPLETION_FISH"),
            "fish guards duplicate complete registration"
        );
        let pwsh = completion_script(CompletionShell::Powershell);
        assert!(
            pwsh.contains("if (-not (Test-Path variable:global:_BittyCompletionRegistered))"),
            "powershell guards duplicate Register-ArgumentCompleter"
        );
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
