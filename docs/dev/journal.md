---
type: journal
status: active
owner: ai-reviewer
source_of_truth: true
created: 2026-09-10
last_updated: 2026-09-12
audience: [next-session-agent, maintainers]
---

# Журнал разработки ai-reviewer

Живой лог решений и прогонов. Релизы по SemVer — в [CHANGELOG.md](../../CHANGELOG.md).
Сессионные отчёты — в [log/](log/). RFC пишутся **до** кода; этот журнал — что случилось после.

## 2026-09-12 — RFC 12: долгий MoA в IDE

**Зачем.** Cursor обрывает `review_run` ~30 с; sidecar пишется после MoA — агент теряет id. Plugin `./bin` резолвится в cwd чужого окна.

**Сделано:** контракт [RFC 12](../../proposals/12-mcp-long-run.md); `wait: false` + `jobId` + stub sidecar до LLM; git-шаблон плагина через `$HOME/.../bin`, не `./bin`; skill поллит `jobId`, не зовёт `mcp_auth` на abort.

## 2026-09-12 — RFC 10: холодный старт MCP

**Зачем.** Plugin MCP не доходил до `connected`: `variables.required` → Authenticate, command от `${AI_REVIEWER_ROOT}`, handshake тянул `pipeline.mjs`, агент шёл в `mcp_auth`.

**Сделано:** RFC 10; `install-local.sh` + `.engine-root` + абсолютный лаунчер (cwd plugin MCP ≠ корень плагина); `mcp.json` git-шаблон `./bin/…`; `variables.required: []`; тонкий handshake; канон W без `-ws`; skill/rule запрещают `mcp_auth`. Черновик направления HTTP/OAuth — [RFC 11](../../proposals/11-http-mcp-oauth.md) (`draft`). Eval/gate не трогали. `REVIEW_MCP_PROTOCOL_VERSION` = 1.

## 2026-09-10 — RFC 08/09: агентный MCP и Cursor Plugin

**Зачем.** Гейт (`pr-review`, `exit 1`) остаётся судьёй merge. Агенту в IDE нужен тот же пайплайн как превью, без GitHub, плюс дешёвые стадии до MoA.

**Сделано в пакете (Unreleased, без тега):**

- RFC 08: stdio-сервер `pr-review-mcp`, тулзы `review_run` / `review_findings`, `previewWouldBlock`, `runDetailed()`, `--head-sha`, транспорт `lib/mcp-stdio.mjs`.
- RFC 09: sidecar `{root}/.ai-review/mcp/runs/{fingerprint}.json`; git-пин `headSha` (префикс + `uncommitted`); дешёвые тулзы `review_route`, `review_prescan`, `fp_check`, `fingerprint_check`, `review_artifact`; фаза C за `REVIEW_MCP_EXTRA=1`; каталог `cursor-plugin/`.
- Тесты: `test/pr-review.mcp.test.mjs`, `test/cursor-plugin.test.mjs`; `npm test` 438 → 438+; eval 16/16.
- По ревью сессии: `OLLAMA_API_KEY` объявлен в `plugin.json` variables без default; тест битого `priorExportPath`.

**Живой прогон MCP** на диффе RFC 08/09 (`uncommitted: true`, пин `014e048f…`):

| Тулза | Итог |
|-------|------|
| `review_route` | `full` |
| `review_prescan` | `completed`, 0 находок, `skipped: no-eslint` |
| `blast_scope` | `graphify_unavailable` (fail-open) |
| `review_run` | `degraded` / `llm unavailable` за ~5.5 мин; процесс жив, GitHub не писался |

**cod-doc.** Проект `ai-reviewer` зарегистрирован (`project add` + `init`). Канон с
frontmatter в БД; L0 — `MASTER.md` с реестром хэшей. Как вести лог — [README.md](README.md).

**Не сделано в этой сессии:** коммиты (план: RFC → фаза A → B → plugin); live Ollama до находок; публичный Marketplace.

## 2026-09-05 — 0.3.0

Нативное ядро (Rust) и RFC 04–07 (окна контекста). См. CHANGELOG.
