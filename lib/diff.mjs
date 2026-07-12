import { readFile } from 'node:fs/promises';
import { resolve, sep } from 'node:path';

export function parseDiff(diff) {
  const files = new Map();
  const annotatedParts = [];
  const changedLines = [];
  let curPath = null;
  let newLine = 0;

  for (const raw of diff.split('\n')) {
    if (raw.startsWith('diff --git')) { curPath = null; annotatedParts.push(raw); continue; }
    if (raw.startsWith('+++ ')) {
      const m = raw.match(/^\+\+\+ b\/(.+)$/);
      curPath = m ? m[1] : null;
      if (curPath && !files.has(curPath)) files.set(curPath, { validLines: new Set(), changed: [] });
      annotatedParts.push(raw);
      continue;
    }
    if (raw.startsWith('--- ')) { annotatedParts.push(raw); continue; }
    const hunk = raw.match(/^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
    if (hunk) { newLine = Number(hunk[1]); annotatedParts.push(raw); continue; }
    if (raw.startsWith('\\')) { annotatedParts.push(raw); continue; }
    if (!curPath) { annotatedParts.push(raw); continue; }
    const rec = files.get(curPath);
    if (raw.startsWith('+') && !raw.startsWith('+++')) {
      rec.validLines.add(newLine);
      rec.changed.push(raw.slice(1));
      changedLines.push(raw.slice(1));
      annotatedParts.push(`${String(newLine).padStart(5)}: ${raw}`);
      newLine++;
    } else if (raw.startsWith('-') && !raw.startsWith('---')) {
      rec.changed.push(raw.slice(1));
      changedLines.push(raw.slice(1));
      annotatedParts.push(`     : ${raw}`);
    } else {
      rec.validLines.add(newLine);
      annotatedParts.push(`${String(newLine).padStart(5)}: ${raw}`);
      newLine++;
    }
  }
  return { files, annotated: annotatedParts.join('\n'), changedText: changedLines.join('\n') };
}

export function extractChangedSymbols(changedText, maxSymbols = 25) {
  const out = new Set();
  const add = (n) => { if (n && n.length >= 3) out.add(n); };
  const patterns = [
    /\bexport\s+(?:default\s+)?(?:async\s+)?function\s+([A-Za-z_$][\w$]*)/g,
    /\bexport\s+(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)/g,
    /\bexport\s+(?:interface|type|enum)\s+([A-Za-z_$][\w$]*)/g,
    /\bexport\s+const\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:<[^>]*>\s*)?\(/g,
    /\bexport\s+const\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?function/g,
  ];
  for (const re of patterns) { let m; while ((m = re.exec(changedText))) add(m[1]); }
  return [...out].slice(0, maxSymbols);
}

export async function readChangedFiles(files, limits, log = () => {}) {
  const map = new Map();
  const root = resolve(process.cwd());
  for (const path of files.keys()) {
    if (/(^|\/)package-lock\.json$|\.(lock|png|jpe?g|gif|svg|ico|pdf|min\.js|map)$/i.test(path)) continue;
    const abs = resolve(root, path);
    if (abs !== root && !abs.startsWith(root + sep)) { log(`пропуск файла вне репо: ${path}`); continue; }
    let content;
    try { content = await readFile(abs, 'utf8'); } catch { continue; }
    const lines = content.split('\n');
    let body = lines.slice(0, limits.maxFileLines).join('\n');
    if (lines.length > limits.maxFileLines) {
      body += `\n… (показаны первые ${limits.maxFileLines} из ${lines.length} строк)`;
    }
    map.set(path, `### ${path}\n\`\`\`\n${body}\n\`\`\``);
  }
  return map;
}

export function splitSegmentByHunks(seg, maxBytes) {
  if (seg.length <= maxBytes) return [seg];
  const lines = seg.split('\n');
  const firstHunk = lines.findIndex((l) => l.startsWith('@@ '));
  if (firstHunk < 0) {
    const out = [];
    for (let off = 0; off < seg.length; off += maxBytes) out.push(seg.slice(off, off + maxBytes));
    return out;
  }
  const header = lines.slice(0, firstHunk).join('\n');
  const hunks = [];
  let cur = [];
  for (const l of lines.slice(firstHunk)) {
    if (l.startsWith('@@ ') && cur.length) { hunks.push(cur.join('\n')); cur = []; }
    cur.push(l);
  }
  if (cur.length) hunks.push(cur.join('\n'));
  const pieces = [];
  let buf = [], size = header.length;
  const flush = () => { if (buf.length) { pieces.push(`${header}\n${buf.join('\n')}`); buf = []; size = header.length; } };
  for (const h of hunks) {
    if (buf.length && size + h.length + 1 > maxBytes) flush();
    buf.push(h); size += h.length + 1;
  }
  flush();
  return pieces.length ? pieces : [seg];
}

export function buildChunks(annotated, contentMap, limits) {
  const pathOf = (seg) => (seg.match(/^\+\+\+ b\/(.+)$/m)?.[1]) || '';
  const fileSegs = annotated.split(/(?=^diff --git )/m).filter((s) => s.trim());
  let total = 0;
  for (const seg of fileSegs) {
    total += Math.min(seg.length, limits.maxDiffBytes)
      + Math.min((contentMap.get(pathOf(seg)) || '').length, limits.maxContextBytes);
  }
  const effChunk = Math.max(limits.chunkBytes, Math.ceil(total / limits.maxChunks));
  const items = [];
  for (const seg of fileSegs) {
    const diff = seg.length > limits.maxDiffBytes
      ? seg.slice(0, limits.maxDiffBytes) + '\n… (файл обрезан в диффе)' : seg;
    let content = contentMap.get(pathOf(seg)) || '';
    if (content.length > limits.maxContextBytes) {
      content = content.slice(0, limits.maxContextBytes) + '\n… (файл обрезан)';
    }
    splitSegmentByHunks(diff, effChunk).forEach((s) => {
      items.push({ s, content, size: s.length + content.length });
    });
  }
  const chunks = [];
  let curSegs = [], curContent = [], bytes = 0;
  const flush = () => {
    if (!curSegs.length) return;
    chunks.push({ annotated: curSegs.join('\n'), fileContents: curContent.join('\n\n') });
    curSegs = []; curContent = []; bytes = 0;
  };
  for (const it of items) {
    if (curSegs.length && bytes + it.size > effChunk) flush();
    curSegs.push(it.s); if (it.content) curContent.push(it.content); bytes += it.size;
  }
  flush();
  return chunks.length ? chunks : [{ annotated, fileContents: '' }];
}
