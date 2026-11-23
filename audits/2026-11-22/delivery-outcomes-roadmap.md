# Delivery outcomes implementation roadmap

November 23, 2026. Release objective: a person opening an ordinary conversation, Mission, CLI result or published report can identify the requested deliverable, its current verdict, the blocking criterion and the evidence behind it. The [source audit](delivery-outcomes.md) records current behavior. Ownership below identifies code areas; individual assignees and dates are open.

| Milestone and owner | Starts after | Deliverable | Definition of done |
|---|---|---|---|
| A. Preserve failed command history — Mission surfaces | Independent | Keep the most recent failed run on the collapsed card and acceptance inbox alongside the latest run. | Existing model regression passes; visually verify both commands on the card and inbox and open the failed run from the card. Local implementation and 10 passing model tests recorded; live UI check pending. |
| B. Define the evidence contract — Runner work and artifact schema | Now | Stable criterion ID, required flag, contract version/digest, candidate identity, attempt ID, evidence source, observation outcome and command reference. Specify separate fields for process exit, assertion result, prerequisite failure and human decision. | Versioned fixtures cover a conversation without a Mission, an approved Mission contract, a candidate change and a legacy text-only spec. Round-trip preserves identities. Legacy criteria show unverified until explicitly bound. |
| C. Capture and replay observations — Runner commands, reviews and persistence | B | Record a declared check against its criterion. Reuse managed command IDs and review executions. Store observations for ordinary conversations and link Mission attempts when present. | Restart after command completion but before acknowledgement; replay yields one observation. Concurrent attempts stay distinct. Missing exit, piped input and absent assertion evidence cannot satisfy a criterion. Capture failure remains visible and retryable. |
| D. Derive delivery outcomes — Runner work model | B; can develop alongside C using B fixtures | One deterministic reducer for required criteria, blocker reasons, evidence freshness, resolved failures and human acceptance. | Generator exit 1 plus SQL exit 0 yields blocked, SQL passed and API checks unrun. A successful rerun of the generator clears only that blocker. Candidate changes invalidate affected proof. Passing all required checks on the current candidate yields success. Acceptance or waiver preserves the recorded technical outcome. |
| E. Expose the projection — Runner transport and CLI | B for schema; C and D for working endpoint | Serve the same projected outcome by conversation and by Mission task; expose machine-readable output and authorized evidence links. | The same attempt has equivalent criterion IDs, states and evidence references through both lookups after restart. An unauthorized caller cannot retrieve private command output. |
| F1. Present completion — conversation and Mission clients | E contract for development; E endpoint for acceptance | Show delivery verdict, unresolved criteria and resolved failure history in the conversation completion summary, Mission card and acceptance inbox. | With no Mission, the conversation still shows the blocker. With a Mission, both surfaces agree. Selecting a command opens its evidence. Human acceptance leaves the technical gap visible. |
| F2. Publish the outcome — artifact store and reader | Same as F1; parallel with F1 | Export an immutable versioned verdict snapshot with candidate, criterion states and scoped evidence references. | Publish while blocked, repair, publish again. The original version still shows blocked; the later version shows the new result. Expired output has an explicit unavailable state. |
| G. Release qualification — scenarios and desktop | C–F complete; fixture preparation starts now | Run the catalog failure, partial repair and final success sequence with and without a Mission. | CLI, conversation, Mission and artifact agree for each candidate before and after Runner restart. Retain test output, candidate revisions and UI evidence. |

## Decision rules to settle in B

A failed required assertion produces a failed verdict. A failed prerequisite that prevented execution produces blocked. Missing or stale required proof produces incomplete. When these coexist, summarize failed first, then blocked, then incomplete, and list every affected criterion. Zero declared criteria yields unverified. An unrelated pass never resolves an earlier failure. These are proposed rules for the versioned fixtures.

## Sequence and release boundary

Finish A's visual check while starting B and the scenario fixture. After B, C and D can proceed independently. E joins those two paths. F1 and F2 can then ship together or independently against the same endpoint. G is the full release gate. The Mind catalog and hosted Hub rollout have their own roadmap; the recorded catalog sequence is sufficient as a Lazybox test fixture.

```mermaid
flowchart LR
  A["A. Failed run on card<br/>merged, live UI check open"]
  B["B. Evidence contract<br/>spec.v2 merged, observation fields open"]
  C["C. Capture and replay<br/>open"]
  D["D. Derive outcomes<br/>Assess merged, freshness and acceptance open"]
  E["E. Outcome endpoint<br/>open"]
  F1["F1. Conversation and Mission verdict"]
  F2["F2. Published verdict snapshot"]
  G{"G. Release qualification<br/>CLI, conversation, Mission and artifact agree<br/>before and after Runner restart"}
  B --> C --> E
  B --> D --> E
  E --> F1 --> G
  E --> F2 --> G
  A --> G
  G -- "any surface disagrees" --> E
```

## November 23 checkpoint: merged

[#76](https://github.com/AntoineToussaint/lazybox/pull/76) landed A's card change and the first slices of B and D.

| Milestone | Merged | Still open |
|---|---|---|
| A | The collapsed card and inbox keep the newest unresolved failed run beside the latest run. A failure clears once a later run of the same command (matched by title) passes. | Live visual check on the card and inbox. A stable command identity instead of the title. |
| B | `spec.v2` with stable criterion IDs and required flags ([`delivery/contract.go`](../../services/runner/code/internal/delivery/contract.go)). `lazybox artifact template delivery-spec` starts one. Publishing recognizes the schema, and each version records the schema it declares. The artifact reader lists criteria by ID and level. Pinned Mission plans keep the bindings and name them in the approval sheet and the worker and reviewer briefs. | Candidate identity, attempt ID, evidence source and separate process-exit, assertion, prerequisite and human-decision fields. Versioned fixtures for the four cases in B. |
| D | [`delivery.Assess`](../../services/runner/code/internal/delivery/outcome.go) folds observations for one contract digest, candidate and attempt. Failed outranks blocked, which outranks incomplete. No required criteria yields unverified. Observations without a source or reference, an unknown result or an empty scope are ignored. Tests cover the catalog sequence and each guard. | Evidence freshness after candidate changes. Human acceptance and waiver that preserve the technical outcome. |
| C, E, F1, F2, G | Nothing yet. | Live command attribution, durable observation capture, the outcome endpoint and the user-facing delivery verdict. |

> **Decision** Mission cards match a resolved failure by run title until C supplies a criterion-bound observation. Two different commands with the same title would clear each other.

> **Note** Verified with `make check` and CI on #76. The new card behaviour has not been looked at in the running app.
