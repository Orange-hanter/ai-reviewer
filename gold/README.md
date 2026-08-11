# Gold set — shadow eval полного pipeline (R-eval)

Отличие от `fixtures/` (eval.mjs): здесь прогоняется **полный pipeline с LLM**
(`--dry-run`, без постинга), а не только post-processing. Кейс = замороженный дифф +
ручная разметка ожидаемого результата.

## Структура кейса

```
gold/<id>/
  meta.json    # { id, description, expect: { wouldBlock, mustFindTitles?, mustNotFindTitles?, note? } }
  diff.patch   # frozen PR diff
```

- `wouldBlock` — ожидаемый вердикт гейта (exit code 1/0).
- `mustFindTitles` / `mustNotFindTitles` — подстроки в title находок (lowercase, мягкий матч).

## Запуск

```bash
set -a; . ~/.hermes/.env; set +a   # OLLAMA_API_KEY
npx pr-review-gold                 # все кейсы последовательно (~5–10 мин каждый)
npx pr-review-gold --case <id>     # один кейс
```

Env пайплайна переопределяется как обычно (`MOA_AGGREGATOR`, `REVIEW_VERIFIER_MODEL`, …) —
это же протокол A/B моделей (см. [docs/model-tuning.md](../docs/model-tuning.md)).

## Текущие seed-кейсы (2026-07-18)

| id | Что проверяет | Разметка |
|----|---------------|----------|
| `tp-real-bugs-unescape` | recall: реальные major (unescape C-quoting) среди FP-шума → блок | wouldBlock=true, title ~«escape» |
| `fp-noise-no-real-blockers` | precision: все major — подтверждённые вручную FP → без блока | wouldBlock=false |

## Как добавить кейс

1. Взять PR, где вердикт известен вручную (после разбора TP/FP): `gh pr diff <N> > diff.patch`
   или `git diff base..head > diff.patch`.
2. `meta.json` с ожиданием и ссылкой на ручной разбор (note).
3. Прогнать раннер — расхождение = либо регресс пайплайна, либо неверная разметка (обсудить).

Кейс, падающий на текущем конфиге — НЕ удалять: это сигнал, а не мусор (фиксируем в
журнал FP репозитория-потребителя).

## Roadmap

- Nightly/weekly workflow с отдельным бюджетом (gold не должен гейтить PR).
- Расширение до 20–30 кейсов: исторические FP из `ai-review-false-positives.md`,
  clean controls (PR без проблем → 0 находок).
- Целевые SLO: blocker precision ≥90%, critical/major recall ≥80% (бэклог #3).
