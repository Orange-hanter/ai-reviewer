---
type: design
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-10-13
---

# 17 — Детерминированные дыры гейта

> Категория: 🎯 Прямое · Риск: средний · Зависимости: нет (RFC 13 — следующий слой поверх)

## Контекст

Разбор гейта по коду и прогон `resolveBlockers` на синтетических находках (Rust и JS дают
одно и то же) нашли места, где решение «блокировать merge» принимается не по той находке,
теряет её без следа или не видит изменение вовсе. Все они детерминированные: чинятся без
LLM и проверяются фикстурой, которая падает до правки.

Вне этого RFC — места, где нужно решение, а не правка: что делает гейт, когда verifier не
отработал (сейчас блок решает самооценка агрегатора), консенсус в MoA, корзины по 3 строки
(см. «Что не делаем»).

## Текущее состояние

1. **Флаг блока ставился на кластер.** `clusterFindings` (`lib/findings.mjs`, `rust/src/findings.rs`)
   считал `tierBlock` и `max(confidence)` по всей корзине `file|floor((line-1)/3)` и писал
   одно `consensusBlock` всем её членам; `resolveBlockers` брал блокером первого. Следствия:
   - minor рядом с major становился блокером и причиной блока в sticky;
   - critical с confidence 0.5 блокировал, если рядом лежал чужой major с 0.95;
   - eslint-правило вне `blockRules` (severity major, confidence 1) делало блокирующим
     кластер, `applyPrescanBlockPolicy` снимал флаг только с самой eslint-находки — и merge
     блокировал перенесённый minor на соседней строке;
   - `lib/export.mjs` отдавал как блокеры все находки с флагом, включая этот minor.
2. **Дедуп с prescan выбрасывал блокеры.** `dedupeAgainstPrescan` (`lib/prescan.mjs`)
   убирал LLM-находку при любой prescan-находке в той же корзине: `max-len` на строке 42
   стирал critical «SQL-инъекция» на строке 41, а сам prescan без `blockRules` не блокирует.
3. **Rename с правками уходил в skip.** `isRenameOnlySection` (`lib/router.mjs`,
   `rust/src/router.rs`) возвращал `true` по одной строке `rename from` до поиска `+`/`-`.
4. **Удалённый файл был невидим.** `parseDiff` брал путь только из `+++ b/…`; на
   `+++ /dev/null` путь — `null`, файл не попадал в `files`/`changedPaths`, удалённые
   строки — в `changedText`. PR «удалил `auth/guard.ts` + правка README» шёл в light.
5. **Блокеры сверх капа verifier'а молча понижались.** `runVerifierStage` (`pipeline.mjs`)
   отдавал verifier'у до `REVIEW_VERIFIER_MAX=12` блокеров; 13-й при `REVIEW_VERIFIER_STRICT=1`
   становился minor с `verifierStatus: overflow`.

## Предложение

1. **Решение — свойство находки.** Находка блокирует, если она сама: в `blockOn`; в
   `blockSingle` или кластер набрал `consensusMin` моделей; её собственная
   `confidence ≥ REVIEW_CONFIDENCE_MIN`; в strict — сама prescan или `verified`; из
   blast-radius — сама `verified`. Кластер остаётся для счёта моделей и `consensus`.
   Блокер кластера — первая блокирующая находка. `pipeline.mjs` считает блокирующие
   кластеры по `g.some(f => f.consensusBlock)`.
2. **Дедуп только по prescan не ниже по severity.** LLM-находка снимается, если в её
   корзине есть prescan-находка той же или более высокой severity.
3. **Rename-only** — `similarity index 100%` или секция без строк `+`/`-`. Ранний выход
   по `rename from` убран.
4. **Удалённый файл**: на `+++ /dev/null` путь берётся из `diff --git` (JS и Rust); новая
   чистая `collectDeletedPaths(diff)` (`lib/diff.mjs`) — prescan не запускается на файлах,
   которых нет в рабочем дереве. Удалённые строки идут в `changedText` → blast-radius видит
   удалённый экспорт. Признак удаления — только `deleted file mode` в заголовке секции (до
   первого `@@`): добавленная строка `++ /dev/null` в диффе выглядит как `+++ /dev/null`, и
   поиск по всей секции выводил живой файл из prescan (обход `blockRules`). Заодно внутри
   хунка `+++`/`---` — строки содержимого, а не заголовки: раньше такая строка сбрасывала
   текущий путь, и всё ниже неё выпадало из `changed`/`changedText`/`validLines`.
5. **Verifier батчами.** `REVIEW_VERIFIER_BATCHES` (дефолт `3`, JS и Rust-конфиг): блокеры
   сверх капа идут следующими вызовами по `REVIEW_VERIFIER_MAX`. Сначала все вызовы, потом
   одно `applyVerifierVerdicts` с пересчитанными `candidateIndex`. Сбой первого батча —
   fail-open, как раньше; сбой хвостового или тайм-бюджет — проверенное остаётся, хвост как
   сверх капа. `stats.batches` — в событии `verifier`.

## Миграция

- Без флага: правки 1–4 — исправление поведения. Откат дефолта батчей — `REVIEW_VERIFIER_BATCHES=1`.
- Потребитель увидит: блоков из-за соседней находки больше нет; PR с rename+правками и с
  удалениями файлов-кода идут маршрутом full (больше LLM-вызовов, возможны новые блоки);
  до двух лишних вызовов verifier на PR с >12 блокерами.
- Формат экспорта не меняется, меняется значение `consensusBlock`: теперь только у
  блокирующей находки. `EXPORT_VERSION` не поднят — форма та же; потребитель, считавший
  блокеры по флагу, получает их правильно.

## Риски и что не делаем

- Не трогаем поведение при несработавшем verifier'е (non-strict блокирует по слову
  агрегатора): это выбор между fail-open и degraded, его решает RFC 13 или отдельное RFC.
- Не меняем корзины на связывание по |Δline| — затрагивает консенсус и Δ-блок.
- Не делаем межмодельный консенсус в MoA: метка модели там одна.
- Риск: блок, который раньше держался на соседе, пропадёт. Это и есть цель; фикстуры
  `gate-neighbour-*` фиксируют, какая находка теперь блокирует.

## Оценка

Одна задача: правки 1–5, фикстуры `gate-neighbour-minor-not-blocker`,
`gate-neighbour-confidence-no-lift`, `gate-eslint-nonblock-via-neighbour`, тесты дедупа,
rename, удалённого файла, батчей verifier; parity-кейсы `rfc17 *` в
`rust/parity/findings.parity.mjs` и `router.parity.mjs`.
