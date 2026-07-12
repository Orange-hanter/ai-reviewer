# @orakul/ai-reviewer

AI code review engine для GitHub PR: unified diff → Graphify blast-radius → LLM (Ollama MoA/consensus) → inline/sticky comments. Блокирует merge только на подтверждённых `critical`/`major` findings.

Первый потребитель — [Orakul](https://github.com/Orange-hanter/Orakul). Пакет переиспользуем в sibling-репозиториях Mozarella.

## Быстрый старт

```bash
git clone git@github.com:Orange-hanter/ai-reviewer.git
cd ai-reviewer
npm install
npm test && npm run eval
```

Offline-ревью diff без GitHub:

```bash
export OLLAMA_API_KEY=...
npx pr-review --dry-run --diff /path/to/changes.patch
```

## CLI

| Команда | Назначение |
|---------|------------|
| `pr-review` | Полный pipeline (PR или `--diff`) |
| `pr-review-eval` | Golden fixtures без LLM |
| `pr-review-feedback` | Harvest 👍/👎 с одного PR |
| `pr-review-feedback-batch` | Batch harvest по последним PR |

## Документация

| Раздел | Файл |
|--------|------|
| Оглавление | [docs/README.md](docs/README.md) |
| Архитектура | [docs/architecture.md](docs/architecture.md) |
| Env / CLI | [docs/configuration.md](docs/configuration.md) |
| Интеграция в CI | [docs/integration.md](docs/integration.md) |
| Журнал FP | [docs/ai-review-false-positives.md](docs/ai-review-false-positives.md) |
| Feedback loop | [feedback/README.md](feedback/README.md) |
| Eval fixtures | [fixtures/README.md](fixtures/README.md) |

## Интеграция (Orakul)

```bash
# pin в потребителе: ops/ai-reviewer-ref → v0.1.0
cd Orakul/ops && npm ci
node node_modules/@orakul/ai-reviewer/bin/pr-review.mjs --dry-run --diff ../tmp.patch
```

См. [docs/integration.md](docs/integration.md).

## Лицензия

Private / UNLICENSED — внутренний инструмент Orange-hanter / Mozarella.
