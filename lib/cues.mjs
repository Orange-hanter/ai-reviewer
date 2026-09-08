/**
 * Косвенные признаки (proximal cues) и ранжирование спанов диффа — RFC 08.
 *
 * Слой отвечает на вопрос «куда смотреть», не читая тел файлов и не вызывая LLM: всё
 * считается по тексту диффа и по фактам, которые пайплайн уже посчитал (prescan,
 * blast-radius, класс пути, профиль). Терминология — из information foraging theory
 * (Pirolli & Card; Lawrance et al., TSE 2013): признак это proximal cue, указатель на
 * добычу, а НЕ сама добыча.
 *
 * Граница, которую нельзя размывать: признак — не находка. У него нет severity, он не
 * доезжает до `resolveBlockers` и не может ничего заблокировать. Prescan — улика (его
 * находки блокируют), признак — только повод потратить на участок больше внимания.
 *
 * Модуль чистый: без I/O, без сети, без профиля через синглтон. Всё внешнее приходит
 * аргументом `facts` — по той же дисциплине DI, что и `chunkNeedsSpecialists`. Отсутствие
 * факта означает «признак не поднят» (вес 0), а не ошибку: на shallow-клоне и без графа
 * слой обязан деградировать молча.
 *
 * В Rust не портируется (как lib/context-window.mjs и lib/incremental.mjs) — в
 * `npm run check:parity` не входит.
 */

/** Заголовок хунка: `@@ -a,b +c,d @@`. Нас интересует только RIGHT-сторона. */
const HUNK_RE = /^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@/;

/** Путь теста: разметка дефектов туда не указывает, а внимание стоит тех же денег. */
const TEST_PATH = /(?:^|\/)(?:tests?|spec|specs|__tests__|testing)\/|(?:^|\/)[^/]*(?:_test|\.test|\.spec|_spec)\.[A-Za-z0-9]+$|(?:^|\/)test_[^/]+$|(?:^|\/)conftest\.py$/;

/**
 * Лексикон рискованных конструкций в ДОБАВЛЕННЫХ строках — «мешок изменённых токенов»
 * JITLine (Pornprasit & Tantithamthavorn, MSR 2021), но не обученный, а выписанный.
 *
 * Правила:
 *   - паттерн обязан быть языконезависимым настолько, насколько возможно: набор кейсов
 *     публичного замера — Python, Go, TypeScript, Ruby;
 *   - каждый класс несёт ГИПОТЕЗУ, а не вердикт: гипотеза попадает в наряд и объясняет
 *     исполнителю, что проверять;
 *   - класс считается один раз на спан, сколько бы совпадений в нём ни было: иначе цикл
 *     из пятидесяти `await` перевесит одну строку с `eval`.
 */
export const LEXICON = [
  {
    key: 'unchecked-error',
    weight: 2,
    hypotheses: ['unchecked-error'],
    re: /\b(?:try|except|catch|rescue|finally|panic|recover|unwrap\(\)|\.expect\(|raise|throw|err\s*!=\s*nil|errors\.(?:Is|As|New)|traceback|swallow)\b|catch\s*\([^)]*\)\s*\{\s*\}/,
  },
  {
    key: 'null-deref',
    weight: 2,
    hypotheses: ['null-deref'],
    re: /\b(?:None|nil|null|undefined|NoneType|Optional|nullable)\b|\?\.|!\.|\.get\(|getattr\(|hasattr\(/,
  },
  {
    key: 'off-by-one',
    weight: 2,
    hypotheses: ['off-by-one', 'boundary'],
    re: /[<>]=|\b(?:len|count|size|length)\b|\.length\b|\brange\(|\[\s*-?\d+\s*[:\]]|\[\s*[a-z_]\w*\s*[-+]\s*1\s*\]|\b(?:first|last|head|tail|slice|substring|splice)\b/i,
  },
  {
    key: 'async-race',
    weight: 2,
    hypotheses: ['async-race', 'ordering'],
    re: /\b(?:await|async|Promise\.(?:all|race|allSettled)|go\s+func|goroutine|threading|Thread\(|mutex|Mutex|RLock|atomic|select\s*\{|transaction|atomic_?block|setTimeout|defer\s|yield)\b/,
  },
  {
    key: 'resource-leak',
    weight: 2,
    hypotheses: ['resource-leak'],
    re: /\b(?:open\(|connect\(|connection|session|cursor|socket|stream|\.close\(\)|dispose|with\s+\w+\s*\(|defer\s+\w+\.Close|acquire|release|pool)\b/,
  },
  {
    key: 'injection',
    weight: 3,
    hypotheses: ['injection'],
    re: /\b(?:execute\(|executemany\(|raw\(|SELECT\s|INSERT\s|UPDATE\s|DELETE\s|innerHTML|dangerouslySetInnerHTML|eval\(|exec\(|os\.system|subprocess|shell\s*=\s*True|child_process|Marshal|pickle\.loads|yaml\.load\()/i,
  },
  {
    key: 'authz',
    weight: 3,
    hypotheses: ['authz'],
    // Одиночные `owner`, `role`, `admin`, `scope` намеренно НЕ включены: в прикладном коде
    // это обычные имена полей, и на них признак срабатывал в тестовых файлах чаще, чем в
    // проверяемом коде. Остались составные имена и коды отказа — они означают решение о
    // доступе, а не предметную область.
    re: /\b(?:permission|permissions|is_superuser|is_staff|has_perm|has_scope|authorize|authorized|authentication|unauthorized|forbidden|can_[a-z_]+|allowed_|denied|acl|401|403)\b/i,
  },
  {
    key: 'secret',
    weight: 3,
    hypotheses: ['secret'],
    re: /\b(?:secret|password|passwd|api_?key|private_?key|credential|token)\b/i,
  },
  {
    key: 'api-contract',
    weight: 2,
    hypotheses: ['api-contract'],
    re: /\b(?:def\s+\w+\(|function\s+\w+\(|func\s+\w+\(|class\s+\w+|interface\s+\w+|serializer|schema|migration|endpoint|route|@api|public\s+\w+\s+\w+\()/,
  },
];

/** Экспортируемый символ в добавленных строках — кандидат в blast-radius. */
const EXPORTED_RE = /\bexport\s+(?:default\s+)?(?:async\s+)?(?:function|class|const|interface|type|enum)\b|\bexport\s*\{|^\s*(?:public|def)\s+\w+|^\s*func\s+[A-Z]\w*\(/m;

/**
 * Ранний выход, вставленный в существующий поток управления: добавленные строки несут
 * `return`/`raise`/`throw`/`abort`, а вокруг них — неизменённый код.
 *
 * Это структурный признак, а не лексический: он говорит не «здесь есть слово», а «у уже
 * работавшего пути появилась новая ветка выхода». Для существующих вызывающих это смена
 * поведения — то, ради чего blast-radius и существует. Признак родился из разбора кейса
 * `sentry-78722`, где охранное условие в двух публичных обработчиках было именно этим и
 * не имело ни одного лексического маркера.
 */
const GUARD_RE = /^\s*(?:return\b|raise\b|throw\b|abort\b|panic\b|os\.exit|sys\.exit|process\.exit)/m;
const BRANCH_RE = /^\s*(?:if|unless|elsif|elif|else\s+if|when|guard|switch|match)\b|^\s*\}\s*else\b/m;

/** Веса содержательных признаков — слагаемые. Перебиваются профилем (секция `cues.weights`). */
export const DEFAULT_WEIGHTS = {
  'shape:modified': 2,
  'shape:dense-add': 1,
  'shape:guard-insert': 3,
  'api:exported-changed': 2,
  'prescan:hit': 3,
  'graph:caller-outside': 2,
  'graph:cross-community': 1,
  'prior:seen': 2,
};

/**
 * Признаки поверхности — МНОЖИТЕЛИ, а не слагаемые.
 *
 * Разница принципиальная и стоила первого замера. Лексические совпадения в тестовом файле
 * описывают код, который тест проверяет, а не риск в самом тесте: в кейсе `sentry-78722`
 * тестовые хунки набрали 5 очков против 1 у исходника, и разметка (всегда указывающая в
 * исходник) оказалась внизу ранжирования. Слагаемым штрафом это не чинится — сумма
 * лексикона не ограничена сверху, и любой фиксированный минус она перебьёт. Множитель
 * ограничивает по построению: он говорит «всё, что здесь найдено, стоит вчетверо меньше»,
 * а не «вычти три».
 *
 * Тесты при этом не выбрасываются (в JIT-литературе их обычно исключают вовсе): дефект в
 * тесте возможен, и слой обязан оставлять ему шанс — просто не за счёт исходника.
 */
export const DEFAULT_FACTORS = {
  'path:test': 0.25,
  'path:docs': 0.1,
  'path:config': 0.6,
  'path:sensitive': 1.5,
  'shape:move-symmetry': 0.2,
};

/** Гипотеза → роль исполнителя. Таблица, а не эвристика в промпте. */
export const HYPOTHESIS_ROLE = {
  injection: 'security',
  authz: 'security',
  secret: 'security',
  'unchecked-error': 'correctness',
  'null-deref': 'correctness',
  'off-by-one': 'correctness',
  boundary: 'correctness',
  'async-race': 'correctness',
  ordering: 'correctness',
  'resource-leak': 'correctness',
  'api-contract': 'architecture',
  'cross-boundary': 'architecture',
};

function weightOf(weights, key, fallback = 0) {
  const w = weights?.[key];
  return Number.isFinite(w) ? w : fallback;
}

/**
 * Спаны изменений: один хунк — один спан. Хунк и есть естественная единица «независимо
 * читаемого куска» (Barnett et al., ICSE 2015 разбивает changeset дешёвым статическим
 * анализом ровно ради этого), а границы уже проставлены git'ом.
 *
 * `lib/diff.mjs` намеренно не трогаем: он под Rust-портом и parity, а разбор `@@` здесь
 * стоит два десятка строк.
 *
 * @param {Map<string, string>} sections — `extractFileDiffSections(diff)` из lib/router.mjs
 * @returns {{file:string,startLine:number,endLine:number,added:number,removed:number,effort:number,addedText:string,removedText:string,index:number}[]}
 */
export function collectSpans(sections) {
  const spans = [];
  let index = 0;
  for (const [file, section] of sections || []) {
    let newLine = 0;
    let cur = null;
    const flush = () => {
      if (!cur) return;
      // Спан без добавленных строк — чистое удаление: смотреть в новой версии не на что.
      if (cur.added > 0) spans.push({ ...cur, index: index++ });
      cur = null;
    };
    for (const raw of String(section || '').split('\n')) {
      const hunk = raw.match(HUNK_RE);
      if (hunk) {
        flush();
        newLine = Number(hunk[1]) || 1;
        cur = {
          file,
          startLine: newLine,
          endLine: newLine,
          added: 0,
          removed: 0,
          effort: Number(hunk[2] ?? 1) || 1,
          addedText: [],
          removedText: [],
        };
        continue;
      }
      if (!cur) continue;
      if (raw.startsWith('+++') || raw.startsWith('---')) continue;
      if (raw.startsWith('+')) {
        cur.addedText.push(raw.slice(1));
        cur.added += 1;
        cur.endLine = newLine;
        newLine += 1;
      } else if (raw.startsWith('-')) {
        cur.removedText.push(raw.slice(1));
        cur.removed += 1;
      } else if (raw.startsWith('\\')) {
        // «\ No newline at end of file» — не строка файла.
      } else {
        cur.endLine = newLine;
        newLine += 1;
      }
    }
    flush();
  }
  return spans.map((s) => ({
    ...s,
    addedText: s.addedText.join('\n'),
    removedText: s.removedText.join('\n'),
    // Эффорт — сколько строк новой версии придётся прочитать, чтобы осмотреть спан.
    effort: Math.max(1, s.endLine - s.startLine + 1),
  }));
}

/** Мультимножество токенов: перемещённый код даёт то же множество в `+` и `-`. */
function tokenBag(text) {
  const bag = new Map();
  for (const t of String(text).match(/[A-Za-z_][\w$]{2,}/g) || []) {
    bag.set(t, (bag.get(t) || 0) + 1);
  }
  return bag;
}

/** Доля общих токенов между добавленным и удалённым (0..1). */
function moveSymmetry(addedText, removedText) {
  const a = tokenBag(addedText);
  const b = tokenBag(removedText);
  if (!a.size || !b.size) return 0;
  let shared = 0;
  let total = 0;
  for (const [t, n] of a) {
    total += n;
    shared += Math.min(n, b.get(t) || 0);
  }
  return total ? shared / total : 0;
}

function inSpan(span, file, line, tolerance = 0) {
  if (String(file || '').replace(/^\.\//, '') !== span.file.replace(/^\.\//, '')) return false;
  const ln = Number(line);
  if (!Number.isFinite(ln)) return true;
  return ln >= span.startLine - tolerance && ln <= span.endLine + tolerance;
}

/**
 * Признаки одного спана.
 *
 * @param {object[]} spans — из `collectSpans`
 * @param {object} facts
 * @param {(path:string)=>string} [facts.fileClass] — класс роутера (мемоизированный!)
 * @param {RegExp} [facts.sensitive] — `profile.layout.sensitive`
 * @param {Set<string>|string[]} [facts.addedPaths] — новые файлы (`collectAddedPaths`)
 * @param {object[]} [facts.prescan] — находки prescan ({ file, line })
 * @param {object[]} [facts.blast] — `parseBlastDependents` ({ file, line, crossCommunity })
 * @param {Set<string>|string[]} [facts.priorPaths] — пути из 👎-журнала и прошлых находок
 * @param {object} [facts.weights] — перебивка весов из профиля
 * @returns {object[]} те же спаны с полями `cues`, `score`, `hypotheses`, `addedFile`
 */
export function collectCues(spans, facts = {}) {
  const weights = { ...DEFAULT_WEIGHTS, ...(facts.weights || {}) };
  const factors = { ...DEFAULT_FACTORS, ...(facts.factors || {}) };
  const addedPaths = facts.addedPaths instanceof Set
    ? facts.addedPaths
    : new Set(facts.addedPaths || []);
  const priorPaths = facts.priorPaths instanceof Set
    ? facts.priorPaths
    : new Set(facts.priorPaths || []);
  const fileClass = typeof facts.fileClass === 'function' ? facts.fileClass : null;

  return (spans || []).map((span) => {
    const cues = [];
    const add = (key, evidence, hypotheses = [], weight = null) => {
      const w = weight === null ? weightOf(weights, key) : weight;
      if (!w) return;
      cues.push({ key, weight: w, kind: 'content', evidence: String(evidence).slice(0, 200), hypotheses });
    };
    const scale = (key, evidence) => {
      const f = factors?.[key];
      if (!Number.isFinite(f) || f === 1) return;
      cues.push({ key, factor: f, kind: 'surface', evidence: String(evidence).slice(0, 200), hypotheses: [] });
    };

    // --- форма изменения (Nagappan & Ball; Hassan) --------------------------------
    if (span.added && span.removed) {
      add('shape:modified', `+${span.added} / -${span.removed}`, []);
    }
    if (span.added / span.effort > 0.8) {
      add('shape:dense-add', `${span.added} из ${span.effort} строк добавлены`, []);
    }
    // Ранний выход, вставленный в существующий код: у спана есть контекстные строки
    // (значит, функция существовала) и добавлена ветка с выходом.
    if (span.effort > span.added && GUARD_RE.test(span.addedText) && BRANCH_RE.test(span.addedText)) {
      const first = span.addedText.split('\n').find((l) => BRANCH_RE.test(l)) || '';
      add('shape:guard-insert', first.trim(), ['api-contract', 'unchecked-error']);
    }
    const symmetry = span.removed ? moveSymmetry(span.addedText, span.removedText) : 0;
    if (symmetry > 0.9) {
      scale('shape:move-symmetry', `${Math.round(symmetry * 100)}% токенов совпадают с удалёнными`);
    }

    // --- поверхность: множители достоверности --------------------------------------
    if (TEST_PATH.test(span.file)) scale('path:test', span.file);
    if (fileClass) {
      const cls = fileClass(span.file);
      if (cls === 'docs' || cls === 'skip') scale('path:docs', `класс ${cls}`);
      else if (cls === 'config') scale('path:config', `класс ${cls}`);
    }
    if (facts.sensitive?.test?.(span.file)) scale('path:sensitive', span.file);

    // --- лексикон (JITLine) --------------------------------------------------------
    for (const entry of LEXICON) {
      const hits = span.addedText.match(new RegExp(entry.re.source, `${entry.re.flags.replace('g', '')}g`));
      if (!hits?.length) continue;
      const key = `lex:${entry.key}`;
      const w = weightOf(weights, key, entry.weight);
      if (!w) continue;
      cues.push({
        key,
        weight: w,
        kind: 'content',
        evidence: `${hits.length}× ${String(hits[0]).trim().slice(0, 60)}`,
        hypotheses: entry.hypotheses,
      });
    }

    // --- API и граф ----------------------------------------------------------------
    if (EXPORTED_RE.test(span.addedText)) {
      add('api:exported-changed', 'изменён экспортируемый символ', ['api-contract']);
    }
    for (const dep of facts.blast || []) {
      if (!inSpan(span, dep.file, dep.line, 0)) continue;
      add('graph:caller-outside', `${dep.caller || dep.symbol} → ${dep.file}:${dep.line}`, ['cross-boundary']);
      if (dep.crossCommunity) add('graph:cross-community', dep.file, ['cross-boundary']);
      break;
    }

    // --- уже посчитанные детерминированные сигналы ---------------------------------
    for (const p of facts.prescan || []) {
      if (!inSpan(span, p.file, p.line, 0)) continue;
      add('prescan:hit', p.title || p.ruleId || 'prescan', []);
      break;
    }
    if (priorPaths.has(span.file)) add('prior:seen', 'путь встречался в прошлых находках', []);

    const hypotheses = [...new Set(cues.flatMap((c) => (c.kind === 'content' ? c.hypotheses : [])))];
    // Содержание складывается, поверхность умножает. Сумма — «что найдено»,
    // произведение — «сколько это здесь стоит».
    const content = cues.reduce((s, c) => s + (c.kind === 'content' ? c.weight : 0), 0);
    const factor = cues.reduce((f, c) => f * (c.kind === 'surface' ? c.factor : 1), 1);
    const score = Number((Math.max(0, content) * factor).toFixed(3));
    return { ...span, cues, content, factor, score, hypotheses, addedFile: addedPaths.has(span.file) };
  });
}

/**
 * Ранжирование. База — unsupervised: очки, делённые на эффорт (Yang et al., FSE 2016
 * показали, что простое ранжирование по плотности конкурентно обученным моделям —
 * значит, стартовать можно без размеченной выборки).
 *
 * `effortExponent` управляет компромиссом: 0 — чистые очки (крупные хунки наверху),
 * 1 — чистая плотность (мелкие хунки наверху, лучший ACC@k, но хуже IFA), 0.5 —
 * середина. Значение выбирается замером, а не осторожностью.
 *
 * @returns {object[]} новый массив, отсортированный по убыванию `density`
 */
export function rankSpans(spans, { effortExponent = 0.5 } = {}) {
  return (spans || [])
    .map((s) => {
      const positive = Math.max(0, s.score ?? 0);
      const density = positive / Math.pow(Math.max(1, s.effort), effortExponent);
      return { ...s, density };
    })
    .sort((a, b) => (
      b.density - a.density
      // Тай-брейк детерминированный: порядок диффа. Иначе одинаковые спаны переставляются
      // между прогонами, и замер перестаёт быть воспроизводимым.
      || a.index - b.index
    ));
}
