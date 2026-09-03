#!/usr/bin/env node
/**
 * Добыча кандидатов публичного gold-набора (RFC 03, backlog B6).
 *
 * Метод — SZZ-lite: ищем смерженный PR-ФИКС, который сам ссылается на PR-ВИНОВНИКА
 * («regression introduced in #N»). Виновник даёт дифф кейса, фикс — разметку.
 *
 * Локация дефекта выводится СОПОСТАВЛЕНИЕМ ТЕКСТА, а не номеров строк: строка, которую
 * удалил фикс, ищется среди строк, которые добавил виновник. Тогда номер берётся из диффа
 * самого кейса, и дрейф файла между двумя PR разметку не смещает. Нет пересечения по
 * тексту → дефект не локализуется в диффе виновника, и кандидат отбраковывается.
 *
 * Запуск:
 *   node bin/pr-review-gold-harvest.mjs grafana/grafana getsentry/sentry
 *   node bin/pr-review-gold-harvest.mjs grafana/grafana --emit gold-public
 *
 * Скрипт НЕ решает, что кейс годен: он готовит кандидатов, отбор — ручной шаг поверх.
 */
import { mkdir, writeFile, readdir, readFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { githubJson, ensureCaseDiff, readCaseMeta } from '../lib/gold-fetch.mjs';

const PHRASES = [
  'regression introduced in',
  'bug introduced in',
  'introduced in PR',
  'broken by PR',
  'regressed in',
  'caused by PR',
];

const PATTERNS = [
  /introduced\s+in\s+https:\/\/github\.com\/[^/\s]+\/[^/\s]+\/pull\/(\d+)/i,
  /(?:regression|bug|issue)?\s*introduced\s+(?:in|by)\s+(?:PR\s*)?#(\d+)/i,
  /(?:broken|regressed|caused)\s+by\s+https:\/\/github\.com\/[^/\s]+\/[^/\s]+\/pull\/(\d+)/i,
  /(?:broken|regressed|caused)\s+by\s+(?:PR\s*)?#(\d+)/i,
];

// Пороги «кейс влезает в прогон и дефект локализован» — см. «Оценка» в RFC 03.
const LIMITS = {
  buggyLines: 800,
  buggyFiles: 15,
  fixLines: 150,
};
/** Строки короче этого не несут смысла (`}`, `);`, пустые) и дают ложные совпадения по тексту. */
const MIN_SIGNIFICANT_LINE = 12;
/** Импорты фикс часто просто переставляет — совпадение по ним указывает не на дефект. */
const IMPORT_LINE = /^\s*(?:import\b|from\s|#include\b|use\s|require\s*\()/;
/** Локация в тесте — не дефект: ревью обязано найти причину, а не проверку на неё. */
const TEST_PATH = /(?:^|\/)(?:tests?|__tests__|spec)\/|[._-](?:test|spec)\.[a-z]+$|_test\.go$|Test\.java$/i;
/** Разрыв в номерах строк, при котором соседние попадания всё ещё считаются одним диапазоном. */
const RANGE_GAP = 2;
/** Clean-контроль берём только среди PR, у которых было время проявить дефект. */
const CLEAN_MIN_AGE_DAYS = 400;
/** Совсем маленький дифф ничего не говорит о precision. */
const CLEAN_MIN_LINES = 30;

/** Разбор unified-патча из ответа `pulls/N/files`: добавленные строки с номерами новой стороны. */
function addedLines(patch) {
  const out = [];
  let newLine = 0;
  for (const raw of String(patch || '').split('\n')) {
    const hunk = /^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
    if (hunk) {
      newLine = Number(hunk[1]);
      continue;
    }
    if (raw.startsWith('+') && !raw.startsWith('+++')) {
      out.push({ line: newLine, text: raw.slice(1) });
      newLine++;
    } else if (raw.startsWith('-') && !raw.startsWith('---')) {
      // строка старой стороны — номер новой не двигает
    } else if (raw.startsWith('\\')) {
      // "\ No newline at end of file"
    } else {
      newLine++;
    }
  }
  return out;
}

/** Строки, которые патч удалил (старая сторона) — то, что фикс признал неверным. */
function removedTexts(patch) {
  const out = new Set();
  for (const raw of String(patch || '').split('\n')) {
    if (raw.startsWith('-') && !raw.startsWith('---')) {
      const t = raw.slice(1).trim();
      if (t.length >= MIN_SIGNIFICANT_LINE && !IMPORT_LINE.test(t)) out.add(t);
    }
  }
  return out;
}

function toRanges(lines) {
  const sorted = [...new Set(lines)].sort((a, b) => a - b);
  const ranges = [];
  for (const n of sorted) {
    const last = ranges[ranges.length - 1];
    if (last && n - last[1] <= RANGE_GAP) last[1] = n;
    else ranges.push([n, n]);
  }
  return ranges;
}

function slug(s, max = 40) {
  return String(s)
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, max)
    .replace(/-+$/, '');
}

/** Backport-PR того же фикса («[release-12.0.1] …») — не независимый кейс. */
function isBackport(title) {
  return /^\s*\[(?:release-|v?\d)/i.test(String(title || ''));
}

async function pullFiles(repo, number) {
  return githubJson(`/repos/${repo}/pulls/${number}/files?per_page=100`);
}

async function harvestRepo(repo) {
  const seen = new Set();
  const byBuggy = new Map();
  let scanned = 0;

  for (const phrase of PHRASES) {
    const q = encodeURIComponent(`repo:${repo} is:pr is:merged "${phrase}"`);
    let res;
    try {
      res = await githubJson(`/search/issues?q=${q}&per_page=40`);
    } catch (e) {
      console.error(`  [${repo}] поиск «${phrase}»: ${e.message}`);
      continue;
    }
    for (const it of res.items || []) {
      if (seen.has(it.number)) continue;
      seen.add(it.number);
      scanned++;

      const hay = `${it.title}\n${it.body || ''}`;
      let buggyNum = null;
      for (const re of PATTERNS) {
        const m = re.exec(hay);
        if (m) {
          buggyNum = Number(m[1]);
          break;
        }
      }
      if (!buggyNum || buggyNum === it.number) continue;
      // Один дефект, разложенный по бэкпортам, не должен давать несколько кейсов.
      if (byBuggy.has(buggyNum) && isBackport(it.title)) continue;

      let fix;
      let buggy;
      try {
        fix = await githubJson(`/repos/${repo}/pulls/${it.number}`);
        buggy = await githubJson(`/repos/${repo}/pulls/${buggyNum}`);
      } catch {
        continue;
      }
      if (!buggy.merged_at || !buggy.merge_commit_sha) continue;
      const buggyLines = buggy.additions + buggy.deletions;
      if (buggyLines > LIMITS.buggyLines || buggy.changed_files > LIMITS.buggyFiles) continue;
      if (fix.additions + fix.deletions > LIMITS.fixLines) continue;

      let fixFiles;
      let buggyFiles;
      try {
        fixFiles = await pullFiles(repo, it.number);
        buggyFiles = await pullFiles(repo, buggyNum);
      } catch {
        continue;
      }

      const buggyByName = new Map(buggyFiles.map((f) => [f.filename, f]));
      const locations = [];
      for (const ff of fixFiles) {
        if (TEST_PATH.test(ff.filename)) continue;
        const bf = buggyByName.get(ff.filename);
        if (!bf?.patch) continue;
        const removed = removedTexts(ff.patch);
        if (!removed.size) continue;
        const hits = addedLines(bf.patch)
          .filter((l) => removed.has(l.text.trim()))
          .map((l) => l.line);
        for (const [from, to] of toRanges(hits)) {
          locations.push({ file: ff.filename, lines: [from, to] });
        }
      }
      if (!locations.length) continue;

      const prev = byBuggy.get(buggyNum);
      // Из нескольких фиксов на одного виновника берём тот, что даёт больше локаций.
      if (prev && prev.expect.mustFindLocations.length >= locations.length) continue;

      byBuggy.set(buggyNum, {
        id: `${slug(repo.split('/')[1], 16)}-${buggyNum}-${slug(buggy.title, 32)}`,
        type: 'tp',
        description: `${repo}#${buggyNum} «${buggy.title}» — дефект, который проект признал фиксом #${it.number}`,
        source: {
          repo,
          pr: buggyNum,
          sha: buggy.merge_commit_sha,
          fixPr: it.number,
          evidence: [
            `фикс #${it.number} «${it.title}» ссылается на #${buggyNum} как на источник дефекта`,
            `дифф виновника: +${buggy.additions}/-${buggy.deletions} в ${buggy.changed_files} файл(ах)`,
            `дифф фикса: +${fix.additions}/-${fix.deletions}`,
            `локации выведены сопоставлением текста удалённых фиксом строк с добавленными в #${buggyNum}`,
          ],
        },
        expect: {
          wouldBlock: true,
          mustFindLocations: locations,
        },
      });
    }
  }
  return { scanned, candidates: [...byBuggy.values()] };
}

/**
 * Clean-контроль: смерженный PR, на который за прошедшие месяцы никто не сослался как на
 * источник дефекта. Это не «дифф без замечаний» — minor/nit разметкой не запрещены;
 * блокер здесь означает, что движок объявил critical/major там, где проект дефекта так и
 * не увидел. Метрика precision — ровно та же логика, что у кейсов `clean-code-*` в `gold/`.
 */
async function harvestClean(repo, want) {
  const cutoff = new Date(Date.now() - CLEAN_MIN_AGE_DAYS * 86400e3).toISOString().slice(0, 10);
  const q = encodeURIComponent(`repo:${repo} is:pr is:merged merged:<${cutoff}`);
  let res;
  try {
    res = await githubJson(`/search/issues?q=${q}&sort=comments&order=desc&per_page=50`);
  } catch (e) {
    console.error(`  [${repo}] поиск clean: ${e.message}`);
    return [];
  }
  const out = [];
  for (const it of res.items || []) {
    if (out.length >= want) break;
    let pr;
    try {
      pr = await githubJson(`/repos/${repo}/pulls/${it.number}`);
    } catch {
      continue;
    }
    if (!pr.merged_at || !pr.merge_commit_sha) continue;
    if (pr.additions + pr.deletions > LIMITS.buggyLines || pr.changed_files > LIMITS.buggyFiles) continue;
    if (pr.additions + pr.deletions < CLEAN_MIN_LINES) continue;

    // Ссылка на этот PR из любого другого PR — повод не считать его чистым.
    let refs;
    try {
      const rq = encodeURIComponent(`repo:${repo} is:pr "#${it.number}" in:body`);
      refs = await githubJson(`/search/issues?q=${rq}&per_page=20`);
    } catch {
      continue;
    }
    const referrers = (refs.items || []).filter((r) => r.number !== it.number);
    if (referrers.length) continue;

    out.push({
      id: `${slug(repo.split('/')[1], 16)}-clean-${it.number}-${slug(pr.title, 28)}`,
      type: 'clean',
      description: `${repo}#${it.number} «${pr.title}» — clean-контроль: смержен до ${cutoff}, ни один PR на него не ссылается`,
      source: {
        repo,
        pr: it.number,
        sha: pr.merge_commit_sha,
        evidence: [
          `смержен ${pr.merged_at?.slice(0, 10)}, отбор среди PR старше ${CLEAN_MIN_AGE_DAYS} дней`,
          `дифф: +${pr.additions}/-${pr.deletions} в ${pr.changed_files} файл(ах)`,
          `поиск «#${it.number} in:body» по PR репозитория не дал ни одной ссылки`,
        ],
      },
      expect: {
        wouldBlock: false,
        note: 'Отсутствие ссылок доказывает лишь отсутствие ЗАМЕЧЕННОГО дефекта. Блокер здесь считается ложным.',
      },
    });
  }
  return out;
}

/**
 * Сверка разметки с диффом: локации выводятся из ответа `pulls/N/files`, а в пайплайн уходит
 * дифф по пину `merge_commit_sha`. Если нумерация этих двух источников когда-нибудь разойдётся,
 * весь замер станет недействительным молча — поэтому сверка отдельным режимом.
 */
function addedLineNumbersByFile(patch) {
  const map = new Map();
  let file = null;
  let newLine = 0;
  for (const raw of patch.split('\n')) {
    const plus = /^\+\+\+ b\/(.+)$/.exec(raw);
    if (plus) {
      file = plus[1];
      map.set(file, new Set());
      continue;
    }
    const hunk = /^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
    if (hunk) {
      newLine = Number(hunk[1]);
      continue;
    }
    if (!file) continue;
    if (raw.startsWith('+') && !raw.startsWith('+++')) {
      map.get(file).add(newLine);
      newLine++;
    } else if (raw.startsWith('-') && !raw.startsWith('---')) {
      // старая сторона номер новой не двигает
    } else if (/^(?:\\|diff |index |--- |new file|deleted file|similarity |rename |old mode|new mode)/.test(raw)) {
      // служебные строки заголовка файла
    } else {
      newLine++;
    }
  }
  return map;
}

async function validateDataset(dir, cacheDir) {
  const ids = (await readdir(dir, { withFileTypes: true }))
    .filter((e) => e.isDirectory()).map((e) => e.name).sort();
  let bad = 0;
  for (const id of ids) {
    const caseDir = join(dir, id);
    const meta = await readCaseMeta(caseDir);
    const locations = meta.expect?.mustFindLocations || [];
    // Дифф тянем ДО проверки локаций: у clean-контролей их нет, но доставка по пину
    // обязана быть проверена и прогреть кэш — иначе кейс падает на сети посреди прогона,
    // который стоит десятки минут.
    let patch;
    try {
      const r = await ensureCaseDiff(caseDir, meta, { cacheDir });
      patch = await readFile(r.path, 'utf8');
    } catch (e) {
      console.log(`💥 ${id} — ${e.message}`);
      bad++;
      continue;
    }
    if (!locations.length) {
      console.log(`· ${id} — локаций нет (clean-контроль), дифф по пину доставлен`);
      continue;
    }
    const added = addedLineNumbersByFile(patch);
    const problems = [];
    for (const loc of locations) {
      const set = added.get(loc.file);
      if (!set) {
        problems.push(`файла нет в диффе: ${loc.file}`);
        continue;
      }
      let hit = false;
      for (let n = loc.lines[0]; n <= loc.lines[1] && !hit; n++) hit = set.has(n);
      if (!hit) problems.push(`${loc.file}:${loc.lines[0]}-${loc.lines[1]} — ни одна строка не добавлена этим диффом`);
    }
    if (problems.length) {
      bad++;
      console.log(`❌ ${id}`);
      for (const p of problems) console.log(`     ${p}`);
    } else {
      console.log(`✅ ${id} — ${locations.length} локац. подтверждены в диффе`);
    }
  }
  console.log(bad
    ? `\n⛔ кейсов с расхождением: ${bad}/${ids.length}`
    : `\n✅ все ${ids.length} кейсов: разметка совпадает с диффом по пину`);
  return bad;
}

const args = process.argv.slice(2);
const validateIdx = args.indexOf('--validate');
if (validateIdx >= 0) {
  const dir = resolve(process.cwd(), args[validateIdx + 1] || 'gold-public');
  const cacheDir = resolve(process.cwd(), process.env.REVIEW_GOLD_CACHE || '.ai-review/gold-public');
  process.exit(await validateDataset(dir, cacheDir) ? 1 : 0);
}
const emitIdx = args.indexOf('--emit');
const emitDir = emitIdx >= 0 ? resolve(process.cwd(), args[emitIdx + 1]) : null;
const cleanIdx = args.indexOf('--clean');
const cleanWant = cleanIdx >= 0 ? Number(args[cleanIdx + 1] || 2) : 0;
const valueIdxs = new Set([emitIdx >= 0 ? emitIdx + 1 : -1, cleanIdx >= 0 ? cleanIdx + 1 : -1]);
const repos = args.filter((a, i) => !a.startsWith('--') && !valueIdxs.has(i));

const force = args.includes('--force');

if (!repos.length) {
  console.error(`Usage:
  pr-review-gold-harvest <owner/repo…> [--emit <dir>] [--clean <N>] [--force]
  pr-review-gold-harvest --validate <dir>`);
  process.exit(1);
}

let emitted = 0;
let skipped = 0;

async function emit(candidate) {
  if (!emitDir) return;
  const dir = join(emitDir, candidate.id);
  const file = join(dir, 'meta.json');
  // Мета кейса может быть отредактирована руками (уточнённые локации, note разметки).
  // Повторная добыча не должна её молча затирать — для этого есть --force.
  if (!force) {
    try {
      await readFile(file, 'utf8');
      skipped++;
      return;
    } catch { /* файла нет — пишем */ }
  }
  await mkdir(dir, { recursive: true });
  await writeFile(file, `${JSON.stringify(candidate, null, 2)}\n`, 'utf8');
  emitted++;
}

for (const repo of repos) {
  const { scanned, candidates } = await harvestRepo(repo);
  console.log(`\n### ${repo} — просмотрено ${scanned} PR, кандидатов ${candidates.length}`);
  for (const c of candidates.sort((a, b) => a.id.localeCompare(b.id))) {
    const locs = c.expect.mustFindLocations;
    console.log(`${c.id}`);
    console.log(`   ${locs.length} локац.: ${locs.slice(0, 3).map((l) => `${l.file}:${l.lines[0]}-${l.lines[1]}`).join(', ')}`);
    await emit(c);
  }
  if (cleanWant > 0) {
    const cleans = await harvestClean(repo, cleanWant);
    console.log(`--- clean-контролей: ${cleans.length}`);
    for (const c of cleans) {
      console.log(`${c.id}`);
      await emit(c);
    }
  }
}
if (emitDir) {
  console.log(`\n[harvest] записано заготовок: ${emitted} → ${emitDir}`);
  if (skipped) console.log(`[harvest] пропущено (мета уже есть, --force перезапишет): ${skipped}`);
}
