# Benchmark methodology

Offline precision/recall reporting for the ai-reviewer gold set (`gold/`), aligned with Entelligence-style evaluation.

## Dataset

- **Frozen diffs** — each case is a committed `diff.patch` plus `meta.json` with human labels.
- **Types** — `tp` (true positive / must block or must find), `fp` (must not block / must not find noise).
- **No live repo** — scoring uses findings JSON from a prior run or `--findings` input; LLM is optional.

## Metrics

| Metric | Definition |
|--------|------------|
| **wouldBlock** | Gate exit code / `wouldBlock` flag matches `meta.expect.wouldBlock`. Primary merge-gate metric. |
| **Title recall** | For each `mustFindTitles` substring, at least one finding title contains it (case-insensitive). Semantic proxy — not exact string match. |
| **Location recall** | For each `mustFindLocations` entry, at least one finding sits on that `file` within `lines ± tolerance` (default 3). Language-independent — used by the public set. |
| **Title precision** | No finding title contains any `mustNotFindTitles` substring. |
| **Severity ranking** | Reported separately; benchmark does not rank severity tiers — only block vs non-block and title presence. |

Title recall and location recall are counted **separately** (`recall` vs `locationRecall`):
«нашёл нужное слово» и «попал в нужную строку» — разные величины, и сведение их в одну
цифру теряет обе.

## Scoring API

```js
import { scoreGoldCase, aggregateBenchmark } from 'ai-reviewer/lib/benchmark.mjs';

const score = scoreGoldCase(meta, { wouldBlock: true, findings: [...] });
const summary = aggregateBenchmark([{ id, type, expect, score, wouldBlock }]);
```

## CLI

```bash
# From prior gold run JSONL (each line: { id, meta, findings, wouldBlock, code })
npx pr-review-benchmark results.jsonl

# Score findings file against one meta
npx pr-review-benchmark --meta gold/tp-real-bugs-unescape/meta.json --findings run.json

# List gold cases (meta-only stub)
npx pr-review-benchmark --gold-dir
```

Full pipeline shadow eval with LLM remains `pr-review-gold`; `pr-review-benchmark` wraps the same scoring functions without calling models.

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
