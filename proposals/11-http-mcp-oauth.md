---
type: design
status: draft
owner: ai-reviewer
source_of_truth: true
created: 2026-09-12
last_updated: 2026-09-12
---

# 11 — HTTP MCP и OAuth Cursor (черновик направления)

> Категория: 🔵 Архитектура · Риск: высокий · Зависимости: RFC 08, RFC 09, RFC 10. **Не реализация.** Заморозку 08/09/10 не переоткрывает: два локальных stdio, `previewWouldBlock`, `review_run` единственный LLM/мьютекс, Authenticate на stdio — не путь.

## Контекст

Кнопка **Authenticate** и тулз `mcp_auth` в чате Cursor — OAuth-поток хоста для **удалённого HTTP-MCP** (токен в запросах к серверу в облаке: Linear, Slack, hosted GitHub MCP). Для локального stdio `ai-reviewer` (`/bin/sh` → `node`) аккаунта нет: процесс и так пользовательский.

Cursor рисует ту же кнопку, когда stdio **не `connected`** (нет лаунчера, `variables.required`, hang handshake). Клик: `logoutServer` + `OAuth credentials cleared` + reconnect `reason=authenticate` в тот же spawn — не чинит холодный старт ([RFC 10](10-mcp-cold-start.md)).

Вопрос, который этот черновик фиксирует, чтобы не изобретать заново: **стоит ли заводить настоящий HTTP MCP + OAuth как третий контур** (рядом с CI-гейтом и local stdio), или направление закрыто.

## Текущее состояние

Сверено по коду и логам, не по памяти:

- Транспорт агента — только stdio: [`lib/mcp-stdio.mjs`](../lib/mcp-stdio.mjs), [`bin/pr-review-mcp.mjs`](../bin/pr-review-mcp.mjs), [`bin/pr-review-structure-mcp.mjs`](../bin/pr-review-structure-mcp.mjs). HTTP/SSE MCP нет.
- Плагин — local copy, `UNLICENSED`; Marketplace binaries и `@cursor/sdk` — non-goal RFC 09.
- Cloud Agents **не видят** local stdio. RFC 09/10: не чинить HTTP-обходом.
- Ключ LLM (`OLLAMA_API_KEY`) — Configure / env, не OAuth Cursor. OAuth не подменяет провайдера моделей.
- Merge-гейт — только CI `pr-review` / `exit 1`. MCP остаётся `previewWouldBlock`.
- Живой ложный след (2026-09-12): `mcpprocess.log` — `reason=authenticate` на `plugin-ai-reviewer-ai-reviewer` при сломанном stdio.

## Набросок возможности (не контракт)

Имеет смысл **отдельным** RFC к реализации, только если цель явно не «починить Authenticate в IDE», а одно из:

1. **Cloud Agents / веб-агент Cursor** видят те же тулзы `review_route` / `review_run`, что local stdio.
2. **Командный hosted MCP**: один сервер ревью на несколько людей, квоты, аудит «кто вызвал MoA».
3. **Идентичность Cursor-аккаунта** на границе сервиса (не на локальном `node`).

Тогда контур такой:

- третий процесс/сервис: HTTP MCP (Streamable HTTP / SSE по спецификации MCP), не подмена stdio в плагине;
- OAuth 2.1 / динамическая регистрация клиента — как требует хост Cursor для `needsAuth`;
- секреты LLM по-прежнему на стороне сервиса (или per-user vault), не «токен Cursor = ключ Ollama»;
- те же инварианты 08: `dryRun`, GitHub не пишется, `previewWouldBlock`, два судьи, structure отдельно;
- local plugin **остаётся** stdio (RFC 10). OAuth не встраивается в `cursor-plugin/mcp.json` type stdio.

Это **новый пакет или сервис**, не строка в plugin MCP. Ноль runtime-зависимостей движка на диске потребителя не обязан распространяться на hosted-контур — это отдельный контракт поставки.

## Когда не делать

- Лечить холодный старт IDE, `mcp_auth`, кнопку Authenticate на `plugin-ai-reviewer-*`.
- Открыть Cloud Agents «на скорую» проксированием stdio через HTTP без модели угроз.
- Считать OAuth заменой `OLLAMA_API_KEY` / Configure.
- Смешивать merge-гейт и удалённый MCP (гейт остаётся CI).

## Открытые вопросы (когда/если открывать реализацию)

1. Кто хостит HTTP MCP (свой сервис vs процесс на машине разработчика с туннелем)? Туннель для Cloud Agents — всё ещё не local stdio, но и не командный продукт.
2. Модель секретов: один ключ Ollama на сервис vs per-user; откуда берётся `REVIEW_MCP_ROOT` без workspace folder.
3. Идентичность репозитория: git pin `headSha` с машины агента vs с клона на сервере.
4. Нужен ли тот же `REVIEW_MCP_PROTOCOL_VERSION = 1` или отдельная версия HTTP-контура.
5. Лицензия `UNLICENSED` и публичный OAuth app / Marketplace — конфликт с RFC 09.

## Миграция

Нет. Local stdio и RFC 10 не меняются, пока этот документ `draft`. Переход в `review` — только с ответами на открытые вопросы и явной целью (Cloud Agents **или** hosted team), не «раз OAuth кнопка есть».

## Риски и что не делаем в этом черновике

- Поверхность атаки hosted MCP (чужой diff, ключ LLM, чтение репозитория).
- Два транспорта с расходящимся поведением тулзов.
- Агенты, которые по STATUS.md жмут `mcp_auth` на stdio — skill/rule RFC 10 это уже запрещают.

## Оценка

Пока ноль задач в коде. Следующий шаг, если продукт скажет «нужны Cloud Agents»: превратить этот черновик в RFC со схемами HTTP, моделью секретов и границей «не гейт» — **до** кода.
