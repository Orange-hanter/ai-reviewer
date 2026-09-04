# Gold set — shadow eval полного pipeline (R-eval)

Отличие от `fixtures/` (eval.mjs): здесь прогоняется **полный pipeline с LLM**
(`--dry-run`, без постинга), а не только post-processing. Кейс = замороженный дифф +
ручная разметка ожидаемого результата.

## Структура кейса

```
gold/<id>/
  meta.json    # { id, description, expect: { wouldBlock, mustFindTitles?, mustNotFindTitles?, mustFindLocations?, note? } }
  diff.patch   # frozen PR diff
```

- `wouldBlock` — ожидаемый вердикт гейта (exit code 1/0).
- `mustFindTitles` / `mustNotFindTitles` — подстроки в title находок (lowercase, мягкий матч).
- `mustFindLocations` — `[{ file, lines: [от, до] }]`: попадание находки в размеченную строку с
  допуском `locationTolerance` (дефолт ±3). Язык-независимая замена сверке заголовков: формулировки
  движка не обязаны совпадать с формулировками разметки. Скоринг — `lib/benchmark.mjs`.

Дифф кейса можно урезать по файлам исходного PR (номера строк внутри оставленных файлов не
съезжают) — факт урезания фиксируется в `expect.note`.

Оффлайн-проверка разметки без LLM: `node bin/pr-review-benchmark.mjs --gold-dir` — читает все
`meta.json` и считает по ним скоринг на пустых находках (stub); ошибка формата вылезет здесь.

## Запуск

```bash
set -a; . ~/.hermes/.env; set +a   # OLLAMA_API_KEY
npx pr-review-gold                 # все кейсы последовательно (~5–10 мин каждый)
npx pr-review-gold --case <id>     # один кейс
```

Env пайплайна переопределяется как обычно (`MOA_AGGREGATOR`, `REVIEW_VERIFIER_MODEL`, …) —
это же протокол A/B моделей (см. [docs/model-tuning.md](../docs/model-tuning.md)).

## Переносимые кейсы (v0.2.1)

| id | Тип | Что проверяет |
|----|-----|---------------|
| `tp-real-bugs-unescape` | tp | recall: реальные major (unescape) среди FP-шума → блок |
| `tp-structure-real-defects` | tp | recall по локациям: 4 реальных дефекта structure-контура среди 29 FP |
| `fp-noise-no-real-blockers` | fp | precision: все major — подтверждённые FP → без блока |
| `clean-code-carryover-fingerprint` | clean | precision: смерженный PR без critical/major |
| `clean-code-footer-last-marker` | clean | футер по последнему маркеру; цитаты маркера в тексте |
| `fp-absence-consumer-out-of-chunk` | fp | absence-claim при consumer в другом чанке |
| `fp-absence-doc-claims-not-in-diff` | fp | «фиксов нет в diff», когда они в workflow-чанке |
| `fp-regen-double-changelog-bump` | fp | ложный major про «двойной бамп changelog» |
| `router-light-conflict-markers-escape` | router | light-маршрут на docs с conflict-markers |

Доменные кейсы Orakul (supplier e2e, rbac gate, IDOR push, …) живут у потребителя —
в пакет не входят.

## Как добавить кейс

1. Взять PR, где вердикт известен вручную: `gh pr diff <N> > diff.patch`.
2. `meta.json` с ожиданием и ссылкой на ручной разбор (note).
3. Прогнать раннер — расхождение = регресс или неверная разметка.

Кейс, падающий на текущем конфиге — НЕ удалять: это сигнал (фиксируем в журнал FP
репозитория-потребителя).
