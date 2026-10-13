<!-- orakul-ai-review -->
## 🤖 Авто код-ревью — 💬 есть замечания

> ✅ Блокеров нет: major есть лишь у одной модели — показаны как замечания (для блока нужен консенсус ≥1); critical не найдено.

### Walkthrough

ревью m1

### Оценки моделей
- **m1** — 1 (🔴0 🟠0 🟡1 🔵0): ревью m1

**Итого:** 1 замечаний — 🔴0 🟠1 🟡0 🔵0. Файлов в диффе: 1.

> 🔧 Prescan (R6 eslint + ast-grep): eslint 1 + ast-grep 0 детерминированных finding(s) на 0 файлах (LLM dedup).

<details><summary>📡 Blast-radius (Graphify) · 0 блоков · symbols 0/0 · graph fresh</summary>

_Пусто: нет экспортов в диффе, символы не разрешились в графе, или code graph не собран. Смотри artifact `pr-review-graphify` и `blast_radius.skip_reasons` в observability._

</details>

<details><summary>🔗 Model reasoning (consensus)</summary>

#### m1

ревью m1

</details>

<sub>¹ Модели: m1 · commit 0123456 · блокирует merge: critical — от любой модели; major — по консенсусу ≥1 модел.; confidence ≥0.8 для блока; инфра-сбои merge не блокируют.</sub>
<!-- orakul-ai-review-data:{"sha":"0123456","findings":[{"file":"server/orders.ts","line":10,"severity":"major","title":"eslint: no-unused-vars","model":"eslint-prescan","fp":"a5dbfb675b4e"}],"runs":[{"sha":"0123456","counts":{"critical":0,"major":1,"minor":0,"nit":0},"added":1,"gone":0,"blocked":false}],"reasoning":[{"role":"model","model":"m1","excerpt":"ревью m1","truncated":false}]} -->