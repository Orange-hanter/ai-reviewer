---
type: architecture
status: draft
owner: team
source_of_truth: false
last_updated: 2026-09-03
head_sha: abc123
facts_fingerprint: ff01
assessment_fingerprint: aa02
generated_by: ai-reviewer
---

# Code structure, contracts and test scenarios

## Data quality

- Head: `abc123` on `main`
- Graph: **ok**
- Temporal alignment: **aligned**
- Entities: **3**, dependencies: **3**
- Function joins unresolved: **12.3%**
- Truncated: **yes**

## Coverage

- Lines: **3/4**
- Functions: **1/2**
- Branches: **0/1**

## Priority hints

### MEDIUM — cov.gap

Add test  now \\ ok

Subjects: `src/a.ts`, ``` x``y ```
Evidence: `ev:1`
Missing: mut\|ation, lcov

## Contract scenarios

| Obligation | Kind | Status | Contract | Reason |
|---|---|---|---|---|
| obl:1 | happy_path | covered | c:1, , c:2 | ok \`x\` |

## Dependency hotspots

| Entity | Fan-in | Execution | Cross-boundary callers |
|---|---:|---|---:|
| validate2() | 5 | executed | 2 |
| e2 | 3 | unavailable | 0 |
| zz | 1.5 |  | 1 |

## Boundaries

- **module auth\|core** — `src/auth`, `` src/`x` ``
- **layer ui** — 

## Notes

This document is a generated projection. JSON facts and assessment artifacts are the machine-readable source for this observation; cod-doc specifications remain the source of truth for intent.

