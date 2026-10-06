/**
 * Семейство FP 7: «файла/модуля нет» о пути, который есть в репозитории, но не в PR.
 *
 * Модель видит импорт или ссылку на файл, которого нет ни в диффе, ни в контексте чанка,
 * и выводит из этого «не существует». Защиты семейств 1/6 (список изменённых файлов,
 * паттерн `file-absent-in-diff`) знают только `changedPaths` — файл вне PR им не виден.
 * Здесь утверждение об отсутствии сверяется с деревом репозитория детерминированно, после
 * LLM: путь нашёлся в дереве → находка снимается как известный FP.
 *
 * Снимается только утверждение «нет файла/модуля». «Символа `X` нет в `a.ts`» сюда не
 * попадает: путь в нём существует по условию, отсутствует символ — это проверка по
 * содержимому, а не по дереву.
 *
 * Не портирован в Rust: работает на JS-регексах с Unicode-классами, как FP-фильтр.
 */

import { execFile } from 'node:child_process';
import { posix } from 'node:path';
import { promisify } from 'node:util';

const execFileP = promisify(execFile);

// \b в JS — только ASCII, кириллицу не видит; граница слова — через \p{L}.
const L = '(?<![\\p{L}\\p{N}_])';
const R = '(?![\\p{L}\\p{N}_])';

/** Утверждение об отсутствии самого файла: «не существует», «not found», «несуществующий файл». */
const ABSENT_PHRASE = `(?:не\\s+существу\\p{L}*|не\\s+найден\\p{L}*|отсутству\\p{L}*\\s+в\\s+(?:репозитори|проект|кодов|дерев)\\p{L}*`
  + `|нет\\s+в\\s+(?:репозитори|проект|кодов|дерев)\\p{L}*|не\\s+резолвит\\p{L}*|не\\s+разреша\\p{L}*`
  + `|does\\s+not\\s+exist|doesn['’]t\\s+exist|is\\s+(?:missing|absent)|not\\s+found|cannot\\s+be\\s+(?:found|resolved)`
  + `|can['’]t\\s+be\\s+(?:found|resolved)|could\\s+not\\s+be\\s+(?:found|resolved)`
  // Только с существительным «файл/модуль/путь»: «вызывает несуществующую функцию» — про символ.
  + `|несуществующ\\p{L}*\\s+(?:файл|модул|пут)\\p{L}*|non-?existent\\s+(?:file|module|path))${R}`;

/** Утверждение после пути. Якорь с обеих сторон — защита от широкого совпадения: между путём
 * и утверждением допустимы только связки («указывает на»), а после него — лишь «в репозитории/
 * в PR» и конец предложения. Иначе «`guard.ts` не найден в списке провайдеров» или
 * «`csp.json` is missing the directive» — верные находки о содержимом — снимались бы как FP. */
const ABSENT_AFTER_ADJACENT = new RegExp(
  `^[\\s'"\`):,]*(?:[—–-]\\s*)?(?:(?:указывает|ведёт|ссылается|points|refers)\\s+(?:на|to)\\s+(?:an?\\s+)?)?`
  + `${L}${ABSENT_PHRASE}`
  + `(?:\\s+(?:в|во|in|from)\\s+(?:the\\s+|этом\\s+)?(?:репозитори|проект|кодов\\p{L}*\\s+баз|дерев|pr|дифф|repo|repository|codebase|project|tree|workspace)\\p{L}*)?`
  + `(?=\\s*(?:$|[—–,:;(!?]|\\.(?:\\s|$)|-\\s))`,
  'iu',
);
const ABSENT_AFTER = new RegExp(`${L}${ABSENT_PHRASE}`, 'iu');

/** Утверждение до пути: «несуществующий модуль `./x`», «Cannot find module './x'».
 * Только формулировки о файле/модуле и вплотную к пути (ABSENT_BEFORE_ADJACENT):
 * «not found in `a.ts`» и «несуществующую функцию … `a.ts`» — про символ, не про файл. */
const ABSENT_BEFORE_SRC = `${L}(?:(?:несуществующ|не\\s+существующ|отсутствующ)\\p{L}*(?:\\s+(?:файл|модул|пут|импорт)\\p{L}*)?`
  + `|cannot\\s+find\\s+module|can['’]t\\s+resolve|unable\\s+to\\s+resolve|module\\s+not\\s+found`
  + `|unresolved\\s+import(?:\\s+of)?|non-?existent(?:\\s+(?:file|module|path))?|no\\s+such\\s+(?:file|module)|missing\\s+(?:file|module))${R}`;
const ABSENT_BEFORE = new RegExp(ABSENT_BEFORE_SRC, 'iu');
const ABSENT_BEFORE_ADJACENT = new RegExp(`${ABSENT_BEFORE_SRC}[\\s:'"\`(]*$`, 'iu');

/** Отсутствие как условие рантайма — «если файл не существует, упадём» — это верная находка
 * о коде, а не утверждение о дереве репозитория. Такое предложение не трогаем. */
const RUNTIME_CONDITION = new RegExp(
  `${L}(?:если|когда|в\\s+случае|при\\s+отсутствии|if|when|unless|in\\s+case)${R}|ENOENT|existsSync|exists\\s*\\(|FileNotFound`,
  'iu',
);

const MAX_GAP_BEFORE = 40;

/** Расширения, которые подставляет резолвер языка файла находки. Чужие не перебираем:
 * `./data` из `.ts` не находит `data.json`, и считать такой импорт разрешённым — расхождение
 * с настоящим резолвером, которое сняло бы верную находку. */
const JS_EXTS = ['.ts', '.tsx', '.js', '.jsx', '.mjs', '.cjs', '.mts', '.cts', '.vue', '.svelte'];
const FAMILIES = [
  { re: /\.(?:[cm]?[jt]sx?|vue|svelte)$/, exts: JS_EXTS, index: JS_EXTS.map((e) => `/index${e}`) },
  { re: /\.py$/, exts: ['.py'], index: ['/__init__.py'] },
];

function familyOf(file) {
  return FAMILIES.find((fam) => fam.re.test(String(file || ''))) || null;
}

/** Варианты пути, в которые резолвер языка может превратить спецификатор. */
function variantsOf(p, fam) {
  const out = [p];
  if (!fam) return out;
  // ESM-TypeScript: `./x.js` в исходнике резолвится в `./x.ts`.
  const m = fam.exts === JS_EXTS && p.match(/^(.*)\.([cm]?)js(x?)$/);
  if (m) out.push(`${m[1]}.${m[2]}ts${m[3]}`, `${m[1]}.tsx`);
  for (const e of fam.exts) out.push(p + e);
  for (const i of fam.index) out.push(p + i);
  return out;
}

// Кандидат в путь: содержит «/» либо оканчивается расширением. Цифры-версии («1.5») и URL отсекаются ниже.
const PATH_TOKEN = /[\w@~.\-/]*[\w@~\-]\/[\w.\-/]*[\w]|(?:\.{1,2}\/)[\w.\-/]+|[\w\-]+(?:\.[\w\-]+)*\.[A-Za-z][A-Za-z0-9]{0,5}(?![\w/])/g;

/** Предложения без разрыва по точке внутри пути: «a.ts» — не конец предложения, «. » — конец. */
function sentences(text) {
  return String(text || '').split(/[!?\n]+|\.(?:\s+|$)|;\s+/u).filter((s) => s.trim());
}

function cleanToken(tok) {
  let t = tok.replace(/^['"`(]+|['"`),:]+$/g, '');
  t = t.replace(/:\d+(?:[-–]\d+)?$/, '');
  return t;
}

function isPathLike(t) {
  if (!t || t.length < 3 || t.length > 300) return false;
  if (t.includes('://')) return false;
  if (/^\d+(?:\.\d+)*$/.test(t)) return false;
  return true;
}

/**
 * Индекс дерева репозитория. `null`, если дерево явно чужое: меньше половины изменённых
 * путей PR в нём есть (локальный `--dry-run --diff` из другого каталога). На чужом
 * дереве совпадение имени — случайность, и снимать по нему находку нельзя.
 *
 * @param {string[]|null} repoFiles  пути от корня репозитория (`git ls-files`)
 * @param {string[]} [changedPaths]
 */
export function buildRepoPathIndex(repoFiles, changedPaths = []) {
  if (!Array.isArray(repoFiles) || !repoFiles.length) return null;
  const set = new Set(repoFiles.map((p) => posix.normalize(String(p)).replace(/^\.\//, '')));
  const changed = (changedPaths || []).filter(Boolean);
  if (changed.length) {
    const present = changed.filter((p) => set.has(p)).length;
    if (present * 2 < changed.length) return null;
  }
  return { set, files: [...set] };
}

function existsExact(index, p, fam) {
  if (!p || p.startsWith('..')) return null;
  for (const v of variantsOf(p, fam)) {
    if (index.set.has(v)) return v;
  }
  return null;
}

/** Совпадение по концу пути принимается, только если оно единственное: куда на самом деле
 * ведёт алиас (`@/lib/db`) или путь не от корня, неизвестно, и при двух кандидатах «файл
 * есть» — догадка, по которой нельзя снимать находку. */
function existsByUniqueSuffix(index, p, fam) {
  const variants = variantsOf(p, fam);
  let hit = null;
  for (const f of index.files) {
    if (!variants.some((v) => f.endsWith(`/${v}`))) continue;
    if (hit) return null;
    hit = f;
  }
  return hit;
}

/**
 * Разрешает упомянутый путь против дерева. Относительный — от каталога файла находки;
 * алиас `@/`, `~/` и путь с «/» — от корня или по единственному совпадению конца пути;
 * голое имя файла — только в каталоге находки (одноимённый файл в другом месте дерева
 * ничего не доказывает). Расширения — только языка файла находки.
 */
export function resolveMentionedPath(token, findingFile, index) {
  const t = cleanToken(token);
  if (!isPathLike(t)) return null;
  const fam = familyOf(findingFile);
  const dir = findingFile ? posix.dirname(String(findingFile)) : '.';
  if (t.startsWith('./') || t.startsWith('../')) {
    return existsExact(index, posix.normalize(posix.join(dir, t)), fam);
  }
  const bare = posix.normalize(t.replace(/^(?:@|~)\//, '').replace(/^\/+/, ''));
  if (bare.includes('/')) {
    return existsExact(index, bare, fam) || existsByUniqueSuffix(index, bare, fam);
  }
  return existsExact(index, posix.normalize(posix.join(dir, bare)), fam);
}

/**
 * Путь из дерева репозитория, об отсутствии которого заявляет находка, или `null`.
 * Утверждение и путь должны стоять в одном предложении вплотную: путь в одном месте текста
 * и «не найден» про символ в другом — не повод снимать находку.
 *
 * @param {{ title?: string, body?: string, file?: string }} finding
 * @param {{ set: Set<string>, files: string[] }|null} index
 */
export function findAbsentButExistingPath(finding, index) {
  if (!index) return null;
  const text = `${finding?.title || ''}. ${finding?.body || ''}`;
  for (const s of sentences(text)) {
    if (RUNTIME_CONDITION.test(s)) continue;
    if (!ABSENT_AFTER.test(s) && !ABSENT_BEFORE.test(s)) continue;
    for (const m of s.matchAll(PATH_TOKEN)) {
      const after = s.slice(m.index + m[0].length);
      const before = s.slice(Math.max(0, m.index - MAX_GAP_BEFORE), m.index);
      const claimed = ABSENT_AFTER_ADJACENT.test(after) || ABSENT_BEFORE_ADJACENT.test(before);
      if (!claimed) continue;
      const hit = resolveMentionedPath(m[0], finding?.file, index);
      if (hit) return hit;
    }
  }
  return null;
}

/**
 * Файлы рабочего дерева (`git ls-files`). Fail-open: не git, нет git, таймаут → `null`,
 * и фильтр семейства 7 просто не работает в этом прогоне.
 */
export async function listRepoFiles({ cwd = process.cwd(), exec = execFileP } = {}) {
  try {
    const { stdout } = await exec('git', ['-C', cwd, 'ls-files', '-z'], {
      timeout: 10_000, maxBuffer: 64 * 1024 * 1024,
    });
    const files = String(stdout).split('\0').filter(Boolean);
    return files.length ? files : null;
  } catch {
    return null;
  }
}
