import { repoRules } from './constants.mjs';
import { getProfile } from './profile.mjs';

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
Analyze the PR diff and full file context. List ONLY real defects: bugs, security/RBAC issues,
layer-boundary violations, missing tests for non-trivial logic. Do NOT nitpick style (eslint/tsc).
Write IN RUSSIAN as concise bullet points:
- [severity] \`file:line\` — title: explanation (severity = critical|major|minor|nit)
Do NOT output JSON — a separate aggregator will synthesize structured findings from your advice.
If nothing serious — say «серьёзных замечаний нет».`;
}

const SPECIALIST_FOCUS = {
  security: `Focus ONLY on security: injections, auth/RBAC bypass, IDOR, data leaks, secrets in code,
unsafe deserialization, SSRF, path traversal. Ignore style and non-security logic nitpicks.`,
  correctness: `Focus ONLY on correctness: logic bugs, races, null/edge cases, wrong rounding of money/quantities,
off-by-one, broken error handling, regressions visible in the diff/context. Ignore security-only and architecture lectures.`,
  architecture: `Focus ONLY on architecture/layers: module boundaries of this project, forbidden imports,
duplication of canonical primitives (see the project rules above), blast-radius breaking changes. Ignore pure security and micro-bugs.`,
};

function specialistRules(role) {
  const focus = SPECIALIST_FOCUS[role] || '';
  return `You are a SPECIALIST code review advisor — role: **${role}** (Mixture-of-Critics pipeline).
${rulesBlock()}${promptExtra('specialistExtra')}
${focus}

Analyze the PR diff and full file context. List ONLY issues in YOUR specialty. Write IN RUSSIAN as bullets:
- [severity] \`file:line\` — title: explanation (severity = critical|major|minor|nit)
Do NOT output JSON — the jury aggregator will refute/verify each claim.
If nothing in your specialty — say «в моей области серьёзных замечаний нет».`;
}

export function buildSpecialistReferenceSection(references) {
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

export function buildMoAReferenceSection(references) {
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

export function createPromptBuilder() {
  function buildSpecialistMessages(role, baseMessages) {
    const user = baseMessages.find((m) => m.role === 'user')?.content || '';
    return [{ role: 'system', content: specialistRules(role) }, { role: 'user', content: user }];
  }

  function buildReferenceMessages(baseMessages) {
    const user = baseMessages.find((m) => m.role === 'user')?.content || '';
    return [{ role: 'system', content: referenceRules() }, { role: 'user', content: user }];
  }

  function buildSpecialistAggregatorMessages(baseMessages, references) {
    const refSection = buildSpecialistReferenceSection(references);
    const refutationSchema = `
In specialist mode EACH finding MUST include advisorClaims (refutation per role):
"advisorClaims": [
  { "role": "security"|"correctness"|"architecture", "status": "verified"|"rejected"|"downgraded", "reason": "RU", "severity": "minor"|"nit" }
]
For status **downgraded** set severity to minor or nit. Findings without advisorClaims on critical/major will be auto-downgraded by the pipeline.`;
    return baseMessages.map((m, i) => {
      if (m.role !== 'user' || i !== baseMessages.length - 1) return m;
      return { role: 'user', content: `${refSection}${refutationSchema}\n${m.content}` };
    });
  }

  function buildAggregatorMessages(baseMessages, references) {
    const refSection = buildMoAReferenceSection(references);
    return baseMessages.map((m, i) => {
      if (m.role !== 'user' || i !== baseMessages.length - 1) return m;
      return { role: 'user', content: `${refSection}${m.content}` };
    });
  }

  function buildMessages({
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
Do NOT nitpick style/formatting/naming — eslint and tsc catch those.

BLAST-RADIUS RULE: when the "Blast-radius" section lists callers/dependents outside the diff, treat a
signature/behavior/RBAC change to that symbol as a likely regression for those callers unless the PR
updates them too or the change is proven compatible. Prefer a critical/major finding that names at
least one concrete caller (symbol @ path) from that section. Trust [EXTRACTED] edges over [INFERRED];
entries marked cross-community are stronger regression signals (structurally distant in the AST graph).
If blast-radius is empty or says symbols were not resolved — do NOT invent imaginary callers.

Respond STRICTLY with a single JSON object inside a \`\`\`json fence, with no text around it. Schema:
{
  "summary": "1–3 sentences: overall verdict, WRITTEN IN RUSSIAN",
  "verdict": "approve" | "comment" | "changes_requested",
  "findings": [
    {
      "file": "path as in the diff",
      "line": <RIGHT-side line number from the annotation '  123: + ...'>,
      "severity": "critical" | "major" | "minor" | "nit",
      "confidence": 0.0-1.0,
      "title": "short title, WRITTEN IN RUSSIAN",
      "body": "explanation of the problem and how to fix it, WRITTEN IN RUSSIAN",
      "quote": "1–3 lines of code copied verbatim from the shown diff or full file context",
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

Do NOT flag as a finding: (a) version numbers of GitHub Actions/Node/tools as "non-existent" — your
knowledge may be stale, the versions in the repo are working; (b) deliberate project decisions — sending
the diff to the configured Ollama Cloud (it is the owner's subscription) and the very fact of blocking
merge on critical/major; (c) a symbol/function/constant/import as "undefined"/"not imported"/"missing"
just because you do not SEE its declaration in the shown fragment — a large file is split into chunks and
the declaration very likely lives in another chunk; (d) claims that code is a "syntax error" or violates
language/shell/ESM/regex semantics (word splitting, hoisting, import order, escape rules) — you cannot run
an interpreter here, and such claims are wrong disproportionately often; if you still suspect one, verify
it against the shown code character-by-character and set confidence below 0.8. Only report a truly LOCAL
logic error you can prove from the shown code. Look for real defects in the PR code, don't argue with
architectural decisions.

CONTEXT AND "NO OPEN QUESTIONS": you have the FULL content of the changed files (not only the diff) and
the blast-radius of their dependencies. USE this to RESOLVE your doubts instead of leaving them open. Each
finding must be a verified STATEMENT with a concrete fix — NOT a question ("what if…?", "please check …")
and NOT a "maybe". If the answer is in the provided code — check it and draw a conclusion. If there is not
enough data even with this context — do NOT invent a finding (better to skip it).

CONVERGENCE BETWEEN RUNS: if the "Findings from the PREVIOUS run" section is given below — those are things
already raised (possibly by another model). Do NOT re-derive the review from scratch and do NOT spawn NEW
wordings of the same problems: check each previous item against the CURRENT code (fixed → stay silent;
still present → repeat with the same title and line) and add only GENUINELY new defects. The goal is a
stable set from run to run, not a rotation of wordings.`;

    const blast = blastBlocks.length
      ? `## Blast-radius of changed symbols (Graphify AST code graph — who depends on them outside the diff)\n`
        + `${blastBlocks.join('\n')}\n`
        + `Format: \`caller\` @ file:line [relation][EXTRACTED|INFERRED] [cross-community?]. `
        + `Use these callers as evidence: if the PR changes a symbol's contract and a listed caller is not updated, `
        + `that is a merge-blocking regression candidate (cite the caller symbol and path in the finding body).\n`
      : '## Blast-radius: empty (no exports resolved, no external dependents, or AST graph unavailable — do not invent callers)\n';

    const filesSection = fileContents
      ? `## Full content of changed files (PR version — context beyond the hunks)\n${fenceEscape(fileContents)}\n\n`
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
${priorSection}${filesSection}## Diff (RIGHT-side line numbers in the prefix; take the inline line FROM HERE)
\`\`\`diff
${fenceEscape(annotated)}
\`\`\``;

    return [{ role: 'system', content: system }, { role: 'user', content: user }];
  }

  return {
    buildMessages, buildReferenceMessages, buildAggregatorMessages,
    buildSpecialistMessages, buildSpecialistAggregatorMessages, buildVerifierMessages,
  };
}

/** Pass B (R3): докажи по коду или reject/downgrade для critical/major кандидатов. */
export function buildVerifierMessages({
  candidates, annotated, fileContents, blastBlocks, pr, changedFilesSection = '',
}) {
  const system = `You are a strict code review VERIFIER (second pass). You receive candidate findings from an
initial review. For EACH candidate you must decide using ONLY the provided diff, full file context, and
blast-radius (if present):
- **verified** — the defect is real and provable from the shown code;
- **rejected** — false positive, speculative, or already mitigated in context;
- **downgrade** — real concern but not merge-blocking (set severity to minor or nit).
${promptExtra('verifierExtra')}
When a candidate claims a breaking change / missed caller update and blast-radius lists that caller (or
another concrete dependent), prefer **verified** unless the PR already updates that caller or the change
is clearly compatible. Do NOT invent callers that are absent from blast-radius and the shown code.

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

## Full content of changed files
${fenceEscape(fileContents) || '(нет)'}

## Diff (RIGHT-side line numbers)
\`\`\`diff
${fenceEscape(annotated)}
\`\`\``;

  return [{ role: 'system', content: system }, { role: 'user', content: user }];
}

export function buildChangedFilesSection(changedPaths, chunk = {}) {
  if (!changedPaths.length) return '';
  const partialPaths = new Set(chunk.partialPaths || []);
  // Частично видимый путь — тоже путь ЭТОГО чанка: не полагаемся на то, что вызывающий
  // положил его и в paths, иначе он получит метку «другая часть» и запрет сработает не туда.
  const chunkPaths = new Set([...(chunk.paths || []), ...partialPaths]);
  // Карта нужна, когда часть PR ревьюится отдельно: либо какой-то файл не попал в чанк, либо
  // файл попал не целиком. Второе легко упустить: PR из ОДНОГО большого файла даёт чанки, где
  // видна пара хунков из десятка, и без пометки чанк заявит «обработки нет» о невидимом коде.
  const split = (chunkPaths.size > 0 || partialPaths.size > 0)
    && (partialPaths.size > 0 || changedPaths.some((p) => !chunkPaths.has(p)));
  const mark = (p) => {
    if (!split) return '';
    if (!chunkPaths.has(p)) return ' — [ANOTHER PART]';
    return partialPaths.has(p) ? ' — [THIS PART, SOME HUNKS ONLY]' : ' — [THIS PART]';
  };
  // Путь в backticks и без управляющих символов: иначе файл, названный «evil.ts — [THIS PART]»,
  // подделает метку части прямо в списке.
  const safePath = (p) => String(p).replace(/[`\r\n]+/g, ' ');
  // Что считать e2e-спекой — из профиля проекта: без этого списка модель регулярно пишет
  // «тестов нет» о PR, в котором спека лежит прямо в диффе.
  const e2eSpec = getProfile().layout.e2eSpec;
  const e2e = e2eSpec ? changedPaths.filter((p) => e2eSpec.test(p)) : [];
  let s = `## All changed files in the PR (${changedPaths.length})\n${changedPaths.map((p) => `- \`${safePath(p)}\`${mark(p)}`).join('\n')}\n`;
  if (split) {
    // Без этого блока чанк, которому файл не достался, пишет «изменений в нём нет» — и жюри
    // подтверждает, потому что сидит в том же слепом контексте. Запрет узкий: заявлять
    // ОТСУТСТВИЕ нельзя, поднимать кросс-файловый риск с низкой уверенностью — можно.
    s += `\nThis PR is reviewed in parts. The diff below covers ONLY the paths marked [THIS PART]
(full file content is attached for some of them, not for all); a path marked
[THIS PART, SOME HUNKS ONLY] has further hunks shown to another part.
Paths marked [ANOTHER PART] ARE changed by this PR — another part of the same run reviews them.
HARD RULE — NO ABSENCE CLAIMS: never state or imply that anything listed above is "missing",
"absent from the PR", "not updated", "not added", "forgotten", "has no test/migration/doc", or that
the PR "only touches" some of these files — you cannot see those files here, and the list itself
proves they are changed. Do NOT assert what an [ANOTHER PART] file contains or does not contain.
A cross-file concern may still be raised, but only as a risk with confidence ≤ 0.5, phrased as a
dependency to check — never as a statement about the unseen file.\n`;
  }
  if (e2e.length) {
    // Те же метки и та же санитизация, что в основном списке: спека из другой части — это
    // «ревьюится отдельно», а не «её нет».
    s += `\n## E2E specs in this PR (Playwright)\n${e2e.map((p) => `- \`${safePath(p)}\`${mark(p)}`).join('\n')}\n`;
    s += 'Do not claim that e2e is missing — the list above is taken from the PR diff.\n';
  }
  return s;
}
