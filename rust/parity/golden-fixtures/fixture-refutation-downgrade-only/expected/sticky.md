<!-- orakul-ai-review -->
## 🤖 Авто код-ревью — 💬 есть замечания

### Walkthrough

ревью moa-specialist

### Оценки моделей
- **moa-specialist** — 1 (🔴0 🟠0 🟡1 🔵0): ревью moa-specialist

**Итого:** 1 замечаний — 🔴0 🟠0 🟡1 🔵0. Файлов в диффе: 1.

> ⚖️ Refutation (R5 jury): dropped 0, downgraded 1, kept 0.

### Замечания вне строк диффа
- 🟡 **возможный IDOR**¹ (`server/api.ts`:11) — стоит проверить auth

<details><summary>📡 Blast-radius (Graphify) · 0 блоков · symbols 0/0 · graph fresh</summary>

_Пусто: нет экспортов в диффе, символы не разрешились в графе, или code graph не собран. Смотри artifact `pr-review-graphify` и `blast_radius.skip_reasons` в observability._

</details>

<details><summary>🔗 Jury (advisor → aggregator)</summary>

#### Jury (advisor → aggregator)

- `server/api.ts:11` «возможный IDOR»
- **security** downgraded: auth на route-level
- **architecture** rejected: не arch issue

</details>

<sub>¹ MoA specialists: security=deepseek-v4-flash, correctness=kimi-k2.7-code, architecture=glm-5.2 · aggregator: minimax-m3 · commit 0123456 · блокирует merge: critical/major — от MoA-aggregator; confidence ≥0.8; инфра-сбои merge не блокируют.</sub>
<!-- orakul-ai-review-data:{"sha":"0123456","findings":[{"file":"server/api.ts","line":11,"severity":"minor","title":"возможный IDOR","model":"moa-specialist","fp":"34e3e29bd0ec"}],"runs":[{"sha":"0123456","counts":{"critical":0,"major":0,"minor":1,"nit":0},"added":1,"gone":0,"blocked":false}]} -->