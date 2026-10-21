---
type: guide
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-09-29
---

# Профиль проекта

Движок один, репозитории разные. Всё, что зависит от конкретного репозитория — имена маркеров
в комментариях, правила проекта для промпта, раскладка каталогов, scope ast-grep правил, пути
для feedback и артефактов — вынесено в **профиль**: один JSON-файл на стороне потребителя.

Без профиля движок работает на нейтральных дефолтах: маркеры `ai-review`, prescan видит любой
TS/JS в корне репозитория, блок правил проекта в промпте пустой (движок не выдумывает
архитектуру, которой не знает).

## Где движок берёт профиль

| Приоритет | Источник |
|-----------|----------|
| 1 | `REVIEW_PROFILE=path/to/profile.json` (env) |
| 2 | профиль, назначенный явно через `setProfile()` (API/тесты) |
| 3 | `.ai-reviewer.json` или `ai-reviewer.config.json` в корне рабочего каталога |
| 4 | встроенный нейтральный профиль |

Нечитаемый или битый профиль **не роняет ревью**: движок пишет строку в лог и продолжает на
дефолтном профиле. Профиль читается один раз на прогон и дальше не меняется.

## Схема

```jsonc
{
  "name": "acme",                          // имя профиля (в логах и observability)
  "markerNamespace": "acme-ai-review",     // <!-- acme-ai-review -->, -inline, -data:
  "toolName": "acme-ai-review",            // SARIF tool.name, имена файлов экспорта, User-Agent
  "sarifRulePrefix": "acme",               // ruleId в SARIF: acme/critical…
  "repositoryUri": "https://github.com/acme/widgets",
  "repoRulesFile": "acme-repo-rules.md",   // или "repoRules": "…текст…"; иначе AGENTS.md
  "strict": false,                         // строгий гейт, как REVIEW_STRICT=1

  "prompts": {                             // опционально: вставки в системные промпты
    "aggregatorExtra": "Prefer findings about money rounding.",
    "verifierExtra": "Treat blast-radius callers as strong evidence."
  },

  "layout": {
    "appDir": "app",                       // каталог приложения ('' = корень репозитория)
    "layerDirs": ["server", "shared", "client"],
    "lintable": "^app/(?:shared|server|client)/.+\\.(?:ts|tsx)$",
    "e2eSpec": "^app/e2e/.*\\.spec\\.ts$",
    "logic": "^(?:app/)?(?:server|shared|client)/.+\\.(?:ts|tsx)$",  // сейчас не читается, см. ниже
    "sensitive": "(?:^|/)ops/(?:pr-merge|deploy)",
    "reviewSkip": ["^docs/openapi/", "^app/server/apiDocs/openapi\\.json$"]  // строка или массив
  },

  "astgrep": {
    "scopes": { "client-imports-server": "^app/client/" },
    "allow":  { "raw-x-forwarded-for": ["app/server/net.ts"] }
  },

  "paths": {
    "feedbackLog": "ops/ai-review/feedback/fp-feedback.jsonl",
    "feedbackCandidatesDir": "ops/ai-review/feedback/candidates",
    "fpPatterns": "ops/ai-review/feedback/fp-patterns.jsonl",
    "fpMarkdown": "docs/ai-review-false-positives.md",
    "observabilityDir": ".ai-review/observability",
    "exportDir": ".ai-review/export",
    "incrementalState": ".ai-review/incremental/state.json",
    "structureExportDir": ".ai-review/structure",
    "coverageLcov": "coverage/lcov.info",
    "astgrepConfig": "ops/ai-review/ast-grep/sgconfig.yml"
  }
}
```

Все поля необязательны — пропущенное берётся из нейтрального дефолта. Пути относительные —
от корня рабочего каталога прогона; `repoRulesFile` — от каталога самого профиля.

## Что делает каждое поле

| Поле | Зачем | Дефолт |
|------|-------|--------|
| `markerNamespace` | Маркеры sticky/inline и ключ машинного футера. **Менять на живом репозитории нельзя**: прошлые комментарии перестанут быть «своими» — потеряются carry-over находок, Δ-блок и harvest 👍/👎 | `ai-review` |
| `toolName` | Имя в SARIF, имена файлов экспорта (`<toolName>.json`, `<toolName>.sarif.json`), User-Agent к GitHub API | = `markerNamespace` |
| `sarifRulePrefix` | `ruleId` в SARIF — по нему Code Scanning группирует алерты | = `toolName` |
| `repositoryUri` | `tool.driver.informationUri` в SARIF | из `GITHUB_REPOSITORY` |
| `repoRules` / `repoRulesFile` | Блок «правила проекта» в системном промпте: слои, канонические примитивы, чем проверяется поведение. Самое сильное средство против FP-лекций «у вас неправильная архитектура» | пусто; иначе **`AGENTS.md`** в корне (или рядом с профилем), если файл есть |
| `strict` | Строгий гейт (`true`): блок и inline блокер-severity — только при prescan или `verifierStatus: verified`, advisory в строку не поднимаются; по умолчанию включает и `REVIEW_REQUIRE_QUOTE`. Складывается с `REVIEW_STRICT` через «или»: env `0` профильный `true` не выключает | `false` |
| `prompts.advisorExtra` / `specialistExtra` / `aggregatorExtra` / `verifierExtra` | Свободный текст, дописывается в соответствующий системный промпт (кастомизация без форка движка) | пусто |
| `layout.appDir` | Каталог, из которого запускается eslint/ast-grep (там `node_modules`) | корень |
| `layout.layerDirs` | Каталоги, которым можно вернуть префикс `appDir`, если путь из diff пришёл без него | — |
| `layout.lintable` | Какие файлы вообще отдаются в prescan | любой TS/JS |
| `layout.e2eSpec` | Что считать e2e-спекой: список таких файлов уходит в промпт, чтобы модель не писала «тестов нет» о PR со спекой в диффе | `*.spec.*` / `*.test.*` (`js`/`ts`, варианты `x`/`c`/`m`) под каталогом `e2e/`, `test/` или `tests/` на любой глубине: `(?:^\|/)(?:e2e\|tests?)/.+\.(?:spec\|test)\.[cm]?[jt]sx?$` |
| `layout.logic` | **Сейчас не читается**: поле валидируется и хранится, но роутер классифицирует файлы по своим зашитым `LOGIC`/`CODE` ([`lib/router.mjs`](../lib/router.mjs), `rust/src/router.rs`) — задавать его бессмысленно | `\.(?:ts\|tsx\|js\|jsx\|mjs\|cjs\|vue\|svelte)$` — только JS/TS/Vue/Svelte |
| `layout.sensitive` | Зоны, где цена пропущенного дефекта выше средней (плюс к встроенным auth/billing/migrations/workflows) — влияет на маршрут находки | пусто |
| `layout.reviewSkip` | Пути, которые **никогда не уходят моделям**: сгенерированное, что CI потребителя сверяет с источником (OpenAPI-спека, её JSON-копии, индекс эндпоинтов). Регекс или массив регексов (склеиваются в `(?:a)\|(?:b)`), без учёта регистра; битый — фича выключена. Совпавший путь роутер считает skip, он исключается из LLM-чанков, находки моделей о нём снимаются, а в sticky он учтён в строке «Не ревьюились моделями». Prescan, blast-radius и карта PR его видят. Lockfile и generated по встроенному регексу роутера исключаются и без этого поля. Подробнее — [configuration.md](configuration.md#router) | пусто |
| `astgrep.scopes` | `ruleId → regex пути`: где правило вообще применимо (globs в `sgconfig.yml` не знают слоёв проекта) | правило действует везде |
| `astgrep.allow` | `ruleId → список путей-исключений` (например, единственный модуль, которому сырой `X-Forwarded-For` положен по роли) | пусто |
| `paths.feedbackLog` | JSONL-журнал feedback | `.ai-review/feedback/fp-feedback.jsonl` |
| `paths.feedbackCandidatesDir` | Кандидаты в fixtures | `.ai-review/feedback/candidates` |
| `paths.fpPatterns` | Паттерны известных FP | пусто → стартовый набор пакета (`feedback/fp-patterns.jsonl`) |
| `paths.fpMarkdown` | Журнал FP в markdown | `docs/ai-review-false-positives.md` |
| `paths.observabilityDir` | JSONL-лог прогонов | `.ai-review/observability` |
| `paths.exportDir` | JSON/SARIF артефакт | `.ai-review/export` |
| `paths.incrementalState` | Состояние инкрементального прогона | `.ai-review/incremental/state.json` |
| `paths.structureExportDir` | Выходы `pr-review-structure` | `.ai-review/structure` |
| `paths.coverageLcov` | LCOV для structure-контура | `coverage/lcov.info` |
| `paths.astgrepConfig` | `sgconfig.yml` для prescan | пусто → ruleset пакета (`ast-grep/sgconfig.yml`) |

Любой путь из `paths` перебивается соответствующей env-переменной (см. [configuration.md](configuration.md)) —
профиль задаёт умолчание репозитория, env — решение конкретного прогона.

## Пример: reference-профиль

[`profiles/orakul.json`](../profiles/orakul.json) — профиль первого потребителя движка. Он же
используется регрессионным набором тестов: движок под ним обязан вести себя ровно так, как
вёл себя in-repo версией до выноса в отдельный пакет.

## Пример: профиль самого движка

[`.ai-reviewer.json`](../.ai-reviewer.json) в корне — профиль, под которым ревьюится сам
`ai-reviewer`; автопоиск подхватывает его без `REVIEW_PROFILE`. Показателен двумя вещами:
`layout.sensitive` покрывает `lib/`, `bin/`, `rust/src/` и `pipeline.mjs` (гейт и его
доказательства), а `repoRules` — это ~16 строк про осознанные решения (реэкспорт канонических
примитивов, fail-open, объявленные усечения, паритетные копии JS↔Rust, ноль зависимостей).
Ровно этот блок закрывает семейство FP «претензия к сознательному решению» из
[review-quality.md](review-quality.md) §2.5. Локальная подготовка окружения под такой
прогон — [model-tuning.md](model-tuning.md), «Локальный прогон с полными сигналами».

## Как завести свой

1. Скопируйте `profiles/orakul.json` в свой репозиторий (например `ops/ai-review/profile.json`).
2. Поменяйте `name`, `markerNamespace`, `toolName`, `repositoryUri`.
3. Опишите слои в `layout` — начните с `appDir` и `lintable`.
4. Напишите `repoRulesFile`: слои, канонические примитивы, чем проверяется поведение.
   Это 15–20 строк, которые лучше всего окупаются на качестве находок.
5. Пропишите `REVIEW_PROFILE` в workflow (см. [integration.md](integration.md)).
6. Прогоняйте `pr-review --dry-run --diff` на реальном патче, пока находки не станут по делу.
