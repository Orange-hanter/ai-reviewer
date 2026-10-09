# Пиктограммы ai-reviewer

Заменяют платформенные эмодзи на собственные знаки в стиле hero (cod-doc) — в README и,
по флагу, в PR-комментариях бота.

**PR-комментарии.** По умолчанию движок печатает эмодзи: текст работает в письмах и логах
без внешних URL. С `REVIEW_ICON_BASE_URL` sticky, шапка review-сводки и inline получают эти SVG (`lib/icons.mjs`).
Репозиторий движка приватный, а GitHub грузит картинки комментариев через прокси без
авторизации, поэтому копия набора опубликована в публичном
[Orange-hanter/ai-reviewer-icons](https://github.com/Orange-hanter/ai-reviewer-icons) и
подключается с пином на тег:
`https://raw.githubusercontent.com/Orange-hanter/ai-reviewer-icons/v1.0.0/icons`.
Изменил знак здесь — опубликуй новый тег там и подними пин: старые комментарии ссылаются на
прежний URL, и перерисовка не должна менять их смысл.

## Набор

| Файл | Вместо | Значение |
|---|---|---|
| `bot.svg` | 🤖 | Заголовок sticky: авто код-ревью |
| `gate-blocked.svg` | ⛔ | Merge заблокирован |
| `gate-open.svg` | ✅ | Merge не блокируется |
| `sev-critical.svg` | 🔴 | severity `critical` |
| `sev-major.svg` | 🟠 | severity `major` |
| `sev-minor.svg` | 🟡 | severity `minor` |
| `sev-nit.svg` | 🔵 | severity `nit` |
| `delta.svg` | 🔄 | Δ-блок: разница с прошлым прогоном |
| `verifier.svg` | 🔎 | Verifier (Pass B) |
| `jury.svg` | ⚖️ | Жюри: refutation |
| `prescan.svg` | 🔧 | Детерминированный prescan (eslint + ast-grep) |
| `fp-up.svg` | 👍 | Harvest: находка полезна |
| `fp-down.svg` | 👎 | Harvest: ложное срабатывание → FP-паттерн |

## Правила

- Сетка 24×24, `viewBox="0 0 24 24"`, обводка 1.9–2.0, скруглённые концы и стыки.
- Severity различаются **формой**, а не только цветом: восьмиугольник → треугольник →
  квадрат → кольцо. Это читается в ч/б и при дальтонизме.
- Палитра выбрана так, чтобы знак был виден и на светлой, и на тёмной теме GitHub:
  `#4493f8` (функциональные), `#e5534b` critical, `#d1782a` major, `#c9a227` minor,
  `#2ea043` ok.
- В Markdown вставляются как `<img>` с `height` 13–21 и с `alt`, равным исходному эмодзи:
  там, где картинка не грузится (npm, plain-text), остаётся прежний смысл.

```markdown
<img src=".github/icons/gate-blocked.svg" alt="⛔" height="16"> **Merge заблокирован:** …
```

- Цвет зашит в файл: `currentColor` не используется, потому что GitHub отдаёт картинку
  через прокси и наследование цвета темы не работает.
