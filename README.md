# Bitty

Bitty is a pre-implementation terminal project. This repository currently
contains only a dependency-free Rust workspace and the read-only quality gates
needed to validate that scaffold. It does not provide a terminal, command-line
interface, public Rust API, configuration runtime, plugin runtime, or other
product behavior.

The accepted bootstrap boundary is recorded in
[ADR 0001](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/adrs/ADR-0001-repository-bootstrap-baseline.md)
and the
[repository bootstrap guide](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/development/repository-bootstrap.md).
Canonical product, architecture, security, and project documentation belongs in
the [bitty-docs repository](https://github.com/bitty-terminal/bitty-docs).

## Current scaffold

- The virtual Cargo workspace has exactly two independent packages:
  `bitty-core` and `bitty-app`.
- Both packages use Rust edition 2024, are not publishable, and have empty
  dependency tables.
- The source targets contain only the minimum needed to compile. There is no
  accepted dependency edge between the packages.
- The pinned stable toolchain includes `rustfmt` and Clippy.
- `just check` runs formatting, Clippy, tests, and workflow linting without
  rewriting source files.

## Status and deferred decisions

This scaffold is foundation evidence, not a product release. The final crate
graph, dependencies, MSRV, license, release profiles, release automation,
publication policy, platform tiers, and product behavior remain deferred to
separate reviewed decisions and tasks.

No commit, branch, pull request, package publication, or release is implied by
the presence of these files.
