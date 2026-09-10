---
type: architecture
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-09-10
last_reviewed: 2026-09-10
---

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

## Limits and honesty of the output

Entities, dependencies and contracts are capped at the `maxItems` values declared in
`schemas/structure-facts.v1.schema.json`. `REVIEW_STRUCTURE_MAX_ENTITIES` and
`REVIEW_STRUCTURE_MAX_EDGES` can lower those caps but not raise them: the producer must not be
able to emit facts its own schema rejects. When a cap bites, the run reports it in `warnings`
and sets `provenance.truncated`. Contracts are cut weakest-evidence-first, so heuristic
`entrypoint` candidates go before proven `implements`/`inherits` contracts.

Anything dropped leaves a trace. Nodes skipped for an unsafe `source_file`, unparsed
`source_location` values and god-node labels that resolve to more than one entity are counted
in `warnings` rather than discarded silently. The generated `code-structure.md` states how many
hints, scenarios and hotspots it omitted — the JSON assessment always holds the full set.

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

For library consumers: `analyzeStructure({ scipIndex })` attests a re-serialized
in-memory index under `memory:index.scip`; callers holding the serialized index
should pass `scipBuffer` so the receipt hash matches the exact bytes.

## Mutation and property evidence

Mutation reports are scoped to confirmed MUST contracts, changed contracts, god-nodes and high fan-in partial scenarios. Aggregate mutation score is inventory only; a scenario hint requires a contract-bound surviving or uncovered mutant.

Confirmed error, boundary and invariant obligations generate draft properties. Drafts must be reviewed before execution. A property report is accepted only when its semantic obligation hash still matches.

## Pinned MCP context

Start the stdio MCP server:

```bash
npx pr-review-structure-mcp
```

Полный прогон ревью из чата — другой сервер, `pr-review-mcp` ([RFC 08](../proposals/08-review-mcp.md), [RFC 09](../proposals/09-cursor-plugin-mcp.md)): другой корень (`REVIEW_MCP_ROOT`), другой TTL. Этот процесс отдаёт только `structure_context`.

The server exposes `structure_context`. Calls must include:

- `factsPath` and `assessmentPath`;
- either `headSha` or `snapshotFingerprint`;
- scope/entity/contract refs and a token budget.

Where `snapshotFingerprint` comes from: it is the `fingerprint` field of the
`structure_facts` payload written by a prior `pr-review-structure` run (default
`<exportDir>/structure-facts.json`). The client reads the exported facts artifact,
takes its `fingerprint`, and passes it together with the same file as `factsPath` —
this pins the context to that exact snapshot instead of an implicit "latest".
There is no circularity: the fingerprint is produced by the export run, consumed
by later MCP calls.

There is no implicit “latest” for planning or fixing. The response is a bounded BFS slice with fingerprints, temporal alignment, relevant contracts, dependencies, scenarios, hints and suggested files.

### Confinement: what the server is allowed to read

`factsPath` and `assessmentPath` arrive **from the client**, so the server treats them as
untrusted input and never reads them as given. Both are resolved through
`resolveContainedPath` against a single allowed root:

| | |
|---|---|
| Root | `STRUCTURE_MCP_ROOT`, or the process working directory when it is unset |
| Escape attempt | throws `path escapes allowed root: <path>` — the call fails, nothing is read |
| Symlinks | resolved with `realpath` **before** the containment check, so a link pointing outside the root does not get through |
| Missing file | containment is still checked against the nearest existing ancestor; only if that ancestor is inside the root does the original `ENOENT` propagate — an absent path outside the root reports the escape, not the absence |

Two consequences worth stating plainly:

- **Set `STRUCTURE_MCP_ROOT` explicitly** when the server is not launched from the
  repository root. The fallback is the working directory, so an agent started elsewhere
  silently gets a different — possibly wider — root than intended.
- The root bounds **reading of the artifacts**, not the content of the answer. Facts and
  assessment written outside the root are simply unreachable; put the export directory
  inside the root you allow.

Example MCP configuration:

```json
{
  "mcpServers": {
    "ai-reviewer-structure": {
      "command": "npx",
      "args": ["pr-review-structure-mcp"],
      "env": { "STRUCTURE_MCP_ROOT": "/abs/path/to/repo" }
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
