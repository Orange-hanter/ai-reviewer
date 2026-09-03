# Changelog

## Unreleased

### Исправлено

- **Слияние SCIP с graphify теряло сущности.** Ключ сопоставления был регистронезависимым
  (`путь + метка.toLowerCase()`), поэтому файл с `class User` и `const user` схлопывался:
  SCIP-данные про `User` приписывались `user`, вторая сущность исчезала из фактов, а рёбра
  на неё оставались висеть на несуществующем `observedId`. Теперь строгое совпадение метки
  важнее регистронезависимого, неоднозначный кандидат не сливается вовсе и попадает в
  `warnings`, а из набора выбывает ровно слитая сущность — не все делящие с ней ключ
- **`validateStructureFacts` не проверял ссылочную целостность**: битые `from`/`to` рёбер и
  `entityRef`/`targetRef` контрактов уезжали потребителю молча — валидатор смотрел только на
  массивы, лимиты, пути и fingerprint. Добавлена проверка: ссылка на неизвестную сущность
  теперь отклоняется
- **MCP-сервер не соблюдал собственный `inputSchema`.** Схема объявлена в `tools/list`, но
  аргументы не проверялись: `dependencyDepth` и размер ответа ограничены внутренними
  потолками `buildStructureContext`, а длина `scopeRefs`/`contractRefs` — ничем, при том что
  `matchesScope` перебирает список на каждую сущность. Границы теперь берутся из самой схемы,
  чтобы код и объявленный контракт не разъезжались
- **Страж varint в SCIP смотрел на сдвиг, а не на величину.** Varint, завершившийся на сдвиге
  49, мог выйти за 2^53: число теряло точность, портились низкие биты, и разбор падал с
  ложным диагнозом «truncated» — ошибка указывала не туда, где сломалось. Проверяется сама
  величина через `Number.isSafeInteger`

### Изменено

- `extractTestCasesFromSource` компилирует выражение для контракта один раз, а не на каждую
  пару тест × контракт: на большом репозитории это были миллионы компиляций одного и того же
  шаблона. Поведение не изменилось — флага `g` нет, состояния у выражения не остаётся
- **LCOV приписывал одноимённым функциям чужие хиты.** `FNDA` адресует функцию только по
  имени, а `FN` может объявить это имя в файле дважды (вложенные функции в разных областях —
  обычный JS): обе записи получали один счётчик, непокрытая функция объявлялась `covered` с
  чужими хитами вопреки собственному `DA` той же строки, и файл рапортовал полное покрытие
  функций. Теперь счётчик привязывается по порядку записей — istanbul пишет `FN` и `FNDA`,
  обходя один и тот же `fnMap`, поэтому N-я `FNDA` относится к N-й одноимённой `FN`. Такая
  привязка помечается `attribution: 'order'` и не претендует на `exact`: она опирается на
  поведение писателя, а не на ключ формата. Если числа записей разошлись (типично после
  слияния отчётов, где `FNDA` схлопнуты по имени), привязывать нечего — `hits: null`,
  observation со `status: unresolved` и `confidence: ambiguous`, имя в `warnings`, а
  знаменатель доли считается только по функциям с однозначным ответом
### Добавлено

- **Артефакт экспорта v2** (`EXPORT_VERSION` 1 → 2): в каждой находке `fp`,
  `verifierStatus`, `actionabilityScore`, `actionabilityPromoted`. Потребитель
  дедуплицирует находки по коду и считает стабильность набора, не разбирая SARIF
  (cod-doc ingest). Изменение аддитивное — читатель v1 не ломается, но сверять версию
  нужно на «известна ли», а не на равенство ([configuration.md](docs/configuration.md))

### Исправлено

- **`fp` не доезжал до артефакта**: отпечаток кода считался только внутри `buildSummary`,
  в список для футера sticky. В экспорт уходил тот же массив находок, но без `fp`, и
  потребитель откатывался на дедуп по заголовку — переформулированная находка на той же
  строке считалась новой. Отпечатки проставляет `attachFingerprints` один раз на прогон,
  до ветвления «футер / Δ-блок / артефакт»; покрыто тестом «артефакт и футер показывают
  один отпечаток»
- **`actionabilityScore` был только у поднятых в инлайн** и в артефакте читался как
  «остальные не оценены». Скор и сигналы теперь пишутся каждой маршрутизированной находке,
  а роль «advisory поднята в строку» несёт отдельный флаг `actionabilityPromoted`:
  по нему авто-триаж не резолвит тред и инлайн печатает объяснение «почему в строке»

## 0.2.4 — swarm contract (E22/E23) + graphify enrichment

### Добавлено

- **E22/E23 swarm contract** (`lib/swarm.mjs`): поле `swarm` в JSON-экспорте +
  sidecar `*-swarm.json`; `strong` только при независимо подтверждённых блокерах;
  `suggestedScopeFiles` из blast `affected_files` вне диффа PR ([docs/swarm.md](docs/swarm.md))
- Consumer kit Orakul: пин `v0.2.4`
- **Graphify enrichment**: из `graph.json` — имя caller, `[EXTRACTED|INFERRED]`,
  `cross-community`; god-nodes из `.graphify_analysis.json`; метрики
  `affected_files` / `confidence` / `cross_community`; омонимный seed не
  угадывается (как `resolve_seed`); `REVIEW_GRAPHIFY_DROP_AMBIGUOUS` читается и
  в `emitBlastRadiusFindings`; `BLAST_PATH_EXTS` — единый список расширений
- Env: `REVIEW_GRAPHIFY_DEPTH`, `REVIEW_GRAPHIFY_DROP_AMBIGUOUS`
- FP-паттерны: `softblast-typedef-cosmetic`, `graphify-path-norm-speculation`
  (+ requiresPath); также `graphify-depth-shell-injection`, `blast-source-literal-softblast`,
  `parse-blast-dependents-export-exists`, `costtracker-engine-deps-intentional`;
  round3/4: dash-flag, softblast-boundary, redact-dup, findings-cluster, cross-chunk
  filter/dropAmbiguous, test white-box/fp-import/fixture-secrets, query-ampersand
- Graphify/blast hygiene: `BLAST_PATH_EXTS` в constants; `opts.layout`; markdown-escape;
  god-node только через opts; `git -C` через safe cwd; LLM stream_options-retry без слота
  httpRetries; redact секретов в LLM/obs логах

### Исправлено

- **redactSecretsForLog**: quoted-значения с пробелами; slack/google/AWS token forms;
  без голых `key`/`token` (FP); Basic/Digest Authorization; unquoted до `,;`/EOL;
  truncation `…`; `envFlag` унифицирует boolean env (вкл. DROP_AMBIGUOUS / VERIFIER / …);
  GRAPHIFY_DEPTH clamp 1…5; blast conf unknown 0.7; top-8 по confidence; symbol `/m`;
  escapeMdTicks(dep.file); metrics Set/Map → JSON-friendly; crossCommunity ≥0;
  `BLAST_CONF_*` / `BLAST_FINDINGS_MAX` в `constants.mjs`
- **Blast layer detection**: учитывает `layout.appDir` (`app/server` ↔ `app/client`); парсит `:L42` и привязывает символ/строку к caller
- **Blast majors soft**: без verifier не блокируют merge; soft только по `source === BLAST_FINDING_SOURCE` (`blastCaller` — actionability)
- **LLM `stream_options`**: fail-open — при 400 повтор без `include_usage`
- **Cost**: убран двойной `recordUsage` (llm + engine); `createReviewEngine({ costTracker })` — deprecated noop + warn (`console.warn` без log)
- Pipeline: канон `filterStalePriorFindings`; `filterFindingsFixedInDiff` — deprecated-алиас
- `stripAppDir` нормализует trailing slash у `appDir`; `softConsensusForBlastCluster` вынесен из тела `clusterFindings`

## 0.2.3 — поверхность: AGENTS.md, Walkthrough, prompts

### Добавлено

- **AGENTS.md** как дефолтный источник `repoRules`, если профиль не задал `repoRules`/`repoRulesFile`
- **Walkthrough** в начало sticky из summary агрегатора (без отдельного LLM-вызова)
- **`prompts.*` в профиле**: `advisorExtra` / `specialistExtra` / `aggregatorExtra` / `verifierExtra`

### Изменено

- CI / шаблоны потребителя: `graphifyy` **0.9.31 → 0.9.52**; GitHub Actions —
  `checkout`/`setup-node`/`setup-python` **v6 → v7**, `cache` **v5 → v6**


## 0.2.2 — LLM abstraction, cost, benchmark, verdict quality

### Добавлено

- **`lib/llm.mjs`** — канонический OpenAI-compatible клиент (`createLlmClient`); `lib/ollama.mjs` — thin re-export
- Env-приоритет: `LLM_API_KEY` / `LLM_BASE_URL` с fallback на `OLLAMA_*`; fail-closed без silent provider fallback
- **Cost tracking**: usage из SSE, `createCostTracker`, `REVIEW_PRICE_JSON`, sticky `💰 ~$0.12 · 45k tokens`
- **`lib/benchmark.mjs`** + `pr-review-benchmark` CLI + [docs/benchmark.md](docs/benchmark.md)
- **Blast-radius findings** (`lib/blast-findings.mjs`): cross-layer callers вне диффа → deterministic major (soft: блок merge только после verifier)
- **`REVIEW_STRICT=1`** / `profile.strict`: блок только prescan или verifier-verified; advisory не inline
- **Grounding quotes**: поле `quote` в schema; `REVIEW_REQUIRE_QUOTE=1` / strict — drop critical/major без quote
- `filterFindingsFixedInDiff` — алиас `filterStalePriorFindings` (тот же fingerprint carry-over; имя для adjacent-commit семантики)
- Actionability signal `blastCaller` (weight 2)

### Изменено

- User-facing ошибки LLM говорят «LLM», не только «Ollama»
- `lib/metrics.mjs`: агрегация prompt/completion/total tokens из `llm_call` events
- `pr-review-gold` использует `scoreGoldCase` из benchmark

## 0.2.1 — blast-radius sync + portable gold

Синхронизация с живым движком Orakul (коммиты после выноса v0.2.0) и расширение gold-набора.

### Добавлено

- `collectBlastSymbols` — добор экспортов из полного текста файла, если hunk без `export`
- Graphify: `graph_missing` / `no_symbols_in_diff` без ложных вызовов `affected`
- Стабильность находок (Jaccard по точкам кода) и воспроизводимость вердикта в sticky / JSONL
- Portable gold: clean-controls, absence-claim FP, router-light, regen FP (8 кейсов)
- [docs/backlog.md](docs/backlog.md) — рыночный бэклог как канон приоритетов

### Изменено

- Промпт aggregator/verifier: обязаны опираться на blast-radius callers, не выдумывать зависимости
- Pipeline: сначала `readChangedFiles`, затем blast; события `stability` / `verdict` в observability

## 0.2.0 — вынос движка в самостоятельное решение


Первый релиз, в котором пакет не привязан к конкретному репозиторию. Движок синхронизирован
с рабочей версией потребителя (август 2026) и расцеплен с ней через профиль проекта.

### Добавлено

- **Профиль проекта** (`lib/profile.mjs`, [docs/profiles.md](docs/profiles.md)): маркеры
  комментариев, правила репозитория для промпта, раскладка каталогов для prescan, scope и
  allowlist ast-grep правил, пути feedback/observability/export — в одном JSON у потребителя.
  Источник: `REVIEW_PROFILE` → явное назначение → `.ai-reviewer.json` в корне → нейтральный
  дефолт. Битый профиль не роняет ревью (fail-open + строка в лог).
- **Маршрут находки по доказуемости** (`lib/actionability.mjs`): inline или свод решает сумма
  детерминированных сигналов (prescan, verifier, повтор, консенсус, чувствительная зона),
  а не ярлык severity. Пороги — `REVIEW_ACTIONABILITY_MIN`, `REVIEW_MAX_ADVISORY_INLINE`.
- **Δ-блок и история прогонов в sticky**: что появилось и что ушло с прошлого прогона;
  отпечаток кода у перенесённой находки (точный фикс снимает находку).
- **Метрики прогонов** (`lib/metrics.mjs`, `pr-review-metrics`): сводка по observability-логу.
- **Gold-набор** (`gold/`, `pr-review-gold`): shadow-прогон полного пайплайна с LLM на
  размеченных TP/FP диффах.
- Новые CLI: `pr-review-metrics`, `pr-review-gold`, `pr-review-probe-models`.
- Шаблоны workflow потребителя — [`examples/ci/`](examples/ci/) (ревью, harvest, promote, метрики),
  все на пиновом чекауте движка вместо копирования кода из базовой ветки.
- `test/profile.test.mjs` — поведение нейтрального профиля и загрузка чужого.

### Изменено

- Пакет переименован `@orakul/ai-reviewer` → **`ai-reviewer`**; дефолтные маркеры
  `<!-- ai-review -->`, SARIF `ai-review/<severity>`, файлы экспорта `ai-review.json` /
  `ai-review.sarif.json`. Прежние имена сохраняются профилем потребителя.
- Из движка убраны project-specific умолчания: пути `ops/pr-review/*` и `.graphify/*`,
  правила репозитория в промпте, allowlist `app/server/net.ts`, scope ast-grep правил,
  раскладка `app/<слой>` в prescan. Всё это теперь приходит из профиля.
- Пути данных по умолчанию — `.ai-review/*` в корне репозитория-потребителя.
- Fixtures и FP-паттерны движка резолвятся от корня пакета, а не от каталога проекта.
- Документация переписана под самостоятельный продукт; журнал FP конкретного проекта из
  пакета убран (он живёт у потребителя), в `docs/false-positives.md` осталось переносимое:
  семейства FP, встроенные защиты, петля обучения.

### Проверки

- `npm test` — 203 теста (юнит, контракты пайплайна, шаблоны workflow).
- `npm run eval` — 16 golden fixtures, гейт post-processing зелёный.
- Регрессионный набор гоняется под reference-профилем `profiles/orakul.json`: движок под ним
  ведёт себя ровно так же, как до выноса в отдельный пакет.

## 0.1.0 — первый вынос кода (2026-07-12)

Снимок движка `ops/pr-review/` первого потребителя: pipeline, MoA/consensus, prescan,
eval fixtures, feedback loop. Интеграция обратно в потребителя не была доведена.
