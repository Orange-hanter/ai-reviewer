---
type: guide
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-09-29
---

# Конфигурация

Приоритет: **env / CLI (прогон) → профиль проекта (репозиторий) → дефолты движка**.
Единый источник — `loadReviewConfig()` в [`lib/config.mjs`](../lib/config.mjs); пути к
артефактам самого пакета резолвятся от корня пакета ([`lib/package-root.mjs`](../lib/package-root.mjs)).
Всё, что зависит от репозитория, лучше держать не в env, а в [профиле](profiles.md).

Boolean env (через `envFlag`): default-on принимают `0`/`false`/`no`/`off` как выкл.;
default-off — `1`/`true`/`yes`/`on` как вкл. Пустое/незадано → default флага.

## CLI

```bash
pr-review [--dry-run] [--diff path/to.patch] [--head-sha SHA] [--pr N] [--repo owner/repo]
pr-review-eval [--fixtures dir] [--json]
pr-review-gold [--case id] [--jsonl out.jsonl]  # shadow-прогон с LLM на gold-диффах
pr-review-benchmark [results.jsonl] | --findings f.json --meta meta.json | --gold-dir dir
pr-review-gold-harvest <owner/repo…> [--emit dir] [--clean N] [--force]
pr-review-gold-harvest --validate [dir]     # dir по умолчанию gold-public
pr-review-metrics [path.jsonl]              # сводка по observability-логу
pr-review-probe-models                      # доступность и латентность моделей
pr-review-probe-cache [model] [--size N]    # даёт ли провайдер скидку за общий префикс
pr-review-feedback [--dry-run] [--pr N] [--repo owner/repo]
pr-review-feedback-batch [--dry-run] [--pr N]
pr-review-structure --graph … --lcov … --facts-out …  # structure-контур, вне гейта
pr-review-mcp                               # stdio MCP: агентный dry-run ревью
pr-review-structure-mcp                     # stdio MCP structure-контура
```

| Флаг | Назначение |
|------|------------|
| `--dry-run` | Не постить в GitHub; summary в stdout |
| `--diff` | Offline-режим: unified diff из файла (без PR API) |
| `--head-sha` | Пин коммита для file-diff (MCP и dry-run). Без флага при `--diff` `headSha` в артефакте пустой |
| `--pr` / `--repo` | Явный PR (иначе `PR_NUMBER`, `GITHUB_REPOSITORY`) |
| `gold --jsonl` | Записи прогона (`id`, `findings`, `wouldBlock`, `wallMs`…) в JSONL — вход для `pr-review-benchmark` |
| `benchmark` | Оценка gold-результатов без LLM: JSON-массив/JSONL прогонов, пара `--findings` + `--meta` или `--gold-dir` (meta-only) |
| `gold-harvest --emit` | Писать кандидатов в `<dir>/<id>/meta.json`; существующую мету не перезаписывает — разметка, поправленная руками, сохраняется |
| `gold-harvest --force` | Перезаписать существующую `meta.json` |
| `gold-harvest --clean N` | Добыть ещё N clean-контролей на репозиторий (без `N` — 2) |
| `gold-harvest --validate` | Проверить набор: доставку диффа по пину (прогревает `REVIEW_GOLD_CACHE`) и локации; есть битые кейсы — exit 1 |

Флаги `pr-review-structure` — [structure.md](structure.md); `pr-review-mcp` — раздел
«Review MCP» ниже.

## Профиль

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_PROFILE` | автопоиск `.ai-reviewer.json`, затем `ai-reviewer.config.json` в корне | Путь к [профилю проекта](profiles.md) |

## GitHub

| Переменная | Default | Описание |
|------------|---------|----------|
| `GITHUB_TOKEN` | — | PAT или `GITHUB_TOKEN` Actions |
| `GITHUB_REPOSITORY` | — | `owner/repo` |
| `PR_NUMBER` | — | Номер PR |
| `GITHUB_API_URL` | `https://api.github.com` | Enterprise API |
| `REVIEW_TRUSTED_AUTHORS` | `github-actions[bot]` | Логины через запятую, от которых движок пишет sticky и inline. Свой комментарий = маркер **и** автор из списка (при наличии `user.type` — только `Bot`); комментарий с маркером от другого автора не читается как прошлый прогон, не перезаписывается и не удаляется. Если отчёты пишет PAT или GitHub App — укажите его логин. Пустое значение — доверять любому автору (прежнее поведение, откат). Алиас — `REVIEW_FEEDBACK_TRUSTED_AUTHORS` |
| `GH_TOKEN` | — | Только gold-загрузчик ([`lib/gold-fetch.mjs`](../lib/gold-fetch.mjs)): после `GITHUB_TOKEN`, перед `gh auth token`; без токена — анонимно |

## Ollama / LLM

| Переменная | Default | Описание |
|------------|---------|----------|
| `LLM_API_KEY` | — | Ключ OpenAI-совместимого провайдера; приоритет над `OLLAMA_API_KEY` |
| `LLM_BASE_URL` | — | API base; приоритет над `OLLAMA_BASE_URL` |
| `OLLAMA_API_KEY` | — | Ключ, если `LLM_API_KEY` не задан. Без ключа LLM-слой не работает |
| `OLLAMA_BASE_URL` | `https://ollama.com/v1` | API base, если `LLM_BASE_URL` не задан |
| `REVIEW_MODE` | `moa` | `moa` \| `consensus` |
| `MOA_REFERENCE_MODELS` | `kimi-k2.7-code,glm-5.3,glm-5.3-flash` | Advisors (prose) |
| `MOA_SPECIALIST_MODELS` | `security:glm-5.3-flash,correctness:kimi-k2.7-code,architecture:glm-5.3` | Роль → модель |
| `MOA_SPECIALIST_{SECURITY,CORRECTNESS,ARCHITECTURE}_MODEL` | — | Модель одной роли; приоритет над её записью в `MOA_SPECIALIST_MODELS` |
| `MOA_AGGREGATOR` | `minimax-m3` | Жюри-агрегатор (JSON) |
| `MOA_REFERENCE_TEMPERATURE` | `0.3` | Advisors |
| `MOA_AGGREGATOR_TEMPERATURE` | `0.4` | Aggregator |
| `MOA_REFERENCE_MAX_TOKENS` | `6000` | Потолок ответа advisor'а. Страховка под лимит «не более 8 пунктов» из промпта роли (RFC 04) |
| `OLLAMA_MODEL` | `kimi-k2.7-code` | Одиночная модель (consensus) |
| `OLLAMA_MODELS` | — | Consensus mode: список моделей |
| `OLLAMA_MODELS_FALLBACK` | `deepseek-v4.1-flash` | Аварийная замена упавшей модели |
| `REVIEW_JSON_MODE` | `1` | JSON-mode для structured output |
| `REVIEW_TEMPERATURE` | `0.4` | Consensus mode |
| `REVIEW_SEED` | `7` | Детерминизм (где поддерживается) |
| `REVIEW_REASONING_EFFORT` | `low` | Reasoning effort advisor-вызовов (`none` — быстрее) |
| `REVIEW_AGGREGATOR_REASONING_EFFORT` | `low` | Reasoning effort ПЕРВОЙ попытки JSON-вызовов (агрегатор жюри, verifier). Рассуждение провайдер считает completion-токенами: замер — 1903 completion-токена на находку у `minimax-m3` против 474 у `sonnet`, вся разница — reasoning. Каскад ретраев `chatJson` (пустой content / оборванный JSON) по-прежнему уходит в `none` и этой ручкой не управляется |
| `LLM_MODEL_SETTINGS` | — | Индивидуальные настройки моделей, JSON `{"<модель>": {…}}` (RFC 20, § 6). Поля: `effort` — effort первой попытки prose-вызовов, а без `jsonEffort` и JSON-вызовов (`none\|minimal\|low\|medium\|high`); `jsonEffort` — effort JSON-вызовов (жюри, verifier); `maxTokens` — бюджет prose-вызова; `provider` — маршрутизация провайдеров OpenRouter (`order`, `ignore`, `quantizations`, `allow_fallbacks`…), уходит в запрос как есть. Модель без настроек ведёт себя по глобальным `REVIEW_*_REASONING_EFFORT`. Неизвестное поле или недопустимое значение не роняет прогон, а пишется в лог. Работает только с Rust-бинарём `llm`. Пример: `{"deepseek/deepseek-v4.1-flash":{"effort":"none"},"z-ai/glm-5.3-flash":{"effort":"low"}}` |
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
| `REVIEW_CONSENSUS` | `1` (moa) / `2` (consensus) | Мин. моделей с находкой уровня блока в кластере (корзина `file|floor((line-1)/3)`), чтобы major стал блокером; остальное находка набирает сама (RFC 17). В `moa` всегда `1` (значение зажимается); в `consensus` — не больше числа моделей |
| `REVIEW_CONFIDENCE_MIN` | `0.8` | Блок только при собственной confidence находки ≥ порога (уверенность соседа по кластеру не засчитывается, RFC 17) |
| `REVIEW_LLM_REQUIRED` | `1` | Полный аутедж LLM блокирует merge (fail-closed); `0` — старое fail-open. Тот же рычаг для сбоя verifier'а при находках уровня блока (RFC 18) и для частей PR, не отревьюенных ни одной моделью из-за сбоя (RFC 19): `1` — degraded-блок, `0` — merge проходит с заметкой. Пропуск частей по тайм-бюджету — только заметка |
| `REVIEW_INLINE_ON` | `critical,major` | Severity, которым положен inline (см. actionability) |
| `REVIEW_STRICT` | `0` (или `strict: true` профиля) | Строгий гейт: находка уровня блока блокирует, только если она сама от prescan или с `verifierStatus: verified`; то же условие для inline блокер-severity; advisory в строку не поднимаются и к verifier'у по доказуемости не допускаются. Включается env **или** профилем: `0` в env профильный `strict` не выключает |
| `REVIEW_REQUIRE_QUOTE` | = `REVIEW_STRICT` | critical/major без `quote` (цитаты кода) отбрасываются при нормализации ответа модели |

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
| `REVIEW_VERIFIER_MAX` | `12` | Максимум кандидатов в одном вызове verifier |
| `REVIEW_VERIFIER_BATCHES` | `3` | Сколько вызовов verifier допустимо на прогон. Блокеры сверх `REVIEW_VERIFIER_MAX` идут следующими батчами; не влезшие в `MAX × BATCHES`, при `REVIEW_VERIFIER_STRICT=1` понижаются до minor (`verifierStatus: overflow`); не дошедшие из-за тайм-бюджета или сбоя хвостового батча — `verifierStatus: unavailable` (RFC 18) |
| `REVIEW_VERIFIER_TEMPERATURE` | `0.2` | — |
| `REVIEW_VERIFIER_CONTEXT_MARGIN` | `80` | ±строк вокруг каждой строки-кандидата в контексте verifier'а. Пересекающиеся окна сливаются, пропуски помечаются `… (строки N–M пропущены)` — реальные номера строк сохраняются. Файл, у которого хоть один кандидат без строки, отдаётся целиком. Второй проход смотрит одну-две строки на файл и делает это ДРУГОЙ моделью (кэш префикса advisor'ов не работает), поэтому целый файл до `REVIEW_MAX_FILE_LINES` строк он не окупает — см. [`lib/context-window.mjs`](../lib/context-window.mjs) |
| `REVIEW_VERIFIER_SYMBOL_CARDS` | `0` | Карточки символов ([RFC 14](../proposals/14-symbol-cards.md), решение 4) во входе verifier'а: карточки файлов кандидатов, тем же отбором и с теми же лимитами, что у advisors и жюри. Без них верная находка «вызов не совпадает с сигнатурой» verifier'у недоказуема и не становится блокером; с ними verifier может подтвердить ложную интерпретацию сигнатуры — поэтому выключено до gold ×3. Включённый флаг входит в отпечаток конфигурации инкрементального прогона |
| `REVIEW_VERIFIER_STRICT` | `1` | Нет вердикта → находка понижается до minor. Сбой verifier'а (исключение, ни одного валидного вердикта, тайм-бюджет) — находки уровня блока получают `verifierStatus: unavailable`, исход прогона — по `REVIEW_LLM_REQUIRED` (RFC 18). `0` — находка без вердикта остаётся как есть, сбой verifier'а гейт не трогает |

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

В отпечаток конфигурации (`configFingerprint`, [`lib/incremental.mjs`](../lib/incremental.mjs))
**не входят**: `REVIEW_REQUIRE_QUOTE`, `REVIEW_BLOCK_SINGLE`, `REVIEW_INLINE_ON`,
`REVIEW_MAX_ADVISORY_INLINE`, температуры, reasoning effort, `MOA_REFERENCE_MAX_TOKENS`,
настройки prescan. Их смена полного прогона не вызывает: перенесённые находки остаются
полученными при прежних значениях до ближайшего полного прогона. Нужен эффект сразу —
удалите файл состояния.

## Router

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_ROUTER` | `1` | `0` — всегда полный прогон |

Классы: lockfile / generated / rename-only (`similarity index 100%` или секция без строк `+`/`-`; rename с правками — обычный файл, RFC 17) → **skip**; удалённый файл классифицируется по своему пути; docs-only → **light**; ни одного
файла-кода (только config `yml/yaml/json/toml/sh/sql`, docs, прочее) → **light**; хоть один
файл-код → **full**. Список «кода» зашит в роутер (`LOGIC`/`CODE` в
[`lib/router.mjs`](../lib/router.mjs), паритетно в `rust/src/router.rs`); `layout.logic`
профиля его не меняет.

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
| `REVIEW_BLAST_REQUIRED` | `0` | `1` — blast-radius не посчитан (нет/не читается `graph.json`, все запросы `affected` упали) → exit 1 до LLM, sticky с причиной (fail-closed). Шаблон `examples/ci/pr-review.yml` ставит `1` |
| `REVIEW_BLAST_VERIFY` | `0` | Проверка blast-находок verifier'ом ([RFC 21](../proposals/21-blast-verification.md)): `0` — прежнее поведение (в `moa` strict-refutation понижает blast-находку до minor, merge она не блокирует); `shadow` — refutation её не понижает, verifier судит её с окном файла вызывающего кода, но гейт не блокирует: событие `blast_would_block`; `1` — блокирует после `verified`. `1`/`true`/`on`, `0`/`false`/`off` — как у прочих флагов, иное значение → `0` |
| `REVIEW_BLAST_VERIFY_MARGIN` | `20` | ±строк вокруг строки вызова в файле вне PR, которое verifier получает для blast-кандидата. Файл читается с той же защитой, что файлы PR (symlink наружу, бинарь — отказ); не прочитан → находка `minor`, `verifierStatus: unverified`, verifier её не получает |

Нет графа → blast-radius пустой, ревью продолжается (при `REVIEW_BLAST_REQUIRED=1` — блок).
Устаревший граф (`graph_stale`), неразрешённые символы и «зависимых нет» сбоем не считаются.
Маршруты router `skip`/`light` graph не требуют: blast-radius там не строится.

Движок после `affected` обогащает ответ из `graph.json`: имя caller, confidence
(`EXTRACTED`/`INFERRED`), community (cross-community сильнее layer-эвристик). В метриках —
`affected_files` (пути для swarm/оркестратора), `confidence`, `cross_community`.

## Structure / contracts / scenarios

Отдельный `pr-review-structure` использует профиль, но не влияет на PR merge-гейт.

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_STRUCTURE` | `1` | Пишется в `cfg.structure.enabled`, но этот флаг никто не читает — **сейчас эффекта нет**: `pr-review-structure` работает при любом значении |
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
| `STRUCTURE_EXPORT_ROOT` | cwd | Корень, за который не выходит запись `writeStructureExports`. Только для вызова через API без `rootDir`: `pr-review-structure` передаёт `rootDir` сам (`--repo-root` или cwd) и переменную не читает |
| `STRUCTURE_MCP_ROOT` | `REVIEW_MCP_ROOT`, иначе рабочий каталог процесса | Единственный корень, из которого MCP-серверу разрешено читать `factsPath`/`assessmentPath`. Пути приходят от клиента и проверяются через `realpath` (symlink наружу не проходит), выход за корень — ошибка `path escapes allowed root`. Порядок: `STRUCTURE_MCP_ROOT` → `REVIEW_MCP_ROOT` → cwd; плейсхолдер `${…}` ≡ не задано; несуществующий путь — `exit 1` на старте. Задавайте явно: дефолт зависит от того, откуда запущен агент — см. [structure.md](structure.md#confinement--что-серверу-разрешено-читать) |

## Review MCP (агентный dry-run)

Отдельный stdio-сервер `pr-review-mcp` ([RFC 08](../proposals/08-review-mcp.md), [RFC 09](../proposals/09-cursor-plugin-mcp.md), [RFC 10](../proposals/10-mcp-cold-start.md), [RFC 12](../proposals/12-mcp-long-run.md)). Это превью для агента, не merge-гейт: GitHub не пишется, даже если в env процесса есть `GITHUB_TOKEN`. Structure MCP — другой процесс (`STRUCTURE_MCP_ROOT` сюда не подставлять). Холодный старт плагина — `cursor-plugin/bin/install-local.sh`; Authenticate не нужен.

В IDE вызывайте `review_run` с `wait: false`: RPC сразу отдаёт stub с `jobId`, MoA идёт в том же процессе. Обрыв `tools/call` ~30 с не убивает прогон, пока stdin жив. Поллинг — `review_findings` / `review_artifact` по `jobId`. `wait: true` (default) — прежнее синхронное поведение для тестов и CLI. Sidecar: `{root}/.ai-review/mcp/runs/{jobId}.json`.

Пустой `review_prescan` на Rust-диффе (`skipped: no-eslint`, 0 находок) — ожидаемо: в пакете нет eslint по `.rs`. Потребительский ast-grep — opt-in профиля, не лечение транспорта.

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_MCP_ROOT` | лаунчер: корень engine | Единственный корень, из которого ревью-MCP читает `diffPath` / `priorExportPath` / sidecar / артефакты. `realpath`, symlink наружу не проходит. Плейсхолдер `${…}` ≡ не задано; битый путь — `exit 1` на старте |
| `AI_REVIEWER_ROOT` | `.engine-root` / cwd-клон / `cursor-plugin/../..` | Клон пакета с `bin/pr-review-mcp.mjs`. Configure перебивает `.engine-root` |
| `AI_REVIEWER_NODE` | `PATH`, `~/.local/bin/node`, Homebrew | Абсолютный путь к `node` ≥ 22, если GUI без `node` в PATH |
| `REVIEW_MCP_EXTRA` | `0` | `1` — в `tools/list` появляются `blast_scope`, `actionability_explain`, `review_metrics` |

`review_run` — всегда dry-run, но по умолчанию пишет артефакт: если `REVIEW_EXPORT_DRY_RUN` не
задан в env сервера, прогону подставляется `1` ([`lib/review-mcp.mjs`](../lib/review-mcp.mjs)).

Тулзы по умолчанию ([RFC 08](../proposals/08-review-mcp.md), [RFC 09](../proposals/09-cursor-plugin-mcp.md)):

- `review_run` — полный MoA, пин `headSha` обязателен, единственный LLM; мьютекс `review already running` на **другой** пин. `wait` default true; `wait: false` — stub `status: running` + `jobId`
- `review_findings` — страница того же прогона по `jobId` или `runFingerprint` (sidecar `{root}/.ai-review/mcp/runs/{jobId}.json`). Пока `running` — пустые findings, не `unknown runFingerprint`
- `review_route` / `review_prescan` / `fp_check` / `fingerprint_check` / `review_artifact` — дешёвый контур без LLM

Живой дифф (`review_run`, `review_route`, `review_prescan`): если корень — git и `uncommitted` не true, `headSha` сверяется префиксом с `rev-parse HEAD`; грязное дерево — ошибка `working tree dirty; pass uncommitted: true`. Не git — сверку пропускаем, `headSha` всё равно обязателен.

В ответе `previewWouldBlock`, не `wouldBlock` сворма. Cursor Plugin — каталог [`cursor-plugin/`](../cursor-plugin/).

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
| `REVIEW_SYMBOL_CARDS` | `1` | Карточки символов ([RFC 14](../proposals/14-symbol-cards.md)): для имён, которые вызывают **добавленные** строки JS/TS-файлов и чьё объявление модель не видит ни в диффе, ни в окне, в промпт advisors и жюри идёт строка объявления с диапазоном (`// lib/config.mjs:L212–L248` + заголовок без тела). Место берётся из графа Graphify (`resolveUniqueNode`, неоднозначное имя — без карточки), текст — из файла рабочего дерева; строка без имени символа значит устаревший граф, и карточки нет. Без `graph.json` фича молча выключена. Verifier карточек не получает. Карточки файла входят в хэш инкрементального прогона **без номеров строк** — сдвиг объявления не пере-ревьюит вызывателей, смена сигнатуры — пере-ревьюит. Включённая фича входит в отпечаток конфигурации: первый прогон после обновления движка — полный. `0` — откат, промпт и хэши как до RFC 14. Основание для включения по умолчанию — gold A/B в RFC 14 (вреда не найдено, по одному прогону) |
| `REVIEW_SYMBOL_CARDS_MAX` | `20` | Карточек на чанк; приоритет — число добавленных строк, вызывающих символ |
| `REVIEW_SYMBOL_CARDS_MAX_CHARS` | `3000` | Символов карточек на чанк (общий бюджет с короткими телами) |
| `REVIEW_SYMBOL_CARDS_BODY_MAX_LINES` | `0` | Тело не длиннее N строк едет целиком, с пометкой `(full body)`; `0` — только объявление. Конец объявления ищет лексер пары скобок (`scanDeclaration` в `lib/symbol-cards.mjs`); не нашёл — карточка без диапазона и без тела |
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
| `REVIEW_EXPORT_DRY_RUN` | `0` | Писать артефакт и при `--dry-run`; при `0` dry-run экспорт не пишет. Ревью-MCP по умолчанию ставит `1` |
| `REVIEW_PRICE_JSON` | — | Цены моделей `{"<model>":{"in":$/1M,"out":$/1M}}`; ключ `*` или `default` — для остальных моделей. Задан → в `efficiencyMetrics` события `run_end` появляется `cost_usd`; битый JSON ≡ не задано |
| `REVIEW_REASONING_STICKY` | `1` | Секция reasoning в sticky |

### Поля артефакта (`version: 2`)

Артефакт `<toolName>.json` — машинный вывод прогона для внешнего потребителя
(cod-doc ingest, оркестратор swarm); собирается в [`lib/export.mjs`](../lib/export.mjs),
`EXPORT_VERSION = 2`. Необязательные поля (`router`, `verdict`, `llmFailed`, `fp`, `prescan`…) без значения
в JSON отсутствуют, а не пустые.

Верхний уровень:

| Поле | Смысл |
|------|-------|
| `version`, `generatedAt` | Версия формата; ISO-время записи |
| `pr` | `{number, title, url}` (`title` ≤ 200 символов) |
| `headSha` | Ревьюенный коммит; пусто при `--diff` без `--head-sha` |
| `reviewMode` | `moa` / `consensus` |
| `router` | `{route, reason}` — маршрут роутера |
| `wouldBlock` | Блокирует ли **этот** прогон |
| `verdict` | `{sha, blocked, reproducibility}`; `reproducibility` — `{total, blocked, flaky}` по прогонам того же sha (от двух), иначе `null` |
| `llmFailed`, `llmErrors` | Сбой LLM-слоя и его ошибки — только если были |
| `counts`, `total` | Число находок по severity и всего |
| `findings`, `blockers` | Находки, формат ниже |
| `reasoning` | Выдержки рассуждений advisors |
| `verifier`, `refutation` | Статистика Pass B и refutation |
| `blast` | `{symbols_total, symbols_resolved, affected_nonempty, graph_status, skip_reason, affected_files, confidence, cross_community}` |
| `swarm` | Контракт для оркестратора — [swarm.md](swarm.md) |
| `prescan` | `{findings, scanned, skipped, byTool, error}` |

В каждой находке `findings[]` / `blockers[]`:

| Поле | Смысл |
|------|-------|
| `file`, `line`, `severity`, `model`, `confidence` | Место, уровень, модель-источник, уверенность |
| `title`, `body`, `bodyTruncated` | Заголовок ≤ 200 символов; тело урезано до 400; флаг — было ли урезано |
| `fp` | Отпечаток кода под находкой — **та же величина, что в футере sticky**. Ключ дедупликации между прогонами: код изменился → отпечаток другой → находка новая. Поля нет, если строки нет в диффе |
| `verifierStatus` | `verified` / `rejected` / `downgraded` / `unverified` / `overflow` — вердикт Pass B |
| `actionabilityScore` | Сумма весов сигналов доказуемости; есть у **каждой** находки |
| `actionabilityPromoted` | Advisory, поднятая в строку по доказуемости (не блокер) |
| `prescan`, `consensusBlock`, `refutationStatus`, `advisorClaims` | Откуда находка и что с ней сделали жюри и линтеры; элемент `advisorClaims` — `{role, status, reason}` (`reason` ≤ 200) |

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
| `REVIEW_FEEDBACK_TRUSTED_AUTHORS` | `github-actions[bot]` | Алиас `REVIEW_TRUSTED_AUTHORS` (см. [GitHub](#github)); при обоих заданных побеждает `REVIEW_TRUSTED_AUTHORS` |
| `REVIEW_FEEDBACK_CONCURRENCY` | `6` | Параллельных запросов реакций |
| `REVIEW_FEEDBACK_SINCE_DAYS` | `14` | `pr-review-feedback-batch`: PR, обновлённые за последние N дней |
| `REVIEW_FEEDBACK_PR_LIMIT` | `30` | `pr-review-feedback-batch`: максимум PR за прогон (из последних 100 по `updated`) |
| `REVIEW_FP_PROMOTE_MIN` | `2` | Сколько одинаковых FP → новый паттерн |
| `REVIEW_FIXTURES_DIR` | fixtures пакета | Куда промоутить fixtures |
| `REVIEW_GOLD_DIR` | gold пакета | Набор для shadow-прогона с LLM (`gold-public` — публичный набор) |
| `REVIEW_GOLD_CACHE` | `.ai-review/gold-public` | Кэш диффов публичных кейсов, скачанных по пину `source.sha`; для кейсов `source.local` — ещё и деревья проекта (`trees/`) |
| `REVIEW_GOLD_REPOS` | — | Локальные клоны для кейсов `gold-private/`: `orakul=/path/Orakul,restate=/path/Restate`; имя — `source.local` кейса |
| `REVIEW_GOLD_CHECKOUT` | `1` | `0` — кейс `source.local` гоняется без дерева проекта на `source.sha` (без окон контекста и карточек) |

## Auto-triage

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_AUTO_TRIAGE` | `1` | Авто-ответ и resolve на своих тредах с известным FP |

## Оформление комментариев

| Переменная | Default | Описание |
|------------|---------|----------|
| `REVIEW_ICON_BASE_URL` | — | База URL пиктограмм: задана — sticky, шапка review-сводки и inline показывают SVG вместо эмодзи (🤖 ⛔ ✅ 🔴 🟠 🟡 🔵 🔄 🔎 ⚖️ 🔧), эмодзи остаётся в `alt`. Пусто — эмодзи, как раньше. Набор — публичный [Orange-hanter/ai-reviewer-icons](https://github.com/Orange-hanter/ai-reviewer-icons); база с пином на тег: `https://raw.githubusercontent.com/Orange-hanter/ai-reviewer-icons/v1.0.0/icons`. HTML-комментарии (футер), код и ответы auto-triage не меняются; логи, dry-run и Telegram — с эмодзи |

## Уведомления

| Переменная | Default | Описание |
|------------|---------|----------|
| `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` | — | Пуш итога прогона (необязательно) |

## Rust-бинари

26 модулей `lib/*.mjs` — обёртки над Rust-бинарями. С 0.6 Rust — основная реализация,
JS-копии внутри обёрток deprecated (RFC 20). Откуда берутся бинари:

- у потребителя — архив `ai-reviewer-bins-<target>.tar.gz` из релиза того же тега. Шаблон
  `examples/ci/pr-review.yml` скачивает его, сверяет sha256 и распаковывает в
  `rust/bin/`. Платформы: `linux-x64`, `linux-arm64`, `darwin-arm64`;
- у разработчика — `cargo build` в `rust/`.

Без бинарей (чистый клон, пин не на тег релиза) всё работает на JS-копиях, без новых правок.

Путь по умолчанию (`lib/rust-bin.mjs`): берётся первый существующий из
`$AI_REVIEWER_RUST_BIN_DIR` → `rust/target/debug` → `rust/target/release` → `rust/bin`.

| Переменная | Смысл |
|------------|-------|
| `AI_REVIEWER_RUST_BIN_DIR` | Каталог со всеми бинарями; проверяется первым |
| `AI_REVIEWER_<MOD>_BIN` | Путь к бинарю одного модуля вместо пути по умолчанию |
| `AI_REVIEWER_<MOD>_FORCE_JS` | `1` — чистый JS (паритетные прогоны, отладка); другие значения не действуют |

Порядок: `FORCE_JS=1` → JS; иначе `_BIN` или путь по умолчанию; файла нет → JS. Бинарь есть,
но упал (non-zero exit) — ошибка пробрасывается, JS его не подменяет: молчаливая подмена
скрыла бы регрессию Rust-кода. `LLM` и `GITHUB` уходят в JS и тогда, когда `fetch` подменён
(тесты, DI).

`<MOD>` (24 префикса; `<bin>` — он же в нижнем регистре, кроме оговорённых):

| `<MOD>` | Модули |
|---------|--------|
| `ACTIONABILITY`, `DIFF`, `ENGINE`, `EXPORT`, `FINDINGS`, `GITHUB`, `LLM`, `PROMPT`, `REPORT`, `ROUTER`, `SPECIALISTS`, `VERIFIER` | одноимённые `lib/<mod>.mjs` |
| `CONFIG` | `lib/config.mjs`, `lib/profile.mjs` |
| `METRICS` | `lib/metrics.mjs`, `lib/observability.mjs` |
| `JSON_REPAIR` | `lib/json.mjs` |
| `STRUCTURE_{ASSESSMENTS,EVIDENCE,GRAPHIFY,LCOV,MCP,MUTATION,PROTOCOL,TESTS}` | `lib/structure-<x>.mjs` |
| `SCIP` | `lib/structure-scip.mjs`, бинарь `structure_scip` |

Пример: `AI_REVIEWER_JSON_REPAIR_FORCE_JS=1`.
