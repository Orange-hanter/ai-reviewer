---
type: design
status: active
owner: ai-reviewer
source_of_truth: true
created: 2026-09-12
last_updated: 2026-09-29
---

# 12 — Долгий MoA в IDE: jobId, wait, spawn без cwd окна

> Категория: 🔵 Архитектура · Риск: средний · Зависимости: RFC 08 (протокол), RFC 09 (sidecar/skill), RFC 10 (лаунчер, non-goal Authenticate). Заморозку 08/09 не переоткрывает: два stdio-сервера, `previewWouldBlock` не гейт, `review_run` единственный LLM/мьютекс, движок не в бандле плагина. RFC 11 (HTTP/OAuth) — другой контур, к таймауту stdio не относится. `REVIEW_MCP_PROTOCOL_VERSION` остаётся **1** (только аддитивные поля). `EXPORT_VERSION` не трогаем.

## Контекст

Живой прогон в Cursor (2026-09-12, потребитель ZeusLogic): дешёвый контур дошёл (`review_route` → `full`, `review_prescan` пустой на Rust — ожидаемо). `review_run` стартовал MoA (~28 чанков). Хост оборвал `tools/call` ~30 с. Бюджет движка — `REVIEW_TIME_BUDGET_MIN` (по умолчанию 20 мин, RFC 08 уже предупреждал: клиентский timeout MCP должен быть не меньше). Sidecar к моменту abort не существовал: `runFingerprint` считается **после** артефакта. Агент не мог вызвать `review_findings`, чинил вслепую; мьютекс `reviewRunning` держался до конца прогона или смерти процесса.

Параллельно plugin MCP с `args: ["./bin/review-mcp.sh"]` резолвился в cwd чужого окна (`/Users/dakh/Git/_my/ZeusLogic/bin/review-mcp.sh` → `No such file` → `Connection closed`). Рабочий канал — user MCP `zeuslogic-review`: абсолютный `node` + `bin/pr-review-mcp.mjs`. Кнопка Authenticate / `mcp_auth` stdio не чинит (RFC 10).

Нужен аддитивный контракт: агент получает стабильный id **до** LLM и поллит sidecar; живой spawn плагина не зависит от cwd окна; один MCP-канал на потребителя.

Пока этот RFC не реализован: полный MoA в IDE — CLI `pr-review.mjs --dry-run`, не chat RPC.

## Текущее состояние

> Снимок кода до реализации. Фазы B–D сделаны в `3f839aa`: `wait`/`jobId` есть в `lib/review-mcp-tools.mjs`, stub sidecar пишется до LLM, git-шаблон `mcp.json` спавнит `$HOME/.cursor/plugins/local/ai-reviewer/bin/…`.

Сверено по коду, не по памяти:

- [`lib/review-mcp-tools.mjs`](../lib/review-mcp-tools.mjs) `REVIEW_RUN_TOOL`: required `diffPath`+`headSha`; полей `wait` / `jobId` нет.
- `REVIEW_FINDINGS_TOOL`: required только `runFingerprint`; `jobId` нет. Описание: «previous review_run».
- [`lib/review-mcp.mjs`](../lib/review-mcp.mjs) `callReviewRun` (~334–403): `reviewRunning` → пин → `await pipeline.runDetailed()` → `fitPreview` → `persistRun` → return. `finally` снимает мьютекс **в том же RPC**. Повторный вызов при живом мьютексе — `review already running`.
- `fitPreview` (~280–302): `runFingerprint = sha256(canonicalJson(artifact | {headSha, reason}))` **после** прогона. Пока MoA идёт, отпечатка нет.
- `persistRun` (~210–225): файл `{root}/.ai-review/mcp/runs/{runFingerprint}.json`. Stub `status: running` не пишется.
- `callReviewFindings` (~406–414): нет sidecar → `unknown runFingerprint`.
- `callReviewArtifact` (~583–584): только `path` к JSON-экспорту, не sidecar/job.
- RFC 08 § риски: клиентский timeout ≥ `REVIEW_TIME_BUDGET_MIN`; иначе агент видит обрыв, не вердикт. Cursor chat это не выполняет (~30 с).
- [`cursor-plugin/mcp.json`](../cursor-plugin/mcp.json): `"args": ["./bin/review-mcp.sh"]`. [`install-local.sh`](../cursor-plugin/bin/install-local.sh) переписывает dest на абсолютный `$dest/bin/…`. Тест `относительный ./bin из чужого cwd не находит лаунчер` это фиксирует; живой Cursor всё ещё может спавнить git-шаблон / `./bin`.
- Skill [`cursor-plugin/skills/review-loop/SKILL.md`](../cursor-plugin/skills/review-loop/SKILL.md): `review_run` последним, timeout ≥ бюджета; не говорит `wait: false`, не говорит поллить после abort RPC, запрет `mcp_auth` уже есть (RFC 10).

## Предложение

### Инвариант процесса

Процесс stdio живёт дольше одного `tools/call`. Если хост только бросает ожидание JSON-RPC и **не** закрывает stdin — MoA обязан доехать и перезаписать sidecar. Если abort убивает процесс (stdin EOF → цикл [`lib/mcp-stdio.mjs`](../lib/mcp-stdio.mjs) завершается) — это **не** первый срез: отдельная задача «detach child», не смешивать с `wait`.

### `jobId`

64 hex, **до** LLM:

```text
jobId = sha256(canonicalJson({
  headSha,
  uncommitted: Boolean(uncommitted),  // отсутствует → false
  diffPath,                           // клиентский путь, не realpath
  diffHash,                           // sha256(utf8 текста диффа)
}))
```

`canonicalJson` — существующий из [`lib/util.mjs`](../lib/util.mjs) (ключи объектов сортируются). Sidecar-ключ на диске = `jobId`, не хеш артефакта.

`runFingerprint` после `completed` / `degraded` — по-прежнему `sha256(canonicalJson(artifact))`, как сейчас. Тело sidecar всегда несёт оба поля (`jobId` обязателен; `runFingerprint` пустая строка пока `running`).

### `wait` у `review_run`

Аддитивное свойство, default **`true`** (тесты и любой клиент без поля не ломаются).

| `wait` | Поведение |
|--------|-----------|
| `true` (default) | Как сейчас: RPC ждёт MoA. **До** `runDetailed` всё равно пишется stub sidecar (чтобы abort/рестарт не терял id). Ответ — `completed`/`degraded` с `jobId` + `runFingerprint`. |
| `false` | После пина, мьютекса и stub — сразу вернуть stub (`status: "running"`). MoA на event loop **того же** процесса. Мьютекс снимается в `finally` фонового прогона, не в момент RPC-ответа. |

Схема `REVIEW_RUN_TOOL.inputSchema.properties.wait`: `{ type: "boolean" }`. `additionalProperties: false` сохраняется.

### Stub sidecar (до LLM)

Путь: `{REVIEW_MCP_ROOT}/.ai-review/mcp/runs/{jobId}.json`.

Ответ `wait: false` и тело stub (укладывается в `tokenBudget`):

```json
{
  "kind": "review_preview",
  "protocolVersion": 1,
  "status": "running",
  "jobId": "<64hex>",
  "runFingerprint": "",
  "headSha": "<пин>",
  "previewWouldBlock": false,
  "blockers": [],
  "findingsPreview": [],
  "startedAt": "<ISO-8601>"
}
```

`previewWouldBlock` на running **всегда false**: превью не гейт, незавершённый прогон не «блокирует».

### Завершение

Перезапись того же файла: `status: "completed" | "degraded"`, заполненный `runFingerprint`, превью как `fitPreview` сейчас (`blockers`, `findingsPreview`, `truncated`, `artifactPath`, `counts`, `route`, `strong`, `reason`). In-memory Map индексирует **оба** ключа (`jobId` и `runFingerprint`). На диске один файл `{jobId}.json`; lookup по `runFingerprint` — память, иначе сканирование каталога sidecar (cap 32, как сейчас). Не писать второй файл `{runFingerprint}.json`: eviction тогда разъезжался бы.

### Повторный `review_run`

1. Тот же пин (`jobId`) и sidecar `status: running` → вернуть существующий stub, **не** второй MoA.
2. Другой пин при живом мьютексе → ошибка `review already running` (текст как сейчас).
3. Sidecar `completed`/`degraded`, тот же `jobId` → идемпотентный ответ из sidecar, без нового MoA. Флага `force` в первом срезе нет.
4. Sidecar вытеснен eviction → новый прогон (новый stub с тем же `jobId`).

### `review_findings` / `review_artifact`

Аддитивно. Ровно одно из: `runFingerprint` | `jobId` (для artifact — ещё `path`, как сейчас). Пока `running` lookup **только** по `jobId` (`runFingerprint` пуст).

`review_findings` на running:

```json
{
  "kind": "review_findings",
  "protocolVersion": 1,
  "status": "running",
  "jobId": "<64hex>",
  "runFingerprint": "",
  "headSha": "<пин>",
  "offset": 0,
  "limit": 20,
  "total": 0,
  "findings": []
}
```

Это **не** `-32602` / `unknown runFingerprint`.

`review_artifact`: `path` остаётся каноном для JSON-экспорта. Аддитивно: `jobId` или `runFingerprint` без `path` читает sidecar и отдаёт `kind: review_artifact` (running → пустые blockers/findingsPreview, `previewWouldBlock: false`). Одновременно `path` и id — ошибка аргументов.

`notifications/progress` — не контракт первого среза. Поллинг `review_findings` / `review_artifact` достаточен.

### Spawn плагина без cwd окна

- Git-шаблон [`cursor-plugin/mcp.json`](../cursor-plugin/mcp.json) **не** живой spawn. `./bin/review-mcp.sh` резолвится в workspace (ZeusLogic → `…/ZeusLogic/bin/…`).
- Живой dest после `install-local.sh`: `command` `/bin/sh`, `args` = абсолютный `$dest/bin/review-mcp.sh` (уже так). Проверить, что Cursor не перезаписывает dest обратно на `./bin`.
- Git-шаблон: не оставлять `./bin` как канон живого spawn. Допустимо: `sh -c` на `$HOME/.cursor/plugins/local/ai-reviewer/bin/review-mcp.sh`, **или** шаблон только как исходник для инсталлятора + тест «cwd=чужой репозиторий → относительный spawn падает, абсолютный dest — handshake».
- `REVIEW_MCP_ROOT` в плагине — корень **потребителя**. Один plugin-сервер на все окна. Второй репозиторий — user/W MCP ([`examples/cursor/mcp.json`](../examples/cursor/mcp.json)), не второй plugin с тем же именем.

### Один канал на потребителя

Дубль plugin + user MCP на один `REVIEW_MCP_ROOT` = два процесса, два мьютекса, два sidecar, путаница id. Для ZeusLogic канон — user MCP (абсолютный `node` + `bin/pr-review-mcp.mjs`). Skill/README: не держать параллельно `plugin-ai-reviewer-ai-reviewer` с тем же корнем.

### Skill / агент

После `review_route` / `review_prescan`: `review_run` с `wait: false`. Abort RPC ≠ мёртвый сервер, ≠ повод звать `mcp_auth`. Поллить `review_findings` по `jobId`, пока `status: running`. Не править nits без findings. `previewWouldBlock` не гейт.

## Миграция

Аддитивно к RFC 08/09/10. Клиенты без `wait` получают sync. Старые `review_findings({ runFingerprint })` после completed работают. Новых required-полей нет. Eval/gate, `EXPORT_VERSION`, номер протокола — без изменений. Реализация B (код `lib/review-mcp.mjs`) не стартует, пока этот документ не в статусе `review`.

> Правило нарушено: B–D вышли в `3f839aa`, когда документ ещё был `draft`, и статус был
> поднят сразу до `active` задним числом (2026-09-29).

## Риски и что не делаем

- **Abort убивает stdin.** Первый срез это не чинит. Не маскировать смертью процесса «успешный» stub.
- **Два MoA.** Идемпотентность по `jobId` + мьютекс; не снимать мьютекс в RPC при `wait: false`.
- **Превью как гейт.** `previewWouldBlock` на running = false; merge по-прежнему только CI `exit 1`.
- **Не делаем в этом RFC:** HTTP MCP / OAuth (RFC 11); `mcp_auth` на stdio; смена `REVIEW_MCP_PROTOCOL_VERSION`; `force` re-run; `notifications/progress`; detach-процесс; потребительский ast-grep / `REVIEW_INCREMENTAL` как «лечение» пустого prescan на Rust (это не транспорт); хуки `afterFileEdit`; второй LLM вне `review_run`.

## Оценка

План `mcp-ide-resilience` в cod-doc (префикс `MCP-`):

- **A** MCP-001 — этот документ на диске и в БД (`draft` → `review` отдельно).
- **B** MCP-002…005 — stub/`jobId`, `wait`, lookup, контракт abort.
- **C** MCP-006…007 — spawn без `./bin` от cwd; тест чужого репозитория.
- **D** MCP-008…009 — skill; один канал на потребителя.
- **E** MCP-010…011 — docs про пустой Rust-prescan; оценка incremental (deferred, не блокирует B).
