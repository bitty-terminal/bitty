---
title: AI-Named Core Surface Reconciliation with Generic Service Capabilities
description: Candidate design record mapping every AI-named ai_panel.rs item to a generic counterpart or a justified AI-specific residual, with keep/demote verdicts and specified follow-up changes (CTX-0407 gap G-5, OQ-066 linkage)
category: specifications
audience: contributor
document_type: design-record
status: draft
---

<!-- markdownlint-disable MD025 -->

# AI-Named Core Surface Reconciliation with Generic Service Capabilities

> Status: **draft design record** (candidate, not accepted). This document
> reconciles the AI-named Core surface in
> `crates/bitty-runtime/src/ai_panel.rs` with generic panel, service, and
> scope capabilities per the BA-6 pressure-test gate. It authorizes no
> implementation, adds no Core API, removes no Core API, changes no wire
> name, and weakens no accepted contract. Every verdict below is a proposal;
> demotion is specified as follow-up change descriptions, not as code edits.
> Lifecycle: this record is bitty-side candidate text; canonicalization into
> `bitty-terminal-docs` specifications and any register update are follow-up
> tasks in their owning repositories and are explicitly out of scope here.
>
> Task: `CTX-0423` | Issue: `bitty`#700 | Gap: `CTX-0407` G-5
> Related: `OQ-066` (stays open), `DIR-018` step 6, `CTX-0420` (parallel,
> untouched), `RFC-0003` (G-6 mapping, stays separate)

## Claim-status legend

Every statement below carries one label. Unlabeled statements are candidate
proposals from this record.

- **Shipped** — implemented and tested in the `bitty` repository (file and
  symbol cited; reproducible with the repo test suite).
- **Accepted** — decided in an accepted ADR, RFC, or direction, whether or
  not it is implemented yet.
- **Draft** — proposed in a draft RFC or draft specification, not accepted.
- **Open** — recorded in the open-question register, no contract of any kind.
- **Candidate** — proposed by this record, needs its own acceptance.

## Problem statement

The `bitty-ai` vertical-slice pressure test showed that an AI turn runs on
generic Core primitives, and its gap list asks Core to suspect a missing
generic abstraction before granting any AI-specific Core API (**draft**,
pressure-test specification gap G-5; reviewer rule BA-6 in the draft AI
architecture). Gap G-5 names the surface this record reviews: `ai_panel.rs`
declares AI-named Core capabilities (`ai.provider`, `ai.stream`, `ai.model`,
`agent.context.*`, `panel.provider`) that predate the pressure test.

The read-only inventory (**shipped**, `crates/bitty-runtime/src/ai_panel.rs`,
about 1180 lines) is:

1. Nine capability constants: `panel.provider`, `panel.create`,
   `agent.context.terminal`, `agent.context.workspace`,
   `agent.memory:persist`, the `mcp.invoke:` prefix, `ai.provider`,
   `ai.stream`, `ai.model`.
2. Seven exact-match gate helpers (`is_agent_context_terminal_allowed`,
   `is_agent_context_workspace_allowed`, `is_agent_memory_persist_allowed`,
   `is_ai_provider_allowed`, `is_ai_stream_allowed`, `is_ai_model_allowed`,
   `is_mcp_invoke_allowed`) plus `is_valid_mcp_tool_name` and
   `is_valid_agent_id` / `validate_agent_id` (thin alias over the
   `bitty-agent` `AgentId` vocabulary).
3. Pure bounded helpers: workspace files (`64` files / `2 MiB` aggregate /
   `256 KiB` per file), context budget (`32 KiB` per turn), memory ring
   (`32` turns / `64 KiB`), MCP framing (`256 KiB` frame, `512 KiB`
   in-flight, depth `32`), title bound (`128` chars), and tiled-layout
   constructors reusing `LayoutNode` `H` splits and stacks.
4. Two pure data types (`AgentWorkspaceFile`, `AgentMemoryEntry`), three
   free constructors (`create_ai_panel`, `validate_ai_panel_config`,
   `ai_panel_tiled_layout`), and five qualified panel commands
   (`bitty-terminal.ai-panel:open/send/clear/new-session/stop`).

The first-party manifest for the same panel (**shipped**,
`crates/bitty-plugin-host/src/bundled.rs`, `ai_panel_manifest`) requests
`panel.provider` + `panel.create` + `agent.context.terminal` +
`agent.context.workspace` + `agent.memory:persist` + per-tool
`mcp.invoke:read_file` / `mcp.invoke:fetch` + `ai.provider` + `ai.stream` +
`ai.model`.

Sibling panels set the generic precedent (**shipped**): file-manager, git,
browser, and mail panels compose `panel.provider` / `panel.create` with
generic families only (`fs.read` / `fs.write`, `process.spawn:git`,
`browser.embed` / `browser.navigation`, `mcp.invoke:mail.*`,
`network.connect:*`, `terminal.semantic-read`). The ai-panel is the only
panel whose Core authority carries `ai.*` / `agent.*` names.

## Non-goals and explicit non-acceptance

- No Core code is added, changed, or removed by this task (docs-only); in
  particular, no capability string, gate helper, bound, command, or type in
  `ai_panel.rs`, `capability.rs`, `manifest.rs`, or `bundled.rs` is edited
  here. Demotion verdicts are specified follow-up changes, not edits.
- No new AI logic in `ai_panel.rs` is proposed or authorized (**accepted**,
  `DIR-018` step 6). The ownership split is restated, not reopened: Bitty
  owns Panel, Execution, IPC, snapshots, capabilities, rich projection, and
  process/PTY; `bitty-ai` owns Provider, Agent, Session, projection, memory,
  orchestration, and selection.
- The parallel IPC dispatcher workstream (`CTX-0420`, gap G-2/G-3) is not
  touched: this record names its planned generic services as counterparts
  but specifies no dispatcher behavior, handler, or method.
- The G-6 consent-to-scope mapping is not duplicated or revised (**draft**,
  `RFC-0003` in shared governance); this record cites its confidence rows
  and keeps the G-5 Core-surface review separate, as that RFC requires.
- `OQ-066` stays **open**: the `32 KiB` context-budget default remains the
  draft default; per-model budget profiles are undecided and no
  implementation claim is made.
- Nothing here weakens the accepted Panel Runtime RFC, its closed `panel.*`
  family, its single-owner rules, or any normative security control; no
  bypass, ambient authority, or allow-all capability is proposed.

## Accepted and shipped ground (not weakened)

| Fact                                                                                                          | Status   | Authority                                                                            |
| ------------------------------------------------------------------------------------------------------------- | -------- | ------------------------------------------------------------------------------------ |
| Closed panel family `panel.provider` / `panel.create` / `panel.focus` / `panel.overlay`; no invented families | Accepted | Panel Runtime RFC, capability-isolation section                                      |
| Single ownership: runtime owns panels, workspace owns layout, registry owns terminals and PTY fds             | Accepted | Panel Runtime RFC, architectural-placement rules                                     |
| 13-scope IPC registry, none AI-named; per-request server-side evaluation; ledgered per-client consent         | Accepted | IPC and Agent RFC (`OQ-018` closed); `crates/bitty-ipc/src/scope.rs`                 |
| Closed capability tables including the `agent` / `mcp` / `ai` families and the `:PARAMETER` rules             | Shipped  | `crates/bitty-plugin-host/src/capability.rs`, `crates/bitty-package/src/manifest.rs` |
| `agent.memory:persist` and `mcp.invoke:TOOL` are parameterized forms of closed heads, not invented families   | Shipped  | `capability_requires_param` / `check_closed_capability` plus the parse tests         |
| Parallel delivery order: snapshot handler, tool dispatch, ExecutionContext, bridge SDK, rich fragments, split | Accepted | `DIR-018` working direction                                                          |
| Small `bitty-agent` vocabulary (`AgentId`) is already sufficient and is left alone                            | Accepted | `DIR-018` already-sufficient list                                                    |
| Terminal-context read maps to `terminal.inspect` via `terminal.snapshot` plus ledger grant (proven slice row) | Draft    | `RFC-0003` proposed-mapping table                                                    |
| Workspace-context and memory-persistence rows have no covering generic scope (open rows, lean to agent layer) | Draft    | `RFC-0003` open-terms section                                                        |

## Per-item reconciliation (candidate verdicts)

The pressure-test gate is applied literally: for each AI-named item, the
generic primitive is tried first; only what no generic primitive covers
remains AI-specific, and then only as agent-layer vocabulary outside Core
authority, never as new Core API.

| #   | AI-named item (`ai_panel.rs`)                                                                                        | Generic counterpart                                                                                                                    | Verdict                          | Rationale                                                                                                                                                                       |
| --- | -------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | `panel.provider` / `panel.create` constants and gates                                                                | Panel Runtime RFC closed `panel.*` family (**accepted**)                                                                               | **Keep (generic)**               | Values are already generic; ai-panel uses a subset and invents no family. Sibling panels prove the same composition.                                                            |
| 2   | `agent.context.terminal` constant and gate                                                                           | `terminal.inspect` scope via `terminal.snapshot` plus ledgered grant (**draft** proven row); planned bounded read service (`CTX-0420`) | **Demote**                       | Per-terminal observation with a byte budget is a generic read-service concern, not AI authority. Specified change D-1.                                                          |
| 3   | `agent.context.workspace` constant and gate                                                                          | No covering generic scope (**draft** open row); agent-layer per-target grant composition                                               | **Demote**                       | No workspace primitive needs first-class Core protection today; adding a `workspace.*` scope family would bake an AI-shaped family into the wire. Specified change D-2.         |
| 4   | `agent.memory:persist` constant and gate                                                                             | No covering generic scope (**draft** open row); agent-layer opt-in persistence (`0600`, bounded retention, redaction)                  | **Demote**                       | Persistence authority is an agent-layer concern per the `DIR-018` split (memory owned by `bitty-ai`). Specified change D-3.                                                     |
| 5   | `mcp.invoke:` prefix constant and gate                                                                               | Generic per-tool invocation with per-tool consent (planned host dispatch, `CTX-0420`; draft `TB-4`)                                    | **Keep shape, rehome ownership** | The shape is already generic: mail-panel uses `mcp.invoke:mail.*` with no AI involvement. Only the `AI_PANEL_` ownership prefix is AI-named. Specified change D-4.              |
| 6   | `ai.provider` constant and gate                                                                                      | `config.inspect` for registry listing only (**draft** proposed row); provider-local consent plus `network.connect` when remote         | **Demote**                       | Provider registry implementation and I/O belong in the AI helper per draft `BA-2`/`BA-3`; Core validating generic service metadata is the most it may do. Specified change D-5. |
| 7   | `ai.stream` constant and gate                                                                                        | RC-10 `validate_chunk` framing discipline (**draft** proven row: chunking is framing, not authority)                                   | **Demote**                       | No Core stream authority is needed; streamed fragments are a generic transport concern (rich-fragment direction, text-first). Specified change D-5.                             |
| 8   | `ai.model` constant and gate                                                                                         | Provider-local consent; listing as in row 6                                                                                            | **Demote**                       | Model selection is owned by `bitty-ai` per the `DIR-018` split (selection listed there explicitly). Specified change D-5.                                                       |
| 9   | `bitty-terminal.ai-panel:*` command set (5 commands)                                                                 | Generic command registry, qualified `owner.name:command` (**accepted**)                                                                | **Keep (generic)**               | Commands are panel actions under generic registry rules with the `32`-per-type bound; a command is not authority and grants nothing by itself.                                  |
| 10  | Tiled-layout constructors (`tiled_layout`, `vertical_stack`, `tiled_with_browser_snapshot`, `ai_panel_tiled_layout`) | `LayoutNode` `H`/`V` primitives, Core-owned decoration (**accepted**)                                                                  | **Keep (generic)**               | No new tiling primitive is introduced; reuse is asserted by the existing layout tests.                                                                                          |
| 11  | `create_ai_panel` / `validate_ai_panel_config`                                                                       | Public Panel Runtime path (`create_panel` then `mount_panel`, typed errors) (**shipped**)                                              | **Keep path (generic)**          | Creation already goes through the public path with no private channel; its capability list follows verdicts 1-8 as those land.                                                  |
| 12  | `validate_agent_id` / `is_valid_agent_id`                                                                            | Small `bitty-agent` `AgentId` vocabulary (**accepted** as already sufficient)                                                          | **Keep (thin alias)**            | A bounded `owner.name` check duplicates nothing structural; rehoming is cosmetic and deferred to an extraction task.                                                            |
| 13  | `AgentMemoryEntry` type and memory-ring helpers                                                                      | Agent-layer memory owned by `bitty-ai` (**accepted** split); bounded-ring mechanics are generic                                        | **Demote ownership**             | The type travels with memory ownership on extraction; Core keeps no memory authority meanwhile. Specified change D-3.                                                           |
| 14  | `AgentWorkspaceFile` type and workspace helpers                                                                      | Ephemeral per-session workspace, disposed at session close (**draft** alternative rejected persistent tree)                            | **Demote ownership**             | Workspace mechanics travel with the agent layer; any second consumer may generalize the bounded-file shape then, not now. Specified change D-2.                                 |
| 15  | Context-budget helpers (`32 KiB` truncate/account)                                                                   | Draft `CP-5` default (**draft**); `OQ-066` per-model profiles (**open**)                                                               | **Keep value, keep open**        | The `32 KiB` default is reused as-is; this record neither accepts a new default nor closes `OQ-066`.                                                                            |
| 16  | MCP framing helpers (`256 KiB` / `512 KiB` / depth `32`)                                                             | IPC framing and RC-9/RC-10 quotas (**accepted**)                                                                                       | **Keep (mirror)**                | Values mirror accepted transport bounds; the planned generic dispatch owns them when it lands.                                                                                  |
| 17  | `AI_PANEL_` Rust symbol prefix on generic values                                                                     | Generic re-export names (not specified here)                                                                                           | **Cosmetic follow-up**           | The prefix is the only AI-named residue on kept items; renaming is churn with no authority effect and is deferred to F-4.                                                       |

Confidence: rows 1, 9, 10, 11 rest on **accepted** contracts plus
**shipped** composition evidence; rows 2, 5, 6, 7 rest on **draft**
`RFC-0003` rows plus planned sibling services; rows 3, 4, 13, 14 follow
the **accepted** `DIR-018` split applied to **draft** open rows; row 15 is
pinned by the **open** `OQ-066` register entry.

## Specified demotion changes (proposals, not edits)

- **D-1 (terminal context).** A follow-up code task introduces the generic
  bounded context-read grant consumed through the planned read service and
  migrates the ai-panel terminal-observation call sites to it; the
  `agent.context.terminal` Core constant and its gate helper are then
  deprecated through the normal removal policy, not deleted silently.
  Acceptance: observation flows without the `agent.*` string; ledgered
  per-terminal denial tests stay green.
- **D-2 (workspace context).** No new Core scope is added. A follow-up task
  moves workspace assembly behind agent-layer per-target grants composed
  from generic read primitives; if a future Core workspace primitive needs
  first-class protection, that need arrives as its own IPC RFC revision
  with a wire-compatibility plan, never as a silent scope stretch.
- **D-3 (memory persistence).** A follow-up task moves persistence consent
  and the `AgentMemoryEntry` type with memory ownership to the agent layer
  under the existing opt-in rules (`0600`, bounded retention, redaction);
  Core retains no memory authority and no disk-write path.
- **D-4 (`mcp.invoke` rehome).** A follow-up task re-documents the per-tool
  invocation shape as Tool Bus vocabulary owned by the generic dispatch
  workstream and re-exports the prefix helper outside the `ai_panel`
  module; wire strings are unchanged.
- **D-5 (provider/stream/model).** A follow-up task removes provider,
  stream, and model authority from Core: registry implementation, I/O,
  selection, and streaming consent live in `bitty-ai`; Core keeps at most
  generic service-metadata validation. Listing, if ever Core-enforced,
  resolves to the narrowest existing scope rather than a new AI scope.
- **D-6 (no-new-AI-logic guard).** Until D-1 through D-5 land, `ai_panel.rs`
  is frozen against new AI-named surface: any proposal needing a new
  `ai.*` / `agent.*` string must first file the generic-abstraction
  alternative and have it rejected with reasons, per the BA-6 reviewer
  rule.

## Ownership restatement (accepted, unchanged)

- `PanelRuntime` owns panel lifecycle; `Workspace` owns layout and
  decoration; the registry owns terminals and PTY descriptors; no panel
  holds a PTY fd (**accepted**, Panel Runtime RFC).
- Panel capabilities stay closed under `panel.*`; `panel.*` subsumes
  nothing and grants nothing implicitly; official panels pass the
  identical capability model with no first-party bypass (**accepted**).
- The `DIR-018` split is restated verbatim: Bitty owns Panel, Execution,
  IPC, snapshots, capabilities, rich projection, and process/PTY;
  `bitty-ai` owns Provider, Agent, Session, projection, memory,
  orchestration, and selection. This record moves authority toward that
  split and moves no requirement between owners.

## Shipped versus accepted versus draft versus open ledger

| Claim                                                                                           | Status                | Authority                                                               |
| ----------------------------------------------------------------------------------------------- | --------------------- | ----------------------------------------------------------------------- |
| Nine capability constants, gate helpers, bounded helpers, types, commands in `ai_panel.rs`      | Shipped               | `crates/bitty-runtime/src/ai_panel.rs` and its tests                    |
| First-party ai-panel manifest capability set                                                    | Shipped               | `crates/bitty-plugin-host/src/bundled.rs`, `ai_panel_manifest`          |
| Closed capability tables with `:PARAMETER` rules                                                | Shipped               | `capability.rs`, `manifest.rs` closed-set checks and tests              |
| Sibling panels compose generic families only                                                    | Shipped               | file-manager, git, browser, mail panel tests                            |
| Closed `panel.*` family; single-owner placement; command-registry rules                         | Accepted              | Panel Runtime RFC                                                       |
| 13-scope registry with per-request evaluation and consent ledger                                | Accepted              | IPC and Agent RFC (`OQ-018` closed)                                     |
| Parallel delivery order and the no-new-AI-logic ownership split                                 | Accepted              | `DIR-018` working direction                                             |
| G-5 gap rule: suspect the missing generic abstraction first                                     | Draft                 | Pressure-test specification, gap G-5                                    |
| Consent-to-scope mapping rows reused here (proven terminal read; open workspace/memory rows)    | Draft                 | `RFC-0003`                                                              |
| Per-model budget profiles; any change to the `32 KiB` default                                   | Open                  | `OQ-066`                                                                |
| Verdicts 1-17, changes D-1..D-6, follow-ups F-1..F-4 in this record                             | Candidate             | This record (`CTX-0423`); needs acceptance                              |
| Generic read service, host tool dispatch, ExecutionContext, bridge SDK, rich-fragment transport | Planned (other tasks) | `CTX-0419`, `CTX-0420`, `CTX-0421`, `CTX-0422` (ready, not implemented) |

## Follow-ups (implementation tasks, not this task)

- **F-1.** Generic bounded context-read service with ledgered per-target
  grants (owner: read-service workstream alongside `CTX-0420`); D-1
  migrates onto it. Do not file inside `CTX-0423`.
- **F-2.** Agent-layer workspace assembly and memory-persistence consent
  with type extraction (owner: agent-layer workstream under `bitty-ai`
  conventions); D-2 and D-3 migrate onto it.
- **F-3.** Provider registry, selection, I/O, and streaming consent in
  `bitty-ai` with Core authority removal (owner: agent-layer workstream);
  D-5 lands through it. Network use stays behind explicit
  `network.connect` consent per the accepted network boundary.
- **F-4.** Cosmetic `AI_PANEL_` prefix clean-up and `mcp.invoke`
  helper re-export (owner: Core cleanup task after F-1..F-3); wire strings
  unchanged; accept churn only with the owning reviewer.
- Canonicalization of the accepted subset of this record into
  `bitty-terminal-docs` specifications (owning repo
  `bitty-terminal-docs`; do not commit there from this task).
- `OQ-066` linkage stays a read-only citation until the budget-model
  decision lands through its owning amendment.

## Residual risks

- Demotion lands across several tasks and repositories; until F-1..F-3
  complete, the AI-named Core strings remain the enforced authority and
  this record changes nothing at runtime. The risk is staleness of the
  candidate text, mitigated by the per-task acceptance clauses above.
- `agent.context.workspace` has no generic home today; agent-layer
  composition (candidate (b)) must still satisfy the cross-window consent
  rule, which is an unresolved question carried by `RFC-0003`, not decided
  here.
- Registry listing under `config.inspect` is a proposed row awaiting owner
  confirmation; if rejected, the listing gate returns here as a defect,
  not as a silent deviation.
- `CTX-0420` scope discipline: this record must not grow dispatcher
  behavior. Any read-service or dispatch detail discovered while drafting
  follow-ups belongs to that workstream.

## References

- Pressure-test specification, gap table G-1..G-6 (draft):
  `https://github.com/bitty-terminal/bitty-ai-docs/blob/main/specifications/ai-vertical-slice-pressure-test.md`
- AI architecture, BA-6 pressure-test gate (draft):
  `https://github.com/bitty-terminal/bitty-ai-docs/blob/main/specifications/ai-architecture.md`
- IPC and Agent RFC (accepted, `OQ-018` closed):
  `https://github.com/bitty-terminal/bitty-ai-docs/blob/main/specifications/ipc-agent-rfc.md`
- AI Consent to Generic Scope Mapping RFC (draft, G-6):
  `https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/rfcs/RFC-0003-ai-consent-scope-mapping.md`
- Decision register, `DIR-018` parallel-delivery direction (accepted):
  `https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/index.md`
- Open-question register, `OQ-066` context-budget row (open):
  `https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/open-questions.md`
- Panel Runtime RFC (accepted):
  `https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/panel-runtime-rfc.md`
