/**
 * Фаза 3, шаг 6: lib/export.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/export (машиночитаемый экспорт прогона: JSON-артефакт
 * через buildReviewJson + SARIF 2.1.0 для GitHub Code Scanning через buildSarif).
 *
 * Публичное API не изменилось (те же экспорты, та же синхронность). Функции
 * вызываются синхронно, поэтому вместо async execFile используется
 * execFileSync: бинарь вызывается на каждую операцию с JSON {"op", ...args}
 * на stdin, ответ — JSON с результатом.
 *
 * Резолвинг бинаря (по аналогии с lib/report.mjs):
 *   AI_REVIEWER_EXPORT_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/export;
 *   AI_REVIEWER_EXPORT_FORCE_JS=1 — принудительно чистый JS (паритет/отладка).
 *
 * Если бинарь не существует (чистый клон до первого `cargo build` — в CI npm
 * test/eval идут раньше cargo build, см. .github/workflows/ci.yml), обёртка
 * деградирует на чистый JS fallback ниже — эталонную копию исходной реализации.
 * Если бинарь ЕСТЬ, но упал (non-zero exit) — ошибка пробрасывается, fallback
 * не используется: молчаливая подмена при битом Rust-коде скрывала бы регрессии.
 *
 * КРИТИЧНО для побайтовости SARIF (внешний контракт для GitHub code scanning):
 * порядок ключей объекта значим — Rust собирает объекты в порядке JS-литералов
 * (serde preserve_order), undefined-поля опускаются, обрезки идут по UTF-16
 * code units (тот же js_slice_utf16, что в рендере sticky). Побайтовый паритет
 * JSON.stringify(sarif, null, 2) проверен rust/parity/export.parity.mjs.
 *
 * Нюансы паритета Rust-порта:
 *   - часы НЕ портированы: generatedAt (top-level и внутри swarm-вердикта)
 *     генерирует обёртка (new Date().toISOString()) и передаёт бинарю
 *     аргументом — как и fs-запись (writeReviewExports остался чистым JS);
 *   - профиль (toolName/sarifRulePrefix/repositoryUri) читается на JS-стороне
 *     и уходит аргументом — профиль живёт в JS (getProfile);
 *   - buildSwarmVerdict (lib/swarm.mjs) портирован в Rust внутрь export-модуля:
 *     нужен только buildReviewJson; E23 suggestedScopeFiles сортируется по
 *     UTF-16 code units, как дефолтный Array#sort в V8;
 *   - sarifAutomationGuid — sha256(`${tool}:${seed}`) с RFC-4122 variant;
 *   - extractReasoningEntries переиспользует порт из crate::report (шаг 5) —
 *     тот же DY-источник для JSON-артефакта и sticky-футера.
 */

import { createHash } from 'node:crypto';
import { existsSync } from 'node:fs';
import { mkdir, writeFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { getProfile } from './profile.mjs';
import { forgeOf } from './forge.mjs';
import { buildCodeQuality, CODEQUALITY_FILE } from './codequality.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_EXPORT_BIN = resolve(__dir, '../rust/target/debug/export');

function resolveExportBin() {
  if (process.env.AI_REVIEWER_EXPORT_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_EXPORT_BIN || '').trim() || DEFAULT_EXPORT_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). */
function runRust(op, args) {
  const bin = resolveExportBin();
  if (!bin) return null;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

// v2: slimFinding несёт fp/verifierStatus/actionabilityScore/actionabilityPromoted —
// дедупликация находок по коду и метрика стабильности набора на стороне потребителя
// (cod-doc ingest). `fp` — тот же отпечаток, что в футере sticky (attachFingerprints);
// `actionabilityScore` есть у каждой находки, `actionabilityPromoted` отмечает advisory,
// поднятую в строку. Изменение аддитивное: потребитель v1 читает v2 без поломки, поэтому
// сверять версию нужно на «известна ли», а не на равенство.
export const EXPORT_VERSION = 2;
export const SARIF_SCHEMA = 'https://json.schemastore.org/sarif-2.1.0.json';
export const SARIF_GUID_RE = /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[1-5][0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$/;
export const TOOL_VERSION = '1.0';

/** Имя инструмента в SARIF и в именах artifact-файлов — из профиля проекта. */
export function toolName() {
  return getProfile().toolName;
}

// ---------------------------------------------------------------------------
// Публичное API (диспетчер Rust ↔ JS fallback)
// ---------------------------------------------------------------------------

/** Stable RFC-4122-shaped GUID for SARIF automationDetails (Code Scanning requires UUID). */
export function sarifAutomationGuid(seed, tool = toolName()) {
  const rust = runRust('sarifAutomationGuid', { seed: String(seed), tool: String(tool) });
  if (rust !== null) return rust;
  return sarifAutomationGuidJs(seed, tool);
}

/** Percent-encode path segments for SARIF artifactLocation.uri. */
export function sarifArtifactUri(file) {
  const rust = runRust('sarifArtifactUri', { file: file ?? null });
  if (rust !== null) return rust;
  return sarifArtifactUriJs(file);
}

/** Shared MoA advisor + consensus reasoning extraction (DRY for JSON + sticky footer). */
export function extractReasoningEntries(reviews, reviewMode, { excerptLen = 600, errorMax = 200 } = {}) {
  const rust = runRust('extractReasoningEntries', {
    reviews: reviews ?? null, reviewMode, excerptLen, errorMax,
  });
  if (rust !== null) return rust;
  return extractReasoningEntriesJs(reviews, reviewMode, { excerptLen, errorMax });
}

/** Полный JSON payload для CI artifact / интеграций. Часы — на JS-стороне. */
export function buildReviewJson(input) {
  const generatedAt = new Date().toISOString();
  const rust = runRust('buildReviewJson', { input, generatedAt });
  if (rust !== null) return rust;
  return buildReviewJsonJs(input);
}

/** Findings для SARIF: по умолчанию только blockers (шум minor/nit в Code Scanning). */
export function selectSarifFindings(payload, sarifAll = false) {
  const rust = runRust('selectSarifFindings', { payload, sarifAll: sarifAll === true });
  if (rust !== null) return rust;
  return selectSarifFindingsJs(payload, sarifAll);
}

/** SARIF 2.1.0 для upload-sarif / Code Scanning. Профиль применяется на JS-стороне. */
export function buildSarif(payload, { repositoryUri, sarifAll = false, profile = getProfile() } = {}) {
  const profileJson = {
    toolName: profile.toolName,
    sarifRulePrefix: profile.sarifRulePrefix,
    repositoryUri: profile.repositoryUri || null,
  };
  const rust = runRust('buildSarif', {
    payload,
    repositoryUri: repositoryUri || null,
    sarifAll: sarifAll === true,
    profile: profileJson,
  });
  if (rust !== null) return rust;
  return buildSarifJs(payload, { repositoryUri, sarifAll, profile });
}

// ---------------------------------------------------------------------------
// Эталонный JS fallback (копия прежней реализации, активна без бинаря / FORCE_JS)
// ---------------------------------------------------------------------------

function sarifAutomationGuidJs(seed, tool = toolName()) {
  const h = createHash('sha256').update(`${tool}:${seed}`).digest('hex');
  const variant = ['8', '9', 'a', 'b'][parseInt(h.slice(16, 17), 16) % 4];
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-4${h.slice(13, 16)}-${variant}${h.slice(17, 20)}-${h.slice(20, 32)}`;
}

const BODY_EXPORT_MAX = 400;
const REASONING_EXPORT_MAX = 600;
const SARIF_MESSAGE_MAX = 500;
const TITLE_MAX = 200;

const SEV_TO_SARIF_LEVEL = {
  critical: 'error',
  major: 'warning',
  minor: 'note',
  nit: 'note',
};

const SEVERITIES = ['critical', 'major', 'minor', 'nit'];

/** SARIF ruleId: `<prefix>/<severity>`; префикс — из профиля (Code Scanning группирует по нему). */
function sarifRuleIds(prefix = getProfile().sarifRulePrefix) {
  return Object.fromEntries(SEVERITIES.map((sev) => [sev, `${prefix}/${sev}`]));
}

/** Redacted finding для artifact (body урезан — как R7 observability). */
function slimFinding(f) {
  const body = String(f.body || '');
  return {
    file: f.file || '',
    line: f.line || 0,
    severity: f.severity,
    title: String(f.title || '').slice(0, TITLE_MAX),
    body: body ? body.slice(0, BODY_EXPORT_MAX) : undefined,
    bodyTruncated: body.length > BODY_EXPORT_MAX,
    model: f.model || '',
    confidence: f.confidence,
    consensusBlock: f.consensusBlock || false,
    prescan: f.prescan || undefined,
    advisorClaims: f.advisorClaims?.map((c) => ({
      role: c.role,
      status: c.status,
      reason: c.reason ? String(c.reason).slice(0, 200) : '',
    })),
    refutationStatus: f.refutationStatus || undefined,
    fp: f.fp || undefined,
    verifierStatus: f.verifierStatus || undefined,
    actionabilityScore: Number.isFinite(f.actionabilityScore)
      ? f.actionabilityScore
      : undefined,
    actionabilityPromoted: f.actionabilityPromoted || undefined,
  };
}

function sarifArtifactUriJs(file) {
  const raw = (file || '').trim();
  if (!raw) return 'unknown';
  return raw.split('/').map((seg) => encodeURIComponent(seg)).join('/');
}

function extractReasoningEntriesJs(reviews, reviewMode, { excerptLen = 600, errorMax = 200 } = {}) {
  const out = [];
  for (const r of reviews || []) {
    if (r.references?.length) {
      for (const ref of r.references) {
        if (ref.error) {
          out.push({
            role: ref.role || 'advisor',
            model: ref.model,
            chunkCount: ref.chunkCount || 1,
            error: String(ref.error).slice(0, errorMax),
          });
        } else if (ref.text) {
          const text = String(ref.text);
          out.push({
            role: ref.role || 'advisor',
            model: ref.model,
            chunkCount: ref.chunkCount || 1,
            excerpt: text.slice(0, excerptLen),
            truncated: text.length > excerptLen,
          });
        }
      }
      continue;
    }
    if (reviewMode !== 'moa' && !r.error && r.summary) {
      const text = String(r.summary);
      out.push({
        role: 'model',
        model: r.model,
        excerpt: text.slice(0, excerptLen),
        truncated: text.length > excerptLen,
      });
    }
  }
  return out;
}

function slimReasoningJs(reviews, reviewMode) {
  return extractReasoningEntriesJs(reviews, reviewMode, { excerptLen: REASONING_EXPORT_MAX });
}

/** buildSwarmVerdict (lib/swarm.mjs) — эталонная копия для JS fallback. */
function buildSwarmVerdictJs(input = {}) {
  const blockers = Array.isArray(input.blockers) ? input.blockers : [];
  const wouldBlock = Boolean(input.wouldBlock);
  const isStrongBlocker = (f) => {
    if (!f) return false;
    if (f.prescan) return true;
    if (f.verifierStatus === 'verified') return true;
    return false;
  };
  const strongBlockers = blockers.filter(isStrongBlocker);
  const strong = wouldBlock && blockers.length > 0 && strongBlockers.length === blockers.length;

  const blast = input.blastMetrics && typeof input.blastMetrics === 'object'
    ? input.blastMetrics
    : {};
  const affected = Array.isArray(blast.affected_files) ? blast.affected_files : [];
  const changed = new Set(input.changedPaths || []);
  const scope = [...new Set(affected || [])]
    .filter((p) => p && !changed.has(p))
    .sort();

  return {
    version: 1,
    role: 'external-verdict',
    generatedAt: new Date().toISOString(),
    headSha: String(input.headSha || '').slice(0, 40),
    pr: {
      number: input.pr?.number ?? 0,
      title: String(input.pr?.title || '').slice(0, 200),
      url: input.pr?.html_url || input.pr?.url || '',
    },
    wouldBlock,
    strong,
    blockers: blockers.map((f) => ({
      file: f.file || '',
      line: Number(f.line) || 0,
      severity: f.severity || '',
      title: String(f.title || '').slice(0, 200),
      source: f.source || undefined,
      prescan: f.prescan || undefined,
      verifierStatus: f.verifierStatus || undefined,
      blastCaller: f.blastCaller || undefined,
      confidence: f.confidence,
    })),
    strongBlockers: strongBlockers.length,
    blastScope: {
      affected_files: affected,
      symbols_total: blast.symbols_total ?? 0,
      symbols_resolved: blast.symbols_resolved ?? 0,
      affected_nonempty: blast.affected_nonempty ?? 0,
      graph_status: blast.graph_status || 'unknown',
      cross_community: blast.cross_community ?? 0,
      skip_reason: blast.skip_reason || undefined,
    },
    suggestedScopeFiles: scope,
    verdict: input.verdict || undefined,
    notes: [
      strong
        ? 'strong: all blockers independently confirmed (prescan or verifier)'
        : wouldBlock
          ? 'wouldBlock without full strong confirmation — treat as advisory for swarm'
          : 'no merge block',
      scope.length
        ? `blast scope: ${scope.length} file(s) outside PR diff`
        : 'blast scope empty',
      input.sourceHint === 'blast-radius'
        ? 'blast-radius findings alone never set strong without verifier'
        : undefined,
    ].filter(Boolean),
  };
}

function buildReviewJsonJs(input) {
  const {
    pr, headSha, reviewMode, route, wouldBlock, verdict, counts, total,
    findings, blockers, reviews, verifierStats, refutationStats, prescanResult,
    llmFailed, llmErrors, blastMetrics, changedPaths,
  } = input;

  const blast = blastMetrics && typeof blastMetrics === 'object'
    ? {
        symbols_total: blastMetrics.symbols_total ?? 0,
        symbols_resolved: blastMetrics.symbols_resolved ?? 0,
        affected_nonempty: blastMetrics.affected_nonempty ?? 0,
        graph_status: blastMetrics.graph_status || 'unknown',
        skip_reason: blastMetrics.skip_reason || undefined,
        affected_files: Array.isArray(blastMetrics.affected_files) ? blastMetrics.affected_files : [],
        confidence: blastMetrics.confidence || undefined,
        cross_community: blastMetrics.cross_community ?? 0,
      }
    : undefined;

  const swarm = buildSwarmVerdictJs({
    wouldBlock: Boolean(wouldBlock),
    blockers,
    blastMetrics: blast || blastMetrics,
    changedPaths,
    headSha,
    pr,
    verdict,
  });

  return {
    version: EXPORT_VERSION,
    generatedAt: new Date().toISOString(),
    pr: {
      number: pr?.number ?? 0,
      title: String(pr?.title || '').slice(0, 200),
      url: pr?.html_url || '',
    },
    headSha: String(headSha || '').slice(0, 40),
    reviewMode: reviewMode || 'consensus',
    router: route ? { route: route.route, reason: route.reason } : undefined,
    wouldBlock: Boolean(wouldBlock),
    // Воспроизводимость вердикта на этом коммите: `wouldBlock` говорит про ЭТОТ прогон,
    // а недетерминизм виден только в сравнении прогонов одного sha. Sticky перезаписывается,
    // artifact — нет, поэтому агрегат должен лежать здесь.
    verdict: verdict || undefined,
    llmFailed: llmFailed || undefined,
    llmErrors: llmErrors?.length ? llmErrors : undefined,
    counts: counts || {},
    total: total ?? (findings?.length ?? 0),
    findings: (findings || []).map(slimFinding),
    blockers: (blockers || []).map(slimFinding),
    reasoning: slimReasoningJs(reviews, reviewMode || 'consensus'),
    verifier: verifierStats || undefined,
    refutation: refutationStats || undefined,
    blast,
    /** E22/E23: контракт для PR-swarm оркестратора (см. docs/swarm.md). */
    swarm,
    prescan: prescanResult ? {
      findings: prescanResult.findings?.length ?? 0,
      scanned: prescanResult.scanned,
      skipped: prescanResult.skipped,
      byTool: prescanResult.byTool,
      error: prescanResult.error,
    } : undefined,
  };
}

function selectSarifFindingsJs(payload, sarifAll = false) {
  if (sarifAll) return payload.findings || [];
  const blockers = payload.blockers || [];
  if (blockers.length) return blockers;
  return (payload.findings || []).filter((f) => f.consensusBlock);
}

function sarifLocationJs(f) {
  const uri = sarifArtifactUriJs(f.file);
  const loc = {
    physicalLocation: {
      artifactLocation: { uri },
    },
  };
  if (f.line > 0) {
    loc.physicalLocation.region = { startLine: f.line };
  }
  return loc;
}

/** SARIF 2.1.0 для upload-sarif / Code Scanning. */
function buildSarifJs(payload, { repositoryUri, sarifAll = false, profile = getProfile() } = {}) {
  const findings = selectSarifFindingsJs(payload, sarifAll);
  const SEV_TO_RULE = sarifRuleIds(profile.sarifRulePrefix);
  const tool = profile.toolName;
  const rules = new Map();
  for (const sev of SEVERITIES) {
    rules.set(SEV_TO_RULE[sev], {
      id: SEV_TO_RULE[sev],
      name: sev,
      shortDescription: { text: `AI review — ${sev}` },
      defaultConfiguration: { level: SEV_TO_SARIF_LEVEL[sev] },
    });
  }

  const results = findings.map((f) => {
    const sev = SEV_TO_RULE[f.severity] ? f.severity : 'minor';
    const title = String(f.title || 'finding').slice(0, TITLE_MAX);
    const result = {
      ruleId: SEV_TO_RULE[sev],
      level: SEV_TO_SARIF_LEVEL[sev],
      message: { text: title.slice(0, SARIF_MESSAGE_MAX) },
      locations: [sarifLocationJs(f)],
      properties: {
        model: f.model || undefined,
        confidence: f.confidence,
        consensusBlock: f.consensusBlock || false,
        prescan: f.prescan || undefined,
      },
    };
    return result;
  });

  const run = {
    tool: {
      driver: {
        name: tool,
        version: TOOL_VERSION,
        informationUri: profile.repositoryUri || repositoryUri || undefined,
        rules: [...rules.values()],
      },
    },
    results,
    automationDetails: {
      description: {
        text: `AI code review PR #${payload.pr?.number ?? '?'}`,
      },
    },
  };
  if (repositoryUri) {
    run.automationDetails.guid = sarifAutomationGuidJs(repositoryUri, tool);
  }

  return {
    $schema: SARIF_SCHEMA,
    version: '2.1.0',
    runs: [run],
  };
}

// ---------------------------------------------------------------------------
// fs-запись артефактов — остаётся в JS (построение документов — в Rust выше)
// ---------------------------------------------------------------------------

export async function writeReviewExports(cfg, payload, log = () => {}, opts = {}) {
  const exp = cfg.export;
  if (!exp?.enabled) return null;

  const tool = toolName();
  const dir = resolve(process.cwd(), exp.dir || getProfile().paths.exportDir);
  await mkdir(dir, { recursive: true });

  const repositoryUri = opts.repositoryUri || forgeOf(cfg).webUrl || undefined;
  const sarifAll = exp.sarifAll === true;
  const sarifFindings = selectSarifFindings(payload, sarifAll);

  const written = [];
  if (exp.json) {
    const jsonPath = join(dir, `${tool}.json`);
    await writeFile(jsonPath, `${JSON.stringify(payload, null, 2)}\n`, 'utf8');
    written.push(jsonPath);
    log(`export R9: JSON → ${jsonPath}`);
    if (payload.swarm) {
      const swarmPath = join(dir, `${tool}-swarm.json`);
      await writeFile(swarmPath, `${JSON.stringify(payload.swarm, null, 2)}\n`, 'utf8');
      written.push(swarmPath);
      log(`export E22: swarm → ${swarmPath}`);
    }
  }
  if (exp.sarif) {
    const sarifPath = join(dir, `${tool}.sarif.json`);
    const sarif = buildSarif(payload, { repositoryUri, sarifAll });
    await writeFile(sarifPath, `${JSON.stringify(sarif, null, 2)}\n`, 'utf8');
    written.push(sarifPath);
    log(`export R9: SARIF → ${sarifPath} (${sarifFindings.length} results${sarifAll ? ', all' : ', blockers'})`);
  }
  // RFC 16: на GitLab — отчёт Code Quality для виджета MR (SARIF там не рендерится).
  if (forgeOf(cfg).kind === 'gitlab') {
    const cqPath = join(dir, CODEQUALITY_FILE);
    const report = buildCodeQuality(payload, { checkPrefix: getProfile().sarifRulePrefix || tool });
    await writeFile(cqPath, `${JSON.stringify(report, null, 2)}\n`, 'utf8');
    written.push(cqPath);
    log(`export: Code Quality → ${cqPath} (${report.length} находок)`);
  }
  return { dir, written, sarifResults: sarifFindings.length };
}
