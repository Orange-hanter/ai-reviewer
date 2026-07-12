# Конфигурация

Единый источник defaults — `loadReviewConfig()` в `lib/config.mjs`. Пути к артефактам пакета резолвятся от корня npm-пакета (`lib/package-root.mjs`), если env не переопределён.

## CLI

```bash
pr-review [--dry-run] [--diff path/to.patch] [--pr N] [--repo owner/repo]
pr-review-eval [--fixtures dir] [--json]
pr-review-feedback [--dry-run] [--pr N] [--repo owner/repo]
pr-review-feedback-batch [--dry-run] [--pr N]
```

| Флаг | Назначение |
|------|------------|
| `--dry-run` | Не постить в GitHub; печать summary в stdout |
| `--diff` | Offline-режим: unified diff из файла (без PR API) |
| `--pr` / `--repo` | Явный PR (иначе `PR_NUMBER`, `GITHUB_REPOSITORY`) |

## GitHub

| Переменная | Default | Описание |
|------------|---------|----------|
| `GITHUB_TOKEN` | — | PAT или `GITHUB_TOKEN` Actions |
| `GITHUB_REPOSITORY` | — | `owner/repo` |
| `PR_NUMBER` | — | Номер PR |
| `GITHUB_API_URL` | `https://api.github.com` | Enterprise API |

## Ollama / LLM

| Переменная | Default | Описание |
|------------|---------|----------|
| `OLLAMA_API_KEY` | — | Обязателен для LLM-слоя |
| `OLLAMA_BASE_URL` | `https://ollama.com/v1` | API base |
| `REVIEW_MODE` | `moa` | `moa` \| `consensus` |
| `MOA_REFERENCE_MODELS` | см. `constants.mjs` | Reference advisors (prose) |
| `MOA_SPECIALIST_MODELS` | role:model map | security / correctness / architecture |
| `MOA_AGGREGATOR` | `minimax-m2.7` | JSON aggregator |
| `MOA_REFERENCE_TEMPERATURE` | `0.6` | Advisors |
| `MOA_AGGREGATOR_TEMPERATURE` | `0.4` | Aggregator |
| `OLLAMA_MODELS` | — | Consensus mode: список моделей |
| `OLLAMA_MODELS_FALLBACK` | `deepseek-v4-flash` | Аварийный fallback |
| `REVIEW_JSON_MODE` | `1` | JSON-mode для structured output |
| `REVIEW_TEMPERATURE` | `0.4` | Consensus mode |
| `REVIEW_SEED` | `7` | Детерминизм (где поддерживается) |
| `REVIEW_HTTP_RETRIES` | `3` | Retry GitHub + Ollama |

## Gate (merge block)

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_BLOCK_ON` | `critical,major` | Severity → exit 1 |
| `REVIEW_INLINE_ON` | `critical,major` | Inline threads (minor/nit → sticky) |
| `REVIEW_BLOCK_SINGLE` | `critical` | Один finding может блокировать |
| `REVIEW_CONSENSUS` | `1` (MoA) / `2` (consensus) | Мин. моделей для кластера-блокера |
| `REVIEW_CONFIDENCE_MIN` | `0.8` | Блок только при confidence ≥ порога |

## Verifier (Pass B)

| Переменная | Default |
|------------|---------|
| `REVIEW_VERIFIER` | `1` |
| `REVIEW_VERIFIER_MODEL` | `deepseek-v4-pro` |
| `REVIEW_VERIFIER_MAX` | `12` |
| `REVIEW_VERIFIER_TEMPERATURE` | `0.2` |

## Router (R4)

| Переменная | Default |
|------------|---------|
| `REVIEW_ROUTER` | `1` |

Классы: lockfile/generated/rename → **skip**; docs-only → **light**; server/shared/client logic → **full**.

## Prescan (R6 / R6+)

| Переменная | Default |
|------------|---------|
| `REVIEW_PRESCAN` | `1` |
| `REVIEW_PRESCAN_BLOCK` | `1` |
| `REVIEW_APP_DIR` | `app` |
| `REVIEW_ESLINT_BIN` | `npx` |
| `REVIEW_PRESCAN_MAX_FILES` | `30` |
| `REVIEW_PRESCAN_ASTGREP` | `1` |
| `REVIEW_ASTGREP_CONFIG` | `<pkg>/ast-grep/sgconfig.yml` |
| `REVIEW_ASTGREP_BIN` | `ast-grep` |

## Graphify

| Переменная | Default |
|------------|---------|
| `GRAPHIFY_BIN` | `.graphify/gf` |
| `GRAPH` | `.graphify/graphify-out/graph.json` |

## Бюджеты

| Переменная | Default |
|------------|---------|
| `REVIEW_MAX_DIFF_BYTES` | `200000` |
| `REVIEW_CHUNK_BYTES` | `56000` |
| `REVIEW_MAX_CHUNKS` | `40` |
| `REVIEW_CONCURRENCY` | `10` |
| `REVIEW_MAX_INLINE` | `50` |
| `REVIEW_MAX_FILE_LINES` | `1500` |
| `REVIEW_MAX_CONTEXT_BYTES` | `180000` |

## Observability / export

| Переменная | Default |
|------------|---------|
| `REVIEW_OBSERVABILITY` | `1` |
| `REVIEW_OBSERVABILITY_DIR` | `.graphify/graphify-out/observability` |
| `REVIEW_OBSERVABILITY_FILE` | `pr-review-runs.jsonl` |
| `REVIEW_EXPORT` | `1` |
| `REVIEW_EXPORT_DIR` | `.graphify/graphify-out/review-export` |

## Feedback (R8)

| Переменная | Default (в пакете) |
|------------|-------------------|
| `REVIEW_FEEDBACK` | `1` |
| `REVIEW_FEEDBACK_LABEL` | `ai-review-fp` |
| `REVIEW_FEEDBACK_LOG` | `feedback/fp-feedback.jsonl` |
| `REVIEW_FP_PATTERNS` | `feedback/fp-patterns.jsonl` |
| `REVIEW_FEEDBACK_CANDIDATES_DIR` | `feedback/candidates` |
| `REVIEW_FP_PROMOTE_MIN` | `2` |

Потребитель (Orakul) переопределяет log/patterns на overlay: `ops/ai-review/`.

## Специалисты (R5)

| Переменная | Default |
|------------|---------|
| `REVIEW_SPECIALISTS` | `1` (в MoA) |

Отключение: `REVIEW_SPECIALISTS=0`.

## Auto-triage (R11)

| Переменная | Default |
|------------|---------|
| `REVIEW_AUTO_TRIAGE` | `1` |
