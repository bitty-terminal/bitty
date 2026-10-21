# Pinned bar plugin fixture

Vendored copy of [`bitty-terminal/bar`](https://github.com/bitty-terminal/bar)
at `d74dd4ca` (`fix: icon and button follow-ups from the #31 review (bar#32)`),
the revision the expanded runtime matrix ran for bitty#2425.

License: MIT (see `LICENSE`, copied from the same revision).

| File                | SHA-256                                                            |
| ------------------- | ------------------------------------------------------------------ |
| `bitty-plugin.toml` | `c86d849367657e4423e08204ba4404fe3ecc7332de6c8a3a55e2202fc6ab2c41` |
| `lua/bar/init.lua`  | `3e8bd934bbe7e002e835f6d44a9713fa79fce31d3ab5cf4eb51cba9e710eb09c` |

Purpose: `tests/bar_activation_budget.rs` activates the matrix `bar-heavy`
plan (`workspaces` / `clock` / `memory` + `pulseaudio`) through
`PluginRuntime` on the production RC-1 budget. The test fails if activation
work goes back to the hundreds of milliseconds the matrix recorded
(`WallClockExceeded { elapsed_ms: 159, budget_ms: 50 }`).

Do not refresh this fixture in passing. A deliberately bumped pin is its own
change with the two hashes above updated in the same commit; the test's
command/event counts and the plan shape are part of the assertion surface.
