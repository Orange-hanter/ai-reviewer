---
type: guide
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-09-12
---

# Cursor Plugin для ai-reviewer

Обёртка над двумя stdio MCP движка: slash-команды, skill и правило. **Не merge-гейт** и не VS Code extension. Движок в плагин не кладётся.

Публичный Marketplace требует open source; этот репозиторий `UNLICENSED` → только local или team marketplace.

Cloud Agents не видят local stdio MCP. Кнопка **Authenticate** этому серверу не нужна (это не OAuth). Не вызывайте `mcp_auth`.

Один plugin-процесс — один `REVIEW_MCP_ROOT` на все окна. Два потребителя сразу — workspace MCP в каждом репо, без дубля plugin. Одновременно plugin MCP и workspace MCP с именами `ai-reviewer` / `ai-reviewer-structure` — запрещено: для разработки движка достаточно **W или P-install**, не оба.

## Локальная установка (P-engine-install)

Единственная команда. Configure не нужен. Cursor не подхватывает symlink наружу из `~/.cursor/plugins/local` — скрипт копирует, не `ln -s`.

```bash
/path/to/ai-reviewer/cursor-plugin/bin/install-local.sh
```

Скрипт делает `rsync -a --delete` в `~/.cursor/plugins/local/ai-reviewer/` и пишет `.engine-root` (абсолютный путь клона). Секреты и машинные пути в `mcp.json` не кладёт.

После смены файлов в репо — ту же команду ещё раз.

В Customize → Plugins включите **Include third-party Plugins, Skills, and other configs**,
затем **Developer: Reload Window**.

Configure (**Customize → Plugins → Configure**) — опциональный override: `AI_REVIEWER_ROOT` / `REVIEW_MCP_ROOT` перебивают `.engine-root`. Схема в `plugin.json` — JSON Schema `{ type: object, properties }` и `required: []`
(иначе Cursor держит loading + Authenticate). В `mcp.json` только плейсхолдеры `${…}`:

- `AI_REVIEWER_ROOT` — клон пакета; пусто — `.engine-root`
- `REVIEW_MCP_ROOT` — репозиторий ревью; пусто — тот же, что engine
- `REVIEW_PROFILE` — опционально
- `OLLAMA_API_KEY` — из Configure, в файлы плагина не кладётся; для `initialize` / дешёвых тулзов не нужен

`mcp.json` вызывает `/bin/sh` и `./bin/review-mcp.sh` в **git-шаблоне** (путь от корня плагина).
`install-local.sh` в установленной копии подставляет абсолютный `$dest/bin/review-mcp.sh`:
cwd plugin MCP у Cursor — часто другое окно, не каталог плагина; относительный `./bin` даёт
`Connection closed` и кнопку Authenticate. Лаунчер сам находит Node и клон движка. `type` — `stdio`.

В Agent chat: `/review-prescan`, `/review-preview`.

## Workspace MCP (W)

Шаблон потребителя: [`examples/cursor/mcp.json`](../examples/cursor/mcp.json) — `REVIEW_MCP_ROOT=${workspaceFolder}`, лаунчер из `${env:AI_REVIEWER_ROOT}/cursor-plugin/bin/…`.

Этот клон: [`.cursor/mcp.json`](../.cursor/mcp.json) с теми же именами серверов и `${workspaceFolder}` на оба корня. Если plugin MCP уже живой, workspace-файл даст дубль — оставьте один канал.

## Два сервера

| Сервер | Bin | Тулзы |
|--------|-----|--------|
| `ai-reviewer` | `bin/pr-review-mcp.mjs` | `review_route`, `review_prescan`, `fp_check`, `fingerprint_check`, `review_artifact`, `review_run`, `review_findings` |
| `ai-reviewer-structure` | `bin/pr-review-structure-mcp.mjs` | `structure_context` |

`STRUCTURE_MCP_ROOT` = тот же `REVIEW_MCP_ROOT` (репозиторий потребителя), не клон движка.

Холодный старт и матрица вариантов — [RFC 10](../proposals/10-mcp-cold-start.md).

## CI остаётся судьёй

CI `pr-review` с `exit 1` — единственный merge-гейт. MCP всегда `dryRun`, GitHub не пишет. Хуков на `afterFileEdit` нет.
