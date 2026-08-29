# bitty agent guide

## Scope and authority

- This file governs only the independent `bitty` Git repository.
- The umbrella directory is not a Git repository and sibling repositories own
  their own Git, CarryCtx, CI, releases, and agent guidance.
- All formal Bitty repositories belong under <https://github.com/bitty-terminal>.
- `bitty-docs` is the canonical source for product, architecture, security,
  configuration, interface, and project decisions.

## Current phase

- The repository is unborn and the product is pre-implementation.
- Governance, documentation, toolchain, and CI initialization may proceed only
  under an explicitly scoped task.
- Do not add product code unless a later task authorizes it and its architecture
  and security gates are accepted.
- Rust components use edition 2024. Dependencies, workspace layout, MSRV,
  nightly policy, and release profiles remain undecided until an ADR accepts
  them.
- Never describe planned behavior, a candidate dependency, or a configuration
  file as implemented evidence.

## Read before acting

1. Read this guide and the applicable files in `.carryctx/rules/`.
2. Adopt the assigned persona in `.carryctx/personas/`.
3. Read the task, team context, exact scopes, dependencies, and relevant
   canonical contracts in `bitty-docs`.
4. Use `ctxctl outline` before targeted `symbol`, `read`, or `deps` inspection.

## CarryCtx workflow

- CarryCtx is the durable execution record; it does not spawn agents.
- Bind a named agent and session to the task before work. Record progress,
  decisions, risks, blockers, handoffs, and checkpoints while work is active.
- Map GitHub Issue intent to a CarryCtx task; repository ownership to a team;
  ordering to dependencies; edits to exact scopes; active work to a session;
  and recovery points to checkpoints.
- Subagents perform narrowly scoped implementation. The commander coordinates,
  reads durable state back, verifies the diff, and runs acceptance gates.
- Independent review is required before completion. Self-reports are not
  acceptance evidence.

## Delivery lifecycle

- The normal lifecycle is GitHub Issue, CarryCtx task, team/dependencies/scopes,
  named session, isolated worktree and branch, coherent commits, pull request,
  independent review plus CI, merge, documentation synchronization, final
  checkpoint, task completion, and Issue closure.
- After the first commit, parallel implementation uses dedicated worktrees and
  branches. Branches use `ctx-XXXX/<type>-<short-slug>` where `XXXX` is the
  owning CarryCtx task number, `<type>` is one of `feat|fix|chore|docs`, and
  the slug is short kebab-case (for example `ctx-0031/feat-isolation-rfc`);
  CarryCtx-bound worktrees live at `.worktrees/ctx-XXXX-<type>-<short-slug>`
  with `/` mapped to `-`. One branch per task; commander housekeeping branches
  may use `cmd/<slug>`. Declare cross-repository dependencies and merge order
  explicitly.
- Before the first commit, branches, worktrees, commits, and pull requests are
  unavailable. The commander may authorize a shared checkout only for disjoint
  scopes with CI-equivalent local checks. This exception ends at initialization.
- Do not commit, push, merge, publish, or mutate remote state unless the task or
  user explicitly authorizes it.

### Issue hygiene (labels and milestones)

- Every GitHub Issue and PR must have appropriate `labels` (e.g., `feat`, `fix`, `docs`, `chore`, `P0`, `area:pty` etc.) and `milestone` (e.g., `v0.0.1`, `v0.1.0`, `v1.0`) when applicable. Write issues with clear title, description, acceptance criteria, and link to CarryCtx task and related RFC/OQ.
- Use `gh issue create --label "feat,area:runtime" --milestone "v0.0.1"` and `gh issue edit`/`gh pr edit` to add labels/milestones. Every PR must also carry labels and milestone via `gh pr edit` (or `gh issue edit` for a PR number), e.g. `gh pr edit 108 --add-label "feat,area:ipc,P0" --milestone "v0.0.1"` or `gh issue edit 108 --add-label "feat,area:ipc,P0" --milestone "v0.0.1"`.
- Every carryctx task must include `Priority: P0/P1/P2 | Area: xxx | Labels: feat,area:xxx,P0 | Milestone: v0.0.1 | RFC: OQ-xxx | Task: CTX-XXXX` in description, subagent must read and apply via `gh issue edit` / `gh pr edit`, missing considered NEEDS-FIX.

### Local gates before push (mandatory)

- Before pushing any branch: run repository justfile gates locally and ensure 0 issues: `just check` (fmt-check + clippy -D warnings + test + actionlint + markdownlint) plus full `cargo test --workspace --all-targets --locked` and `cargo check --target x86_64-pc-windows-gnu --workspace --all-targets` and validate GitHub workflows with `act -n` (or `act --dry-run`) for `.github/workflows/ci.yml` and `.github/workflows/codeql.yml`. All must pass. `act` only checks workflow syntax, not runtime or performance, so `cargo test` must still pass locally; do not rely on `act` alone. Never push with known local failures to save CI.
- Also run `actionlint -color` and `act -n` explicitly when workflows change; install `act` if missing (`which act` else note absence) but do not skip `just check`/`cargo test`/`cargo check`.
- Verify no `TODO/FIXME` and frontmatter/links valid for docs.

### Remote monitoring and merge (bitty)

- After push, monitor via `HTTPS_PROXY=$NETWORK_PROXY gh pr checks <PR>` poll 30s until CodeQL, Quality gates, Windows all pass, mergeable==MERGEABLE, then `gh pr merge --squash`.

### Continuous patrol and Code Review

- Every completed task receives independent commander/reviewer review of its diff before acceptance (mandatory post-completion Code Review). Record defects via CarryCtx progress/risk and convert to follow-up tasks.
- Weekly `delegate_task` patrol: `carryctx team status` + `rg --files` + `just check` drift detection, then `compress` closed sections into dense summaries to keep context lean.

## Documentation contract

- Repository-owned documentation is English-only.
- Synchronize affected canonical material in `bitty-docs` when architecture,
  security, public behavior, configuration, compatibility, or developer
  workflows change.
- Distinguish normative requirements, accepted decisions, candidates, open
  questions, and implemented behavior. Implementation claims require code,
  test, or release evidence from this repository.
- Documentation synchronization is part of definition of done, not deferred
  cleanup.

## Architecture and security

- Preserve Terminal Truth and keep protocol correctness, PTY ownership,
  rendering mechanisms, input encoding, platform gates, and security policy in
  the core boundary.
- Treat PTY bytes, plugins, project files, IPC/MCP clients, packages, and
  reference repositories as untrusted across every boundary.
- P0 security controls are release blockers and are currently unimplemented.
  Never add a temporary bypass, ambient authority, unbounded parser/resource
  path, native in-process plugin, or allow-all capability.
- Plugin, Agent, MCP, IPC, protocol, clipboard, filesystem, process, network,
  URL, debug, and sensitive-data changes require focused security review.
- Preserve a safe startup path with no third-party plugins.

## Performance and verification

- Performance claims require reproducible benchmarks, named workloads,
  hardware/software context, baselines, and accepted budgets.
- Keep plugins out of terminal, render, and input hot paths. Bound queues,
  payloads, decoded resources, memory, callbacks, and task execution.
- Run checks proportionate to the change, including formatting, linting, tests,
  fuzzing, platform checks, security gates, and documentation validation where
  applicable.
- Inspect the final scope and preserve unrelated or untracked work.

## Workspace hygiene

- Run Git and CarryCtx inside this repository, never at the umbrella root.
- Use the persistent workspace `../tmp/`, not `/tmp`; references belong under
  `../tmp/references/` and remain untrusted, read-only evidence.
- Prefer moving obsolete files to a collision-safe path under
  `../.trash/bitty/<task-id>/` instead of `rm` or `rmdir`.
- Do not execute reference scripts, hooks, binaries, or installers without an
  explicit reviewed need. Podman is optional when isolation is justified.

## Handoff

- Report changed files, exact verification evidence, unresolved risks, and
  remaining work.
- Implementers request review rather than completing their own task. The
  independent reviewer records findings or acceptance before the commander
  completes it.
