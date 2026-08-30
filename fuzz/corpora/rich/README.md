<!-- markdownlint-disable MD025 MD060 -->

# Fuzz corpora — rich ImageStore (R-002, P0-AC-003/004)

Retained corpus for the bounded rich `ImageStore` adversarial suite.
This directory is the content-addressable artifact cited by the risk
evidence RFC for R-002 (R-002 → P0-AC-003 `decompression-bomb
pre-allocation` + P0-AC-004 `aggregate image-store budget`) and by the
evidence matrix row `R-002 Evidence: P0-AC-003/004 / fuzz/corpora/rich/`.

## Location and hash

- Root: `fuzz/corpora/rich/` (20 `*.bin` files, ~17 KiB total).
- Manifest: `fuzz/corpora/rich/SHA256SUMS` — per-file `sha256sum` output,
  committed alongside the corpora so any drift is diff-visible.
- Corporate scope: `fuzz/corpora/**` belongs to CTX-0089
  `ctx-0089/feat-rich-r002-verification` (disjoint from Subagent B's
  `docs/security/**`). Do not move corpora into `docs/security/**`.
- Earlier vt corpus `fuzz/corpora/vt/` (30 files, R-001) is retained
  unchanged; this directory is additive for R-002.

## Corpus format

Files `01–14` encode a 15-byte header `u32 LE width | u32 LE
height | u32 LE compressed_len | u16 LE frame_count | u8 source`
followed by filler bytes. Files `15–17` encode a `u32 LE count`
plus filler for aggregate-load stress. Files `18–19` are 8 KiB
deterministic PRNG soups (seeds `0x20260830`, `0xDEADBEEF`) for
random-byte fuzz. A future `cargo fuzz` harness may feed these
headers directly to `ImageStore::insert` to exercise every IMG
admission path headlessly.

Corpus files are **retained evidence only**; they carry no execution
and are not imported as dependencies. The hash manifest is the
verification artifact — do not claim Verified without citing the hash.

## Coverage (adversarial dimensions per P0-AC-003/004)

| File                                     | Dimension                  | P0-AC   | Limit exercised          |
| ---------------------------------------- | -------------------------- | ------- | ------------------------ |
| `01-compressed-bomb-4mib-plus1.bin`      | compressed payload 4 MiB+1 | 003     | IMG-1 4 MiB              |
| `02-dimensions-bomb-8192x8192.bin`       | tiny wire, huge 8192 dims  | 003     | IMG-2 4096               |
| `03-animation-bomb-4096x4096x64.bin`     | max decoded ×64 = 4 GiB    | 003     | IMG-7 256 MiB aggregate  |
| `04-zero-width.bin`                      | width 0                    | 003     | ZeroDimension            |
| `05-zero-height.bin`                     | height 0                   | 003     | ZeroDimension            |
| `06-dimensions-at-cap-4096x4096.bin`     | exactly at 4096 cap        | 003     | IMG-2 boundary OK        |
| `07-decoded-at-cap-64mib.bin`            | 4096×4096×4 = 64 MiB       | 003     | IMG-3 boundary OK        |
| `08-animation-at-cap-256mib.bin`         | 1024×1024×4×64 = 256 MiB   | 003     | IMG-7 boundary OK        |
| `09-animation-over-cap-1025x1024x64.bin` | one over aggregate         | 003     | IMG-7 rejection          |
| `10-frames-255-clamped-bomb.bin`         | 255 clamped→64 then bomb   | 003     | IMG-6 clamp + IMG-7      |
| `11-frames-0-clamped-ok.bin`             | 0 clamped→1 then OK        | 003     | IMG-6 clamp low          |
| `12-overflow-dims-u32-max.bin`           | u32::MAX dims              | 003     | IMG-2 overflow-checked   |
| `13-tiny-wire-huge-decoded.bin`          | 32 B wire, 64 MiB decoded  | 003     | decompression bomb shape |
| `14-placement-bomb-128-plus5.bin`        | 133 placements             | 004     | IMG-8 128 FIFO           |
| `15-aggregate-count-300-tiny.bin`        | 300×1×1 tiny               | 004     | IMG-5 256 count FIFO     |
| `16-aggregate-bytes-20-large.bin`        | 20×64 MiB = 1280 MiB       | 004     | IMG-4 256 MiB FIFO       |
| `17-aggregate-mixed-300.bin`             | 30 large + 270 tiny        | 004     | IMG-4/5 mixed FIFO       |
| `18-random-soup-8kib.bin`                | 8 KiB PRNG seed A          | 003/004 | random-byte fuzz         |
| `19-random-soup-2-8kib.bin`              | 8 KiB PRNG seed B          | 003/004 | random-byte fuzz diverse |
| `20-compressed-at-cap-4mib.bin`          | exactly 4 MiB              | 003     | IMG-1 boundary OK        |
| `SHA256SUMS`                             | manifest                   | —       | `sha256sum *.bin` sorted |

## Gates

- `cargo test -p bitty-rich --all-targets --locked` covers every IMG
  limit by named test (e.g. `compressed_exact_cap_ok_one_over_denied_no_alloc`,
  `dimensions_exact_cap_ok_one_over_denied_no_alloc`,
  `animation_total_boundary_256mib_ok_one_over_denied`,
  `decompression_bomb_pre_allocation_no_alloc_peak_under_64mib_per_image`,
  `sustained_load_bytes_invariant_fifo_256mib`,
  `sustained_load_count_invariant_fifo_256`,
  `placement_admission_128_fifo_and_image_eviction_cleans_placements`) —
  all headless deterministic and zero panics/hangs (P0-AC-003/004
  thresholds). 93 headless tests pass in this crate (65→93 on this
  branch).
- `fuzz/corpora/rich/` retention itself satisfies P0-AC-003/004
  `corpus retained in-repo`; the SHA256 manifest satisfies the
  risk-evidence RFC artifact `corpus hash` requirement.
- Aggregate invariants: `total_bytes ≤ 256 MiB`, `len ≤ 256`,
  `placement_len ≤ 128`, `decoded_bytes ≤ 64 MiB`, `total_bytes ==
sum(iter total_bytes)` all asserted under sustained load.

## Regeneration

Seeds are hand-curated with deterministic `python3` invocations (see
commit message of the seeding commit for the exact `python3 << 'PY'`
block). Re-run
`sha256sum fuzz/corpora/rich/*.bin | sort > fuzz/corpora/rich/SHA256SUMS`
after any edit so the manifest stays in sync.
