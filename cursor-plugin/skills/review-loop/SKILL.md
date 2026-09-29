---
name: review-loop
description: Порядок вызова тулзов ai-reviewer в IDE. Дешёвые стадии до MoA. Превью не гейт.
type: guide
status: active
owner: ai-reviewer
---

# Контур ревью в чате

Тулзы `ai-reviewer` — превью для агента, не merge-гейт. Merge блокирует только CI `pr-review` (`exit 1`). Поле вердикта — `previewWouldBlock`, не `wouldBlock`.

Транспорт — **stdio**, не OAuth. **Не вызывай `mcp_auth`** у namespace `plugin-ai-reviewer-*` / `ai-reviewer*` / `user-*-review`. Кнопка Authenticate на этом сервере не нужна и не чинит spawn. Если статус `loading` / `error` / в чате только `mcp_auth` — это сломанный stdio (нет engine, нет node, hang handshake), а не «нужно авторизоваться»: логи MCP, `cursor-plugin/bin/install-local.sh`, Reload Window. Не читай файлы репозитория вместо тулзов, пока не исчерпан stdio.

Обрыв `tools/call` (~30 с) **не** значит, что сервер мёртв: `review_run` с `wait: false` уже пишет sidecar. Не вызывай `mcp_auth`. Полли `review_findings` по `jobId`, пока `status: running`. Не правь nits и не «чини» дифф вслепую, пока нет findings.

Один MCP-канал на один `REVIEW_MCP_ROOT`. Не держи plugin и user MCP параллельно на одном корне: два мьютекса, два sidecar. Второй репозиторий — user/W MCP (`examples/cursor/mcp.json`), не второй plugin-сервер.

Cloud Agents не видят local stdio MCP — не обходи это HTTP-сервером.

## Порядок

1. `review_route` — skip/light/full.
2. `review_prescan` — eslint + ast-grep, без LLM. Пустой результат на Rust (`skipped: no-eslint`) — ожидаемо, не баг транспорта.
3. `fp_check` — не чини код, если `match: true`.
4. По желанию `fingerprint_check` — код под находкой ещё там?
5. `review_run` — **последним**, только когда дешёвое не ответило. Единственный вызов с LLM. Всегда **`wait: false`**. Забери `jobId` из stub (`status: running`).
6. Хвост — `review_findings` по `jobId` (пока running — пустой список; после completed можно и `runFingerprint`). Sidecar на диске переживает и обрыв RPC, и рестарт процесса.

Не запускай MoA на каждый edit. Хуков `afterFileEdit` нет и быть не должно.

Дифф — файл внутри `REVIEW_MCP_ROOT`. `headSha` обязателен; грязное дерево — `uncommitted: true`.

Structure — отдельный сервер `ai-reviewer-structure` (тулза `structure_context`), не смешивать с ревью.
