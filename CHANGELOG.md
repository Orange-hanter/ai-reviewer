# Changelog

## 0.2.3 — поверхность: AGENTS.md, Walkthrough, prompts

### Добавлено

- **AGENTS.md** как дефолтный источник `repoRules`, если профиль не задал `repoRules`/`repoRulesFile`
- **Walkthrough** в начало sticky из summary агрегатора (без отдельного LLM-вызова)
- **`prompts.*` в профиле**: `advisorExtra` / `specialistExtra` / `aggregatorExtra` / `verifierExtra`

### Изменено

- CI / шаблоны потребителя: `graphifyy` **0.9.31 → 0.9.52**; GitHub Actions —
  `checkout`/`setup-node`/`setup-python` **v6 → v7**, `cache` **v5 → v6**


## 0.2.2 — LLM abstraction, cost, benchmark, verdict quality

### Добавлено

- **`lib/llm.mjs`** — канонический OpenAI-compatible клиент (`createLlmClient`); `lib/ollama.mjs` — thin re-export
- Env-приоритет: `LLM_API_KEY` / `LLM_BASE_URL` с fallback на `OLLAMA_*`; fail-closed без silent provider fallback
- **Cost tracking**: usage из SSE, `createCostTracker`, `REVIEW_PRICE_JSON`, sticky `💰 ~$0.12 · 45k tokens`
- **`lib/benchmark.mjs`** + `pr-review-benchmark` CLI + [docs/benchmark.md](docs/benchmark.md)
- **Blast-radius findings** (`lib/blast-findings.mjs`): cross-layer callers вне диффа → deterministic major
- **`REVIEW_STRICT=1`** / `profile.strict`: блок только prescan или verifier-verified; advisory не inline
- **Grounding quotes**: поле `quote` в schema; `REVIEW_REQUIRE_QUOTE=1` / strict — drop critical/major без quote
- `filterFindingsFixedInDiff` — adjacent-commit carry-over через fingerprint-инвалидацию
- Actionability signal `blastCaller` (weight 2)

### Изменено

- User-facing ошибки LLM говорят «LLM», не только «Ollama»
- `lib/metrics.mjs`: агрегация prompt/completion/total tokens из `llm_call` events
- `pr-review-gold` использует `scoreGoldCase` из benchmark

## 0.2.1 — blast-radius sync + portable gold

Синхронизация с живым движком Orakul (коммиты после выноса v0.2.0) и расширение gold-набора.

### Добавлено

- `collectBlastSymbols` — добор экспортов из полного текста файла, если hunk без `export`
- Graphify: `graph_missing` / `no_symbols_in_diff` без ложных вызовов `affected`
- Стабильность находок (Jaccard по точкам кода) и воспроизводимость вердикта в sticky / JSONL
- Portable gold: clean-controls, absence-claim FP, router-light, regen FP (8 кейсов)
- [docs/backlog.md](docs/backlog.md) — рыночный бэклог как канон приоритетов

### Изменено

- Промпт aggregator/verifier: обязаны опираться на blast-radius callers, не выдумывать зависимости
- Pipeline: сначала `readChangedFiles`, затем blast; события `stability` / `verdict` в observability

## 0.2.0 — вынос движка в самостоятельное решение


Первый релиз, в котором пакет не привязан к конкретному репозиторию. Движок синхронизирован
с рабочей версией потребителя (август 2026) и расцеплен с ней через профиль проекта.

### Добавлено

- **Профиль проекта** (`lib/profile.mjs`, [docs/profiles.md](docs/profiles.md)): маркеры
  комментариев, правила репозитория для промпта, раскладка каталогов для prescan, scope и
  allowlist ast-grep правил, пути feedback/observability/export — в одном JSON у потребителя.
  Источник: `REVIEW_PROFILE` → явное назначение → `.ai-reviewer.json` в корне → нейтральный
  дефолт. Битый профиль не роняет ревью (fail-open + строка в лог).
- **Маршрут находки по доказуемости** (`lib/actionability.mjs`): inline или свод решает сумма
  детерминированных сигналов (prescan, verifier, повтор, консенсус, чувствительная зона),
  а не ярлык severity. Пороги — `REVIEW_ACTIONABILITY_MIN`, `REVIEW_MAX_ADVISORY_INLINE`.
- **Δ-блок и история прогонов в sticky**: что появилось и что ушло с прошлого прогона;
  отпечаток кода у перенесённой находки (точный фикс снимает находку).
- **Метрики прогонов** (`lib/metrics.mjs`, `pr-review-metrics`): сводка по observability-логу.
- **Gold-набор** (`gold/`, `pr-review-gold`): shadow-прогон полного пайплайна с LLM на
  размеченных TP/FP диффах.
- Новые CLI: `pr-review-metrics`, `pr-review-gold`, `pr-review-probe-models`.
- Шаблоны workflow потребителя — [`examples/ci/`](examples/ci/) (ревью, harvest, promote, метрики),
  все на пиновом чекауте движка вместо копирования кода из базовой ветки.
- `test/profile.test.mjs` — поведение нейтрального профиля и загрузка чужого.

### Изменено

- Пакет переименован `@orakul/ai-reviewer` → **`ai-reviewer`**; дефолтные маркеры
  `<!-- ai-review -->`, SARIF `ai-review/<severity>`, файлы экспорта `ai-review.json` /
  `ai-review.sarif.json`. Прежние имена сохраняются профилем потребителя.
- Из движка убраны project-specific умолчания: пути `ops/pr-review/*` и `.graphify/*`,
  правила репозитория в промпте, allowlist `app/server/net.ts`, scope ast-grep правил,
  раскладка `app/<слой>` в prescan. Всё это теперь приходит из профиля.
- Пути данных по умолчанию — `.ai-review/*` в корне репозитория-потребителя.
- Fixtures и FP-паттерны движка резолвятся от корня пакета, а не от каталога проекта.
- Документация переписана под самостоятельный продукт; журнал FP конкретного проекта из
  пакета убран (он живёт у потребителя), в `docs/false-positives.md` осталось переносимое:
  семейства FP, встроенные защиты, петля обучения.

### Проверки

- `npm test` — 203 теста (юнит, контракты пайплайна, шаблоны workflow).
- `npm run eval` — 16 golden fixtures, гейт post-processing зелёный.
- Регрессионный набор гоняется под reference-профилем `profiles/orakul.json`: движок под ним
  ведёт себя ровно так же, как до выноса в отдельный пакет.

## 0.1.0 — первый вынос кода (2026-07-12)

Снимок движка `ops/pr-review/` первого потребителя: pipeline, MoA/consensus, prescan,
eval fixtures, feedback loop. Интеграция обратно в потребителя не была доведена.
