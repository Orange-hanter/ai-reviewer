/**
 * Фаза 1, шаг 2: lib/json.mjs — тонкая JS-обёртка над Rust-бинарём
 * rust/target/debug/json_repair (порт толерантного извлечения/ремонта JSON).
 *
 * Публичное API не изменилось (те же экспорты, та же синхронность). Все потребители
 * (lib/llm.mjs, lib/github.mjs, lib/feedback.mjs, index.mjs, тесты) вызывают функции
 * СИНХРОННО, поэтому вместо async execFile используется execFileSync: бинарь вызывается
 * на каждую операцию с JSON {"op", ...args} на stdin, ответ — JSON с результатом.
 *
 * Резолвинг бинаря (по аналогии с lib/diff.mjs):
 *   AI_REVIEWER_JSON_REPAIR_BIN — полный путь/имя бинаря (override);
 *   по умолчанию — <repo>/rust/target/debug/json_repair;
 *   AI_REVIEWER_JSON_REPAIR_FORCE_JS=1 — принудительно чистый JS (паритетные прогоны/отладка).
 *
 * Если бинарь не существует (чистый клон до первого `cargo build` — в CI npm test/eval
 * идут раньше cargo build, см. .github/workflows/ci.yml), обёртка деградирует на чистый
 * JS fallback ниже — эталонную копию исходной реализации. Если бинарь ЕСТЬ, но упал
 * (non-zero exit) — ошибка пробрасывается с восстановленными флагами (badEscape,
 * truncated), fallback не используется: молчаливая подмена реализации при битом
 * Rust-коде скрывала бы регрессии.
 *
 * Нюансы паритета Rust-порта (проверены parity-прогоном rust/parity/json-repair.parity.mjs):
 *   - числа переживают round-trip через stdout в JS-семантике (1e999 → Infinity);
 *   - непарные \uD800-суррогаты: V8 кладёт lone surrogate, Rust — U+FFFD (String не
 *     может хранить lone surrogate); успех/ошибка совпадают;
 *   - вложенность > 4096 уровней: Rust даёт чистую ошибку, V8 парсит (stack-guard);
 *   - тексты внутренних ошибок парсера (V8: "Unexpected token ...") не воспроизводятся —
 *     используются фиксированные русские тексты из JS-веток.
 */

import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dir = dirname(fileURLToPath(import.meta.url));
const DEFAULT_JSON_REPAIR_BIN = resolve(__dir, '../rust/target/debug/json_repair');

function resolveJsonRepairBin() {
  if (process.env.AI_REVIEWER_JSON_REPAIR_FORCE_JS === '1') return null;
  const bin = (process.env.AI_REVIEWER_JSON_REPAIR_BIN || '').trim() || DEFAULT_JSON_REPAIR_BIN;
  return existsSync(bin) ? bin : null;
}

/**
 * Вызов Rust-бинаря; null — бинарь недоступен (fallback на JS). Доменные ошибки
 * extractJsonObject приходят как exit 2 + {"error", "badEscape", "truncated"} в stderr
 * и reconstruct'ятся в JS-Error с теми же флагами (как у чистого JS fallback).
 */
function runRust(op, args) {
  const bin = resolveJsonRepairBin();
  if (!bin) return null;
  let raw;
  try {
    raw = execFileSync(bin, [], {
      input: JSON.stringify({ op, ...args }),
      encoding: 'utf8',
      maxBuffer: 128 * 1024 * 1024,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
  } catch (e) {
    const info = parseErrorPayload(e?.stderr);
    if (info) {
      throw Object.assign(new Error(info.error), {
        badEscape: !!info.badEscape,
        truncated: !!info.truncated,
      });
    }
    throw e; // бинарь есть, но упал вне контракта — не подменяем fallback-ом
  }
  return JSON.parse(raw);
}

function parseErrorPayload(stderr) {
  try {
    const info = JSON.parse(String(stderr || '').trim());
    return info && typeof info.error === 'string' ? info : null;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Публичное API (диспетчер Rust ↔ JS fallback)
// ---------------------------------------------------------------------------

/** Экранирует невалидные `\d`, `\w`, Windows-пути и прочие LLM-артефакты внутри JSON-строк. */
export function repairBadEscapes(s) {
  const str = String(s);
  const r = runRust('repairBadEscapes', { s: str });
  if (r !== null) return r;
  return repairBadEscapesJs(str);
}

export function extractJsonObject(text, { onRepaired } = {}) {
  const str = String(text);
  const r = runRust('extractJsonObject', { text: str });
  if (r !== null) {
    if (r.repaired != null) onRepaired?.(r.repaired);
    return r.value;
  }
  return extractJsonObjectJs(str, { onRepaired });
}

// ---------------------------------------------------------------------------
// Чистый JS fallback — эталонная копия исходной реализации (lib/json.mjs до Фазы 1).
// Используется только когда Rust-бинарь отсутствует; при битом бинаре НЕ подменяет его.
// ---------------------------------------------------------------------------

const VALID_JSON_ESC = new Set(['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u']);

function repairBadEscapesJs(s) {
  try {
    JSON.parse(s);
    return s;
  } catch {
    /* needs repair */
  }
  let out = '';
  let inStr = false;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (!inStr) {
      out += c;
      if (c === '"') inStr = true;
      continue;
    }
    if (c === '"') {
      inStr = false;
      out += c;
      continue;
    }
    if (c === '\\') {
      const next = s[i + 1];
      if (!next) {
        out += '\\\\';
        continue;
      }
      if (next === 'u') {
        const hexPart = s.slice(i + 2, i + 6);
        const hex = (hexPart.match(/^[0-9a-fA-F]*/)?.[0]) || '';
        if (hex.length === 4) {
          out += s.slice(i, i + 6);
        } else if (hex.length > 0) {
          out += '\\\\u' + hex;
        } else {
          out += '\\\\u';
          i += 1;
          continue;
        }
        i += 1 + hex.length;
        continue;
      }
      if (VALID_JSON_ESC.has(next)) {
        out += c + next;
        i++;
        continue;
      }
      out += '\\\\' + next;
      i++;
      continue;
    }
    out += c;
  }
  if (inStr) out += '"';
  return out;
}

function tryParseJson(s, { onRepaired } = {}) {
  try {
    return JSON.parse(s);
  } catch (e) {
    if (!/Bad escaped|Unexpected (?:token|end)|JSON/i.test(e.message || '')) throw e;
    const fixed = repairBadEscapesJs(s);
    try {
      const obj = JSON.parse(fixed);
      onRepaired?.('escapes');
      return obj;
    } catch {
      throw Object.assign(e, { badEscape: true });
    }
  }
}

function extractJsonObjectJs(text, { onRepaired } = {}) {
  let s = String(text).trim();
  const fence = s.match(/```(?:json)?\s*([\s\S]*?)```/i);
  if (fence) s = fence[1].trim();
  const start = s.indexOf('{');
  if (start < 0) throw new Error('в ответе модели нет JSON');
  const stack = [];
  let inStr = false, esc = false;
  for (let i = start; i < s.length; i++) {
    const c = s[i];
    if (inStr) { if (esc) esc = false; else if (c === '\\') esc = true; else if (c === '"') inStr = false; continue; }
    if (c === '"') inStr = true;
    else if (c === '{' || c === '[') stack.push(c);
    else if (c === '}' || c === ']') {
      stack.pop();
      if (stack.length === 0) {
        try {
          return tryParseJson(s.slice(start, i + 1), { onRepaired });
        } catch (e) {
          if (e.badEscape) {
            throw Object.assign(new Error(e.message || 'невалидные escape в JSON ответа модели'), {
              badEscape: true,
              truncated: false,
            });
          }
          throw e;
        }
      }
    }
  }
  const needsStructuralRepair = inStr || stack.length > 0;
  let repaired = s.slice(start);
  if (inStr) repaired += '"';
  repaired = repaired.replace(/,\s*$/, '');
  while (stack.length) repaired += stack.pop() === '{' ? '}' : ']';
  repaired = repaired.replace(/,(\s*[}\]])/g, '$1');
  try {
    let escapeFixed = false;
    const obj = tryParseJson(repaired, {
      onRepaired: (kind) => { if (kind === 'escapes') escapeFixed = true; },
    });
    onRepaired?.(escapeFixed ? 'escapes' : 'truncated');
    return obj;
  } catch (e) {
    if (e.badEscape) {
      throw Object.assign(new Error('не удалось исправить escape в оборванном JSON'), {
        badEscape: true,
        truncated: needsStructuralRepair,
      });
    }
    throw Object.assign(new Error('не удалось сбалансировать JSON из ответа модели'), {
      truncated: needsStructuralRepair,
    });
  }
}
