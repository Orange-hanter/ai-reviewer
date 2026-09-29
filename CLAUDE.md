---
type: standard
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-09-29
last_reviewed: 2026-09-29
---

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Что это

`ai-reviewer` — движок AI код-ревью для PR с правом блокировать merge. Чистый ESM (`.mjs`),
Node ≥ 22, **ноль runtime-зависимостей**: `npm install` не нужен, и добавлять зависимости
нельзя — README держит это как заявленное свойство пакета. Провайдер LLM по умолчанию —
Ollama Cloud через `lib/llm.mjs`.

Весь код, комментарии, документация и commit-сообщения — на русском; префиксы коммитов
conventional (`feat:`, `fix:`, `test:`, `docs:`, `chore:`, `release:`).

## Команды

```bash
npm test          # node --test test/*.test.mjs — юнит + контракты (в т.ч. шаблоны examples/ci/)
npm run eval      # golden fixtures без LLM — гейт post-processing
npm run check     # test + eval + check:rust (cargo build/test + check:parity); нужен cargo;
                  # ровно это гоняет CI (.github/workflows/ci.yml)

node --test test/pr-review.test.mjs                                # один файл
node --test --test-name-pattern="parseDiff: validLines" test/…     # один тест

# Прогон ревью на произвольном диффе, без GitHub (нужен OLLAMA_API_KEY)
git -C /path/to/repo diff origin/main... > /tmp/changes.patch
OLLAMA_API_KEY=… node bin/pr-review.mjs --dry-run --diff /tmp/changes.patch

# Shadow-прогон с LLM на размеченных диффах (медленно: ~5–10 мин на кейс)
OLLAMA_API_KEY=… node bin/pr-review-gold.mjs [--case <id>]

# Structure-контур (вне merge-гейта)
node bin/pr-review-structure.mjs --graph .graphify/graphify-out/graph.json \
  --lcov coverage/lcov.info --facts-out .ai-review/structure/facts.json
```

Протокол смены моделей и промптов: `bin/pr-review-probe-models.mjs` → `npm run eval` →
`pr-review-gold`. Новая конфигурация не мержится без зелёного eval.

## Архитектура: что не видно из одного файла

**Поток прогона** (`pipeline.mjs`, `createReviewPipeline(cfg, deps)` → `run()`):
router (skip / light / full) → `parseDiff` + чанки → контекст (окна ±`REVIEW_CONTEXT_MARGIN`,
по умолчанию 40 строк, вокруг изменённых строк, короткий файл целиком — RFC 07; + blast-radius
через Graphify) → prescan (eslint + ast-grep, детерминированно, до LLM) → engine (MoA:
specialist-advisors → жюри-агрегатор с refutation) → verifier Pass B (отдельная модель) →
FP-фильтр, кластеризация, гейт (`resolveBlockers`) → actionability (inline или свод) →
sticky + inline + Δ-блок в GitHub, JSON/SARIF-артефакт, JSONL observability.

**Цена прогона — предмет проектирования, а не следствие** ([RFC 04](proposals/04-token-economy.md)).
Три места, где состав вызовов решается, а не задан: контекст **добавленного** файла не
прикладывается (его дифф и есть содержимое, `collectAddedPaths`); чанк без исполняемого
файла идёт одним агрегатором без жюри (`lib/chunk-routing.mjs`) — и потому его находки не
блокируют merge; роли, назначенные на одну модель, опрашиваются одним вызовом
(`lib/specialist-merge.mjs`), причём сорванный формат секций НЕ раздаёт один текст трём
ролям — это подделало бы консенсус для refutation. Прежде чем добавлять роль или контекст,
посмотри `efficiencyMetrics` в `run_end`: знаменатель там — находки, доехавшие до человека.

**Инкрементальный прогон** (`lib/incremental.mjs`, `REVIEW_INCREMENTAL`, [RFC 05](proposals/05-incremental-review.md))
не ревьюит заново файлы, чей вход для модели не изменился, и переносит их находки. Перенос
идёт **после** refutation и verifier: обе стадии находка прошла в своём прогоне и принесла
вердикты с собой, а пропусти её через них ещё раз — strict-refutation понизит подтверждённый
блокер до minor только потому, что жюри в этом прогоне по нему не высказывалось. Порядок этих
трёх шагов — не деталь реализации, а то, чем гейт держится при экономии.

**DI вместо моков.** `deps` принимает `github`, `engine`, `prescan`, `graphify`, `die`.
Тесты (`test/pr-review.pipeline.test.mjs`) и gold-раннер подменяют их и гоняют весь `run()`
без сети. Новый внешний эффект заводи через `deps`, а не прямым import внутри стадии.

**Конфиг — один замороженный объект.** `loadReviewConfig(env, cli)` собирает профиль + env +
CLI; глобалов нет. Приоритет: env/CLI → профиль репозитория → дефолты движка. Boolean-env
читай через `envFlag` из `lib/util.mjs`. Каждую новую переменную документируй в
`docs/configuration.md`.

**Профиль — process-wide singleton** (`lib/profile.mjs`), read-only после загрузки. Всё
репозиторно-специфичное (маркеры комментариев, правила проекта для промпта, раскладка
каталогов, scope ast-grep, пути) приходит только оттуда. `MARKERS` в `lib/constants.mjs` —
живые getters, а не снимок: модули импортируются раньше, чем грузится профиль. Смена
`markerNamespace` на живом репозитории рвёт carry-over находок, Δ-блок и harvest 👍/👎.

**Пути двух видов.** Артефакты пакета (ast-grep ruleset, стартовые fp-patterns, fixtures,
schemas) — от корня пакета через `pkgPath()`. Данные потребителя (feedback, observability,
export) — от рабочего каталога через `profile.paths.*`, перебиваемые env.

**Fail-open по умолчанию, fail-closed осознанно.** `bin/pr-review.mjs` ловит любое исключение
и выходит с `0`: инфра-сбой не блокирует merge. Exit `1` — только осознанный, внутри
`runReview`, на подтверждённых блокерах. Исключения: `REVIEW_LLM_REQUIRED=1` и
`REVIEW_VERIFIER_STRICT=1`. Не превращай мягкий сбой в жёсткий без причины.

**Два независимых судьи.** Агрегатор и verifier намеренно вне пула advisors. Блок требует
refutation жюри + вердикта verifier + `confidence ≥ REVIEW_CONFIDENCE_MIN`. Blast-radius
majors (`source === BLAST_FINDING_SOURCE`) — soft: без verifier не блокируют.

**Блокировка и маршрут разведены.** «Блокирует ли» — по severity (`REVIEW_BLOCK_ON`);
«где показывать» — по сумме детерминированных сигналов в `lib/actionability.mjs`. Не
возвращай маршрут по ярлыку severity. `actionabilityScore` пишется в саму находку: его
читают рендер инлайна (объяснение «почему в строке») и auto-triage (поднятую нить бот не
резолвит молча) — менять его смысл, не проверив обоих читателей, нельзя.

**Сходимость между прогонами.** Sticky несёт скрытый футер `<!-- <ns>-data:{…} -->` с
находками, отпечатками кода (`findingFingerprint`) и историей. Код под находкой изменился →
снимаем, остался → переносим не переформулируя (`filterStalePriorFindings`). Отпечаток —
ключ сходимости и для потребителя артефакта, поэтому футер и экспорт обязаны показывать одну
величину: считать его в двух местах по-разному — тихая рассинхронизация.

**Артефакт экспорта версионирован.** `EXPORT_VERSION` растёт и на аддитивных изменениях,
поэтому потребитель обязан сверять «версия известна», а не равенство. Поля описаны в
`docs/configuration.md`.

## Rust-ядро

`rust/` — порт части `lib/*.mjs` (diff, router, findings, engine, llm, prompt, specialists,
verifier, report, github, export, actionability, metrics, config/profile, json, structure-*).
JS-модуль остаётся обёрткой с тем же API: зовёт бинарь из `rust/target/debug/` (JSON на stdin),
а без бинаря работает эталонный JS. `AI_REVIEWER_<MOD>_BIN` — путь к бинарю,
`AI_REVIEWER_<MOD>_FORCE_JS=1` — чистый JS. Оркестрация (`pipeline.mjs`, chunk-routing,
specialist-merge, incremental) — только JS. Меняешь логику портированного модуля — меняй обе
стороны: `rust/parity/*.parity.mjs` (`npm run check:parity`) сверяют собранный бинарь с JS и
без `cargo build` сходятся тривиально.

## Structure-контур

`lib/structure*.mjs`, `bin/pr-review-structure*.mjs`, `schemas/` — отдельный доказательный
контур: facts/assessment/obligations, EvidenceReceipt, провайдеры Graphify/SCIP/LCOV,
mutation/property evidence, MCP-сервер с пином по SHA. Разбор — `docs/structure.md`.

Отдельно от него — ревью-MCP `bin/pr-review-mcp.mjs` (дешёвые стадии + `review_run` как pinned
dry-run MoA, `wait: false` + поллинг по `jobId`, RFC 08–12) и `cursor-plugin/`, который
подключает оба MCP в Cursor. Оба — не гейт.

**Граница, которую нельзя размывать:**

- гейт **не знает** про structure: ни `pipeline.mjs`, ни `lib/*.mjs` (кроме `structure-*`)
  не импортируют structure-модули, и structure ничего не блокирует;
- провайдеры отдают receipts, а не вердикт: статусы `covered|partial|missing|unverifiable`
  считает только детерминированный assessment engine;
- у протокола своя версия (`STRUCTURE_PROTOCOL_VERSION`), не связанная с `EXPORT_VERSION`;
- агрегатное покрытие (LCOV по файлу) никогда не подменяет покрытие сценария.

## Тесты и fixtures

- `node --test` запускает каждый файл в своём процессе — это важно, потому что профиль
  process-wide. Регрессионный набор импортирует `test/helpers/reference-profile.mjs`
  (профиль `profiles/orakul.json`); `test/profile.test.mjs` намеренно его **не** импортирует —
  там проверяется нейтральный профиль. Не смешивай в одном файле.
- Часть тестов выставляет env (`OLLAMA_MODELS`) **до** динамического `import('../index.mjs')` —
  сохраняй этот порядок.
- **Eval fixture** = каталог `fixtures/<id>/` с `meta.json` (`expected`), `diff.patch`,
  `findings.json`, опц. `verifier.json`. Тест `eval harness: все fixtures green` сверяет
  число seed-кейсов (16) — при добавлении поправь счётчик. Каталогов в `fixtures/` больше:
  `fixtures/structure/` — контрактные образцы схем, не eval-кейсы (нет `meta.json` → пропускаются).
- **Gold** (`gold/<id>/`) — диффы для прогона с LLM; падающий кейс — сигнал, не мусор.
- Шаблоны workflow в `examples/ci/` покрыты контрактными тестами в `test/pr-review.eval.test.mjs`.
- FP-паттерны пакета (`feedback/fp-patterns.jsonl`) переносимы между проектами, поэтому
  каждый обязан быть ограничен по путям (`requiresPath`) — иначе проглотит верную находку
  в чужом репозитории. Тест на пересуппрессию держит это.

## Процесс

Решение дороже отката (границы модулей, веса actionability, смена namespace, strict-режим)
фиксируется письменно до кода — если каталог `proposals/` уже существует, то в нём, по
формату из `proposals/README.md`.

Релиз: зелёный `npm run check`, секция в `CHANGELOG.md` (есть `## Unreleased`), версия в
`package.json` и бейдж в README, пин в `examples/consumers/orakul/ai-reviewer-ref`, тег.
Потребитель подключает движок по пину `ops/ai-reviewer-ref`, читаемому из базовой ветки —
код PR никогда не исполняется с секретами прогона. Breaking для потребителя: смена namespace
маркеров, формата футера sticky, контрактов `swarm`/`export`/`structure`.

`.ai-review/` и `.graphify/` — gitignored артефакты локальных прогонов.

## Документация

L0-навигатор — `MASTER.md`. Журнал разработки — `docs/dev/journal.md` (как вести —
`docs/dev/README.md`). Реестр RFC — `proposals/README.md`.

`docs/architecture.md` (поток и решения) · `docs/configuration.md` (все env и поля артефакта) ·
`docs/profiles.md` (схема профиля) · `docs/false-positives.md` (семейства FP и защиты) ·
`docs/review-quality.md` (почему находки не доезжают до строки) · `docs/model-tuning.md` ·
`docs/structure.md` · `docs/swarm.md` · `docs/integration.md` · `docs/benchmark.md` ·
`docs/benchmark-public.md` · `docs/backlog.md` · `cursor-plugin/README.md`. Индекс — `docs/README.md`.
