# Terminfo (`TERM` contract) — `bitty` entry ships (CTX-1121, #1764)

Bitty ships a canonical `terminfo/bitty.ti` (source) compiled with `tic -x`.
`crates/bitty-pty/src/builder.rs::DEFAULT_TERM` stays `xterm-256color` for
fallback safety; export `TERM=bitty` only where the entry is installed
(local with entry installed, else keep `xterm-256color`). `bitty doctor`
probes the entry via `infocmp` and hints `tic -x terminfo/bitty.ti` when
missing. The retired placeholder `terminfo/bitty.terminfo` (CTX-0451) must
not return; `scripts/check-terminfo.sh` fails on its path, its marker, on
raw-source installs to `/usr/share/terminfo`, and on aspirational caps.

## What `bitty.ti` claims (audited only)

Base `use=xterm-256color`; only differing audited extensions are listed.
Inherited from the base (no override): `cols#80`, `lines#24`
(`State::GRID_COLUMNS`/`GRID_ROWS`), `colors#256`, `E3` (`ED 3`
`EraseInDisplay::Scrollback`), `Ss`/`Se` (`DECSCUSR` `CursorStyle`),
`XM` (SGR mouse `1006`; `1000`/`1002`/`1003` + `1005`/`1015`/`1016` also
handled), `BE`/`BD`/`PS`/`PE` (`2004` bracketed paste), `fe`/`fd` (`1004`
focus reporting).

Audited extensions in `bitty.ti`:

- `Tc`: truecolor `SGR 38;2`/`48;2` (plus `58` underline color, colon and
  semicolon forms) via `bitty-vt` parser + `bitty-render` resolve.
- `Su` + `Setulc` + `Smulx`: styled underlines `SGR 4:0-5`
  (None/Single/Double/Curly/Dotted/Dashed) + underline color `58`/`59`.
- `Sync`: synchronized updates `DEC 2026` (`\E[?2026h`/`l`); DCS legacy
  deliberately absent (modern `DEC 2026` standard, cf. Alacritty/foot).
- `Hls`: `OSC 8` hyperlinks (tmux canonical
  `\E]8;%?%p1%l%tid=%p1%s%;;%p2%s\E\\`, for tmux passthrough + SSH).
- `pairs#32767`: ncurses-limit correction of the base `pairs#0x10000`.
- `Ms@`: cancels the `Ms` (`OSC 52` clipboard) inherited from
  `xterm-256color`; bitty clipboard policy is default-deny (PARTIAL), so the
  compiled entry must not advertise `Ms`.

Deliberately excluded: `Ms` (`OSC 52` clipboard, parser-supported but
runtime-gated default-deny + state-inert — PARTIAL, cancelled with `Ms@`),
`fullkbd` (legacy Kitty `DEC 2017` full mode, absent; bitty implements the
progressive `CSI u` protocol which has no terminfo string cap), `RGB`
(covered by `Tc`; `kitty-direct` geometry not claimed).

## Landing checklist (one reviewed change)

1. Edit `terminfo/bitty.ti` only with implementation-proven caps; compile
   with `tic -x -o <dir> terminfo/bitty.ti` and verify round-trip
   `TERMINFO=<dir> infocmp -x bitty` shows the advertised caps.
2. Install the compiled database entry (never the raw source) — see
   `packaging/README.md` § Terminfo for `tic -x` per-format steps
   (`.deb`/`.rpm`/`.pkg.tar.zst`/AUR, Homebrew/macOS, Windows ConPTY note).
3. Update `scripts/check-terminfo.sh` + its fixture test together when caps
   change; `TERM` stays `xterm-256color` by default (move to `bitty` only at
   a minor-version bump with compat-matrix + docs updates). Callers may
   override via `PtyBuilder::env("TERM", "...")`.
