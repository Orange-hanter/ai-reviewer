# Golden-фикстуры пайплайна

Эталон поведения `pipeline.mjs`, снятый **до** его удаления (RFC 16, решение 7). Сверяет их
`rust/tests/pipeline_golden.rs` — без Node в рантайме.

```bash
node rust/parity/pipeline-golden-generate.mjs        # перегенерировать golden-fixtures/
cargo test --manifest-path rust/Cargo.toml --test pipeline_golden
```

Кейс = каталог `golden-fixtures/<id>/`; реестр — `golden-fixtures/index.json`.

| Файл | Что внутри |
|------|-----------|
| `diff.patch` | дифф PR (его отдаёт фейковый `github.loadDiff`) |
| `env.json` | `env` для `loadReviewConfig`, разобранный `cli`, замороженные часы `fixedNow`, флаг `forceJs` |
| `inputs.json` | ответы фейков: `loadDiff`, `priorSticky`, `prescan`, `blastRadius`, `reviews`, `verifier`, `githubResults`, `telegram`, `repoFiles` (дерево репозитория для FP-семейства 7 из `fixtures/<id>/repo-files.txt`; `null` — семейство выключено) |
| `expected/exit-code.txt` | код возврата `run()` |
| `expected/sticky.md` | тело последнего `upsertSticky` (байт в байт, с машинным футером) |
| `expected/calls.json` | порядок (`post`/`delete`/`sticky`/`telegram`) и ключевые аргументы вызовов — без тел |
| `expected/export.json`, `sarif.json`, `swarm.json` | артефакты `writeReviewExports`, скопированные байт в байт |

Детерминизм держится на: замороженных часах (`Date.now`/`new Date` → `fixedNow`; иначе плывёт
`generatedAt` в экспорте), выключенной observability (её `runId` — единственный источник
случайности в пайплайне), `AI_REVIEWER_*_FORCE_JS=1` (эталон — JS-ветка, не Rust-мост) и cwd =
корень репозитория. `REVIEW_EXPORT_DIR` в `env.json` заменён плейсхолдером `${EXPORT_DIR}`:
реальный каталог экспорта временный.
