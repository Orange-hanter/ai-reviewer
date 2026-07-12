/** Загрузка FP-паттернов из JSONL (R8 → filter). */

import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dir = dirname(fileURLToPath(import.meta.url));
export const DEFAULT_FP_PATTERNS_PATH = resolve(__dir, '../feedback/fp-patterns.jsonl');
export const MAX_FP_REGEX_LENGTH = 512;

/** @typedef {{ id: string, regex: RegExp, requiresPath?: RegExp, matchChangedBasename?: boolean }} FpPattern */

export function isSafeRegexSource(source) {
  const s = String(source || '');
  if (!s || s.length > MAX_FP_REGEX_LENGTH) return false;
  if (/\(\?[^)]*[+*][^)]*\)[+*]/.test(s)) return false;
  return true;
}

export function parseFpPatternsText(text) {
  const patterns = [];
  for (const line of String(text || '').split('\n')) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;
    let row;
    try {
      row = JSON.parse(trimmed);
    } catch {
      continue;
    }
    if (!row?.id || !row?.regex || !isSafeRegexSource(row.regex)) continue;
    const req = row.requiresPath;
    if (req && !isSafeRegexSource(req)) continue;
    try {
      patterns.push({
        id: String(row.id),
        regex: new RegExp(row.regex, 'i'),
        requiresPath: req ? new RegExp(req, 'i') : null,
        matchChangedBasename: Boolean(row.matchChangedBasename),
      });
    } catch {
      /* skip invalid regex */
    }
  }
  return patterns;
}

export function loadFpPatternsSync(file = DEFAULT_FP_PATTERNS_PATH) {
  for (const path of [file, DEFAULT_FP_PATTERNS_PATH]) {
    try {
      const parsed = parseFpPatternsText(readFileSync(path, 'utf8'));
      if (parsed.length) return parsed;
    } catch {
      /* try fallback path */
    }
  }
  return [];
}

let cachedPatterns = null;
let cachedPath = null;

export function getFpPatterns(file = DEFAULT_FP_PATTERNS_PATH) {
  if (cachedPatterns && cachedPath === file) return cachedPatterns;
  cachedPatterns = loadFpPatternsSync(file);
  cachedPath = file;
  return cachedPatterns;
}

/** @param {string} [patternsFile] */
export function resetFpPatternsCache(patternsFile) {
  if (patternsFile === undefined || patternsFile === cachedPath) {
    cachedPatterns = null;
    cachedPath = null;
  }
}

/** @param {FpPattern[]} patterns */
export function matchFpPatterns(text, changedPaths, patterns) {
  const paths = changedPaths || [];
  for (const p of patterns) {
    if (!p.regex.test(text)) continue;
    if (p.requiresPath) {
      const hit = paths.some((path) => p.requiresPath.test(path));
      if (!hit) continue;
    }
    if (p.matchChangedBasename) {
      let basenameHit = false;
      for (const path of paths) {
        const b = path.split('/').pop();
        if (b && text.includes(b)) {
          basenameHit = true;
          break;
        }
      }
      if (!basenameHit) continue;
    }
    return p.id;
  }
  return null;
}
