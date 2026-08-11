const VALID_JSON_ESC = new Set(['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u']);

/** Экранирует невалидные `\d`, `\w`, Windows-пути и прочие LLM-артефакты внутри JSON-строк. */
export function repairBadEscapes(s) {
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
    const fixed = repairBadEscapes(s);
    try {
      const obj = JSON.parse(fixed);
      onRepaired?.('escapes');
      return obj;
    } catch {
      throw Object.assign(e, { badEscape: true });
    }
  }
}

export function extractJsonObject(text, { onRepaired } = {}) {
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
