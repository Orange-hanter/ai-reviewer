---
type: journal
status: active
owner: ai-reviewer
source_of_truth: true
created: 2026-09-10
last_updated: 2026-09-29
audience: [next-session-agent, maintainers]
---

# Журнал разработки ai-reviewer

Живой лог решений и прогонов. Релизы по SemVer — в [CHANGELOG.md](../../CHANGELOG.md).
Сессионные отчёты — в [log/](log/). RFC пишутся **до** кода; этот журнал — что случилось после.

## 2026-09-29 — RFC 13: сверка плана gate-judge с кодом

**Зачем.** План `gate-judge` (25 задач) декомпозирован из RFC, которого не было, а меняет сам merge-гейт.

**Сделано:** черновик [RFC 13](../../proposals/13-gate-judge.md). Решения владельца перенесены дословно; ссылки задач сверены с кодом (52 верны, ~20 съехали). Существенное: при дефолтном strict-verifier почти все блокеры `verified`, то есть твёрдые — судья в штатном прогоне может только ужесточать, дефект «гейт не умеет уставать» рельсом 1 не лечится (В1); твёрдость надо проверять по кластеру, а не по представителю; скор доказуемости появляется после точки вставки судьи; чтение футера режет находки до 40 и теряет их возраст.

**Не сделано:** ответы на В1–В3, grooming задач, чек-лист «Готов к декомпозиции». Код не трогали.

## 2026-09-29 — аудит документации

**Зачем.** Документы разошлись с кодом: RFC 07/09/10/12 реализованы, но числились `review`/`draft`; `integration.md` обещал fail-closed без LLM-ключа, которого нет; CHANGELOG относил фазу 3.6 Rust (`export`/`metrics`, `413273d`) к 0.3.0, хотя она пришла мержем #18 (`014e048`) после тега; Rust-слой и часть env гейта не были описаны.

**Сделано:** статусы RFC в frontmatter, реестре и MASTER; пометки «снимок до реализации» в RFC; фаза 3.6 — в Unreleased; `configuration`/`profiles`/`architecture`/`integration`/README/CLAUDE сверены с кодом; битые ссылки в `feedback/` и `fixtures/`. Код не трогали.

**cod-doc.** Файлы `cursor-plugin/README.md` и этот журнал были правлены после импорта, но drift показывал `in_sync` при `file_hash ≠ projection_hash`; проверка ссылок cod-doc считает битыми якоря на заголовки `###` — секциями у него заведены только `##` (например `12-mcp-long-run.md#spawn-плагина-без-cwd-окна`), на GitHub они рабочие. Оба — баги cod-doc (ADO-241, ADO-242 в проекте cod-doc), не документов. Документы разложены по дереву cod-doc, Инбокс пуст.

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

**Не сделано в этой сессии:** коммиты (план: RFC → фаза A → B → plugin; закоммичено в тот же день — `eeb77ab`, `9d668ce`, `0f47f71`, `4a7be89`, `4527b77`); live Ollama до находок; публичный Marketplace.

## 2026-09-05 — 0.3.0

Нативное ядро (Rust) и RFC 04–07 (окна контекста). См. CHANGELOG.
