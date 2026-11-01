# Миграция Orakul на пин ai-reviewer

1. Скопировать в Orakul:
   - `profile.json` + `repo-rules.md` → `ops/ai-review/`
   - `ai-reviewer-ref` → `ops/ai-reviewer-ref` (одна строка `v0.7.0`)
2. В `.github/workflows/pr-review.yml` заменить шаг `Run AI review` (git archive
   `ops/pr-review`) на шаги из `examples/ci/pr-review.yml`: Read pin → Checkout
   `Orange-hanter/ai-reviewer` → Download engine binaries → `node
   .ai-reviewer-engine/bin/pr-review.mjs` с `REVIEW_PROFILE=ops/ai-review/profile.json`.
   Шаг бинарей (с 0.6) качает нативное ядро из релиза по тегу пина. Без него движок
   работает на deprecated JS-копиях, без нового каскада и промптов.
3. Feedback / fixtures / ast-grep остаются в `ops/pr-review/` (пути в профиле).
4. Исполняемый вендорный entry (`ops/pr-review.mjs` в гейте) больше не используется.
5. После пина: оркестратор / swarm читает артефакт `*-swarm.json`
   (см. [docs/swarm.md](../../../docs/swarm.md)).
6. Провайдер Orakul — OpenRouter (с 2026-10-15; замеры — [RFC 20](../../../proposals/20-rust-primary.md)
   § 4–7). Ростер в шаге `Run AI review` (secret `OPENROUTER_API_KEY`):

   ```yaml
   LLM_API_KEY: ${{ secrets.OPENROUTER_API_KEY }}
   LLM_BASE_URL: https://openrouter.ai/api/v1
   MOA_SPECIALIST_MODELS: security:z-ai/glm-5.3-flash,correctness:deepseek/deepseek-v4.1-flash,architecture:deepseek/deepseek-v4.1-flash
   MOA_AGGREGATOR: anthropic/claude-haiku-5.5
   REVIEW_VERIFIER_MODEL: 'minimax/minimax-m3.1-flash:free'
   REVIEW_VERIFIER_FALLBACK_MODEL: '~deepseek/deepseek-pro-latest'
   LLM_MODEL_SETTINGS: '{"deepseek/deepseek-v4.1-flash":{"effort":"none"},"z-ai/glm-5.3-flash":{"effort":"high","maxTokens":12000},"minimax/minimax-m3.1-flash:free":{"effort":"high"}}'
   ```

   deepseek без рассуждения (на `low` — пустой ответ), glm с обязательным рассуждением
   (`none` → 400), verifier на бесплатной модели с запасной: лимит `:free` не блокирует PR.
   Ветки правок в Orakul — `fix/…` или `feat/…`: CI Orakul (`ci.yml`) других префиксов не ловит.
