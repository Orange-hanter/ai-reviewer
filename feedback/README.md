# R8 — GitHub feedback harvest

Сбор сигналов «ложное срабатывание» / «верное замечание» для eval loop и аналитики.

**Как это работает:** [docs/false-positives.md](../docs/false-positives.md) · [docs/architecture.md](../docs/architecture.md)

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

## Сигналы

| Сигнал | Источник | Значение |
|--------|----------|----------|
| 👎 (`-1`) | trusted inline comment | FP — ложный блокер |
| 👍 (`+1`) | trusted inline comment | TP — верное замечание |
| label `ai-review-fp` | PR labels | все findings из trusted sticky → FP |

Trusted author по умолчанию: `github-actions[bot]` (`REVIEW_FEEDBACK_TRUSTED_AUTHORS`).

## CLI

```bash
# нужны GITHUB_TOKEN, GITHUB_REPOSITORY, PR_NUMBER
npx pr-review-feedback --pr 123

# batch по последним PR (или добавьте --pr N)
npx pr-review-feedback-batch --dry-run

# проверить / мигрировать persisted log
node feedback/migrate-feedback.mjs
node feedback/migrate-feedback.mjs --write

# promote candidates → fixtures (eval check)
node feedback/promote-feedback.mjs

# aggregate recurring FP from JSONL (≥2)
node feedback/promote-feedback.mjs --aggregate
```

## CI

Workflow [pr-review-feedback.yml](../../../.github/workflows/pr-review-feedback.yml):

- trigger: daily cron, label `ai-review-fp` или `workflow_dispatch`
- trusted-copy из `origin/main` (anti pwn-request)
- legacy rows мигрируются в schema v1 перед harvest
- новые записи публикуются отдельным `bot/ai-review-feedback-*` PR; прямой push в `main` запрещён
- пока bot PR открыт, следующий cron не создаёт конкурирующий PR
- artifact `pr-review-feedback-<N|run_id>` (30 d)

Weekly promote читает только уже смерженный `fp-feedback.jsonl` из `main`.
Автоматический цикл заканчивается merge-ready bot PR: merge остаётся review-gate.

## Candidate → fixture (кратко)

1. Скачать `candidates/fp-pr-<N>-*.json` (локально или из artifact).
2. Создать `fixtures/<id>/` с `meta.json`, `findings.json`, `diff.patch`.
3. `npx pr-review-eval` — green.
4. PR.

Пошагово → [fixtures/README.md](../fixtures/README.md) и спека §11.2.

## Env

| Переменная | Default |
|------------|---------|
| `REVIEW_FEEDBACK` | `1` |
| `REVIEW_FEEDBACK_LABEL` | `ai-review-fp` |
| `REVIEW_FEEDBACK_TRUSTED_AUTHORS` | `github-actions[bot]` |
| `REVIEW_FEEDBACK_LOG` | `paths.feedbackLog` профиля |
| `REVIEW_FEEDBACK_CANDIDATES_DIR` | `paths.feedbackCandidatesDir` профиля |
| `REVIEW_FP_PATTERNS` | `paths.fpPatterns` профиля |
| `REVIEW_FP_PROMOTE_MIN` | `2` |
| `REVIEW_INLINE_ON` | `critical,major` (severity для inline threads) |
| `REVIEW_FEEDBACK_CONCURRENCY` | `6` |
| `REVIEW_FEEDBACK_SINCE_DAYS` | `14` (batch window) |
| `REVIEW_FEEDBACK_PR_LIMIT` | `30` (batch cap) |
| `REVIEW_FEEDBACK_WRITE` | `1` (0 = dry-run) |
| `REVIEW_FEEDBACK_MD_APPEND` | `0` (1 → append в ai-review-false-positives.md) |
