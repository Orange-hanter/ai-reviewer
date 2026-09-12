---
type: design
status: review
owner: ai-reviewer
source_of_truth: true
created: 2026-09-10
last_updated: 2026-09-12
---

# 09 — Cursor Plugin и агентные стадии ревью-MCP

> Категория: 🔵 Архитектура · Риск: средний · Зависимости: RFC 01 (граница structure), RFC 08 (review MCP)

## Контекст

RFC 08 дал агенту полный MoA из чата (`review_run` / `review_findings`) как pinned dry-run.
Этого мало для IDE-контура: агент либо сразу жжёт LLM, либо не знает, что дифф — docs-only,
что находка — известный FP, что код под отпечатком уже ушёл. «Все функции `lib/`» в MCP
класть нельзя: это внутренности движка, не стадии пайплайна.

«Полноценный плагин» здесь — Cursor Plugin (slash-команды, skill, rule, settings + оба
stdio MCP), не VS Code sidebar и не `@cursor/sdk`. Публичный Marketplace требует open source;
репозиторий `UNLICENSED` / `private: true` → local (`~/.cursor/plugins/local`) или team
marketplace. Движок в плагин не кладётся (бинарники marketplace не принимает; плюс
заявленный ноль npm-зависимостей).

## Текущее состояние

Сверено по коду, не по памяти:

- Ревью-MCP: [`lib/review-mcp.mjs`](../lib/review-mcp.mjs). Тулзы только `review_run` и
  `review_findings`. `review_findings` читает in-memory `Map` на 8 прогонов (`STORE_CAP`):
  рестарт процесса → `unknown runFingerprint`.
- Пин `headSha` — клиентский штамп `^[0-9a-fA-F]{7,64}$`. Сверка с `git rev-parse HEAD` нет.
  В `review_run` сравнение с артефактом — `args.headSha.slice(0, 40)` (ломает SHA-256 пин).
- `priorExportPath` реализован через `loadPriorExport` + silent GitHub, юнит-теста нет.
- `limit` клампится схемой, отдельного теста нет.
- Structure MCP остаётся отдельным процессом ([`lib/structure-mcp.mjs`](../lib/structure-mcp.mjs));
  [`test/structure-boundary.test.mjs`](../test/structure-boundary.test.mjs) запрещает импорт
  `structure*` из `lib/review-mcp.mjs` и `bin/pr-review-mcp.mjs`.
- Обёртки стадий уже есть и не копируются: `routeDiff` ([`lib/router.mjs`](../lib/router.mjs)),
  `createPrescanRunner` ([`lib/prescan-runner.mjs`](../lib/prescan-runner.mjs)),
  `matchFpPatterns` ([`lib/fp-patterns.mjs`](../lib/fp-patterns.mjs)),
  `findingFingerprint` / `filterStalePriorFindings` ([`lib/findings.mjs`](../lib/findings.mjs)),
  `scoreActionability` ([`lib/actionability.mjs`](../lib/actionability.mjs); **не**
  `routeFindings` — мутирует находки), `createGraphifyClient` + `suggestedScopeFiles`.
- [`bin/pr-review-mcp.mjs`](../bin/pr-review-mcp.mjs) не делает `chdir` в `REVIEW_MCP_ROOT`:
  eslint/ast-grep и `readChangedFiles` смотрят `process.cwd()`.
- Cursor Plugin в репозитории нет.

## Предложение

Расширить тот же `pr-review-mcp` стадиями пайплайна и обернуть оба сервера Cursor Plugin.
`REVIEW_MCP_PROTOCOL_VERSION` остаётся **1** (новые `kind` аддитивны). `EXPORT_VERSION` не
трогаем.

### Замороженные решения

- Два MCP-сервера. Structure не импортировать из `lib/review-mcp.mjs`. Не сливать `tools/list`.
- `review_run` — единственный вызов с LLM и единственный, кто берёт мьютекс `reviewRunning`.
  Текст ошибки мьютекса: `review already running`.
- Имена тулзов — как ниже. Не синонимы (`route_diff`, `check_fp`).
- Ответы агенту: `previewWouldBlock`, никогда top-level `wouldBlock`.
- GitHub: ни один новый тулз не создаёт клиент. `review_run` — только `createSilentGithub`.
- Дешёвые тулзы бросают `Error` → `rpcError` `-32602`. `PipelineSoftFail` — только `review_run`.
- Хуки `afterFileEdit` / `afterTabFileEdit` не вызывают `review_run`. В v1 `hooks/` нет.
- Cloud Agents не видят local stdio MCP — одной фразой в RFC и skill, без HTTP-обхода.
- Плагин не содержит бинарь движка. Путь: `AI_REVIEWER_ROOT`.

### Общие инварианты

- Корень: `extra.root || extra.env?.REVIEW_MCP_ROOT || process.env.REVIEW_MCP_ROOT || cwd()`.
  Пути — `resolveContainedPath`; ошибка `path escapes allowed root: …`.
- Пин `headSha`: regex `^[0-9a-fA-F]{7,64}$`. Сравнение **префиксное**, case-insensitive:
  `a.startsWith(b) || b.startsWith(a)`, общая длина ≥ 7.
- Cwd: `bin/pr-review-mcp.mjs` при заданном `REVIEW_MCP_ROOT` делает `process.chdir(root)`
  один раз. `lib/review-mcp.mjs` `chdir` не вызывает.
- Git (только `review_run`, `review_route`, `review_prescan`): DI `extra.git`, timeout 5s.
  1. Не git → сверку HEAD пропускаем, `headSha` обязателен.
  2. Git и `uncommitted !== true`: `rev-parse HEAD` prefix-совпадает, иначе
     `headSha does not match HEAD`. Грязное дерево → `working tree dirty; pass uncommitted: true`.
  3. `uncommitted: true`: HEAD не сверяем; источник истины — `diffPath`.
  4. `review_findings` / `review_artifact` git не трогают.
- LLM-ключ только из env процесса, не из аргументов тулза.
- `title`+`body` суммарно ≤ 8192, иначе `text too long`.
- `tokenBudget` 256–32000, default 4000.

### Фаза A — дыры v1

Sidecar `{root}/.ai-review/mcp/runs/{runFingerprint}.json` после успешного `review_run`.
In-memory Map — кэш; miss → диск; нет файла → `unknown runFingerprint`. Cap на диске: 32
файла, вытеснение по `mtime`. В sidecar полный клиентский `headSha` (без slice). Проверку
`artifact.headSha !== args.headSha.slice(0, 40)` убрать, заменить prefix-match.

`priorExportPath` и clamp `limit` покрыть тестом.

### Фаза B — дешёвый контур (без LLM, без мьютекса)

| Тулза | Required | Ответ `kind` |
|---|---|---|
| `review_route` | `diffPath`, `headSha` | `review_route` — `routeDiff` |
| `review_prescan` | `diffPath`, `headSha` | `review_prescan` — slim findings, `extra.prescanExec` в тестах |
| `fp_check` | `title`, `body`, `file` | `fp_check` — `matchFpPatterns`, пустой `file` → `file is required` |
| `fingerprint_check` | `file`, `line`, `priorFp`, `diffPath` | `fingerprint_check` — `still_present` / `stale` / `unknown_line` |
| `review_artifact` | `path` | `review_artifact` — slim обёртка, > 8 MiB → `artifact too large` |

`tools/list` без флага: `review_run`, `review_findings` + пять выше.

### Фаза C — `REVIEW_MCP_EXTRA=1` (`envFlag`, default off)

Без флага тулзы нет в `tools/list`, `tools/call` → `unknown tool: …`.

- `blast_scope` — `createGraphifyClient` + `suggestedScopeFiles`, max 50 файлов.
  Нет graphify → `files: []`, `skipReason: graphify_unavailable`, не throw.
- `actionability_explain` — только `scoreActionability`, объект находки не мутировать.
- `review_metrics` — confined JSONL, `parseJsonl` + `aggregateMetrics`, битые строки skip.

### Cursor Plugin

Каталог [`cursor-plugin/`](../cursor-plugin/) в этом репо (не `.cursor/` потребителя):

```
cursor-plugin/
  .cursor-plugin/plugin.json
  mcp.json
  bin/review-mcp.sh
  bin/structure-mcp.sh
  bin/mcp-env.sh
  bin/install-local.sh
  commands/review-preview.md
  commands/review-prescan.md
  commands/fp-check.md
  commands/structure-context.md
  skills/review-loop/SKILL.md
  rules/preview-not-gate.mdc
  README.md
```

`mcp.json` — ровно два сервера, `type: stdio`. Command — `/bin/sh`, `args` —
`./bin/review-mcp.sh` / `./bin/structure-mcp.sh` (путь от корня **плагина**, не
`${AI_REVIEWER_ROOT}/…`; GUI Cursor часто без `node` в PATH; лаунчер ищет бинарь
и клон движка сам, см. [RFC 10](10-mcp-cold-start.md)). `OLLAMA_API_KEY` только как
`${OLLAMA_API_KEY}`. `GITHUB_TOKEN` не класть. `STRUCTURE_MCP_ROOT` = `${REVIEW_MCP_ROOT}`
(репозиторий потребителя, не клон движка). `variables` в `plugin.json` — JSON Schema
`{ type: object, properties }` и `required: []` (иначе Cursor не спавнит stdio
и показывает Authenticate).

Slash-команды — Agent chat, не Command Palette. Skill: route → prescan → fp_check →
(опц.) fingerprint_check → `review_run` последним. Rule always-on: находки IDE advisory;
merge блокирует только CI `pr-review` (`exit 1`).

## Миграция

Аддитивно для RFC 08: старые клиенты `review_run` / `review_findings` продолжают работать
(`kind: review_preview` не меняется). Появляется git-сверка: клиент на грязном дереве
обязан передать `uncommitted: true`. Sidecar — новый файл, экспорт `tool.json` не
контракт paging.

Плагин ставится локально, CI-гейт не меняется.

## Риски и что не делаем

Non-goals: GitHub write; один MCP-сервер; marketplace binaries; `@cursor/sdk` / VS Code
webview; MCP Apps; смена namespace маркеров; хуки на каждый edit; live Ollama в CI.

- **Превью примут за гейт.** Rule + skill + `previewWouldBlock`.
- **MoA на каждый edit.** Хуков нет; skill запрещает.
- **Cwd.** Без `chdir` в bin prescan смотрит не тот репозиторий.
- **Cloud Agents.** Local stdio MCP им недоступен; не чиним HTTP-обходом.

Ручной чеклист (не CI): local plugin → `/review-prescan` → `/review-preview` на крошечном патче.

## Оценка

4 задачи: RFC (этот документ); фаза A (sidecar, git, тесты `priorExportPath`/`limit`);
фаза B (пять тулзов + тесты); `cursor-plugin/` + docs + фаза C за флагом.
