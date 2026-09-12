---
description: Превью полного ревью (dry-run MoA). Не merge-гейт.
type: guide
status: active
owner: ai-reviewer
---

Сделай превью ревью текущего диффа через MCP `ai-reviewer`. Это не merge-гейт: `previewWouldBlock` не блокирует merge; судья — CI `pr-review` (`exit 1`).

1. Сохрани `git diff` (или `origin/main...HEAD`) в файл **внутри** `REVIEW_MCP_ROOT`, например `.ai-review/mcp/work.patch`. Не передавай дифф аргументом тулзы.
2. Возьми пин: `git rev-parse HEAD`. Если рабочее дерево грязное — передай `uncommitted: true`.
3. Сначала вызови `review_route` с тем же `diffPath` и `headSha`. Если маршрут `skip` или `light` — **не** вызывай `review_run`, пока пользователь явно не попросит полный MoA.
4. Иначе вызови `review_run` с тем же `headSha` и **`wait: false`**. Ответ — stub с `jobId` (`status: running`). Обрыв RPC ~30 с не значит, что MoA умер: полли `review_findings` по `jobId`. Не вызывай `mcp_auth`. Не правь код, пока findings пустые и статус `running`.
5. Хвост находок — `review_findings` по `jobId` (после completed можно `runFingerprint`). Не чини код только из-за `fp_check.match: true`. Cloud Agents не видят local stdio MCP.
