#!/usr/bin/env python3
# vt-ref-diff.py — bitty#2873 differential layout audit.
#
# Runs a full-screen program inside a tmux pane of a fixed size, records the
# exact bytes it writes (`pipe-pane -O`), replays those bytes through
# `bitty_vt::Parser` + `bitty_term_state::State`, and compares the resulting
# grid with tmux's own capture of the same run. tmux is the reference
# terminal here: both sides interpret one identical byte stream at one
# identical geometry, so any row that differs is a state-machine divergence,
# not a rendering or timing artifact.
#
# Usage:
#   scripts/vt-ref-diff.py [options] -- <command...>
#   scripts/vt-ref-diff.py --corpus          # the programs named in #2873
#   scripts/vt-ref-diff.py --capture <dir> -- <command...>
#
# Options:
#   --cols N / --rows N   pane geometry (default 100x30)
#   --keys K1,K2          send these keys before capturing (tmux key names,
#                         e.g. `C-d`, `q`, `Escape`)
#   --settle SECONDS      idle time before capture (default 1.8)
#   --capture DIR         also write <slug>.raw and <slug>.grid into DIR, the
#                         fixture pair read by
#                         crates/bitty-term-state/tests/tui_reference_grid_2873.rs
#
# Exit status: 0 when every program matches, 1 otherwise (so it can gate a
# manual audit run). Requires tmux 3.x, python3 and a cargo build of the
# `bitty-term-state` replay example. Not part of `just check`: it needs a real
# pty and real programs.
#
# Reference notes found while building this (bitty#2873):
#   * tmux keeps a *virtual* column past the right margin while a wrap is
#     latched, xterm keeps the cursor on the last column. EL/ECH/ICH/DCH at a
#     latched margin therefore differ by one erased cell between the two
#     references without changing where later text lands.
#   * tmux advances a latched LF by two rows; ECMA-48 and xterm keep the
#     column and advance one row.
#   * tmux does not restore the deferred wrap through DECRC; bitty does. Both
#     are defensible, and no program in the corpus depends on it.

import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CRATE = "bitty-term-state"

USAGE = """\
usage: scripts/vt-ref-diff.py [options] -- <command...>
       scripts/vt-ref-diff.py --corpus
       scripts/vt-ref-diff.py --capture <dir> -- <command...>

  --cols N / --rows N   pane geometry (default 100x30)
  --keys K1,K2          tmux keys to send before the capture
  --settle SECONDS      idle time before the capture (default 1.8)
  --capture DIR         also write <slug>-<cols>x<rows>.{raw,grid} fixtures
  --corpus              run the programs named in bitty#2873

Exit status is 0 when every grid matches the reference terminal."""


def replay(raw_path: str, cols: int, rows: int) -> list[str]:
    env = dict(os.environ, BITTY_REPLAY_BARE="1")
    out = subprocess.run(
        ["cargo", "run", "-q", "--offline", "-p", CRATE, "--example", "replay_dump",
         "--", raw_path, str(cols), str(rows)],
        cwd=REPO, capture_output=True, text=True, check=True, env=env,
    ).stdout
    rows_out = [line.rstrip() for line in out.splitlines() if not line.startswith("#")]
    while rows_out and rows_out[-1] == "":
        rows_out.pop()
    return rows_out


def capture(cmd: str, cols: int, rows: int, keys: list[str], settle: float):
    """Run cmd in a tmux pane; return (raw bytes, reference grid rows)."""
    sock = f"vtreff{os.getpid()}"
    with tempfile.NamedTemporaryFile(suffix=".raw", delete=False) as handle:
        raw_path = handle.name
    try:
        try:
            subprocess.run(
                ["tmux", "-L", sock, "-f", "/dev/null", "new-session", "-d",
                 "-x", str(cols), "-y", str(rows), cmd],
                check=True, capture_output=True,
            )
            subprocess.run(["tmux", "-L", sock, "set-option", "-g", "status", "off"],
                           capture_output=True, check=False)
            subprocess.run(["tmux", "-L", sock, "pipe-pane", "-O", "-t", "0",
                            f"cat > {shlex.quote(raw_path)}"], capture_output=True, check=False)
            time.sleep(settle)
            for key in keys:
                subprocess.run(["tmux", "-L", sock, "send-keys", "-t", "0", key],
                               capture_output=True, check=False)
                time.sleep(0.4)
            if keys:
                time.sleep(0.6)
            pane = subprocess.run(["tmux", "-L", sock, "capture-pane", "-p", "-t", "0"],
                                  capture_output=True, text=True, check=True).stdout
        finally:
            subprocess.run(["tmux", "-L", sock, "kill-server"], capture_output=True, check=False)
        with open(raw_path, "rb") as handle:
            raw = handle.read()
    finally:
        try:
            os.unlink(raw_path)
        except OSError:
            pass
    grid = [line.rstrip() for line in pane.splitlines()]
    while grid and grid[-1] == "":
        grid.pop()
    return raw, grid


def slugify(cmd: str) -> str:
    tokens = cmd.split()
    first = tokens[0] if tokens else ""
    keep = [ch if ch.isalnum() else "-" for ch in first]
    return "".join(keep).strip("-") or "program"


def run_one(label: str, cmd: str, opts) -> bool:
    raw, reference = capture(cmd, opts.cols, opts.rows, opts.keys, opts.settle)
    with tempfile.NamedTemporaryFile(suffix=".raw", delete=False) as handle:
        handle.write(raw)
        raw_path = handle.name
    try:
        got = replay(raw_path, opts.cols, opts.rows)
    finally:
        os.unlink(raw_path)

    if opts.capture:
        os.makedirs(opts.capture, exist_ok=True)
        slug = slugify(cmd)
        with open(os.path.join(opts.capture, f"{slug}-{opts.cols}x{opts.rows}.raw"), "wb") as h:
            h.write(raw)
        with open(os.path.join(opts.capture, f"{slug}-{opts.cols}x{opts.rows}.grid"), "w") as h:
            h.write("\n".join(reference) + ("\n" if reference else ""))

    ok = got == reference
    print(f"{'MATCH' if ok else 'DIFF ':5}  {label:28} {len(raw):7} bytes  "
          f"{len(reference):3} rows  {cmd}")
    if ok:
        return True
    for row in range(max(len(got), len(reference))):
        bitty_row = got[row] if row < len(got) else "<missing>"
        ref_row = reference[row] if row < len(reference) else "<missing>"
        if bitty_row != ref_row:
            print(f"        row {row:02} bitty={bitty_row!r}")
            print(f"                tmux ={ref_row!r}")
    return False


CORPUS = [
    ("vim", "vim -u NONE -N probe.txt"),
    ("nvim", "nvim -u NONE probe.txt"),
    ("less", "less probe.txt"),
    ("htop", "htop"),
    ("fzf", "sh -c 'ls /usr/bin | fzf'"),
]


def main(argv: list[str]) -> int:
    class Opts:
        def __init__(self) -> None:
            self.cols = 100
            self.rows = 30
            self.keys: list[str] = []
            self.settle = 1.8
            self.capture: str | None = None

    opts = Opts()
    rest = list(argv)
    commands: list[str] = []
    corpus = False
    while rest:
        arg = rest.pop(0)
        if arg == "--":
            cmd_str = " ".join(rest).strip()
            if cmd_str:
                commands.append(cmd_str)
            rest = []
        elif arg == "--corpus":
            corpus = True
        elif arg in ("--cols", "--rows", "--keys", "--settle", "--capture"):
            if not rest:
                print(f"missing value for {arg}", file=sys.stderr)
                return 2
            val = rest.pop(0)
            if arg == "--cols":
                opts.cols = int(val)
            elif arg == "--rows":
                opts.rows = int(val)
            elif arg == "--keys":
                opts.keys = [k for k in val.split(",") if k]
            elif arg == "--settle":
                opts.settle = float(val)
            elif arg == "--capture":
                opts.capture = val
        elif arg in ("-h", "--help"):
            print(USAGE)
            return 0
        else:
            print(f"unknown argument: {arg}", file=sys.stderr)
            return 2

    if not commands and not corpus:
        print("give a command after `--` or pass --corpus", file=sys.stderr)
        return 2

    # The corpus programs read a file in the current directory.
    workdir = tempfile.mkdtemp(prefix="vtreff-")
    try:
        with open(os.path.join(workdir, "probe.txt"), "w") as handle:
            handle.writelines(
                f"line {i:03}  alpha beta gamma delta epsilon zeta eta theta\n"
                for i in range(200)
            )

        results = []
        if corpus:
            quoted_workdir = shlex.quote(workdir)
            for label, cmd in CORPUS:
                results.append(
                    run_one(label, f"sh -c {shlex.quote(f'cd {quoted_workdir} && {cmd}')}", opts)
                )
        for index, cmd in enumerate(commands):
            tokens = cmd.split()
            results.append(run_one(tokens[0] if tokens else f"cmd{index}", cmd, opts))
    finally:
        shutil.rmtree(workdir, ignore_errors=True)

    passed = sum(1 for ok in results if ok)
    print(f"\n{passed}/{len(results)} grids match the reference terminal")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
