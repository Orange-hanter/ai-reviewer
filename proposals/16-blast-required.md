---
type: design
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-10-13
---

# 16 — Обязательный blast-radius

> Категория: 🎯 Прямое · Риск: низкий · Зависимости: `REVIEW_LLM_REQUIRED` (образец fail-closed)

## Контекст

Локальные прогоны 2026-09-02 (в т.ч. на Sonnet-шиме) шли с
`blast_radius: {symbols_total: 25, symbols_resolved: 0, graph_status: "missing"}`: `graph.json`
не был собран, graphify CLI не установлен. Ревью при этом проходило «зелёным» — breaking
change с caller вне диффа гейт увидеть не мог, а снаружи это неотличимо от «зависимых нет».
Причина в логе была, но её никто не читает, пока не спросит.

## Текущее состояние

- `lib/graphify.mjs` `buildBlastRadius`: нет `graph.json` → `graph_status: 'missing'`,
  `skip_reason: 'graph_missing'`; ошибки CLI на символ → `graphify_unavailable|timeout|error`;
  битый граф → `graph_enrichment_unavailable`. Метрики уходят в `plan` JSONL и в экспорт.
- `pipeline.mjs` блокирует только по находкам и по `REVIEW_LLM_REQUIRED` (полный аутедж LLM).
  Сбой blast-радиуса — всегда fail-open.
- Шаблон `examples/ci/pr-review.yml` собирает граф шагом с `continue-on-error: true`.

## Предложение

1. `REVIEW_BLAST_REQUIRED` → `cfg.gate.blastRequired` (JS и Rust-конфиг, parity), дефолт `0`.
2. `blastUnavailableReason(metrics) → string|null` в `lib/graphify.mjs`. Сбой:
   `graph_missing`, `graph_unreadable`, либо все запросы `affected` упали с инфра-ошибкой.
   **Не сбой:** `graph_stale`, `symbol_unresolved`, `no_dependents`, `no_symbols_in_diff`,
   клиент старого API (`client_did_not_report`) — blast посчитан или судить не о чем.
3. Пайплайн проверяет сразу после blast-стадии, **до LLM**: exit `1`, артефакт с
   `wouldBlock: true`, sticky с причиной, событие `blast_failed`. Токены на ревью, которое
   всё равно заблокируется, не тратятся.
4. Шаблон CI ставит `REVIEW_BLAST_REQUIRED: '1'`; шаг сборки графа остаётся
   `continue-on-error` — упавшую сборку ловит ревью и объясняет в sticky.

## Миграция

Дефолт движка `0` — потребитель без Graphify при обновлении пина не блокируется. Включение —
через шаблон; кто скопировал шаблон и граф не строит, убирает флаг (строка в CHANGELOG).

## Риски и что не делаем

- Флап graphify на одном прогоне блокирует PR. Принято: тот же компромисс, что у
  `REVIEW_LLM_REQUIRED`; лечится перезапуском job.
- Устаревший граф не блокирует: в CI граф строится в том же job, локально stale — предупреждение.
- Router `skip`/`light` граф не требует — blast-radius там не строится.
- Бинарь при нуле символов не проверяется: запросов нет, blast не нужен.

## Оценка

Одна задача: флаг + функция + проверка в пайплайне + тесты + шаблон/доки.
