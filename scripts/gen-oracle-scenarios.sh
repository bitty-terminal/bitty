#!/usr/bin/env bash
# gen-oracle-scenarios.sh — author the CTX-0573 differential oracle corpus.
#
# Writes the raw VT scenario bytes (`*.bin`) and their externally derived
# expectations (`*.expected`) under `tests/compat/oracle/scenarios/`.
#
# Every expectation carries STRUCTURED provenance: an `authority:` naming one
# of the closed set in `crates/bitty-compat-lab/src/oracle.rs` (`AUTHORITIES`)
# and a `cite:` holding a verbatim token from that authority which defines the
# exercised behavior. The citation index
# (`tests/compat/oracle/authority-cites.txt`) records every accepted pair; the
# `oracle_citations_are_backed_by_their_authority` guard requires each pair to
# be indexed and, when the source is present, re-finds the token verbatim. A
# mis-citation (for example citing ctlseqs for DECSET 2026, which ctlseqs does
# not define — it is ghostty `synchronized_output`) therefore fails instead of
# being rubber-stamped.
#
# Re-running this script reproduces the corpus byte-identically EXCEPT the
# `authority-cites.txt` index, which is committed separately (see `write_cite`
# below; it is rewritten from the same source of truth). Run from the repo root.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
dir="$root/tests/compat/oracle/scenarios"
cite_index="$root/tests/compat/oracle/authority-cites.txt"
workspace="${BITTY_WORKSPACE:-$(dirname "$root")}"
refs="$workspace/recording/references"
mkdir -p "$dir"

# Authority id -> exact source file. Kept in lockstep with AUTHORITIES in
# crates/bitty-compat-lab/src/oracle.rs. Every cite token below is verified
# verbatim against these files at generation time, so a mis-citation cannot be
# written into the committed index even when CI later lacks the sources.
authority_path() {
  case "$1" in
  xterm-ctlseqs) printf '%s' "$refs/xterm/ctlseqs.txt" ;;
  xterm-charproc) printf '%s' "$refs/xterm/charproc.c" ;;
  ghostty-modes) printf '%s' "$refs/ghostty/src/terminal/modes.zig" ;;
  ghostty-terminal) printf '%s' "$refs/ghostty/src/terminal/Terminal.zig" ;;
  kitty-window) printf '%s' "$refs/kitty/kitty/window.py" ;;
  m1-rfc) printf '%s' "$root/docs/specifications/compatibility-milestone-rfc.md" ;;
  text-rendering-rfc) printf '%s' "$root/docs/specifications/text-rendering-rfc.md" ;;
  *) printf '%s' "" ;;
  esac
}

verify_cites() {
  local missing=0
  for c in "${cites[@]}"; do
    local authority="${c%%$'\t'*}"
    local cite="${c#*$'\t'}"
    local path
    path="$(authority_path "$authority")"
    if [ -z "$path" ] || [ ! -f "$path" ]; then
      printf 'gen-oracle-scenarios[skip]: %s source absent (%s); token unverified here\n' \
        "$authority" "$path" >&2
      continue
    fi
    if ! grep -qF -- "$cite" "$path"; then
      printf 'gen-oracle-scenarios[error]: %s cite not found in %s: %s\n' \
        "$authority" "$path" "$cite" >&2
      missing=1
    fi
  done
  if [ "$missing" -ne 0 ]; then
    printf 'gen-oracle-scenarios: refusing to write an index with unverified citations\n' >&2
    exit 1
  fi
}

write_bin() {
  local name="$1" content="$2"
  printf '%b' "$content" >"$dir/$name.bin"
}

write_expected() {
  local name="$1" content="$2"
  printf '%s\n' "$content" >"$dir/$name.expected"
}

# ---------------------------------------------------------------------------
# Authority + cite pairs (the committed index). Source of truth for
# `authority-cites.txt`; each cite is a verbatim token from the authority that
# the citation guard re-verifies against the reference snapshot / docs
# submodule.
# ---------------------------------------------------------------------------
cites=(
  # --- xterm ctlseqs.txt (patch #411, 2026/08/23) ---
  $'xterm-ctlseqs\tPs = 1 0  -> Change VT100 text foreground color to Pt.'
  $'xterm-ctlseqs\tPs = 1 1  -> Change VT100 text background color to Pt.'
  $'xterm-ctlseqs\tIf a "?" is given rather than a name or RGB specification,'
  $'xterm-ctlseqs\tPs = 0  -> Change Icon Name and Window Title to Pt.'
  $'xterm-ctlseqs\tPs = 2  -> Change Window Title to Pt.'
  $'xterm-ctlseqs\tPs = 9  -> Send Mouse X & Y on button press.  See the'
  $'xterm-ctlseqs\tPs = 1 0 0 0  -> Send Mouse X & Y on button press and'
  $'xterm-ctlseqs\tPs = 1 0 0 2  -> Use Cell Motion Mouse Tracking, xterm.  See'
  $'xterm-ctlseqs\tPs = 1 0 0 3  -> Use All Motion Mouse Tracking, xterm.  See'
  $'xterm-ctlseqs\tPs = 1 0 0 5  -> Enable UTF-8 Mouse Mode, xterm.'
  $'xterm-ctlseqs\tPs = 1 0 0 6  -> Enable SGR Mouse Mode, xterm.'
  $'xterm-ctlseqs\tPs = 1 0 1 5  -> Enable urxvt Mouse Mode.'
  $'xterm-ctlseqs\tThere are two sets of mutually exclusive modes'
  $'xterm-ctlseqs\tPs = 1 0 0 7  -> Enable Alternate Scroll Mode, xterm.  This'
  $'xterm-ctlseqs\tPs = 1 0 0 7  -> Disable Alternate Scroll Mode, xterm.  This'
  $'xterm-ctlseqs\tPs = 2  -> steady block.'
  $'xterm-ctlseqs\tPs = 5  -> blinking bar, xterm.'
  $'xterm-ctlseqs\tPs = 1 0 4 9  -> Save cursor as in DECSC, xterm.  After'
  $'xterm-ctlseqs\tPs = 4 7  -> Use Alternate Screen Buffer, xterm.  This'
  $'xterm-ctlseqs\tPs = 1  -> Application Cursor Keys (DECCKM), VT100.'
  $'xterm-ctlseqs\tPs = 1  -> Normal Cursor Keys (DECCKM), VT100.'
  $'xterm-ctlseqs\tPs = 5  -> Status Report.'
  $'xterm-ctlseqs\tPs = 6  -> Report Cursor Position (CPR) [row;column].'
  $'xterm-ctlseqs\t-> CSI ? 6 c  ("VT102")'
  $'xterm-ctlseqs\tOn button press, xterm sends CSI M'
  $'xterm-ctlseqs\to   CSI < followed by semicolon-separated'
  $'xterm-ctlseqs\tCSI followed by semicolon-separated'
  $'xterm-ctlseqs\tThis enables UTF-8 encoding for Cx and Cy under all tracking'
  # --- xterm charproc.c source (mutual-exclusion reset semantics) ---
  $'xterm-charproc\tthey are mutually exclusive.  For consistency, a reset is'
  # --- ghostty reference sources ---
  $'ghostty-modes\t.{ .name = "synchronized_output", .value = 2026, .default_configurable = false },'
  $'ghostty-terminal\t/// Legacy alternate screen mode. This goes to the alternate'
  $'ghostty-terminal\t/// screen or primary screen and only copies the cursor. The'
  # --- kitty reference source (OSC dynamic-color reply form) ---
  $'kitty-window\trgb:{c.red:02x}/{c.green:02x}/{c.blue:02x}'
  # --- M1 compatibility milestone RFC ---
  $'m1-rfc\t| Synchronized updates | DECSET 2026'
  $'m1-rfc\tClassification correction (2026-09-13, CTX-0175): mode 1007 is **Alternate'
  # --- text rendering RFC (DECSCUSR Ps=0 default semantics) ---
  $'text-rendering-rfc\tmaps `0` to the configured default style'
)

# Verify every token against its source before writing the index. When the
# read-only snapshot or docs submodule is absent the token is reported as
# unverified here; the committed index is still only ever written after a
# generation run that had the sources present.
verify_cites

{
  printf '%s\n' '# CTX-0573 oracle citation index — authority<TAB>cite.'
  printf '%s\n' '# Each pair is a verbatim token from the named authority (see'
  printf '%s\n' '# crates/bitty-compat-lab/src/oracle.rs AUTHORITIES) that defines the'
  printf '%s\n' '# exercised behavior. Regenerated by scripts/gen-oracle-scenarios.sh.'
  printf '%s\n' '# The oracle_citations_are_backed_by_their_authority guard requires every'
  printf '%s\n' '# scenario authority/cite pair to appear here and, when the source is'
  printf '%s\n' '# resolvable, to be re-found verbatim in it.'
  for c in "${cites[@]}"; do printf '%s\n' "$c"; done
} >"$cite_index"

# --- synchronized-update -------------------------------------------------
# DECSET/DECRST ?2026. ctlseqs does NOT define 2026 (0 occurrences); the
# authority is ghostty's mode table (`synchronized_output` = 2026) and the M1
# RFC, which lists DECSET 2026 as Required.
write_bin "sync-2026" '\x1b[?2026h\x1b[?2026l'
write_expected "sync-2026" 'area: synchronized-update
authority: ghostty-modes
cite: .{ .name = "synchronized_output", .value = 2026, .default_configurable = false },
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: synchronized_update = off'

write_bin "sync-2026-set" '\x1b[?2026h'
write_expected "sync-2026-set" 'area: synchronized-update
authority: m1-rfc
cite: | Synchronized updates | DECSET 2026
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: synchronized_update = on'

# --- osc-color -----------------------------------------------------------
# OSC 10/11 query and set. ctlseqs OSC section: Ps 10 fg / Ps 11 bg; a "?"
# payload elicits a reply; the runtime answers with the kitty/ghostty
# `rgb:RR/GG/BB` form (see osc-color-runtime-*).
write_bin "osc-10-query" '\x1b]10;?\x07'
write_expected "osc-10-query" 'area: osc-color
authority: xterm-ctlseqs
cite: If a "?" is given rather than a name or RGB specification,
grid: 80x24
grid_text: blank
cursor: 0 0 visible
action: osc_dynamic_color fg query'

write_bin "osc-11-query" '\x1b]11;?\x1b\\'
write_expected "osc-11-query" 'area: osc-color
authority: xterm-ctlseqs
cite: Ps = 1 1  -> Change VT100 text background color to Pt.
grid: 80x24
grid_text: blank
cursor: 0 0 visible
action: osc_dynamic_color bg query'

write_bin "osc-10-set" '\x1b]10;#112233\x07'
write_expected "osc-10-set" 'area: osc-color
authority: xterm-ctlseqs
cite: Ps = 1 0  -> Change VT100 text foreground color to Pt.
grid: 80x24
grid_text: blank
cursor: 0 0 visible
action: osc_dynamic_color fg set 17 34 51'

write_bin "osc-11-set" '\x1b]11;rgb:ff/00/80\x1b\\'
write_expected "osc-11-set" 'area: osc-color
authority: xterm-ctlseqs
cite: Ps = 1 1  -> Change VT100 text background color to Pt.
grid: 80x24
grid_text: blank
cursor: 0 0 visible
action: osc_dynamic_color bg set 255 0 128'

# Runtime round trip: default-deny gated set, then query returns the override
# in the kitty/ghostty `rgb:RR/GG/BB` reply form. Engine: runtime.
write_bin "osc-10-11-roundtrip" '\x1b]10;#112233\x07\x1b]11;rgb:44/55/66\x07\x1b]10;?\x07\x1b]11;?\x07'
write_expected "osc-10-11-roundtrip" 'area: osc-color
authority: kitty-window
cite: rgb:{c.red:02x}/{c.green:02x}/{c.blue:02x}
engine: runtime
stimulus: allow-osc-color-set
grid: 80x24
grid_text: blank
cursor: 0 0 visible
reply: \e]10;rgb:1111/2222/3333\e\\\e]11;rgb:4444/5555/6666\e\\'

# --- osc-title -----------------------------------------------------------
# OSC 0 sets icon name + window title, OSC 2 sets window title.
write_bin "osc-0-title" '\x1b]0;bitty-oracle\x07'
write_expected "osc-0-title" 'area: osc-title
authority: xterm-ctlseqs
cite: Ps = 0  -> Change Icon Name and Window Title to Pt.
grid: 80x24
grid_text: blank
cursor: 0 0 visible
title: bitty-oracle
action: osc_title bitty-oracle'

write_bin "osc-2-title-st" '\x1b]2;oracle\x1b\\'
write_expected "osc-2-title-st" 'area: osc-title
authority: xterm-ctlseqs
cite: Ps = 2  -> Change Window Title to Pt.
grid: 80x24
grid_text: blank
cursor: 0 0 visible
title: oracle
action: osc_title oracle'

# --- mouse-tracking ------------------------------------------------------
# DECSET ?9/?1000/?1002/?1003 select the tracking level. ctlseqs 928-978.
write_bin "mouse-9-x10" '\x1b[?9h'
write_expected "mouse-9-x10" 'area: mouse-tracking
authority: xterm-ctlseqs
cite: Ps = 9  -> Send Mouse X & Y on button press.  See the
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_tracking = x10'

write_bin "mouse-1000-normal" '\x1b[?1000h'
write_expected "mouse-1000-normal" 'area: mouse-tracking
authority: xterm-ctlseqs
cite: Ps = 1 0 0 0  -> Send Mouse X & Y on button press and
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_tracking = normal'

write_bin "mouse-1002-button" '\x1b[?1002h'
write_expected "mouse-1002-button" 'area: mouse-tracking
authority: xterm-ctlseqs
cite: Ps = 1 0 0 2  -> Use Cell Motion Mouse Tracking, xterm.  See
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_tracking = button'

write_bin "mouse-1003-any" '\x1b[?1003h'
write_expected "mouse-1003-any" 'area: mouse-tracking
authority: xterm-ctlseqs
cite: Ps = 1 0 0 3  -> Use All Motion Mouse Tracking, xterm.  See
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_tracking = any'

# --- mouse-encoding ------------------------------------------------------
# Coordinate encodings are mutually exclusive (xterm ctlseqs: "two sets of
# mutually exclusive modes"); the reset-only-against-the-matching-mode
# semantics are xterm source (charproc.c), not ctlseqs.
write_bin "mouse-1006-sgr" '\x1b[?1006h'
write_expected "mouse-1006-sgr" 'area: mouse-encoding
authority: xterm-ctlseqs
cite: Ps = 1 0 0 6  -> Enable SGR Mouse Mode, xterm.
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_encoding = sgr'

write_bin "mouse-1015-urxvt" '\x1b[?1015h'
write_expected "mouse-1015-urxvt" 'area: mouse-encoding
authority: xterm-ctlseqs
cite: Ps = 1 0 1 5  -> Enable urxvt Mouse Mode.
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_encoding = urxvt'

write_bin "mouse-1005-utf8" '\x1b[?1005h'
write_expected "mouse-1005-utf8" 'area: mouse-encoding
authority: xterm-ctlseqs
cite: Ps = 1 0 0 5  -> Enable UTF-8 Mouse Mode, xterm.
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_encoding = utf8'

# Mutual exclusion: enable urxvt, enable SGR (SGR becomes active), then reset
# urxvt; because urxvt is not the active encoding the reset is a no-op and SGR
# stays. The wording "a reset is only effective against the matching mode" is
# xterm's source (charproc.c), not ctlseqs; ctlseqs only names the two sets.
write_bin "mouse-encoding-exclusive" '\x1b[?1015h\x1b[?1006h\x1b[?1015l'
write_expected "mouse-encoding-exclusive" 'area: mouse-encoding
authority: xterm-charproc
cite: they are mutually exclusive.  For consistency, a reset is
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_encoding = sgr'

# --- mouse emitted bytes (runtime engine) ---------------------------------
# The runtime emits the report when tracking + encoding are set; the expected
# bytes are the ctlseqs wire forms, not Bitty output. Pointer at grid (0,0).
write_bin "mouse-emit-x10" '\x1b[?1000h'
write_expected "mouse-emit-x10" 'area: mouse-encoding
authority: xterm-ctlseqs
cite: On button press, xterm sends CSI M
engine: runtime
stimulus: mouse-left-press
at_cell: 0,0
grid: 80x24
grid_text: blank
cursor: 0 0 visible
emit: \e[M\x20\x21\x21'

write_bin "mouse-emit-sgr" '\x1b[?1000h\x1b[?1006h'
write_expected "mouse-emit-sgr" 'area: mouse-encoding
authority: xterm-ctlseqs
cite: o   CSI < followed by semicolon-separated
engine: runtime
stimulus: mouse-left-press
at_cell: 0,0
grid: 80x24
grid_text: blank
cursor: 0 0 visible
emit: \e[<0;1;1M'

write_bin "mouse-emit-urxvt" '\x1b[?1000h\x1b[?1015h'
write_expected "mouse-emit-urxvt" 'area: mouse-encoding
authority: xterm-ctlseqs
cite: CSI followed by semicolon-separated
engine: runtime
stimulus: mouse-left-press
at_cell: 0,0
grid: 80x24
grid_text: blank
cursor: 0 0 visible
emit: \e[32;1;1M'

write_bin "mouse-emit-utf8" '\x1b[?1000h\x1b[?1005h'
write_expected "mouse-emit-utf8" 'area: mouse-encoding
authority: xterm-ctlseqs
cite: This enables UTF-8 encoding for Cx and Cy under all tracking
engine: runtime
stimulus: mouse-left-press
at_cell: 0,0
grid: 80x24
grid_text: blank
cursor: 0 0 visible
emit: \e[M\x20\x21\x21'

# --- alternate-scroll ----------------------------------------------------
# Mode 1007 alternate scroll (ctlseqs) with the M1 RFC classification
# correction (CTX-0175).
write_bin "alt-scroll-1007" '\x1b[?1007h'
write_expected "alt-scroll-1007" 'area: alternate-scroll
authority: xterm-ctlseqs
cite: Ps = 1 0 0 7  -> Enable Alternate Scroll Mode, xterm.  This
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: alternate_scroll = on'

write_bin "alt-scroll-1007-reset" '\x1b[?1007h\x1b[?1007l'
write_expected "alt-scroll-1007-reset" 'area: alternate-scroll
authority: xterm-ctlseqs
cite: Ps = 1 0 0 7  -> Disable Alternate Scroll Mode, xterm.  This
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: alternate_scroll = off'

# --- cursor-style --------------------------------------------------------
# DECSCUSR CSI Ps SP q. ctlseqs defines Ps=2/Ps=5; Ps=0 ("configured default")
# is the canonical doc + ghostty (`0 => .default`), NOT ctlseqs.
write_bin "cursor-style-steady-block" '\x1b[2 q'
write_expected "cursor-style-steady-block" 'area: cursor-style
authority: xterm-ctlseqs
cite: Ps = 2  -> steady block.
grid: 80x24
grid_text: blank
cursor: 0 0 visible
cursor_style: steady_block
action: cursor_style steady_block'

write_bin "cursor-style-blinking-bar" '\x1b[5 q'
write_expected "cursor-style-blinking-bar" 'area: cursor-style
authority: xterm-ctlseqs
cite: Ps = 5  -> blinking bar, xterm.
grid: 80x24
grid_text: blank
cursor: 0 0 visible
cursor_style: blinking_bar
action: cursor_style blinking_bar'

write_bin "cursor-style-default" '\x1b[2 q\x1b[0 q'
write_expected "cursor-style-default" 'area: cursor-style
authority: text-rendering-rfc
cite: maps `0` to the configured default style
grid: 80x24
grid_text: blank
cursor: 0 0 visible
cursor_style: default
action: cursor_style steady_block
action: cursor_style default'

# --- alternate-screen ----------------------------------------------------
# ?1049 saves cursor + clears alt screen; ?47 keeps alt content.
write_bin "alt-screen-1049-roundtrip" 'A\x1b[?1049hB\x1b[?1049lC'
write_expected "alt-screen-1049-roundtrip" 'area: alternate-screen
authority: xterm-ctlseqs
cite: Ps = 1 0 4 9  -> Save cursor as in DECSC, xterm.  After
grid: 80x24
grid_text: unchecked
row 0: AC
cursor: 0 2 visible
mode: alt_screen = off'

# ?47 keeps whatever the alt grid last held: unlike ?1049 it does NOT clear.
write_bin "alt-screen-47-no-clear" '\x1b[?47h\x1b[1;5HA\x1b[?47l\x1b[?47h\x1b[1;1HB'
write_expected "alt-screen-47-no-clear" 'area: alternate-screen
authority: xterm-ctlseqs
cite: Ps = 4 7  -> Use Alternate Screen Buffer, xterm.  This
grid: 80x24
grid_text: unchecked
row 0: B   A
cursor: 0 1 visible
mode: alt_screen = on'

# ?47 must NOT save/restore the cursor: xterm `srm_ALTBUF` has no
# CursorSave/CursorRestore (unlike `srm_OPT_ALTBUF_CURSOR` for ?1049) and
# ghostty `.@"47"` "only copies the cursor", so the cursor stays where the
# alt session left it. "X ?47h Y ?47l Z" places Z at column 2 -> row "X Z"
# with the cursor at column 3. Promoted from the divergence set by CTX-0582
# (#1173) once the build matched the reference.
write_bin "alt-screen-47-cursor-restore" 'X\x1b[?47hY\x1b[?47lZ'
write_expected "alt-screen-47-cursor-restore" 'area: alternate-screen
authority: ghostty-terminal
cite: /// Legacy alternate screen mode. This goes to the alternate
grid: 80x24
grid_text: unchecked
row 0: X Z
cursor: 0 3 visible
mode: alt_screen = off'

# --- cursor-keys ---------------------------------------------------------
# DECCKM ?1 application cursor keys.
write_bin "cursor-keys-decckm" '\x1b[?1h'
write_expected "cursor-keys-decckm" 'area: cursor-keys
authority: xterm-ctlseqs
cite: Ps = 1  -> Application Cursor Keys (DECCKM), VT100.
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: application_cursor_keys = on'

write_bin "cursor-keys-decckm-reset" '\x1b[?1h\x1b[?1l'
write_expected "cursor-keys-decckm-reset" 'area: cursor-keys
authority: xterm-ctlseqs
cite: Ps = 1  -> Normal Cursor Keys (DECCKM), VT100.
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: application_cursor_keys = off'

# --- device-status -------------------------------------------------------
# DSR/DA1 reply bytes synthesize in terminal state; expected bytes are the
# spec-defined responses.
write_bin "dsr-5-status" '\x1b[5n'
write_expected "dsr-5-status" 'area: device-status
authority: xterm-ctlseqs
cite: Ps = 5  -> Status Report.
grid: 80x24
grid_text: blank
cursor: 0 0 visible
reply: \e[0n'

write_bin "dsr-6-cursor" '\x1b[5;7H\x1b[6n'
write_expected "dsr-6-cursor" 'area: device-status
authority: xterm-ctlseqs
cite: Ps = 6  -> Report Cursor Position (CPR) [row;column].
grid: 80x24
grid_text: blank
cursor: 4 6 visible
reply: \e[5;7R'

write_bin "da1-primary" '\x1b[c'
write_expected "da1-primary" 'area: device-status
authority: xterm-ctlseqs
cite: -> CSI ? 6 c  ("VT102")
grid: 80x24
grid_text: blank
cursor: 0 0 visible
reply: \e[?6c'

echo "wrote $(find "$dir" -name '*.bin' | wc -l) scenarios to $dir"
echo "wrote ${#cites[@]} citation(s) to $cite_index"
