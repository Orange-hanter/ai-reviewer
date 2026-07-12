# Интеграция в репозиторий-потребитель

## npm-пакет

```json
{
  "dependencies": {
    "@orakul/ai-reviewer": "github:Orange-hanter/ai-reviewer#v0.1.0"
  }
}
```

Локальная разработка в монорепо Mozarella:

```json
"@orakul/ai-reviewer": "file:../ai-reviewer"
```

Symlink для `file:../ai-reviewer` (Orakul):

```bash
./ops/link-ai-reviewer.sh   # Orakul/ai-reviewer → ../ai-reviewer
cd ops && npm ci
```

## Pin версии (CI anti-pwn)

Движок **не** берётся из кода PR — только из доверенного pin:

```
ops/ai-reviewer-ref   # одна строка: v0.1.0 или commit SHA
```

GitHub Actions (Orakul):

```yaml
- name: Read pin
  id: pin
  run: echo "ref=$(tr -d '[:space:]' < ops/ai-reviewer-ref)" >> "$GITHUB_OUTPUT"

- uses: actions/checkout@v6
  with:
    repository: Orange-hanter/ai-reviewer
    ref: ${{ steps.pin.outputs.ref }}
    path: ai-reviewer

- working-directory: ops
  run: npm ci

- run: node ops/node_modules/@orakul/ai-reviewer/bin/pr-review.mjs
  env:
    OLLAMA_API_KEY: ${{ secrets.OLLAMA_API_KEY }}
    GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
    PR_NUMBER: ${{ github.event.pull_request.number }}
    REVIEW_APP_DIR: app
    GRAPHIFY_BIN: .graphify/venv/bin/graphify
    GRAPH: .graphify/graphify-out/graph.json
```

## Thin wrappers (опционально)

Orakul сохраняет совместимость со старыми путями:

| Wrapper | Делегирует в |
|---------|--------------|
| `ops/pr-review.mjs` | `@orakul/ai-reviewer` |
| `ops/pr-review.eval.mjs` | eval harness |
| `ops/pr-review.feedback.mjs` | feedback harvest |

## Overlay feedback

Данные обучения (JSONL, patterns) могут жить в потребителе:

```bash
REVIEW_FEEDBACK_LOG=ops/ai-review/fp-feedback.jsonl
REVIEW_FP_PATTERNS=ops/ai-review/fp-patterns.jsonl
```

Движок и golden fixtures — в пакете; project-specific FP log — в Orakul.

## Локальный прогон (agent-verify слой 3)

```bash
export OLLAMA_API_KEY=...
./ops/agent-verify.sh --uncommitted
# или offline:
node ops/pr-review.mjs --dry-run --diff /tmp/changes.patch
```

## Требования потребителя

| Компонент | Нужен для |
|-----------|-----------|
| `app/` + `npm ci` | eslint prescan (R6) |
| Graphify graph | blast-radius (fail-open без графа) |
| GitHub secrets | CI review + feedback |
| `@ast-grep/cli` | ast-grep prescan (fail-open без бинарника) |

## Версионирование

- Semver теги на `ai-reviewer` (`v0.1.0`, …)
- Orakul bump: правка `ops/ai-reviewer-ref` + `npm ci` в PR
- Eval fixtures в пакете — регрессия при bump pin
