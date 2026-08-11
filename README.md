# ai-reviewer

Движок AI код-ревью для pull request'ов: unified diff → blast-radius по графу кода →
детерминированный prescan (eslint + ast-grep) → LLM (Mixture-of-Agents с жюри и verifier)
→ inline-комментарии и sticky-отчёт в GitHub.

Merge блокируется только на **подтверждённых** `critical`/`major` — тех, что пережили
refutation жюри и второй проход независимой модели. Инфра-сбои merge не блокируют.

Движок ничего не знает о конкретном репозитории: маркеры комментариев, правила проекта для
промпта, раскладка каталогов, scope правил и пути данных приходят из
[профиля проекта](docs/profiles.md) — одного JSON-файла на стороне потребителя.

## Быстрый старт

```bash
git clone git@github.com:Orange-hanter/ai-reviewer.git
cd ai-reviewer
npm test && npm run eval          # зависимостей нет — только Node ≥ 22
```

Offline-ревью произвольного диффа, без GitHub:

```bash
git -C /path/to/repo diff origin/main... > /tmp/changes.patch
OLLAMA_API_KEY=… npx pr-review --dry-run --diff /tmp/changes.patch
```

Ревью настоящего PR:

```bash
GITHUB_TOKEN=… GITHUB_REPOSITORY=owner/repo PR_NUMBER=123 OLLAMA_API_KEY=… \
  REVIEW_PROFILE=ops/ai-review/profile.json npx pr-review
```

## CLI

| Команда | Назначение |
|---------|------------|
| `pr-review` | Полный прогон (PR или `--diff`) |
| `pr-review-eval` | Golden fixtures без LLM — гейт post-processing |
| `pr-review-gold` | Shadow-прогон с LLM на размеченных TP/FP диффах |
| `pr-review-metrics` | Сводка по observability-логу прогонов |
| `pr-review-probe-models` | Доступность, latency и JSON-mode моделей |
| `pr-review-feedback` | Сбор 👍/👎 с одного PR |
| `pr-review-feedback-batch` | Batch-сбор по последним PR |

## Что внутри

- **Mixture-of-Agents**: три специалиста (security / correctness / architecture) → жюри-агрегатор
  с обязательной refutation каждой претензии → verifier Pass B по кандидатам в блокеры.
- **Prescan**: eslint и ast-grep по изменённым файлам — детерминированные находки идут и в гейт,
  и в промпт как «уже сказано, не повторяй».
- **Blast-radius**: кто зависит от изменённых символов вне диффа (graphify, fail-open).
- **Сходимость**: sticky несёт машинный футер с находками, отпечатками кода и историей
  прогонов — следующий прогон показывает Δ и не переоткрывает закрытое.
- **Маршрут по доказуемости**: где показать находку, решает сумма детерминированных сигналов,
  а не ярлык severity от модели.
- **Петля обучения**: 👎 или метка → журнал → промоут в FP-паттерн и eval fixture.

Зависимостей нет: только стандартная библиотека Node.

## Документация

| Раздел | Файл |
|--------|------|
| Оглавление | [docs/README.md](docs/README.md) |
| Архитектура | [docs/architecture.md](docs/architecture.md) |
| Профиль проекта | [docs/profiles.md](docs/profiles.md) |
| CLI и env | [docs/configuration.md](docs/configuration.md) |
| Интеграция в CI | [docs/integration.md](docs/integration.md) |
| Ложные срабатывания | [docs/false-positives.md](docs/false-positives.md) |
| Модели и параметры | [docs/model-tuning.md](docs/model-tuning.md) |

## Подключение к репозиторию

Движок берётся по пину и никогда не исполняется из кода PR:

```
ops/ai-reviewer-ref          # v0.2.0 или commit SHA
ops/ai-review/profile.json   # профиль проекта
```

Шаблоны workflow — [`examples/ci/`](examples/ci/), пошагово — [docs/integration.md](docs/integration.md).

## Разработка

```bash
npm test        # юнит + контракты (в т.ч. шаблоны workflow), node:test
npm run eval    # golden fixtures без LLM
npm run check   # то и другое
```

Регрессионный набор гоняется под reference-профилем [`profiles/orakul.json`](profiles/orakul.json) —
тем же, на котором движок работал внутри первого потребителя до выноса в отдельный пакет.
Поведение дефолтного (нейтрального) профиля покрыто отдельно в `test/profile.test.mjs`.

## Лицензия

Private / UNLICENSED — внутренний инструмент Orange-hanter / Mozarella.
