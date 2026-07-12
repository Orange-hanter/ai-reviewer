import { REPO_RULES } from './constants.mjs';

/** Не даём содержимому diff/file ломать markdown fence в промпте. */
function fenceEscape(text) {
  return String(text ?? '').replace(/```/g, '``\u200b`');
}

const REFERENCE_RULES = `You are a code review advisor (reference model in a Mixture-of-Agents pipeline).
${REPO_RULES}

Analyze the PR diff and full file context. List ONLY real defects: bugs, security/RBAC issues,
layer-boundary violations, missing tests for non-trivial logic. Do NOT nitpick style (eslint/tsc).
Write IN RUSSIAN as concise bullet points:
- [severity] \`file:line\` — title: explanation (severity = critical|major|minor|nit)
Do NOT output JSON — a separate aggregator will synthesize structured findings from your advice.
If nothing serious — say «серьёзных замечаний нет».`;

const SPECIALIST_FOCUS = {
  security: `Focus ONLY on security: injections, auth/RBAC bypass, IDOR, data leaks, secrets in code,
unsafe deserialization, SSRF, path traversal. Ignore style and non-security logic nitpicks.`,
  correctness: `Focus ONLY on correctness: logic bugs, races, null/edge cases, wrong rounding (must use shared/round.ts),
off-by-one, broken error handling, regressions visible in the diff/context. Ignore security-only and architecture lectures.`,
  architecture: `Focus ONLY on architecture/layers: shared↔server↔client boundaries, forbidden imports,
duplication of canonical primitives (see REPO_RULES), blast-radius breaking changes. Ignore pure security and micro-bugs.`,
};

function specialistRules(role) {
  const focus = SPECIALIST_FOCUS[role] || '';
  return `You are a SPECIALIST code review advisor — role: **${role}** (Mixture-of-Critics pipeline).
${REPO_RULES}

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
    return [{ role: 'system', content: REFERENCE_RULES }, { role: 'user', content: user }];
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
  }) {
    const system = `You are a senior review engineer. You perform a strict but constructive code review of a PR.
${REPO_RULES}

What to look for (in decreasing importance): real bugs and logic errors; vulnerabilities (injections,
privilege/data leaks, RBAC bypass); layer-boundary violations and duplication of canonical primitives;
races/edge-cases; missing tests for non-trivial logic; breaking callers per blast-radius.
Do NOT nitpick style/formatting/naming — eslint and tsc catch those.

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
      "suggestion": "opt. fixed code as one or several lines"
    }
  ]
}
IMPORTANT: summary/title/body are shown to Russian-speaking developers — write them IN RUSSIAN (keep code,
identifiers and file paths as-is). Set line EXACTLY as in the '123:' prefix of the line in the annotated
diff. If a finding is not about a specific line — omit line (leave 0). Do not invent findings without a
real problem — an empty findings array is fine. Set confidence to how sure you are (0–1); only use ≥0.8
for critical/major you want to block merge on.

Do NOT flag as a finding: (a) version numbers of GitHub Actions/Node/tools as "non-existent" — your
knowledge may be stale, the versions in the repo are working; (b) deliberate project decisions — sending
the diff to the configured Ollama Cloud (it is the owner's subscription) and the very fact of blocking
merge on critical/major; (c) a symbol/function/constant/import as "undefined"/"not imported"/"missing"
just because you do not SEE its declaration in the shown fragment — a large file is split into chunks and
the declaration very likely lives in another chunk. Only report a truly LOCAL logic error you can prove
from the shown code. Look for real defects in the PR code, don't argue with architectural decisions.

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
      ? `## Blast-radius of changed symbols (Graphify, code graph — who depends on them outside the diff)\n${blastBlocks.join('\n')}\n`
      : '## Blast-radius: no significant external dependencies found (or symbols are ambiguous)\n';

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

    const user = `PR #${pr.number}: «${pr.title}»

${changedFilesSection || ''}${prescanPart}${blast}
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
initial review. For EACH candidate you must decide using ONLY the provided diff and full file context:
- **verified** — the defect is real and provable from the shown code;
- **rejected** — false positive, speculative, or already mitigated in context;
- **downgrade** — real concern but not merge-blocking (set severity to minor or nit).

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
    ? `## Blast-radius\n${blastBlocks.join('\n')}\n\n`
    : '';

  const user = `PR #${pr.number}: «${pr.title}»

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

export function buildChangedFilesSection(changedPaths) {
  if (!changedPaths.length) return '';
  const e2e = changedPaths.filter((p) => /^app\/e2e\/.*\.spec\.ts$/.test(p));
  let s = `## All changed files in the PR (${changedPaths.length})\n${changedPaths.map((p) => `- ${p}`).join('\n')}\n`;
  if (e2e.length) {
    s += `\n## E2E specs in this PR (Playwright)\n${e2e.map((p) => `- ${p}`).join('\n')}\n`;
    s += 'Do not claim that e2e is missing — the list above is taken from the PR diff.\n';
  }
  return s;
}
