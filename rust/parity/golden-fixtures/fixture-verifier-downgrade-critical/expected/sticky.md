<!-- orakul-ai-review -->
## 🤖 Авто код-ревью — 💬 есть замечания

### Walkthrough

ревью m1

### Оценки моделей
- **m1** — 1 (🔴0 🟠0 🟡1 🔵0): ревью m1

**Итого:** 1 замечаний — 🔴0 🟠0 🟡1 🔵0. Файлов в диффе: 1.

> 🔎 Verifier (Pass B, deepseek-v4-pro): подтверждено 0, отклонено FP 0, понижено 1.

### Замечания вне строк диффа
- 🟡 **возможный IDOR**¹ (`server/api.ts`:11) — нужна проверка прав

<details><summary>📡 Blast-radius (Graphify) · 0 блоков · symbols 0/0 · graph fresh</summary>

_Пусто: нет экспортов в диффе, символы не разрешились в графе, или code graph не собран. Смотри artifact `pr-review-graphify` и `blast_radius.skip_reasons` в observability._

</details>

<details><summary>🔗 Model reasoning (consensus)</summary>

#### m1

ревью m1

</details>

<sub>¹ Модели: m1 · commit 0123456 · блокирует merge: critical — от любой модели; major — по консенсусу ≥1 модел.; confidence ≥0.8 для блока; инфра-сбои merge не блокируют.</sub>
<!-- orakul-ai-review-data:{"sha":"0123456","findings":[{"file":"server/api.ts","line":11,"severity":"minor","title":"возможный IDOR","model":"m1","fp":"34e3e29bd0ec"}],"runs":[{"sha":"0123456","counts":{"critical":0,"major":0,"minor":1,"nit":0},"added":1,"gone":0,"blocked":false}],"reasoning":[{"role":"model","model":"m1","excerpt":"ревью m1","truncated":false}]} -->