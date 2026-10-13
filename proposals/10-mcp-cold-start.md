---
type: design
status: active
owner: ai-reviewer
source_of_truth: true
created: 2026-09-12
last_updated: 2026-09-29
---

# 10 — Холодный старт MCP: корни, лаунчер, handshake

> Категория: 🔵 Архитектура · Риск: средний · Зависимости: RFC 08 (review MCP), RFC 09 (Cursor Plugin). Заморозку 08/09 не переоткрывает: два сервера, `previewWouldBlock`, `review_run` единственный LLM/мьютекс, движок не кладётся в плагин, хуков `afterFileEdit` нет, `REVIEW_MCP_PROTOCOL_VERSION` = 1.

## Контекст

Плагин обнаруживается, stdio часто не доходит до `connected`. Цепочка, уже доказанная логами Cursor:

- User-scoped plugin MCP спавнится **без workspace folders**; GUI часто **без `node` в PATH**.
- `variables.required: ["AI_REVIEWER_ROOT", "REVIEW_MCP_ROOT"]` → Cursor не стартует stdio и показывает **Authenticate**.
- `mcp_auth` на stdio: `logoutServer` + `OAuth credentials cleared` + reconnect в тот же сломанный spawn.
- Неуспех handshake (hang ~60s = `coldStartTimeoutMs`) → в чате только `mcp_auth` → агент читает файлы вместо тулзов.
- Лаунчер умирал, если `AI_REVIEWER_ROOT` пуст или плейсхолдер; `mcp.json` звал `${AI_REVIEWER_ROOT}/cursor-plugin/bin/…` — без Configure команды нет.
- `bin/pr-review-mcp.mjs` импортировал `lib/review-mcp.mjs` → весь `pipeline.mjs` **до** `initialize`.

Нужна матрица вариантов, которые обязаны работать с первого раза, и явный fail-fast вместо hang/OAuth.

## Текущее состояние

Сверено по коду до этой правки, не по памяти:

- [`cursor-plugin/mcp.json`](../cursor-plugin/mcp.json): `command` = `/bin/sh`, `args` = `${AI_REVIEWER_ROOT}/cursor-plugin/bin/review-mcp.sh`. Без подстановки переменной исполняемого файла нет.
- [`cursor-plugin/.cursor-plugin/plugin.json`](../cursor-plugin/.cursor-plugin/plugin.json): JSON Schema `variables` с `required: ["AI_REVIEWER_ROOT", "REVIEW_MCP_ROOT"]`.
- [`cursor-plugin/bin/mcp-env.sh`](../cursor-plugin/bin/mcp-env.sh): `mcp_resolve_engine` сразу `mcp_die`, если `AI_REVIEWER_ROOT` пуст/плейсхолдер. `.engine-root` нет.
- [`bin/pr-review-mcp.mjs`](../bin/pr-review-mcp.mjs): статический `import { handleReviewMcpRequest } from '../lib/review-mcp.mjs'` — тянет [`pipeline.mjs`](../pipeline.mjs) до первого кадра.
- Плейсхолдер `${REVIEW_MCP_ROOT}` в bin уже не делает `chdir` (защита от crash); битый заданный путь молча пропускал `chdir`.
- Workspace [`.cursor/mcp.json`](../.cursor/mcp.json) в этом клоне звал серверы `ai-reviewer-ws` / `ai-reviewer-structure-ws` — второй набор имён рядом с plugin MCP.
- Skill/rule не запрещали `mcp_auth`. Authenticate на stdio — не поддерживаемый путь, но агент шёл туда по STATUS.md.

## Предложение

### Корни и каналы

- **engine** — клон пакета (`bin/pr-review-mcp.mjs` существует).
- **consumer** — репозиторий ревью (`REVIEW_MCP_ROOT`), может не совпадать с engine.
- Плейсхолдер `${…}` и пустая строка ≡ «не задано».
- **P — plugin MCP** (`~/.cursor/plugins/local/ai-reviewer`, user-scoped, один процесс на все окна).
- **W — workspace MCP** (`.cursor/mcp.json` / шаблон потребителя, `${workspaceFolder}`).
- Одновременно P и W с **одними** именами серверов — запрещено (дубли). Имена канона: `ai-reviewer` / `ai-reviewer-structure`. Для разработки движка — **один** канал: W **или** P-install, не оба.

### Resolve engine (лаунчер)

Порядок в [`cursor-plugin/bin/mcp-env.sh`](../cursor-plugin/bin/mcp-env.sh):

1. usable `AI_REVIEWER_ROOT` (не плейсхолдер) → должен содержать `bin/pr-review-mcp.mjs`, иначе `exit 1`.
2. файл `$(pluginDir)/.engine-root` (абсолютный путь клона; не в git).
3. ~~предки `cwd`, пока нет `bin/pr-review-mcp.mjs`.~~ *Снято в RFC 20 (T31, безопасность): `cwd` plugin MCP — открытое окно Cursor, недоверенный каталог; репозиторий, похожий на клон пакета, отдавал бы лаунчеру на исполнение свой `rust/target/*/pr-review` или `bin/pr-review-mcp.mjs`.*
4. если лаунчер живёт в `…/cursor-plugin/bin` внутри клона — `../..`.

`REVIEW_MCP_ROOT`: usable env → иначе engine. `STRUCTURE_MCP_ROOT` = тот же, если не задан отдельно.

`plugin.json` `variables`: JSON Schema `{ type: object, properties }` и `required: []`. Configure — override, не условие спавна.

Command в git [`cursor-plugin/mcp.json`](../cursor-plugin/mcp.json): `"command": "/bin/sh"`, `"args": ["./bin/review-mcp.sh"]` (путь относительно **корня плагина**, не `${AI_REVIEWER_ROOT}/…`). *Заменено [RFC 12](12-mcp-long-run.md#spawn-плагина-без-cwd-окна): git-шаблон спавнит `sh -c` на `$HOME/.cursor/plugins/local/ai-reviewer/bin/…`, `./bin` больше не канон.*

### Инсталлятор

- `rsync -a --delete` каталога `cursor-plugin/` → `~/.cursor/plugins/local/ai-reviewer/` (не symlink). Пишет `.engine-root` = абсолютный путь клона, из которого вызвали скрипт. Не трогает `OLLAMA_API_KEY`. Не прописывает путь **engine** в `mcp.json`.
- **Лаунчеры в установленной копии — абсолютные** (`$dest/bin/review-mcp.sh`). Plugin MCP у Cursor спавнится с cwd чужого окна (не корень плагина): `./bin/review-mcp.sh` резолвится в `{чужой-workspace}/bin/…` → `No such file` → `Connection closed` → в чате `mcp_auth`. Git-шаблон [`cursor-plugin/mcp.json`](../cursor-plugin/mcp.json) остаётся `./bin/…` (относительно корня плагина); живой spawn держит `install-local.sh`. *(Шаблон позже заменён RFC 12, см. выше.)*

Это единственный путь «скопировал плагин → Reload → тулзы есть» без Customize.

### Handshake без пайплайна

[`bin/pr-review-mcp.mjs`](../bin/pr-review-mcp.mjs): `initialize` / `notifications/initialized` / `tools/list` / `ping` отвечают из тонкого модуля каталога тулзов ([`lib/review-mcp-tools.mjs`](../lib/review-mcp-tools.mjs) + [`lib/review-mcp-handshake.mjs`](../lib/review-mcp-handshake.mjs)). `import('../lib/review-mcp.mjs')` — только на `tools/call`.

То же для structure: handshake без `structure-context` / Rust-бинаря; домен — на `tools/call`.

`chdir` только из usable root; заданный и несуществующий `REVIEW_MCP_ROOT` → stderr + `exit 1`, не hang. Кадр ответа — один `stdout.write` ([`lib/mcp-stdio.mjs`](../lib/mcp-stdio.mjs)).

Критерий: handshake с `PATH=/usr/bin:/bin` и без `AI_REVIEWER_ROOT` в env, но с `.engine-root`, < 5s; в `tools/list` есть `review_route`. `OLLAMA_API_KEY` для `initialize` / дешёвых тулзов не нужен; `review_run` деградирует как сейчас (`llm unavailable`).

### Матрица (контракт теста)

Каждая строка — spawn handshake или явный `exit 1` + stderr, **не** timeout клиента (~60s).

**Обязаны работать**

| id | Смысл |
|----|--------|
| P-engine-install | `install-local.sh` → `.engine-root` → Reload. Configure не нужен. `REVIEW_MCP_ROOT` по умолчанию = engine. |
| P-configure | Customize задаёт `AI_REVIEWER_ROOT` / опц. `REVIEW_MCP_ROOT`. Перебивает `.engine-root`. |
| W-engine | Открыт клон, `.cursor/mcp.json` с `${workspaceFolder}` на оба корня. Без plugin variables. |
| W-consumer | В репо потребителя копия [`examples/cursor/mcp.json`](../examples/cursor/mcp.json): `REVIEW_MCP_ROOT=${workspaceFolder}`, engine из env / `.engine-root` / Configure. |
| Cheap-without-key | Нет `OLLAMA_API_KEY` → `initialize` / `review_route` / `review_prescan` живы. |
| PATH-без-node | `PATH=/usr/bin:/bin`, node в `~/.local/bin` или Homebrew — лаунчер находит. |
| Плейсхолдеры | env `${AI_REVIEWER_ROOT}` / `${REVIEW_MCP_ROOT}` — не `chdir` crash, не OAuth; resolve или `exit 1` < 2s. |

**Обязаны fail-fast** (не hang, не Authenticate)

| id | stderr / exit |
|----|----------------|
| Нет node | `node ≥ 22 не найден`, exit 1 |
| Нет engine | инструкция `install-local.sh` / Configure, exit 1 |
| Битый `REVIEW_MCP_ROOT` | не `chdir` в мусор; stderr, exit 1 при старте |
| Authenticate / `mcp_auth` | не поддерживаемый путь. Skill/rule: не вызывать `mcp_auth` у `plugin-ai-reviewer-*`; при error/loading — stdio/логи, не «авторизуйся». |

### Агент

[`cursor-plugin/skills/review-loop/SKILL.md`](../cursor-plugin/skills/review-loop/SKILL.md) и [`cursor-plugin/rules/preview-not-gate.mdc`](../cursor-plugin/rules/preview-not-gate.mdc): транспорт stdio, не OAuth.

## Миграция

Аддитивно к RFC 09. Протокол ревью-MCP не меняется (`REVIEW_MCP_PROTOCOL_VERSION` = 1). Eval/gate не трогаем. Потребитель с уже заполненным Configure продолжает работать (P-configure). Старый `args` с `${AI_REVIEWER_ROOT}/cursor-plugin/bin/…` в git больше не канон — после `install-local.sh` локальная копия получает **абсолютный** `$dest/bin/review-mcp.sh` (cwd plugin MCP ≠ корень плагина). Git-шаблон оставался `./bin/…` до RFC 12.

Workspace-имена `-ws` в этом клоне уходят: канон `ai-reviewer` / `ai-reviewer-structure`. Кто держал оба канала — оставляет один.

## Риски и что не делаем

Non-goals: OAuth / кнопка Authenticate на **stdio**; HTTP-обход для Cloud Agents в этом RFC; несколько `REVIEW_MCP_ROOT` на один plugin-процесс; перенос движка в плагин; хуки `afterFileEdit`; смена `previewWouldBlock` / мьютекса `review_run`.

Черновик направления «настоящий HTTP MCP + OAuth как третий контур» — [RFC 11](11-http-mcp-oauth.md) (`draft`). Не открывает реализацию и не делает Authenticate на плагине поддерживаемым путём.

- **Один plugin `REVIEW_MCP_ROOT` на все окна.** Два потребителя сразу — только W в каждом репо, без дубля P.
- **Cloud Agents** по-прежнему без local stdio.
- **Превью примут за гейт** — не трогаем: rule RFC 09.

Ручной чеклист после Reload: в `mcpprocess.log` — `Successfully connected to stdio server` для `plugin-ai-reviewer-ai-reviewer` **без** `reason=authenticate`. В чате namespace `ready`, есть `review_route`, нет единственного `mcp_auth`.

## Оценка

RFC (этот документ); лаунчер + `./bin` + variables без `required`; `install-local.sh`; тонкий handshake; W-шаблон; skill/rule; тесты матрицы; docs (README плагина, строка в RFC 09).
