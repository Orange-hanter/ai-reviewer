---
type: design
status: draft
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-10-16
---

# 21 — Decision-модели в shadow: классификация FP и риск чанка

> Категория: 🟡 Адаптация · Риск: низкий (гейт не трогается) · Зависимости: RFC 20 (новое поведение модуля llm — только Rust), RFC 13 (путь включения `0` → `shadow`), [проба API](../docs/probe-decisions-2026-10-16.md)

## Контекст

Осенью 2026 появились decision-модели — `microsoft/microsoft-decision-1` (Qwen3.5-9B,
Azure), `openai/gpt-6-luna-decisions`, Jev от TypeSafe. Модель получает вход и вопрос с
закрытым набором вариантов и возвращает вероятность каждого варианта, не генерируя текста:
$0.04–0.10 за 1M входа, 0.4–0.6 с на тёплом вызове.

Что известно о классе из открытых реализаций и независимых замеров (обзор в журнале):

- вероятность = нормированное правдоподобие варианта; калибровка без подбора температуры на
  своих метках не гарантирована (instruct-модели переуверены, у Jev в независимом замере
  ECE 0.133);
- есть смещение по порядку вариантов и конкуренция формулировок;
- один проход слаб на многошаговом рассуждении — «реален ли дефект в коде» ровно такая задача;
- в каскаде с LLM-подстраховкой уверенные ошибки decision-модели LLM повторяла в 96% случаев:
  второго независимого судьи из неё не выходит.

Отсюда граница: в гейт и verifier не пускаем. Кандидаты — два решения по закрытому набору,
которые сейчас принимаются регексом или эвристикой:

- **Q1 — семейство FP находки**: 8 семейств `docs/false-positives.md` + `other_fp` + `real`.
  Сейчас — 51 регекс `feedback/fp-patterns.jsonl`, у которого нет понятия «семейство».
- **Q2 — нужно ли чанку жюри**: `jury` | `single`. Сейчас — `chunkNeedsSpecialists` по классу
  пути.

Цель этого RFC — **только замер**: дают ли модели сигнал, стабильный и независимый от
порядка, на наших данных. Влияние на вывод — отдельный RFC 22 по данным замера.

## Текущее состояние

- FP-фильтр: `filterFalsePositiveFindings` (`lib/findings.mjs:224`) матчит
  `` `${title} ${body}` `` через `matchesKnownFalsePositive` (`:216`), которая выбрасывает id
  паттерна; id отдаёт `matchFpPatterns` (`lib/fp-patterns.mjs:80`). Вызывается единственным
  путём `runGateResolution` (`pipeline.mjs:398`) из основной ветки (`:1005`) и fail-closed
  (`:901`). У строк jsonl есть `id`, `description`, `regex`, `requiresPath` — поля семейства
  нет.
- Маршрут чанка: `chunkNeedsSpecialists(chunk, {fileClass, sensitive})`
  (`lib/chunk-routing.mjs:32`), зовётся из `planCodeChunks` (`lib/engine.mjs:349`). Событие
  `chunk_routing` (`lib/engine.mjs:503-513`) пишется, только если есть хотя бы один lean-чанк.
  `limits.chunkRouting` входит в ключ инкрементального прогона (`lib/incremental.mjs:84`).
- LLM-клиент (`lib/llm.mjs:111`) знает только `chat/completions`; бинарь `llm`
  (`rust/src/bin/llm.rs`) различает вход по ключу `cascade`.
- Цена: общий `costTracker` печатается в sticky (`pipeline.mjs:1107` → `formatCostStickyLine`)
  и питает `efficiencyMetrics` в `run_end` (`lib/cost.mjs:81`).
- Режима shadow в пайплайне нет; прецедент офлайн-shadow — `bin/pr-review-scout.mjs` (RFC 15).
- API (проба 2026-10-16): обе модели — один эндпоинт OpenRouter
  `POST /api/alpha/decisions`, `{model, state, questions:{name:{type,instructions,criteria}}}`
  → `answers[name].{choice, probabilities, confidence}`, `usage.cost`. Decision-1 не
  детерминирована (TV повтора ≈ 0.04), luna детерминирована и квантует вероятности с шагом
  0.01. Перестановки вариантов внутри одного вызова у Decision-1 дают побитово одинаковый
  ответ — смещение порядка мерить только отдельными вызовами.

## Предложение

### Этап 0 — офлайн-замер (пайплайн не меняется)

1. **Вызов в Rust.** `rust/src/llm/decide.rs`, ветка по ключу `"decide"` в
   `rust/src/bin/llm.rs`. Вход
   `{decide:true, url, key, model, state, question:{name,instructions,criteria}, permutations, httpRetries, timeoutMs, maxStateChars}`;
   выход
   `{answer:{probabilities, perPermutation, argmax, flip, tvMax, confidence, refusal, truncated}, usage, latencyMs}`;
   ошибка — exit 2 и `{error,status,quota,throttled}` в stderr (разбирает существующий
   `parseErrorPayload`). Перестановки — циклические сдвиги criteria отдельными вызовами,
   ответ сопоставляется по значению варианта. Модуль rust-owned: JS-копии и parity нет.
2. **Конфиг** — секция `decision` (`REVIEW_DECISION`, `_BASE_URL`, `_API_KEY`, `_MODEL`,
   `_PERMUTATIONS`, `_TIMEOUT_MS`, `_MAX_CALLS`, `_CONCURRENCY`, `_QUESTIONS`), см.
   `docs/configuration.md`. Ключ: `REVIEW_DECISION_API_KEY` → `OPENROUTER_API_KEY` →
   `LLM_API_KEY`.
3. **`lib/decision.mjs`** — варианты Q1/Q2 с описаниями, таблица `FP_FAMILY_BY_PATTERN_ID`
   (все id пакетного jsonl → семейство или `other_fp`), построители входа, клиент с лимитом
   параллелизма, бюджетом вызовов и circuit breaker на квоте, свой учёт цены.
4. **`bin/pr-review-decision-eval.mjs`** — корпус из `fixtures/` и gold-прогонов, метки из
   gold meta (`mustFindLocations` → `real`, `mustNotFindTitles` и блокеры в `type: clean` →
   `fp`), отчёт по каждой модели: согласие с метками (AUC `p(real)` с бутстрап-ДИ), шум
   повтора, смещение порядка (отдельно от шума), κ между моделями, совпадение с регексом, Q2
   против `chunkNeedsSpecialists`, латентность, цена. Результат — `docs/decision-stage0-<дата>.md`
   и решение go/no-go здесь.

### Этап 1 — shadow в пайплайне (после go)

`REVIEW_DECISION=shadow` при включённой observability: Q1 по находкам до FP-фильтра (хук в
`runGateResolution`, обе ветки), Q2 по чанкам параллельно engine. Вердикты — только в события
`decision_fp`, `decision_chunk`, `decision_error`, `decision_summary`.

## Миграция

Путь включения `off` → `shadow`; дефолта «включено» в этом RFC нет. Потребителю ничего делать
не нужно: без флага ни одного вызова. Без Rust-бинаря shadow пишет одно `decision_error
{reason:'no-binary'}` и молчит (бинари потребителю пока не доставляются, RFC 20).

## Риски и что не делаем

**Инварианты (держатся тестами):**

- вердикт не пишется ни в одно поле находки; sticky, inline, export, футер, exit code — байт
  в байт как при `off`, в том числе когда decider бросает или зависает;
- свой `costTracker` — общий меняет строку цены в sticky;
- `decision.*` не входит в `incrementalConfigKey`; `EXPORT_VERSION` не меняется;
- `lib/decision.mjs` не импортирует structure;
- любая ошибка → `decision_error`, прогон идёт как без shadow; квота → decider выключен до
  конца прогона.

**Не делаем:** замену verifier'а или confidence агрегатора; удаление находок по вердикту;
порог и калибровку на этом объёме данных; поле семейства в формате `fp-patterns.jsonl`.

**Что позволяет текущий объём** (~9 gold + 20 fixtures, ~40–80 находок, размечено ~10–25):
стабильность и смещение порядка (без меток, сотни наблюдений), κ между моделями, совпадение с
регексом, цену и латентность, вывод «сигнала нет», если ДИ AUC накрывает 0.5. Не позволяет:
порог, калибровку, точность по семействам, выигрыш над регексом. Для RFC 22 нужен корпус
больше; способ разметки решается там.

**Критерии этапа 0 → 1** (по модели): ошибки API < 5%; смещение порядка (TV перестановок
сверх шума повтора) — argmax меняется ≤ 15%; смена argmax за 5 повторов ≤ 10%; нижняя граница
ДИ AUC > 0.5 или κ с регексом на fp-fixtures ≥ 0.6; p95 латентности < таймаута.

**Закрытие эксперимента:**

- K0 — эндпоинт или формат нестабилен (на 2026-10-16 не сработал);
- K1 — обе модели проваливают стабильность или порядок;
- K2 — у обеих ДИ AUC накрывает 0.5 и κ с регексом < 0.4;
- K3 — Q2 отправляет чанк с известным багом в `single` при p ≥ 0.7 (закрывает Q2);
- K4 — κ(Decision-1, luna) > 0.9 → вторая модель не даёт независимого сигнала, остаётся дешёвая.

Закрытие — статус «отклонено по замеру», код shadow удаляется, отчёты остаются в `docs/`.

**Отдельный риск luna:** `gpt-6-luna` — кандидат в агрегатор и verifier
(`docs/probe-openrouter-2026-10-14.md`). Если она там окажется, её decision-вариант по Q1
коррелирует с verifier'ом и как независимый сигнал бесполезен.

## Оценка

Этап 0: проба API (сделана), Rust `decide` + конфиг — 1 день, `lib/decision.mjs` и инструмент
замера — 1 день, прогон gold с экспортом — ~1 ч машинного времени. Этап 1: хуки и тесты
тождества вывода — 1 день.
