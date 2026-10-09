/**
 * RFC 16: отчёт GitLab Code Quality (`artifacts:reports:codequality`) из payload экспорта.
 * SARIF GitLab в виджете MR не рендерит, а Code Quality показывает и в Free-тарифе.
 *
 * Отдельный модуль, а не ветка в lib/export.mjs: export портирован в Rust и сверяется
 * parity, а этот отчёт нужен только GitLab-потребителю и в Rust не нужен.
 * Формат: https://docs.gitlab.com/ci/testing/code_quality/#code-quality-report-format
 */

import { createHash } from 'node:crypto';

export const CODEQUALITY_FILE = 'gl-code-quality-report.json';

const SEVERITY = { critical: 'critical', major: 'major', minor: 'minor', nit: 'info' };

/**
 * Все находки прогона, не только блокеры: виджет показывает их как «новые» относительно
 * target-ветки, а гейт и так держит exit code — дублировать его выборкой незачем.
 * Отпечаток без номера строки: GitLab сравнивает отчёты по нему, и сдвиг строки соседним
 * коммитом не должен выглядеть как «исправлено + новое».
 */
export function buildCodeQuality(payload, { checkPrefix = 'ai-reviewer' } = {}) {
  return (payload?.findings || [])
    .filter((f) => f && f.file)
    .map((f) => {
      const severity = SEVERITY[f.severity] || 'info';
      const title = String(f.title || '').trim() || '(без заголовка)';
      return {
        description: title,
        check_name: `${checkPrefix}/${f.severity || 'unknown'}`,
        fingerprint: createHash('sha256').update(`${f.file}\n${f.severity || ''}\n${title}`).digest('hex').slice(0, 32),
        severity,
        location: { path: f.file, lines: { begin: Math.max(1, Number(f.line) || 1) } },
      };
    });
}
