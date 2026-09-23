---
title: OQ owner decision packet
description: Batch decision packet for owner-blocked open questions (CTX-0704)
category: specifications
audience: maintainer
document_type: design-record
status: candidate
---

<!-- markdownlint-disable MD025 -->

# OQ owner decision packet (CTX-0704)

> Status: **candidate decision packet** (CTX-0704). It decides nothing: each
> row needs the owner's adopt/refuse/defer ruling first.
>
> Task: `CTX-0704` | Register: `OQ` in `bitty-docs`
> (`docs/decisions/open-questions.md`)
> Source: owner `Xuepoo` batch-decision request covering ~35 blocked `bitty`
> issues. `OQ-029` is already closed (accepted 2026-08-28) and needs no
> ruling; it is noted once and excluded below.

## Claim-status legend

- **Shipped** — implemented and tested in the `bitty` repository.
- **Accepted** — decided in an accepted contract.
- **Candidate** — recorded direction with no contract force.
- **Open** — follow-up work with no contract.

## Decision table

| OQ | Question (one line) | Affected `bitty` issues | Candidate direction | Recommendation | What closes if picked |
| --- | --- | --- | --- | --- | --- |
| OQ-050 | Stable scrollback-line identity for semantic command-block anchors; where per-view fold state lives and persists. | #984 | Ordinal-anchored zones (`ordinal`/`kind`/`exit_code`); row anchoring future work. | Defer stable identity, keep ordinal anchors: unblocks M1 without render evidence. | #984 moves to implementation; OQ-S1/S2 stay scoped. |
| OQ-051 | Non-terminal panels via compositor sub-surface/Scene path vs per-leaf grid snapshot; ownership vs Rich Presentation and Panel Runtime. | #985, #990 | Bounded `SceneNode` model, unconsumed; typed `ViewContent::Panel` exists. | Defer: no compositor contract until Panel Runtime is accepted. | #985, #990 get a placed contract to implement. |
| OQ-052 | Which native window forms enter scope (niri ribbon, panel rules, semantic workspaces, unified `Mod`); which owner/`LayoutProvider` implements them. | #1016, #1018, #1043, #1044, #1045, #1156, #1157, #1158 | Dwindle splits and floating overlays shipped; ribbon/rules/workspaces/`Mod` have no contract. | Defer as a bundle, decide piecemeal post-M1; keep shipped scope frozen. | Each listed issue becomes a separately schedulable scope. |
| OQ-054 | `api_key_env` vs `api_key_cmd` semantics, resolution order, and the project-override boundary that cannot widen credentials. | #1092 | AI Architecture MPC-1..MPC-4; composes with ADR 0006 and MP-10. | Adopt MPC-1..MPC-4: explicit order with project-never-widens rule. | #1092 moves to schema implementation. |
| OQ-055 | Secret-storage tiers in scope; consent, audit, and redaction per tier. | #1091 | ADR 0006 fixes `os.getenv` denial and allowlisted `bitty.env.get`; tiers are candidate-only. | Adopt tier list with per-tier consent/audit/redaction. | #1091 moves to implementation on top of ADR 0006. |
| OQ-056 | Capability dimensions beyond v1 (semantic UI slots, presentation projection, workspace policies, automation actions, service multiplicity); in which API version. | #1000, #1017 | Candidate dimensions in Plugin Roadmap; v1 surface stays authoritative. | Defer to API v2: freeze v1, record dimensions as v2 scope. | #1000, #1017 proceed against the frozen v1 surface. |
| OQ-057 | Capability-enforced role contract for multi-agent work (role-authority map, prompt binding, dispatch limits); enforcement points. | #1094 | Commander/Implementer/Tester/Reviewer table; must also cover execution-sandbox layer (CRE-5). | Adopt role table plus sandbox-layer coverage: tool capability alone leaves shell writes open. | #1094 moves to enforcement-point implementation. |
| OQ-058 | Spatial multi-agent orchestration contract (role panels, IPC kinds/routing, panel/session lifecycle coupling), presentation non-authoritative. | #995, #1001, #1014, #1052 | SMO-1..SMO-4 topology; delivery semantics (envelope, deadline, priority, dedup, cancel, expiry) still candidate. | Adopt SMO-1..SMO-4 including delivery semantics: kinds without delivery guarantees cannot route. | #1001 (and cross-refs #995, #1014, #1052) get a routable contract. |
| OQ-068 | `.wheel/` project-definition directory contract (layout, schema, tracked-vs-state split, trust); `.agents/` compatibility without a competing source of truth. | #1096 | Declarative-data-only Git-tracked `.wheel/` (`project.toml`, `agents/`, `workflows/`, `prompts/`, `policies/`, `tools/`, `skills/`); discovery `.wheel/` then `.agents/`; Wheel rename confirmed. | Adopt: name is settled, schema is the only open part, and deferral blocks project trust. | #1096 moves to schema implementation. |
| OQ-083 | Panel lease/description/handoff contract for an embodied multi-agent workspace; composition with Stable Id hierarchy and event bus. | #973, #1052, #1095 | Company/floor/workstation mapping; RUN-21 evidence kernel (`Idle`/`Occupied`, acquire/release/handoff) exists. | Adopt lease kernel direction: evidence already constrains the shape. | #1052, #1095 move to contract acceptance; #973 epic unblocks its lease track. |
| OQ-084 | Core ontology and identity model (Instance, Workspace, Panel, Surface, ExecutionContext, Terminal, Session, Resource, Service, Agent); `PanelId`/`WorkspaceId`/`ResourceId`/`ExecutionContextId`/`AgentId`/`GenerationId` relations. | #1093 | Candidate from 2026-09-13 review; Panel/Execution and Restore/Persistence separations are the immediate refinements. | Adopt ontology with the two separations: identity confusion is the root blocker for OQ-058/061/083. | #1093 closes into an ADR; dependent OQs gain stable terms. |
| OQ-085 | Trust levels for plugin/helper/tool boundaries (L0 Core through L4 external/MCP); capability domains allowed per level. | #1090 | Five-level candidate; current boundaries and P0 gates stay authoritative. | Adopt levels plus per-level domain matrix: P0 gates need a level to attach to. | #1090 moves to security-corpus revision. |
| OQ-086 | Sensitive-input prompt detection signal (termios no-echo vs heuristics); interaction classes; agent-input interlock, consent, snapshot exclusion. | #973, #1053 | Termios-based detection, three classes, fail-closed no-echo interlock, no-capture rule; denial shape and evidence undecided. | Adopt detection plus fail-closed interlock: fail-open input capture is the worst default. | #1053 moves to IPC/Agent amendment; #973 epic unblocks its input track. |
| OQ-087 | Command-risk classification and syntax-level audit contract for agent commands; composition with Tool Bus validation. | #973, #1054 | CRA-1..CRA-5 under Tool Bus; no tier engine, deny policy, or consent wiring; composes with PP-3 ledger and OQ-086. | Adopt tiers plus hard-deny classes: classification without deny classes is advisory only. | #1054 moves to tier-engine implementation; #973 epic unblocks its command track. |
| OQ-088 | Cross-platform Leader default/fallback (`Alt+Space` vs Windows-reserved), modal timeout/cancel semantics, user override surface. | #981, #1045 | `Alt+Space` default, Windows fallback set, fail-open timeout, config surface. | Adopt default plus fallback set and override: ships the key path while Windows stays usable. | #981, #1045 move to binding implementation. |
| OQ-089 | Bitty Beacon spatial action engine contract (targets, actions, labels, handedness, script authority, config); generalization of Hint Mode. | #981 | P7 engine (spatial focus, fold toggle, focus routing, script dispatch); labels, authority, config undecided; depends on OQ-050 anchors. | Adopt engine direction after OQ-050: labels without anchors have nothing to point at. | #981 moves to engine contract once anchors land. |
| RFC-OQ-3 | Panel becomes typed `View` content (A), replaces `View` as leaf, or side-car composes; `ViewId` vs `PanelId` migration. | #995, #997 | Option A in Panel Placement Decision; implementation already shaped that way. | Adopt Option A: implementation, history preservation, and evidence all point there. | #997 closes; #995 gains its placement premise. |
| RFC-OQ-8 | `Browser` panels need an extra per-window process budget beyond the RC-3 aggregate. | #1006, #1016 | No budget decision; PB-2 idle-RSS pressure (#1190) argues against a blank check. | Defer pending browser-view measurement: a budget without numbers is a blank check. | #1006 stays a gated decision with a measurement entry condition. |
| RFC-OQ-9 | Seven presentation modes, `preferred_mode` plus Panel Rules precedence, scrolling layout, and workspace save/restore enter a Panel RFC together or as separate follow-ups. | #1013, #1017, #1147 | Bundled-vs-split undecided. | Defer the bundle, accept separate follow-ups: one ruling cannot cover four mechanisms. | Each listed issue becomes an independently acceptable follow-up. |

## Per-OQ briefs

### OQ-050 — scrollback anchor identity

- Candidate: ordinal-anchored semantic zones; `CommandBlock`/`FoldState` implemented-only and unwired; row anchoring future work.
- Recommendation: defer stable identity, keep ordinal anchors for M1.
- If picked: #984 converts to implementation work; OQ-S1/S2 keep their scope.

### OQ-051 — non-terminal panel content path

- Candidate: bounded `SceneNode` model exists but no render-pipeline consumer; typed `ViewContent::Panel` exists.
- Recommendation: defer until the Panel Runtime acceptance fixes ownership.
- If picked: #985 and #990 implement against a placed contract.

### OQ-052 — native window-form scope

- Candidate: adaptive splits and floating overlays shipped; ribbon, panel rules, semantic workspaces, unified `Mod` have no contract.
- Recommendation: defer the bundle; rule on each form separately post-M1.
- If picked: #1016, #1018, #1043, #1044, #1045, #1156, #1157, #1158 schedule independently.

### OQ-054 — credential reference semantics

- Candidate: MPC-1..MPC-4 resolution order with a project-cannot-widen boundary, composing with ADR 0006 and MP-10.
- Recommendation: adopt; the boundary rule is the security load-bearing part.
- If picked: #1092 converts to provider-schema implementation.

### OQ-055 — secret-storage tiers

- Candidate: tiers (host env, `0600` secrets file, OS keyring, command references) with per-tier consent/audit/redaction, unimplemented.
- Recommendation: adopt; ADR 0006 already proves the denial/audit pattern.
- If picked: #1091 converts to tier implementation.

### OQ-056 — post-v1 capability dimensions

- Candidate: UI slots, presentation projection, workspace policies, automation actions, service multiplicity recorded as v2 scope.
- Recommendation: defer to API v2 and freeze v1.
- If picked: #1000 and #1017 build on the frozen v1 surface.

### OQ-057 — multi-agent role contract

- Candidate: Commander/Implementer/Tester/Reviewer table; prompts never grant authority; sandbox layer must be covered (CRE-5).
- Recommendation: adopt table plus sandbox coverage.
- If picked: #1094 converts to enforcement-point implementation.

### OQ-058 — spatial orchestration contract

- Candidate: SMO-1..SMO-4 topology; envelope identity, attribution, deadline, priority, dedup, cancellation, expiry still candidate.
- Recommendation: adopt rules plus delivery semantics.
- If picked: #1001 (with #995, #1014, #1052 cross-refs) gains a routable contract.

### OQ-068 — `.wheel/` project contract

- Candidate: declarative data-only tracked tree, stated discovery order, Wheel rename settled, schema open, nothing implemented.
- Recommendation: adopt; schema drafting is cheaper than continued project-trust drift.
- If picked: #1096 converts to schema implementation.

### OQ-083 — panel lease and handoff

- Candidate: workstation mapping refining OQ-058; RUN-21 transition kernel evidence exists; presentation stays non-authoritative.
- Recommendation: adopt the kernel direction.
- If picked: #1052 and #1095 convert to contract acceptance; #973 unblocks its lease track.

### OQ-084 — core ontology and identity

- Candidate: ten-concept ontology with ID relations; Panel/Execution and Restore/Persistence separations are the immediate refinements.
- Recommendation: adopt with both separations; this is the root blocker for the identity family.
- If picked: #1093 closes into an ADR and OQ-058/061/083 reuse its terms.

### OQ-085 — trust levels and capability domains

- Candidate: L0–L4 model with per-level domain matrix; current boundaries and P0 gates stay authoritative meanwhile.
- Recommendation: adopt; P0 gates need levels to attach to.
- If picked: #1090 converts to a security-corpus revision.

### OQ-086 — sensitive-input signal

- Candidate: termios-based no-echo detection, three interaction classes, fail-closed interlock, no-capture rule.
- Recommendation: adopt; fail-open capture is the unacceptable default.
- If picked: #1053 converts to an IPC/Agent amendment; #973 unblocks its input track.

### OQ-087 — command-risk classification

- Candidate: CRA-1..CRA-5 under Tool Bus composing with the PP-3 ledger and OQ-086; engine, deny policy, and wiring missing.
- Recommendation: adopt tiers plus hard-deny classes.
- If picked: #1054 converts to tier-engine work; #973 unblocks its command track.

### OQ-088 — Leader key contract

- Candidate: `Alt+Space` default with Windows fallback set, fail-open timeout, and config override surface.
- Recommendation: adopt; it ships the path without stranding Windows.
- If picked: #981 and #1045 convert to binding implementation.

### OQ-089 — Beacon spatial action engine

- Candidate: P7 engine generalizing Hint Mode; labels, authority, and config undecided; anchored on OQ-050.
- Recommendation: adopt the direction only after OQ-050 resolves ordering.
- If picked: #981 converts to an engine contract once anchors exist.

### RFC-OQ-3 — panel placement

- Candidate: Option A (typed `View` content) in the Panel Placement Decision; current implementation already matches it.
- Recommendation: adopt Option A.
- If picked: #997 closes and #995 builds on the placement premise.

### RFC-OQ-8 — browser-view process budget

- Candidate: undecided; PB-2 idle-RSS pressure weighs against an extra budget.
- Recommendation: defer pending measurement.
- If picked: #1006 remains gated with a measurement entry condition.

### RFC-OQ-9 — presentation-mode packaging

- Candidate: modes, precedence, scrolling layout, and save/restore bundled or split, undecided.
- Recommendation: defer the bundle; accept separate follow-ups.
- If picked: #1013, #1017, #1147 proceed as independent acceptances.

## Note on OQ-029

`OQ-029` (key directory, enrollment, rotation, revocation, freshness for
signed releases) is already accepted via the Package Follow-up RFC
(closed 2026-08-28). No owner ruling is needed; no `bitty` issue references
it.
