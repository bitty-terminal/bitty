# CTX-2413 expanded runtime matrix harness

Harness and evidence for bitty-terminal/bitty#2413: the owner-requested
runtime acceptance dimensions as automated rows.

- `harness.py` — the matrix runner (extended from the CTX-1344 harness).
- `EXPANDED-PLAN.md` — every dimension, configuration, and row id.
- `fixtures/` — component broker fixtures (harness input, never product source).
- `headed-harness.sh` — prerequisite-checking wrapper for headed runs.

## Prerequisites

- Hyprland session (`hyprctl version` works), `grim`, `ydotool`, `wtype`,
  `python3` with Pillow, `vim`, `tesseract` (optional, OCR evidence only).
- A candidate `bitty` binary. Build it outside the worktree:

  ```sh
  CARGO_TARGET_DIR=$BITTY_WORKSPACE/.targets/ctx-2413-expanded-matrix \
    cargo build -p bitty-terminal --locked
  ```

- Wallpapers under `~/Pictures/Wallpapers` (or pass `--wallpaper`).
- Network for plugin source clones; `--network-extensions` additionally
  installs the pinned published components (`graphics` 0.0.24, `net` 0.0.1).

## Runs

```sh
# plan only (no desktop interaction)
python3 harness.py matrix --plan expanded --binary <candidate>

# CTX-2413 smoke: one row per new dimension
python3 harness.py run --plan smoke2413 --binary <candidate> \
  --out recording/ctx-2413-expanded-matrix/smoke-0.0.25

# full 0.0.26 campaign
python3 harness.py run --plan expanded --binary <candidate> \
  --out recording/ctx-2413-expanded-matrix/full-0.0.26 \
  --network-extensions --reset-xdg --reuse-sources

# RESULTS.md for an existing run
python3 harness.py report --out <run-dir>

# restore a recorded desktop if a run was interrupted
python3 harness.py restore-desktop --out <run-dir>
```

`--only <cid,...>` runs a subset of a plan. `--strict` exits 1 when any row is
`FAIL` (default: product failures are data, harness defects are exit 3).

## Isolation and safety

- XDG roots default to `$XDG_RUNTIME_DIR/ctx-2413/m26` (fallback
  `/tmp/ctx-2413/m26`) and are refused unless the path contains `ctx-2413`.
  Every socket path is asserted under the 100-byte AF_UNIX budget.
- The owner's config, plugin store, session, and clipboard are never touched.
- Headed runs use Hyprland workspace 5; the desktop (workspace, focused
  window, clients) is recorded before the run and restored afterwards.
- No pattern kills. Only PIDs recorded by the harness are signalled, and only
  after a window close and SIGTERM both fail.
- The harness never edits product source.

## Statuses

`PASS`, `FAIL` (product), `BLOCKED` (open dependency, never a pass),
`NOT_EXERCISED` (environmental skip, recorded), `NOT_APPLICABLE` (dimension
cannot exist in this build). See `EXPANDED-PLAN.md`.

## Evidence

Each run writes `verification.json` (all rows), `RESULTS.md` (rendered),
`provenance.txt`, `desktop-before.json`, `desktop-after.json`, `pids.json`,
`shots/`, `logs/`, and `assets/`. Rows reference screenshots by path.

## Smoke vs campaign

`smoke2413` proves the new rows execute against the current main binary. The
full campaign runs later on the 0.0.26 candidate and publishes `RESULTS.md`
with pass/fail/not-applicable per row. Product defects found by either run are
filed as issues; the harness never fixes them.
