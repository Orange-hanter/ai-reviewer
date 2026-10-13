<!-- orakul-ai-review -->
## 🤖 Авто код-ревью — 🔴 нужны правки

> ⛔ **Merge заблокирован:** 1 замечание. Правило блокировки: critical — от любой модели; major — по консенсусу ≥1 модел.; confidence ≥0.8 для блока. Исправьте (или ослабьте `REVIEW_BLOCK_ON`/`REVIEW_BLOCK_SINGLE`/`REVIEW_CONSENSUS`).

### Walkthrough

ревью m1

### Оценки моделей
- **m1** — 2 (🔴1 🟠1 🟡0 🔵0): ревью m1

**Итого:** 2 замечаний — 🔴1 🟠1 🟡0 🔵0. Файлов в диффе: 1.

<details><summary>📡 Blast-radius (Graphify) · 0 блоков · symbols 0/0 · graph fresh</summary>

_Пусто: нет экспортов в диффе, символы не разрешились в графе, или code graph не собран. Смотри artifact `pr-review-graphify` и `blast_radius.skip_reasons` в observability._

</details>

<details><summary>🔗 Model reasoning (consensus)</summary>

#### m1

ревью m1

</details>

<sub>¹ Модели: m1 · commit 0123456 · блокирует merge: critical — от любой модели; major — по консенсусу ≥1 модел.; confidence ≥0.8 для блока; инфра-сбои merge не блокируют.</sub>
<!-- orakul-ai-review-data:{"sha":"0123456","findings":[{"file":"server/orders.ts","line":10,"severity":"critical","title":"возможная утечка соединения","model":"m1","fp":"a5dbfb675b4e"},{"file":"server/orders.ts","line":11,"severity":"major","title":"SQL-инъекция через конкатенацию id","model":"m1","fp":"0a7a07677bf3"}],"runs":[{"sha":"0123456","counts":{"critical":1,"major":1,"minor":0,"nit":0},"added":2,"gone":0,"blocked":true}],"reasoning":[{"role":"model","model":"m1","excerpt":"ревью m1","truncated":false}]} -->