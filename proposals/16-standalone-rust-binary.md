# 16 — Standalone Rust-бинарь: завершение порта и cutover

> Категория: 🔵 Архитектура · Риск: высокий · Зависимости: нет (гейт и structure переезжают вместе)

## Контекст

Порт движка на Rust идёт снизу вверх с Фазы 1 и дошёл до середины: листья
(`diff`, `json`, `findings`, `report`, `github`, `llm`, `engine`, весь `structure-*`)
уже на Rust, а ствол — pipeline, prescan, graphify, feedback, observability, все CLI —
на JS. Промежуточное состояние держится обёртками: каждый портированный `lib/*.mjs`
спавнит одноразовый бинарь на операцию и несёт рядом полную JS-копию как fallback.

Это состояние **дороже обоих концов**. Обёртка `execFileSync` на операцию отдаёт весь
выигрыш Rust процессам; JS-копия обязана поддерживаться синхронно с Rust, а
`test/rust-parity-guard.test.mjs:1-15` прямо признаёт, что удержать её нечем:
`npm test` идёт до `cargo build`, поэтому юнит-тесты видят именно fallback, и правка,
внесённая только в JS-копию, проходит CI зелёной и остаётся нерабочей на машине, где
бинарь собран. Каждый следующий портированный модуль добавляет ещё одну такую пару.

Владелец решил не оставлять миграцию на полпути: конечное состояние итерации — **полный
cutover**. Один бинарь `pr-review` с подкомандами, JS-ядро удалено, потребитель качает
prebuilt-бинарь по пину вместо чекаута репозитория. RFC фиксирует то, что дороже отката:
границу «что переезжает и что остаётся на JS», формат пина и артефактов релиза, и то, что
именно порт обязан сохранить байт в байт.

## Текущее состояние

Сверено по коду в worktree `worktree-rfc-04-standalone-rust` (= `main` b905ec3), не по
описанию плана. Расхождения с планом отмечены явно. Подразделы ниже — снимок на момент
написания RFC; что изменилось с тех пор — в первом подразделе.

### Состояние ветки после слияния `main` (98753e8)

**Сделано** (закоммичено в ветке): T01–T13, T15, T16. T09 — golden-harness: генератор
`rust/parity/pipeline-golden-generate.mjs` и 29 эталонных кейсов в
`rust/parity/golden-fixtures/` (28 исходных + `fixture-fp-absent-path-outside-diff` из
`main`), перегенерированы от JS после слияния. Тест `rust/tests/pipeline_golden.rs`
по-прежнему под `#[ignore]` до T25.

**Дрейф JS → Rust после слияния.** `main` после b905ec3 менял и сами портированные модули.
Часть правок пришла в `main` уже с Rust-стороной (`config`, `prompt`, `report`, `export`,
`metrics`, `actionability` RFC 06, `specialists` — ростер GLM 5.3). Остальное перенесено
коммитом SYNC:

| Модуль | Что поменялось в JS | Rust |
|---|---|---|
| `constants` | дефолтные модели `glm-5.3`/`glm-5.3-flash`, fallback `deepseek-v4.1-flash`; `VERDICT_RANK` удалён | тест сверки дефолтов, `VERDICT_RANK` удалён |
| `llm` | таймаут запроса 420 с; effort первой попытки `chatJson` из `aggregatorEffort`; `onUsage` у `chatText`; `refMaxTokens` 6000 | `LlmConfig::aggregator_effort`, `ChatTextOpts::on_usage` (`UsageCallback`) |
| `engine` | агрегат без `verdict`; `prompt_chars` в `llm_call`; токены prose-вызовов через `onUsage` | `ModelReview::verdict` удалён, `messages_chars`/`prompt_meta`, usage в `track_text` |
| `verifier` | `prompt_chars` и usage в событии; `symbolCards` во входе | события — да; `symbolCards` — T38 |
| `cost` | `efficiencyMetrics` | `efficiency_metrics` |
| `util` | `nonNegEnv` | `non_neg_env` |
| `profile` | `paths.incrementalState` | `ProfilePaths::incremental_state` (дефолт — в `config.rs`) |
| `diff` | `collectAddedPaths`, `readChangedFiles({addedPaths})` | `collect_added_paths`; чтение файлов — T35 |
| `graphify` | `graphIndex` в результате blast | `BlastRadius::graph_index` (`#[serde(skip)]`) |
| `findings`/`eval` | FP-семейство 7 (`repoIndex`), `repo-files.txt` фикстур | T34 |
| `engine` | маршрутизация чанков, слияние ролей, раскладка shared-prefix, summary из кодового чанка (RFC 04) | T36, T37 |

**Новые JS-модули `main`, которых RFC не покрывал**, — задачи T34–T42 в «Оценке».
Оркестрационные (`incremental`, стадии `pipeline.mjs`) — волна 4 рядом с T26.
RFC 13 (судья гейта) в `lib/` ещё не реализован (только план `gate-judge`) — задачи на
порт нет, пока нет JS-эталона. `bin/pr-review-scout.mjs`, `bin/pr-review-probe-cache.mjs`,
`bin/pr-review-symbol-cards-probe.mjs` — dev-тулы, под non-goal «не переносим dev-тулы».

### Паттерн обёртки един для всех портированных модулей

Эталон — [`lib/router.mjs:38-58`](../lib/router.mjs):

```js
const DEFAULT_ROUTER_BIN = resolve(__dir, '../rust/target/debug/router');
function resolveRouterBin() {
  if (process.env.AI_REVIEWER_ROUTER_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_ROUTER_BIN || '').trim() || DEFAULT_ROUTER_BIN;
  return existsSync(bin) ? bin : null;
}
function runRust(op, args) { /* execFileSync, JSON {"op", ...args} на stdin */ }
```

Каждая публичная функция — диспетчер: `runRust(...)` вернул не-null → результат Rust,
иначе — JS-копия (`lib/router.mjs:65-69`). Контракт деградации везде одинаков: **бинаря
нет** — молча JS; **бинарь есть и упал** — ошибка наружу.

### Что уже портировано

23 модуля `lib/*.mjs` на HEAD содержат `*_FORCE_JS` (`grep -l FORCE_JS lib/*.mjs`):
`actionability config diff engine findings github json llm profile prompt report router
specialists verifier` + 9 `structure-*` (`assessments evidence graphify lcov mcp mutation
protocol scip tests`). В `rust/src/bin/` на HEAD — 23 бинаря, из них `eval.rs` —
стаб; парных паритетных прогонов — 20 (`rust/parity/*.parity.mjs`).

> **Расхождение с планом.** В рабочем дереве лежат незакоммиченные файлы задачи T02
> (`lib/{export,metrics,observability}.mjs`, `rust/src/{export,metrics,triage}.rs`,
> `rust/src/bin/{export,metrics}.rs`, две parity-обёртки), поэтому фактическое число
> обёрток — 26, бинарей — 25, паритетных прогонов — 22, а `pub mod triage;` в
> [`rust/src/lib.rs:35`](../rust/src/lib.rs) уже добавлен. План перечисляет `observability.mjs` в непортированном стволе — это верно
> для HEAD и уже неверно для дерева: там `countBySeverity`/`slimFindings` ходят в бинарь
> `metrics`, а `createObservability` остался JS.

### Что осталось в JS внутри портированных файлов

Это не долг, а осознанная граница — фабрики с DI и внутрипроцессным состоянием:

- [`lib/engine.mjs:160`](../lib/engine.mjs) `createReviewEngine` — оркестрация MoA целиком
  на JS; в Rust ушли три батч-операции (`planChunks`, `demote`, `aggregate`,
  `lib/engine.mjs:18-22`). Причина названа в шапке файла: DI-моки подменяют `ollama`/`prompt`
  внутри процесса Node, дедлайн бюджета — `Date.now()` оркестратора,
  fail-open-семантика завязана на то, что ошибки моделей возвращаются, а не бросаются.
- [`lib/llm.mjs:107`](../lib/llm.mjs) `createLlmClient` — адаптивные семафоры (сквозное
  состояние между запросами; бинарь сообщает 429 флагом `throttled`, обёртка режет лимиты
  AIMD), JSON-mode graceful fallback, costTracker, observability. Плюс жёсткое условие
  деградации: `globalThis.fetch !== NATIVE_FETCH` → всегда JS (`lib/llm.mjs:47-53`).
- [`lib/github.mjs:55`](../lib/github.mjs) `createGithubClient` — то же условие по
  инжектированному `fetch` (`lib/github.mjs:62-68`); Rust вызывается пометодно.
- [`lib/profile.mjs:230-245`](../lib/profile.mjs) — `getProfile`/`setProfile`/`resetProfile`
  над модульным `let active`: process-wide singleton, в комментарии помечен «не портировано
  в Rust намеренно: fs + process-wide singleton». Его читают живые getters
  [`lib/constants.mjs:56-62`](../lib/constants.mjs) `MARKERS`.

### Непортированный ствол

- [`pipeline.mjs`](../pipeline.mjs), 681 строка: `createReviewPipeline(cfg, deps)` на
  `pipeline.mjs:226`, дефолты `deps` — `pipeline.mjs:227-237`, тело прогона
  `async function run()` — `pipeline.mjs:245`, гейт `runGateResolution` — `pipeline.mjs:211`,
  CLI-вход `runReview` — `pipeline.mjs:672`.
- `lib/*.mjs` без обёртки: `feedback.mjs` (427), `graphify.mjs` (401), `structure.mjs` (378),
  `structure-{context,drift,identity,export}.mjs`, `prescan.mjs` (193),
  `prescan-{runner,shared}.mjs`, `astgrep.mjs` (175), `blast-findings.mjs` (185),
  `triage.mjs` (145), `swarm.mjs` (114), `cost.mjs` (93), `eval.mjs` (195),
  `util.mjs` (207), `constants.mjs` (104), `fp-patterns.mjs` (101),
  `feedback-schema.mjs` (89), `package-root.mjs` (9), `ollama.mjs` (2).
- Все 11 `bin/*.mjs` (1232 строки суммарно) и 4 скрипта `feedback/*.mjs`.

### CI гоняет JS до сборки Rust

[`.github/workflows/ci.yml:23-37`](../.github/workflows/ci.yml): `npm test` (23) →
`npm run eval` (27) → `dtolnay/rust-toolchain` (29) → `cargo build` (33) → `cargo test` (37).
Следствие уже описано выше: юнит-тесты движка всегда видят JS-fallback, а не Rust.

### Паритетные прогоны нигде не запускаются

`rust/parity/*.parity.mjs` (20 файлов на HEAD, 22 после T02) не упомянуты ни в
[`package.json:46-52`](../package.json) (`test`, `eval`, `check`, `check:rust`,
`benchmark:public`), ни в `.github/workflows/ci.yml`. Их запускает человек вручную.
Замена — грубый текстовый страж `test/rust-parity-guard.test.mjs`, который сверяет
исходники по regex и покрывает ровно два модуля (`router`, `structure-lcov`).

### Контракт с потребителем — чекаут репозитория и `node`

[`examples/ci/pr-review.yml:110-133`](../examples/ci/pr-review.yml): шаг `Read pinned engine
ref` читает `ops/ai-reviewer-ref` **из базовой ветки** (`git show "origin/$BASE_REF:…"`,
строка 117), валидирует `^[A-Za-z0-9_./-]+$`, пустой пин → ревью пропускается (exit 0,
строки 118-122); затем `actions/checkout` репозитория `Orange-hanter/ai-reviewer` по этому
ref в `.ai-reviewer-engine` (127-133) и `run: node .ai-reviewer-engine/bin/pr-review.mjs`
(183). Тот же паттерн в трёх остальных шаблонах — всего 6 вызовов
`node .ai-reviewer-engine/…` в `examples/ci/`. Пин сегодня — голая строка тега:
`examples/consumers/orakul/ai-reviewer-ref` = `v0.2.4`,
[`docs/integration.md:11`](../docs/integration.md) — «одна строка: тег (v0.2.0) или полный
commit SHA».

### Regex-диалект: crate `regex` не хватает в двух местах

- `feedback/fp-patterns.jsonl` — 51 паттерн, из них **2** используют lookahead:
  `e2e-in-diff` (`(?=\s|$)`, `(?=\s)`) и `file-absent-in-diff` (`(?=\s)`). Это данные
  потребителя, а не наш код: любой репозиторий может положить в `requiresPath`/`regex`
  выражение с lookahead, и падение на компиляции = проглоченный FP-фильтр.
- [`lib/util.mjs:187-206`](../lib/util.mjs) `redactSecretsForLog` — квотированные значения
  ловятся выражением `(['"])(?:\\.|(?!\2).)*\2`, то есть backreference **и** negative
  lookahead одновременно. Crate `regex` не умеет ни того, ни другого.

### Ассеты резолвятся от `__dirname` пакета

[`lib/astgrep.mjs:18-19`](../lib/astgrep.mjs) `DEFAULT_ASTGREP_CONFIG =
resolve(__dir, '../ast-grep/sgconfig.yml')`;
[`lib/fp-patterns.mjs:7-8`](../lib/fp-patterns.mjs) `DEFAULT_FP_PATTERNS_PATH =
resolve(__dir, '../feedback/fp-patterns.jsonl')`; общий хелпер —
[`lib/package-root.mjs:5-9`](../lib/package-root.mjs) `pkgPath()`. Все три предполагают,
что рядом с исполняемым кодом лежит дерево пакета. У одиночного бинаря его нет.

### Публичная поверхность

[`index.mjs`](../index.mjs) — 99 строк, 43 `export`-блока; `package.json:24-31` объявляет
`exports` на `.`, `./pipeline`, `./lib/*`, `./feedback/*`, `./bin/*`, `./profiles/*` и 11
`bin`-точек (`package.json:11-23`). [`README.md:40`](../README.md) продаёт «Ноль
зависимостей. Node ≥ 22 и всё: `npm install` не нужен», `README.md:158` — «зависимостей
нет, npm install не нужен». Cutover меняет это утверждение по существу: зависимостей у
потребителя становится ещё меньше (не нужен даже Node), но и `cargo` появляется у нас.

### Что порт обязан сохранить неизменным

`EXPORT_VERSION = 2` ([`lib/export.mjs:78`](../lib/export.mjs)),
`STRUCTURE_PROTOCOL_VERSION = 1` (`lib/structure-protocol.mjs:79`), формат футера sticky
`<!-- ${MARKERS.dataKey}${footerJson} -->` с экранированием `-->`/`<!--`
([`lib/report.mjs:683-684`](../lib/report.mjs)) и разбором «последнее вхождение»
(`lib/constants.mjs:76-90`), namespace маркеров из профиля. Это контракты сходимости
между прогонами и с потребителем артефакта.

## Предложение

Довести порт до конца и выключить JS-ядро. Десять решений ниже — то, что фиксируется до
кода; остальное — механика порта.

### 1. Layout: один пакет, один бинарь, девять подкоманд

`rust/` остаётся единственным пакетом `ai-reviewer`. `src/lib.rs` — библиотека,
`[[bin]] pr-review` = `src/main.rs` + clap-подкоманды:

```
pr-review review | structure | structure-mcp | eval | metrics
          | feedback | feedback-batch | feedback-migrate | feedback-promote
```

Каждая подкоманда — `src/cli/<name>.rs::run(args) -> anyhow::Result<i32>`; `main()`
возвращает код из `run` и больше нигде `process::exit` не зовёт. 25 JSON-stdin бинарей и
22 `rust/parity/*.parity.mjs` (счёт после T02) — временный мост: живут до волны 6 и удаляются в cutover.

### 2. Трейты DI вместо `deps`-объекта

`src/traits.rs` — зеркало `pipeline.mjs:227-237` и фейков
`test/pr-review.pipeline.test.mjs:41-75`. Четыре трейта:

| Трейт | Методы |
|---|---|
| `GithubApi` | `gh`, `load_diff`, `read_prior_findings`, `read_prior_sticky_data`, `upsert_sticky`, `delete_stale_inline`, `post_review`, `list_inline_comments`, `list_issue_labels`, `read_sticky_review`, `list_pull_comment_reactions`, `list_unresolved_review_threads`, `reply_review_thread`, `resolve_review_thread` (список — `lib/github.mjs:271-275`) |
| `EngineApi` | `review_all_models`, `verify_findings` (`lib/engine.mjs:412`) |
| `PrescanApi` | `run` (`lib/prescan-runner.mjs:35-40`) |
| `GraphifyApi` | `build_blast_radius` (`lib/graphify.mjs:400`) |

Фейки — `src/testutil.rs` (публичный модуль, не `#[cfg(test)]`: их зовёт и golden-harness
в `rust/tests/`): `FakeGithub`, `FakeEngine`, `FakePrescan`, `FakeGraphify` с журналом
вызовов, повторяющим `calls.order` из JS-теста (`delete` → `post` → `sticky`).

Стиль — **блокирующий, без tokio**. Параллелизм MoA — `std::thread::scope` +
`AdaptiveSemaphore` (порт `createAdaptiveSemaphore`, `lib/util.mjs:134-164`, вместе с
логикой AIMD по 429 из `lib/llm.mjs`). `reqwest` уже подключён с `blocking`
(`rust/Cargo.toml:31`), async-рантайм в дерево не приходит.

### 3. Профиль — значение, а не singleton

`Profile::resolve(env, cwd) -> Profile` собирается один раз в `main()` и передаётся по
`&Profile` вниз. `getProfile()`/`setProfile()`/`resetProfile()` не портируются вовсе;
`MARKERS` заменяется методом `Profile::markers() -> &Markers`. Практическое следствие,
ради которого это делается: два профиля в одном процессе перестают конфликтовать, и
`node --test`-правило «файл на процесс» (CLAUDE.md, раздел «Тесты и fixtures») перестаёт
быть обязательным для Rust-тестов.

### 4. Regex: `fancy-regex` ровно там, где нужен

`fancy-regex` (backtracking, поддерживает lookaround и backreferences) — **только** на
данных потребителя, где мы не контролируем диалект:

- `fp-patterns`: поля `regex` и `requiresPath`;
- профиль: `lintable`, `e2eSpec`, `logic`, `sensitive`;
- `astgrep.scopes`.

`redactSecretsForLog` (`lib/util.mjs:187-206`) **переписывается вручную** без regex:
конструкция `(['"])(?:\\.|(?!\2).)*\2` — это парсер квотированной строки, и посимвольный
скан честнее, чем backtracking по логу произвольного размера. Весь остальной код движка
остаётся на crate `regex` (линейное время — свойство, которое мы не хотим терять на
входе, приходящем из LLM).

Заодно убираются два костыля, существовавшие ровно из-за раздвоения реализаций:
«soft mode» в `actionability` и JS-only FP-путь в `findings`.

### 5. Ассеты — внутрь бинаря

- `include_dir!("$CARGO_MANIFEST_DIR/../ast-grep")` — весь ruleset (`sgconfig.yml` + правила);
  при запуске распаковывается в `std::env::temp_dir()`, потому что читать его будет внешний
  процесс `ast-grep`, а не мы;
- `include_str!("$CARGO_MANIFEST_DIR/../feedback/fp-patterns.jsonl")` — стартовые
  FP-паттерны (51 штука).

Env-override сохраняются как есть (`REVIEW_ASTGREP_CONFIG`, путь fp-patterns из профиля):
эмбед — это дефолт, а не запрет на подмену. `fixtures/` и `gold/` **не** эмбедятся — они
dev-only и в бинаре потребителя не нужны.

### 6. Внешние инструменты — по-прежнему спавн

`eslint` (`npx eslint` / `node_modules/.bin/eslint`), `ast-grep`, `graphify`, `git` —
спавн процессов ровно как сегодня, с тем же fail-open при отсутствии. Мы не становимся
их хостом.

### 7. Паритет: юнит-тесты + сквозной golden-harness

Для каждого нового модуля — юнит-тесты, портированные из соответствующего `test/*.test.mjs`.
Сверх этого — `rust/tests/pipeline_golden.rs` на фикстурах, **сгенерированных JS-пайплайном
до его удаления**: `rust/parity/pipeline-golden-generate.mjs` гоняет `createReviewPipeline`
с фейками из `test/pr-review.pipeline.test.mjs` и складывает
`rust/parity/golden-fixtures/<id>/{diff.patch,fake-reviews.json,env.json,expected/{export.json,sticky.md,exit-code.txt}}`.
Сверка — байт в байт по всем трём выходам. Это единственная проверка на `pipeline.rs`, и
она обязана существовать раньше него.

### 8. Cutover: что удаляется, что выживает

Удаляются: `pipeline.mjs`, `index.mjs`, все `lib/*.mjs` **кроме** `gold-fetch.mjs` и
`benchmark.mjs`, потребительские `bin/*.mjs`, `feedback/*.mjs` (4 скрипта),
`rust/src/bin/*`, `rust/parity/*`, JS-тесты движка.

Выживают: 4 dev-тула — `bin/pr-review-gold.mjs` (переписывается на спавн
`pr-review review --dry-run`), `bin/pr-review-gold-harvest.mjs`,
`bin/pr-review-probe-models.mjs`, `bin/pr-review-benchmark.mjs`; их библиотеки
`lib/gold-fetch.mjs` и `lib/benchmark.mjs`; новый `test/ci-templates.test.mjs` —
контрактные тесты шаблонов `examples/ci/`, вынесенные из `test/pr-review.eval.test.mjs`.

`package.json`: `bin` — 4 dev-тула, `exports` удаляется целиком (см. решение 10).

### 9. Релиз: prebuilt-бинари и пин с хэшем

`.github/workflows/release.yml` по тегу `v*`: матрица `{x86_64, aarch64} × {linux, macos}`,
`reqwest` переводится на `rustls-tls` (кросс-сборка с OpenSSL — отдельный ад, см. риски).
Артефакты — `pr-review-<target-triple>.tar.gz` (`pr-review-x86_64-unknown-linux-gnu.tar.gz`,
`pr-review-aarch64-unknown-linux-gnu.tar.gz`, `pr-review-x86_64-apple-darwin.tar.gz`,
`pr-review-aarch64-apple-darwin.tar.gz`) плюс `SHA256SUMS`.

**Новый формат пина** `ops/ai-reviewer-ref` — одна строка из двух полей:

```
v0.3.0 sha256:<sha256 файла SHA256SUMS>
```

Тег говорит, что качать; хэш `SHA256SUMS` закрывает цепочку доверия целиком (файл сумм
проверяется по пину, каждый бинарь — по файлу сумм). Пин по-прежнему читается **из базовой
ветки**, поэтому свойство «код PR не исполняется с секретами прогона» сохраняется — и
усиливается: теперь и подменённый релизный артефакт не проходит.

Шаблон потребителя: скачать `SHA256SUMS` → сверить с пином → скачать бинарь под текущий
triple → сверить с `SHA256SUMS` → `chmod +x` → `./pr-review review`. **Любой сбой на этом
пути — fail-open: `exit 0` и понятное сообщение в лог, без блокировки merge.** Это ровно
та же семантика, что у сегодняшнего пустого пина (`examples/ci/pr-review.yml:118-122`) и у
`bin/pr-review.mjs:11-14`: недоступность инфраструктуры ревью не блокирует чужой merge.

### 10. Программный API удаляется

`index.mjs` и `exports` из `package.json` уходят. Интерфейс движка — CLI и JSON-артефакты
(`export.json`, SARIF, structure-facts, observability JSONL). Причина прямая: у одиночного
бинаря нет модульной поверхности, а поддерживать её через N-API — отдельный продукт
(см. non-goals). Потребителей `import 'ai-reviewer'` вне репозитория нет.

## Миграция

Работа разложена на семь волн; внутри волны задачи параллельны, между волнами —
последовательны. Владение файлами эксклюзивно: `rust/src/main.rs`, `rust/src/cli/mod.rs`,
список `pub mod` в `rust/src/lib.rs` и `rust/Cargo.toml` правятся только в T03, остальные
задачи трогают исключительно свои модули.

| Волна | Задачи | Параллельность | Что появляется |
|---|---|---|---|
| 0 | T01 RFC, T02 перенос незакоммиченного, T03 скелет | T01 ‖ T02 → T03 | `[[bin]] pr-review`, 9 заглушек подкоманд, пре-сеянные `pub mod` |
| 1 | T04 util, T05 profile+constants, T06 concurrency, T07 fp_patterns/findings, T08 traits+testutil, T09 golden-скаффолд | все ‖ | фундамент: `Profile`, `AdaptiveSemaphore`, `fancy-regex`, замороженные сигнатуры трейтов, golden-фикстуры |
| 2 | T10 llm, T11 github, T12 engine, T13 graphify_client, T14 astgrep+prescan, T15 blast/cost/swarm, T16 observability, T17 feedback, T34 absent_path, T35 context_window, T36 chunk_routing+specialist_merge+prompt_layout, T37 engine RFC 04, T38 symbol_cards | ‖ (T12 после T10; T37 после T36; T38 после T13) | все реализации трейтов, детерминированный prescan, экономия токенов RFC 04/07/14 |
| 3 | T18 structure_identity+context, T19 structure_drift, T20 structure_export, T21 structure glue, T22 cli structure, T40 mcp_stdio, T23 cli structure-mcp | T18‖T19‖T20‖T40 → T23; → T21 → T22 | structure-контур целиком; **независим от волн 1–2** |
| 4 | T24 triage, T39 incremental, T26 pipeline, T28 eval, T29 metrics | T24‖T28‖T29‖T39 → T26 | стадии прогона, `PipelineOutcome` вместо `die`/`exit` |
| 5 | T25 golden финал, T27 cli review, T30 cli feedback-семейство, T41 cli review-mcp | T27‖T30‖T41 → T25 | все подкоманды работают (9 + `review-mcp`) |
| 6 | T31 шаблоны CI+docs+package.json, T32 release.yml, T42 dev-тулы RFC 15, T33 cutover | T31‖T32‖T42 → T33 | удаление JS-ядра |

**Порядок cutover нарушать нельзя:**

1. Golden-фикстуры (T09) генерируются **JS-пайплайном** — до того, как он удалён. Без них
   `pipeline.rs` не с чем сверять.
2. `cargo test --test pipeline_golden` зелёный (T25) — все 16 seed-фикстур плюс
   router skip/light и telegram, `#[ignore]` снят.
3. `release.yml` (T32) прогнан **реально** через `workflow_dispatch` и четыре артефакта
   собрались — до T33. Обнаружить сломанную кросс-сборку после удаления JS означает
   остаться без работающего движка вообще.
4. Только затем T33: удаления, переписывание `bin/pr-review-gold.mjs` на спавн бинаря,
   `ci.yml` меняет порядок на `cargo build` → `cargo test` → `npm test` (остаток).

**Для потребителя** переход — одна правка `ops/ai-reviewer-ref` (тег + `sha256:`) и замена
шаблона workflow. Пока пин старого формата — старый шаблон и старый движок по чекауту;
формат пина различим по наличию второго поля, так что оба живут параллельно ровно столько,
сколько нужно потребителю.

## Риски и что не делаем

**Риск 1: `pipeline.rs` (T26) — самая большая последовательная машина в порте.** 26 стадий,
единственная проверка — golden-harness, который идёт *после* неё (T25). Смягчение
структурное, а не «внимательнее писать»: T09 генерирует фикстуры заранее (волна 1),
T08 замораживает сигнатуры трейтов до того, как их начнут реализовывать, и T26 не может
стартовать раньше, чем оба закрыты.

**Риск 2: кросс-сборка aarch64-linux с TLS (T32).** Классическое место, где сборка ломается
о системный OpenSSL. Смягчение: `rustls-tls` вместо native-tls, `cross` для линуксовых
таргетов, и — главное — реальный прогон workflow **до** cutover, а не после.

**Риск 3: prescan/astgrep (T14) легко превращается из fail-open в fail-closed.** Сегодня
отсутствие `eslint`/`ast-grep` даёт пустой prescan, а не падение; в Rust `?` на
`Command::spawn` даёт ровно обратное поведение одним символом. Плюс embedded ruleset должен
остаться читаемым **внешним** `ast-grep` после распаковки. Смягчение: фикстуры
`prescan-astgrep-xff` и `prescan-eslint-layer-block` в обязательном DoD и интеграционный
тест с реальным бинарём (`#[ignore]`, если его нет в PATH).

**Чего не делаем:**

- **Не эмбедим `ast-grep` как библиотеку.** Он остаётся внешним процессом. Втягивание
  `ast_grep_core` привязало бы нас к его версии парсеров и утащило бы tree-sitter-грамматики
  в дерево — ради экономии одного `spawn`.
- **Не переносим dev-тулы на Rust.** `gold`, `gold-harvest`, `probe-models`, `benchmark`
  остаются JS поверх JSON-вывода бинаря: они гоняются человеком на своей машине, их цена
  запуска не важна, а скорость правки — важна.
- **Не делаем N-API-обвязку.** Программный API удаляется (решение 10), а не переезжает в
  нативный модуль: поддержка npm-пакета с prebuilt `.node` под четыре таргета — отдельный
  продукт со своим релизным циклом.
- **Не меняем `EXPORT_VERSION`, `STRUCTURE_PROTOCOL_VERSION`, формат футера sticky и
  namespace маркеров.** Порт обязан воспроизвести их **байт в байт**: футер несёт отпечатки
  находок, по которым считается сходимость между прогонами, а `EXPORT_VERSION` потребитель
  сверяет на «версия известна». Смена реализации не является поводом двинуть контракт —
  если порт не может повторить байт, это дефект порта, а не повод поднять версию.
- **Не добавляем tokio.** Весь ввод-вывод блокирующий, параллелизм — `std::thread::scope`.
  Один async-рантайм ради HTTP-клиента протёк бы по всем сигнатурам сразу.
- **Не трогаем границу structure ↔ гейт.** `structure*`-модули по-прежнему ничего не
  блокируют и не импортируются из `pipeline`; контрактный тест
  `test/structure-boundary.test.mjs` переезжает в Rust как проверка графа модулей.

## Оценка

42 задачи (T34–T42 добавлены после слияния `main` 98753e8). DoD каждой — команда, а не
суждение. ✅ — закоммичено в ветке.

**Волна 0**

| # | Суть | DoD |
|---|---|---|
| ✅ T01 | RFC 16 + строка в `proposals/README.md` | чек-лист «Готов к декомпозиции» закрыт |
| ✅ T02 | перенос незакоммиченных `export`/`metrics`/`triage` из основного чекаута | `cargo build && cargo test`; `node rust/parity/{export,metrics}.parity.mjs`; `npm test` |
| ✅ T03 | `[[bin]] pr-review`, deps (`fancy-regex`, `include_dir`, `tempfile`, `assert_cmd`, `reqwest`+rustls), 9 заглушек `src/cli/*.rs`, пре-сев `pub mod` | `cargo build`; `pr-review --help` показывает 9 подкоманд; `cargo test` |

**Волна 1**

| # | Суть | DoD |
|---|---|---|
| ✅ T04 | `util.rs` ← `lib/util.mjs` (13 функций, `redact_secrets_for_log` вручную) | `cargo test util::` |
| ✅ T05 | `profile.rs` + `constants.rs`: `Profile::resolve/markers`, поверх готового `config.rs` | `cargo test profile:: constants::` + тест независимости двух профилей в процессе |
| ✅ T06 | `concurrency.rs`: `AdaptiveSemaphore`, `run_limited` на `std::thread::scope` | `cargo test concurrency::` |
| ✅ T07 | `fp_patterns.rs` на `fancy_regex` + FP-фильтр в `findings.rs`, снятие soft-mode | `cargo test fp_patterns:: findings::`; все 51 паттерн компилируются; корпус-сравнение с JS на `fixtures/*/diff.patch` |
| ✅ T08 | `traits.rs` (4 трейта) + `testutil.rs` (4 фейка с журналом) | `cargo build`; `cargo doc --no-deps` без warnings |
| ✅ T09 | генератор golden-фикстур на JS + `rust/tests/pipeline_golden.rs` под `#[ignore]` | ≥ 3 фикстуры сгенерированы; тест компилируется |

**Волна 2**

| # | Суть | DoD |
|---|---|---|
| ✅ T10 | `llm.rs`: `RealLlmClient` — семафоры, retry-каскад `chatJson`/`chatText` | `cargo test llm::` |
| ✅ T11 | `github.rs`: `RealGithubClient: GithubApi` (14 методов) | `cargo test github::`, кейсы `test/github.test.mjs` |
| ✅ SEC-T11 | аудит `github.rs`/`lib/github.mjs`: абсолютный URL (Link) только на origin api; sticky/инлайн «свой» = маркер + `trustedAuthors` (чтение футера, upsert, чистка); `Debug` без токена | `cargo test github::tests::sec_`, `test/github.test.mjs` SEC-T11, сценарии SEC-T11 в `rust/parity/github.parity.mjs`; тесты падают до фикса |
| ✅ T12 | `engine.rs`: `RealEngine: EngineApi` + `verify_findings` | `cargo test engine::`, кейсы `test/pr-review.engine.test.mjs` |
| ✅ T13 | `graphify_client.rs`: индекс, blast-форматирование, спавн git+graphify | `cargo test graphify_client::` |
| ✅ T14 | `astgrep.rs` + `prescan.rs`, embedded ruleset → `temp_dir()`, fail-open | `cargo test astgrep:: prescan::`; фикстуры `prescan-astgrep-xff`, `prescan-eslint-layer-block`; интеграционный тест с реальным `ast-grep` |
| ✅ T15 | `blast_findings.rs`, `cost.rs`, `swarm.rs` — чистые порты | `cargo test blast_findings:: cost:: swarm::` |
| ✅ T16 | `observability.rs` ← `lib/observability.mjs` | `cargo test observability::` |
| ✅ T17 | `feedback.rs` ← `feedback-schema` + `feedback` (harvest через `GithubApi`) | `cargo test feedback::` |
| ✅ T34 | `absent_path.rs` ← `lib/absent-path.mjs` (`buildRepoPathIndex`, `findAbsentButExistingPath`, `listRepoFiles` спавном `git ls-files`, fail-open) + FP-семейство 7 в `findings::filter_false_positive_findings` (`repoIndex`, только LLM-находки). Зависит от T07 | `cargo test absent_path:: findings::`; кейсы `test/absent-path.test.mjs`; фикстура `fp-absent-path-outside-diff` |
| ✅ T35 | `context_window.rs` ← `lib/context-window.mjs` (окна ±`contextMargin` RFC 07: `windowChunkFileContents`, `buildVerifierFileContents`) + `read_changed_files` с маркером добавленного файла (`collect_added_paths` уже в `diff.rs`). Зависит от T04 | `cargo test context_window:: diff::`; `REVIEW_CONTEXT_MARGIN=0` — откат на целые файлы |
| ✅ T36 | `chunk_routing.rs`, `specialist_merge.rs`, `prompt_layout.rs` и `build_merged_specialist_system` в `prompt.rs` — чистые порты RFC 04 (решения 2, 4, 5). Зависит от T12 | `cargo test chunk_routing:: specialist_merge:: prompt_layout::`; сорванный формат секций не раздаёт один текст трём ролям |
| ✅ T37 | `engine.rs`: интеграция RFC 04 — чанк без кода идёт одним агрегатором без жюри, слитые специалисты на одной модели, раскладка `shared-prefix` (`cache_prefix_chars`), summary агрегата из кодового чанка (`pickAggregateSummary`), событие `chunk_routing`. Зависит от T36 | `cargo test engine::`; кейсы RFC 04 из `test/pr-review.engine.test.mjs`; `rust/parity/engine.parity.mjs` |
| ✅ T38 | `symbol_cards.rs` ← `lib/symbol-cards.mjs` (RFC 14): карточки advisors/жюри и verifier'у по флагу, граф — `BlastRadius::graph_index`, строки рабочего дерева — инъекцией `readLines`, линейный разбор без ReDoS. Зависит от T13 | `cargo test symbol_cards::`; кейсы `test/symbol-cards.test.mjs` |

**Волна 3**

| # | Суть | DoD |
|---|---|---|
| ✅ T18 | `structure_identity.rs` + `structure_context.rs` | `cargo test structure_identity:: structure_context::` на `fixtures/structure/` |
| ✅ T19 | `structure_drift.rs` | `cargo test structure_drift::` |
| ✅ T20 | `structure_export.rs` | `cargo test structure_export::` |
| ✅ T21 | `structure.rs` — glue `analyze_structure`, git exec; без импорта pipeline/engine/github/llm | `cargo test structure::` |
| ✅ T22 | подкоманда `structure` ← `bin/pr-review-structure.mjs` | `assert_cmd`-тест: `facts.json` совпадает с JS на фикстуре |
| ✅ T40 | (сделано в рамках T23) `mcp_stdio.rs` ← `lib/mcp-stdio.mjs`: общий stdio-транспорт MCP (Content-Length-кадры, частичные чтения, batch, oversized 8 МБ, notifications) + handshake-модули `structure-mcp-handshake`/`structure-mcp-tools` (RFC 10: handshake без домена) | `cargo test mcp_stdio::` |
| ✅ T23 | подкоманда `structure-mcp` поверх T40 (Content-Length framing, 8 МБ, batch, сигналы) | `cargo test --test structure_mcp_stdio` |

**Волна 4**

| # | Суть | DoD |
|---|---|---|
| ✅ T24 | `triage.rs`: `build_triage_context`, `run_auto_triage` (FP-совпадение по `fp_patterns` в Rust) | `cargo test triage::` |
| T39 | `incremental.rs` ← `lib/incremental.mjs` (RFC 05): хэш входа модели по файлу, состояние в `paths.incrementalState`, `fullEvery`; перенос находок **после** refutation и verifier. Зависит от T04, T05 | `cargo test incremental::`; кейсы `test/incremental.test.mjs` |
| T26 | `pipeline.rs` ← `pipeline.mjs` (`createReviewPipeline`/`runDetailed`/`runGateResolution`; после слияния `main` — 1066 строк: `repoIndex` для FP-семейства 7, окна контекста, карточки символов, инкрементальный перенос, `efficiencyMetrics` в `run_end`), `PipelineOutcome` вместо `die`/`exit`. Зависит от T24, T28, T29, T34–T39 | `cargo build`; `cargo test pipeline::` |
| ✅ T28 | `eval.rs` + подкоманда `eval`, замена стаба `bin/eval.rs` | `pr-review eval` → 16/16 (фактически 17/17 — столько seed-кейсов в `fixtures/` сейчас; вывод и код возврата байт в байт с `npm run eval`, `node rust/parity/eval.parity.mjs`) |
| ✅ T29 | подкоманда `metrics` ← `bin/pr-review-metrics.mjs` | отчёт совпадает по структуре с JS |

**Волна 5**

| # | Суть | DoD |
|---|---|---|
| T25 | golden-финал: 16 фикстур + router skip/light + telegram, снять `#[ignore]` | `cargo test --test pipeline_golden` |
| T27 | подкоманда `review` ← `bin/pr-review.mjs` + `runReview`: fail-open exit 0, блокеры exit 1, strict-режимы | dry-run на фикстуре с фейковым LLM-сервером |
| T30 | подкоманды `feedback`, `feedback-batch`, `feedback-migrate`, `feedback-promote` | `cargo test --test feedback_cli` |
| T41 | подкоманда `review-mcp` ← `bin/pr-review-mcp.mjs` + `lib/review-mcp{,-handshake,-tools}.mjs` (RFC 08–12: дешёвые стадии, `review_run` как pinned dry-run MoA поверх `runDetailed`, `wait: false` + поллинг по `jobId`, атомарный слот). Десятая подкоманда; `cursor-plugin/bin/review-mcp.sh` переводится в T31. Зависит от T26, T40 | `cargo test --test review_mcp_stdio`; кейсы `test/pr-review.mcp.test.mjs` |

**Волна 6**

| # | Суть | DoD |
|---|---|---|
| T31 | `examples/ci/*.yml` (скачивание по пину + sha256), `docs/{integration,architecture,configuration}.md`, `README.md`, `CLAUDE.md`, `CHANGELOG.md`, `package.json`, `examples/consumers/orakul/`, вынос `test/ci-templates.test.mjs` | `node --test test/ci-templates.test.mjs`; ни одного `node .ai-reviewer-engine/` в шаблонах |
| T32 | `release.yml`: матрица 4 таргета, `SHA256SUMS`, публикация по тегу | `cargo build --release` локально; `actionlint` зелёный; реальный прогон по `workflow_dispatch` |
| T42 | dev-тулы RFC 15 (`lib/cues.mjs`, `lib/brief.mjs`, `bin/pr-review-scout.mjs`) и пробники (`pr-review-probe-cache`, `pr-review-symbol-cards-probe`): остаются JS по non-goal «dev-тулы», поэтому решение 8 получает исключение — эти модули и их импорты (`diff`, `router`, `symbol-cards`) читают JSON-вывод бинаря или вынесены в `tools/`; порт `cues`/`brief` — только если RFC 15 перейдёт к этапу 1 (наряды в гейте) | `node bin/pr-review-scout.mjs --gold-dir gold` работает после T33 |
| T33 | cutover: удаления по решению 8, `pr-review-gold.mjs` на спавн бинаря, новый порядок шагов `ci.yml` | `cargo build/test` и `npm test` зелёные; `grep -rn "pipeline.mjs\|lib/.*\.mjs" --include=*.mjs` не находит ссылок на удалённое; один бинарь запускает 9 подкоманд |
