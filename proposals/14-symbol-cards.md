---
type: design
status: draft
owner: ai-reviewer
source_of_truth: true
created: 2026-10-08
last_updated: 2026-10-08
---

# 14 — Карточки символов

> Категория: 🔵 Архитектура · Риск: средний · Зависимости: [RFC 04](04-token-economy.md) (цена входа), [RFC 05](05-incremental-review.md) (хэш входа), [RFC 07](07-windowed-file-context.md) (окна), `docs/false-positives.md` (семейства 4, 6, 7)

## Контекст

Модель видит дифф, окна ±40 строк вокруг изменённых строк и blast-radius экспортов. Чего она
не видит — объявлений того, к чему обращаются изменённые строки, если объявление лежит вне
окна или вне PR. Из этой слепоты растут три наблюдаемых класса ошибок:

1. **Утверждение об отсутствии символа.** «`parseLimits` не определена», «экспорта `X` нет» —
   о символе, который есть в репозитории. Это семейство 7, но про символ, а не про файл;
   фильтр семейства 7 его сознательно не снимает (`docs/false-positives.md`, «отсутствие
   символа в существующем файле»).
2. **Гипотеза о контракте.** Модель не знает арность/параметры вызываемой функции и
   домысливает: «вызов без `opts` упадёт», «функция возвращает Promise, нет await».
   Семейство 4.
3. **Объявление за окном.** RFC 07 признал это платой: правка в конце длинной функции
   отрезана от её сигнатуры. Риск 2 в RFC 07.

Сейчас против этого работает только текст промпта (`SPLIT_NOTE_RULE`, правило (c) «не
undefined только потому, что не видишь объявления»). Правило запрещает вывод, но не даёт
данных. Предложение — дать данные, узко, и сначала снимать ошибку детерминированно после LLM.

## Текущее состояние

Сверено по коду на `be66618`:

- **Граф есть, сигнатур в нём нет.** `buildGraphIndex` (`lib/graphify.mjs:80-118`) читает у
  узла `id`, `label`, `source_file`, `community`; `structure-graphify` дополнительно
  `source_location` (`"L10"`) и ставит `signature: null` (`lib/structure-graphify.mjs:266`).
  Индекс по имени без `()` (`byBareLabel`) и `resolveUniqueNode` (`lib/graphify.mjs:124`) уже есть.
- **Сигнатуры есть только в SCIP**, и SCIP читает только structure-контур
  (`lib/structure-scip.mjs:243-260`, `lib/structure.mjs:182-204`). В CI SCIP не строится
  (`examples/ci/pr-review.yml:59-92` ставит только graphify).
- **Свежесть графа.** `readGraphFreshness` (`lib/graphify.mjs:266-283`) сравнивает
  `Built from commit:` из отчёта с HEAD. Нет графа → `graph_status: 'missing'`, блоков нет
  (`:341-349`). Свежесть влияет только на blast-radius.
- **Извлечения используемых идентификаторов нет.** `collectBlastSymbols`
  (`lib/diff.mjs:119`, `:281-334`) берёт только объявления `export …`. Вызовы и `new X` в
  изменённых строках никто не собирает.
- **Окно не отдаёт свои диапазоны.** `windowChunkFileContents` (`lib/context-window.mjs:98-120`)
  возвращает `{ map, windowed, before, after }`; сохранённые строки видны только как маркеры
  `… (строки N–M пропущены)` в тексте.
- **Промпт.** `buildMessagesJs` (`lib/prompt.mjs:265-357`): blast-блок `:326-332`, контекст
  `:334-336`, порядок `:350-354`. Портирован в Rust (`rust/src/prompt.rs`, parity
  `rust/parity/prompt.parity.mjs`). `SPLIT_NOTE_RULE` — `lib/prompt.mjs:435` / `rust/src/prompt.rs:471`.
  Blast-блоки едут `pipeline.mjs:474` → `engine.reviewAllModels` (`:633`) →
  `buildMessages` (`lib/engine.mjs:348-353, 502`).
- **Хэш инкрементального прогона** (`lib/incremental.mjs:41-49`) = sha256 от
  `{diff секции файла, окно контекста}`; blast в него не входит.
- **Образец пост-LLM фильтра.** Семейство 7: `lib/absent-path.mjs` (`listRepoFiles`
  fail-open `:205-215`, `buildRepoPathIndex` `:120-129`, `findAbsentButExistingPath`
  `:183-199`), подключён в `filterFalsePositiveFindings` (`lib/findings.mjs:221-240`) только
  для LLM-находок, индекс строится в `pipeline.mjs:659-664`, DI `deps.repoFiles`
  (`pipeline.mjs:313`). В Rust не портирован. Eval читает `repo-files.txt` фикстуры
  (`lib/eval.mjs:38`); счётчик eval-кейсов — 17 (`test/pr-review.eval.test.mjs:111, 116`).

## Предложение

Три решения, в этом порядке. Каждое следующее не начинается, пока предыдущее не замерено; третье — только по метрике второго.

### Решение 1. Семейство 8: «символа нет» о символе, который есть в графе

После LLM, в том же FP-фильтре, что семейство 7. Находка снимается, если одновременно:

1. в одном предложении утверждение об отсутствии (`не определена`, `не существует`,
   `не найден`, `не экспортируется`, `is not defined`, `does not exist`,
   `is not exported`) стоит вплотную к идентификатору в обратных кавычках;
2. идентификатор однозначно разрешается в узел графа (`resolveUniqueNode`);
3. файл узла **не входит в PR** — если входит, модель могла видеть удаление, и снимать
   нельзя (граф мог быть построен до PR);
4. строка `source_location` этого файла в рабочем дереве содержит идентификатор — проверка
   против устаревшего графа, без опоры на `graph_fresh`.

Не снимается: условие рантайма (`if (!fn)`, `typeof x === 'undefined'` — та же
`RUNTIME_CONDITION`, что в `absent-path`); утверждение с явно названным другим файлом
(«`parse` не экспортируется из `a.ts`», а узел в `b.ts`); неоднозначное имя (2+ узла).

Контракт:

```js
// lib/absent-symbol.mjs — JS-only, как absent-path
export function buildSymbolIndex(graphIndex, { changedPaths, readLine }) // → index | null
export function findAbsentButExistingSymbol(finding, index)            // → { name, path, line } | null
```

- `graphify` получает метод `symbolIndex()` (тот же `loadGraphEnrichment`, без нового
  чтения графа); пайплайн строит индекс рядом с `repoIndex` (`pipeline.mjs:659-664`) и кладёт
  в `fpOpts.symbolIndex`. Нет графа / ошибка → `null`, фильтр выключен, строка в лог.
- Снятая находка пишется в JSONL с `fpReason: 'absent_symbol_in_graph'`.
- Eval: fixture `fixtures/fp-absent-symbol-outside-diff/` с `graph-nodes.json` (узлы
  `{label, source_file, source_location}`) и `repo-lines.json` (строки объявлений);
  `lib/eval.mjs` читает их так же, как `repo-files.txt`. Счётчик 17 → 18. Плюс отрицательная
  fixture: символ удалён в PR → находка остаётся.

### Решение 2. Карточки символов во входе advisors и жюри

Под флагом `REVIEW_SYMBOL_CARDS` (по умолчанию `0`).

**Отбор.** Для каждого исполняемого файла чанка:

1. из добавленных строк диффа собрать идентификаторы вызовов и конструкторов
   (`name(`, `new Name`, `obj.name(` → `name`), минус ключевые слова и глобалы языка;
2. выкинуть объявленные в видимом модели тексте (дифф + окно этого файла);
3. разрешить остаток через `resolveUniqueNode`; неоднозначные — пропуск;
4. текст карточки — строка объявления `source_file:Lnn`, продолженная до `{` / `=>` / `;`
   (≤ 3 строк, ≤ 160 символов). Источник: `contentMap`, если файл в PR (это и есть случай
   «объявление за окном»), иначе рабочее дерево. Строка не содержит имени → карточки нет
   (устаревший граф);
5. ≤ `REVIEW_SYMBOL_CARDS_MAX` (по умолчанию 20) карточек и ≤ 3000 символов на чанк,
   приоритет — символы, вызванные из большего числа изменённых строк.

**Блок в промпте** — после контекста, до blast-radius:

```
## Declarations of symbols used by changed lines (declaration line only, body NOT shown)
- `export function parseLimits(raw, { strict = false } = {})` — lib/config.mjs:L212
```

Правило к блоку: карточка доказывает, что символ существует и какова его сигнатура; она
**не** доказывает ничего о теле — находки о поведении символа по карточке запрещены.

**Контракт:**

```js
// lib/symbol-cards.mjs — JS-only, оркестрация, как chunk-routing
export function extractUsedIdentifiers(addedLines, lang)                    // → string[]
export function buildSymbolCards(files, { graphIndex, contentMap, windows, readLines, limits })
  // → { byPath: Map<path, string[]>, metrics: { candidates, resolved, cards, chars, skip } }
```

- `buildMessages` получает параметр `symbolCards: string[]`; JS и `rust/src/prompt.rs`
  меняются вместе, parity-кейс с непустыми карточками.
- **Инкрементальность.** Карточки файла входят в хэш `fileInputHashes`:
  `{ diff, context, cards }`. Сменилась сигнатура вызываемой функции → файл ревьюится
  заново; не сменилась → перенос работает как сейчас.
- **Метрики.** `plan.chunkPlan[].symbol_cards`, `.symbol_cards_chars`;
  `run_end.symbolCards` — `{ candidates, resolved, cards, skip_reasons }`.
- Verifier карточки в v1 не получает: он режет полный `contentMap` сам, и это отдельный замер.

### Решение 3 (условное). Ранжирование кандидатов дешёвой моделью

Включается, только если по метрикам решения 2 `run_end.symbolCards.candidates` превышает
`REVIEW_SYMBOL_CARDS_MAX` больше чем в ~20 % чанков на gold и реальных PR. Иначе
детерминированного приоритета (число вызывающих изменённых строк) хватает, и стадия не нужна.

- Флаг `REVIEW_SYMBOL_CARDS_RANKER=<model>` (по умолчанию пусто — стадии нет); модель — из
  пула `OLLAMA_MODELS`, самая дешёвая.
- Вход: дифф чанка + список кандидатов `{id, name, path:line, declaration}`. Выход — только
  `{"keep": ["id", …]}`; id вне списка отбрасываются. **Текст карточки модель не пишет и не
  меняет** — он всегда из файла.
- Кэш выбора по `sha256(diff секции + список кандидатов)` в состоянии инкрементального
  прогона: тот же вход → тот же набор карточек → хэш RFC 05 стабилен.
- Сбой, таймаут, невалидный JSON → детерминированный приоритет (fail-open). Стадия
  проводится через `deps` (DI), `llm_call` с `callType: 'symbol_ranker'` в JSONL.
- Критерий оставить: на gold recall не ниже решения 2 при том же лимите, а
  `tokens_per_inline` с учётом вызова ранкера не выше.

## Миграция

1. Решение 1 — включено по умолчанию, как семейство 7: фильтр только снимает, fail-open,
   держится двумя fixture.
2. Решение 2 — `REVIEW_SYMBOL_CARDS=0` → прогон gold с `1` → дефолт `1`, если критерии ниже
   выполнены. Документация переменных — `docs/configuration.md`.
3. Breaking для потребителя нет: формат футера, export и namespace не меняются. Новые поля
   JSONL — аддитивные.

## Риски и что не делаем

- **Риск: домысливание по сигнатуре.** Модель видит `validate(input)` и пишет «не
  проверяет null». Контроль — правило блока и gold `fp-absence-consumer-out-of-chunk`,
  `fp-absence-doc-claims-not-in-diff` (`mustNotFindTitles`, `wouldBlock: false`). Новый
  absence-класс на них — сигнал остановки.
- **Риск: рост ложных блокеров.** `fp-noise-no-real-blockers` ✅ → ❌ отменяет решение 2
  (прецедент A5 в RFC 04).
- **Риск: цена.** Успех решения 2 — `run_end.efficiencyMetrics.tokens_per_inline` и
  `tokens_per_blocker` не растут; recall на `tp-real-bugs-unescape` и
  `tp-structure-real-defects` не падает.
- **Риск: снятие верной находки (решение 1).** Закрыт условиями 3–4 и отрицательной fixture.
- Не делаем: сборку дерева моделью (это задача парсера: детерминированно, мс, без
  галлюцинаций пути/строки); свободные сводки символов от предварительной модели —
  непроверенное утверждение едет одним входом и в advisors, и в жюри, и в verifier, его ошибка
  становится согласованной ошибкой обоих судей (та же причина, по которой
  `specialist-merge` не раздаёт один текст трём ролям);
  дерево/outline всего репозитория; тела функций; SCIP в CI (строки объявления
  из графа + файла достаточно, SCIP — отдельный RFC, если не хватит); tool-use модели;
  карточки для verifier; порт `absent-symbol` и `symbol-cards` в Rust (оркестрация и
  FP-фильтр JS-only).

## Оценка

| # | Задача | Объём |
|---|--------|-------|
| 1 | `lib/absent-symbol.mjs` + подключение в `filterFalsePositiveFindings` + `graphify.symbolIndex()` | S |
| 2 | Две fixture семейства 8, `lib/eval.mjs` читает `graph-nodes.json`/`repo-lines.json`, счётчик 18 | S |
| 3 | Раздел «Семейство 8» в `docs/false-positives.md` | XS |
| 4 | `lib/symbol-cards.mjs` + юнит-тесты извлечения и отбора | M |
| 5 | Параметр `symbolCards` в `prompt.mjs` и `prompt.rs`, parity-кейс | S |
| 6 | Карточки в `fileInputHashes`, метрики в `chunkPlan` и `run_end` | S |
| 7 | `REVIEW_SYMBOL_CARDS*` в config + `docs/configuration.md` | XS |
| 8 | Gold-прогон с флагом, запись в журнал, решение о дефолте | M |
| 9 | (условно, по метрике из п. 8) ранкер решения 3: стадия через `deps`, кэш выбора, fail-open | M |
