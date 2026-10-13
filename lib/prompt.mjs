/**
 * Фаза 3, шаг 3: lib/prompt.mjs — JS-обёртка над Rust-бинарём
 * rust/target/debug/prompt (порт конструкции промптов: buildMessages,
 * buildVerifierMessages, buildChangedFilesSection, reference-секции и т.д.).
 *
 * Почему обёртка, а не прямой вызов: промпты зависят от мутабельного профиля
 * (getProfile/repoRules) и от layout.e2eSpec — JS-RegExp, диалект которого
 * fancy-regex на стороне Rust покрывает не целиком. Профиль сериализуется аргументами
 * на КАЖДЫЙ вызов (профиль меняется между прогонами: setProfile в тестах/CLI):
 *   profile: {repoRules, prompts: {advisorExtra, specialistExtra,
 *             aggregatorExtra, verifierExtra}, e2eSpecSource: string|null}
 * e2eSpec приходит ИСХОДНИКОМ (RegExp.source), Rust компилирует fancy-regex с (?i) —
 * как new RegExp(raw, 'i') в lib/profile.mjs (lookahead/backreference поддержаны).
 * Синтаксис, который fancy-regex не компилирует, → бинарь падает
 * с маркером E2ESPEC_UNSUPPORTED → согласованная деградация на чистый JS
 * (как layout.sensitive в lib/actionability.mjs); любая другая ошибка бинаря
 * пробрасывается — молчаливая подмена скрывала бы регрессии Rust-кода.
 *
 * Побайтовый паритет с JS-fallback проверяется rust/parity/prompt.parity.mjs:
 * промпт — это ВХОД модели, расхождение меняет поведение LLM.
 *
 * Резолвинг бинаря (по аналогии с lib/engine.mjs):
 *   AI_REVIEWER_PROMPT_BIN — override; по умолчанию <repo>/rust/target/debug/prompt;
 *   AI_REVIEWER_PROMPT_FORCE_JS=1 — принудительно чистый JS (паритет/отладка).
 * Бинаря нет (CI до cargo build) — эталонный JS fallback ниже.
 *
 * Потребители обёртки (engine-оркестрация через инжектимый prompt, pipeline)
 * не меняются: DI-моки подменяют весь объект prompt целиком, createPromptBuilder
 * возвращает тот же набор методов.
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { repoRules } from './constants.mjs';
import { getProfile } from './profile.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_PROMPT_BIN = resolve(__dir, '../rust/target/debug/prompt');

function resolvePromptBin() {
  if (process.env.AI_REVIEWER_PROMPT_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_PROMPT_BIN || '').trim() || DEFAULT_PROMPT_BIN;
  return existsSync(bin) ? bin : null;
}

/** Нужные части активного профиля — читаются на каждый вызов (профиль живой). */
function profileCtx() {
  const p = getProfile();
  const prompts = p.prompts || {};
  return {
    repoRules: p.repoRules || '',
    prompts: {
      advisorExtra: prompts.advisorExtra || '',
      specialistExtra: prompts.specialistExtra || '',
      aggregatorExtra: prompts.aggregatorExtra || '',
      verifierExtra: prompts.verifierExtra || '',
    },
    e2eSpecSource: p.layout?.e2eSpec ? p.layout.e2eSpec.source : null,
  };
}

/**
 * Вызов Rust-бинаря; null — бинарь недоступен или e2eSpec неподдерживаем
 * regex-крейтом (деградация на JS). Остальные ошибки бинаря — throw.
 */
function runRust(op, args) {
  const bin = resolvePromptBin();
  if (!bin) return null;
  try {
    const raw = execFileSync(bin, [], {
      input: JSON.stringify({ op, profile: profileCtx(), ...args }),
      encoding: 'utf8',
      maxBuffer: 128 * 1024 * 1024,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    return JSON.parse(raw);
  } catch (e) {
    // lookahead-ветка: вход для бинаря неподдерживаем, как курируемые
    // FP-паттерны в lib/findings.mjs — обе стороны согласованно на чистом JS.
    if (String(e?.stderr || e?.message || '').includes('E2ESPEC_UNSUPPORTED')) return null;
    throw e;
  }
}

/** Блок правил проекта из профиля (пустая строка, если профиля нет — тогда и блока нет). */
function rulesBlock() {
  const rules = repoRules();
  return rules ? `${rules}\n` : '';
}

/** Вставка из профиля: пустая → ничего, иначе отдельный абзац. */
function promptExtra(key) {
  const text = getProfile().prompts?.[key] || '';
  return text ? `\n${text}\n` : '';
}

/** Не даём содержимому diff/file ломать markdown fence в промпте. */
function fenceEscape(text) {
  return String(text ?? '').replace(/```/g, '``\u200b`');
}

/** PR title в промпт: убираем backticks и переводы строк (prompt-injection hygiene, P2-13). */
function safePrTitle(title) {
  return String(title || '').replace(/[`\n\r]+/g, ' ').slice(0, 200);
}

function referenceRules() {
  return `You are a code review advisor (reference model in a Mixture-of-Agents pipeline).
${rulesBlock()}${promptExtra('advisorExtra')}
Analyze the PR diff and the attached file context (windows around the changed lines). List ONLY
real defects: bugs, security/RBAC issues, layer-boundary violations, missing tests for
non-trivial logic. Do NOT nitpick style (eslint/tsc).
Write IN RUSSIAN as concise bullet points:
- [severity] \`file:line\` — title: explanation (severity = critical|major|minor)
  quote: <one line copied verbatim from the diff or file context>
Report AT MOST ${SPECIALIST_MAX_ITEMS} items, strongest first. Do NOT report nits. EVERY item, whatever its
severity, MUST carry its own \`quote:\` line: if you cannot quote the offending line verbatim AND name a
concrete fix, leave the item out.
Do NOT output JSON — a separate aggregator will synthesize structured findings from your advice.
If nothing serious — say «серьёзных замечаний нет».`;
}

/**
 * Потолок числа пунктов у одной роли (RFC 04, решение 3). Зашит константой, а не env:
 * ту же величину обязан знать rust/src/prompt.rs, и лишний параметр в PromptCtx — лишняя
 * поверхность для молчаливого рассинхрона сторон.
 *
 * Обоснование величины: на замере PR #6 из 34 находок до строки в PR доехала одна, а 33
 * ушли в свод «вне строк диффа». Длинный хвост оплачивается трижды — выходом специалиста,
 * входом жюри и работой refutation.
 */
const SPECIALIST_MAX_ITEMS = 8;

const SPECIALIST_FOCUS = {
  security: `Focus ONLY on security: injections, auth/RBAC bypass, IDOR, data leaks, secrets in code,
unsafe deserialization, SSRF, path traversal. Ignore style and non-security logic nitpicks.`,
  correctness: `Focus ONLY on correctness: logic bugs, races, null/edge cases, wrong rounding of money/quantities,
off-by-one, broken error handling, half-done decoding of an external format (a quoted/escaped/encoded value is
recognized and sliced out, but the escapes inside it are never decoded back), regressions visible in the
diff/context. Ignore security-only and architecture lectures.`,
  architecture: `Focus ONLY on architecture/layers: module boundaries of this project, forbidden imports,
duplication of canonical primitives (see the project rules above), blast-radius breaking changes. Ignore pure security and micro-bugs.`,
};

/** Заголовок секции роли в слитом ответе — общий контракт промпта и разбора. */
export const MERGED_ROLE_HEADING = (role) => `### ROLE: ${role}`;

/**
 * Системный промпт для случая, когда все роли назначены на ОДНУ модель (RFC 04, решение 4).
 * Три «независимых критика» на одной модели с одинаковым контекстом независимы только на
 * бумаге: платить за независимость, которой нет, — три вызова вместо одного.
 *
 * Правила проекта и specialistExtra вставляются ОДИН раз (в трёх отдельных промптах они
 * ехали трижды), фокусы ролей — по разделам. Rust-порт — build_merged_specialist_system
 * в rust/src/prompt.rs (RFC 20 T36), диспетчер — buildMergedSpecialistSystem ниже.
 */
function buildMergedSpecialistSystemJs(roles) {
  const list = (Array.isArray(roles) ? roles : []).filter((r) => Object.hasOwn(SPECIALIST_FOCUS, r));
  const sections = list
    .map((role) => `${MERGED_ROLE_HEADING(role)}\n${SPECIALIST_FOCUS[role]}`)
    .join('\n\n');
  return `You are a PANEL of ${list.length} independent code review specialists (Mixture-of-Critics pipeline),
each with a strictly separate remit. Work through the remits one at a time and do NOT let one
remit answer for another: a finding belongs to exactly one section, the one whose remit covers it.
${rulesBlock()}${promptExtra('specialistExtra')}
${sections}

Analyze the PR diff and the attached file context (windows around the changed lines). Write IN RUSSIAN as bullets:
- [severity] \`file:line\` — title: explanation (severity = critical|major|minor)
  quote: <one line copied verbatim from the diff or file context>
Report AT MOST ${SPECIALIST_MAX_ITEMS} items PER SECTION, strongest first — a long tail costs the jury
more than it finds. Do NOT report nits. EVERY item, whatever its severity, MUST carry its own
\`quote:\` line: if you cannot quote the offending line verbatim AND name a concrete fix, leave the
item out. Better three proven defects than twenty observations.
Do NOT output JSON — the jury aggregator will refute/verify each claim.

Output EXACTLY ${list.length} sections, in this order, each opening with its heading verbatim on its own line:
${list.map((role) => MERGED_ROLE_HEADING(role)).join('\n')}
A section with nothing to report must still be present — write «в моей области серьёзных замечаний нет».`;
}

function specialistRules(role) {
  const focus = SPECIALIST_FOCUS[role] || '';
  return `You are a SPECIALIST code review advisor — role: **${role}** (Mixture-of-Critics pipeline).
${rulesBlock()}${promptExtra('specialistExtra')}
${focus}

Analyze the PR diff and the attached file context (windows around the changed lines). List ONLY issues
in YOUR specialty. Write IN RUSSIAN as bullets:
- [severity] \`file:line\` — title: explanation (severity = critical|major|minor)
  quote: <one line copied verbatim from the diff or file context>
Report AT MOST ${SPECIALIST_MAX_ITEMS} items, strongest first — a long tail costs the jury more than it finds.
Do NOT report nits. EVERY item, whatever its severity, MUST carry its own \`quote:\` line: if you cannot
quote the offending line verbatim AND name a concrete fix, leave the item out.
Better three proven defects than twenty observations.
Do NOT output JSON — the jury aggregator will refute/verify each claim.
If nothing in your specialty — say «в моей области серьёзных замечаний нет».`;
}

function buildSpecialistReferenceSectionJs(references) {
  const blocks = references
    .filter((r) => r.text && !r.error)
    .map((r) => `### Specialist: ${r.role} (${r.model})\n${r.text}`);
  if (!blocks.length) return '';
  const failed = references.filter((r) => r.error)
    .map((r) => `- ${r.role} (${r.model}): ${r.error}`).join('\n');
  let s = `## Specialist advisor analyses (Mixture-of-Critics — synthesize + refute)\n\n${blocks.join('\n\n')}\n`;
  if (failed) s += `\n### Specialists unavailable\n${failed}\n`;
  s += `\nYou are the JURY aggregator. Synthesize findings from specialists. For EACH output finding include \`advisorClaims\`:
one entry per specialist role that raised a related concern, with YOUR refutation:
- **verified** — claim proven in the code (keep finding)
- **rejected** — false positive / already mitigated (if ALL roles rejected → omit finding entirely)
- **downgraded** — real but not merge-blocking (lower severity to minor/nit)

Do NOT blindly copy specialist bullets — verify against code.\n\n`;
  return s;
}

function buildMoAReferenceSectionJs(references) {
  const blocks = references
    .filter((r) => r.text && !r.error)
    .map((r) => `### Advisor: ${r.model}\n${r.text}`);
  if (!blocks.length) return '';
  const failed = references.filter((r) => r.error).map((r) => `- ${r.model}: ${r.error}`).join('\n');
  let s = `## Reference model analyses (Mixture-of-Agents — synthesize, do not blindly copy)\n\n${blocks.join('\n\n')}\n`;
  if (failed) s += `\n### Advisors unavailable\n${failed}\n`;
  s += `\nUse the advisors as hints: verify each claim against the code, drop false positives, merge duplicates, and output the final JSON findings yourself.\n\n`;
  return s;
}

/**
 * Потолок числа находок у жюри (RFC 04, экономия ВЫХОДА). У minimax-m3 находка агрегатора стоит
 * ~1900 completion-токенов, а роли уже ограничены SPECIALIST_MAX_ITEMS каждая: без своего потолка
 * жюри перемножает их хвосты. Как и SPECIALIST_MAX_ITEMS — константа, а не env: ту же величину
 * обязан знать rust/src/prompt.rs.
 */
const AGGREGATOR_MAX_ITEMS = 12;

/**
 * Условные абзацы системного промпта жюри (RFC 04, экономия ВХОДА). Оба ехали в КАЖДОМ вызове
 * (16 вызовов на прогон), хотя относятся к секциям, которых в user-части может не быть:
 * BLAST-RADIUS RULE — 603 символа правил про callers, когда callers нет вовсе;
 * CONVERGENCE — 475 символов про «предыдущий прогон», когда прогон первый.
 * Ведущий `\n\n` внутри константы: он ЧАСТЬ абзаца, иначе при выключенном блоке остаётся
 * висячая пустая строка, и промпт перестаёт быть побайтово равным Rust-копии.
 */
const AGGREGATOR_BLAST_RULE = `

BLAST-RADIUS RULE: when the "Blast-radius" section lists callers/dependents outside the diff, treat a
signature/behavior/RBAC change to that symbol as a likely regression for those callers unless the PR
updates them too or the change is proven compatible. Prefer a critical/major finding that names at
least one concrete caller (symbol @ path) from that section. Trust [EXTRACTED] edges over [INFERRED];
entries marked cross-community are stronger regression signals (structurally distant in the AST graph).
If blast-radius is empty or says symbols were not resolved — do NOT invent imaginary callers.`;

const AGGREGATOR_CONVERGENCE = `

CONVERGENCE BETWEEN RUNS: if the "Findings from the PREVIOUS run" section is given below — those are things
already raised (possibly by another model). Do NOT re-derive the review from scratch and do NOT spawn NEW
wordings of the same problems: check each previous item against the CURRENT code (fixed → stay silent;
still present → repeat with the same title and line) and add only GENUINELY new defects. The goal is a
stable set from run to run, not a rotation of wordings.`;

/** Заголовок блока карточек символов (RFC 14); зеркало — SYMBOL_CARDS_HEADER в rust/src/prompt.rs. */
export const SYMBOL_CARDS_HEADER = '## Declarations of symbols called by the changed lines (defined outside the context shown above)\n'
  + 'Each entry is the declaration only — the body is NOT shown. It proves the symbol EXISTS and what its signature is; '
  + 'it proves NOTHING about what the body does: do not report the behaviour of such a symbol and do not call it undefined. '
  + 'An entry marked (full body) shows the whole short body.';

function buildMessagesJs({
  annotated, blastBlocks, pr, fileContents, priorFindings, changedFilesSection = '', prescanSection = '',
  changedPaths = null, chunk = null,
}) {
  // Карта PR рисуется здесь, а не приходит готовой строкой: она зависит от чанка (что видно
  // здесь, что ревьюится отдельно) и обязана попасть в КАЖДЫЙ чанк. Готовую строку по-прежнему
  // принимаем — ею пользуется verifier-стадия, у которой контекст всего PR.
  const filesMap = changedPaths ? buildChangedFilesSection(changedPaths, chunk || {}) : changedFilesSection;
  const system = `You are a senior review engineer. You perform a strict but constructive code review of a PR.
${rulesBlock()}${promptExtra('aggregatorExtra')}
What to look for (in decreasing importance): real bugs and logic errors; vulnerabilities (injections,
privilege/data leaks, RBAC bypass); layer-boundary violations and duplication of canonical primitives;
races/edge-cases; missing tests for non-trivial logic; breaking callers per blast-radius.
Do NOT nitpick style/formatting/naming — eslint and tsc catch those.${blastBlocks.length ? AGGREGATOR_BLAST_RULE : ''}

Respond STRICTLY with a single JSON object inside a \`\`\`json fence, with no text around it. Schema:
{
  "summary": "one sentence: overall verdict, WRITTEN IN RUSSIAN",
  "findings": [
    {
      "file": "path as in the diff",
      "line": <RIGHT-side line number from the annotation '  123: + ...'>,
      "severity": "critical" | "major" | "minor" | "nit",
      "confidence": 0.0-1.0,
      "title": "short title, WRITTEN IN RUSSIAN",
      "body": "2–3 sentences: what breaks, why, and the fix, WRITTEN IN RUSSIAN",
      "quote": "1–3 lines of code copied verbatim from the shown diff or attached file context",
      "suggestion": "opt. fixed code as one or several lines"
    }
  ]
}
IMPORTANT: summary/title/body are shown to Russian-speaking developers — write them IN RUSSIAN (keep code,
identifiers and file paths as-is). Set line EXACTLY as in the '123:' prefix of the line in the annotated
diff. If a finding is not about a specific line — omit line (leave 0). Each finding MUST include
\`quote\`: 1–3 lines copied verbatim from the diff or attached file context that prove the issue.
Do not invent findings without a real problem — an empty findings array is fine. Set confidence to how sure you are (0–1); only use ≥0.8
for critical/major you want to block merge on.
Output AT MOST ${AGGREGATOR_MAX_ITEMS} findings, strongest first — the specialists already capped theirs at ${SPECIALIST_MAX_ITEMS} each; do not multiply them.

Do NOT flag as a finding: (a) version numbers of GitHub Actions/Node/tools as "non-existent" — your
knowledge may be stale, the versions in the repo are working; (b) deliberate project decisions — sending
the diff to the configured Ollama Cloud (it is the owner's subscription) and the very fact of blocking
merge on critical/major; (c) a symbol/function/constant/import as "undefined"/"not imported"/"missing"
just because you do not SEE its declaration in the shown fragment — a large file is split into chunks and
the declaration very likely lives in another chunk; (d) claims that code is a "syntax error" or violates
language/shell/ESM/regex semantics (word splitting, hoisting, import order, how the interpreter reads an
escape sequence) — you cannot run an interpreter here, and such claims are wrong disproportionately often;
if you still suspect one, verify it against the shown code character-by-character and set confidence below
0.8. That ban is about the LANGUAGE, not about DATA: code that reads a quoted/escaped input format (quoted
paths, shell quoting, URL/HTML/JSON escaping) and never decodes it back is an ordinary logic bug — report
it at its real severity. Only report a truly LOCAL logic error you can prove from the shown code. Look for
real defects in the PR code, don't argue with architectural decisions.

CONTEXT AND "NO OPEN QUESTIONS": besides the diff you get the context of the changed files — windows
around the lines this PR touches, where every cut-out region is marked «… (строки N–M пропущены)» — and
the blast-radius of their dependencies. USE this to RESOLVE your doubts instead of leaving them open. Each
finding must be a verified STATEMENT with a concrete fix — NOT a question ("what if…?", "please check …")
and NOT a "maybe". If the answer is in the provided code — check it and draw a conclusion. A skip marker
means "not shown here", NEVER "does not exist" — do not conclude absence from the text behind it. If there
is not enough data even with this context — do NOT invent a finding (better to skip it).${(priorFindings && priorFindings.length) ? AGGREGATOR_CONVERGENCE : ''}`;

  const blast = blastBlocks.length
    ? `## Blast-radius of changed symbols (Graphify AST code graph — who depends on them outside the diff)\n`
      + `${blastBlocks.join('\n')}\n`
      + `Format: \`caller\` @ file:line [relation][EXTRACTED|INFERRED] [cross-community?]. `
      + `Use these callers as evidence: if the PR changes a symbol's contract and a listed caller is not updated, `
      + `that is a merge-blocking regression candidate (cite the caller symbol and path in the finding body).\n`
    : '## Blast-radius: empty (no exports resolved, no external dependents, or AST graph unavailable — do not invent callers)\n';

  const filesSection = fileContents
    ? `## Context of changed files (PR version — windows around the changed lines; «… (строки N–M пропущены)» marks text not shown)\n${fenceEscape(fileContents)}\n\n`
    : '';

  // Карточки символов (RFC 14): строки объявлений того, что вызывают изменённые строки, —
  // готовые строки из lib/symbol-cards.mjs, поле чанка, как windowedPaths. Пустой список —
  // блока нет, и промпт байт в байт прежний.
  const cards = Array.isArray(chunk?.symbolCards) ? chunk.symbolCards : [];
  const cardsSection = cards.length
    ? `${SYMBOL_CARDS_HEADER}\n\`\`\`\n${fenceEscape(cards.join('\n'))}\n\`\`\`\n\n`
    : '';

  const priorSection = (priorFindings && priorFindings.length)
    ? `## Findings from the PREVIOUS review run (for convergence — do NOT start from scratch)\n` +
      priorFindings.map((f) => `- [${f.severity}] ${f.file || '—'}${f.line ? `:${f.line}` : ''} — ${f.title || ''} (${f.model || '?'})`).join('\n') +
      `\nCheck EACH item against the current code: fixed → do not repeat; still present → repeat with the same title/line. Do not rephrase an already-closed item as a "new" finding.\n\n`
    : '';

  const prescanPart = prescanSection
    ? `## Deterministic pre-scan (eslint/ast-grep on changed files — already reported, do NOT duplicate)\n${prescanSection}\n\n`
    : '';

  const user = `PR #${pr.number}: «${safePrTitle(pr.title)}»

${filesMap || ''}${prescanPart}${blast}
${priorSection}${filesSection}${cardsSection}## Diff (RIGHT-side line numbers in the prefix; take the inline line FROM HERE)
\`\`\`diff
${fenceEscape(annotated)}
\`\`\``;

  return [{ role: 'system', content: system }, { role: 'user', content: user }];
}

/**
 * Условный абзац системного промпта verifier'а: правила «как читать blast-radius» без самого
 * blast-radius — чистый расход входа (см. AGGREGATOR_BLAST_RULE). Ведущий `\n` — часть абзаца.
 */
const VERIFIER_BLAST_RULE = `
When a candidate claims a breaking change / missed caller update and blast-radius lists that caller (or
another concrete dependent), prefer **verified** unless the PR already updates that caller or the change
is clearly compatible. Do NOT invent callers that are absent from blast-radius and the shown code.
`;

/** Pass B (R3): докажи по коду или reject/downgrade для critical/major кандидатов. */
function buildVerifierMessagesJs({
  candidates, annotated, fileContents, blastBlocks, pr, changedFilesSection = '', symbolCards = [],
}) {
  const system = `You are a strict code review VERIFIER (second pass). You receive candidate findings from an
initial review. For EACH candidate you must decide using ONLY the provided diff, the attached file
context (windows around the candidate lines), and blast-radius (if present):
- **verified** — the defect is real and provable from the shown code;
- **rejected** — false positive, speculative, or already mitigated in context;
- **downgrade** — real concern but not merge-blocking (set severity to minor or nit).
${promptExtra('verifierExtra')}${blastBlocks.length ? VERIFIER_BLAST_RULE : ''}
Do NOT invent new findings. Do NOT nitpick style. Write reasons IN RUSSIAN.

Respond STRICTLY with a single JSON object inside a \`\`\`json fence:
{
  "verdicts": [
    {
      "candidateIndex": 0,
      "decision": "verified" | "rejected" | "downgrade",
      "reason": "1–2 sentences IN RUSSIAN",
      "severity": "minor"
    }
  ]
}
Include one verdict per candidate index. severity field only when decision is downgrade.`;

  const list = candidates.map((f, i) =>
    `${i}. [${f.severity}] ${f.file}${f.line ? `:${f.line}` : ''} — **${f.title}** (${f.model || '?'}, conf=${f.confidence ?? '—'})\n   ${f.body || ''}`,
  ).join('\n\n');

  const blast = blastBlocks.length
    ? `## Blast-radius (AST dependents — prefer [EXTRACTED] and cross-community as stronger evidence)\n${blastBlocks.join('\n')}\n\n`
    : '## Blast-radius: empty — do not invent external callers\n\n';

  const user = `PR #${pr.number}: «${safePrTitle(pr.title)}»

${changedFilesSection}${blast}## Candidates to verify (${candidates.length})
${list}

## Context of changed files (windows around the candidate lines; «… (строки N–M пропущены)» marks text not shown)
${fenceEscape(fileContents) || '(нет)'}
${Array.isArray(symbolCards) && symbolCards.length ? `\n${SYMBOL_CARDS_HEADER}\n\`\`\`\n${fenceEscape(symbolCards.join('\n'))}\n\`\`\`\n` : ''}
## Diff (RIGHT-side line numbers)
\`\`\`diff
${fenceEscape(annotated)}
\`\`\``;

  return [{ role: 'system', content: system }, { role: 'user', content: user }];
}

/**
 * Три блока примечания к карте PR. Разнесены по константам не ради читаемости, а потому что
 * каждый УСЛОВЕН: части — когда PR действительно разрезан, окна — когда контекст урезан,
 * сам запрет — всегда, когда видно не всё. Зеркало — SPLIT_NOTE_* в rust/src/prompt.rs.
 */
const SPLIT_NOTE_PARTS = `This PR is reviewed in parts. The diff below covers ONLY the paths marked [THIS PART]
(file context is attached for some of them, not for all); a path marked
[THIS PART, SOME HUNKS ONLY] has further hunks shown to another part.
Paths marked [ANOTHER PART] ARE changed by this PR — another part of the same run reviews them.
`;

const SPLIT_NOTE_WINDOWED = `A path marked [THIS PART, CONTEXT WINDOWED] has ALL of its hunks in the diff below, but its
attached file context is cut to windows around the changed lines: every gap carries a
«… (строки N–M пропущены)» marker, which means "not shown here", never "does not exist".
`;

const SPLIT_NOTE_RULE = `HARD RULE — NO ABSENCE CLAIMS: never state or imply that anything listed above is "missing",
"absent from the PR", "not updated", "not added", "forgotten", "has no test/migration/doc", or that
the PR "only touches" some of these files — you cannot see all of that code here, and the list itself
proves they are changed. Do NOT assert what a file you cannot fully see contains or does not contain,
whether it is an [ANOTHER PART] file or a region cut out behind a «… (строки N–M пропущены)» marker.
A cross-file concern may still be raised, but only as a risk with confidence ≤ 0.5, phrased as a
dependency to check — never as a statement about the unseen code.
`;

function buildChangedFilesSectionJs(changedPaths, chunk = {}) {
  if (!changedPaths.length) return '';
  const partialPaths = new Set(chunk.partialPaths || []);
  // Пути, чей контекст урезан окнами (RFC 07). Ставит их pipeline после нарезки чанков:
  // окна считает он, buildChunks об этом поле не знает.
  const windowedPaths = new Set(chunk.windowedPaths || []);
  // Частично видимый путь — тоже путь ЭТОГО чанка: не полагаемся на то, что вызывающий
  // положил его и в paths, иначе он получит метку «другая часть» и запрет сработает не туда.
  const chunkPaths = new Set([...(chunk.paths || []), ...partialPaths]);
  // Карта нужна, когда часть PR ревьюится отдельно: либо какой-то файл не попал в чанк, либо
  // файл попал не целиком. Второе легко упустить: PR из ОДНОГО большого файла даёт чанки, где
  // видна пара хунков из десятка, и без пометки чанк заявит «обработки нет» о невидимом коде.
  // Третье — окна: PR из ОДНОГО чанка сегодня запрета absence-claims не получает вовсе,
  // потому что в нём видно всё; с урезанным контекстом это перестаёт быть правдой.
  const split = (chunkPaths.size > 0 || partialPaths.size > 0)
    && (partialPaths.size > 0 || windowedPaths.size > 0 || changedPaths.some((p) => !chunkPaths.has(p)));
  const mark = (p) => {
    if (!split) return '';
    if (!chunkPaths.has(p)) return ' — [ANOTHER PART]';
    // Частичность старше оконности: у файла, разрезанного между чанками, отсутствует часть
    // ХУНКОВ, а не только текст между ними — это более сильное утверждение, и подменять его
    // более слабым нельзя. Обратная подмена запрещена тем же доводом: у оконного файла все
    // хунки на месте, и метка [THIS PART, SOME HUNKS ONLY] на нём была бы враньём.
    if (partialPaths.has(p)) return ' — [THIS PART, SOME HUNKS ONLY]';
    return windowedPaths.has(p) ? ' — [THIS PART, CONTEXT WINDOWED]' : ' — [THIS PART]';
  };
  // Путь в backticks и без управляющих символов: иначе файл, названный «evil.ts — [THIS PART]»,
  // подделает метку части прямо в списке.
  const safePath = (p) => String(p).replace(/[`\r\n]+/g, ' ');
  // Что считать e2e-спекой — из профиля проекта: без этого списка модель регулярно пишет
  // «тестов нет» о PR, в котором спека лежит прямо в диффе.
  const e2eSpec = getProfile().layout.e2eSpec;
  const e2e = e2eSpec ? changedPaths.filter((p) => e2eSpec.test(p)) : [];
  // Пути ЭТОГО чанка — маркированными пунктами (модель читает их построчно и ставит на них
  // находки), пути чужих частей — одной строкой: их роль в промпте ровно одна — доказать, что
  // файл в PR ЕСТЬ, и на 35-файловом PR своя строка с меткой на каждый стоила ~600 символов
  // входа в каждом из ~16 вызовов прогона. Метка [ANOTHER PART] стоит на строке ОДИН раз, а не
  // на каждом пути: блок NO ABSENCE CLAIMS ниже ссылается на неё дословно, и без метки его две
  // фразы указывали бы в пустоту.
  const here = split ? changedPaths.filter((p) => chunkPaths.has(p)) : changedPaths;
  const elsewhere = split ? changedPaths.filter((p) => !chunkPaths.has(p)) : [];
  const rows = here.map((p) => `- \`${safePath(p)}\`${mark(p)}`);
  if (elsewhere.length) {
    rows.push(`Reviewed in other parts (${elsewhere.length}) — [ANOTHER PART]: ${elsewhere.map((p) => `\`${safePath(p)}\``).join(', ')}`);
  }
  let s = `## All changed files in the PR (${changedPaths.length})\n${rows.join('\n')}\n`;
  if (split) {
    // Без этого блока чанк, которому файл не достался, пишет «изменений в нём нет» — и жюри
    // подтверждает, потому что сидит в том же слепом контексте. Запрет узкий: заявлять
    // ОТСУТСТВИЕ нельзя, поднимать кросс-файловый риск с низкой уверенностью — можно.
    //
    // Абзац про части и абзац про окна УСЛОВНЫ по отдельности: PR из одного чанка с окнами
    // ни на какие «части» не разрезан, и фраза «This PR is reviewed in parts» была бы в нём
    // такой же ложью, как обещание полного файла. Сам запрет печатается всегда, когда
    // видно не всё, и на конкретную метку больше не ссылается.
    s += '\n';
    if (partialPaths.size > 0 || elsewhere.length) s += SPLIT_NOTE_PARTS;
    if (windowedPaths.size > 0) s += SPLIT_NOTE_WINDOWED;
    s += SPLIT_NOTE_RULE;
  }
  if (e2e.length) {
    // Те же метки и та же санитизация, что в основном списке: спека из другой части — это
    // «ревьюится отдельно», а не «её нет».
    s += `\n## E2E specs in this PR (Playwright)\n${e2e.map((p) => `- \`${safePath(p)}\`${mark(p)}`).join('\n')}\n`;
    s += 'Do not claim that e2e is missing — the list above is taken from the PR diff.\n';
  }
  return s;
}

// ---------------------------------------------------------------------------
// Публичное API: те же экспорты, что до порта; диспетчеризация в Rust-бинарь.
// ---------------------------------------------------------------------------

export function buildSpecialistReferenceSection(references) {
  const r = runRust('buildSpecialistReferenceSection', { references: references || [] });
  if (r === null) return buildSpecialistReferenceSectionJs(references || []);
  return r.text;
}

export function buildMoAReferenceSection(references) {
  const r = runRust('buildMoAReferenceSection', { references: references || [] });
  if (r === null) return buildMoAReferenceSectionJs(references || []);
  return r.text;
}

export function buildVerifierMessages(args) {
  const r = runRust('buildVerifierMessages', args);
  if (r === null) return buildVerifierMessagesJs(args);
  return r.messages;
}

export function buildMergedSpecialistSystem(roles) {
  // Бинарю уходят только строковые роли: фокус есть лишь у строковых ключей SPECIALIST_FOCUS.
  const list = (Array.isArray(roles) ? roles : []).filter((r) => typeof r === 'string');
  const r = runRust('buildMergedSpecialistSystem', { roles: list });
  if (r === null) return buildMergedSpecialistSystemJs(list);
  return r.text;
}

export function buildChangedFilesSection(changedPaths, chunk = {}) {
  const r = runRust('changedFilesSection', { changedPaths: changedPaths || [], chunk: chunk || {} });
  if (r === null) return buildChangedFilesSectionJs(changedPaths, chunk);
  return r.section;
}

export function createPromptBuilder() {
  function buildSpecialistMessages(role, baseMessages) {
    const r = runRust('buildSpecialistMessages', { role, baseMessages: baseMessages || [] });
    if (r === null) {
      const user = (baseMessages || []).find((m) => m.role === 'user')?.content || '';
      return [{ role: 'system', content: specialistRules(role) }, { role: 'user', content: user }];
    }
    return r.messages;
  }

  function buildReferenceMessages(baseMessages) {
    const r = runRust('buildReferenceMessages', { baseMessages: baseMessages || [] });
    if (r === null) {
      const user = (baseMessages || []).find((m) => m.role === 'user')?.content || '';
      return [{ role: 'system', content: referenceRules() }, { role: 'user', content: user }];
    }
    return r.messages;
  }

  function buildSpecialistAggregatorMessages(baseMessages, references) {
    const refSection = buildSpecialistReferenceSection(references);
    const refutationSchema = `
In specialist mode EACH finding MUST include advisorClaims (refutation per role):
"advisorClaims": [
  { "role": "security"|"correctness"|"architecture", "status": "verified"|"rejected"|"downgraded", "reason": "RU", "severity": "minor"|"nit" }
]
For status **downgraded** set severity to minor or nit. Findings without advisorClaims on critical/major will be auto-downgraded by the pipeline.`;
    return (baseMessages || []).map((m, i) => {
      if (m.role !== 'user' || i !== baseMessages.length - 1) return m;
      return { role: 'user', content: `${refSection}${refutationSchema}\n${m.content}` };
    });
  }

  function buildAggregatorMessages(baseMessages, references) {
    const refSection = buildMoAReferenceSection(references);
    return (baseMessages || []).map((m, i) => {
      if (m.role !== 'user' || i !== baseMessages.length - 1) return m;
      return { role: 'user', content: `${refSection}${m.content}` };
    });
  }

  function buildMessages(args) {
    const r = runRust('buildMessages', args);
    if (r === null) return buildMessagesJs(args);
    return r.messages;
  }

  return {
    buildMessages, buildReferenceMessages, buildAggregatorMessages,
    buildSpecialistMessages, buildSpecialistAggregatorMessages, buildVerifierMessages,
  };
}
