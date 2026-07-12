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
    else if (c === '}' || c === ']') { stack.pop(); if (stack.length === 0) return JSON.parse(s.slice(start, i + 1)); }
  }
  let repaired = s.slice(start);
  if (inStr) repaired += '"';
  repaired = repaired.replace(/,\s*$/, '');
  while (stack.length) repaired += stack.pop() === '{' ? '}' : ']';
  repaired = repaired.replace(/,(\s*[}\]])/g, '$1');
  try {
    const obj = JSON.parse(repaired);
    onRepaired?.();
    return obj;
  } catch {
    throw Object.assign(new Error('не удалось сбалансировать JSON из ответа модели'), { truncated: true });
  }
}
