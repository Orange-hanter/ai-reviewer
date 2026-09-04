<!-- orakul-ai-review -->
## 🤖 Авто код-ревью — 🔴 нужны правки

> ⛔ **Merge заблокирован:** 1 замечание. Правило блокировки: critical/major — от MoA-aggregator; confidence ≥0.8. Исправьте (или ослабьте `REVIEW_BLOCK_ON`/`REVIEW_BLOCK_SINGLE`/`REVIEW_CONSENSUS`). Инфра-сбои merge не блокируют.

### Walkthrough

ревью moa-specialist

### Оценки моделей
- **moa-specialist** — 1 (🔴1 🟠0 🟡0 🔵0): ревью moa-specialist

**Итого:** 1 замечаний — 🔴1 🟠0 🟡0 🔵0. Файлов в диффе: 1.

<details><summary>📡 Blast-radius (Graphify) · 0 блоков · symbols 0/0 · graph fresh</summary>

_Пусто: нет экспортов в диффе, символы не разрешились в графе, или code graph не собран. Смотри artifact `pr-review-graphify` и `blast_radius.skip_reasons` в observability._

</details>

<details><summary>🔗 Jury (advisor → aggregator)</summary>

#### Jury (advisor → aggregator)

- `server/api.ts:11` «IDOR на подписке»
- **security** verified: IDOR подтверждён
- **correctness** rejected: не про логику

</details>

<sub>¹ MoA specialists: security=deepseek-v4-flash, correctness=kimi-k2.7-code, architecture=glm-5.2 · aggregator: minimax-m3 · commit 0123456 · блокирует merge: critical/major — от MoA-aggregator; confidence ≥0.8; инфра-сбои merge не блокируют.</sub>
<!-- orakul-ai-review-data:{"sha":"0123456","findings":[{"file":"server/api.ts","line":11,"severity":"critical","title":"IDOR на подписке","model":"moa-specialist","fp":"34e3e29bd0ec"}],"runs":[{"sha":"0123456","counts":{"critical":1,"major":0,"minor":0,"nit":0},"added":1,"gone":0,"blocked":true}]} -->