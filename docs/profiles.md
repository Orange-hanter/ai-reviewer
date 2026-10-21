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
| `layout.reviewSkip` | Пути, которые **никогда не уходят моделям**: сгенерированное, что CI потребителя сверяет с источником (OpenAPI-спека, её JSON-копии, индекс эндпоинтов). Регекс или массив регексов (склеиваются в `(?:a)\|(?:b)`), без учёта регистра; битый — фича выключена. Применяется **только при `REVIEW_PROFILE_TRUSTED=1` и `REVIEW_BASE_REF`** (движок сам читает профиль из base, см. [«Доверие»](#доверие)). Совпавший путь роутер считает skip, он исключается из LLM-чанков, находки моделей о нём снимаются, а в sticky он учтён в строке «Не ревьюились моделями». Prescan, blast-radius и карта PR его видят. Исключается только инертный файл (allowlist); код, `.github/`, пин движка, всё, что читает движок, и `layout.sensitive` — никогда. См. [ниже](#layoutreviewskip-и-доверие). npm/yarn/pnpm lockfile и source map JS/TS/CSS исключаются и без этого поля. Подробнее — [configuration.md](configuration.md#router) | пусто |
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

## `layout.reviewSkip` и доверие

**Trust gate.** `reviewSkip` применяется, только когда потребитель объявил профиль доверенным:
`REVIEW_PROFILE_TRUSTED=1` **и** назвал base: `REVIEW_BASE_REF` — SHA коммита (40/64 hex,
в CI — `github.event.pull_request.base.sha`) или полный ref `refs/remotes/origin/main` /
`refs/heads/main`. Короткое имя (`origin/main`, `main`, `HEAD`) отвергается до git: git
резолвит его по правилам DWIM, и ref с тем же именем из checkout'а PR подменил бы base. Без флага `reviewSkip` игнорируется целиком, в логе одна строка
`reviewSkip ignored: profile not marked trusted (REVIEW_PROFILE_TRUSTED)`. Флаг без
`REVIEW_BASE_REF`, с невалидным ref или ref, который git не резолвит в коммит, **снимается**:
`REVIEW_PROFILE_TRUSTED=1 снят: <причина>` в логе, дальше — как без флага.

### Доверие

В потребителе рабочее дерево — checkout PR. Всё, что в нём лежит, пишет автор PR, и любой
файл, который движок читает как настройку гейта, — способ этот гейт ослабить: промпт
(`repoRulesFile`, `AGENTS.md`), FP-паттерны, снимающие находки (`paths.fpPatterns`), правила
ast-grep, сам профиль с `reviewSkip`. Модель доверия:

| Вход | Без доверия | `REVIEW_PROFILE_TRUSTED=1` + `REVIEW_BASE_REF` |
|------|-------------|-----------------------------------------------|
| файл профиля (`REVIEW_PROFILE` / автопоиск) | рабочее дерево | `git cat-file blob <base>:<путь>` |
| `repoRulesFile`, `AGENTS.md`/`agents.md` (каталог профиля и корень) | рабочее дерево | base |
| `paths.fpPatterns` (`REVIEW_FP_PATTERNS`) | рабочее дерево | base |
| `paths.astgrepConfig` + его `ruleDirs`/`utilDirs` | рабочее дерево | копия base во временном каталоге, ast-grep запускается с ней |
| `layout.reviewSkip` | не применяется | применяется |

- Файла нет в base — он **отсутствует**: копия из рабочего дерева не подставляется
  (`repoRulesFile` → пустые правила, fpPatterns → стартовый набор пакета, sgconfig → ast-grep
  пропущен).
- Путь вне рабочего каталога или внутри пакета движка (`.ai-reviewer-engine/` в checkout'е
  потребителя) читается с диска: его не пишет автор PR.
- `paths.fpMarkdown` и `paths.feedbackLog` прогон ревью не читает: их пишет и читает
  feedback-цикл (`pr-review-feedback`), у которого свой checkout.
- Не покрыто: конфиг eslint prescan, граф graphify, состояние инкрементального прогона,
  lcov — они либо собираются в CI заново, либо не управляют гейтом.

CI-шаблон дополнительно восстанавливает эти файлы из base до запуска движка (защита для пинов
старше этой версии и для инструментов, читающих диск). Шаг в
[`examples/ci/pr-review.yml`](../examples/ci/pr-review.yml):

```sh
BASE="$BASE_SHA"   # github.event.pull_request.base.sha; без события PR — refs/remotes/origin/$BASE_REF
# файлы: нет в base — удалить из рабочего дерева
for f in ops/ai-review/profile.json ops/ai-review/repo-rules.md AGENTS.md agents.md ops/pr-review/feedback/fp-patterns.jsonl; do
  git show "$BASE:$f" > "$f" 2>/dev/null || rm -f "$f"
done
# каталоги ast-grep: целиком из base (добавленное в PR — удалить)
for d in ops/pr-review/ast-grep; do
  rm -rf "$d"; git ls-tree -d --name-only "$BASE" -- "$d" | grep -q . && git checkout "$BASE" -- "$d"
done
# env шага ревью:
#   REVIEW_PROFILE_TRUSTED: '1'
#   REVIEW_BASE_REF: ${{ github.event.pull_request.base.sha || format('refs/remotes/origin/{0}', github.base_ref || 'main') }}
```

**Встроенный skip** (без профиля) убирает из входа моделей только npm/yarn/pnpm lockfile
(`package-lock.json`, `yarn.lock`, `pnpm-lock.yaml` — точно по имени) и source map JS/TS/CSS
(`*.js.map`, `*.mjs.map`, `*.cjs.map`, `*.jsx.map`, `*.ts.map`, `*.tsx.map`, `*.css.map`).
Голый `*.map` (nginx-карты, данные) не skip. Остальной класс skip роутера (`dist/`, `build/`,
`*.generated.*`, произвольный `*.lock`) решает только маршрут PR.

**Источники пакетов в lockfile** проверяются детерминированно, не моделью. Каждое значение
добавленной строки npm/yarn/pnpm lockfile — не только `resolved`/`tarball`, но и ключи
записей yarn, `version`, `requires`, `repo`, любые поля — декодируется так, как его прочтёт
менеджер пакетов (двойные кавычки — JSON-литерал, одинарные — YAML, иначе plain-токен), и
хост берётся из `new URL()` декодированного значения. Находка **major** в форме prescan
(`ruleId: lockfile-foreign-registry`, `prescanBlock: true`), если источник:

| Источник | Заголовок |
|----------|-----------|
| `https://` на хосте вне `registry.npmjs.org`, `registry.yarnpkg.com`, env `REVIEW_LOCKFILE_REGISTRIES` | `…не из разрешённого реестра — <host>` |
| `http://` на разрешённом хосте | `…по нешифрованному http — <host>` |
| другая схема с `//`: `git+ssh://`, `git+https://`, `ssh://`, `file://` | `…не из реестра по https — <scheme>//<host>` |
| `git:`/`git+…`, `github:`/`gitlab:`/`bitbucket:`/`gist:`, `git@host:path`, `exec:`, `patch:` | `…из git/хостинга кода — <prefix>` |
| `file:`/`link:`/`portal:` с абсолютным путём, `~` или выходом выше корня репозитория | `…локальный источник пакета вне репозитория` |
| обратный слэш в значении, `%`-escape, userinfo (`@`) или порт в адресе, URL не парсится, кавычка без пары | `…источник пакета не разобран однозначно — <причина>` (fail-closed) |

`npm:`-алиасы и `workspace:` — реестр и монорепо, не находка. В `package-lock.json`
пропускаются ссылки на донаты: строка ровно вида `"url": "…"`/`"funding": "…"` и
многострочная запись `"funding": [ … ]` из `type`/`url`/строк; любой другой ключ внутри
снимает пропуск. Yarn berry с `patch:` даёт находку всегда (в его спецификации `%3A`). Она отменяет router skip/light
(PR из одного lockfile идёт полным путём гейта) и, как major eslint, блокирует merge при
`REVIEW_BLOCK_SINGLE`, включающем `major`. Профилем список реестров не расширяется: профиль
недоверенного прогона пишет автор PR. `composer.lock`, `Cargo.lock`, `poetry.lock`,
`Gemfile.lock` такой проверки не имеют и поэтому уходят моделям; скрыть их может только
`reviewSkip` доверенного профиля.

**Allowlist** — действует и для доверенного профиля. Из входа моделей можно убрать только
инертный файл:

| Инертно (skip допустим) | Пример |
|-------------------------|--------|
| lockfile (по имени, список выше) | `package-lock.json`, `app/yarn.lock` |
| source map JS/TS/CSS | `*.js.map`, `*.min.js.map`, `*.css.map` (голый `*.map` — нет) |
| документация | `*.md`, `*.mdx`, `*.rst` |
| данные | `*.json`, `*.yaml`, `*.yml` — `openapi.json`, `orakul-api.yaml` |

Не инертны никогда, даже с подходящим расширением:
- dotfile — имя начинается с `.` (`.eslintrc.json`, `.gitlab-ci.yml`, `.cursorrules`, `.env`);
- всё под `.github/`, `.gitlab/`, `.circleci/`, `.husky/`, `.devcontainer/`, `.vscode/`, `.idea/`,
  `k8s/`, `helm/`, `charts/`, `terraform/`, `infra/`, `deploy/` (в любом месте пути);
- инструкции агентам: `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `copilot-instructions.md`;
- конфиги сборки/CI/инструментов по имени: `package.json`, `tsconfig*.json`, `jsconfig*.json`,
  `*.config.json`, `docker-compose*.yml`, `compose*.yml`, `openapitools.json`, `renovate.json`,
  `vercel.json`, `netlify.json`/`.yml`, `azure-pipelines.yml`, `bitbucket-pipelines.yml`,
  `action.yml`, `Chart.yaml`, `values*.yaml`.

Всё прочее (`.txt`, shell, SQL, `Dockerfile`, `Makefile`, `.tf`, `.toml`, `ini`, код) не
исключается, что бы ни говорил паттерн. Причина в логе — `not-inert`.

Сверх allowlist файл не исключается, если это:

| Путь | Причина | Почему |
|------|---------|--------|
| исходник по расширению (`ts`/`tsx`/`js`/`mjs`/`cjs`/`py`/`rs`/`go`/… — регексы `LOGIC`/`CODE` роутера) | `code` | там живёт поведение; `x.generated.ts` и `dist/foo.js` тоже |
| всё под `.github/` | `.github` | workflows, actions, CODEOWNERS |
| пин движка `ai-reviewer-ref` (в любом каталоге) | `engine-pin` | им выбирается версия гейта |
| всё, что читает движок, путём от корня репозитория | `profile` | правка входа гейта не должна прятаться самим гейтом |
| пути, совпавшие с `layout.sensitive` | `sensitive` | объявленная зона повышенной цены ошибки |

«Всё, что читает движок» — не перечень известных файлов, а инверсия:
- файл профиля (`REVIEW_PROFILE` / автопоиск), `.ai-reviewer.json`, `ai-reviewer.config.json`,
  `repoRulesFile` (от каталога профиля), `AGENTS.md`/`agents.md` рядом с профилем и в корне;
- **каждое** строковое значение `paths.*` профиля — и как файл, и как каталог-префикс;
- путевые поля итогового конфига (env перебивает профиль): `incremental.stateFile`,
  `graphify.graph`/`report`, `structure.graph`/`graphReport`/`graphAnalysis`/`lcov`/
  `obligations`/`mutationReport`/`propertyReport`, `feedback.logFile`/`mdPath`/`patternsFile`;
  каталоги-префиксы `feedback.candidatesDir`, `export.dir`, `observability.dir`,
  `structure.exportDir`;
- sgconfig ast-grep и каталоги из его `ruleDirs`, `utilDirs`, `testConfigs[].testDir`/
  `snapshotDir` (относительно каталога конфига); при доверии — каталоги и версии из base, и
  версии из PR. Не прочитался или не разобрался — fail-closed: защищается каталог конфига и
  любой `.yml`/`.yaml` в PR. Разбор — подмножество YAML (парсера в зависимостях нет), и любая
  строка вне него даёт «не разобрался»: ключ в кавычках, `---`, `?`, флоу-маппинг, якорь/алиас,
  merge-ключ `<<`, вложенные строки под ключом, который не `ruleDirs`/`utilDirs`/`testConfigs`/
  `languageGlobs`/`customLanguages`/`languageInjections`, кавычки или флоу внутри значения.

Совпадение с защищённым путём просто не применяется: файл ревьюится как обычно, его прошлые
находки переносятся как обычно. В логе CI — строка `skip-пути: N совпадений проигнорировано`
со списком `путь (reviewSkip|builtIn: причина)`.

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
