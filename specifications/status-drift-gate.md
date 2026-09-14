# Status drift gate (CTX-0437)

The workspace has 19 crates (see `Cargo.toml` `[workspace] members`).
`scripts/check-status-drift.sh` enforces this count against
`CONTRIBUTING.md` and this document; see the script header for the full
rule set.

## Why

Research note 024 §4 found code-doc status contradictions accumulating:
`bitty-agent` claimed OQ-018 open while `bitty-ipc` recorded the accepted
IPC-Agent RFC closing it, and `bitty-runtime` described the Plugin Platform
RFC as Proposed/Draft after acceptance. This gate makes those contradictions
fail `just check` and CI instead of drifting silently.

## What it checks

1. **OQ status claims** — in-repo `*.rs`/`*.md` lines that pair a tabled OQ
   with stale language (`remain(s) open`, `unresolved`, `has/have not
landed`, `not landed`, `not yet implemented`, `will be decided when`,
   `future`) fail when the canonical register marks that OQ
   accepted/closed.
2. **Crate count** — every `N crates` / `N-crate` statement in
   `CONTRIBUTING.md` and this document must equal the `Cargo.toml`
   workspace member count, else the gate fails with both numbers.
3. **Submodule wiring** — `.gitmodules` must carry the `docs` mount
   (`path = docs`, URL referencing `bitty-terminal-docs`), and
   `README.md` / `CONTRIBUTING.md` / `AGENTS.md` must not claim later-phase
   wiring.
4. **RFC status spot-checks** — in-repo Proposed/Draft claims about the
   Plugin Platform, Isolation Resource, and IPC-Agent RFCs fail when the
   owning frontmatter (sibling checkout when present, else the vendored
   `accepted` value) is `accepted`.

## Expectations table and sources

| OQ     | Vendored        | Canonical source (read-only, verified before encoding)                                                                                                                                             |
| ------ | --------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| OQ-008 | accepted        | bitty-docs `decisions/open-questions.md`: Accepted rich-presentation-rfc (closed 2026-08-28)                                                                                                       |
| OQ-011 | accepted        | bitty-docs `decisions/open-questions.md`: Accepted plugin-platform-rfc; owning `bitty-plugins-docs/specifications/plugin-platform-rfc.md` frontmatter `status: accepted`                           |
| OQ-012 | accepted        | Same register + owning RFC as OQ-011                                                                                                                                                               |
| OQ-013 | accepted        | Same register + owning RFC as OQ-011 (DropOldest v1 default closed)                                                                                                                                |
| OQ-014 | accepted        | bitty-docs `decisions/open-questions.md`: Accepted isolation-resource-rfc (closed 2026-08-28); owning `bitty-plugins-docs/specifications/isolation-resource-rfc.md` frontmatter `status: accepted` |
| OQ-018 | accepted        | bitty-docs `decisions/open-questions.md`: Accepted ipc-agent-rfc (closed 2026-08-29); owning `bitty-ai-docs/specifications/ipc-agent-rfc.md` frontmatter `status: accepted`                        |
| OQ-053 | accepted/closed | bitty-docs `decisions/open-questions.md`: Accepted Bundled-Plugin Split Decision (closed 2026-09-14)                                                                                               |

Owning RFC statuses are re-read at gate time from
`../bitty-plugins-docs/...` / `../bitty-ai-docs/...` / `docs/...` when
those checkouts exist; otherwise the vendored `accepted` value applies
(CI checks out only this repository).

## Running

```sh
./scripts/check-status-drift.sh
./scripts/tests/check-status-drift.test.sh
just status-drift
just status-drift-test
```

Timing is under 60 seconds (about 13 seconds on the reference host).
Deterministic: `git ls-files` + `grep`/`sed` scans only, no network,
fixed rule order. There is deliberately no `rg` (ripgrep) dependency:
`rg` is absent from `ubuntu-latest` runners.

## Escape hatch

`// status-drift-exempt: <reason>` (or `# ...` in shell/docs) on the hit
line or one of the 3 lines above it exempts that hit. Use only with a
reviewed reason; the exemption text itself is grep-able.

## CI

Wired into `just check` (`status-drift`, `status-drift-test`) and the
`Quality gates` job in `.github/workflows/ci.yml`.
