<!-- markdownlint-disable MD025 MD060 -->

# Fuzzing — cargo-fuzz targets and retained corpora

`fuzz/` holds the cargo-fuzz target crate for the VT parser family plus the
content-addressed seed corpora cited by the R-001 evidence.

- `fuzz/Cargo.toml` — `bitty-fuzz` package, `cargo-fuzz = true`, three
  `[[bin]]` targets, path dependency on `crates/bitty-vt`.
- `fuzz/fuzz_targets/` — target sources (`common.rs` shared helpers).
- `fuzz/corpora/` — retained seed corpora (this file documents them).
- `fuzz/corpora/rich/` — separate R-002 `ImageStore` corpus; see
  `fuzz/corpora/rich/README.md`.

## Target crate

| Target           | Surface                                                                 |
| ---------------- | ----------------------------------------------------------------------- |
| `vt_parser`      | arbitrary bytes into `bitty_vt::Parser` (whole + byte-wise replay)      |
| `osc_string`     | OSC payload bodies (`ESC ] ...` BEL / `ST`)                             |
| `dcs_apc_string` | DCS/APC/SOS/PM bodies plus kitty `APC G` single-shot and chunked shapes |

Every target feeds bytes through the real public `bitty_vt::Parser::advance`
API and drives it to completion. Each input asserts two contract invariants:
a fresh parser is deterministic across identical runs, and a byte-wise feed
produces the same action sequence as one bulk feed. The string targets also
replay each envelope with a small kitty ledger cap so the raw-`APC`
overflow/chunk-growth rejection paths (`KittyApcAssembler`) are reachable from
a short input without allocating the production 320 MB ledger.

Bounds (enforced in `fuzz/fuzz_targets/common.rs`, not by the harness):

- Input is truncated to `MAX_INPUT_BYTES` (32 KiB); every retained seed is
  ≤ 8 KiB, so seeds are never truncated.
- Retained actions are capped at `MAX_ACTIONS` (65536); every byte is still
  parsed, only the comparison vector stops growing.
- No wall-clock, sleeps, randomness, filesystem, or network access exists in a
  target. A hang trips libFuzzer's `-timeout` (25 s in the local smoke); a
  panic/abort is a crash artifact. Both fail the campaign.

`fuzz/` is intentionally **not** a root workspace member: `fuzz/Cargo.toml`
carries an empty `[workspace]` table, so `cargo fmt/clippy/test --workspace`
in the root and `just check` never build libFuzzer targets. Fuzzing requires
nightly; the stable workspace gate is unaffected.

## How to run

```sh
# Build all targets (compile-only check)
cargo +nightly fuzz build

# Run one target; a crash writes under fuzz/artifacts/<target>/
cargo +nightly fuzz run vt_parser
cargo +nightly fuzz run osc_string
cargo +nightly fuzz run dcs_apc_string

# Bounded smoke against the committed seeds
cargo +nightly fuzz run vt_parser fuzz/corpora/vt_parser -- -runs=20000
```

cargo-fuzz needs the `nightly` toolchain and `cargo-fuzz`; install it into a
user directory with `cargo +nightly install cargo-fuzz --locked`. Without
nightly, a compile-only check is
`cargo check --manifest-path fuzz/Cargo.toml` (stable can type-check the
target crate).

## Corpus retention policy on new findings

- A crash/hang is a finding: fix it, then commit the **minimized** artifact
  (`cargo +nightly fuzz tmin <target> <artifact>`) into
  `fuzz/corpora/<target>/` with a descriptive name, and add its row to the
  coverage table below.
- New coverage discovered by long runs (`fuzz/corpus/<target>/` is
  gitignored): promote the interesting cases into `fuzz/corpora/<target>/`
  deliberately — never commit the scratch corpus wholesale. Bound any added
  seed to the target's `MAX_INPUT_BYTES`.
- Regenerate the manifest after any edit (from the repo root, so the
  committed `fuzz/corpora/<target>/` path prefix stays stable):
  `(cd fuzz/corpora/<target> && sha256sum *.bin | sed 's#  #  fuzz/corpora/<target>/#') > fuzz/corpora/<target>/SHA256SUMS`
- Verify retention from the repo root:
  `sha256sum -c fuzz/corpora/<target>/SHA256SUMS`.
- Corpora are retained evidence: they are never executed as part of the
  stable workspace build and are not imported as dependencies.

## Retained corpora

| Directory                     | Target           | Seeds | Source                                         |
| ----------------------------- | ---------------- | ----- | ---------------------------------------------- |
| `fuzz/corpora/vt_parser`      | `vt_parser`      | 30    | byte-identical copy of `fuzz/corpora/vt/*.bin` |
| `fuzz/corpora/osc_string`     | `osc_string`     | 40    | OSC bodies extracted from `vt/` + hand-curated |
| `fuzz/corpora/dcs_apc_string` | `dcs_apc_string` | 18    | DCS/APC/SOS/PM bodies + kitty `G` shapes       |
| `fuzz/corpora/vt`             | (source corpus)  | 30    | R-001 retained corpus (below)                  |
| `fuzz/corpora/rich`           | (ImageStore)     | 20    | R-002 corpus, own README                       |

The `vt_parser` seeds are byte-identical copies of the R-001 corpus, so the
fuzz target and the in-repo evidence share one corpus. The derived target
corpora are additive; the `vt/` corpus remains the canonical R-001 artifact.

## Coverage (adversarial dimensions per P0-AC-002, R-001)

| File                                     | Dimension                                       | P0-AC   | Limit exercised                              |
| ---------------------------------------- | ----------------------------------------------- | ------- | -------------------------------------------- |
| `01-plain-text.bin`                      | printable ground state                          | 001     | baseline                                     |
| `02-c0-controls.bin`                     | C0 controls, tab/bell/line                      | 001/002 | control resync                               |
| `03-cursor-addressing.bin`               | CSI cursor `H`/`f`/`d`/`\``                     | 001     | coordinate defaults/sentinels                |
| `04-sgr-colors.bin`                      | SGR indexed+RGB, colon/semicolon forms          | 001     | param parsing                                |
| `05-decset-decrst.bin`                   | DECSET/DECRST `?25h`, `?1048`, `?1002/1006`     | 001     | private-mode dispatch                        |
| `06-erase-scroll.bin`                    | erase/scroll/region `J/K/X/S/T/r`               | 001     | mode rejection                               |
| `07-charsets-shifts.bin`                 | charset `(/)/`/`*`/`+` and `SO`/`SI` shifts     | 001     | charset lanes                                |
| `08-osc-title-hyperlink.bin`             | OSC 0/2 title                                   | 001     | join_segments                                |
| `09-osc-hyperlink-prompt.bin`            | OSC 8 hyperlink, OSC 133 prompt marks           | 001     | hyperlink id/prompt map                      |
| `10-osc-clipboard-truncated.bin`         | OSC 52 clipboard, bounded 543 B                 | 002     | BoundedBytes                                 |
| `11-malformed-resync.bin`                | `ESC [` swallowed, then `ESC[31m`               | 002     | malformed resync                             |
| `12-dcs-and-status.bin`                  | DCS `P…q` plus DSR/DA CSI                       | 002     | DCS→Unknown                                  |
| `13-utf8-invalid-split.bin`              | invalid `FF FE` + split 🎉                      | 002     | U+FFFD / partial UTF-8                       |
| `14-param-stress.bin`                    | 32/64-param SGR overflow + u16 saturation       | 001     | MAX_PARAMS=32, C128, CSI param truncation    |
| `15-truncated-escape.bin`                | truncated `ESC`, `ESC[`, `ESC[31`, `ESC]`       | 002     | truncated ESC/CSI/OSC                        |
| `16-unterminated-osc.bin`                | OSC `]2;…` no BEL/ST                            | 002     | unterminated OSC is inert until BEL/ST       |
| `17-unterminated-dcs.bin`                | DCS `P+q544e …` no ST                           | 002     | unterminated DCS is pending until `ESC\`     |
| `18-unterminated-apc-sos-pm.bin`         | `ESC _/^/X` strings no ST                       | 002     | APC/SOS/PM inert (vte `SosPmApcString`)      |
| `19-invalid-utf8-heavy.bin`              | `FF FE 80 81 C080` surrogate/overlong           | 002     | heavy FFFD soup                              |
| `20-random-byte-soup.bin` / `21-…-2.bin` | 8 KiB PRNG soup ×2 (distinct seeds)             | 002     | random-byte fuzz                             |
| `22-csi-u16-boundary.bin`                | `65534/65535/65536/99999` `C`                   | 001     | C128 u16 saturation at 65535                 |
| `23-param-count-boundary.bin`            | 32 vs 64 semicolon params `m`                   | 001     | param-count overflow → `ignore` but dispatch |
| `24-osc-payload-*.bin` (×6)              | `52;c;` payload 1024/1025/2048/4095/4096/5000 B | 001/002 | vte MAX_OSC_RAW=1024 + Bounded 4096          |
| `25-chunk-split-esc.bin`                 | ESC at chunk boundary + OSC/DCS interleaved     | 002     | incremental chunking identity                |
| `SHA256SUMS`                             | manifest                                        | —       | `sha256sum *.bin` sorted                     |

## Gates

- `cargo test -p bitty-vt --all-targets --locked` covers every limit by
  named test (e.g. `csi_numeric_boundary_at_u16_max_saturates_deterministically`,
  `osc_payload_at_raw_and_bounded_caps_truncates_deterministically`,
  `truncated_escape_resynchronizes_deterministically`,
  `unterminated_osc_dcs_apc_strings_are_panic_free_and_deterministic`,
  `boundary_matrix_zero_panics_all_limits`) — all parse-twice deterministic
  and zero panics/hangs (P0-AC-001 threshold).
- `crates/bitty-vt/tests/replay.rs::seeds_corpus_is_panic_free_and_deterministic`
  replays every `seeds/*.bin` panic-free and deterministic (≥10 seeds, actually
  14); `crates/bitty-vt/tests/harness.rs::vt_corpus_bounded_and_deterministic_for_bitty_vt`
  replays every `tests/compat/*/corpus/*.bin` (≥16 corpora).
- The `fuzz/` target crate is compile-checked with `cargo +nightly fuzz build`
  (or `cargo check --manifest-path fuzz/Cargo.toml`); it is outside the stable
  workspace and does not affect `just check`.
- `fuzz/corpora/vt/` retention itself satisfies P0-AC-002
  `corpus retained in-repo`; the SHA256 manifest satisfies the risk-evidence
  RFC artifact `corpus hash` requirement.

## Regeneration

Seeds `01–14` are hand-curated; `15–25` are generated by the maintainer
script (see commit message of the seeding commit for the exact `python3 -c`
invocation); `20`/`21` use distinct PRNG seeds (`0x20260826deadbeef` and
`0x20260830`) for diversity. Re-run `sha256sum fuzz/corpora/vt/*.bin | sort >
fuzz/corpora/vt/SHA256SUMS` after any edit so the manifest stays in sync.
