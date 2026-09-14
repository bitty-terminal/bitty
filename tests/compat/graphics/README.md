<!-- markdownlint-disable MD025 -->

# Graphics Protocol Compatibility (`tests/compat/graphics`)

Kitty graphics protocol (`APC G ... ST`) admission, chunked reassembly, and
bounded rejection; headless bounded.

## Source

- Single-chunk and chunked `APC G` transmissions shaped after the assembler
  fixtures in `crates/bitty-vt/src/kitty_apc.rs`
  (`chunked_reassembly_is_exact`,
  `chunked_empty_edge_chunks_assemble_chafa_shape`) and the chafa
  `--format kitty` wire shape.
- Terminal state treats completed transmissions as inert
  (`TerminalAction::KittyGraphics` is routed to the runtime `KittyImageLayer`,
  not to `State`); pixel placement and paint are owned by
  `bitty-runtime` (`kitty_transmit_image`, `kitty_display_image`) and its
  headless tests under `crates/bitty-runtime/tests/`. These corpora prove the
  parser-to-action admission path stays bounded and deterministic in the
  compat-lab.

## Bounds

- `#![forbid(unsafe_code)]`, headless, `MAX_CORPUS_BYTES = 8 KiB`,
  `MAX_ACTIONS = 4096`, `KITTY_APC_LEDGER_CAP` in `bitty-vt`.
- No window/GPU — corpora are escape bytes; rendering is out of compat-lab
  scope.

## Layout

```text
graphics/
  README.md
  corpus/
    01-kitty-image-single.bin  # a=T f=32 2x2 RGBA single chunk
    02-kitty-image-chunked.bin # m=1/m=0 chunked reassembly (chafa shape)
```
