---
type: guide
status: active
owner: ai-reviewer
source_of_truth: true
last_updated: 2026-09-10
---

# Документация ai-reviewer

| Раздел | О чём |
|--------|-------|
| [architecture.md](architecture.md) | Как устроен прогон: поток данных, модули, ключевые решения |
| [../MASTER.md](../MASTER.md) | L0-навигатор для агента (реестр канона + хэши) |
| [dev/README.md](dev/README.md) | Куда писать журнал, execution-log, RFC; sync в cod-doc |
| [dev/journal.md](dev/journal.md) | Журнал разработки (решения и прогоны; релизы — CHANGELOG) |
| [profiles.md](profiles.md) | Профиль проекта — чем движок адаптируется к репозиторию |
| [configuration.md](configuration.md) | CLI и все env-переменные с дефолтами |
| [integration.md](integration.md) | Подключение к репозиторию-потребителю (пин + workflow) |
| [swarm.md](swarm.md) | E22/E23: внешний verdict + blast scope для PR-swarm |
| [benchmark.md](benchmark.md) | Precision/recall gold-бенчмарк |
| [false-positives.md](false-positives.md) | Семейства ложных срабатываний, защиты, петля обучения |
| [model-tuning.md](model-tuning.md) | Роли моделей, температуры, размер чанка, reasoning effort |
| [review-quality.md](review-quality.md) | Разбор низкого сигнала ревью: замер, причины по коду, план |

Рядом с кодом:

| Файл | О чём |
|------|-------|
| [../feedback/README.md](../feedback/README.md) | Формат журнала 👍/👎, promote паттернов |
| [../fixtures/README.md](../fixtures/README.md) | Golden fixtures для eval без LLM |
| [../gold/README.md](../gold/README.md) | Размеченные диффы для shadow-прогона с LLM |
| [../examples/ci/](../examples/ci/) | Шаблоны workflow для потребителя |
| [../profiles/orakul.json](../profiles/orakul.json) | Reference-профиль (он же — база регрессионных тестов) |
