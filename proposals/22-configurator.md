# 22. Конфигуратор: flow настройки с применением через PR

> Категория: 🔵 Архитектура · Риск: средний · Зависимости: RFC 20 (Rust — основная реализация), RFC 16, RFC 05

## Контекст

Движок читает ≈160 env-переменных, ≈20 полей профиля и набор флагов CLI. Потребитель
настраивает их правкой env-блока в `.github/workflows/pr-review.yml` и JSON-профиля, а связи
между опциями (что зажимается, что без чего бесполезно, что рвёт сходимость) описаны только
прозой в `docs/configuration.md`. Ошибка конфигурации видна лишь по следующему прогону —
или не видна вовсе.

Нужен flow, который проводит по сценарию и стадиям пайплайна, показывает последствия выбора
до применения и сам применяет результат в репозитории-потребители. Основной адресат — свои
репозитории с self-hosted раннерами на Арче (5800X) и фолбэком на `ubuntu-latest`.

Решения, принятые на интервью (2026-10-29):

| Вопрос | Решение |
|--------|---------|
| Что делает flow на выходе | Применяет сам |
| Канал применения | PR в репозиторий-потребитель; конфиг действует после merge |
| Ось flow | Сценарий → стадии пайплайна |
| Старт | Текущий конфиг из `main` + пресеты, накладываемые как дельта |
| Цели прогона | Арч self-hosted и `ubuntu-latest`; локальный dry-run и MCP — нет |
| Провайдеры | Ollama Cloud, OpenRouter |
| Где живёт | Постоянный сервис на Арче (systemd), доступ по WireGuard/LAN |
| Доступ и запись | Слушает только адрес WG/LAN, вход по токену сессии; пишет fine-grained PAT |
| Репозитории | Все репозитории владельца с `ops/ai-reviewer-ref`; bootstrap нового |
| Пакетность | Один flow → N репозиториев → N PR с общей env-дельтой |
| Глубина опций | Все, слоями: основные / «Эксперт» / скрытые (dev-only и без эффекта) |
| Версия | Схема опций берётся по пину; bump пина — шаг flow, входит в тот же PR |
| Последствия | Конфликты и зависимости; цена и время по истории; живая схема пайплайна |
| Секреты | Задаются через write-only форму |
| Раннер | Тумблер `vars.SELF_HOSTED_CI` + статус раннеров и их тулчейна |
| Профиль | Редактируется полностью; регэкспы сверяются с деревом файлов репозитория |
| Сопутствующие workflow | `pr-review-metrics.yml` — да; feedback-harvest/promote — шаг есть, выключен |

## Текущее состояние

- Конфиг собирается в `load_review_config_parts` (`rust/src/config.rs:654`); JS-копия —
  `loadReviewConfig` (`lib/config.mjs:85`), deprecated по RFC 20. Машинного перечня опций
  нет: ключи разбросаны литералами (`env_flag`, `or_env`, `num_env`), например
  `GRAPH` — `rust/src/config.rs:750`, `TELEGRAM_BOT_TOKEN` — `rust/src/config.rs:1064`.
- Описание опций — только таблицы `docs/configuration.md` и `docs/profiles.md`.
- Шаблон потребителя `examples/ci/pr-review.yml`: env-блок ≈40 переменных в шаге
  `Run AI review`, раннер — `vars.SELF_HOSTED_CI` (`examples/ci/pr-review.yml:36`). Шаблон
  покрыт контрактными тестами в `test/pr-review.eval.test.mjs`.
- Раннеры Арча: `github-runner@{ci,review-2}.service` в `/opt/gh-runner/`, зарегистрированы
  только на `Orange-hanter/Orakul`, работают от пользователя `daniil`.
- Релиз прикладывает только архивы бинарей (`.github/workflows/release-binaries.yml:141`);
  описание опций конкретного тега потребителю недоступно.

Что интервью и разбор нашли в текущей конфигурации — то, что конфигуратор должен ловить
правилами:

| Место | Что не так |
|-------|------------|
| `examples/ci/pr-review.yml` | `REVIEW_LLM_TIMEOUT_MS: '240000'` при дефолте движка 420000; `docs/configuration.md` прямо говорит, что 240000 обрывали половину prose-вызовов |
| `REVIEW_STRUCTURE` | Пишется в конфиг, никто не читает — эффекта нет |
| `layout.logic` профиля | Не читается роутером (`LOGIC`/`CODE` зашиты в `lib/router.mjs`, `rust/src/router.rs`) |
| Промпт жюри (`AGGREGATOR_HEAD`, `<confidence>` в `rust/src/prompt.rs:368`, `:403`) | Порог «0.8» и уровни «critical and major» записаны текстом. При другом `REVIEW_CONFIDENCE_MIN` или `REVIEW_BLOCK_ON` модель калибруется под 0.8 |
| `REVIEW_CONSENSUS` в `moa` | Всегда зажимается до `1`; значение из env молча игнорируется |
| Шаги upload в шаблоне | Пути `.graphify/graphify-out/observability` и `review-export/` — из профиля Orakul, у нейтрального профиля другие (`.ai-review/…`) |

## Предложение

### 1. Схема опций — `schemas/config-options.v1.json`

Единственный машинный источник для конфигуратора и для документации. Элемент `options[]`:

```jsonc
{
  "key": "REVIEW_VERIFIER_STRICT",
  "kind": "env",               // env | profile | workflow | var | secret
  "type": "flag",              // flag | int | float | enum | list | string | regex | json | path
  "default": true,
  "enum": null, "range": null, // для int/float — [min, max]; для enum — значения
  "stage": "verifier",         // scenario | router | context | prescan | moa | verifier | gate |
                               // delivery | economy | artifacts | profile | access
  "layer": "main",             // main | expert | hidden
  "effect": ["gate"],          // gate | cost | latency | delivery | artifact | convergence
  "fingerprint": false,        // входит ли в configFingerprint инкрементального прогона
  "failMode": "closed",        // open | closed | null — для схемы пайплайна
  "since": "0.3.0",
  "deadReason": null,          // непусто → layer hidden, в UI «нет эффекта»
  "docs": "docs/configuration.md#verifier-pass-b"
}
```

`rules[]` — декларативные связи, вычисляются детерминированно и на сервере, и в странице:

```jsonc
{ "id": "consensus-clamped-in-moa",
  "when": { "all": [{ "eq": ["REVIEW_MODE", "moa"] }, { "gt": ["REVIEW_CONSENSUS", 1] }] },
  "level": "warn",              // info | warn | error (error запрещает Apply)
  "message": "В moa REVIEW_CONSENSUS зажимается до 1 — значение не действует" }
```

Стартовый набор правил: зажим консенсуса; `REVIEW_INCREMENTAL=1` без шага `actions/cache`
на каталог состояния; `REVIEW_BLAST_REQUIRED=1` без шага сборки графа; смена
`markerNamespace` на репозитории с прошлыми прогонами (`error`, снимается явным
подтверждением); `REVIEW_LLM_TIMEOUT_MS < 300000`; `REVIEW_VERIFIER=0` при
`REVIEW_VERIFIER_STRICT=1` (блокеры станут minor); модель verifier'а или агрегатора в пуле
advisors; `REVIEW_PROMPT_LAYOUT=shared-prefix` на Ollama Cloud; `layout.reviewSkip` без `REVIEW_PROFILE_TRUSTED=1` и `REVIEW_BASE_REF` (игнорируется целиком); опция с `deadReason`.

Контрактный тест `test/config-options.test.mjs`: множество ключей схемы `kind: env` равно
объединению (а) строковых литералов-ключей, читаемых в `rust/src/config.rs`, и
(б) ключей из таблиц `docs/configuration.md`. Новая переменная без записи в схеме —
красный тест. Тест на node, как остальные контракты шаблонов; продуктового JS не добавляет.

Схема прикладывается к GitHub-релизу (`config-options.v1.json` рядом с архивами бинарей).
Для пинов до первого релиза со схемой конфигуратор берёт свою схему и помечает шаг
«Версия» предупреждением.

### 2. Сервис — Rust-бинарь `configurator`

Модуль `rust/src/configurator/` и бинарь `rust/src/bin/configurator.rs`. Продуктового JS
нет: страница — статика (`tools/configurator/`), встроенная в бинарь `include_str!`.
HTTP — крейт `tiny_http` (синхронный, без async-рантайма; линкуется только в этот бинарь).
GitHub — существующий `rust/src/github.rs` (`reqwest` blocking).

| Метод | Путь | Что делает |
|-------|------|------------|
| GET | `/api/repos` | Репозитории владельца с `ops/ai-reviewer-ref` в default-ветке + кандидаты на bootstrap |
| GET | `/api/repos/{o}/{r}/state` | Пин, `pr-review.yml`, профиль, `vars`, наличие секретов, открытый PR конфигуратора |
| GET | `/api/schema?ref=vX.Y.Z` | Схема опций тега (кэш на диске по тегу) |
| GET | `/api/repos/{o}/{r}/tree` | Список путей default-ветки — для проверки регэкспов профиля |
| GET | `/api/repos/{o}/{r}/history?n=20` | `run_end` последних N прогонов из artifacts `pr-review-observability-*` |
| GET | `/api/runners` | Раннеры репозиториев (online/busy) + проба тулчейна хоста: `node`, `gh`, `ast-grep`, graphify venv |
| GET | `/api/prompts?ref=vX.Y.Z` | Системные промпты всех вызовов (роли, слитая панель, жюри, verifier — с условными вариантами blast/priors) от бинаря `prompt` того же тега; на месте вставок профиля — метки `⟦REPO_RULES⟧`, `⟦*_EXTRA⟧`, страница подставляет значения |
| POST | `/api/plan` | `{repos[], delta}` → дифф файлов, сработавшие правила, оценка цены; ничего не пишет |
| POST | `/api/apply` | По одному PR на репозиторий из ветки `ai-reviewer-config/<slug>`; открытый PR конфигуратора обновляется, а не плодится |
| PUT | `/api/repos/{o}/{r}/secrets/{name}` | Write-only: `gh secret set` с `GH_TOKEN`=PAT, значение через stdin, не через argv |
| PUT | `/api/repos/{o}/{r}/vars/SELF_HOSTED_CI` | Тумблер раннера — единственная запись мимо PR |

**Запись workflow без YAML-парсера.** Конфигуратор владеет только размеченной областью
env-блока:

```yaml
        env:
          # >>> ai-reviewer-configurator: env (генерируется, правки руками перезапишутся)
          REVIEW_MODE: 'moa'
          # <<< ai-reviewer-configurator: env
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

Внутри области — только опции, отличающиеся от дефолтов пина, значения в одинарных
кавычках. Ссылки на секреты и всё вне области не трогаются. Workflow без разметки
(первое применение, bootstrap) генерируется целиком из `examples/ci/pr-review.yml` того же
тега, и PR показывает полный дифф. Профиль пишется через `serde_json` с `preserve_order` —
дифф минимальный.

**Оценка цены.** Медиана `wallMs`, токенов и `cost_usd` по последним N `run_end`,
масштабированная на отношение числа LLM-вызовов новой конфигурации к текущей (роли × модели
× чанки, verifier-батчи). Без истории — только относительная оценка «×1.4 вызовов».
В UI подписано как оценка.

### 3. Flow

| # | Шаг | Опции |
|---|-----|-------|
| 0 | Репозитории и версия | Выбор 1…N репозиториев; пин, bump с диффом схемы (новые/удалённые опции) и выдержкой CHANGELOG |
| 1 | Сценарий | Раннер (Арч / ubuntu-latest / тумблер), провайдер (Ollama Cloud / OpenRouter), пресет-дельта (Экономный / Шаблон движка / Строгий) |
| 2 | Router | `REVIEW_ROUTER` |
| 3 | Контекст | Окна, карточки символов, blast-radius, Graphify |
| 4 | Prescan | eslint, ast-grep, block-правила, `REVIEW_APP_DIR` |
| 5 | MoA | Режим, роли → модели, merge ролей, агрегатор, температуры, effort, `LLM_MODEL_SETTINGS` |
| 6 | Verifier | Модель, strict, батчи, контекст, карточки |
| 7 | Гейт | `BLOCK_ON`, `BLOCK_SINGLE`, confidence, `LLM_REQUIRED`, strict, require-quote |
| 8 | Доставка | Inline, actionability, auto-triage, reasoning в sticky, иконки, Telegram |
| 9 | Экономия и бюджеты | Incremental (+ шаг `actions/cache`), chunk-routing, размеры чанков, таймауты, concurrency, тайм-бюджет |
| 10 | Артефакты | Export/SARIF, observability, `REVIEW_PRICE_JSON`, `pr-review-metrics.yml`; feedback — свёрнут, выключен |
| 11а | Промпты моделей | Настоящие тексты каждого вызова при текущей конфигурации, по разделам; вставки правил проекта и `prompts.*Extra` подсвечены; пример user-сообщения |
| 11 | Профиль | Маркеры, layout-регэкспы с проверкой на дереве, ast-grep scopes/allow, repoRules, `prompts.*Extra`. В пакетном режиме — по вкладке на репозиторий |
| 12 | Секреты и раннер | Наличие секретов, форма записи; статус раннеров и тулчейна |
| 13 | Проверка и применение | Дифф по файлам, правила, оценка цены → Apply → ссылки на PR |

Боковая панель на всех шагах: схема пайплайна (включённые стадии, модель на роли,
fail-open/fail-closed), сработавшие правила, оценка цены.

### 4. Развёртывание на Арче

- Системный пользователь `ai-reviewer-cfg`, **не** `daniil`: раннер `review-2` исполняет
  код PR (flat-config eslint) от `daniil`, и PAT с правом на секреты не должен быть ему
  доступен.
- `ai-reviewer-configurator.service`: `LoadCredential=pat:/etc/ai-reviewer-configurator/pat`,
  `LoadCredential=session:/etc/ai-reviewer-configurator/session-token`,
  `ProtectSystem=strict`, `StateDirectory=` под кэш схем.
- Слушает адрес WG-интерфейса (и LAN — по флагу). Вход — форма с токеном сессии, cookie
  `HttpOnly; SameSite=Strict`; на POST/PUT — проверка `Origin`.
- Fine-grained PAT на репозитории с пином: Contents RW, Pull requests RW, Variables RW,
  Secrets RW, Actions R (artifacts), Administration R (список self-hosted раннеров).

## Миграция

1. Схема + контрактный тест + правка `examples/ci/pr-review.yml`: разметка env-области,
   `REVIEW_LLM_TIMEOUT_MS` убран из шаблона (действует дефолт 420000). Поведение движка
   не меняется.
2. `release-binaries.yml` прикладывает `config-options.v1.json` к релизу.
3. Бинарь `configurator` read-only (`state`, `schema`, `plan`) — проверка на Orakul без записи.
4. `apply`, секреты, тумблер раннера.
5. Unit-файл и `docs/configurator.md` (установка на Арче, состав PAT).

У потребителя ничего не меняется, пока он не смержит PR конфигуратора. Инвариант
pwn-request сохраняется: bump пина в PR действует только после merge.

## Риски и что не делаем

- **PAT с Secrets RW на постоянно включённой машине, которая исполняет код PR.** Снижается
  отдельным системным пользователем и `LoadCredential`; остаточный риск — root на хосте.
- **Разметка env-области — новый контракт шаблона.** Ручная правка внутри области
  перезаписывается; PR показывает это в диффе.
- **Оценка цены по истории неточна** при смене моделей: история другой модели не
  переносится, показывается только отношение числа вызовов.

Не делаем: запись в `main` мимо PR; авто-merge; регистрацию раннеров и управление
systemd-сервисами раннеров; локальный dry-run и MCP как цели; провайдеров кроме Ollama Cloud
и OpenRouter (шим подписки Claude, локальный Ollama); YAML-парсер и правки вне размеченной
области; dry-run нового конфига на живом PR (отдельный RFC при необходимости); влияние на
гейт — конфигуратор не импортируется ни `pipeline.mjs`, ни `lib/*.mjs`.

## Оценка

| # | Задача |
|---|--------|
| 1 | `schemas/config-options.v1.json` — все опции и стартовые правила |
| 2 | `test/config-options.test.mjs` — схема ↔ `rust/src/config.rs` ↔ `docs/configuration.md` |
| 3 | Шаблон: разметка env-области, таймаут; контрактные тесты шаблона |
| 4 | Релиз: схема как ассет |
| 5 | `rust/src/configurator/`: чтение состояния, схема по тегу, дерево, история, раннеры |
| 6 | Правила и `plan`: рендер env-области и профиля, оценка цены |
| 7 | `apply`, секреты, тумблер; ветка и обновление открытого PR |
| 8 | Страница `tools/configurator/` по прототипу; `include_str!` |
| 9 | systemd-юнит, `docs/configurator.md`, установка на Арче |
