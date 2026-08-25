# Contributing

Thank you for your interest in contributing to Bitty. This repository is in a
pre-implementation bootstrap phase: it contains a dependency-free Rust
workspace scaffold and quality gates, not product behavior. Canonical product,
architecture, security, and project documentation lives in the
[bitty-docs](https://github.com/bitty-terminal/bitty-docs) repository.

Start by reading [AGENTS.md](AGENTS.md). It defines the governance model,
CarryCtx workflow, delivery lifecycle, and constraints that every contributor
and agent must follow.

## Prerequisites

- Rust — the exact channel is pinned in `rust-toolchain.toml`; `rustup`
  resolves and installs it automatically (includes `rustfmt` and Clippy)
- `just` — command runner for the quality gates
- `actionlint` — GitHub Actions workflow linting, invoked by `just actionlint`

## Setup

Clone the repository and let `rustup` provision the pinned toolchain. There is
no separate setup target yet; tooling wiring (hooks, additional just targets,
CI workflows) is deferred to follow-up initialization tasks.

## Development loop

All checks run through the justfile:

```bash
just check              # fmt-check + clippy + test + actionlint
just fmt-check          # cargo fmt --all -- --check
just clippy             # cargo clippy --workspace --all-targets --locked -- -D warnings
just test               # cargo test --workspace --all-targets --locked
```

Run `just check` before requesting review. Formatting, lints, tests, and
workflow validation must pass without warnings.

## Delivery lifecycle

Changes follow the standard lifecycle: GitHub Issue, CarryCtx task, branch or
worktree, coherent commits, pull request, independent review plus CI, merge,
then Issue closure. Until the repository's first commit exists, branch, commit,
pull request, and merge stages are unavailable; scoped shared-checkout work is
the only authorized exception. See [AGENTS.md](AGENTS.md) for the normative
rules.

## Committing

Use Conventional Commits:

```text
feat(core): add bounded VT parser skeleton
fix(app): correct workspace member resolution
docs(readme): clarify scaffold scope
```

A Conventional Commits configuration is provided in
`commitlint.config.ts`. Enforcement through Git hooks or CI is not wired up
yet and remains a follow-up task.

## Scope expectations

- Do not add product code, dependencies, or configuration unless an explicitly
  scoped task authorizes it.
- Documentation synchronization in `bitty-docs` is part of definition of done.
- Never describe scaffolding or plans as implemented behavior.

## Reporting issues

Open a GitHub Issue describing the problem, expected behavior, and evidence.
Security vulnerabilities must not be reported through public issues; see
[SECURITY.md](SECURITY.md).
