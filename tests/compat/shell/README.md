<!-- markdownlint-disable MD025 -->

# Shell Compatibility (`tests/compat/shell`)

Shell integration corpus — `OSC 133` prompt marks (`A/B/C/D`), `OSC 7` cwd, hyperlink prompt, and failure-agnostic replay.

## Source

- Shell marks: `OSC 133 ; A` (prompt start), `133 ; B` (input start), `133 ; C` (output start), `133 ; D` (output end), plus `1337` / `1339` (WezTerm / Ghostty extensions). Captured from `zsh`/`bash`/`fish` with shell integration (`script` logs of `cargo build`, `nvim` TUI, `fzf`).
- Ghostty / kitty / WezTerm differential — compare `ZoneRecord` log (`bitty-term-state::zone_len`, `zones()`) to reference terminal's shell-integration dump (Ghostty `shell-integration` zone JSON, kitty `marks` API). Zones are bounded `ZONE_RECORDS_MAX = 1024`, oldest dropped first.
- Existing baseline — `replay.rs::fixture_shell_session_replay` (full `133;A`/`133;B`/`133;C`/`133;D;0` flow with hyperlink, bracketed paste, alt-screen), `seeds/09-osc-hyperlink-prompt.bin`.

## Bounds

- `#![forbid(unsafe_code)]`, headless, `MAX_CORPUS_BYTES = 8 KiB`, `MAX_ACTIONS = 4096`, `ZONE_RECORDS_MAX = 1024`.
- No window/GPU — shell marks affect `State::zones`, not render.

## Layout

```text
shell/
  README.md
  corpus/
    01-prompt-marks.bin       # 133;A/B/C/D flow
    placeholder.bin
```
