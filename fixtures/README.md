# Eval fixtures (R1)

Golden PR-кейсы для регрессии post-processing + gate **без LLM**.

**Как это работает:** [docs/architecture.md](../docs/architecture.md) · [docs/false-positives.md](../docs/false-positives.md)

## Структура кейса

| Файл | Назначение |
|------|------------|
| `meta.json` | `id`, `description`, `expected`, опц. `gate` override |
| `diff.patch` | unified diff (для `changedPaths` и FP-filter) |
| `findings.json` | `{ "findings": [...] }` — mock-вывод «модели» |
| `verifier.json` | опц. `{ "verdicts": [...] }` — mock Pass B (R3) |
| `findings[].advisorClaims` | опц. refutation R5 |
| `findings[].prescan` | опц. eslint-prescan finding (R6) |

Число seed-кейсов проверяет тест `eval harness: все fixtures green` (не дублируем счётчик здесь вручную).

### `meta.json` → `expected`

| Поле | Что сверяет |
|------|-------------|
| `shouldBlock` | `wouldBlock` гейта |
| `blockerCount` | число blockers |
| `findingCount` | число findings **после** FP/refutation/verifier |
| `titles` | titles среди **blockers** (должны присутствовать) |
| `noTitles` | titles среди **всех** оставшихся findings (не должны присутствовать) |

`titles` ≠ `noTitles` по области намеренно: FP может понизить severity вместо удаления — проверка только blockers такое пропустит.

## Прогон

```bash
npx pr-review-eval          # все fixtures
npx pr-review-eval --json   # machine-readable
node --test test/pr-review.eval.test.mjs
```

CI: step в [pr-review.yml](../../../.github/workflows/pr-review.yml).

## Добавить кейс вручную

1. Скопировать каталог по образцу `fp-e2e-in-diff/` или `gate-critical-high-conf/`.
2. `expected.shouldBlock` + `blockerCount` — что ждём от gate.
3. `npx pr-review-eval` — green перед PR.

## Из R8 feedback (рекомендуемый путь)

```mermaid
flowchart LR
  HAR[feedback harvest] --> CAND[candidates/*.json]
  CAND --> FIX[fixtures/id/]
  FIX --> EVAL[eval.mjs green]
```

1. Harvest: 👎 на inline или label `ai-review-fp` → `npx pr-review-feedback --pr N`.
2. Взять `feedback/candidates/<suggestedId>.json`.
3. Создать `fixtures/<suggestedId>/`:
   - `meta.json` ← `candidate.meta`
   - `findings.json` ← `{ "findings": candidate.findings }`
   - `diff.patch` ← `gh pr diff N -- path/to/file`
4. Для FP: `expected: { "shouldBlock": false, "blockerCount": 0 }`.
5. Eval green → PR.

### Приоритет seed из журнала FP

| PR | Паттерн | Fixture id (предложение) |
|----|---------|--------------------------|
| #245 | e2e «не в чанке» | `fp-pr-245-e2e-in-chunk` |
| #254 | file-absent галлюцинация | `fp-pr-254-file-in-diff` ✅ |
| #257 | file-absent + e2e в diff | `fp-pr-257-file-and-e2e-in-diff` ✅ |
| #257 | self-contradicting critical | `fp-pr-257-not-in-chunk` (ещё нет — не auto-filter) |

Детали инцидентов → [docs/false-positives.md](../docs/false-positives.md).

**Критерий приёмки модели/промпта:** новая конфигурация не мержится без green eval на всех fixtures (включая новые FP-кейсы).
