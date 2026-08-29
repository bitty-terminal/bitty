<!-- markdownlint-disable MD025 -->

# Terminfo (`terminfo/bitty.ti`) — stub

Stub for the future `bitty` terminfo entry. `crates/bitty-pty/src/builder.rs::DEFAULT_TERM` is currently `xterm-256color`; this directory will hold the draft `terminfo.src` defining `bitty`/`bitty-256color` capabilities consistent with the VT features already in `bitty-vt`/`bitty-term-state`.

Status: stub. No `bitty.ti` file is committed yet — this draft records the intended shape so reviewers have a pinned location and contract without claiming an implemented entry.

Intended draft capabilities (pending acceptance in `compatibility-milestone-rfc`):

- `cols#80`, `lines#24` default geometry (matches `State::GRID_COLUMNS`/`GRID_ROWS`).
- `colors#256`, `pairs#32767`, `RGB` / `Tc` (true color `SGR 38;2`/`48;2` supported by `bitty-vt`).
- `smulx` / `Smulx` underline varieties (`SGR 4:x` straight/double/curly/dotted/dashed via `bitty-term-state`).
- `Hls` hyperlink `Osc 8` (`Hyperlink` OSC, see `TerminalAction::OscHyperlink`).
- `E3` scrollback clear (`ED 3` via `TerminalAction::EraseInDisplay::Scrollback`).
- `Ss` / `Se` cursor styling (`DECSCUSR`, `CursorStyle`).
- Existing `xterm-256color` baseline otherwise; `bitty` diff from `xterm-256color` stays minimal and is recorded in the diff header of `bitty.ti` when landed.

Contract: `DEFAULT_TERM` remains `xterm-256color` until a `tic`-compiled `bitty` entry is published and `docs/product/unicode-ime.md` (§ Terminfo / `TERM` contract) plus the compatibility matrix are updated to move the default at a minor-version bump. Callers may override via `PtyBuilder::env("TERM", "...")`.
