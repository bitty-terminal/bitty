//! Ghostty-style `bitty +<action>` helper aliases (CTX-1446, issue #2464).
//!
//! Every action is an alias over an existing implementation; this module owns
//! only the name table and help text. The parser maps each action onto the
//! canonical fields (`list_word` + `list_kind`, `config_word` +
//! `config_cmd`, or `component_word` + `component_raw`), so dispatch runs the
//! exact same code as `bitty list ...`, `bitty config ...`, and
//! `bitty extension list`.
//!
//! Aliases never add authority: static catalogs stay VM-less and network-free,
//! config actions use the startup loader, and `+list-extensions` reuses the
//! extension inventory. All helpers are local-class and safe-mode clean.

#![forbid(unsafe_code)]

use crate::cli::ConfigCommand;
use crate::list::ListKind;

/// One supported `+<action>` alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlusAction {
    /// `+list-themes` -> `bitty list themes`.
    ListThemes,
    /// `+list-colors` -> `bitty list colors`.
    ListColors,
    /// `+list-fonts` -> `bitty list fonts`.
    ListFonts,
    /// `+list-keybinds` -> `bitty list keymaps`.
    ListKeybinds,
    /// `+list-actions` -> `bitty list actions`.
    ListActions,
    /// `+list-plugins` -> `bitty list plugins`.
    ListPlugins,
    /// `+list-extensions` -> `bitty extension list`.
    ListExtensions,
    /// `+show-config` -> `bitty config show`.
    ShowConfig,
    /// `+validate-config` -> `bitty config validate`.
    ValidateConfig,
    /// `+edit-config` -> `bitty config edit` plus post-edit validation.
    EditConfig,
}

impl PlusAction {
    /// Every alias in help order (single source for parsing, help, and
    /// completion; never duplicate this list).
    pub(crate) const ALL: &[(&str, Self)] = &[
        ("+list-themes", Self::ListThemes),
        ("+list-colors", Self::ListColors),
        ("+list-fonts", Self::ListFonts),
        ("+list-keybinds", Self::ListKeybinds),
        ("+list-actions", Self::ListActions),
        ("+list-plugins", Self::ListPlugins),
        ("+list-extensions", Self::ListExtensions),
        ("+show-config", Self::ShowConfig),
        ("+validate-config", Self::ValidateConfig),
        ("+edit-config", Self::EditConfig),
    ];

    /// Parses one exact alias token (case-sensitive, like `+show-font`).
    pub(crate) fn parse(token: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(name, _)| *name == token)
            .map(|(_, action)| *action)
    }

    /// Canonical alias spelling.
    pub(crate) fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(_, action)| *action == self)
            .map_or("+unknown", |(name, _)| *name)
    }

    /// One-line summary for the top-level help.
    pub(crate) fn summary(self) -> &'static str {
        match self {
            Self::ListThemes => "Themes with provenance (built-in + local)",
            Self::ListColors => "16 ANSI + UI tokens with resolved contrast",
            Self::ListFonts => "Discovered system fonts: family, style, source",
            Self::ListKeybinds => "Resolved keybinds (alias for list keymaps)",
            Self::ListActions => "Chrome actions + core commands",
            Self::ListPlugins => "Static bundled plugin catalog",
            Self::ListExtensions => "Installed native extension inventory",
            Self::ShowConfig => "Validated effective config with layer sources",
            Self::ValidateConfig => "Validate config; nonzero when invalid",
            Self::EditConfig => "Edit config in $VISUAL/$EDITOR, then validate",
        }
    }

    /// Canonical list kind for catalog aliases.
    pub(crate) fn list_kind(self) -> Option<ListKind> {
        match self {
            Self::ListThemes => Some(ListKind::Themes),
            Self::ListColors => Some(ListKind::Colors),
            Self::ListFonts => Some(ListKind::Fonts),
            Self::ListKeybinds => Some(ListKind::Keymaps),
            Self::ListActions => Some(ListKind::Actions),
            Self::ListPlugins => Some(ListKind::Plugins),
            Self::ListExtensions | Self::ShowConfig | Self::ValidateConfig | Self::EditConfig => {
                None
            }
        }
    }

    /// Canonical config verb for config aliases.
    pub(crate) fn config_command(self) -> Option<ConfigCommand> {
        match self {
            Self::ShowConfig => Some(ConfigCommand::Show),
            Self::ValidateConfig => Some(ConfigCommand::Validate),
            Self::EditConfig => Some(ConfigCommand::Edit),
            _ => None,
        }
    }

    /// Completion flags offered after this alias.
    pub(crate) fn flags(self) -> &'static [&'static str] {
        match self {
            Self::ListThemes | Self::ListPlugins => &["--format", "--safe", "--no-color"],
            Self::ListColors | Self::ListKeybinds | Self::ListActions => {
                &["--format", "--config", "--profile", "--safe", "--no-color"]
            }
            Self::ListFonts => &["--format", "--no-color"],
            Self::ListExtensions => &["--format", "--no-color"],
            Self::ShowConfig | Self::ValidateConfig => {
                &["--format", "--config", "--profile", "--safe"]
            }
            Self::EditConfig => &["--config", "--profile", "--safe"],
        }
    }

    /// Per-action help text (stdout for `bitty <alias> --help`).
    pub(crate) fn help_text(self) -> String {
        let usage = match self {
            Self::ListThemes => {
                "Usage: bitty +list-themes [--format table|json|jsonl] [--safe] [--no-color]"
            }
            Self::ListColors => {
                "Usage: bitty +list-colors [--format table|json|jsonl] [--config PATH] [--profile NAME] [--safe] [--no-color]"
            }
            Self::ListFonts => "Usage: bitty +list-fonts [--format table|json|jsonl] [--no-color]",
            Self::ListKeybinds => {
                "Usage: bitty +list-keybinds [--format table|json|jsonl] [--config PATH] [--profile NAME] [--safe] [--no-color]"
            }
            Self::ListActions => {
                "Usage: bitty +list-actions [--format table|json|jsonl] [--config PATH] [--profile NAME] [--safe] [--no-color]"
            }
            Self::ListPlugins => {
                "Usage: bitty +list-plugins [--format table|json|jsonl] [--safe] [--no-color]"
            }
            Self::ListExtensions => {
                "Usage: bitty +list-extensions [--format table|json|jsonl] [--no-color]"
            }
            Self::ShowConfig => {
                "Usage: bitty +show-config [--format table|json|jsonl] [--config PATH] [--profile NAME] [--safe]"
            }
            Self::ValidateConfig => {
                "Usage: bitty +validate-config [--format table|json|jsonl] [--config PATH] [--profile NAME] [--safe]"
            }
            Self::EditConfig => {
                "Usage: bitty +edit-config [--format table|json|jsonl] [--config PATH] [--profile NAME] [--safe]"
            }
        };
        let canonical = match self {
            Self::ListThemes
            | Self::ListColors
            | Self::ListFonts
            | Self::ListKeybinds
            | Self::ListActions
            | Self::ListPlugins => "bitty list",
            Self::ListExtensions => "bitty extension list",
            Self::ShowConfig => "bitty config show",
            Self::ValidateConfig => "bitty config validate",
            Self::EditConfig => "bitty config edit",
        };
        format!(
            "bitty {name} — {summary}\n\
             \n\
             {usage}\n\
             \n\
             Alias for `{canonical}`; both spellings run the same local-class\n\
             implementation and emit the same payload. JSON uses the stable\n\
             envelope v1 (`v`, `command`, `ok`, `result`, plus `error` on\n\
             failure); diagnostics go to stderr.\n\
             \n\
             Read-only for every list/show/validate action: no plugin VM is\n\
             loaded, static catalogs never touch the network, and `--safe`\n\
             selects the built-in safe config. `+edit-config` is the only\n\
             action that writes (through the configured editor) and it\n\
             re-validates the file afterwards without rewriting it.\n\
             \n\
             Exit codes: 0 success; 1 editor/generic failure; 2 usage or\n\
             invalid configuration.\n",
            name = self.name(),
            summary = self.summary(),
            usage = usage,
            canonical = canonical,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_alias_parses_to_itself_and_round_trips() {
        for (name, action) in PlusAction::ALL {
            assert_eq!(PlusAction::parse(name), Some(*action));
            assert_eq!(action.name(), *name);
        }
        assert_eq!(PlusAction::parse("+List-themes"), None);
        assert_eq!(PlusAction::parse("+unknown"), None);
    }

    #[test]
    fn list_aliases_map_onto_canonical_kinds() {
        assert_eq!(PlusAction::ListThemes.list_kind(), Some(ListKind::Themes));
        assert_eq!(PlusAction::ListColors.list_kind(), Some(ListKind::Colors));
        assert_eq!(PlusAction::ListFonts.list_kind(), Some(ListKind::Fonts));
        assert_eq!(
            PlusAction::ListKeybinds.list_kind(),
            Some(ListKind::Keymaps)
        );
        assert_eq!(PlusAction::ListActions.list_kind(), Some(ListKind::Actions));
        assert_eq!(PlusAction::ListPlugins.list_kind(), Some(ListKind::Plugins));
        assert_eq!(PlusAction::ListExtensions.list_kind(), None);
    }

    #[test]
    fn config_aliases_map_onto_canonical_verbs() {
        assert_eq!(
            PlusAction::ShowConfig.config_command(),
            Some(ConfigCommand::Show)
        );
        assert_eq!(
            PlusAction::ValidateConfig.config_command(),
            Some(ConfigCommand::Validate)
        );
        assert_eq!(
            PlusAction::EditConfig.config_command(),
            Some(ConfigCommand::Edit)
        );
    }

    #[test]
    fn help_text_names_the_alias_and_canonical_command() {
        let help = PlusAction::ListKeybinds.help_text();
        assert!(help.contains("bitty +list-keybinds"));
        assert!(help.contains("bitty list"));
        assert!(help.contains("envelope v1"));
    }
}
