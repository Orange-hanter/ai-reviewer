<!-- orakul-ai-review -->
## 🤖 Авто код-ревью — 🔴 нужны правки

> ⛔ **Merge заблокирован:** 1 замечание. Правило блокировки: critical — от любой модели; major — по консенсусу ≥2 модел.; confidence ≥0.8 для блока. Исправьте (или ослабьте `REVIEW_BLOCK_ON`/`REVIEW_BLOCK_SINGLE`/`REVIEW_CONSENSUS`). Инфра-сбои merge не блокируют.

### Оценки моделей
- **m1** — ⚠️ недоступна (HTTP 500)
- **m2** — ⚠️ недоступна (HTTP 429 (rate limit))

**Итого:** 1 замечаний — 🔴1 🟠0 🟡0 🔵0. Файлов в диффе: 1.

> 🔧 Prescan (R6 eslint + ast-grep): eslint 1 + ast-grep 0 детерминированных finding(s) на 1 файлах (LLM dedup).

<details><summary>📡 Blast-radius (Graphify) · 0 блоков · symbols 0/0 · graph fresh</summary>

_Пусто: нет экспортов в диффе, символы не разрешились в графе, или code graph не собран. Смотри artifact `pr-review-graphify` и `blast_radius.skip_reasons` в observability._

</details>

<sub>¹ Модели: m1, m2 · commit 0123456 · блокирует merge: critical — от любой модели; major — по консенсусу ≥2 модел.; confidence ≥0.8 для блока; инфра-сбои merge не блокируют.</sub>
<!-- orakul-ai-review-data:{"sha":"0123456","findings":[{"file":"app/server/api.ts","line":3,"severity":"critical","title":"no-restricted-globals","model":"eslint","fp":"1af61ac0485b"}],"runs":[{"sha":"0123456","counts":{"critical":1,"major":0,"minor":0,"nit":0},"added":1,"gone":0,"blocked":true}]} -->