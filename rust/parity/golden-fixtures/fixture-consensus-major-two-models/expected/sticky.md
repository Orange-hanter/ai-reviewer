<!-- orakul-ai-review -->
## 🤖 Авто код-ревью — 🔴 нужны правки

> ⛔ **Merge заблокирован:** 1 замечание. Правило блокировки: critical — от любой модели; major — по консенсусу ≥2 модел.; confidence ≥0.8 для блока. Исправьте (или ослабьте `REVIEW_BLOCK_ON`/`REVIEW_BLOCK_SINGLE`/`REVIEW_CONSENSUS`). Инфра-сбои merge не блокируют.

### Walkthrough

ревью m1

### Оценки моделей
- **m1** — 1 (🔴0 🟠1 🟡0 🔵0): ревью m1
- **m2** — 1 (🔴0 🟠1 🟡0 🔵0): ревью m2

**Итого:** 2 замечаний — 🔴0 🟠2 🟡0 🔵0. Файлов в диффе: 1.

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
<!-- orakul-ai-review-data:{"sha":"0123456","findings":[{"file":"shared/round.ts","line":10,"severity":"major","title":"не round2","model":"m1","fp":"e94358eae70e"},{"file":"shared/round.ts","line":11,"severity":"major","title":"не round2","model":"m2","fp":"d10b36aa74a5"}],"runs":[{"sha":"0123456","counts":{"critical":0,"major":2,"minor":0,"nit":0},"added":2,"gone":0,"blocked":true}],"reasoning":[{"role":"model","model":"m1","excerpt":"ревью m1","truncated":false},{"role":"model","model":"m2","excerpt":"ревью m2","truncated":false}]} -->