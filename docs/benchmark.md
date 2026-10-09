---
type: guide
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-09-29
---

# Методика бенчмарка

Офлайн-отчёт precision/recall по gold-набору ai-reviewer (`gold/`), согласованный с оценкой в стиле Entelligence.

## Набор данных

- **Замороженные диффы** — каждый кейс — закоммиченный `diff.patch` плюс `meta.json` с человеческой разметкой.
- **Типы** — `tp` (true positive / обязан заблокировать или обязан найти), `fp` (не должен блокировать / не должен находить шум).
- **Без живого репозитория** — оценка использует JSON находок из предыдущего прогона или вход `--findings`; LLM опциональна.

## Метрики

| Метрика | Определение |
|--------|------------|
| **wouldBlock** | Exit-код гейта / флаг `wouldBlock` совпадает с `meta.expect.wouldBlock`. Основная метрика merge-гейта. |
| **Title recall** | Для каждой подстроки из `mustFindTitles` хотя бы один заголовок находки её содержит (без учёта регистра). Семантический прокси — не точное совпадение строк. |
| **Location recall** | Для каждой записи `mustFindLocations` хотя бы одна находка стоит в этом `file` в пределах `lines ± tolerance` (по умолчанию 3). Не зависит от языка — используется публичным набором. |
| **Title precision** | Ни один заголовок находки не содержит ни одной подстроки из `mustNotFindTitles`. |
| **Severity ranking** | Отчитывается отдельно; бенчмарк не ранжирует уровни severity — только block vs non-block и наличие заголовка. |

Title recall и location recall считаются **раздельно** (`recall` vs `locationRecall`):
«нашёл нужное слово» и «попал в нужную строку» — разные величины, и сведение их в одну
цифру теряет обе.

## API оценки

```js
import { scoreGoldCase, aggregateBenchmark } from 'ai-reviewer/lib/benchmark.mjs';

const score = scoreGoldCase(meta, { wouldBlock: true, findings: [...] });
const summary = aggregateBenchmark([{ id, type, expect, score, wouldBlock }]);
```

## CLI

```bash
# Из JSONL предыдущего gold-прогона (каждая строка: { id, meta, findings, wouldBlock, code })
npx pr-review-benchmark results.jsonl

# Оценить файл находок против одного meta
npx pr-review-benchmark --meta gold/tp-real-bugs-unescape/meta.json --findings run.json

# Список gold-кейсов (заглушка только по meta)
npx pr-review-benchmark --gold-dir
```

Полный shadow eval пайплайна с LLM — по-прежнему `pr-review-gold`; `pr-review-benchmark` оборачивает те же функции оценки без вызова моделей.

## Публичный набор (`gold-public/`)

Собственный `gold/` — это наши же PR, размеченные нами: он ловит регрессии движка, но не
доказывает точность на чужом коде. Публичный набор закрывает именно это (backlog B6,
решение зафиксировано в [RFC 03](../proposals/03-public-benchmark-dataset.md)).

**Разметка выводится из решений самого проекта, а не из нашего суждения.** Метод — SZZ-lite:

1. ищем смерженный PR-**фикс**, тело которого ссылается на PR-**виновника**
   («regression introduced in #N»);
2. дифф виновника (пин по `merge_commit_sha`) становится кейсом;
3. **локация дефекта** выводится сопоставлением текста: строка, удалённая фиксом, ищется
   среди строк, добавленных виновником. Номер берётся из диффа самого кейса, поэтому дрейф
   файла между двумя PR разметку не смещает. Нет пересечения по тексту → дефект не
   локализуется в диффе, кандидат отбраковывается.

Отбрасываются: бэкпорты одного фикса (иначе один дефект даёт пять «кейсов»), совпадения по
строкам `import` (фикс их переставляет) и локации в тестах (ревью обязано найти причину,
а не проверку на неё).

`clean`-контроли меряют precision: смерженный PR старше 400 дней, на который не сослался ни
один другой PR. Это доказывает отсутствие **замеченного** дефекта, не отсутствие дефекта.

### Чужие диффы не вендорятся

Кейс — это `gold-public/<id>/meta.json` **без** `diff.patch`: пин `source.{repo,pr,sha}`,
дифф тянется по `sha` в `REVIEW_GOLD_CACHE` (`.ai-review/gold-public`, gitignored). Причина —
лицензии чужих репозиториев (AGPL/GPL/FSL) и вес монорепо-диффов. Воспроизводимость держит
пин: он указывает на неизменяемый коммит, неявного «взять свежее» в контракте нет.

### Команды

```bash
# добыть кандидатов (разметка) — печатает, а с --emit кладёт заготовки meta.json
node bin/pr-review-gold-harvest.mjs grafana/grafana getsentry/sentry --emit gold-public --clean 2

# прогон с LLM + сводка
OLLAMA_API_KEY=… npm run benchmark:public
```

Харвестер **не решает**, что кейс годен: он готовит кандидатов, отбор — ручной шаг поверх.
Число отбракованных вручную публикуется вместе с результатом — иначе отбор превращается
в подгонку.

### Как читать цифру

Набор меряет **recall по известным дефектам**, а не абсолютный recall: SZZ-пара доказывает,
что дефект был, но не что он единственный. Выборка смещена в сторону дефектов, которые
проект заметил и описал ссылкой на виновника. Сравнения с конкурентами здесь нет — для него
нужен их прогон на том же наборе.

## Набор своих проектов (`gold-private/`)

Тот же SZZ, но по локальной истории проектов, где работает ревью (Orakul, Restate, Iot_Gateway,
cod-doc), без ссылки «introduced in #N»: виновника находит `git blame` строк, удалённых
фиксом (`fix…` в заголовке или ветка `fix/…`), на родителе фикса. Кейс несёт `source.local`
(имя клона) и `source.base..source.sha`; дифф строится из клона по `REVIEW_GOLD_REPOS`, код в
репозиторий ai-reviewer не попадает. Раннер разворачивает дерево проекта на `source.sha` и
ревьюит из него — окна контекста и карточки символов настоящие, а не из каталога ai-reviewer.

Тег `contract` — фикс поменял вызов функции, объявленной в одном другом файле проекта:
подмножество для замера карточек символов (RFC 14). Сборщик и ограничения разметки —
[gold-private/README.md](../gold-private/README.md).

```bash
node bin/pr-review-gold-harvest-local.mjs --repo /path/Orakul --name orakul --branch origin/main --since 2026-01-01 --emit gold-private
REVIEW_GOLD_DIR=gold-private REVIEW_GOLD_REPOS="orakul=/path/Orakul,…" OLLAMA_API_KEY=… node bin/pr-review-gold.mjs
```
