/**
 * RFC 06 «Один вердикт жюри — один штраф в actionability»
 * (proposals/06-actionability-single-penalty.md, разбор — docs/review-quality.md §2.3).
 *
 * Сигнал `juryDowngraded` (−2) не применяется по advisorClaims, если refutation уже
 * понизила severity (`refutationStatus === 'downgraded'`): иначе один и тот же вердикт
 * жюри вычитается дважды и одна понижённая претензия из пяти держит потолок в 1 балл
 * при пороге маршрута 4. Штраф по `verifierStatus` — отдельный судья, его RFC не трогает.
 *
 * Файл гоняется дважды в CI-смысле: с Rust-бинарём (если собран) и через
 * AI_REVIEWER_ACTIONABILITY_FORCE_JS=1 — обе реализации обязаны совпадать.
 */

import './helpers/reference-profile.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';

import { scoreActionability, routeFindings } from '../lib/actionability.mjs';

const CTX = { fileClass: () => 'logic' };

/** Minor в логическом файле из чувствительной зоны: базовые +1 confidence, +1 sensitivePath, +1 logicFile. */
function baseFinding(extra = {}) {
  return {
    file: 'server/auth/orders.ts',
    line: 42,
    severity: 'minor',
    title: 'подмена статуса заказа без проверки прав',
    confidence: 0.9,
    ...extra,
  };
}

const FIVE_CLAIMS = [
  { status: 'downgraded' },
  { status: 'verified' },
  { status: 'verified' },
  { status: 'verified' },
  { status: 'verified' },
];

const keys = (reasons) => reasons.map((r) => r.key);

test('actionability RFC 06: refutation downgraded — jury-штраф не берётся второй раз', () => {
  const withClaims = scoreActionability(
    baseFinding({ advisorClaims: FIVE_CLAIMS, refutationStatus: 'downgraded' }),
    CTX,
  );
  const withoutClaims = scoreActionability(baseFinding(), CTX);

  assert.ok(
    !keys(withClaims.reasons).includes('juryDowngraded'),
    'понижение уже отработало на severity — второй раз не вычитаем',
  );
  assert.equal(withClaims.score, withoutClaims.score, 'права те же, что у находки без claims');
  assert.equal(withClaims.score, 3, 'confidence + logicFile + sensitivePath');
});

test('actionability RFC 06: без refutationStatus=downgraded штраф жюри остаётся', () => {
  for (const refutationStatus of [undefined, 'kept', 'verified', 'downgraded_no_claims']) {
    const f = baseFinding({ advisorClaims: FIVE_CLAIMS });
    if (refutationStatus !== undefined) f.refutationStatus = refutationStatus;
    const { score, reasons } = scoreActionability(f, CTX);
    assert.ok(
      keys(reasons).includes('juryDowngraded'),
      `refutationStatus=${refutationStatus}: severity не понижалась, штраф на месте`,
    );
    assert.equal(score, 1, `refutationStatus=${refutationStatus}: 3 − 2`);
  }
});

test('actionability RFC 06: штраф verifier — отдельный судья, поведение не изменилось', () => {
  for (const verifierStatus of ['downgraded', 'rejected']) {
    const { score, reasons } = scoreActionability(
      baseFinding({ verifierStatus, refutationStatus: 'downgraded' }),
      CTX,
    );
    assert.deepEqual(keys(reasons), ['juryDowngraded', 'confidence', 'sensitivePath', 'logicFile'], verifierStatus);
    assert.equal(score, 1, `${verifierStatus}: 3 − 2, refutationStatus verifier не гасит`);
  }
});

test('actionability RFC 06: понижённая находка доезжает до строки на общих основаниях', () => {
  const finding = baseFinding({
    advisorClaims: FIVE_CLAIMS,
    refutationStatus: 'downgraded',
    suggestion: 'проверить ownerId перед сменой статуса',
    consensus: true,
  });
  const files = new Map([['server/auth/orders.ts', { validLines: new Set([42]) }]]);
  const [scored] = routeFindings([finding], files, {
    inlineOn: ['critical', 'major'],
    threshold: 4,
    maxAdvisoryInline: 5,
    ctx: CTX,
  });
  assert.equal(scored.score, 6, 'confidence + logicFile + suggestion + consensus, без −2');
  assert.equal(scored.route, 'inline');
  assert.equal(scored.promoted, true);
});
