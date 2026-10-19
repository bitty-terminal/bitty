//! `bitty +show-font`: read-only resolved-font probe (CTX-1370, issue #2263).
//!
//! Terminal-font detectors need the font a terminal actually resolved, but
//! process-name heuristics cannot see bitty's Lua configuration. This
//! command loads the same merged config startup uses (`CLI > file > profile >
//! defaults`, plus `--safe`), prints stable plain text, and exits without
//! creating a runtime, a window, a PTY, or a plugin VM.
//!
//! Output (one fact per line, stable prefixes):
//!
//! ```text
//! font_family: JetBrainsMono Nerd Font
//! font_size: 12
//! font_fallback: Noto Sans Mono
//! ```
//!
//! fastfetch parses `font_family: ` / `font_size: ` (the same prefixes
//! kitty's `+kitten query-terminal` uses); extra lines are ignored.
//!
//! Exit codes are typed:
//!
//! - `0` — resolved font printed.
//! - `2` — the merged config could not be loaded (invalid or missing
//!   explicit file, invalid profile), or usage was rejected.
//!
//! [`ShowFontError::Unavailable`] / [`EXIT_UNAVAILABLE`] remain as
//! defense-in-depth for direct callers of [`render_font`], but config
//! validation rejects an empty/overlong family and a non-finite, zero, or
//! `> 128` size at load time, so the CLI path normally reports those as exit
//! `2` (CTX-1370 review F5). The usage text lists only the reachable codes.
//!
//! # Live config vs. file config
//!
//! This probe starts a fresh process and reads the merged config files. The
//! DCS font-query path in a running terminal answers from the live
//! `RuntimeConfig`, which a live zoom or appearance change can update without
//! writing the file. The two paths can therefore report different sizes until
//! the config is saved/restarted; the DCS answer is the value the running
//! terminal is actually using (review F4).
//!
//! The command grants no capability and never reads or writes anything
//! outside the existing config-loading path.

#![forbid(unsafe_code)]

use crate::cli::Args;
use crate::config_cli::load_merged_config;

/// Exit code for config/usage failures (matches startup's usage class).
pub(crate) const EXIT_CONFIG: i32 = 2;

/// Exit code for a loaded config with no usable font identity.
pub(crate) const EXIT_UNAVAILABLE: i32 = 3;

/// Typed failure for the resolved-font probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShowFontError {
    /// The merged config could not be loaded (message already printed).
    Config,
    /// The effective font identity is missing or invalid.
    Unavailable,
}

impl ShowFontError {
    /// Process exit code for this failure class.
    #[must_use]
    pub(crate) const fn exit_code(self) -> i32 {
        match self {
            Self::Config => EXIT_CONFIG,
            Self::Unavailable => EXIT_UNAVAILABLE,
        }
    }
}

/// Usage text for `bitty +show-font`.
#[must_use]
pub(crate) fn show_font_usage() -> &'static str {
    "Usage: bitty +show-font [--config PATH] [--profile NAME] [--safe]\n\
     \n\
     Print the resolved terminal font as stable plain text:\n  \
       font_family: <family>\n  \
       font_size: <points>\n  \
       font_fallback: <family>   (zero or more lines)\n\
     \n\
     Read-only: loads the same merged config startup uses, then exits.\n  \
       Exit 0  font printed\n  \
       Exit 2  config could not be loaded / usage error\n"
}

/// Renders the stable text block for a resolved font config.
///
/// Returns [`ShowFontError::Unavailable`] when the family is blank/overlong
/// or the size is not finite and positive. Fallback entries are printed only
/// when they are non-empty, so a hostile file cannot inject extra blank
/// `font_fallback:` lines.
pub(crate) fn render_font(font: &bitty_config::types::FontConfig) -> Result<String, ShowFontError> {
    let family = font.family.trim();
    if family.is_empty()
        || family.len() > bitty_config::types::MAX_FONT_FAMILY_LEN
        || !(font.size.is_finite() && font.size > 0.0 && font.size <= 128.0)
    {
        return Err(ShowFontError::Unavailable);
    }
    let mut out = format!("font_family: {family}\nfont_size: {}\n", font.size);
    for fallback in &font.fallback {
        let name = fallback.trim();
        if !name.is_empty() && name.len() <= bitty_config::types::MAX_FONT_FAMILY_LEN {
            out.push_str("font_fallback: ");
            out.push_str(name);
            out.push('\n');
        }
    }
    Ok(out)
}

/// Dispatches `bitty +show-font`; returns the process exit code.
///
/// Extra bare positionals are usage errors: the probe never spawns a shell,
/// so silently treating them as a program would be misleading.
pub(crate) fn run_cli(args: &Args) -> i32 {
    if args.program.is_some() || !args.program_args.is_empty() {
        eprintln!(
            "bitty +show-font: unexpected argument '{}'\n{}",
            args.program
                .as_deref()
                .or_else(|| args.program_args.first().map(String::as_str))
                .unwrap_or_default(),
            show_font_usage()
        );
        return EXIT_CONFIG;
    }
    let loaded = match load_merged_config(args) {
        Ok(loaded) => loaded,
        Err(message) => {
            eprintln!("bitty +show-font: {message}");
            return ShowFontError::Config.exit_code();
        }
    };
    match render_font(&loaded.merged.effective.font) {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(err) => {
            eprintln!("bitty +show-font: resolved font is unavailable");
            err.exit_code()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_config::types::{FontConfig, MAX_FONT_FAMILY_LEN};

    #[test]
    fn default_font_renders_stable_lines() {
        let text = render_font(&FontConfig::default()).expect("default font");
        assert_eq!(
            text,
            "font_family: JetBrainsMono Nerd Font\nfont_size: 12\n"
        );
    }

    #[test]
    fn fractional_size_has_no_trailing_zero_padding() {
        let font = FontConfig {
            family: "Maple Mono".into(),
            size: 13.5,
            ..FontConfig::default()
        };
        assert_eq!(
            render_font(&font).expect("font"),
            "font_family: Maple Mono\nfont_size: 13.5\n"
        );
    }

    #[test]
    fn fallbacks_are_listed_after_family_and_size() {
        let font = FontConfig {
            family: "JetBrainsMono Nerd Font".into(),
            fallback: vec!["Noto Sans Mono".into(), "  ".into()],
            ..FontConfig::default()
        };
        assert_eq!(
            render_font(&font).expect("font"),
            "font_family: JetBrainsMono Nerd Font\nfont_size: 12\nfont_fallback: Noto Sans Mono\n"
        );
    }

    #[test]
    fn unusable_identity_is_typed_unavailable() {
        for font in [
            FontConfig {
                family: String::new(),
                ..FontConfig::default()
            },
            FontConfig {
                size: f32::NAN,
                ..FontConfig::default()
            },
            FontConfig {
                size: 0.0,
                ..FontConfig::default()
            },
            FontConfig {
                family: "x".repeat(MAX_FONT_FAMILY_LEN + 1),
                ..FontConfig::default()
            },
        ] {
            assert_eq!(render_font(&font), Err(ShowFontError::Unavailable));
        }
        assert_eq!(ShowFontError::Unavailable.exit_code(), EXIT_UNAVAILABLE);
        assert_eq!(ShowFontError::Config.exit_code(), EXIT_CONFIG);
    }
}
