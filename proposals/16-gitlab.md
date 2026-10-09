---
type: design
status: draft
owner: ai-reviewer
source_of_truth: true
created: 2026-10-09
last_updated: 2026-10-09
---

# 16 — GitLab как второй хостинг кода

> Категория: 🔵 Архитектура · Риск: высокий · Зависимости: [RFC 05](05-incremental-review.md) (перенос находок через футер), [RFC 13](13-gate-judge.md) (история кругов в футере), `docs/integration.md`, `docs/configuration.md`

Черновик. Цель — чтобы потребитель на GitLab (gitlab.com и self-managed) получал то же, что
потребитель на GitHub: гейт merge, sticky с Δ-блоком и переносом находок, инлайн-замечания,
auto-triage тредов, feedback-контур и шаблоны CI. Решения Р1–Р5 ниже требуют подтверждения
до кода.

## Контекст

Движок знает один хостинг. Абстракции «хостинг кода» нет: единственный шов — DI
`deps.github` в `createReviewPipeline`, заглушка `createSilentGithub` в ревью-MCP и
`deps.createGithubClient` в feedback-batch. Всё остальное (конфиг, диалект markdown, формат
инлайна, экспорт, CI-шаблоны, сервисные скрипты) написано под GitHub напрямую.

Отдельный вопрос — модель доверия. На GitHub потребитель держит инвариант «код PR не
исполняется с секретами прогона»: пин движка читается из базовой ветки, workflow —
`pull_request`. На GitLab `.gitlab-ci.yml` берётся из ветки MR, и переносить этот
инвариант буквально нельзя (Р4).

## Текущее состояние

Сверено по коду на `1883c61`.

**Клиент.** `lib/github.mjs` — 14 методов (`lib/github.mjs:592-596`): `gh`, `loadDiff`,
`readPriorFindings`, `readPriorStickyData`, `upsertSticky`, `deleteStaleInline`,
`postReview`, `listInlineComments`, `listIssueLabels`, `readStickyReview`,
`listPullCommentReactions`, `listUnresolvedReviewThreads`, `replyReviewThread`,
`resolveReviewThread`. Транспорт (`request`/`list` с Link-пагинацией и ретраями) портирован
в Rust (`rust/src/github.rs`, бинарь `github`), оркестрация — только JS. Заголовок
`X-GitHub-Api-Version` и `Accept: application/vnd.github+json` зашиты в обе стороны.

**Пайплайн.**
- Клиент: `pipeline.mjs:322` (`deps.github ?? createGithubClient`).
- Креды: `pipeline.mjs:353` — `cfg.github.{repo,prNumber,token}`; тексты ошибок
  называют `GITHUB_TOKEN` (`pipeline.mjs:383-384`).
- Дифф: `loadDiff` (`pipeline.mjs:397`) берёт unified diff через media type
  `application/vnd.github.v3.diff`; draft → пропуск. Base SHA не используется.
- Инлайн: `postReview` → `reviewId` → `deleteStaleInline({ exceptReviewId })`
  (`pipeline.mjs:260-268`). Порядок post-then-delete опирается на то, что GitHub публикует
  все инлайны одним review с id.
- Sticky: `upsertSticky` (`pipeline.mjs:276`) — issue comment, найденный по маркеру в
  начале тела; футер `<!-- <ns>-data:{…} -->`.
- Triage: `listUnresolvedReviewThreads` / `replyReviewThread` / `resolveReviewThread`
  через GraphQL (`lib/github.mjs:534-585`), вызывается из `lib/triage.mjs`.
- Гейт — exit code процесса; check runs, статусы, лейблы пайплайн не трогает.

**Формат.**
- Инлайн-элемент — `{ path, line, side: 'RIGHT', body }` (`lib/findings.mjs:453`).
  Позиция — только номер строки новой стороны.
- `validLines` содержит и добавленные, и контекстные строки (`lib/diff.mjs:261, 272`).
  Номера старой стороны и старый путь переименованного файла в инлайн не попадают.
- Предложение правки — блок ` ```suggestion ` (`lib/findings.mjs:428`), GitHub-синтаксис.
- Sticky: `<details>`, `<sub>`, HTML-комментарии, лимит 65 000 символов
  (`lib/report.mjs:393, 685`) — под лимит GitHub.

**Экспорт.**
- `repositoryUri` по умолчанию собирается как `https://github.com/${repo}`
  (`lib/export.mjs:487`).
- `pr.url` берётся из `pr.html_url` (`lib/export.mjs:362`, swarm — `lib/export.mjs:283`).
- SARIF пишется в файл, но в шаблоне CI никуда не загружается.

**Feedback.**
- `lib/feedback.mjs` собирает 👍/👎 через reactions на review-комментариях, FP-сигнал —
  лейбл PR.
- Бот определяется по `user.type === 'Bot'` и `REVIEW_FEEDBACK_TRUSTED_AUTHORS`
  (дефолт `github-actions[bot]`, `lib/config.mjs:426`).

**Сервисные контуры, прибитые к api.github.com.**
- `lib/gold-fetch.mjs:18`, `bin/pr-review-gold-harvest.mjs` — бенчмарк.
- `feedback/promote-candidate.mjs:72` — промоут FP-паттернов.

**CI.** `examples/ci/*.yml` — четыре GitHub Actions workflow. Контрактные тесты лежат в
`test/pr-review.eval.test.mjs`. Пин движка читается из базовой ветки
(`git show origin/$BASE_REF:ops/ai-reviewer-ref`).

**Не связаны с хостингом:** engine, verifier, findings-гейт, actionability (кроме формата
тела), metrics, observability, structure-контур.

## Предложение

### 1. Шов `forge`

Новый модуль `lib/forge.mjs`:

```js
/** @typedef {'github'|'gitlab'} ForgeKind */
export function createForgeClient(cfg, log, opts = {}) // → ForgeClient по cfg.forge.kind
```

Контракт `ForgeClient` — то, что реально зовут пайплайн, triage и feedback. Имена методов
остаются прежними, чтобы `deps.github`-заглушки в тестах переехали механически:

| Метод | Возврат (нормализованный) | GitHub | GitLab |
|---|---|---|---|
| `loadDiff()` | `{ change, diff, diffRefs }` или `{ draft: true }` | `pulls/:n` + diff media type | `merge_requests/:iid` + см. Р2 |
| `readPriorStickyData()` | `{ findings, runs, sha }` | issue comments | MR notes (`system: false`) |
| `upsertSticky(body)` | `'created' \| 'updated'` | PATCH/POST issue comment | PUT/POST MR note |
| `postReview({ inline, diffRefs })` | `ReviewToken` (непрозрачный) | review id | множество id созданных discussions |
| `deleteStaleInline({ except })` | `number` | DELETE review comment | DELETE discussion note |
| `listUnresolvedThreads()` | `Thread[]` | GraphQL `reviewThreads` | REST `discussions` (`resolvable && !resolved`) |
| `replyThread(id, body)` / `resolveThread(id)` | — | GraphQL mutations | `POST …/discussions/:id/notes`, `PUT …/discussions/:id?resolved=true` |
| `listInlineComments()`, `listLabels()`, `readStickyReview()`, `listReactions(id)` | нормализованные | как сейчас | notes / `labels` поля MR / `award_emoji` |

Нормализованные формы:

```js
/** @typedef {{ number:number, title:string, url:string, headSha:string, baseSha:string,
 *   startSha:string, draft:boolean, author:{ login:string, bot:boolean } }} Change */
/** @typedef {{ id:string, path:string, line:number,
 *   comments:{ id:string, body:string, author:{ login:string, bot:boolean } }[] }} Thread */
```

- `deps.github` → `deps.forge`. `deps.github` принимается как алиас на один минорный релиз.
- `createSilentGithub` → `createSilentForge` (`lib/review-mcp.mjs:365`).
- `ReviewToken` непрозрачен для пайплайна. `pipeline.mjs:265-268` перестаёт знать, что это
  число.
- Методы triage переименовываются (`listUnresolvedThreads`, без `Review`). Вызов —
  только в `lib/triage.mjs`.
- Пайплайн, triage и feedback читают `change.*`, а не `pr.html_url`, `pr.head.sha`,
  `user.type`.

### 2. Конфиг

`cfg.forge = { kind, api, token, repo, number, webUrl }`. Автоопределение `kind`:

1. env `REVIEW_FORGE=github|gitlab` или CLI `--forge`;
2. `GITLAB_CI=true` → `gitlab`;
3. `GITHUB_ACTIONS=true` → `github`;
4. иначе `github` (текущее поведение).

| Поле | GitHub (как сейчас) | GitLab |
|---|---|---|
| `api` | `GITHUB_API_URL` | `CI_API_V4_URL`, перебивается `GITLAB_API_URL` |
| `token` | `GITHUB_TOKEN` | `AI_REVIEW_GITLAB_TOKEN` (см. Р4); `CI_JOB_TOKEN` писать notes не может |
| `repo` | `GITHUB_REPOSITORY` | `CI_PROJECT_ID`, иначе URL-encoded `CI_PROJECT_PATH` |
| `number` | `PR_NUMBER` | `CI_MERGE_REQUEST_IID` |
| `webUrl` | `https://github.com/<repo>` | `CI_PROJECT_URL` |

- `cfg.github` остаётся замороженным алиасом `cfg.forge` при `kind === 'github'` на тот же
  переходный релиз.
- `lib/export.mjs:487` берёт `repositoryUri` из `cfg.forge.webUrl`.
- Тексты ошибок `pipeline.mjs:383-384` называют переменные выбранного хостинга.
- Все переменные документируются в `docs/configuration.md` (новая секция «GitLab»).
- Rust `config.rs` повторяет резолвинг: он под parity.

### 3. Позиция инлайна

GitLab отклоняет discussion без полной позиции: `base_sha`, `start_sha`, `head_sha`,
`old_path`, `new_path` и `new_line` для добавленной строки или пара `old_line`+`new_line`
для контекстной. Сейчас движок этого не знает (см. «Текущее состояние»). Нужно:

- `parseDiff` (JS + `rust/src/diff.rs`, под parity) дополнительно отдаёт на файл `oldPath` и
  карту `newLine → oldLine` для контекстных строк.
- `lib/findings.mjs:453` строит хостинго-нейтральный элемент
  `{ path, oldPath, line, oldLine|null, body }`. `side: 'RIGHT'` добавляет GitHub-клиент.
- Тело инлайна не меняется: ` ```suggestion ` без параметров GitLab читает как `-0+0`, то
  есть замену строки якоря — так же, как GitHub. Rust-порт тела не трогаем.

### 4. Публикация инлайнов на GitLab

У GitLab нет «одного review из N комментариев с id». Схема:

1. Создать N draft notes (`POST …/merge_requests/:iid/draft_notes` с `position`).
2. Опубликовать их разом (`POST …/draft_notes/bulk_publish`).
3. Пометить созданные discussion id как `ReviewToken`.
4. Удалить старые инлайны с маркером, кроме токена и тредов с ответами — та же логика,
   что `lib/github.mjs:489-505`.

Если draft notes недоступны (старый self-managed; версию сверить при реализации), клиент
падает на последовательные `POST …/discussions`. Одна отклонённая позиция (400) не роняет
остальные: находка уходит в sticky, как сейчас уходит не-инлайновая.

### 5. Гейт на GitLab

Гейт — по-прежнему exit code. Джоба `ai-review` в merge request pipeline
(`rules: $CI_PIPELINE_SOURCE == "merge_request_event"`, `interruptible: true`) с
`allow_failure: false`. В проекте потребителя включено «Pipelines must succeed».
Commit status и external status checks не вводим: exit code уже несёт вердикт, а второй
канал разойдётся с ним.

### 6. Экспорт

- `EXPORT_VERSION` 2 → 3 (аддитивно): поле `forge: { kind, webUrl }`.
- `pr.url` заполняется из `change.url`; имя `pr` сохраняется, чтобы не ломать потребителя
  артефакта.
- Новый опциональный артефакт `<toolName>.codequality.json` (формат GitLab Code Quality)
  под `REVIEW_EXPORT_CODEQUALITY=1`. GitLab показывает его в виджете MR в Free-тарифе;
  SARIF GitLab в MR не рендерит. Порт в Rust `export` и parity — в том же шаге.

### 7. Шаблоны CI и потребитель

`examples/ci/gitlab/`:

| Файл | Аналог | Механизм |
|---|---|---|
| `ai-review.gitlab-ci.yml` | `pr-review.yml` | MR pipeline; пин из target-ветки: `git fetch origin $CI_MERGE_REQUEST_TARGET_BRANCH_NAME && git show FETCH_HEAD:ops/ai-reviewer-ref`; движок — `git clone` по пину; `artifacts:paths` (observability, export), `artifacts:reports:codequality` |
| `ai-review-feedback.gitlab-ci.yml` | `pr-review-feedback.yml` | pipeline schedule; MR создаётся через `POST /merge_requests` |
| `ai-review-feedback-promote.gitlab-ci.yml` | `…-promote.yml` | schedule |
| `ai-review-metrics.gitlab-ci.yml` | `pr-review-metrics.yml` | `GET /projects/:id/jobs/artifacts/:ref/download?job=` |

- Шаблоны подключаются через `include: project:` или `include: remote:` по пину.
- Контрактные тесты в `test/pr-review.eval.test.mjs` расширяются на `.gitlab-ci.yml`:
  пин из target-ветки, `allow_failure: false`, токен не печатается, нет
  `pipeline_source == push`.
- `docs/integration.md` — секция «GitLab». `examples/consumers/` — образец GitLab-потребителя.

### 8. Feedback и triage

- Реакции: `award_emoji` на note (`thumbsup` / `thumbsdown`). FP-лейбл — поле `labels` MR.
- Бот: у note-автора на GitLab нет надёжного `type`. Доверенность — только по
  `REVIEW_FEEDBACK_TRUSTED_AUTHORS`. Дефолт для GitLab — пустой список; `lib/config.mjs:426`
  становится зависимым от `kind`.
- `feedback/promote-candidate.mjs` переводится на `forge.loadFileDiff`.

### Решения, нужные до кода

| # | Вопрос | Предлагаю | Почему |
|---|---|---|---|
| Р1 | Где живёт GitLab-клиент | только JS (`lib/gitlab.mjs`) в первой итерации; транспорт Rust обобщается шагом 6 | оркестрация и так в JS; Rust-транспорт GitHub зашивает его заголовки, и обобщать его до появления второго клиента — гадать о контракте |
| Р2 | Откуда дифф | в CI — локальный `git diff $CI_MERGE_REQUEST_DIFF_BASE_SHA...$CI_COMMIT_SHA` (репозиторий уже склонирован, `--diff` уже есть); вне CI — сборка unified diff из `GET …/merge_requests/:iid/diffs` | у `/diffs` нет `diff --git`-заголовков и mode-строк, синтез — источник расхождений с `parseDiff`; локальный git даёт тот же формат, что GitHub |
| Р3 | Многострочные правки | не вводим: якорь инлайна — одна строка, ` ```suggestion ` совпадает на обоих хостингах | диапазон `-N+M` понадобится, только если находка начнёт нести диапазон строк — это отдельное изменение формата находки |
| Р4 | Модель доверия | документировать явно: в MR из ветки того же проекта `.gitlab-ci.yml` берётся из MR, и секреты (`OLLAMA_API_KEY`, `AI_REVIEW_GITLAB_TOKEN`) доступны автору MR. Рекомендация потребителю — masked-переменные плюс внешний путь CI-конфига (`Settings → CI/CD → ci.yml@group/ai-review-ci`). MR из форков секретов не получают по умолчанию — не включать «pipelines for fork MRs in parent project» | инвариант GitHub «код PR без секретов» на GitLab держится только настройкой проекта, не движком; молчать об этом — хуже, чем не поддерживать GitLab |
| Р5 | Переименование `deps.github` / `cfg.github` | алиасы на один минорный релиз, затем удаление | внешние вызывающие (`index.mjs` не экспортирует клиент) — только тесты и MCP, но swarm-интеграторы могли читать `cfg.github` |

## Миграция

1. **Шаг 1 — шов без смены поведения.** `lib/forge.mjs`, `cfg.forge`, `deps.forge`,
   нормализованные `Change`/`Thread`, непрозрачный `ReviewToken`.
   `npm run check` и `npm run eval` не меняются ни на байт, Rust parity сходится.
2. **Шаг 2 — диф и позиция.** `oldPath` и `oldLine` в `parseDiff` (JS + Rust + parity),
   нейтральный элемент инлайна. Eval-fixture с переименованием и контекстной строкой.
3. **Шаг 3 — `lib/gitlab.mjs`.** `loadDiff` (Р2), sticky, draft notes + bulk publish +
   fallback, post-then-delete. Тест `test/gitlab.test.mjs` на мок-fetch, тот же набор
   кейсов, что `test/github.test.mjs`.
4. **Шаг 4 — triage и feedback на discussions и award emoji.**
5. **Шаг 5 — CI-шаблоны, codequality-экспорт, docs, образец потребителя.** E2E на
   тестовом проекте gitlab.com: прогон на MR с блокером → job failed, merge заблокирован;
   повторный пуш → перенос находок и Δ-блок; ответ в треде → triage.
6. **Шаг 6 (опц.) — Rust-транспорт `forge`.** Op `request`/`list` с параметризованными
   заголовками auth и версии. Parity против `lib/gitlab.mjs`.
7. **Релиз 0.5.0.** `CHANGELOG`, `EXPORT_VERSION` 3, снятие алиасов — в 0.6.0.

Breaking для существующих потребителей: нет. Namespace маркеров и формат футера не
меняются.

## Риски и что не делаем

- **Позиции, которые GitLab отклоняет.** Строка вне диффа, устаревший `head_sha` после
  rebase. Позицию отбрасываем, находку отправляем в sticky. В observability — счётчик
  `inlineRejected`.
- **Rate limit gitlab.com.** Draft notes — 1 запрос на инлайн, bulk publish — 1.
  `isTransient` уже ретраит 429 (`lib/util.mjs:48`).
- **Self-managed старых версий.** Минимальная поддерживаемая версия фиксируется на шаге 3
  по наличию `draft_notes/bulk_publish`. Ниже работает fallback, атомарности нет.
- **Не делаем:** Bitbucket и Gitea (шов позволит, но без потребителя — гадание);
  external status checks и commit statuses (п. 5); GitLab-источник для
  `gold-fetch`/`gold-harvest`/публичного бенчмарка — датасеты живут на GitHub;
  ревью-MCP и structure-контур не трогаем — они не пишут в хостинг.

## Оценка

| Шаг | Задач | Где риск |
|---|---|---|
| 1. Шов | 4 (forge-модуль, конфиг + Rust config, переезд вызовов и тестов, MCP-заглушка) | parity `config` |
| 2. Позиция | 3 (JS diff, Rust diff + parity, findings + fixture) | parity `diff`, счётчик fixtures (16 → 17) |
| 3. GitLab-клиент | 4 (транспорт, sticky, инлайн + fallback, loadDiff) | позиции |
| 4. Triage/feedback | 3 | доверенность авторов |
| 5. CI/экспорт/доки | 5 (шаблон ревью, сервисные шаблоны, codequality JS+Rust, docs, E2E) | модель доверия у потребителя |
| 6. Rust-транспорт | 2 | — |

Итого ~21 задача. Шаги 1–3 — минимум для работающего гейта на GitLab без triage и feedback.
