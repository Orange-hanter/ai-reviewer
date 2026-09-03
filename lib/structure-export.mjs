import { mkdir, writeFile } from 'node:fs/promises';
import { dirname } from 'node:path';
import { resolveContainedPath } from './util.mjs';

function percent(ratio) {
  return ratio == null ? 'n/a' : `${Math.round(ratio * 1000) / 10}%`;
}

// текст, попадающий в документ, приходит из репозитория и из obligations cod-doc:
// перевод строки в нём не должен становиться заголовком, а обратная кавычка — рвать разметку
function esc(value) {
  return String(value ?? '')
    .replaceAll('\\', '\\\\')
    .replaceAll('|', '\\|')
    .replaceAll('`', '\\`')
    .replaceAll(/[\n\r]/g, ' ');
}

// значение внутри code span: длина ограды берётся по самой длинной серии кавычек в значении
function code(value) {
  const text = String(value ?? '').replaceAll(/[\n\r]/g, ' ');
  const longest = (text.match(/`+/g) || []).reduce((max, run) => Math.max(max, run.length), 0);
  const fence = '`'.repeat(longest + 1);
  return longest ? `${fence} ${text} ${fence}` : `${fence}${text}${fence}`;
}

// документ — проекция; молча терять из неё строки нельзя, иначе он врёт о полноте
function truncationNote(items, max, what) {
  return items.length > max
    ? ['', `_Showing ${max} of ${items.length} ${what}; the JSON assessment holds the rest._`, '']
    : [];
}

export function buildStructureMarkdown(result, {
  owner = 'ai-reviewer',
  generatedAt = result.facts.generatedAt,
} = {}) {
  const facts = result.facts;
  const assessment = result.assessment;
  const quality = assessment.assessments.dataQuality;
  const totals = assessment.assessments.codeCoverage.totals;
  const hints = assessment.hints || [];
  const scenarios = assessment.assessments.contractScenarios || [];
  const deps = facts.facts.dependencies || [];
  const entities = new Map(facts.facts.entities.map((entity) => [entity.observedId, entity]));
  const lines = [
    '---',
    'type: architecture',
    'status: draft',
    `owner: ${owner}`,
    'source_of_truth: false',
    `last_updated: ${String(generatedAt).slice(0, 10)}`,
    `head_sha: ${facts.provenance.headSha}`,
    `facts_fingerprint: ${facts.fingerprint}`,
    `assessment_fingerprint: ${assessment.fingerprint}`,
    'generated_by: ai-reviewer',
    '---',
    '',
    '# Code structure, contracts and test scenarios',
    '',
    '## Data quality',
    '',
    `- Head: \`${facts.provenance.headSha}\` on \`${facts.provenance.branchRef}\``,
    `- Graph: **${facts.provenance.graphStatus}**`,
    `- Temporal alignment: **${assessment.temporalAlignment}**`,
    `- Entities: **${facts.facts.entities.length}**, dependencies: **${deps.length}**`,
    `- Function joins unresolved: **${percent(quality.unresolvedJoinRate)}**`,
    `- Truncated: **${facts.provenance.truncated ? 'yes' : 'no'}**`,
    '',
  ];
  if (totals) {
    lines.push(
      '## Coverage',
      '',
      `- Lines: **${totals.lines.hit}/${totals.lines.found}**`,
      `- Functions: **${totals.functions.hit}/${totals.functions.found}**`,
      `- Branches: **${totals.branches.hit}/${totals.branches.found}**`,
      '',
    );
  }
  lines.push('## Priority hints', '');
  if (!hints.length) lines.push('No evidence-backed hints for this snapshot.', '');
  else {
    for (const hint of hints.slice(0, 50)) {
      lines.push(
        `### ${esc(String(hint.priority).toUpperCase())} — ${esc(hint.ruleId)}`,
        '',
        esc(hint.remediation),
        '',
        `Subjects: ${hint.subjectRefs.map((ref) => code(ref)).join(', ')}`,
        `Evidence: ${hint.evidence.length ? hint.evidence.map((ref) => code(ref)).join(', ') : 'none'}`,
        `Missing: ${hint.missingEvidence.length ? hint.missingEvidence.map((ref) => esc(ref)).join(', ') : 'none'}`,
        '',
      );
    }
    lines.push(...truncationNote(hints, 50, 'hints'));
  }
  lines.push(
    '## Contract scenarios',
    '',
    '| Obligation | Kind | Status | Contract | Reason |',
    '|---|---|---|---|---|',
  );
  for (const scenario of scenarios.slice(0, 200)) {
    lines.push(`| ${esc(scenario.obligationRef)} | ${esc(scenario.kind)} | ${esc(scenario.status)} | ${esc(scenario.contractRefs.join(', '))} | ${esc(scenario.statusReason)} |`);
  }
  lines.push(...truncationNote(scenarios, 200, 'contract scenarios'));
  lines.push('', '## Dependency hotspots', '');
  const hotspots = assessment.assessments.dependencyRisk.highFanIn || [];
  if (!hotspots.length) lines.push('No high fan-in hotspots at the configured threshold.');
  else {
    lines.push('| Entity | Fan-in | Execution | Cross-boundary callers |', '|---|---:|---|---:|');
    for (const hotspot of hotspots.slice(0, 100)) {
      const entity = entities.get(hotspot.entityRef);
      lines.push(`| ${esc(entity?.label || hotspot.entityRef)} | ${hotspot.fanIn} | ${esc(hotspot.executionStatus)} | ${hotspot.crossBoundaryCallers} |`);
    }
    lines.push(...truncationNote(hotspots, 100, 'hotspots'));
  }
  lines.push('', '## Boundaries', '');
  for (const boundary of facts.facts.boundaries) {
    lines.push(`- **${esc(boundary.kind)} ${esc(boundary.name)}** — ${(boundary.paths || []).map((path) => code(path)).join(', ')}`);
  }
  lines.push('', '## Notes', '', 'This document is a generated projection. JSON facts and assessment artifacts are the machine-readable source for this observation; cod-doc specifications remain the source of truth for intent.', '');
  return lines.join('\n');
}

export async function writeStructureExports(result, {
  factsPath,
  assessmentPath,
  propertyDraftsPath,
  driftPath,
  markdownPath,
  owner,
  rootDir = process.env.STRUCTURE_EXPORT_ROOT || process.cwd(),
} = {}) {
  // запись ограничена корнем; resolveContainedPath раскрывает symlink'и
  // (ближайший существующий предок), всё, что уходит за пределы root, отклоняется
  const resolveInside = (path) => resolveContainedPath(path, { rootDir });
  const written = [];
  const writeJson = async (path, value) => {
    if (!path) return;
    const absolute = await resolveInside(path);
    await mkdir(dirname(absolute), { recursive: true });
    await writeFile(absolute, `${JSON.stringify(value, null, 2)}\n`, 'utf8');
    written.push(absolute);
  };
  await writeJson(factsPath, result.facts);
  await writeJson(assessmentPath, result.assessment);
  await writeJson(propertyDraftsPath, {
    version: 1,
    kind: 'property_drafts',
    factsFingerprint: result.facts.fingerprint,
    obligationsRevision: result.assessment.obligationsRevision,
    properties: result.propertyDrafts,
  });
  await writeJson(driftPath, result.drift || null);
  if (markdownPath) {
    const absolute = await resolveInside(markdownPath);
    await mkdir(dirname(absolute), { recursive: true });
    await writeFile(absolute, `${buildStructureMarkdown(result, { owner })}\n`, 'utf8');
    written.push(absolute);
  }
  return written;
}
