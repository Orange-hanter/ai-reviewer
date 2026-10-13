# Миграция Orakul на пин ai-reviewer

1. Скопировать в Orakul:
   - `profile.json` + `repo-rules.md` → `ops/ai-review/`
   - `ai-reviewer-ref` → `ops/ai-reviewer-ref` (одна строка `v0.5.1`)
2. В `.github/workflows/pr-review.yml` заменить шаг `Run AI review` (git archive
   `ops/pr-review`) на шаги из `examples/ci/pr-review.yml`: Read pin → Checkout
   `Orange-hanter/ai-reviewer` → `node .ai-reviewer-engine/bin/pr-review.mjs` с
   `REVIEW_PROFILE=ops/ai-review/profile.json`.
3. Feedback / fixtures / ast-grep остаются в `ops/pr-review/` (пути в профиле).
4. Исполняемый вендорный entry (`ops/pr-review.mjs` в гейте) больше не используется.
5. После пина: оркестратор / swarm читает артефакт `*-swarm.json`
   (см. [docs/swarm.md](../../../docs/swarm.md)).
