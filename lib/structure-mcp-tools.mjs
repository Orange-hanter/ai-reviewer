/**
 * Каталог тулза structure-MCP без graph/Rust (RFC 10).
 */
export const STRUCTURE_CONTEXT_TOOL = Object.freeze({
  name: 'structure_context',
  description: 'Return a bounded structure/contracts/test-scenario context pinned to a facts snapshot.',
  inputSchema: {
    type: 'object',
    additionalProperties: false,
    required: ['factsPath', 'assessmentPath'],
    anyOf: [
      { required: ['headSha'] },
      { required: ['snapshotFingerprint'] },
    ],
    properties: {
      factsPath: { type: 'string', minLength: 1 },
      assessmentPath: { type: 'string', minLength: 1 },
      headSha: { type: 'string', pattern: '^[0-9a-fA-F]{7,64}$' },
      snapshotFingerprint: { type: 'string', pattern: '^[0-9a-f]{64}$' },
      scopeRefs: { type: 'array', maxItems: 20, items: { type: 'string', maxLength: 1000 } },
      taskRef: { type: ['string', 'null'], maxLength: 500 },
      contractRefs: { type: 'array', maxItems: 20, items: { type: 'string', maxLength: 500 } },
      tokenBudget: { type: 'integer', minimum: 256, maximum: 32000 },
      dependencyDepth: { type: 'integer', minimum: 0, maximum: 3 },
    },
  },
});
