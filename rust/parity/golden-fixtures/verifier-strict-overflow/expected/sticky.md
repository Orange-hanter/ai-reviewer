<!-- orakul-ai-review -->
## 🤖 Авто код-ревью — 🔴 нужны правки

> ⛔ **Merge заблокирован:** 5 замечаний. Правило блокировки: critical — от любой модели; major — по консенсусу ≥2 модел.; confidence ≥0.8 для блока. Исправьте (или ослабьте `REVIEW_BLOCK_ON`/`REVIEW_BLOCK_SINGLE`/`REVIEW_CONSENSUS`).

### Walkthrough

s

### Оценки моделей
- **m1** — 13 (🔴13 🟠0 🟡0 🔵0): s

**Итого:** 13 замечаний — 🔴13 🟠0 🟡0 🔵0. Файлов в диффе: 1.

> 🔎 Verifier (Pass B, deepseek-v4-pro): подтверждено 13, отклонено FP 0, понижено 0.

### Замечания вне строк диффа
- 🔴 **баг 7**¹ (`app/server/api.ts`:7) — подробности находки
- 🔴 **баг 8**¹ (`app/server/api.ts`:8) — подробности находки
- 🔴 **баг 9**¹ (`app/server/api.ts`:9) — подробности находки
- 🔴 **баг 10**¹ (`app/server/api.ts`:10) — подробности находки
- 🔴 **баг 11**¹ (`app/server/api.ts`:11) — подробности находки
- 🔴 **баг 12**¹ (`app/server/api.ts`:12) — подробности находки
- 🔴 **баг 13**¹ (`app/server/api.ts`:13) — подробности находки

<details><summary>📡 Blast-radius (Graphify) · 0 блоков · symbols 0/0 · graph fresh</summary>

_Пусто: нет экспортов в диффе, символы не разрешились в графе, или code graph не собран. Смотри artifact `pr-review-graphify` и `blast_radius.skip_reasons` в observability._

</details>

<details><summary>🔗 Model reasoning (consensus)</summary>

#### m1

s

</details>

<sub>¹ Модели: m1, m2 · commit 0123456 · блокирует merge: critical — от любой модели; major — по консенсусу ≥2 модел.; confidence ≥0.8 для блока; инфра-сбои merge не блокируют.</sub>
<!-- orakul-ai-review-data:{"sha":"0123456","findings":[{"file":"app/server/api.ts","line":1,"severity":"critical","title":"баг 1","model":"m1","fp":"40588068b21f"},{"file":"app/server/api.ts","line":2,"severity":"critical","title":"баг 2","model":"m1","fp":"527ce65aacda"},{"file":"app/server/api.ts","line":3,"severity":"critical","title":"баг 3","model":"m1","fp":"1af61ac0485b"},{"file":"app/server/api.ts","line":4,"severity":"critical","title":"баг 4","model":"m1","fp":"4b4ac22414a5"},{"file":"app/server/api.ts","line":5,"severity":"critical","title":"баг 5","model":"m1","fp":"6e022b4c49ef"},{"file":"app/server/api.ts","line":6,"severity":"critical","title":"баг 6","model":"m1"},{"file":"app/server/api.ts","line":7,"severity":"critical","title":"баг 7","model":"m1"},{"file":"app/server/api.ts","line":8,"severity":"critical","title":"баг 8","model":"m1"},{"file":"app/server/api.ts","line":9,"severity":"critical","title":"баг 9","model":"m1"},{"file":"app/server/api.ts","line":10,"severity":"critical","title":"баг 10","model":"m1"},{"file":"app/server/api.ts","line":11,"severity":"critical","title":"баг 11","model":"m1"},{"file":"app/server/api.ts","line":12,"severity":"critical","title":"баг 12","model":"m1"},{"file":"app/server/api.ts","line":13,"severity":"critical","title":"баг 13","model":"m1"}],"runs":[{"sha":"0123456","counts":{"critical":13,"major":0,"minor":0,"nit":0},"added":13,"gone":0,"blocked":true}],"reasoning":[{"role":"model","model":"m1","excerpt":"s","truncated":false}]} -->