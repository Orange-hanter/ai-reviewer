---
type: guide
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-09-29
---

# Интеграция в репозиторий-потребитель

Движок живёт в отдельном репозитории и **никогда не берётся из кода PR**: CI выкачивает его
по пину — так вредоносный PR не сможет выполнить свой код с секретами прогона.

Готовые шаблоны workflow — в [`examples/ci/`](../examples/ci/).

## 1. Пин версии

```
ops/ai-reviewer-ref     # одна строка: тег (v0.2.0) или полный commit SHA
```

Обновление версии движка = правка одной строки в PR: видно в диффе, откатывается ревертом.

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
      - name: Read pinned engine ref
        id: pin
        env:
          BASE_REF: ${{ github.base_ref }}
        run: |
          BASE_REF="${BASE_REF:-main}"
          [[ "$BASE_REF" =~ ^[A-Za-z0-9_./-]+$ ]] || { echo "invalid BASE_REF"; exit 1; }
          # Пин читаем из БАЗОВОЙ ветки: файл из кода PR позволил бы подменить движок.
          REF="$(git show "origin/$BASE_REF:ops/ai-reviewer-ref" | tr -d '[:space:]')"
          [[ "$REF" =~ ^[A-Za-z0-9_./-]+$ ]] || { echo "invalid engine ref"; exit 1; }
          echo "ref=$REF" >> "$GITHUB_OUTPUT"

      - name: Checkout pinned AI review engine
        uses: actions/checkout@v7
        with:
          repository: Orange-hanter/ai-reviewer
          ref: ${{ steps.pin.outputs.ref }}
          path: .ai-reviewer-engine

      - name: Run AI review
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          PR_NUMBER: ${{ github.event.pull_request.number }}
          OLLAMA_API_KEY: ${{ secrets.OLLAMA_API_KEY }}
          REVIEW_PROFILE: ops/ai-review/profile.json
        run: node .ai-reviewer-engine/bin/pr-review.mjs
```

Полный шаблон со всеми моделями, бюджетами и артефактами —
[`examples/ci/pr-review.yml`](../examples/ci/pr-review.yml).

Требования к правам job'а: `contents: read`, `pull-requests: write`. **Не** давайте
`contents: write` job'у, который исполняет линтеры на коде PR (flat-config eslint — это
исполняемый JS из чекаута PR).

## 4. Остальные workflow

| Шаблон | Что делает |
|--------|------------|
| [`pr-review.yml`](../examples/ci/pr-review.yml) | Ревью PR (гейт) |
| [`pr-review-feedback.yml`](../examples/ci/pr-review-feedback.yml) | Сбор 👍/👎 и метки → bot PR с журналом |
| [`pr-review-feedback-promote.yml`](../examples/ci/pr-review-feedback-promote.yml) | Еженедельно: ≥2 одинаковых FP → паттерн + fixture |
| [`pr-review-metrics.yml`](../examples/ci/pr-review-metrics.yml) | Сводка по observability-логам прогонов |

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
git diff origin/main... > /tmp/changes.patch
OLLAMA_API_KEY=… REVIEW_PROFILE=ops/ai-review/profile.json \
  node .ai-reviewer-engine/bin/pr-review.mjs --dry-run --diff /tmp/changes.patch
```

В монорепо удобно держать клон движка рядом и запускать `node ../ai-reviewer/bin/pr-review.mjs`.

## 7. Swarm / оркестратор (E22–E23)

При `REVIEW_EXPORT=1` в каталоге экспорта появляются:

- `<tool>.json` — полный payload, включая поле `swarm`;
- `<tool>-swarm.json` — только контракт внешнего вердикта.

Описание полей и правила `strong` — [swarm.md](swarm.md). Готовый kit для Orakul:
[`examples/consumers/orakul/`](../examples/consumers/orakul/).

## 8. Обновление версии

1. В движке: зелёный `npm run check` (`npm test` + `npm run eval` + `check:rust`: `cargo build`,
   `cargo test`, паритет JS ↔ Rust), тег `vX.Y.Z`. После публикации GitHub-релиза workflow
   `release-binaries` прикладывает к нему архивы Rust-бинарей и sha256. Дождись их: шаблон
   потребителя качает бинари по пину, а без архива движок работает на deprecated JS-копиях.
   Пин на SHA или ветку архива не находит — пиньте тег.
2. У потребителя: правка `ops/ai-reviewer-ref` → PR → ревью нового движка идёт **старым**
   (пин читается из базовой ветки) → merge.
3. Первый прогон после bump сравнить с предыдущим: Δ-блок в sticky показывает, что изменилось
   в наборе находок на том же коде.
