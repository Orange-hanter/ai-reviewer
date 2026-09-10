---
name: review-loop
description: Порядок вызова тулзов ai-reviewer в IDE. Дешёвые стадии до MoA. Превью не гейт.
type: guide
status: active
owner: ai-reviewer
---

# Контур ревью в чате

Тулзы `ai-reviewer` — превью для агента, не merge-гейт. Merge блокирует только CI `pr-review` (`exit 1`). Поле вердикта — `previewWouldBlock`, не `wouldBlock`.

Cloud Agents не видят local stdio MCP — не обходи это HTTP-сервером.

## Порядок

1. `review_route` — skip/light/full.
2. `review_prescan` — eslint + ast-grep, без LLM.
3. `fp_check` — не чини код, если `match: true`.
4. По желанию `fingerprint_check` — код под находкой ещё там?
5. `review_run` — **последним**, только когда дешёвое не ответило. Единственный вызов с LLM. Timeout ≥ `REVIEW_TIME_BUDGET_MIN`.
6. Хвост — `review_findings` по `runFingerprint` (sidecar на диске, переживает рестарт).

Не запускай MoA на каждый edit. Хуков `afterFileEdit` нет и быть не должно.

Дифф — файл внутри `REVIEW_MCP_ROOT`. `headSha` обязателен; грязное дерево — `uncommitted: true`.

Structure — отдельный сервер `structure_context`, не смешивать с ревью.
