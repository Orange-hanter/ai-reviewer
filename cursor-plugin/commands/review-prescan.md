---
description: Детерминированный eslint+ast-grep prescan без LLM.
type: guide
status: active
owner: ai-reviewer
---

Запусти только `review_prescan` на текущем диффе. LLM и `review_run` не вызывай.

1. Сохрани дифф в файл внутри `REVIEW_MCP_ROOT` (например `.ai-review/mcp/work.patch`).
2. Пин: `git rev-parse HEAD`; на грязном дереве — `uncommitted: true`.
3. Вызови `review_prescan`. Это не гейт: находки advisory.
