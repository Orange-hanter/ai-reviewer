---
type: design
status: active
owner: ai-reviewer
source_of_truth: true
created: 2026-09-09
last_updated: 2026-09-10
---

# 08 — MCP полного прогона: превью для агента, не второй гейт

> Категория: 🔵 Архитектура · Риск: средний · Зависимости: RFC 01 (граница structure)

## Контекст

У агента уже есть MCP-поверхность — `pr-review-structure-mcp` и тулз `structure_context`
(пин SHA/fingerprint, confinement `STRUCTURE_MCP_ROOT`). Это off-PR снимок контрактов и
сценариев, не ревью. Сам merge-гейт вызывается CLI `pr-review` или CI; сворм читает
sidecar JSON, не MCP.

Агенту в IDE нужен полный MoA из чата: «что бы гейт сказал на этот дифф», без постинга в
GitHub. Это новый контракт с потребителем и затрагивает пайплайн (возврат артефакта,
`die` без `process.exit`, пин SHA на `--diff`) — поэтому RFC до кода.

## Текущее состояние

Сверено по коду, не по памяти:

- Structure MCP: [`lib/structure-mcp.mjs`](../lib/structure-mcp.mjs), транспорт
  [`bin/pr-review-structure-mcp.mjs`](../bin/pr-review-structure-mcp.mjs). Один тулз,
  пин обязателен, пути через `resolveContainedPath`.
- Гейт: [`createReviewPipeline(cfg, deps).run()`](../pipeline.mjs) возвращает **только
  exit-код** (`pipeline.mjs`, JSDoc `@returns {Promise<number>}`). Артефакт пишется в
  `persistExports` и на `--dry-run` **молча пропускается**, пока нет `REVIEW_EXPORT_DRY_RUN`
  (`pipeline.mjs` около строк 110–112; тест R9).
- `deps.die` по умолчанию делает `process.exit(0)` (`pipeline.mjs` около строки 294) —
  сорвёт долгоживущий stdio-процесс.
- При `--diff` `headSha` остаётся пустой строкой (`pipeline.mjs` около строк 331–337).
  Пин, как у `structure_context`, сверять не с чем.
- Dry-run печатает summary через `console.log` (`pipeline.mjs` около строк 376, 832–834) —
  на stdout MCP это ломает `Content-Length`-кадры.
- Профиль — process-wide singleton ([`lib/profile.mjs`](../lib/profile.mjs)): два
  параллельных прогона в одном процессе недопустимы.
- Граница RFC 01: [`test/structure-boundary.test.mjs`](../test/structure-boundary.test.mjs)
  запрещает импорт `structure*` из пайплайна и `lib/*.mjs` вне контура.

## Предложение

Отдельный сервер `pr-review-mcp` (bin + `lib/review-mcp.mjs`). Не расширение
`pr-review-structure-mcp`. Ноль runtime-зависимостей: тот же JSON-RPC + `Content-Length`,
транспорт выносится в [`lib/mcp-stdio.mjs`](../lib/mcp-stdio.mjs) и переиспользуется обоими
бинарями.

Поле вердикта в ответе MCP — **`previewWouldBlock`**, не `wouldBlock` сворма. Merge по
прежнему только `exit 1` в CI. `strong` в превью — информативный, не second-judge для
оркестратора.

### Тулзы

`review_run` — единственный дорогой вызов:

| Аргумент | Обязателен | Смысл |
|---|---|---|
| `diffPath` | да | Патч внутри `REVIEW_MCP_ROOT` |
| `headSha` | да | Пин `^[0-9a-fA-F]{7,64}$` |
| `tokenBudget` | нет | 256…32000, дефолт 4000 |
| `priorExportPath` | нет | Прошлый артефакт для carry-over отпечатков |

Дифф в аргументах тулзы не передаём.

Ответ `kind: review_preview`: `headSha`, `runFingerprint` (sha256 артефакта), `artifactPath`,
`previewWouldBlock`, `strong`, `route`, `counts`, урезанные `blockers` / `findingsPreview`,
`truncated`, `status: completed \| degraded`. JSON-RPC error — только плохие аргументы,
escape пути, занятый мьютекс.

`review_findings` — дочитать хвост того же прогона: `runFingerprint` + фильтр `file` /
`severity` + `offset`/`limit`. Если передан `headSha` — обязан совпасть с пином прогона.

### Пайплайн

- `runDetailed()` → `{ exitCode, artifact, exportResult, wouldBlock, degraded, reason }`.
  `run()` по-прежнему возвращает число.
- `cli.headSha` / `deps.headSha` заполняют `headSha` на file-diff.
- MCP всегда `dryRun: true`, github-заглушка (ни sticky, ни inline, ни auto-triage, ни
  telegram), `die` бросает `PipelineSoftFail` → `status: degraded`, процесс жив.
- Dry-run summary идёт через `log()`, не в сырой `console.log`.
- Мьютекс: второй параллельный `review_run` — ошибка `review already running`.

### Сигнатуры

```js
export class PipelineSoftFail extends Error {}
export function createReviewPipeline(cfg, deps): { run, runDetailed }

export const REVIEW_RUN_TOOL    // tools/list
export const REVIEW_FINDINGS_TOOL
export async function handleReviewMcpRequest(request, extra?): JsonRpcMessage
```

`extra.createPipeline` / `extra.github` / `extra.notify` — DI для тестов и логов в
`notifications/message`. Ключ LLM — только env процесса, не аргумент тулзы.

## Миграция

Аддитивно: CLI `pr-review` и structure MCP не меняют контракт. Новый bin в `package.json`.
`--head-sha` у `parseCli` опционален и пуст по умолчанию. Потребитель, который читал только
`structure_context`, ничего не ломает.

## Риски и что не делаем

- **Превью примут за гейт.** Имя поля `previewWouldBlock` + `kind: review_preview`; в
  документации явно: CI остаётся судьёй.
- **Таймаут клиента.** Прогон 5–20 мин (`REVIEW_TIME_BUDGET_MIN`). Клиентский timeout MCP
  должен быть не меньше; иначе агент видит обрыв, не вердикт. Логи — через
  `notifications/message`.
- **stdout.** Любой `console.log` в dry-run ломает кадры. Исправляем в пайплайне.
- **Не делаем:** GitHub-запись из MCP; тулзы `finding_explain` / `fp_check` / `prescan`;
  смешение с `structure_context`; `@modelcontextprotocol/sdk`; Rust-порт оркестрации;
  неявный «latest» без пина.

## Оценка

5 задач: RFC (этот документ); хуки пайплайна; `lib/review-mcp.mjs`; общий stdio + bin;
тесты и документация. Порядок жёсткий: без `runDetailed` и безопасного `die` MCP нельзя
подключать к пайплайну.
