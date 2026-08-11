# Архитектура

Автоматическое ревью pull request'а: unified diff → blast-radius по графу кода →
детерминированный prescan → LLM (Mixture-of-Agents) → inline + sticky в GitHub.

Merge блокируется exit-кодом `1` только на **подтверждённых** блокерах (`critical`/`major`).
Инфра-сбои (модель, GitHub, граф) — мягкий exit `0`; исключение — полный аутедж LLM при
`REVIEW_LLM_REQUIRED=1`: без ревью гейт не может подтвердить PR.

## Поток данных

```mermaid
flowchart TB
  subgraph inputs [Вход]
    GH[GitHub PR diff]
    GF[Graphify affected]
    PRIOR[Prior findings из sticky]
    PROF[Профиль проекта]
  end

  subgraph pipeline [pipeline.mjs]
    ROUTE[router: skip / light / full]
    PARSE[parseDiff + чанки]
    CTX[файлы целиком + blast-radius]
    PRE[prescan: eslint + ast-grep]
    ENG[engine: advisors → aggregator]
    VER[verifier Pass B]
    FILT[FP-фильтр + кластеризация]
    ACT[actionability: inline или свод]
    OUT[inline + sticky + Δ-блок]
  end

  GH --> ROUTE --> PARSE --> CTX --> ENG
  GF --> CTX
  PROF --> PRE
  PRE --> ENG
  PRIOR --> ENG
  ENG --> VER --> FILT --> ACT --> OUT --> GH
  OUT --> EXP[JSON + SARIF artifact]
  OUT --> OBS[JSONL run log]
```

## Модули

| Модуль | Назначение |
|--------|------------|
| `bin/pr-review.mjs` | CLI полного прогона |
| `pipeline.mjs` | `createReviewPipeline()`, `runReview()` — оркестрация |
| `lib/profile.mjs` | [Профиль проекта](profiles.md): маркеры, правила, раскладка, пути |
| `lib/config.mjs` | `loadReviewConfig()` — профиль + env + CLI в один замороженный объект |
| `lib/diff.mjs` | Парсинг diff, чанкинг, аннотация номеров строк |
| `lib/graphify.mjs` | `graphify affected` → blast-radius изменённых символов |
| `lib/prescan.mjs`, `lib/astgrep.mjs`, `lib/prescan-shared.mjs` | eslint + ast-grep по изменённым файлам |
| `lib/engine.mjs` | MoA и consensus, fallback моделей, дедлайн прогона |
| `lib/ollama.mjs` | SSE-чат, JSON-mode, ретраи, адаптивный семафор по 429 |
| `lib/prompt.mjs` | Системные промпты: aggregator, advisors, verifier |
| `lib/specialists.mjs` | Роли advisors + refutation жюри |
| `lib/verifier.mjs` | Pass B: доказать / отклонить / понизить кандидатов в блокеры |
| `lib/findings.mjs` | Нормализация, FP-фильтр, кластеризация, гейт |
| `lib/actionability.mjs` | Маршрут находки по доказуемости, а не по ярлыку severity |
| `lib/report.mjs` | Sticky, Δ-блок, машинный футер, свод advisory |
| `lib/triage.mjs` | Авто-ответ и resolve на своих тредах с известным FP |
| `lib/github.mjs` | GitHub API: sticky, inline-треды, реакции |
| `lib/feedback.mjs`, `feedback/*` | Сбор 👍/👎 → JSONL → паттерны и fixtures |
| `lib/export.mjs` | JSON + SARIF артефакт прогона |
| `lib/observability.mjs`, `lib/metrics.mjs` | JSONL-лог прогонов и сводка по нему |
| `lib/eval.mjs` | Golden fixtures без LLM |

## Ключевые решения

### Маршрут находки — по доказуемости

Раньше маршрут задавала severity, то есть ярлык модели: одна и та же тема на трёх прогонах
одного sha получала critical, major и minor. Теперь **блокировка** осталась по severity, а
**где показывать** считается по детерминированным сигналам, которые пайплайн уже имеет:
подтверждение prescan'ом на той же строке, вердикт verifier'а, повтор с прошлого прогона,
независимый консенсус моделей, чувствительная зона файла, наличие конкретного фикса.
Порог — `REVIEW_ACTIONABILITY_MIN`, веса — в [`lib/actionability.mjs`](../lib/actionability.mjs).

### Сходимость между прогонами

Sticky несёт скрытый машинный футер `<ns>-data:` с находками, отпечатками кода и историей
прогонов. Следующий прогон читает его и: показывает Δ (что появилось/ушло), не переоткрывает
уже закрытые темы, не переформулирует их заново. Отпечаток — это код, на который показывает
находка: код изменился → находка снимается, код на месте → переносится.

Маркер комментария — часть профиля. **Смена namespace на живом репозитории рвёт эту цепочку**:
прошлые sticky/inline перестают быть «своими».

### Два независимых судьи перед блоком

Aggregator собирает находки из advisors и обязан к каждой приложить refutation по ролям.
Verifier (отдельная модель, вне пула advisors) доказывает или отклоняет кандидатов в блокеры.
При `REVIEW_VERIFIER_STRICT=1` отсутствие вердикта понижает находку до minor — блок требует
подтверждения второй моделью.

### Prescan раньше LLM

eslint и ast-grep дают детерминированные находки на изменённых файлах: они и блокируют
по своим правилам, и уходят в промпт как «уже сказано, не повторяй». Scope ast-grep правил
и исключения — из профиля: правило «client не импортирует server» осмысленно только там, где
такие слои есть.

## Режимы LLM

**MoA (по умолчанию).** Specialist advisors (security / correctness / architecture) пишут
prose-разбор → жюри-агрегатор синтезирует JSON-находки с refutation по каждой претензии →
verifier проверяет кандидатов в блокеры. Агрегатор и verifier не входят в пул advisors.

**Consensus.** `REVIEW_MODE=consensus`: каждая модель проходит каждый чанк, блокером
становится кластер, набравший `REVIEW_CONSENSUS` независимых подтверждений.

## Границы

| В scope | Вне scope |
|---------|-----------|
| Логика, безопасность, границы слоёв, blast-radius регрессии | Стиль и форматирование (это работа eslint/tsc) |
| Снижение FP: refutation, verifier, паттерны, actionability | Замена human review |
| Воспроизводимый гейт на fixtures | Готовый пресет под любой стек без профиля |

## Проверки самого движка

| Что | Команда |
|-----|---------|
| Юнит + контракты | `npm test` |
| Golden fixtures без LLM (16 кейсов) | `npm run eval` |
| Shadow-прогон с LLM на размеченных диффах | `OLLAMA_API_KEY=… npx pr-review-gold` |

Регрессионный набор гоняется под reference-профилем `profiles/orakul.json` — тем же, на
котором движок работал до выноса в отдельный пакет.
