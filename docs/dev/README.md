---
type: guide
status: active
owner: ai-reviewer
source_of_truth: true
created: 2026-09-10
last_updated: 2026-09-29
---

# Документация для агента и разработчика

Канон гейта — [architecture.md](../architecture.md). L0-навигатор — [MASTER.md](../../MASTER.md).
Этот каталог — лог разработки: что решили и что прогнали. SemVer-релизы остаются в
[CHANGELOG.md](../../CHANGELOG.md). RFC пишутся **до** кода в [proposals/](../../proposals/).

## Что куда писать

| Документ | Тип в cod-doc | Когда |
|----------|---------------|--------|
| [journal.md](journal.md) | `journal` | решение, живой прогон, тупик или handoff сессии |
| [log/YYYY-MM-DD-*.md](log/) | `execution-log` | закрытая сессия с приёмкой плана |
| [CHANGELOG.md](../../CHANGELOG.md) | `journal` | только то, что увидит потребитель в релизе |
| [proposals/*.md](../../proposals/) | `design` | до кода, если решение дороже отката |

Не дублировать RFC в журнале целиком: в журнале — исход, в RFC — контракт.

## Sync в cod-doc

Проект: `ai-reviewer` (БД `.cod-doc/state.db`, gitignored).

После правки канона:

```bash
cod-doc doc import <file.md> -p ai-reviewer --author <agent>
cod-doc hash update MASTER.md          # если файл в реестре L0
cod-doc doc import MASTER.md -p ai-reviewer --author <agent>
cod-doc doc drift -p ai-reviewer --all # edited_in_place == 0
```

Новый файл в дереве — `cod-doc import docs -p ai-reviewer` (с exclude fixtures/gold/examples).
Не регистрировать `profiles/*.md` и `examples/consumers/*/repo-rules.md`: это payload `repoRules`
в промпт, не документ. Вне cod-doc также README каталогов данных (`fixtures/`, `gold/`,
`gold-public/`, `examples/`), `.github/` и `.cursor/plans/`: они живут в репозитории, а
`docs/README.md` на них ссылается.
`doc export` не трогать: проекция на диск ещё не round-trip.
