# Миграция Orakul на пин ai-reviewer

Движок подключается prebuilt-бинарём `pr-review` по пину с sha256
([RFC 16](../../../proposals/16-standalone-rust-binary.md), решение 9;
[docs/integration.md](../../../docs/integration.md)).

1. Скопировать в Orakul:
   - `profile.json` + `repo-rules.md` → `ops/ai-review/`
   - `../../ci/fetch-pr-review.sh` → `ops/ai-review/fetch-pr-review.sh`
   - `ai-reviewer-ref` → `ops/ai-reviewer-ref` — одна строка `<tag> sha256:<хэш SHA256SUMS>`.
     **Хэш в этом файле — заглушка из нулей до первого релиза с бинарями (T32)**: при
     релизе он заменяется настоящим (`curl -fsSL …/releases/download/<tag>/SHA256SUMS | sha256sum`).
     Скрипт сверки отвергает хэш из нулей (шаг красный, exit 1), а релиза по тегу до T32 нет
     (HTTP 404 — тоже exit 1) — копировать до релиза нельзя; до него
     остаётся пин `v0.4.0` и прежний шаблон (чекаут + `node`).
2. В `.github/workflows/` заменить шаблоны на `examples/ci/*.yml`: шаги `Read pinned engine
   ref` и `Checkout pinned AI review engine` уходят, их место — `Fetch pinned pr-review
   binary`; запуск — `"$PR_REVIEW_BIN" review` с `REVIEW_PROFILE=ops/ai-review/profile.json`.
   Переход одним PR: его ревью штатно пропустится (в базовой ветке ещё нет скрипта сверки),
   со следующего PR работает бинарь.
3. Feedback / fixtures / ast-grep остаются в `ops/pr-review/` (пути в профиле).
4. Исполняемый вендорный entry (`ops/pr-review.mjs` в гейте) больше не используется.
5. После пина: оркестратор / swarm читает артефакт `*-swarm.json`
   (см. [docs/swarm.md](../../../docs/swarm.md)).
