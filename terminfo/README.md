# Terminfo (`TERM` contract) — no entry ships yet

Bitty does not claim `TERM=bitty`. `crates/bitty-pty/src/builder.rs::DEFAULT_TERM`
is `xterm-256color`, and no package format installs a terminfo entry. The
placeholder `terminfo/bitty.terminfo` that both AUR recipes used to install to
`/usr/share/terminfo/b/bitty` was removed under CTX-0451 (034 item 6): a
published package must not carry fake capabilities.

`scripts/check-terminfo.sh` guards the decision. It fails when the retired
placeholder path, a placeholder marker, or an install into
`/usr/share/terminfo` reappears in the packaging inputs.

## Landing a real `bitty` entry

The intended shape of a future entry (pending acceptance in
`compatibility-milestone-rfc`) is a minimal diff from `xterm-256color`:

- `cols#80`, `lines#24` default geometry (matches `State::GRID_COLUMNS`/`GRID_ROWS`).
- `colors#256`, `pairs#32767`, `RGB` / `Tc` (true color `SGR 38;2`/`48;2` supported by `bitty-vt`).
- `smulx` / `Smulx` underline varieties (`SGR 4:x` straight/double/curly/dotted/dashed via `bitty-term-state`).
- `Hls` hyperlink `Osc 8` (`Hyperlink` OSC, see `TerminalAction::OscHyperlink`).
- `E3` scrollback clear (`ED 3` via `TerminalAction::EraseInDisplay::Scrollback`).
- `Ss` / `Se` cursor styling (`DECSCUSR`, `CursorStyle`).

Those capabilities are design draft, not implementation evidence; the entry does
not exist and nothing installs it.

A real entry must land as one reviewed change:

1. Write `terminfo/bitty.ti` (source), compile it with `tic -x`, and verify the
   compiled entry round-trips with `infocmp -x bitty` against the advertised
   capabilities before packaging anything.
2. Install the compiled database entry (never the raw source) and update
   `scripts/check-terminfo.sh`, the packaging recipes, and this file together.
3. Update the `TERM` contract in
   `docs/specifications/text-compatibility.md` (§ Terminfo) and the release
   compatibility matrix (`docs/product/compat-matrix.md`), then move
   `DEFAULT_TERM` to `bitty` at a minor-version bump. Callers may override via
   `PtyBuilder::env("TERM", "...")`.
