import { test } from 'node:test';
import assert from 'node:assert/strict';
import { iconize, withIcons, ICON_FILES } from '../lib/icons.mjs';

const BASE = 'https://icons.example/v1/icons';
const img = (name, alt, h = 16) => `<img src="${BASE}/${name}.svg" alt="${alt}" height="${h}">`;

test('iconize: пустая база — текст без изменений', () => {
  const md = '## 🤖 Авто код-ревью\n\n> ⛔ **Merge заблокирован**';
  assert.equal(iconize(md, ''), md);
  assert.equal(iconize(md, undefined), md);
});

test('iconize: заголовок крупнее строки, хвостовой слэш базы срезается', () => {
  const md = '## 🤖 Авто код-ревью — 🔴 нужны правки\n\n> ⛔ **Merge заблокирован:** 2.';
  assert.equal(
    iconize(md, `${BASE}/`),
    `## ${img('bot', '🤖', 20)} Авто код-ревью — ${img('sev-critical', '🔴', 20)} нужны правки\n\n`
      + `> ${img('gate-blocked', '⛔')} **Merge заблокирован:** 2.`,
  );
});

test('iconize: ⚖ с вариационным селектором и без — один знак', () => {
  assert.equal(iconize('⚖️ a ⚖ b', BASE), `${img('jury', '⚖️')} a ${img('jury', '⚖️')} b`);
});

test('iconize: HTML-комментарии (маркер и JSON-футер) не трогаются — футер парсится', () => {
  const footer = '<!-- ai-review-data:{"findings":[{"title":"🔴 IDOR ✅"}]} -->';
  const md = `<!-- ai-review -->\n## 🤖 Ревью\n\n${footer}`;
  const out = iconize(md, BASE);
  assert.ok(out.startsWith('<!-- ai-review -->\n'));
  assert.ok(out.endsWith(footer));
  assert.deepEqual(JSON.parse(footer.slice('<!-- ai-review-data:'.length, -' -->'.length)).findings[0].title, '🔴 IDOR ✅');
});

test('iconize: fenced-блок (suggestion) и инлайн-код буквальны', () => {
  const md = '🟡 **minor: x** в `a ✅ b`\n\n```suggestion\nlog("✅ ok")\n```\n🔵 после';
  assert.equal(
    iconize(md, BASE),
    `${img('sev-minor', '🟡')} **minor: x** в \`a ✅ b\`\n\n\`\`\`suggestion\nlog("✅ ok")\n\`\`\`\n${img('sev-nit', '🔵')} после`,
  );
});

test('iconize: эмодзи вне набора не меняются', () => {
  assert.equal(iconize('⚠️ 💬 ⚪ 📈', BASE), '⚠️ 💬 ⚪ 📈');
});

test('ICON_FILES: каждому знаку есть SVG в .github/icons', async () => {
  const { existsSync } = await import('node:fs');
  for (const name of Object.values(ICON_FILES)) {
    assert.ok(existsSync(new URL(`../.github/icons/${name}.svg`, import.meta.url)), name);
  }
});

test('withIcons: sticky и inline — с картинками, остальные методы и аргументы — как были', async () => {
  const calls = [];
  const gh = {
    upsertSticky: async (body) => calls.push(['sticky', body]),
    postReview: async (arg) => { calls.push(['review', arg]); return 7; },
    replyToThread: async (body) => calls.push(['reply', body]),
  };
  assert.equal(withIcons(gh, ''), gh);
  const wrapped = withIcons(gh, BASE);
  await wrapped.upsertSticky('🔄 delta');
  assert.equal(await wrapped.postReview({ headSha: 'abc', inline: [{ path: 'a.js', line: 3, body: '🟠 **major: y**' }] }), 7);
  await wrapped.replyToThread('<sub>🤖 auto-triage</sub>');
  assert.deepEqual(calls, [
    ['sticky', `${img('delta', '🔄')} delta`],
    ['review', { headSha: 'abc', inline: [{ path: 'a.js', line: 3, body: `${img('sev-major', '🟠')} **major: y**` }] }],
    ['reply', '<sub>🤖 auto-triage</sub>'],
  ]);
});
