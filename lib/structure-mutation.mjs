/**
 * Фаза 2, шаг 3: lib/structure-mutation.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/structure_mutation (порт parseMutationReport и
 * criticalContractRefs).
 *
 * Публичное API не изменилось (те же экспорты, та же синхронность). Синхронные
 * функции вызывают бинарь через execFileSync: JSON {"op", ...args} на stdin,
 * ответ — JSON в формате JS-возврата.
 *
 * Резолвинг бинаря (по аналогии с lib/structure-lcov.mjs):
 *   AI_REVIEWER_STRUCTURE_MUTATION_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/structure_mutation;
 *   AI_REVIEWER_STRUCTURE_MUTATION_FORCE_JS=1 — принудительно чистый JS.
 *
 * Если бинарь не существует (чистый клон до первого `cargo build`), обёртка
 * деградирует на чистый JS fallback ниже — эталонную копию исходной реализации.
 * Если бинарь ЕСТЬ, но упал (non-zero exit) — ошибка пробрасывается, fallback
 * не используется: молчаливая подмена реализации при битом Rust-коде скрывала бы
 * регрессии.
 *
 * Нюансы паритета (проверены parity-прогоном rust/parity/structure-mutation.parity.mjs):
 *   - `Number(location.start?.line || mutant.line || 0) || null`: строка '0' —
 *     truthy, но даёт line null; ' 5 '/'1e2' — как в JS Number();
 *   - entityRef/observedId — ключи JS Map (скаляры по значению, объекты по
 *     идентичности); guards entity.line/endLine применяются только когда оба
 *     операнда truthy;
 *   - без location.line привязка только при единственном кандидате файла;
 *     иначе stable-сортировка по дистанции (NaN-дистанция — как 0 у V8);
 *   - MUTANT_STATUS — точное строковое совпадение, иначе
 *     String(status || 'unknown').toLowerCase(); killedBy — Set по raw-значениям
 *     (объекты не дедуплицируются), потом map(String) и побайтовая сортировка;
 *   - report.files принимает и массив (Object.entries по индексам, как в JS);
 *   - criticalContractRefs: flatMap(contractRefs || []) оборачивает не-массив в
 *     один элемент; entityRef === observedId — строгое равенство; итог —
 *     filter(Boolean) + дефолтный Array.sort (по String).
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { normalizeRepoPath, stableId } from './structure-evidence.mjs';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_MUTATION_BIN = resolve(__dir, '../rust/target/debug/structure_mutation');

function resolveMutationBin() {
  if (process.env.AI_REVIEWER_STRUCTURE_MUTATION_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_STRUCTURE_MUTATION_BIN || '').trim() || DEFAULT_MUTATION_BIN;
  return existsSync(bin) ? bin : null;
}

/** Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). Ошибка бинаря пробрасывается. */
function runRust(op, args) {
  const bin = resolveMutationBin();
  if (!bin) return null;
  const raw = execFileSync(bin, [], {
    input: JSON.stringify({ op, ...args }),
    encoding: 'utf8',
    maxBuffer: 128 * 1024 * 1024,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  return JSON.parse(raw);
}

// ---------------------------------------------------------------------------
// Публичное API (диспетчер Rust ↔ JS fallback)
// ---------------------------------------------------------------------------

/** Разбирает mutation-testing-elements отчёт в наблюдения evidence. */
export function parseMutationReport(report, facts, { scopeContractRefs = [] } = {}) {
  const r = runRust('parseMutationReport', { report, facts, scopeContractRefs });
  if (r !== null) return r;
  return parseMutationReportJs(report, facts, { scopeContractRefs });
}

/** Контракты критичного скоупа: must-обязательства + highFanIn + god-ноды. */
export function criticalContractRefs(facts, dependencyRisk, obligations) {
  const r = runRust('criticalContractRefs', { facts, dependencyRisk, obligations });
  if (r !== null) return r;
  return criticalContractRefsJs(facts, dependencyRisk, obligations);
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/structure-mutation.mjs
// до Фазы 2). Используется только когда Rust-бинарь отсутствует; при битом бинаре
// НЕ подменяет его.
// ---------------------------------------------------------------------------

const MUTANT_STATUS = new Map([
  ['Killed', 'killed'],
  ['Survived', 'survived'],
  ['NoCoverage', 'no_coverage'],
  ['Timeout', 'timeout'],
  ['CompileError', 'compile_error'],
  ['RuntimeError', 'runtime_error'],
  ['Ignored', 'ignored'],
]);

function locationOf(mutant) {
  const location = mutant.location || {};
  return {
    line: Number(location.start?.line || mutant.line || 0) || null,
    endLine: Number(location.end?.line || location.start?.line || mutant.line || 0) || null,
  };
}

function contractForMutant(path, location, contracts, entitiesById) {
  const candidates = [];
  for (const contract of contracts || []) {
    const entity = entitiesById.get(contract.entityRef);
    if (!entity || entity.path !== path) continue;
    if (entity.line && location.line && entity.line > location.line) continue;
    if (entity.endLine && location.line && entity.endLine < location.line) continue;
    candidates.push({ contract, entity });
  }
  // без location.line оба guard'а выше пропускаются, а сортировка по расстоянию вырождается
  // в «первый контракт файла» — это ложное evidence, поэтому привязываем только однозначное
  if (!location.line) return candidates.length === 1 ? candidates[0].contract : null;
  candidates.sort((a, b) => {
    const aDistance = Math.abs((a.entity.line || location.line || 0) - (location.line || 0));
    const bDistance = Math.abs((b.entity.line || location.line || 0) - (location.line || 0));
    return aDistance - bDistance;
  });
  return candidates[0]?.contract || null;
}

function parseMutationReportJs(report, facts, { scopeContractRefs = [] } = {}) {
  const entitiesById = new Map((facts?.entities || []).map((entity) => [entity.observedId, entity]));
  const allowed = new Set(scopeContractRefs || []);
  const observations = [];
  const warnings = [];
  const files = report?.files && typeof report.files === 'object' ? report.files : {};
  for (const [rawPath, file] of Object.entries(files)) {
    let path;
    try { path = normalizeRepoPath(rawPath); } catch (error) {
      warnings.push(error.message);
      continue;
    }
    for (const mutant of file?.mutants || []) {
      const location = locationOf(mutant);
      const contract = contractForMutant(path, location, facts?.contracts || [], entitiesById);
      if (allowed.size && (!contract || !allowed.has(contract.id))) continue;
      const mutationStatus = MUTANT_STATUS.get(mutant.status) || String(mutant.status || 'unknown').toLowerCase();
      observations.push({
        id: stableId('mutant', path, mutant.id, mutant.mutatorName, location.line),
        level: 'contract_edge_coverage',
        subjectRef: contract?.id || `file:${path}`,
        contractRef: contract?.id || null,
        path,
        line: location.line,
        endLine: location.endLine,
        mutator: String(mutant.mutatorName || mutant.type || 'unknown'),
        replacement: mutant.replacement == null ? null : String(mutant.replacement),
        status: mutationStatus === 'killed' ? 'covered'
          : ['survived', 'no_coverage'].includes(mutationStatus) ? 'uncovered' : 'partial',
        mutationStatus,
        killedBy: [...new Set(mutant.killedByTests || mutant.killedBy || [])].map(String).sort(),
        confidence: contract ? 'exact' : 'heuristic',
        evidence: [`mutation:${path}:${mutant.id}`],
      });
    }
  }
  observations.sort((a, b) => a.id.localeCompare(b.id));
  return {
    observations,
    warnings,
    summary: {
      total: observations.length,
      killed: observations.filter((item) => item.mutationStatus === 'killed').length,
      survived: observations.filter((item) => item.mutationStatus === 'survived').length,
      noCoverage: observations.filter((item) => item.mutationStatus === 'no_coverage').length,
      timeout: observations.filter((item) => item.mutationStatus === 'timeout').length,
    },
  };
}

function criticalContractRefsJs(facts, dependencyRisk, obligations) {
  const must = new Set((obligations || [])
    .filter((obligation) => obligation.status === 'confirmed' && obligation.priority === 'must')
    .flatMap((obligation) => obligation.contractRefs || []));
  for (const hotspot of dependencyRisk?.highFanIn || []) must.add(hotspot.contractRef || hotspot.entityRef);
  for (const entity of facts?.entities || []) {
    if (entity.isGodNode) {
      for (const contract of facts.contracts || []) {
        if (contract.entityRef === entity.observedId) must.add(contract.id);
      }
    }
  }
  return [...must].filter(Boolean).sort();
}
