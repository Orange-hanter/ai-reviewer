<!-- orakul-ai-review -->
## 🤖 Авто код-ревью — 🔴 нужны правки

> ⛔ **Merge заблокирован:** 1 замечание. Правило блокировки: critical — от любой модели; major — по консенсусу ≥2 модел.; confidence ≥0.8 для блока. Исправьте (или ослабьте `REVIEW_BLOCK_ON`/`REVIEW_BLOCK_SINGLE`/`REVIEW_CONSENSUS`). Инфра-сбои merge не блокируют.

### Walkthrough

ревью m1

### Оценки моделей
- **m1** — 2 (🔴2 🟠0 🟡0 🔵0): ревью m1
- **m2** — 2 (🔴0 🟠1 🟡1 🔵0): ревью m2

**Итого:** 2 замечаний — 🔴1 🟠0 🟡1 🔵0. Файлов в диффе: 1.

### Замечания вне строк диффа
- 🟡 **Нет обработки отсутствующего шаблона**¹ (`app/server/routes/orders.ts`:24) — Если `./templates/index.ts` не существует в окружении, loadTemplate бросит ENOENT без обработки.

<details><summary>🗑 Отфильтровано как известные FP (2)</summary>

- 🔴 **Импорт несуществующего модуля pricing** _(m1)_ (`app/server/routes/orders.ts:4`)
- 🟠 **templates не найден** _(m2)_ (`app/server/routes/orders.ts:6`)

> _Скрыто FP-паттернами из feedback (`ops/pr-review/feedback/fp-patterns.jsonl`) или сверкой с деревом репозитория («файла нет» о файле вне PR; путь — в логе прогона). Если среди них есть верное замечание — это ошибка фильтра, поправьте его._

</details>

<details><summary>📡 Blast-radius (Graphify) · 0 блоков · symbols 0/0 · graph fresh</summary>

_Пусто: нет экспортов в диффе, символы не разрешились в графе, или code graph не собран. Смотри artifact `pr-review-graphify` и `blast_radius.skip_reasons` в observability._

</details>

<details><summary>🔗 Model reasoning (consensus)</summary>

#### m1

ревью m1

#### m2

ревью m2

</details>

<sub>¹ Модели: m1, m2 · commit 0123456 · блокирует merge: critical — от любой модели; major — по консенсусу ≥2 модел.; confidence ≥0.8 для блока; инфра-сбои merge не блокируют.</sub>
<!-- orakul-ai-review-data:{"sha":"0123456","findings":[{"file":"app/server/routes/orders.ts","line":5,"severity":"critical","title":"Импорт audit-log без файла","model":"m1","fp":"5da014cc5fbe"},{"file":"app/server/routes/orders.ts","line":24,"severity":"minor","title":"Нет обработки отсутствующего шаблона","model":"m2","fp":"f560770017da"}],"runs":[{"sha":"0123456","counts":{"critical":1,"major":0,"minor":1,"nit":0},"added":2,"gone":0,"blocked":true}],"reasoning":[{"role":"model","model":"m1","excerpt":"ревью m1","truncated":false},{"role":"model","model":"m2","excerpt":"ревью m2","truncated":false}]} -->