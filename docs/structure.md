# Structure, contracts and test scenarios

`pr-review-structure` is an off-PR analysis path. It does not change the merge gate. It produces evidence-backed facts and assessments that cod-doc, agents and CI can consume.

## Artifacts

The protocol has four versioned schemas in `schemas/`:

- `structure-facts.v1`: boundaries, entities, contracts, dependencies, test inventory and views;
- `structure-assessment.v1`: coverage observations, scenario statuses, risk assessments and hints;
- `obligations-export.v1`: documented MUST/SHOULD/MAY obligations supplied by cod-doc;
- `evidence-receipt.v1`: immutable provenance for every analysis provider.

Facts and assessments have separate fingerprints. Changing documentation or test evidence does not duplicate code facts.

## Providers

The producer combines independent capabilities:

- Graphify: topology, communities and call/import candidates;
- SCIP: exact TypeScript/JavaScript symbol kinds, ranges, references and signatures when present;
- LCOV: exact file line counts and aggregate function execution;
- source/ast-grep-style test inventory: test and assertion candidates;
- Mutation Testing Elements/Stryker reports: killed, survived and uncovered mutants;
- property reports: seeds, run counts, shrinking and counterexamples.

Evidence precedence is SCIP/compiler exact → Graphify EXTRACTED → Graphify INFERRED → AMBIGUOUS. A SCIP reference is not treated as a runtime call.

Providers emit receipts, never a final `covered` decision. The deterministic assessment engine computes statuses.

## Scenario statuses

- `covered`: a confirmed obligation has a unique contract, a linked test, execution evidence and scenario-specific evidence.
- `partial`: some evidence exists, but the scenario threshold is not met.
- `missing`: a fresh, complete inventory has a unique contract but no test evidence.
- `unverifiable`: stale, truncated, ambiguous or temporally mismatched input.

Aggregate LCOV cannot prove an error, boundary or invariant scenario. File coverage is never substituted for function or scenario coverage.

## CLI

```bash
npx pr-review-structure \
  --graph .graphify/graphify-out/graph.json \
  --graph-report .graphify/graphify-out/GRAPH_REPORT.md \
  --scip index.scip \
  --lcov coverage/lcov.info \
  --obligations .cod-doc/obligations.json \
  --facts-out .ai-review/structure/structure-facts.json \
  --assessment-out .ai-review/structure/structure-assessment.json \
  --drift-out .ai-review/structure/structure-drift.json \
  --property-drafts-out .ai-review/structure/property-drafts.json \
  --markdown .ai-review/structure/code-structure.md
```

At least one of Graphify, SCIP or LCOV must be readable. Missing optional providers produce warnings and lower the evidence ceiling.

Exit codes:

- `0`: artifacts written, including degraded partial analysis;
- `1`: no usable source, invalid input or write failure;
- `2`: invalid CLI usage.

## SCIP pilot

Generate a semantic index in the consumer repository:

```bash
npx @sourcegraph/scip-typescript index
npx pr-review-structure --scip index.scip --graph .graphify/graphify-out/graph.json
```

Graphify remains the fallback. The pilot should compare unresolved/ambiguous symbol joins before and after SCIP before making the indexer mandatory.

## Mutation and property evidence

Mutation reports are scoped to confirmed MUST contracts, changed contracts, god-nodes and high fan-in partial scenarios. Aggregate mutation score is inventory only; a scenario hint requires a contract-bound surviving or uncovered mutant.

Confirmed error, boundary and invariant obligations generate draft properties. Drafts must be reviewed before execution. A property report is accepted only when its semantic obligation hash still matches.

## Pinned MCP context

Start the stdio MCP server:

```bash
npx pr-review-structure-mcp
```

The server exposes `structure_context`. Calls must include:

- `factsPath` and `assessmentPath`;
- either `headSha` or `snapshotFingerprint`;
- scope/entity/contract refs and a token budget.

There is no implicit “latest” for planning or fixing. The response is a bounded BFS slice with fingerprints, temporal alignment, relevant contracts, dependencies, scenarios, hints and suggested files.

Example MCP configuration:

```json
{
  "mcpServers": {
    "ai-reviewer-structure": {
      "command": "npx",
      "args": ["pr-review-structure-mcp"]
    }
  }
}
```

## cod-doc integration

cod-doc remains the source of truth for intent:

1. export confirmed obligations;
2. run the structure producer at a pinned commit;
3. ingest facts and assessment snapshots;
4. compute structure drift separately from DB↔Markdown projection drift;
5. expose a task-scoped structure context;
6. promote evidence-backed gaps into findings/tasks.

Generated Markdown is a convenience projection with `source_of_truth: false`. It must not overwrite specification documents automatically.

## Public API

The package root exports parsers, providers, assessment helpers, drift/link/waiver helpers, export functions and the MCP handler. Consumers can embed these without invoking the CLI.
