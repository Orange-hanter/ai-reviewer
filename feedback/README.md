# R8 — GitHub feedback harvest

Сбор сигналов «ложное срабатывание» / «верное замечание» для eval loop и аналитики.

**Спека:** [docs/architecture.md](../docs/architecture.md) · **Конфиг:** [docs/configuration.md](../docs/configuration.md)

## Артефакты

| Путь | Назначение |
|------|------------|
| `fp-feedback.jsonl` | append-only schema-v1 log (reaction 👍/👎, label `ai-review-fp`) |
| `fp-feedback.schema.json` | JSON Schema persisted-записи |
| `fp-patterns.jsonl` | data-driven FP regex patterns (→ `matchesKnownFalsePositive`) |
| `candidates/*.json` | черновики eval-fixtures — дополните `diff.patch` → [fixtures/](../fixtures/) |
| `migrate-feedback.mjs` | проверка / атомарная миграция legacy JSONL в schema v1 |
| `promote-feedback.mjs` | CLI: candidates → fixtures; `--aggregate` из JSONL |
| `promote-patterns.mjs` | агрегация ≥N одинаковых FP → patterns + fixtures |

## CLI

```bash
npx pr-review-feedback --pr 123
npx pr-review-feedback-batch --dry-run
node feedback/migrate-feedback.mjs --write
node feedback/promote-feedback.mjs --aggregate
```

## CI

Workflow [.github/workflows/pr-review-feedback.yml](../.github/workflows/pr-review-feedback.yml) — daily cron, label `ai-review-fp`, bot PR для persistence.

Weekly promote: [pr-review-feedback-promote.yml](../.github/workflows/pr-review-feedback-promote.yml).

## Env

См. таблицу Feedback в [docs/configuration.md](../docs/configuration.md).

Потребитель Orakul переопределяет пути на `ops/ai-review/fp-*.jsonl`.
