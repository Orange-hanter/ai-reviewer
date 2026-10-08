---
description: Проверить, не известный ли это ложноположительный паттерн.
type: guide
status: active
owner: ai-reviewer
---

Вызови `fp_check` для находки (title, body, file). Паттерны — пакетные, regex в аргументах не передавай.

Если `match: true` — не «чини» код под эту формулировку: это известный FP. Объясни `patternId`.
Пустой `file` недопустим (`requiresPath` иначе не сработает).
