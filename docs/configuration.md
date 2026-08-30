# Конфигурация

Приоритет: **env / CLI (прогон) → профиль проекта (репозиторий) → дефолты движка**.
Единый источник — `loadReviewConfig()` в [`lib/config.mjs`](../lib/config.mjs); пути к
артефактам самого пакета резолвятся от корня пакета ([`lib/package-root.mjs`](../lib/package-root.mjs)).
Всё, что зависит от репозитория, лучше держать не в env, а в [профиле](profiles.md).

Boolean env (через `envFlag`): default-on принимают `0`/`false`/`no`/`off` как выкл.;
default-off — `1`/`true`/`yes`/`on` как вкл. Пустое/незадано → default флага.

## CLI

```bash
pr-review [--dry-run] [--diff path/to.patch] [--pr N] [--repo owner/repo]
pr-review-eval [--fixtures dir] [--json]
pr-review-gold [--case id]                  # shadow-прогон с LLM на gold-диффах
pr-review-metrics [path.jsonl]              # сводка по observability-логу
pr-review-probe-models                      # доступность и латентность моделей
pr-review-feedback [--dry-run] [--pr N] [--repo owner/repo]
pr-review-feedback-batch [--dry-run] [--pr N]
```

| Флаг | Назначение |
|------|------------|
| `--dry-run` | Не постить в GitHub; summary в stdout |
| `--diff` | Offline-режим: unified diff из файла (без PR API) |
| `--pr` / `--repo` | Явный PR (иначе `PR_NUMBER`, `GITHUB_REPOSITORY`) |

## Профиль

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_PROFILE` | автопоиск `.ai-reviewer.json` | Путь к [профилю проекта](profiles.md) |

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
| `MOA_REFERENCE_MODELS` | `kimi-k2.7-code,glm-5.2,deepseek-v4-flash` | Advisors (prose) |
| `MOA_SPECIALIST_MODELS` | `security:deepseek-v4-flash,correctness:kimi-k2.7-code,architecture:glm-5.2` | Роль → модель |
| `MOA_AGGREGATOR` | `minimax-m3` | Жюри-агрегатор (JSON) |
| `MOA_REFERENCE_TEMPERATURE` | `0.3` | Advisors |
| `MOA_AGGREGATOR_TEMPERATURE` | `0.4` | Aggregator |
| `MOA_REFERENCE_MAX_TOKENS` | `12000` | Потолок ответа advisor'а |
| `OLLAMA_MODEL` | `kimi-k2.7-code` | Одиночная модель (consensus) |
| `OLLAMA_MODELS` | — | Consensus mode: список моделей |
| `OLLAMA_MODELS_FALLBACK` | `deepseek-v4-flash` | Аварийная замена упавшей модели |
| `REVIEW_JSON_MODE` | `1` | JSON-mode для structured output |
| `REVIEW_TEMPERATURE` | `0.4` | Consensus mode |
| `REVIEW_SEED` | `7` | Детерминизм (где поддерживается) |
| `REVIEW_REASONING_EFFORT` | `low` | Reasoning effort advisor-вызовов (`none` — быстрее) |
| `REVIEW_HTTP_RETRIES` | `3` | Retry GitHub + Ollama |
| `REVIEW_LLM_TIMEOUT_MS` | `240000` | Таймаут одной попытки LLM |
| `REVIEW_PROVIDER_CONCURRENCY` | `12` | Глобальный cap параллельных LLM-вызовов (AIMD по 429) |
| `REVIEW_PROVIDER_PER_MODEL` | `6` | Cap параллельных вызовов на модель |

## Gate (блокировка merge)

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_BLOCK_ON` | `critical,major` | Severity → exit 1 |
| `REVIEW_BLOCK_SINGLE` | `critical` | Severity, которой хватает одной модели |
| `REVIEW_CONSENSUS` | `1` (moa) / `2` (consensus) | Мин. моделей для кластера-блокера |
| `REVIEW_CONFIDENCE_MIN` | `0.8` | Блок только при confidence ≥ порога |
| `REVIEW_LLM_REQUIRED` | `1` | Полный аутедж LLM блокирует merge (fail-closed); `0` — старое fail-open |
| `REVIEW_INLINE_ON` | `critical,major` | Severity, которым положен inline (см. actionability) |

## Маршрут находки (actionability)

Где показывать находку, считается по доказуемости (prescan/verifier/повтор/консенсус…),
а не по ярлыку severity — см. [`lib/actionability.mjs`](../lib/actionability.mjs).

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_ACTIONABILITY_MIN` | `4` | Порог суммы весов сигналов для inline |
| `REVIEW_MAX_ADVISORY_INLINE` | `5` | Сколько advisory максимум уходит в inline |
| `REVIEW_VERIFY_ADVISORY` | `5` | Сколько лучших advisory отдать verifier'у |
| `REVIEW_MAX_INLINE` | `50` | Потолок inline-комментариев на прогон |

## Verifier (Pass B)

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_VERIFIER` | `1` | Второй проход по кандидатам в блокеры |
| `REVIEW_VERIFIER_MODEL` | `deepseek-v4-pro` | Модель-судья (вне пула advisors) |
| `REVIEW_VERIFIER_MAX` | `12` | Максимум кандидатов на проверку |
| `REVIEW_VERIFIER_TEMPERATURE` | `0.2` | — |
| `REVIEW_VERIFIER_STRICT` | `1` | Нет вердикта → находка понижается до minor (fail-closed) |

## Router

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_ROUTER` | `1` | `0` — всегда полный прогон |

Классы: lockfile / generated / rename-only → **skip**; docs-only → **light**; код → **full**.

## Specialists / refutation

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_SPECIALISTS` | `1` (в moa) | Роли security/correctness/architecture вместо анонимных advisors |

## Prescan (eslint + ast-grep)

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_PRESCAN` | `1` | Детерминированный проход по изменённым файлам |
| `REVIEW_PRESCAN_BLOCK` | `1` | Правила из block-списка блокируют merge |
| `REVIEW_PRESCAN_BLOCK_RULES` | `no-restricted-imports,no-restricted-globals` | eslint-правила уровня блока |
| `REVIEW_PRESCAN_MAX_FILES` | `30` | Потолок файлов на прогон |
| `REVIEW_APP_DIR` | `layout.appDir` профиля | Каталог с `node_modules` для линтеров |
| `REVIEW_ESLINT_BIN` | `npx` | Если нет локального `node_modules/.bin/eslint` |
| `REVIEW_PRESCAN_ASTGREP` | `1` | ast-grep анти-паттерны |
| `REVIEW_ASTGREP_CONFIG` | `paths.astgrepConfig` профиля → ruleset пакета | `sgconfig.yml` |
| `REVIEW_ASTGREP_BIN` | `ast-grep` | Путь к бинарю |
| `REVIEW_PRESCAN_ASTGREP_BLOCK_RULES` | `raw-x-forwarded-for` | ast-grep правила уровня блока |

Оба линтера **fail-open**: нет бинаря/конфига — prescan молча пропускается.

## Graphify (blast-radius)

| Переменная | Default | Описание |
|------------|---------|----------|
| `GRAPHIFY_BIN` | `.graphify/gf` | Бинарь graphify |
| `GRAPH` | `.graphify/graphify-out/graph.json` | Граф кода |
| `GRAPHIFY_REPORT` | `.graphify/graphify-out/GRAPH_REPORT.md` | Отчёт графа |
| `REVIEW_GRAPHIFY_DEPTH` | `2` | `--depth` для `graphify affected` (clamp 1…5) |
| `REVIEW_GRAPHIFY_DROP_AMBIGUOUS` | `1` | `0`/`false`/`no`/`off` — оставлять AMBIGUOUS-рёбра в blast |

Нет графа → blast-radius пустой, ревью продолжается.

Движок после `affected` обогащает ответ из `graph.json`: имя caller, confidence
(`EXTRACTED`/`INFERRED`), community (cross-community сильнее layer-эвристик). В метриках —
`affected_files` (пути для swarm/оркестратора), `confidence`, `cross_community`.

## Бюджеты

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_MAX_DIFF_BYTES` | `200000` | Больше — diff урезается |
| `REVIEW_CHUNK_BYTES` | `56000` | Размер чанка |
| `REVIEW_MAX_CHUNKS` | `40` | Потолок числа чанков |
| `REVIEW_CONCURRENCY` | `10` | Параллельных chunk-pipeline |
| `REVIEW_MAX_FILE_LINES` | `1500` | Полный текст файла в контекст |
| `REVIEW_MAX_CONTEXT_BYTES` | `180000` | Потолок контекста |
| `REVIEW_MAX_SYMBOLS` | `25` | Символов в blast-radius |
| `REVIEW_MAX_DEPS_PER_SYMBOL` | `20` | Зависимостей на символ |
| `REVIEW_GOD_NODE_THRESHOLD` | `20` | Порог «god node» в графе |
| `REVIEW_TIME_BUDGET_MIN` | `20` | Дедлайн LLM-фазы: дальше частичный отчёт |

## Observability / export

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_OBSERVABILITY` | `1` | JSONL-лог прогона |
| `REVIEW_OBSERVABILITY_DIR` | `paths.observabilityDir` профиля | Каталог лога |
| `REVIEW_OBSERVABILITY_FILE` | `pr-review-runs.jsonl` | Имя файла |
| `REVIEW_EXPORT` | `1` | JSON + SARIF артефакт |
| `REVIEW_EXPORT_DIR` | `paths.exportDir` профиля | Каталог экспорта |
| `REVIEW_EXPORT_JSON` / `REVIEW_EXPORT_SARIF` | `1` | Что писать |
| `REVIEW_EXPORT_SARIF_ALL` | `0` | В SARIF все находки, а не только блокеры |
| `REVIEW_REASONING_STICKY` | `1` | Секция reasoning в sticky |

## Feedback (обучение на 👍/👎)

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_FEEDBACK` | `1` | Harvest включён |
| `REVIEW_FEEDBACK_LABEL` | `ai-review-fp` | Метка PR «здесь ложное срабатывание» |
| `REVIEW_FEEDBACK_LOG` | `paths.feedbackLog` профиля | JSONL-журнал (данные проекта) |
| `REVIEW_FP_PATTERNS` | `paths.fpPatterns` профиля → набор пакета | Паттерны известных FP |
| `REVIEW_FEEDBACK_CANDIDATES_DIR` | `paths.feedbackCandidatesDir` профиля | Кандидаты в fixtures |
| `REVIEW_FEEDBACK_MD` | `paths.fpMarkdown` профиля | Журнал FP в markdown |
| `REVIEW_FEEDBACK_MD_APPEND` | `0` | Дописывать журнал автоматически |
| `REVIEW_FEEDBACK_CANDIDATES` | `1` | Писать кандидатов |
| `REVIEW_FEEDBACK_WRITE` | `1` | `0` — dry-run harvest |
| `REVIEW_FEEDBACK_TRUSTED_AUTHORS` | `github-actions[bot]` | Чьи inline считаем своими |
| `REVIEW_FEEDBACK_CONCURRENCY` | `6` | Параллельных запросов реакций |
| `REVIEW_FP_PROMOTE_MIN` | `2` | Сколько одинаковых FP → новый паттерн |
| `REVIEW_FIXTURES_DIR` | fixtures пакета | Куда промоутить fixtures |
| `REVIEW_GOLD_DIR` | gold пакета | Набор для shadow-прогона с LLM |

## Auto-triage

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_AUTO_TRIAGE` | `1` | Авто-ответ и resolve на своих тредах с известным FP |

## Уведомления

| Переменная | Default | Описание |
|------------|---------|----------|
| `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` | — | Пуш итога прогона (необязательно) |
