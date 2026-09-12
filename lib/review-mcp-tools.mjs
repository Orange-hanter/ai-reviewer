/**
 * Каталог тулзов ревью-MCP без пайплайна (RFC 10): handshake / tools/list
 * не импортируют pipeline.mjs.
 */
import { envFlag } from './util.mjs';

const HEAD_SHA_PROP = Object.freeze({ type: 'string', pattern: '^[0-9a-fA-F]{7,64}$' });
const UNCOMMITTED_PROP = Object.freeze({ type: 'boolean' });
const DIFF_PATH_PROP = Object.freeze({ type: 'string', minLength: 1 });
const TOKEN_BUDGET_PROP = Object.freeze({ type: 'integer', minimum: 256, maximum: 32000 });
const EXTRA_PATHS_MAX = 20;

export const REVIEW_RUN_TOOL = Object.freeze({
  name: 'review_run',
  description: 'Run a pinned dry-run review (no GitHub). Returns a bounded preview; use review_findings to page the rest.',
  inputSchema: {
    type: 'object',
    additionalProperties: false,
    required: ['diffPath', 'headSha'],
    properties: {
      diffPath: DIFF_PATH_PROP,
      headSha: HEAD_SHA_PROP,
      tokenBudget: TOKEN_BUDGET_PROP,
      priorExportPath: { type: 'string', minLength: 1 },
      uncommitted: UNCOMMITTED_PROP,
    },
  },
});

export const REVIEW_FINDINGS_TOOL = Object.freeze({
  name: 'review_findings',
  description: 'Page findings of a previous review_run by runFingerprint. Does not invoke the LLM.',
  inputSchema: {
    type: 'object',
    additionalProperties: false,
    required: ['runFingerprint'],
    properties: {
      runFingerprint: { type: 'string', pattern: '^[0-9a-f]{64}$' },
      headSha: HEAD_SHA_PROP,
      file: { type: 'string', maxLength: 1000 },
      severity: { type: 'string', enum: ['critical', 'major', 'minor', 'nit'] },
      offset: { type: 'integer', minimum: 0, maximum: 10000 },
      limit: { type: 'integer', minimum: 1, maximum: 100 },
    },
  },
});

export const REVIEW_ROUTE_TOOL = Object.freeze({
  name: 'review_route',
  description: 'Classify a pinned diff as skip/light/full without invoking the LLM.',
  inputSchema: {
    type: 'object',
    additionalProperties: false,
    required: ['diffPath', 'headSha'],
    properties: {
      diffPath: DIFF_PATH_PROP,
      headSha: HEAD_SHA_PROP,
      uncommitted: UNCOMMITTED_PROP,
    },
  },
});

export const REVIEW_PRESCAN_TOOL = Object.freeze({
  name: 'review_prescan',
  description: 'Run eslint + ast-grep prescan on a pinned diff. Does not invoke the LLM.',
  inputSchema: {
    type: 'object',
    additionalProperties: false,
    required: ['diffPath', 'headSha'],
    properties: {
      diffPath: DIFF_PATH_PROP,
      headSha: HEAD_SHA_PROP,
      uncommitted: UNCOMMITTED_PROP,
      tokenBudget: TOKEN_BUDGET_PROP,
    },
  },
});

export const FP_CHECK_TOOL = Object.freeze({
  name: 'fp_check',
  description: 'Check whether a finding title+body matches a known false-positive pattern for a file.',
  inputSchema: {
    type: 'object',
    additionalProperties: false,
    required: ['title', 'body', 'file'],
    properties: {
      title: { type: 'string' },
      body: { type: 'string' },
      file: { type: 'string' },
      extraPaths: { type: 'array', items: { type: 'string' }, maxItems: EXTRA_PATHS_MAX },
    },
  },
});

export const FINGERPRINT_CHECK_TOOL = Object.freeze({
  name: 'fingerprint_check',
  description: 'Check whether the code under a prior finding fingerprint is still present in a pinned diff.',
  inputSchema: {
    type: 'object',
    additionalProperties: false,
    required: ['file', 'line', 'priorFp', 'diffPath'],
    properties: {
      file: { type: 'string', minLength: 1 },
      line: { type: 'integer', minimum: 1 },
      priorFp: { type: 'string', minLength: 1, maxLength: 64 },
      diffPath: DIFF_PATH_PROP,
      headSha: HEAD_SHA_PROP,
      uncommitted: UNCOMMITTED_PROP,
    },
  },
});

export const REVIEW_ARTIFACT_TOOL = Object.freeze({
  name: 'review_artifact',
  description: 'Read a bounded preview of a pinned review JSON artifact. Does not invoke the LLM.',
  inputSchema: {
    type: 'object',
    additionalProperties: false,
    required: ['path'],
    properties: {
      path: { type: 'string', minLength: 1 },
      headSha: HEAD_SHA_PROP,
      tokenBudget: TOKEN_BUDGET_PROP,
    },
  },
});

export const BLAST_SCOPE_TOOL = Object.freeze({
  name: 'blast_scope',
  description: 'List files outside the diff affected by changed symbols (Graphify). Does not invoke the LLM.',
  inputSchema: {
    type: 'object',
    additionalProperties: false,
    required: ['diffPath', 'headSha'],
    properties: {
      diffPath: DIFF_PATH_PROP,
      headSha: HEAD_SHA_PROP,
      uncommitted: UNCOMMITTED_PROP,
    },
  },
});

export const ACTIONABILITY_EXPLAIN_TOOL = Object.freeze({
  name: 'actionability_explain',
  description: 'Explain actionability score and reasons for one finding. Does not mutate the finding or invoke the LLM.',
  inputSchema: {
    type: 'object',
    additionalProperties: false,
    required: ['finding'],
    properties: {
      finding: {
        type: 'object',
        additionalProperties: false,
        required: ['file', 'line', 'severity', 'title', 'body'],
        properties: {
          file: { type: 'string' },
          line: { type: 'integer' },
          severity: { type: 'string' },
          title: { type: 'string' },
          body: { type: 'string' },
          verifierStatus: { type: 'string' },
          prescan: {},
          advisorClaims: { type: 'array' },
          refutationStatus: { type: 'string' },
        },
      },
    },
  },
});

export const REVIEW_METRICS_TOOL = Object.freeze({
  name: 'review_metrics',
  description: 'Aggregate a confined observability JSONL file. Does not invoke the LLM.',
  inputSchema: {
    type: 'object',
    additionalProperties: false,
    required: ['path'],
    properties: {
      path: { type: 'string', minLength: 1 },
    },
  },
});

const DEFAULT_TOOLS = Object.freeze([
  REVIEW_RUN_TOOL,
  REVIEW_FINDINGS_TOOL,
  REVIEW_ROUTE_TOOL,
  REVIEW_PRESCAN_TOOL,
  FP_CHECK_TOOL,
  FINGERPRINT_CHECK_TOOL,
  REVIEW_ARTIFACT_TOOL,
]);

const EXTRA_TOOLS = Object.freeze([
  BLAST_SCOPE_TOOL,
  ACTIONABILITY_EXPLAIN_TOOL,
  REVIEW_METRICS_TOOL,
]);

export function extraEnabled(extra = {}) {
  return envFlag('REVIEW_MCP_EXTRA', false, extra.env ?? process.env);
}

export function listedTools(extra = {}) {
  return extraEnabled(extra) ? [...DEFAULT_TOOLS, ...EXTRA_TOOLS] : [...DEFAULT_TOOLS];
}
