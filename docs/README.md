# Документация ai-reviewer

| Раздел | О чём |
|--------|-------|
| [architecture.md](architecture.md) | Как устроен прогон: поток данных, модули, ключевые решения |
| [profiles.md](profiles.md) | Профиль проекта — чем движок адаптируется к репозиторию |
| [configuration.md](configuration.md) | CLI и все env-переменные с дефолтами |
| [integration.md](integration.md) | Подключение к репозиторию-потребителю (пин + workflow) |
| [swarm.md](swarm.md) | E22/E23: внешний verdict + blast scope для PR-swarm |
| [benchmark.md](benchmark.md) | Precision/recall gold-бенчмарк |
| [false-positives.md](false-positives.md) | Семейства ложных срабатываний, защиты, петля обучения |
| [model-tuning.md](model-tuning.md) | Роли моделей, температуры, размер чанка, reasoning effort |
| [review-quality.md](review-quality.md) | Разбор низкого сигнала ревью: замер, причины по коду, план |
| [scent-measurement.md](scent-measurement.md) | Замер слоя косвенных признаков без LLM: несут ли поверхностные признаки адрес дефекта ([RFC 08](../proposals/08-cue-driven-briefs.md)) |

Рядом с кодом:

| Файл | О чём |
|------|-------|
| [../feedback/README.md](../feedback/README.md) | Формат журнала 👍/👎, promote паттернов |
| [../fixtures/README.md](../fixtures/README.md) | Golden fixtures для eval без LLM |
| [../gold/README.md](../gold/README.md) | Размеченные диффы для shadow-прогона с LLM |
| [../examples/ci/](../examples/ci/) | Шаблоны workflow для потребителя |
| [../profiles/orakul.json](../profiles/orakul.json) | Reference-профиль (он же — база регрессионных тестов) |
