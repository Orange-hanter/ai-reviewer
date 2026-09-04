<!-- orakul-ai-review -->
## 🤖 Авто код-ревью — 💬 есть замечания

### Оценки моделей
- **m1** — ⚠️ недоступна (HTTP 500)
- **m2** — 1 (🔴0 🟠0 🟡1 🔵0): 

**Итого:** 1 замечаний — 🔴0 🟠0 🟡1 🔵0. Файлов в диффе: 1.

### Замечания вне строк диффа
- 🟡 **магическое число**¹ (`app/server/api.ts`:4) — подробности находки

<details><summary>📡 Blast-radius (Graphify) · 0 блоков · symbols 0/0 · graph fresh</summary>

_Пусто: нет экспортов в диффе, символы не разрешились в графе, или code graph не собран. Смотри artifact `pr-review-graphify` и `blast_radius.skip_reasons` в observability._

</details>

<sub>¹ Модели: m1, m2 · commit 0123456 · блокирует merge: critical — от любой модели; major — по консенсусу ≥2 модел.; confidence ≥0.8 для блока; инфра-сбои merge не блокируют.</sub>
<!-- orakul-ai-review-data:{"sha":"0123456","findings":[{"file":"app/server/api.ts","line":4,"severity":"minor","title":"магическое число","model":"m2","fp":"4b4ac22414a5"}],"runs":[{"sha":"0123456","counts":{"critical":0,"major":0,"minor":1,"nit":0},"added":1,"gone":0,"blocked":false}]} -->