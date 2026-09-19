#!/usr/bin/env bash
# gen-oracle-scenarios.sh — author the CTX-0573 differential oracle corpus.
#
# Writes the raw VT scenario bytes (`*.bin`) and their externally derived
# expectations (`*.expected`) under `tests/compat/oracle/scenarios/`.
#
# The expectations are derived from the authoritative control-sequence
# specification pinned in the read-only reference snapshot
# (`recording/references/xterm/ctlseqs.txt`, xterm patch #411, 2026/08/23)
# and from the M1 protocol matrix
# (`docs/specifications/compatibility-milestone-rfc.md`); none is derived
# from Bitty's own output. Re-running this script reproduces the corpus
# byte-identically. Run from the repository root.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
dir="$root/tests/compat/oracle/scenarios"
mkdir -p "$dir"

write_bin() {
  local name="$1" content="$2"
  printf '%b' "$content" >"$dir/$name.bin"
}

write_expected() {
  local name="$1" content="$2"
  printf '%s\n' "$content" >"$dir/$name.expected"
}

# --- synchronized-update -------------------------------------------------
# DECSET/DECRST ?2026. xterm ctlseqs.txt (patch #411) documents 2026 as the
# synchronized update mode (ghostty src/terminal/modes.zig names it
# `synchronized_output` value 2026); M1 RFC lists it as Required.
write_bin "sync-2026" '\x1b[?2026h\x1b[?2026l'
write_expected "sync-2026" 'area: synchronized-update
provenance: spec|M1 RFC "Synchronized updates DECSET 2026"; xterm patch #411 ctlseqs.txt; ghostty synchronized_output=2026
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: synchronized_update = off'

write_bin "sync-2026-set" '\x1b[?2026h'
write_expected "sync-2026-set" 'area: synchronized-update
provenance: spec|M1 RFC "Synchronized updates DECSET 2026"; xterm patch #411 ctlseqs.txt
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: synchronized_update = on'

# --- osc-color -----------------------------------------------------------
# OSC 10/11 query and set. ctlseqs.txt lines 2034-2036: OSC 10 sets the
# VT100 text foreground, OSC 11 the background; a `?` payload queries.
write_bin "osc-10-query" '\x1b]10;?\x07'
write_expected "osc-10-query" 'area: osc-color
provenance: spec|xterm patch #411 ctlseqs.txt "OSC Ps ; Pt ST": Ps=10 foreground, "?" queries
grid: 80x24
grid_text: blank
cursor: 0 0 visible
action: osc_dynamic_color fg query'

write_bin "osc-11-query" '\x1b]11;?\x1b\\'
write_expected "osc-11-query" 'area: osc-color
provenance: spec|xterm patch #411 ctlseqs.txt "OSC Ps ; Pt ST": Ps=11 background, "?" queries (ST terminator)
grid: 80x24
grid_text: blank
cursor: 0 0 visible
action: osc_dynamic_color bg query'

write_bin "osc-10-set" '\x1b]10;#112233\x07'
write_expected "osc-10-set" 'area: osc-color
provenance: spec|xterm patch #411 ctlseqs.txt "OSC Ps ; Pt ST": Ps=10 RGB set
grid: 80x24
grid_text: blank
cursor: 0 0 visible
action: osc_dynamic_color fg set 17 34 51'

write_bin "osc-11-set" '\x1b]11;rgb:ff/00/80\x1b\\'
write_expected "osc-11-set" 'area: osc-color
provenance: spec|xterm patch #411 ctlseqs.txt "OSC Ps ; Pt ST": Ps=11 rgb:R/G/B set
grid: 80x24
grid_text: blank
cursor: 0 0 visible
action: osc_dynamic_color bg set 255 0 128'

# --- osc-title -----------------------------------------------------------
# OSC 0 sets icon name + window title, OSC 2 sets window title.
# ctlseqs.txt lines 2034-2036 and 2271.
write_bin "osc-0-title" '\x1b]0;bitty-oracle\x07'
write_expected "osc-0-title" 'area: osc-title
provenance: spec|xterm patch #411 ctlseqs.txt "Ps=0 Change Icon Name and Window Title to Pt"; OSC 0 BEL-terminated
grid: 80x24
grid_text: blank
cursor: 0 0 visible
title: bitty-oracle
action: osc_title bitty-oracle'

write_bin "osc-2-title-st" '\x1b]2;oracle\x1b\\'
write_expected "osc-2-title-st" 'area: osc-title
provenance: spec|xterm patch #411 ctlseqs.txt "Ps=2 Change Window Title to Pt"; OSC 2 ST-terminated
grid: 80x24
grid_text: blank
cursor: 0 0 visible
title: oracle
action: osc_title oracle'

# --- mouse-tracking ------------------------------------------------------
# DECSET ?9/?1000/?1002/?1003 select the tracking level. ctlseqs.txt 928-929,
# 971-978; X10 sends CSI M CbCxCy on press only.
write_bin "mouse-9-x10" '\x1b[?9h'
write_expected "mouse-9-x10" 'area: mouse-tracking
provenance: spec|xterm patch #411 ctlseqs.txt "Ps = 9 -> Send Mouse X & Y on button press (X10)"
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_tracking = x10'

write_bin "mouse-1000-normal" '\x1b[?1000h'
write_expected "mouse-1000-normal" 'area: mouse-tracking
provenance: spec|xterm patch #411 ctlseqs.txt "Ps = 1000 -> Send Mouse X & Y on button press and release"
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_tracking = normal'

write_bin "mouse-1002-button" '\x1b[?1002h'
write_expected "mouse-1002-button" 'area: mouse-tracking
provenance: spec|xterm patch #411 ctlseqs.txt "Ps = 1002 -> Use Cell Motion Mouse Tracking" (Button-event)
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_tracking = button'

write_bin "mouse-1003-any" '\x1b[?1003h'
write_expected "mouse-1003-any" 'area: mouse-tracking
provenance: spec|xterm patch #411 ctlseqs.txt "Ps = 1003 -> Use All Motion Mouse Tracking" (Any-event)
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_tracking = any'

# --- mouse-encoding ------------------------------------------------------
# coordinate encodings are mutually exclusive; a DECRST only clears the
# matching active encoding. ctlseqs.txt 980-989.
write_bin "mouse-1006-sgr" '\x1b[?1006h'
write_expected "mouse-1006-sgr" 'area: mouse-encoding
provenance: spec|xterm patch #411 ctlseqs.txt "Ps = 1006 -> Enable SGR Mouse Mode" (mutually exclusive)
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_encoding = sgr'

write_bin "mouse-1015-urxvt" '\x1b[?1015h'
write_expected "mouse-1015-urxvt" 'area: mouse-encoding
provenance: spec|xterm patch #411 ctlseqs.txt "Ps = 1015 -> Enable urxvt Mouse Mode"
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_encoding = urxvt'

write_bin "mouse-1005-utf8" '\x1b[?1005h'
write_expected "mouse-1005-utf8" 'area: mouse-encoding
provenance: spec|xterm patch #411 ctlseqs.txt "Ps = 1005 -> Enable UTF-8 Mouse Mode"
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_encoding = utf8'

# Mutual exclusion: enable urxvt, enable SGR (SGR becomes active), then reset
# urxvt; because urxvt is not the active encoding the reset is a no-op and SGR
# stays. ctlseqs.txt: encodings are mutually exclusive and "a reset is only
# effective against the matching mode".
write_bin "mouse-encoding-exclusive" '\x1b[?1015h\x1b[?1006h\x1b[?1015l'
write_expected "mouse-encoding-exclusive" 'area: mouse-encoding
provenance: spec|xterm patch #411 ctlseqs.txt: encodings mutually exclusive, a reset is only effective against the matching mode
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: mouse_encoding = sgr'

# --- alternate-scroll ----------------------------------------------------
# Mode 1007 alternate scroll; M1 RFC classification correction (CTX-0175)
# and ctlseqs.txt 982/3017.
write_bin "alt-scroll-1007" '\x1b[?1007h'
write_expected "alt-scroll-1007" 'area: alternate-scroll
provenance: spec|xterm patch #411 ctlseqs.txt "Ps = 1007 -> Enable Alternate Scroll Mode"; M1 RFC 1007 = Alternate Scroll
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: alternate_scroll = on'

write_bin "alt-scroll-1007-reset" '\x1b[?1007h\x1b[?1007l'
write_expected "alt-scroll-1007-reset" 'area: alternate-scroll
provenance: spec|xterm patch #411 ctlseqs.txt "Ps = 1007 -> Disable Alternate Scroll Mode"
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: alternate_scroll = off'

# --- cursor-style --------------------------------------------------------
# DECSCUSR CSI Ps SP q. ctlseqs.txt 1545-1556.
write_bin "cursor-style-steady-block" '\x1b[2 q'
write_expected "cursor-style-steady-block" 'area: cursor-style
provenance: spec|xterm patch #411 ctlseqs.txt "CSI Ps SP q Set cursor style (DECSCUSR)": Ps=2 steady block
grid: 80x24
grid_text: blank
cursor: 0 0 visible
cursor_style: steady_block
action: cursor_style steady_block'

write_bin "cursor-style-blinking-bar" '\x1b[5 q'
write_expected "cursor-style-blinking-bar" 'area: cursor-style
provenance: spec|xterm patch #411 ctlseqs.txt "CSI Ps SP q Set cursor style (DECSCUSR)": Ps=5 blinking bar
grid: 80x24
grid_text: blank
cursor: 0 0 visible
cursor_style: blinking_bar
action: cursor_style blinking_bar'

write_bin "cursor-style-default" '\x1b[2 q\x1b[0 q'
write_expected "cursor-style-default" 'area: cursor-style
provenance: spec|xterm patch #411 ctlseqs.txt "CSI Ps SP q Set cursor style (DECSCUSR)": Ps=0 default
grid: 80x24
grid_text: blank
cursor: 0 0 visible
cursor_style: default
action: cursor_style steady_block
action: cursor_style default'

# --- alternate-screen ----------------------------------------------------
# ?1049 saves cursor + clears alt screen; ?47 keeps alt content. ctlseqs.txt
# 958, 1022-1031, 1105, 1163-1172.
write_bin "alt-screen-1049-roundtrip" 'A\x1b[?1049hB\x1b[?1049lC'
write_expected "alt-screen-1049-roundtrip" 'area: alternate-screen
provenance: spec|xterm patch #411 ctlseqs.txt "Ps=1049 Save cursor ... switch to Alternate Screen Buffer, clearing it first"; 1049l "Use Normal Screen Buffer and restore cursor"
grid: 80x24
grid_text: unchecked
row 0: AC
cursor: 0 2 visible
mode: alt_screen = off'

# ?47 keeps whatever the alt grid last held: unlike ?1049 it does NOT clear
# the alternate screen on entry. Explicit CUP placement makes the assertion
# independent of cursor save/restore semantics (xterm and ghostty treat the
# cursor as global across a ?47 switch; the grid content is unambiguous).
write_bin "alt-screen-47-no-clear" '\x1b[?47h\x1b[1;5HA\x1b[?47l\x1b[?47h\x1b[1;1HB'
write_expected "alt-screen-47-no-clear" 'area: alternate-screen
provenance: spec|xterm patch #411 ctlseqs.txt "Ps=47 Use Alternate Screen Buffer" (no clear on entry, unlike 1049); ghostty SwitchScreenMode .@"47" "The screen is not erased"
grid: 80x24
grid_text: unchecked
row 0: B   A
cursor: 0 1 visible
mode: alt_screen = on'

# --- cursor-keys ---------------------------------------------------------
# DECCKM ?1 application cursor keys. ctlseqs.txt 919/1070.
write_bin "cursor-keys-decckm" '\x1b[?1h'
write_expected "cursor-keys-decckm" 'area: cursor-keys
provenance: spec|xterm patch #411 ctlseqs.txt "Ps = 1 -> Application Cursor Keys (DECCKM), VT100"
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: application_cursor_keys = on'

write_bin "cursor-keys-decckm-reset" '\x1b[?1h\x1b[?1l'
write_expected "cursor-keys-decckm-reset" 'area: cursor-keys
provenance: spec|xterm patch #411 ctlseqs.txt "Ps = 1 -> Normal Cursor Keys (DECCKM)"
grid: 80x24
grid_text: blank
cursor: 0 0 visible
mode: application_cursor_keys = off'

# --- device-status -------------------------------------------------------
# DSR/DA1 reply bytes synthesize in terminal state; the expected bytes are the
# spec-defined responses, not Bitty output. ctlseqs.txt 769-775 (Primary DA
# CSI ? 6 c "VT102"), 1383-1392 (DSR 5 -> CSI 0 n, DSR 6 -> CSI r;c R).
write_bin "dsr-5-status" '\x1b[5n'
write_expected "dsr-5-status" 'area: device-status
provenance: spec|xterm patch #411 ctlseqs.txt "CSI Ps n Device Status Report": Ps=5 -> CSI 0 n
grid: 80x24
grid_text: blank
cursor: 0 0 visible
reply: \e[0n'

write_bin "dsr-6-cursor" '\x1b[5;7H\x1b[6n'
write_expected "dsr-6-cursor" 'area: device-status
provenance: spec|xterm patch #411 ctlseqs.txt "CSI Ps n Device Status Report": Ps=6 -> CSI r ; c R (1-based)
grid: 80x24
grid_text: blank
cursor: 4 6 visible
reply: \e[5;7R'

write_bin "da1-primary" '\x1b[c'
write_expected "da1-primary" 'area: device-status
provenance: spec|xterm patch #411 ctlseqs.txt "CSI Ps c Send Device Attributes (Primary DA)": VT102 -> CSI ? 6 c
grid: 80x24
grid_text: blank
cursor: 0 0 visible
reply: \e[?6c'

echo "wrote $(find "$dir" -name '*.bin' | wc -l) scenarios to $dir"
