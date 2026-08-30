# Swarm contract (E22 / E23)

Machine-readable bridge between **ai-reviewer** (merge gate) and a PR-swarm
orchestrator (ZAIrgRush and similar). Same principle as ZAIrgRush
`strong=verdict`: two independent judges — swarm must not self-certify merge.

## Where it appears

Every JSON export (`REVIEW_EXPORT=1`) includes a `swarm` object. A sidecar
`<toolName>-swarm.json` is written next to the main artifact for consumers that
only need the contract.

Built by [`lib/swarm.mjs`](../lib/swarm.mjs) and attached in
[`buildReviewJson`](../lib/export.mjs).

## Shape (`version: 1`)

| Field | Meaning |
|-------|---------|
| `role` | Always `external-verdict` |
| `wouldBlock` | Same as gate exit intent for this run |
| `strong` | `true` only if `wouldBlock` **and** every blocker is independently confirmed (`prescan` or `verifierStatus === 'verified'`) |
| `blockers` | Slim blocker list (file/line/severity/source/prescan/verifierStatus/blastCaller) |
| `blastScope` | Graphify metrics + `affected_files` |
| `suggestedScopeFiles` | **E23**: paths from `affected_files` that are **not** in the PR diff — expand swarm task scope |
| `verdict` | Sticky reproducibility blob when present |
| `notes` | Human-readable hints for logs |

Soft blast majors without verifier never set `strong` (they also do not set
`wouldBlock` under the soft-gate). Advisory-only noise stays out of the
external verdict.

## Orchestrator usage

1. Read `<tool>-swarm.json` (or `swarm` from the main JSON).
2. If `strong === true` → treat as second-judge **block** (do not merge / do not
   mark swarm verdict as pass).
3. If `wouldBlock && !strong` → advisory for swarm; do not equate with a hard
   external block.
4. Union `suggestedScopeFiles` into task scope beyond the PR diff.

Public helpers: `buildSwarmVerdict`, `isStrongBlocker`, `suggestedScopeFiles`,
`SWARM_CONTRACT_VERSION` from the package root export.
