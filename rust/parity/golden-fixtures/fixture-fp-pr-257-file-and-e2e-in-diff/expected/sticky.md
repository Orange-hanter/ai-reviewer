<!-- orakul-ai-review -->
## 🤖 Авто код-ревью — ✅ можно мёржить

### Walkthrough

ревью m1

### Оценки моделей
- **m1** — 2 (🔴0 🟠2 🟡0 🔵0): ревью m1
- **m2** — 2 (🔴0 🟠2 🟡0 🔵0): ревью m2

**Итого:** 0 замечаний — 🔴0 🟠0 🟡0 🔵0. Файлов в диффе: 2.

<details><summary>🗑 Отфильтровано как известные FP (4)</summary>

- 🟠 **push-sw.js отсутствует в PR** _(m1)_ (`app/client/public/push-sw.js:1`)
- 🟠 **нет e2e на web push** _(m1)_ (`app/server/notifications.ts:10`)
- 🟠 **push-sw.js отсутствует в PR** _(m2)_ (`app/client/public/push-sw.js:2`)
- 🟠 **нет e2e на web push** _(m2)_ (`app/server/notifications.ts:11`)

> _Скрыто FP-паттернами из feedback (`ops/pr-review/feedback/fp-patterns.jsonl`). Если среди них есть верное замечание — это ошибка паттерна, поправьте его._

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
<!-- orakul-ai-review-data:{"sha":"0123456","findings":[],"runs":[{"sha":"0123456","counts":{"critical":0,"major":0,"minor":0,"nit":0},"added":0,"gone":0,"blocked":false}],"reasoning":[{"role":"model","model":"m1","excerpt":"ревью m1","truncated":false},{"role":"model","model":"m2","excerpt":"ревью m2","truncated":false}]} -->