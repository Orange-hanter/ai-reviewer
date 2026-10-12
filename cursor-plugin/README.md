---
type: guide
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-10-12
---

# Cursor Plugin для ai-reviewer

Обёртка над двумя stdio MCP движка: slash-команды, skill и правило. **Не merge-гейт** и не VS Code extension. Движок в плагин не кладётся.

Публичный Marketplace требует open source; этот репозиторий `UNLICENSED` → только local или team marketplace.

Cloud Agents не видят local stdio MCP. Кнопка **Authenticate** этому серверу не нужна (это не OAuth). Не вызывайте `mcp_auth`.

Один plugin-процесс — один `REVIEW_MCP_ROOT` на все окна. Два потребителя сразу — workspace/user MCP в каждом репо ([`examples/cursor/mcp.json`](../examples/cursor/mcp.json): абсолютный лаунчер engine + `REVIEW_MCP_ROOT=${workspaceFolder}`), без второго plugin-сервера. Одновременно plugin MCP и user/workspace MCP с одним корнем — два мьютекса и два sidecar; оставьте один канал. Для разработки движка достаточно **W или P-install**, не оба.

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
- `AI_REVIEWER_BIN` — prebuilt-бинарь `pr-review` (RFC 16); пусто — сборка клона, затем node
- `REVIEW_MCP_ROOT` — репозиторий ревью; пусто — тот же, что engine
- `REVIEW_PROFILE` — опционально
- `OLLAMA_API_KEY` — из Configure, в файлы плагина не кладётся; для `initialize` / дешёвых тулзов не нужен

`mcp.json` в **git-шаблоне** вызывает `/bin/sh -c` с `$HOME/.cursor/plugins/local/ai-reviewer/bin/review-mcp.sh`
(не `./bin` от cwd окна). `install-local.sh` в установленной копии подставляет абсолютный
`$dest/bin/review-mcp.sh`. cwd plugin MCP у Cursor — часто другое окно; относительный `./bin`
даёт `Connection closed`. Лаунчер сам находит бинарь (или Node) и клон движка. `type` — `stdio`.

## Рантайм: бинарь `pr-review` или node

Лаунчеры зовут подкоманды единого бинаря ([RFC 16](../proposals/16-standalone-rust-binary.md)):
`pr-review review-mcp` и `pr-review structure-mcp`. Порядок поиска бинаря:

1. `AI_REVIEWER_BIN` — явный путь (например, распакованный из релиза). Клон движка тогда
   не обязателен; `REVIEW_MCP_ROOT` по умолчанию — cwd. Задан, но не исполняемый — fail-fast,
   без тихого отката на node.
2. `rust/target/release/pr-review`, затем `rust/target/debug/pr-review` клона (`cargo build`).
3. Откат на node-путь (`bin/pr-review-mcp.mjs`, `bin/pr-review-structure-mcp.mjs`) — только
   пока JS-ядро не удалено; удаление — задача T33, после неё остаются пункты 1–2.

`AI_REVIEWER_MCP_RUNTIME` фиксирует выбор: `auto` (по умолчанию, порядок выше), `bin` (только
бинарь, без него — ошибка), `node` (только JS; пригодится, если debug-сборка клона устарела).

В Agent chat: `/review-prescan`, `/review-preview`, `/fp-check`, `/structure-context`.

## Workspace MCP (W)

Шаблон потребителя: [`examples/cursor/mcp.json`](../examples/cursor/mcp.json) — `REVIEW_MCP_ROOT=${workspaceFolder}`, лаунчер из `${env:AI_REVIEWER_ROOT}/cursor-plugin/bin/…`.

Этот клон: [`.cursor/mcp.json`](../.cursor/mcp.json) с теми же именами серверов и `${workspaceFolder}` на оба корня. Если plugin MCP уже живой, workspace-файл даст дубль — оставьте один канал.

## Два сервера

| Сервер | Подкоманда (откат до T33) | Тулзы |
|--------|-----|--------|
| `ai-reviewer` | `pr-review review-mcp` (`bin/pr-review-mcp.mjs`) | `review_route`, `review_prescan`, `fp_check`, `fingerprint_check`, `review_artifact`, `review_run`, `review_findings`; при `REVIEW_MCP_EXTRA=1` ещё `blast_scope`, `actionability_explain`, `review_metrics` |
| `ai-reviewer-structure` | `pr-review structure-mcp` (`bin/pr-review-structure-mcp.mjs`) | `structure_context` |

`STRUCTURE_MCP_ROOT` = тот же `REVIEW_MCP_ROOT` (репозиторий потребителя), не клон движка.

Холодный старт — [RFC 10](../proposals/10-mcp-cold-start.md). Долгий MoA в чате (`wait: false`, `jobId`) — [RFC 12](../proposals/12-mcp-long-run.md). Пустой `review_prescan` на Rust (`no-eslint`) — норма, не баг MCP.

## CI остаётся судьёй

CI `pr-review` с `exit 1` — единственный merge-гейт. MCP всегда `dryRun`, GitHub не пишет. Хуков на `afterFileEdit` нет.
