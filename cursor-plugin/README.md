---
type: guide
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-09-10
---

# Cursor Plugin для ai-reviewer

Обёртка над двумя stdio MCP движка: slash-команды, skill и правило. **Не merge-гейт** и не VS Code extension. Движок в плагин не кладётся — нужен клон пакета (`AI_REVIEWER_ROOT`).

Публичный Marketplace требует open source; этот репозиторий `UNLICENSED` → только local или team marketplace.

Cloud Agents не видят local stdio MCP.

## Локальная установка

```bash
mkdir -p ~/.cursor/plugins/local
ln -s /abs/path/to/ai-reviewer/cursor-plugin ~/.cursor/plugins/local/ai-reviewer
```

В переменных плагина:

- `AI_REVIEWER_ROOT` — абсолютный путь к клону `ai-reviewer`
- `REVIEW_MCP_ROOT` — репозиторий, который ревьюите (корень confinement)
- `REVIEW_PROFILE` — опционально, путь к профилю потребителя
- `OLLAMA_API_KEY` — из окружения IDE, в файлы плагина не кладётся

Перезапустите Cursor. В Agent chat: `/review-prescan`, `/review-preview`.

## Два сервера

| Сервер | Bin | Тулзы |
|--------|-----|--------|
| `ai-reviewer` | `bin/pr-review-mcp.mjs` | `review_route`, `review_prescan`, `fp_check`, `fingerprint_check`, `review_artifact`, `review_run`, `review_findings` |
| `ai-reviewer-structure` | `bin/pr-review-structure-mcp.mjs` | `structure_context` |

`STRUCTURE_MCP_ROOT` = тот же `REVIEW_MCP_ROOT` (репозиторий потребителя), не клон движка.

## CI остаётся судьёй

CI `pr-review` с `exit 1` — единственный merge-гейт. MCP всегда `dryRun`, GitHub не пишет. Хуков на `afterFileEdit` нет.
