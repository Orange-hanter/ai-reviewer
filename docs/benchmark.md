# Benchmark methodology

Offline precision/recall reporting for the ai-reviewer gold set (`gold/`), aligned with Entelligence-style evaluation.

## Dataset

- **Frozen diffs** — each case is a committed `diff.patch` plus `meta.json` with human labels.
- **Types** — `tp` (true positive / must block or must find), `fp` (must not block / must not find noise).
- **No live repo** — scoring uses findings JSON from a prior run or `--findings` input; LLM is optional.

## Metrics

| Metric | Definition |
|--------|------------|
| **wouldBlock** | Gate exit code / `wouldBlock` flag matches `meta.expect.wouldBlock`. Primary merge-gate metric. |
| **Title recall** | For each `mustFindTitles` substring, at least one finding title contains it (case-insensitive). Semantic proxy — not exact string match. |
| **Title precision** | No finding title contains any `mustNotFindTitles` substring. |
| **Severity ranking** | Reported separately; benchmark does not rank severity tiers — only block vs non-block and title presence. |

## Scoring API

```js
import { scoreGoldCase, aggregateBenchmark } from 'ai-reviewer/lib/benchmark.mjs';

const score = scoreGoldCase(meta, { wouldBlock: true, findings: [...] });
const summary = aggregateBenchmark([{ id, type, expect, score, wouldBlock }]);
```

## CLI

```bash
# From prior gold run JSONL (each line: { id, meta, findings, wouldBlock, code })
npx pr-review-benchmark results.jsonl

# Score findings file against one meta
npx pr-review-benchmark --meta gold/tp-real-bugs-unescape/meta.json --findings run.json

# List gold cases (meta-only stub)
npx pr-review-benchmark --gold-dir
```

Full pipeline shadow eval with LLM remains `pr-review-gold`; `pr-review-benchmark` wraps the same scoring functions without calling models.
