<!-- markdownlint-disable MD025 MD060 -->

# Fuzz corpora — VT parser (R-001, P0-AC-001/002)

Retained corpus for the bounded VT parser adversarial suite. This directory
is the content-addressable artifact cited by the risk evidence RFC for
R-001 (R-001 → P0-AC-001 `adversarial: limit-boundary suite` +
P0-AC-002 `adversarial: VT/UTF-8/OSC/DCS/APC fuzz`) and by the evidence
matrix row `R-001 Evidence: P0-AC-001/002 / fuzz/corpora/vt/`.

## Location and hash

- Root: `fuzz/corpora/vt/` (30 `*.bin` files, ~35 KiB total).
- Manifest: `fuzz/corpora/vt/SHA256SUMS` — per-file `sha256sum` output,
  committed alongside the corpora so any drift is diff-visible.
- Corporate scope: `fuzz/corpora/**` belongs to CTX-0088
  `ctx-0088/feat-vt-r001-verification` (disjoint from Subagent B's
  `docs/security/**`). Do not move corpora into `docs/security/**`.

## Coverage (adversarial dimensions per P0-AC-002)

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
- `fuzz/corpora/vt/` retention itself satisfies P0-AC-002
  `corpus retained in-repo`; the SHA256 manifest satisfies the risk-evidence
  RFC artifact `corpus hash` requirement.

## Regeneration

Seeds `01–14` are hand-curated; `15–25` are generated by the maintainer
script (see commit message of the seeding commit for the exact `python3 -c`
invocation); `20`/`21` use distinct PRNG seeds (`0x20260826deadbeef` and
`0x20260830`) for diversity. Re-run `sha256sum fuzz/corpora/vt/*.bin | sort >
fuzz/corpora/vt/SHA256SUMS` after any edit so the manifest stays in sync.
