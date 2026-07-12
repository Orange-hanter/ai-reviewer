import { SEV, MARKERS } from './constants.mjs';
import { extractReasoningEntries } from './export.mjs';

const REASONING_ADVISOR_MAX = 1800;
const REASONING_SECTION_MAX = 10_000;
const STICKY_REASONING_EXCERPT = 400;

function reasoningTitle({ hasRefs, hasClaims, hasConsensus }) {
  if (hasRefs && hasClaims) return 'MoA reasoning (advisors → aggregator)';
  if (hasRefs) return 'MoA advisor notes';
  if (hasClaims) return 'Jury (advisor → aggregator)';
  if (hasConsensus) return 'Model reasoning (consensus)';
  return 'Review reasoning';
}

/** Excerpt для sticky footer JSON (без полного prose). */
export function buildStickyReasoningFooter(reviews, reviewMode, { excerptLen = STICKY_REASONING_EXCERPT } = {}) {
  return extractReasoningEntries(reviews, reviewMode, { excerptLen })
    .filter((e) => e.excerpt || e.error);
}

/** Collapsible MoA advisor prose / consensus summaries + jury (advisorClaims). */
export function buildReasoningSection(reviews, allFindings, { enabled = true, reviewMode = 'consensus' } = {}) {
  if (!enabled) return { text: '', truncated: false };

  const refs = (reviews || []).flatMap((r) => r.references || []).filter((ref) => ref.text || ref.error);
  const claimsFindings = (allFindings || []).filter((f) => f.advisorClaims?.length);
  const consensusRows = reviewMode !== 'moa'
    ? (reviews || []).filter((r) => !r.error && r.summary)
    : [];

  if (!refs.length && !claimsFindings.length && !consensusRows.length) {
    return { text: '', truncated: false };
  }

  let body = '';
  let budget = REASONING_SECTION_MAX;
  let truncated = false;

  for (const ref of refs) {
    const label = ref.role ? `${ref.role} · ${ref.model}` : ref.model;
    let chunk = '';
    if (ref.error) {
      chunk = `⚠️ ${ref.error.slice(0, 300)}`;
    } else {
      const raw = String(ref.text || '');
      chunk = raw.slice(0, REASONING_ADVISOR_MAX);
      if (raw.length > REASONING_ADVISOR_MAX) {
        chunk += '\n\n…(обрезано)';
        truncated = true;
      }
    }
    const block = `#### ${label}${ref.chunkCount > 1 ? ` (${ref.chunkCount} chunks)` : ''}\n\n${chunk}\n\n`;
    if (block.length > budget) {
      truncated = true;
      break;
    }
    body += block;
    budget -= block.length;
  }

  for (const r of consensusRows) {
    const raw = String(r.summary || '');
    let chunk = raw.slice(0, REASONING_ADVISOR_MAX);
    if (raw.length > REASONING_ADVISOR_MAX) {
      chunk += '\n\n…(обрезано)';
      truncated = true;
    }
    const block = `#### ${r.model}\n\n${chunk}\n\n`;
    if (block.length > budget) {
      truncated = true;
      break;
    }
    body += block;
    budget -= block.length;
  }

  if (claimsFindings.length && budget > 200) {
    body += '#### Jury (advisor → aggregator)\n\n';
    for (const f of claimsFindings.slice(0, 12)) {
      const loc = f.file ? `\`${f.file}${f.line ? `:${f.line}` : ''}\`` : '—';
      const lines = (f.advisorClaims || []).map((c) =>
        `- **${c.role}** ${c.status}${c.reason ? `: ${c.reason.slice(0, 160)}` : ''}`,
      ).join('\n');
      const block = `- ${loc} «${(f.title || '').slice(0, 80)}»\n${lines}\n`;
      if (block.length > budget) {
        truncated = true;
        break;
      }
      body += block;
      budget -= block.length;
    }
  }

  if (!body.trim()) return { text: '', truncated: false };

  if (truncated) {
    body += '\n> _Reasoning обрезан по лимиту секции (~10k симв.). Полный JSON artifact — без prose body._\n';
  }

  const title = reasoningTitle({
    hasRefs: refs.length > 0,
    hasClaims: claimsFindings.length > 0,
    hasConsensus: consensusRows.length > 0 && !refs.length,
  });
  const text = `\n<details><summary>🔗 ${title}</summary>\n\n${body.trim()}\n\n</details>\n`;
  return { text, truncated };
}

export function buildSummary(cfg, input) {
  const {
    reviews, ok, allFindings, outside, blastBlocks, files, pr, headSha, note,
    blockerClusters, verifierStats, refutationStats,
  } = input;
  const { gate, reviewMode, models, moa, verifier, specialists } = cfg;
  const { blockOn, blockSingle, consensusMin } = gate;

  const counts = { critical: 0, major: 0, minor: 0, nit: 0 };
  for (const f of allFindings) if (counts[f.severity] != null) counts[f.severity]++;
  const total = allFindings.length;
  const blocking = blockerClusters.length;
  const verdictRu = blocking ? '🔴 нужны правки' : (total ? '💬 есть замечания' : '✅ можно мёржить');
  const modelList = ok.map((r) => r.model).join(' + ') || models.list.join(' + ');
  const consensusTier = blockOn.filter((s) => !blockSingle.includes(s));
  const ruleNote = reviewMode === 'moa'
    ? `${blockOn.join('/')} — от MoA-aggregator (advisors: ${moa.references.join('+')}); confidence ≥${gate.confidenceMin}`
    : `${blockSingle.join('/')} — от любой модели` +
      (consensusTier.length ? `; ${consensusTier.join('/')} — по консенсусу ≥${consensusMin} модел.` : '') +
      `; confidence ≥${gate.confidenceMin} для блока`;

  let md = `${MARKERS.review}\n## 🤖 Авто код-ревью (${modelList}) — ${verdictRu}\n\n`;
  if (blocking) {
    md += `> ⛔ **Merge заблокирован:** ${blocking} замечани${blocking === 1 ? 'е' : 'й'}. ` +
      `Правило блокировки: ${ruleNote}. Исправьте (или ослабьте \`REVIEW_BLOCK_ON\`/\`REVIEW_BLOCK_SINGLE\`/\`REVIEW_CONSENSUS\`). ` +
      `Инфра-сбои merge не блокируют.\n\n`;
  } else if (allFindings.some((f) => blockOn.includes(f.severity)) && reviewMode !== 'moa') {
    md += `> ✅ Блокеров нет: ${consensusTier.join('/') || blockOn.join('/')} есть лишь у одной модели — показаны как ` +
      `замечания (для блока нужен консенсус ≥${consensusMin}); ${blockSingle.join('/')} не найдено.\n\n`;
  } else if (allFindings.some((f) => blockOn.includes(f.severity)) && reviewMode === 'moa') {
    md += `> ✅ Merge не блокируется: aggregator не подтвердил ${blockOn.join('/')} как блокеры.\n\n`;
  }

  md += `### Оценки моделей\n`;
  for (const r of reviews) {
    const fbNote = r.fallbackFor ? ` _(резерв вместо ${r.fallbackFor})_` : '';
    if (r.error) { md += `- **${r.model}**${fbNote} — ⚠️ недоступна (${r.error.slice(0, 120)})\n`; continue; }
    const c = { critical: 0, major: 0, minor: 0, nit: 0 };
    for (const f of r.findings) if (c[f.severity] != null) c[f.severity]++;
    md += `- **${r.model}**${fbNote} — ${r.findings.length} (${SEV.critical}${c.critical} ${SEV.major}${c.major} ${SEV.minor}${c.minor} ${SEV.nit}${c.nit}): ${r.summary || ''}\n`;
    if (r.references?.length) {
      for (const ref of r.references) {
        const label = ref.role ? `${ref.role}/${ref.model}` : ref.model;
        if (ref.error) md += `  - specialist **${label}** — ⚠️ ${ref.error.slice(0, 80)}\n`;
        else md += `  - specialist **${label}** — ok (${(ref.text || '').length} симв.)\n`;
      }
    }
  }

  md += `\n**Итого:** ${total} замечаний — ${SEV.critical}${counts.critical} ${SEV.major}${counts.major} ${SEV.minor}${counts.minor} ${SEV.nit}${counts.nit}. Файлов в диффе: ${files.size}.\n`;
  if (verifierStats && verifier.enabled && (verifierStats.rejected || verifierStats.downgraded || verifierStats.verified)) {
    md += `\n> 🔎 Verifier (Pass B, ${verifier.model}): подтверждено ${verifierStats.verified}, отклонено FP ${verifierStats.rejected}, понижено ${verifierStats.downgraded}.\n`;
  }
  if (refutationStats && specialists.enabled && (refutationStats.dropped || refutationStats.downgraded)) {
    const nc = refutationStats.downgradedNoClaims ? `, noClaims→minor ${refutationStats.downgradedNoClaims}` : '';
    md += `\n> ⚖️ Refutation (R5 jury): dropped ${refutationStats.dropped}, downgraded ${refutationStats.downgraded}${nc}, kept ${refutationStats.kept}.\n`;
  }
  if (input.prescanResult?.findings?.length) {
    const n = input.prescanResult.findings.length;
    const bt = input.prescanResult.byTool;
    const tools = bt
      ? `eslint ${bt.eslint ?? 0} + ast-grep ${bt.astgrep ?? 0}`
      : `${n}`;
    md += `\n> 🔧 Prescan (R6 eslint + ast-grep): ${tools} детерминированных finding(s) на ${input.prescanResult.scanned} файлах (LLM dedup).\n`;
  }
  if (note) md += `\n> ⚠️ ${note}\n`;

  if (outside.length) {
    md += `\n### Замечания вне строк диффа\n`;
    for (const f of outside) {
      const loc = f.file ? ` (\`${f.file}\`${f.line ? `:${f.line}` : ''})` : '';
      md += `- ${SEV[f.severity] || '⚪'} **${f.title || 'замечание'}** _(${f.model})_${loc} — ${f.body || ''}\n`;
    }
  }
  if (blastBlocks.length) {
    md += `\n<details><summary>📡 Blast-radius (Graphify)</summary>\n\n${blastBlocks.join('\n')}\n\n</details>\n`;
  }

  const reasoningEnabled = cfg.export?.reasoningInSticky !== false;
  const { text: reasoningMd, truncated: reasoningTruncated } = buildReasoningSection(reviews, allFindings, {
    enabled: reasoningEnabled,
    reviewMode,
  });
  md += reasoningMd;

  const gateNote = blockOn.length ? `блокирует merge: ${ruleNote}` : 'ревью советующее';
  const modelFoot = reviewMode === 'moa'
    ? (specialists.enabled
      ? `MoA specialists: ${specialists.assignments.map((a) => `${a.role}=${a.model}`).join(', ')} · aggregator: ${moa.aggregator}`
      : `MoA advisors: ${moa.references.join(', ')} · aggregator: ${moa.aggregator}`)
    : `Модели: ${models.list.join(', ')}`;
  md += `\n<sub>${modelFoot} · commit ${String(headSha).slice(0, 7) || '—'} · ${gateNote}; инфра-сбои merge не блокируют.</sub>`;

  const slim = allFindings.map((f) => ({ file: f.file, line: f.line, severity: f.severity, title: f.title, model: f.model }));
  const reasoning = reasoningEnabled ? buildStickyReasoningFooter(reviews, reviewMode) : [];
  const footerData = {
    sha: String(headSha).slice(0, 7),
    findings: slim,
    ...(reasoning.length ? { reasoning } : {}),
  };
  const footer = `\n<!-- orakul-ai-review-data:${JSON.stringify(footerData)} -->`;
  const LIMIT = 65_000;
  if (md.length + footer.length > LIMIT) {
    md = md.slice(0, Math.max(0, LIMIT - footer.length - 120)) + '\n\n> …(отчёт обрезан под лимит GitHub)\n';
    if (reasoningMd) {
      md += '\n> _Секция reasoning могла быть частично обрезана — см. artifact `pr-review-export`._\n';
    }
  } else if (reasoningTruncated) {
    md += '\n> _Reasoning частично обрезан в sticky — полные excerpts в JSON artifact._\n';
  }
  return { md: md + footer, counts, total };
}

/** Sticky для skip/light (R4) — без LLM, merge не блокируется. */
export function buildRouterSummary(cfg, { route, pr, headSha, changedPaths }) {
  const labels = { skip: '⏭ пропуск LLM', light: '📄 docs/config-only' };
  const title = labels[route.route] || route.route;
  let md = `${MARKERS.review}\n## 🤖 Авто код-ревью — ${title}\n\n`;
  md += `> ✅ **Merge не блокируется:** роутер классифицировал PR как \`${route.route}\` (${route.reason}). `;
  md += 'ИИ-ревью кода не запускалось — экономия токенов на lockfile/docs/rename-only.\n\n';
  md += `### Изменённые файлы (${changedPaths.length})\n`;
  for (const f of route.files) md += `- \`${f.path}\` _(${f.class})_\n`;
  md += `\n<sub>Router R4 · commit ${String(headSha).slice(0, 7) || '—'} · REVIEW_ROUTER=${cfg.router.enabled ? '1' : '0'}</sub>`;
  return md;
}
