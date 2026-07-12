# Eval fixtures (R1)

Golden PR-кейсы для регрессии post-processing + gate **без LLM**.

**Спека:** [docs/architecture.md](../docs/architecture.md) · **FP-журнал:** [docs/ai-review-false-positives.md](../docs/ai-review-false-positives.md)

## Структура кейса

| Файл | Назначение |
|------|------------|
| `meta.json` | `id`, `description`, `expected`, опц. `gate` override |
| `diff.patch` | unified diff (для `changedPaths` и FP-filter) |
| `findings.json` | `{ "findings": [...] }` — mock-вывод «модели» |
| `verifier.json` | опц. `{ "verdicts": [...] }` — mock Pass B (R3) |

**15 seed-кейсов** (gate, consensus, verifier, refutation, prescan eslint/ast-grep, fp-e2e-in-diff, fp-pr-254/257).

## Прогон

```bash
npm run eval
npm test
npx pr-review-eval --json
```

## Добавить кейс

1. Скопировать каталог по образцу `fp-e2e-in-diff/`.
2. `expected.shouldBlock` + `blockerCount` — ожидание gate.
3. `npm run eval` — green перед PR.

Из R8 feedback: harvest → `feedback/candidates/` → promote → fixture. См. [feedback/README.md](../feedback/README.md).
