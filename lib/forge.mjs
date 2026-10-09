/**
 * RFC 16: шов хостинга кода. Пайплайн, auto-triage и feedback работают с клиентом
 * хостинга через один контракт — тот, что исторически задал createGithubClient:
 *
 *   loadDiff() → { pr: { number, title, html_url }, headSha, diff } | { draft: true } | null
 *   readPriorStickyData() → { findings, runs, sha };  readPriorFindings() → findings
 *   upsertSticky(body) → 'created' | 'updated'
 *   postReview({ inline, headSha, diff }) → ReviewToken | null   (непрозрачен для пайплайна)
 *   deleteStaleInline({ exceptReviewId?: ReviewToken }) → число удалённых
 *   listInlineComments() / readStickyReview() / listIssueLabels() / listPullCommentReactions(id)
 *     → GitHub-форма: { id, body, path, line, user: { login, type? } }, [{ name }], [{ content, user }]
 *   listUnresolvedReviewThreads() → [{ id, path, line, comments: [{ body, author }] }]
 *   replyReviewThread(id, body), resolveReviewThread(id)
 *   selfLogin?() → логин владельца токена (только там, где бот не узнаётся по user.type)
 *
 * Новый хостинг приводит свои ответы к этой форме, а не расширяет её: пайплайн не знает,
 * где работает.
 */

import { createGithubClient } from './github.mjs';
import { createGitlabClient } from './gitlab.mjs';

export const FORGE_KINDS = Object.freeze(['github', 'gitlab']);

/** Переменные окружения, которых не хватает для постинга, — для текста ошибки. */
const CREDS_HINT = {
  github: 'REPO/PR_NUMBER/GITHUB_TOKEN',
  gitlab: 'CI_PROJECT_ID/CI_MERGE_REQUEST_IID/AI_REVIEW_GITLAB_TOKEN',
};

/**
 * Настройки хостинга прогона. Конфиги, собранные до RFC 16 (тесты, swarm-интеграторы),
 * несут только `github` — для них хостинг GitHub.
 */
export function forgeOf(cfg) {
  if (cfg?.forge) return cfg.forge;
  const gh = cfg?.github || {};
  return {
    kind: 'github',
    api: gh.api || '',
    token: gh.token || '',
    repo: gh.repo || '',
    prNumber: gh.prNumber || 0,
    webUrl: gh.repo ? `https://github.com/${gh.repo}` : '',
  };
}

export function forgeCredsHint(cfg) {
  return CREDS_HINT[forgeOf(cfg).kind] || CREDS_HINT.github;
}

/** Клиент хостинга по `cfg.forge.kind`. */
export function createForgeClient(cfg, log, opts = {}) {
  const forge = forgeOf(cfg);
  if (forge.kind === 'gitlab') return createGitlabClient({ ...cfg, forge }, log, opts);
  return createGithubClient(cfg, log, opts);
}
