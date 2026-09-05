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
pr-review-probe-cache [model] [--size N]    # даёт ли провайдер скидку за общий префикс
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
| `MOA_REFERENCE_MAX_TOKENS` | `6000` | Потолок ответа advisor'а. Страховка под лимит «не более 8 пунктов» из промпта роли (RFC 04) |
| `OLLAMA_MODEL` | `kimi-k2.7-code` | Одиночная модель (consensus) |
| `OLLAMA_MODELS` | — | Consensus mode: список моделей |
| `OLLAMA_MODELS_FALLBACK` | `deepseek-v4-flash` | Аварийная замена упавшей модели |
| `REVIEW_JSON_MODE` | `1` | JSON-mode для structured output |
| `REVIEW_TEMPERATURE` | `0.4` | Consensus mode |
| `REVIEW_SEED` | `7` | Детерминизм (где поддерживается) |
| `REVIEW_REASONING_EFFORT` | `low` | Reasoning effort advisor-вызовов (`none` — быстрее) |
| `REVIEW_AGGREGATOR_REASONING_EFFORT` | `low` | Reasoning effort ПЕРВОЙ попытки JSON-вызовов (агрегатор жюри, verifier). Рассуждение провайдер считает completion-токенами: замер — 1903 completion-токена на находку у `minimax-m3` против 474 у `sonnet`, вся разница — reasoning. Каскад ретраев `chatJson` (пустой content / оборванный JSON) по-прежнему уходит в `none` и этой ручкой не управляется |
| `REVIEW_PROMPT_LAYOUT` | `legacy` | `shared-prefix` — общая рамка и контекст чанка первыми, инструкция роли последней: все роли чанка делят побайтово одинаковый префикс, который провайдер может отдать из кэша. **Замер `pr-review-probe-cache` показал, что Ollama Cloud скидки за общий префикс не даёт** — включать имеет смысл только на провайдере с префиксным кэшем, проверив тем же пробником |
| `REVIEW_HTTP_RETRIES` | `3` | Retry GitHub + Ollama |
| `REVIEW_LLM_TIMEOUT_MS` | `420000` | Таймаут одной попытки LLM. 240000 обрывали половину prose-вызовов (медиана kimi — 331 с): вход оплачен, ответа нет |
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
| `REVIEW_VERIFY_ADVISORY_MIN` | `REVIEW_ACTIONABILITY_MIN − 3` (= `1`) | Порог **допуска** advisory к verifier'у — отдельный от порога маршрута и намеренно ниже его на **вес сигнала `verified`** (3, см. `ACTIONABILITY_SIGNALS` в [`lib/actionability.mjs`](../lib/actionability.mjs)). Кандидатом становится всё, что *станет* доказуемым, если verifier подтвердит; окончательный порог inline остаётся `REVIEW_ACTIONABILITY_MIN`. Равные пороги давали курицу и яйцо: чтобы minor дошёл до verifier'а за +3, он должен был уже набрать 4 без него (docs/review-quality.md §2.2, P1) |
| `REVIEW_MAX_INLINE` | `50` | Потолок inline-комментариев на прогон |

## Verifier (Pass B)

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_VERIFIER` | `1` | Второй проход по кандидатам в блокеры |
| `REVIEW_VERIFIER_MODEL` | `deepseek-v4-pro` | Модель-судья (вне пула advisors) |
| `REVIEW_VERIFIER_MAX` | `12` | Максимум кандидатов на проверку |
| `REVIEW_VERIFIER_TEMPERATURE` | `0.2` | — |
| `REVIEW_VERIFIER_CONTEXT_MARGIN` | `80` | ±строк вокруг каждой строки-кандидата в контексте verifier'а. Пересекающиеся окна сливаются, пропуски помечаются `… (строки N–M пропущены)` — реальные номера строк сохраняются. Файл, у которого хоть один кандидат без строки, отдаётся целиком. Второй проход смотрит одну-две строки на файл и делает это ДРУГОЙ моделью (кэш префикса advisor'ов не работает), поэтому целый файл до `REVIEW_MAX_FILE_LINES` строк он не окупает — см. [`lib/context-window.mjs`](../lib/context-window.mjs) |
| `REVIEW_VERIFIER_STRICT` | `1` | Нет вердикта → находка понижается до minor (fail-closed) |

## Инкрементальный прогон

Не ревьюить заново файлы, чей **вход для модели** (секция диффа + приложенный контекст) не
изменился с прошлого прогона; их находки переносятся вместе с вердиктами жюри и verifier —
см. [RFC 05](../proposals/05-incremental-review.md).

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_INCREMENTAL` | `0` | Включает инкрементальный прогон |
| `REVIEW_INCREMENTAL_STATE` | `paths.incrementalState` профиля (`.ai-review/incremental/state.json`) | Файл состояния: хэши входа по файлам и находки прошлого прогона. Версия состояния 2: хэш входа — JSON `{diff, context}` (без NUL в исходнике), отпечаток конфигурации включает порог маршрута/допуска verifier и таймаут LLM |
| `REVIEW_INCREMENTAL_FULL_EVERY` | `10` | Каждый N-й прогон полный — страховка от накопления дрейфа |

В CI состояние нужно сохранять между прогонами (`actions/cache` на `.ai-review/incremental/`).
Нет кэша — каждый прогон полный, то есть прежнее поведение. Прогон становится полным также при
смене конфигурации (модели, роли, раскладка промпта, нарезка чанков, параметры гейта,
порог маршрута/допуска verifier, таймаут LLM): находки прошлого прогона получены другим
движком и сопоставлять их с новыми нельзя.

## Router

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_ROUTER` | `1` | `0` — всегда полный прогон |

Классы: lockfile / generated / rename-only → **skip**; docs-only → **light**; код → **full**.

## Specialists / refutation

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_SPECIALISTS` | `1` (в moa) | Роли security/correctness/architecture вместо анонимных advisors |
| `REVIEW_SPECIALISTS_MERGE` | `auto` | `auto` — один вызов на все роли, когда модель у них одна (три мнения одной модели независимы только на бумаге); `always` — сливать и при разных моделях; `never` — всегда по отдельности |

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

## Structure / contracts / scenarios

Отдельный `pr-review-structure` использует профиль, но не влияет на PR merge-гейт.

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_STRUCTURE` | `1` | Structure producer доступен |
| `REVIEW_STRUCTURE_GRAPH` | `GRAPH` | Graphify facts |
| `REVIEW_STRUCTURE_GRAPH_REPORT` | `GRAPHIFY_REPORT` | Freshness |
| `REVIEW_STRUCTURE_GRAPH_ANALYSIS` | `.graphify_analysis.json` рядом с graph.json | Communities/god-nodes; sidecar в каталоге graph.json, следует за override `GRAPH`/`REVIEW_STRUCTURE_GRAPH` |
| `REVIEW_STRUCTURE_SCIP` | пусто | SCIP semantic index |
| `REVIEW_STRUCTURE_LCOV` | `paths.coverageLcov` | LCOV evidence |
| `REVIEW_STRUCTURE_OBLIGATIONS` | пусто | cod-doc obligations export |
| `REVIEW_STRUCTURE_MUTATION` | пусто | Mutation Testing Elements report |
| `REVIEW_STRUCTURE_PROPERTIES` | пусто | Property execution report |
| `REVIEW_STRUCTURE_EXPORT_DIR` | `paths.structureExportDir` | Facts/assessment/drift outputs |
| `REVIEW_STRUCTURE_SCOPE` | пусто | Path-prefix scope |
| `REVIEW_STRUCTURE_TRUST_TIER` | `trusted_local` | `signed_ci`, `trusted_local`, `untrusted`. ⚠️ `untrusted` понижает evidence ceiling всех receipts: сценарии не поднимутся выше статического инвентаря, включайте только для недоверенных источников |
| `REVIEW_STRUCTURE_INCLUDE_AMBIGUOUS` | `0` | Включать AMBIGUOUS Graphify edges |
| `REVIEW_STRUCTURE_MAX_ENTITIES` | `5000` | Hard cap facts |
| `REVIEW_STRUCTURE_MAX_EDGES` | `15000` | Hard cap dependencies |
| `REVIEW_STRUCTURE_HIGH_FAN_IN` | `5` | Hotspot threshold |
| `REVIEW_STRUCTURE_MAX_UNRESOLVED_RATE` | `0.1` | Выше порога запрещён строгий `missing` |
| `STRUCTURE_MCP_ROOT` | рабочий каталог процесса | Единственный корень, из которого MCP-серверу разрешено читать `factsPath`/`assessmentPath`. Пути приходят от клиента и проверяются через `realpath` (symlink наружу не проходит), выход за корень — ошибка `path escapes allowed root`. Задавайте явно: дефолт зависит от того, откуда запущен агент — см. [structure.md](structure.md#confinement-what-the-server-is-allowed-to-read) |

Подробности протокола и MCP — в [structure.md](structure.md).

## Бюджеты

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_MAX_DIFF_BYTES` | `200000` | Больше — diff урезается |
| `REVIEW_CHUNK_BYTES` | `56000` | Размер чанка |
| `REVIEW_MAX_CHUNKS` | `40` | Потолок числа чанков |
| `REVIEW_CONCURRENCY` | `10` | Параллельных chunk-pipeline |
| `REVIEW_MAX_FILE_LINES` | `1500` | Полный текст файла в контекст |
| `REVIEW_MAX_CONTEXT_BYTES` | `180000` | Потолок контекста |
| `REVIEW_SKIP_ADDED_FILE_CONTEXT` | `1` | Не прикладывать полный текст **добавленных** файлов: их дифф и есть содержимое целиком. `0` — прежнее поведение |
| `REVIEW_CHUNK_ROUTING` | `1` | Чанк без исполняемого файла (нет кода, CI-конфигурации и чувствительных путей) обслуживает один агрегатор вместо жюри. `0` — всегда полный MoA |
| `REVIEW_CONTEXT_MARGIN` | `40` | ±строк вокруг изменённых строк файла в контексте advisors и жюри ([RFC 07](../proposals/07-windowed-file-context.md)). Центр окон — `validLines` из `parseDiff` (RIGHT-номера `+`-строк и контекстных строк хунка), пересекающиеся окна сливаются, пропуски помечаются `… (строки N–M пропущены)`. Такой путь метится в карте PR как `[THIS PART, CONTEXT WINDOWED]` и включает запрет absence-claims даже на PR из одного чанка. **`0` — окна выключены**, файлы едут целиком: это откат решения. Ноль здесь значащий (читается `nonNegEnv`, не `numEnv`) |
| `REVIEW_CONTEXT_WINDOW_MIN_CHARS` | `8000` | Файл, чья запись контекста короче этого, прикладывается целиком — окна не считаются. Порог снимает класс «маленький файл, плотные правки», где окна не экономят ни байта, а риск усечения берут целиком |
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

### Поля артефакта (`version: 2`)

Артефакт `<toolName>.json` — машинный вывод прогона для внешнего потребителя
(cod-doc ingest, оркестратор swarm). В каждой находке `findings[]` / `blockers[]`:

| Поле | Смысл |
|------|-------|
| `fp` | Отпечаток кода под находкой — **та же величина, что в футере sticky**. Ключ дедупликации между прогонами: код изменился → отпечаток другой → находка новая. Пусто, если строки нет в диффе |
| `verifierStatus` | `verified` / `rejected` / `downgraded` / `unverified` / `overflow` — вердикт Pass B |
| `actionabilityScore` | Сумма весов сигналов доказуемости; есть у **каждой** находки |
| `actionabilityPromoted` | Advisory, поднятая в строку по доказуемости (не блокер) |
| `prescan`, `consensusBlock`, `refutationStatus`, `advisorClaims` | Откуда находка и что с ней сделали жюри и линтеры |

Версия артефакта растёт и на аддитивных изменениях, поэтому потребитель обязан сверять её
на «версия известна», а не на равенство: строгое `version == 1` сломается на `2`, где не
убрано ни одного поля. Что ломает потребителя по-настоящему — удаление поля, смена смысла
существующего или другой `markerNamespace` (см. [profiles.md](profiles.md)).

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
| `REVIEW_GOLD_DIR` | gold пакета | Набор для shadow-прогона с LLM (`gold-public` — публичный набор) |
| `REVIEW_GOLD_CACHE` | `.ai-review/gold-public` | Кэш диффов публичных кейсов, скачанных по пину `source.sha` |

## Auto-triage

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_AUTO_TRIAGE` | `1` | Авто-ответ и resolve на своих тредах с известным FP |

## Уведомления

| Переменная | Default | Описание |
|------------|---------|----------|
| `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` | — | Пуш итога прогона (необязательно) |
