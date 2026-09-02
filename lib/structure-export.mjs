import { mkdir, writeFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';

function percent(ratio) {
  return ratio == null ? 'n/a' : `${Math.round(ratio * 1000) / 10}%`;
}

function esc(value) {
  return String(value ?? '').replaceAll('|', '\\|').replaceAll('\n', ' ');
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
        `### ${hint.priority.toUpperCase()} — ${hint.ruleId}`,
        '',
        hint.remediation,
        '',
        `Subjects: ${hint.subjectRefs.map((ref) => `\`${esc(ref)}\``).join(', ')}`,
        `Evidence: ${hint.evidence.length ? hint.evidence.map((ref) => `\`${esc(ref)}\``).join(', ') : 'none'}`,
        `Missing: ${hint.missingEvidence.length ? hint.missingEvidence.join(', ') : 'none'}`,
        '',
      );
    }
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
  lines.push('', '## Dependency hotspots', '');
  const hotspots = assessment.assessments.dependencyRisk.highFanIn || [];
  if (!hotspots.length) lines.push('No high fan-in hotspots at the configured threshold.');
  else {
    lines.push('| Entity | Fan-in | Execution | Cross-boundary callers |', '|---|---:|---|---:|');
    for (const hotspot of hotspots.slice(0, 100)) {
      const entity = entities.get(hotspot.entityRef);
      lines.push(`| ${esc(entity?.label || hotspot.entityRef)} | ${hotspot.fanIn} | ${hotspot.executionStatus} | ${hotspot.crossBoundaryCallers} |`);
    }
  }
  lines.push('', '## Boundaries', '');
  for (const boundary of facts.facts.boundaries) {
    lines.push(`- **${boundary.kind} ${esc(boundary.name)}** — ${boundary.paths.map((path) => `\`${esc(path)}\``).join(', ')}`);
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
} = {}) {
  const written = [];
  const writeJson = async (path, value) => {
    if (!path) return;
    const absolute = resolve(path);
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
    const absolute = resolve(markdownPath);
    await mkdir(dirname(absolute), { recursive: true });
    await writeFile(absolute, `${buildStructureMarkdown(result, { owner })}\n`, 'utf8');
    written.push(absolute);
  }
  return written;
}
