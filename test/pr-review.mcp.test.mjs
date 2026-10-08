// MCP полного прогона (RFC 08): handleReviewMcpRequest + DI пайплайна, без сети и LLM.
import './helpers/reference-profile.mjs';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { writeFile, rm, mkdtemp, readdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const {
  handleReviewMcpRequest, resetReviewMcpStore, REVIEW_RUN_TOOL, REVIEW_FINDINGS_TOOL,
  REVIEW_ROUTE_TOOL, REVIEW_PRESCAN_TOOL, FP_CHECK_TOOL, FINGERPRINT_CHECK_TOOL,
  REVIEW_ARTIFACT_TOOL, BLAST_SCOPE_TOOL, ACTIONABILITY_EXPLAIN_TOOL, REVIEW_METRICS_TOOL,
  PipelineSoftFail, parseDiff, findingFingerprint,
} = await import('../index.mjs');

const HEAD = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
const CODE_DIFF = `diff --git a/foo.ts b/foo.ts
--- a/foo.ts
+++ b/foo.ts
@@ -1,2 +1,3 @@
 const a = 1;
-const b = 2;
+const b = 3;
+export function foo() { return a; }
`;

const ENV = {
  OLLAMA_API_KEY: 'k',
  GITHUB_TOKEN: 'should-not-post',
  REVIEW_MODE: 'consensus',
  OLLAMA_MODELS: 'm1,m2',
  REVIEW_OBSERVABILITY: '0',
  REVIEW_EXPORT: '0',
  REVIEW_AUTO_TRIAGE: '0',
  REVIEW_VERIFIER: '0',
};

function criticalFinding(title = 'реальный баг') {
  return {
    file: 'foo.ts', line: 3, severity: 'critical', title, body: 'null pointer',
    confidence: 0.9, model: 'm1',
  };
}

const criticalReview = {
  model: 'm1', summary: 'нашёл баг', verdict: 'changes_requested',
  findings: [criticalFinding()],
};

function pipelineDeps({ reviews = [criticalReview], engineHold } = {}) {
  return {
    graphify: { buildBlastRadius: async () => [] },
    prescan: {
      run: async () => ({
        findings: [], scanned: 0, skipped: 0, truncated: false,
        lintableTotal: 0, byTool: null, error: null, promptSection: '',
      }),
    },
    engine: {
      reviewAllModels: async () => {
        if (engineHold) await engineHold;
        return reviews;
      },
      verifyFindings: async () => [],
    },
  };
}

function runCall(arguments_) {
  return {
    jsonrpc: '2.0',
    id: 1,
    method: 'tools/call',
    params: { name: 'review_run', arguments: arguments_ },
  };
}

function findingsCall(arguments_) {
  return {
    jsonrpc: '2.0',
    id: 2,
    method: 'tools/call',
    params: { name: 'review_findings', arguments: arguments_ },
  };
}

function toolCall(name, arguments_, id = 3) {
  return {
    jsonrpc: '2.0',
    id,
    method: 'tools/call',
    params: { name, arguments: arguments_ },
  };
}

function cleanGit(head = HEAD) {
  return {
    isInsideWorkTree: async () => true,
    revParseHead: async () => head,
    statusPorcelain: async () => '',
  };
}

function extraBase(root, overrides = {}) {
  return {
    root,
    env: { ...ENV, REVIEW_MCP_ROOT: root },
    pipelineDeps: pipelineDeps(),
    githubCalls: { postReview: 0, upsertSticky: 0, deleteStaleInline: 0, loadDiff: 0 },
    ...overrides,
  };
}

async function withRoot(fn) {
  const root = await mkdtemp(join(tmpdir(), 'review-mcp-'));
  try {
    return await fn(root);
  } finally {
    resetReviewMcpStore();
    await rm(root, { recursive: true, force: true });
  }
}

test('tools/list объявляет дешёвый контур без REVIEW_MCP_EXTRA', async () => {
  const listed = await handleReviewMcpRequest(
    { jsonrpc: '2.0', id: 1, method: 'tools/list' },
    { env: {} },
  );
  assert.deepEqual(listed.result.tools.map((t) => t.name), [
    REVIEW_RUN_TOOL.name, REVIEW_FINDINGS_TOOL.name,
    REVIEW_ROUTE_TOOL.name, REVIEW_PRESCAN_TOOL.name,
    FP_CHECK_TOOL.name, FINGERPRINT_CHECK_TOOL.name, REVIEW_ARTIFACT_TOOL.name,
  ]);
});

test('tools/list с REVIEW_MCP_EXTRA=1 добавляет фазу C', async () => {
  const listed = await handleReviewMcpRequest(
    { jsonrpc: '2.0', id: 1, method: 'tools/list' },
    { env: { REVIEW_MCP_EXTRA: '1' } },
  );
  assert.deepEqual(listed.result.tools.map((t) => t.name), [
    REVIEW_RUN_TOOL.name, REVIEW_FINDINGS_TOOL.name,
    REVIEW_ROUTE_TOOL.name, REVIEW_PRESCAN_TOOL.name,
    FP_CHECK_TOOL.name, FINGERPRINT_CHECK_TOOL.name, REVIEW_ARTIFACT_TOOL.name,
    BLAST_SCOPE_TOOL.name, ACTIONABILITY_EXPLAIN_TOOL.name, REVIEW_METRICS_TOOL.name,
  ]);
});

test('review_run без headSha / diffPath — -32602', async () => {
  resetReviewMcpStore();
  const noPin = await handleReviewMcpRequest(runCall({ diffPath: 'a.patch' }));
  assert.equal(noPin.error.code, -32602);
  assert.match(noPin.error.message, /headSha/);
  const noDiff = await handleReviewMcpRequest(runCall({ headSha: HEAD }));
  assert.equal(noDiff.error.code, -32602);
  assert.match(noDiff.error.message, /diffPath/);
});

test('review_run: путь вне REVIEW_MCP_ROOT — escape, файл не читается', async () => {
  await withRoot(async (root) => {
    const outside = await mkdtemp(join(tmpdir(), 'review-mcp-out-'));
    try {
      const patch = join(outside, 'x.patch');
      await writeFile(patch, CODE_DIFF);
      const githubCalls = { postReview: 0, upsertSticky: 0, deleteStaleInline: 0, loadDiff: 0 };
      const extra = {
        root,
        env: { ...ENV, REVIEW_MCP_ROOT: root },
        pipelineDeps: pipelineDeps(),
        githubCalls,
      };
      const res = await handleReviewMcpRequest(runCall({ diffPath: patch, headSha: HEAD }), extra);
      assert.equal(res.error.code, -32602);
      assert.match(res.error.message, /escapes allowed root/);
      assert.equal(githubCalls.postReview, 0);
    } finally {
      await rm(outside, { recursive: true, force: true });
    }
  });
});

test('review_run: github.postReview / upsertSticky не вызываются даже с GITHUB_TOKEN', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const githubCalls = { postReview: 0, upsertSticky: 0, deleteStaleInline: 0, loadDiff: 0 };
    const extra = {
      root,
      env: { ...ENV, REVIEW_MCP_ROOT: root },
      pipelineDeps: pipelineDeps(),
      githubCalls,
    };
    const res = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
    }), extra);
    assert.equal(res.error, undefined, res.error?.message);
    const preview = res.result.structuredContent;
    assert.equal(preview.kind, 'review_preview');
    assert.equal(preview.headSha, HEAD);
    assert.equal(preview.previewWouldBlock, true);
    assert.equal(preview.status, 'completed');
    assert.equal('wouldBlock' in preview, false, 'гейт-поле wouldBlock не светим на верхнем уровне');
    assert.equal(githubCalls.postReview, 0);
    assert.equal(githubCalls.upsertSticky, 0);
  });
});

test('review_run: process.exit не вызывается на soft-fail die', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const exits = [];
    const orig = process.exit;
    process.exit = (code) => { exits.push(code); throw new Error(`process.exit(${code})`); };
    try {
      const extra = {
        root,
        env: { REVIEW_MODE: 'consensus', OLLAMA_MODELS: 'm1', REVIEW_OBSERVABILITY: '0', REVIEW_EXPORT: '0' },
        pipelineDeps: pipelineDeps(),
      };
      const res = await handleReviewMcpRequest(runCall({
        diffPath: join(root, 'changes.patch'),
        headSha: HEAD,
      }), extra);
      assert.equal(exits.length, 0, 'process.exit не должен вызываться');
      assert.equal(res.error, undefined, res.error?.message);
      assert.equal(res.result.structuredContent.status, 'degraded');
      assert.match(res.result.structuredContent.reason, /LLM API key|OLLAMA_API_KEY/);
    } finally {
      process.exit = orig;
    }
  });
});

test('review_run: тот же пин пока running — stub, другой пин — already running', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    await writeFile(join(root, 'other.patch'), CODE_DIFF);
    let release;
    const hold = new Promise((r) => { release = r; });
    const extra = {
      root,
      env: { ...ENV, REVIEW_MCP_ROOT: root },
      pipelineDeps: pipelineDeps({ engineHold: hold }),
    };
    const first = handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
    }), extra);
    const same = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
    }), extra);
    assert.equal(same.error, undefined, same.error?.message);
    assert.equal(same.result.structuredContent.status, 'running');
    assert.match(same.result.structuredContent.jobId, /^[0-9a-f]{64}$/);
    const other = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'other.patch'),
      headSha: HEAD,
    }), extra);
    assert.equal(other.error.code, -32602);
    assert.match(other.error.message, /already running/);
    release();
    const done = await first;
    assert.equal(done.error, undefined, done.error?.message);
    assert.equal(done.result.structuredContent.status, 'completed');
    assert.equal(done.result.structuredContent.jobId, same.result.structuredContent.jobId);
  });
});

test('review_run укладывается в tokenBudget; хвост — через review_findings', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const findings = Array.from({ length: 12 }, (_, i) => ({
      ...criticalFinding(`баг ${i} ${'x'.repeat(80)}`),
      line: 3 + i,
    }));
    const extra = {
      root,
      env: { ...ENV, REVIEW_MCP_ROOT: root },
      pipelineDeps: pipelineDeps({
        reviews: [{ ...criticalReview, findings }],
      }),
    };
    const res = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
      tokenBudget: 256,
    }), extra);
    const preview = res.result.structuredContent;
    assert.equal(preview.truncated, true);
    const tokens = Math.ceil(JSON.stringify(preview).length / 4);
    assert.ok(tokens <= 256, `preview ${tokens} > tokenBudget 256`);
    assert.match(preview.runFingerprint, /^[0-9a-f]{64}$/);

    const page = await handleReviewMcpRequest(findingsCall({
      runFingerprint: preview.runFingerprint,
      headSha: HEAD,
      limit: 5,
    }), extra);
    assert.equal(page.error, undefined, page.error?.message);
    assert.ok(page.result.structuredContent.findings.length >= 1);
    assert.ok(page.result.structuredContent.total >= preview.blockers.length);

    const mismatch = await handleReviewMcpRequest(findingsCall({
      runFingerprint: preview.runFingerprint,
      headSha: 'bbbbbbb',
    }), extra);
    assert.equal(mismatch.error.code, -32602);
    assert.match(mismatch.error.message, /does not match run/);
  });
});

test('tokenBudget клампится к схеме, клиенту не доверяем', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const extra = {
      root,
      env: { ...ENV, REVIEW_MCP_ROOT: root },
      pipelineDeps: pipelineDeps(),
    };
    const res = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
      tokenBudget: 1,
    }), extra);
    const preview = res.result.structuredContent;
    const tokens = Math.ceil(JSON.stringify(preview).length / 4);
    assert.ok(tokens <= 256, `кламп минимума схемы: ${tokens}`);
  });
});

test('PipelineSoftFail экспортируется для MCP die', () => {
  const err = new PipelineSoftFail('нет ключа');
  assert.equal(err.name, 'PipelineSoftFail');
});

test('review_findings после resetReviewMcpStore читает sidecar с диска', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const extra = extraBase(root);
    const res = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
    }), extra);
    const preview = res.result.structuredContent;
    resetReviewMcpStore();
    const page = await handleReviewMcpRequest(findingsCall({
      runFingerprint: preview.runFingerprint,
      headSha: HEAD,
    }), extra);
    assert.equal(page.error, undefined, page.error?.message);
    assert.ok(page.result.structuredContent.findings.length >= 1);
    assert.equal(page.result.structuredContent.headSha, HEAD);
  });
});

test('wait:false: stub до MoA, findings running, затем completed по jobId', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    let release;
    const hold = new Promise((r) => { release = r; });
    let background;
    const extra = extraBase(root, {
      pipelineDeps: pipelineDeps({ engineHold: hold }),
      captureRun: (p) => { background = p; },
    });
    const res = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
      wait: false,
    }), extra);
    assert.equal(res.error, undefined, res.error?.message);
    const stub = res.result.structuredContent;
    assert.equal(stub.status, 'running');
    assert.match(stub.jobId, /^[0-9a-f]{64}$/);
    assert.equal(stub.runFingerprint, '');
    assert.equal(stub.previewWouldBlock, false);
    const names = await readdir(join(root, '.ai-review/mcp/runs'));
    assert.deepEqual(names, [`${stub.jobId}.json`]);

    const page = await handleReviewMcpRequest(findingsCall({ jobId: stub.jobId, headSha: HEAD }), extra);
    assert.equal(page.error, undefined, page.error?.message);
    assert.equal(page.result.structuredContent.status, 'running');
    assert.deepEqual(page.result.structuredContent.findings, []);
    assert.equal(page.result.structuredContent.total, 0);

    const art = await handleReviewMcpRequest(toolCall('review_artifact', { jobId: stub.jobId }), extra);
    assert.equal(art.result.structuredContent.status, 'running');
    assert.equal(art.result.structuredContent.previewWouldBlock, false);

    release();
    await background;
    const done = await handleReviewMcpRequest(findingsCall({ jobId: stub.jobId }), extra);
    assert.equal(done.error, undefined, done.error?.message);
    assert.equal(done.result.structuredContent.status, 'completed');
    assert.ok(done.result.structuredContent.findings.length >= 1);
    assert.match(done.result.structuredContent.runFingerprint, /^[0-9a-f]{64}$/);

    resetReviewMcpStore();
    const fromDisk = await handleReviewMcpRequest(findingsCall({
      runFingerprint: done.result.structuredContent.runFingerprint,
    }), extra);
    assert.equal(fromDisk.error, undefined, fromDisk.error?.message);
    assert.ok(fromDisk.result.structuredContent.findings.length >= 1);

    const again = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
      wait: false,
    }), extra);
    assert.equal(again.result.structuredContent.status, 'completed');
    assert.equal(again.result.structuredContent.jobId, stub.jobId);
  });
});

test('git: headSha does not match HEAD', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const extra = extraBase(root, { git: cleanGit(HEAD) });
    const res = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: 'bbbbbbb',
    }), extra);
    assert.equal(res.error.code, -32602);
    assert.match(res.error.message, /headSha does not match HEAD/);
  });
});

test('пин: не-hex headSha назван форматом, а не «required»', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const extra = extraBase(root, { git: cleanGit(HEAD) });
    const res = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: 'gold-case-id',
    }), extra);
    assert.equal(res.error.code, -32602);
    assert.match(res.error.message, /headSha must be 7–64 hex chars/);
    const missing = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
    }), extra);
    assert.match(missing.error.message, /headSha is required/);
  });
});

test('git: грязное дерево без uncommitted', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const extra = extraBase(root, {
      git: { ...cleanGit(), statusPorcelain: async () => ' M foo.ts' },
    });
    const res = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
    }), extra);
    assert.equal(res.error.code, -32602);
    assert.match(res.error.message, /working tree dirty; pass uncommitted: true/);
  });
});

test('git: uncommitted: true проходит на грязном дереве', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const extra = extraBase(root, {
      git: { ...cleanGit(), statusPorcelain: async () => ' M foo.ts' },
    });
    const res = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
      uncommitted: true,
    }), extra);
    assert.equal(res.error, undefined, res.error?.message);
    assert.equal(res.result.structuredContent.kind, 'review_preview');
  });
});

test('prefix-пин: 7 hex совпадает с 40-символьным HEAD', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const extra = extraBase(root, { git: cleanGit(HEAD) });
    const res = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD.slice(0, 7),
    }), extra);
    assert.equal(res.error, undefined, res.error?.message);
    assert.equal(res.result.structuredContent.headSha, HEAD.slice(0, 7));
  });
});

test('priorExportPath: silent github получает findings, GitHub не пишется', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const priorPath = join(root, 'prior.json');
    const priorFinding = { file: 'foo.ts', line: 3, title: 'старый', fp: 'abc123abc123' };
    await writeFile(priorPath, JSON.stringify({
      findings: [priorFinding],
      verdict: { runs: [{ sha: HEAD }] },
      headSha: HEAD,
    }));
    let seenPrior;
    const extra = extraBase(root, {
      githubCalls: { postReview: 0, upsertSticky: 0, deleteStaleInline: 0, loadDiff: 0 },
      createPipeline: (_cfg, deps) => ({
        runDetailed: async () => {
          seenPrior = await deps.github.readPriorStickyData();
          return {
            artifact: { findings: [], blockers: [], wouldBlock: false, headSha: HEAD },
            wouldBlock: false,
            degraded: false,
          };
        },
      }),
    });
    const res = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
      priorExportPath: priorPath,
    }), extra);
    assert.equal(res.error, undefined, res.error?.message);
    assert.equal(seenPrior.findings[0].title, 'старый');
    assert.equal(extra.githubCalls.postReview, 0);
    assert.equal(extra.githubCalls.upsertSticky, 0);
  });
});

test('priorExportPath вне корня — path escapes allowed root', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const outside = await mkdtemp(join(tmpdir(), 'review-mcp-prior-'));
    try {
      const prior = join(outside, 'prior.json');
      await writeFile(prior, '{"findings":[]}');
      const extra = extraBase(root);
      const res = await handleReviewMcpRequest(runCall({
        diffPath: join(root, 'changes.patch'),
        headSha: HEAD,
        priorExportPath: prior,
      }), extra);
      assert.equal(res.error.code, -32602);
      assert.match(res.error.message, /escapes allowed root/);
    } finally {
      await rm(outside, { recursive: true, force: true });
    }
  });
});

test('priorExportPath: битый JSON — ошибка парсера, не hang', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    await writeFile(join(root, 'prior.json'), '{not-json');
    const extra = extraBase(root);
    const res = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
      priorExportPath: join(root, 'prior.json'),
    }), extra);
    assert.equal(res.error.code, -32602);
    assert.match(res.error.message, /JSON|Unexpected|not-json|position/i);
  });
});

test('review_findings: limit клампится к схеме (150 → 100, omit → 20)', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const extra = extraBase(root);
    const res = await handleReviewMcpRequest(runCall({
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
    }), extra);
    const fp = res.result.structuredContent.runFingerprint;
    const clamped = await handleReviewMcpRequest(findingsCall({
      runFingerprint: fp, limit: 150,
    }), extra);
    assert.equal(clamped.result.structuredContent.limit, 100);
    const omitted = await handleReviewMcpRequest(findingsCall({
      runFingerprint: fp,
    }), extra);
    assert.equal(omitted.result.structuredContent.limit, 20);
  });
});

test('review_route: logic-дифф → full, без GitHub', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const extra = extraBase(root);
    const res = await handleReviewMcpRequest(toolCall('review_route', {
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
    }), extra);
    assert.equal(res.error, undefined, res.error?.message);
    const body = res.result.structuredContent;
    assert.equal(body.kind, 'review_route');
    assert.equal(body.route, 'full');
    assert.equal(extra.githubCalls.postReview, 0);
  });
});

test('review_prescan не вызывает engine', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    let engineCalls = 0;
    const extra = extraBase(root, {
      prescanExec: async () => ({
        findings: [{ file: 'foo.ts', line: 3, severity: 'major', title: 'lint', body: 'x' }],
        scanned: 1, lintableTotal: 1, truncated: false, skipped: 0,
        byTool: { eslint: 1, astgrep: 0 },
      }),
      pipelineDeps: {
        engine: {
          reviewAllModels: async () => { engineCalls++; return []; },
          verifyFindings: async () => { engineCalls++; return []; },
        },
      },
    });
    const res = await handleReviewMcpRequest(toolCall('review_prescan', {
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
    }), extra);
    assert.equal(res.error, undefined, res.error?.message);
    const body = res.result.structuredContent;
    assert.equal(body.kind, 'review_prescan');
    assert.equal(body.status, 'completed');
    assert.equal(body.findings[0].title, 'lint');
    assert.equal(engineCalls, 0);
    assert.equal(extra.githubCalls.postReview, 0);
  });
});

test('fp_check: requiresPath не матчится на чужом файле', async () => {
  const extra = { env: {}, githubCalls: { postReview: 0, upsertSticky: 0, deleteStaleInline: 0, loadDiff: 0 } };
  const title = 'typedef Finding не описывает blast-radius';
  const body = 'softBlast дублирует проверку через два поля';
  const hit = await handleReviewMcpRequest(toolCall('fp_check', {
    title, body, file: 'lib/findings.mjs',
  }), extra);
  assert.equal(hit.error, undefined, hit.error?.message);
  assert.equal(hit.result.structuredContent.match, true);
  assert.equal(hit.result.structuredContent.patternId, 'softblast-typedef-cosmetic');
  const miss = await handleReviewMcpRequest(toolCall('fp_check', {
    title, body, file: 'lib/other.mjs',
  }), extra);
  assert.equal(miss.result.structuredContent.match, false);
  assert.equal(miss.result.structuredContent.patternId, null);
  const empty = await handleReviewMcpRequest(toolCall('fp_check', {
    title, body, file: '  ',
  }), extra);
  assert.equal(empty.error.code, -32602);
  assert.match(empty.error.message, /file is required/);
  assert.equal(extra.githubCalls.postReview, 0);
});

test('fingerprint_check: still_present / stale / unknown_line', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const { files } = parseDiff(CODE_DIFF);
    const rec = files.get('foo.ts');
    const priorFp = findingFingerprint(rec, 3);
    assert.ok(priorFp);
    const extra = extraBase(root);
    const present = await handleReviewMcpRequest(toolCall('fingerprint_check', {
      file: 'foo.ts', line: 3, priorFp, diffPath: join(root, 'changes.patch'),
    }), extra);
    assert.equal(present.error, undefined, present.error?.message);
    assert.equal(present.result.structuredContent.status, 'still_present');
    assert.equal(present.result.structuredContent.line, 3);

    const unknown = await handleReviewMcpRequest(toolCall('fingerprint_check', {
      file: 'foo.ts', line: 999, priorFp: 'deadbeef0000', diffPath: join(root, 'changes.patch'),
    }), extra);
    assert.equal(unknown.result.structuredContent.status, 'unknown_line');

    const staleDiff = `diff --git a/foo.ts b/foo.ts
--- a/foo.ts
+++ b/foo.ts
@@ -1,2 +1,3 @@
 const a = 1;
-const b = 2;
+const b = 3;
+export function bar() { return 0; }
`;
    await writeFile(join(root, 'stale.patch'), staleDiff);
    const stale = await handleReviewMcpRequest(toolCall('fingerprint_check', {
      file: 'foo.ts', line: 3, priorFp, diffPath: join(root, 'stale.patch'),
    }), extra);
    assert.equal(stale.result.structuredContent.status, 'stale');
    assert.equal(extra.githubCalls.postReview, 0);
  });
});

test('review_artifact: slim обёртка, previewWouldBlock, confine', async () => {
  await withRoot(async (root) => {
    const artifactPath = join(root, 'orakul-ai-review.json');
    await writeFile(artifactPath, JSON.stringify({
      headSha: HEAD,
      wouldBlock: true,
      blockers: [criticalFinding()],
      findings: [criticalFinding(), { ...criticalFinding('nit'), severity: 'nit', line: 4 }],
      counts: { findings: 2, blockers: 1 },
      router: { route: 'full' },
      swarm: { strong: true },
    }));
    const extra = extraBase(root);
    const res = await handleReviewMcpRequest(toolCall('review_artifact', {
      path: artifactPath,
      headSha: HEAD,
    }), extra);
    assert.equal(res.error, undefined, res.error?.message);
    const body = res.result.structuredContent;
    assert.equal(body.kind, 'review_artifact');
    assert.equal(body.previewWouldBlock, true);
    assert.equal('wouldBlock' in body, false);
    assert.equal(body.strong, true);
    assert.ok(body.blockers.length >= 1);
    const outside = await mkdtemp(join(tmpdir(), 'review-mcp-art-'));
    try {
      const escaped = join(outside, 'x.json');
      await writeFile(escaped, '{"wouldBlock":true}');
      const bad = await handleReviewMcpRequest(toolCall('review_artifact', {
        path: escaped,
      }), extra);
      assert.match(bad.error.message, /escapes allowed root/);
    } finally {
      await rm(outside, { recursive: true, force: true });
    }
  });
});

test('фаза C без флага — unknown tool', async () => {
  const res = await handleReviewMcpRequest(toolCall('blast_scope', {
    diffPath: 'a.patch', headSha: HEAD,
  }), { env: {} });
  assert.equal(res.error.code, -32602);
  assert.match(res.error.message, /unknown tool/);
});

test('blast_scope: suggestedScopeFiles, без LLM', async () => {
  await withRoot(async (root) => {
    await writeFile(join(root, 'changes.patch'), CODE_DIFF);
    const extra = extraBase(root, {
      env: { ...ENV, REVIEW_MCP_ROOT: root, REVIEW_MCP_EXTRA: '1' },
      graphify: {
        buildBlastRadius: async () => ({
          metrics: { affected_files: ['lib/a.ts', 'foo.ts'], skip_reason: 'none' },
        }),
      },
    });
    const res = await handleReviewMcpRequest(toolCall('blast_scope', {
      diffPath: join(root, 'changes.patch'),
      headSha: HEAD,
    }), extra);
    assert.equal(res.error, undefined, res.error?.message);
    const body = res.result.structuredContent;
    assert.equal(body.kind, 'blast_scope');
    assert.deepEqual(body.files, ['lib/a.ts']);
    assert.equal(extra.githubCalls.postReview, 0);
  });
});

test('actionability_explain не мутирует находку', async () => {
  const finding = {
    file: 'foo.ts', line: 3, severity: 'minor', title: 'x', body: 'y',
    verifierStatus: 'verified',
  };
  const extra = { env: { REVIEW_MCP_EXTRA: '1' } };
  const res = await handleReviewMcpRequest(toolCall('actionability_explain', { finding }), extra);
  assert.equal(res.error, undefined, res.error?.message);
  const body = res.result.structuredContent;
  assert.equal(body.kind, 'actionability_explain');
  assert.equal(typeof body.score, 'number');
  assert.ok(Array.isArray(body.reasons));
  assert.equal('actionabilityScore' in finding, false);
});

test('review_metrics: JSONL, битая строка не роняет', async () => {
  await withRoot(async (root) => {
    const path = join(root, 'obs.jsonl');
    await writeFile(path, [
      '{"type":"run_end","wouldBlock":true}',
      'not-json',
      '{"type":"run_end","wouldBlock":false}',
      '',
    ].join('\n'));
    const extra = extraBase(root, {
      env: { ...ENV, REVIEW_MCP_ROOT: root, REVIEW_MCP_EXTRA: '1' },
    });
    const res = await handleReviewMcpRequest(toolCall('review_metrics', { path }), extra);
    assert.equal(res.error, undefined, res.error?.message);
    const body = res.result.structuredContent;
    assert.equal(body.kind, 'review_metrics');
    assert.equal(body.runs.total, 2);
    assert.equal(body.runs.blocked, 1);
  });
});

