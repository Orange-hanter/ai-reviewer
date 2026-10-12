---
type: guide
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-10-12
---

# Интеграция в репозиторий-потребитель

Движок — prebuilt-бинарь `pr-review` из релизов отдельного репозитория
`Orange-hanter/ai-reviewer` ([RFC 16](../proposals/16-standalone-rust-binary.md), решение 9).
Он **никогда не берётся из кода PR**: CI скачивает его по пину из базовой ветки и сверяет
sha256 до запуска — так PR не подменит движок, а подменённый релизный артефакт не пройдёт
сверку. Ни `node`, ни `npm install` движку не нужны. Код PR при этом в шаге ревью исполняется —
через prescan (eslint из `node_modules` PR) и Graphify; граница доверия — в
[§3](#секреты-и-код-pr).

Готовые шаблоны workflow — в [`examples/ci/`](../examples/ci/).

> До первого релиза с бинарями (T32) шаблоны этого раздела запускать не на чем: оставайтесь
> на пине старого формата (`v0.4.0`) и прежнем шаблоне (чекаут + `node`). Формат пина
> различим по второму полю, поэтому старый и новый способы живут параллельно.

## 1. Пин версии и скрипт сверки

```
ops/ai-reviewer-ref              # одна строка: <tag> sha256:<sha256 файла SHA256SUMS релиза>
ops/ai-review/fetch-pr-review.sh # копия examples/ci/fetch-pr-review.sh
```

Пример пина:

```
v0.5.0 sha256:3f1c…e9a0
```

Тег говорит, что качать; хэш `SHA256SUMS` закрывает цепочку доверия: файл сумм сверяется с
пином, архив бинаря — с файлом сумм, и только потом распаковывается. Хэш берётся из релиза:

```bash
curl -fsSL https://github.com/Orange-hanter/ai-reviewer/releases/download/v0.5.0/SHA256SUMS | sha256sum
```

Обновление версии движка = правка одной строки в PR: видно в диффе, откатывается ревертом.
Скрипт сверки тоже читается из базовой ветки — PR не может его ослабить.

### Контракт релиза

| Артефакт | Содержимое |
|----------|------------|
| `pr-review-<target-triple>.tar.gz` | в корне архива — исполняемый файл, член архива `pr-review` (`./pr-review` тоже принимается) |
| `SHA256SUMS` | строки `<sha256-hex>  <имя-архива>` (формат `sha256sum`, имена без путей) |

Таргеты: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`,
`aarch64-apple-darwin`. Платформа раннера определяется по `uname -s`/`uname -m`.

### Что происходит при сбое поставки

| Ситуация | Исход шага | Почему |
|----------|------------|--------|
| пина нет или он пуст; пин старого формата и нет скрипта сверки | пропуск, exit `0` | bootstrap-PR |
| пин нового формата, а скрипта сверки в базовой ветке нет | шаг красный, exit `1` | пропущен шаг подключения: иначе required-check вечно зелёный без ревью |
| сеть или сервер релизов не отвечают (нет HTTP-ответа, 5xx, 429), платформа раннера не собирается, нет `curl`/`sha256sum` | пропуск, exit `0`, `::warning::` | **fail-open**: недоступность ревью не блокирует merge |
| релиза по тегу пина нет или в нём нет архива из `SHA256SUMS` (HTTP 404/410) | шаг красный, exit `1` | релиз ответил, и ответ противоречит пину: опечатка в теге, удалённый или ещё не опубликованный релиз |
| sha256 `SHA256SUMS` ≠ пин или архив ≠ `SHA256SUMS` | шаг красный, exit `1` | **fail-closed поставки**: это подмена артефакта, а не аутедж |
| сверенный архив без `pr-review` в корне или не распаковывается | шаг красный, exit `1` | релиз собран не по контракту |
| пин старого формата при наличии скрипта, битый пин, хэш-заглушка из нулей | шаг красный, exit `1` | ошибка конфигурации в базовой ветке |

Пропуск — это пустой output `bin` шага сверки: шаг запуска стоит под
`if: steps.engine.outputs.bin != ''`. Fail-closed поставки не меняет политику самого движка:
внутри ревью инфра-сбой по-прежнему даёт exit `0`, а exit `1` — только подтверждённые блокеры.

## 2. Профиль проекта

```
ops/ai-review/profile.json      # см. docs/profiles.md
ops/ai-review/repo-rules.md     # правила проекта для промпта
```

Данные обучения тоже живут у потребителя (движок их не хранит):

```
ops/ai-review/feedback/fp-feedback.jsonl     # журнал 👍/👎
ops/ai-review/feedback/fp-patterns.jsonl     # промоутнутые паттерны FP
ops/ai-review/fixtures/                      # fixtures, выросшие из своих FP
```

## 3. Workflow

```yaml
      # Секретов у шага нет: релиз публичный. Пин и скрипт — из БАЗОВОЙ ветки.
      - name: Fetch pinned pr-review binary
        id: engine
        env:
          BASE_REF: ${{ github.base_ref }}
        run: |
          BASE_REF="${BASE_REF:-main}"
          [[ "$BASE_REF" =~ ^[A-Za-z0-9_./-]+$ ]] || { echo "invalid BASE_REF"; exit 1; }
          ENGINE="$RUNNER_TEMP/ai-reviewer"
          rm -rf "$ENGINE" && mkdir -p "$ENGINE"
          git show "origin/$BASE_REF:ops/ai-reviewer-ref" > "$ENGINE/ref" 2>/dev/null || : > "$ENGINE/ref"
          if ! git show "origin/$BASE_REF:ops/ai-review/fetch-pr-review.sh" > "$ENGINE/fetch-pr-review.sh" 2>/dev/null; then
            # пин нового формата без скрипта — не bootstrap, а пропущенный шаг подключения
            grep -Eq '^[^[:space:]]+[[:space:]]+sha256:' "$ENGINE/ref" && exit 1
            echo "bin=" >> "$GITHUB_OUTPUT"; exit 0
          fi
          bash "$ENGINE/fetch-pr-review.sh" "$ENGINE/ref" "$ENGINE/bin"

      # Секреты — только шагу запуска сверенного бинаря.
      - name: Run AI review
        if: steps.engine.outputs.bin != ''
        env:
          PR_REVIEW_BIN: ${{ steps.engine.outputs.bin }}
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          PR_NUMBER: ${{ github.event.pull_request.number }}
          OLLAMA_API_KEY: ${{ secrets.OLLAMA_API_KEY }}
          REVIEW_PROFILE: ops/ai-review/profile.json
        run: |
          "$PR_REVIEW_BIN" review
```

Полный шаблон со всеми моделями, бюджетами и артефактами —
[`examples/ci/pr-review.yml`](../examples/ci/pr-review.yml). Его контракт (нет чекаута движка,
бинарь только после сверки, пин из базовой ветки, секреты не у чекаутов) держит
`test/ci-templates.test.mjs`.

Требования к правам job'а: `contents: read`, `pull-requests: write`. **Не** давайте
`contents: write` job'у, который исполняет линтеры на коде PR (flat-config eslint — это
исполняемый JS из чекаута PR).

### Секреты и код PR

Пин и скрипт сверки из базовой ветки защищают **движок**: PR не выбирает, какой бинарь
запустится. Они не делают шаг ревью свободным от кода PR. С `REVIEW_PRESCAN=1` бинарь
запускает `app/node_modules/.bin/eslint` — он поставлен `npm ci` по lock-файлу PR
(`--ignore-scripts` отключает только lifecycle-скрипты) и грузит исполняемый
`app/eslint.config.js` из PR; `GRAPHIFY_BIN=.graphify/venv/bin/graphify` тоже путь в рабочем
дереве PR. Дочерние процессы наследуют env шага, то есть `OLLAMA_API_KEY`, `GITHUB_TOKEN`
(`pull-requests: write`) и `TELEGRAM_BOT_TOKEN`; `actions/checkout` по умолчанию к тому же
оставляет `GITHUB_TOKEN` в `.git/config`.

| Откуда PR | Секреты в шаге ревью | Что делать |
|-----------|----------------------|------------|
| форк | пустые, `GITHUB_TOKEN` только на чтение | утекать нечему |
| ветка этого репозитория | выдаются | приемлемо, если право push в репозиторий = доверие наравне с владельцами секретов; иначе `REVIEW_PRESCAN: '0'`, без `GRAPHIFY_BIN`, `persist-credentials: false` у чекаута |

## 4. Остальные workflow

| Шаблон | Что делает | Подкоманды |
|--------|------------|------------|
| [`pr-review.yml`](../examples/ci/pr-review.yml) | Ревью PR (гейт) | `review` |
| [`pr-review-feedback.yml`](../examples/ci/pr-review-feedback.yml) | Сбор 👍/👎 и метки → bot PR с журналом | `feedback-migrate`, `feedback-batch` |
| [`pr-review-feedback-promote.yml`](../examples/ci/pr-review-feedback-promote.yml) | Еженедельно: ≥2 одинаковых FP → паттерн + fixture | `feedback-promote`, `eval` |
| [`pr-review-metrics.yml`](../examples/ci/pr-review-metrics.yml) | Сводка по observability-логам прогонов | `metrics` |

Служебные шаблоны чекаутят базовую ветку, поэтому пин и скрипт сверки читают из рабочего
дерева (`bash ops/ai-review/fetch-pr-review.sh ops/ai-reviewer-ref …`). `setup-node` в них
остаётся только для инлайновых REST-хелперов (gh CLI на self-hosted раннере нет), не для движка.

Оба диспатчимых workflow потребителя (`ci.yml`, `pr-review.yml`) должны иметь
`workflow_dispatch:` — feedback-бот дёргает их после мержа данных.

## 5. Что нужно от репозитория

| Компонент | Для чего | Если нет |
|-----------|----------|----------|
| `OLLAMA_API_KEY` (или `LLM_API_KEY`) | LLM-слой | ревью не запускается: soft-fail, exit `0` при любом `REVIEW_LLM_REQUIRED` |
| `node_modules` c eslint в `layout.appDir` | eslint prescan | prescan пропускается |
| `@ast-grep/cli` | ast-grep prescan | пропускается |
| Граф graphify | blast-radius | пустой blast-radius |
| Метка `ai-review-fp` | ручная разметка FP | harvest берёт только реакции |

Всё перечисленное fail-open: ревью деградирует или пропускается, merge не блокируется.
`REVIEW_LLM_REQUIRED=1` действует, только когда ключ задан, но все модели прогона упали:
тогда гейт fail-closed. Отсутствие ключа — конфигурация, а не аутедж, и блоком не считается.

## 6. Локальный прогон

```bash
bash ops/ai-review/fetch-pr-review.sh ops/ai-reviewer-ref ~/.local/ai-reviewer   # печатает путь
git diff origin/main... > /tmp/changes.patch
OLLAMA_API_KEY=… REVIEW_PROFILE=ops/ai-review/profile.json \
  ~/.local/ai-reviewer/pr-review review --dry-run --diff /tmp/changes.patch
```

Из клона движка: `cargo build --manifest-path rust/Cargo.toml` и
`rust/target/debug/pr-review review --dry-run --diff …`. Прежний `node bin/pr-review.mjs`
работает до удаления JS-ядра (T33).

## 7. Swarm / оркестратор (E22–E23)

При `REVIEW_EXPORT=1` в каталоге экспорта появляются:

- `<tool>.json` — полный payload, включая поле `swarm`;
- `<tool>-swarm.json` — только контракт внешнего вердикта.

Описание полей и правила `strong` — [swarm.md](swarm.md). Готовый kit для Orakul:
[`examples/consumers/orakul/`](../examples/consumers/orakul/).

## 8. Обновление версии

1. В движке: зелёный `npm run check` (`npm test` + `npm run eval` + `check:rust`: `cargo build`,
   `cargo test`, паритет JS ↔ Rust), тег `vX.Y.Z` → релиз с четырьмя архивами и `SHA256SUMS`.
2. У потребителя: правка `ops/ai-reviewer-ref` (`vX.Y.Z sha256:<хэш SHA256SUMS>`) → PR → ревью
   нового движка идёт **старым** (пин читается из базовой ветки) → merge.
3. Первый прогон после bump сравнить с предыдущим: Δ-блок в sticky показывает, что изменилось
   в наборе находок на том же коде.

## 9. Переход со старого пина (чекаут + node)

Breaking для потребителя ([CHANGELOG](../CHANGELOG.md), Unreleased). Порядок — одним PR:

1. Скопировать `examples/ci/fetch-pr-review.sh` → `ops/ai-review/fetch-pr-review.sh`.
2. Заменить workflow на шаблоны из `examples/ci/` (шаги `Read pinned engine ref` и
   `Checkout pinned AI review engine` уходят, их место — `Fetch pinned pr-review binary`).
3. `ops/ai-reviewer-ref`: `v0.4.0` → `vX.Y.Z sha256:<хэш SHA256SUMS>`.

Ревью самого этого PR пойдёт по базовой ветке: старый шаблон из base не участвует — workflow
берётся из PR, а пин и скрипт сверки ещё отсутствуют в базе, поэтому новый шаблон штатно
пропустит ревью (bootstrap). Начиная со следующего PR работает бинарь.
