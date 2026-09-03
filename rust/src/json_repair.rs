//! Порт lib/json.mjs: толерантное извлечение/ремонт JSON из вывода LLM
//! (`extractJsonObject` + `repairBadEscapes`). Зона максимального риска расхождения
//! с JS — паритет проверен parity-прогоном на всех входах JS-тестов + генеративных
//! обрезках (см. rust/parity/json-repair.parity.mjs).
//!
//! Паритет с V8/Node (и намеренные отклонения, зафиксированные в parity-прогоне):
//! - итерация по ASCII-структуре ({, }, [, ], ", \\) — побайтовый скан безопасен:
//!   ASCII-байт не встречается внутри мультибайтовых UTF-8 последовательностей;
//! - JS `String#trim` / `\s` — Unicode WhiteSpace ∪ LineTerminator ∪ U+FEFF,
//!   но БЕЗ U+0085 (NEL), который Rust `char::is_whitespace` считал бы пробелом
//!   (см. `is_js_ws`);
//! - числа: arbitrary_precision — сырые лексемы переживают round-trip через stdout
//!   и повторный JSON.parse на стороне JS (1e999 → Infinity как у V8);
//! - суррогаты: V8 принимает непарные `\uD800`/`\uDC00` как lone surrogates,
//!   serde_json падает → перед парсом непарные суррогатные эскейпы заменяются на
//!   U+FFFD (успех/ошибка совпадают, отличается сам code point — String в Rust
//!   не может хранить lone surrogate);
//! - вложенность: у V8 лимита нет (проверено до 100_000), serde_json без лимита
//!   падает со stack overflow на ~20_000 → итеративный depth-guard MAX_NESTING_DEPTH
//!   чистой ошибкой (отклонение только для глубины > guard);
//! - тексты сообщений о внутренних ошибках парсера (V8: "Unexpected token ...")
//!   не воспроизводятся — используются фиксированные русские тексты из JS-веток;
//!   флаги `badEscape`/`truncated` и success/failure — в паритете.

use std::fmt;

use serde::Deserialize;
use serde_json::Value;

/// Ошибка extractJsonObject — аналог JS-объекта Error с флагами badEscape/truncated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonRepairError {
    pub message: String,
    pub bad_escape: bool,
    pub truncated: bool,
}

impl fmt::Display for JsonRepairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JsonRepairError {}

impl JsonRepairError {
    fn new(message: impl Into<String>, bad_escape: bool, truncated: bool) -> Self {
        JsonRepairError { message: message.into(), bad_escape, truncated }
    }
}

/// Вид ремонта — аналог аргумента onRepaired('escapes' | 'truncated') в JS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairedKind {
    Escapes,
    Truncated,
}

impl RepairedKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RepairedKind::Escapes => "escapes",
            RepairedKind::Truncated => "truncated",
        }
    }
}

/// Результат extractJsonObject: распарсенное значение + вид ремонта (None — чистый parse).
#[derive(Debug, Clone)]
pub struct ExtractOutcome {
    pub value: Value,
    pub repaired: Option<RepairedKind>,
}

// ---------------------------------------------------------------------------
// JS-семантика whitespace (String#trim и \s в regex)
// ---------------------------------------------------------------------------

/// JS `\s` = [\t\n\v\f\r \u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000\ufeff].
/// Отличие от Rust `char::is_whitespace`: U+0085 (NEL) в JS НЕ whitespace.
fn is_js_ws(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{0B}'
            | '\u{0C}'
            | '\r'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// JS `String#trim`: срезает whitespace с обеих сторон.
fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_ws)
}

fn skip_js_ws(s: &str, mut i: usize) -> usize {
    while i < s.len() {
        let c = s[i..].chars().next().expect("i на границе символа");
        if !is_js_ws(c) {
            break;
        }
        i += c.len_utf8();
    }
    i
}

// ---------------------------------------------------------------------------
// repairBadEscapes
// ---------------------------------------------------------------------------

const VALID_JSON_ESC: [char; 9] = ['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u'];

/// Экранирует невалидные `\d`, `\w`, Windows-пути и прочие LLM-артефакты внутри JSON-строк.
/// Порт repairBadEscapes из lib/json.mjs (посимвольно, включая ветки `\u` с частичным hex).
pub fn repair_bad_escapes(s: &str) -> String {
    if parse_json(s).is_ok() {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut in_str = false;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if !in_str {
            out.push(c);
            if c == '"' {
                in_str = true;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = false;
            out.push(c);
            i += 1;
            continue;
        }
        if c != '\\' {
            out.push(c);
            i += 1;
            continue;
        }
        // c == '\\'
        let next = chars.get(i + 1).copied();
        match next {
            None => {
                // висячий бэкслеш в конце → экранируем
                out.push_str("\\\\");
                i += 1;
            }
            Some('u') => {
                // берём hex-префикс длиной до 4 (как JS /^[0-9a-fA-F]*/ на окне [i+2, i+6))
                let mut hex_len = 0usize;
                while hex_len < 4 {
                    match chars.get(i + 2 + hex_len) {
                        Some(h) if h.is_ascii_hexdigit() => hex_len += 1,
                        _ => break,
                    }
                }
                if hex_len == 4 {
                    out.push('\\');
                    out.push('u');
                    for h in &chars[i + 2..i + 6] {
                        out.push(*h);
                    }
                } else if hex_len > 0 {
                    // частичный hex: \\u + hex → литеральный текст `\uXX`
                    out.push_str("\\\\u");
                    for h in &chars[i + 2..i + 2 + hex_len] {
                        out.push(*h);
                    }
                } else {
                    // `\u` без hex: \\u, а саму 'u' пропускаем (i += 1 в JS + шаг цикла)
                    out.push_str("\\\\u");
                    i += 2; // '\' и 'u'
                    continue;
                }
                i += 2 + hex_len; // '\' 'u' + hex
            }
            Some(n) if VALID_JSON_ESC.contains(&n) => {
                out.push('\\');
                out.push(n);
                i += 2;
            }
            Some(n) => {
                out.push_str("\\\\");
                out.push(n);
                i += 2;
            }
        }
    }
    if in_str {
        out.push('"');
    }
    out
}

// ---------------------------------------------------------------------------
// Парсинг с паритетом V8
// ---------------------------------------------------------------------------

/// Итеративный depth-guard: serde_json с disable_recursion_limit падает со stack
/// overflow примерно на ~20_000 уровней; guard даёт чистую ошибку раньше.
/// V8 лимита не имеет — отклонение только для глубины > MAX_NESTING_DEPTH.
const MAX_NESTING_DEPTH: usize = 4096;

/// Максимальная глубина вложенности {/[ вне строк; считает итеративно (без стека).
fn max_nesting_depth(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut max_depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for &b in bytes {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max_depth
}

/// Заменяет непарные суррогатные `\uXXXX`-эскейпы на U+FFFD.
///
/// V8 принимает их как lone surrogates и кладёт code unit D800/DC00 в строку;
/// serde_json падает с "lone leading surrogate". String в Rust lone surrogate
/// хранить не может, поэтому → U+FFFD: успех/ошибка парсинга совпадают с V8,
/// отличается сам code point (только для патологических `\uD800`-эскейпов).
fn sanitize_unpaired_surrogates(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'u') {
            if let Some((hi, len)) = read_hex_escape(bytes, i) {
                if (0xD800..=0xDBFF).contains(&hi) {
                    // high surrogate: парой является только непосредственно следующий \uDC00-\uDFFF
                    let is_pair = match bytes.get(i + len) {
                        Some(b'\\') if bytes.get(i + len + 1) == Some(&b'u') => read_hex_escape(
                            bytes,
                            i + len,
                        )
                        .map_or(false, |(lo, _)| (0xDC00..=0xDFFF).contains(&lo)),
                        _ => false,
                    };
                    if is_pair {
                        out.push_str(&s[i..i + len + 6]);
                        i += len + 6;
                        continue;
                    }
                    out.push('\u{FFFD}');
                    i += len;
                    continue;
                }
                if (0xDC00..=0xDFFF).contains(&hi) {
                    out.push('\u{FFFD}');
                    i += len;
                    continue;
                }
                out.push_str(&s[i..i + len]);
                i += len;
                continue;
            }
        }
        // обычный символ (UTF-8 границы сохраняются: ASCII-байт никогда не часть
        // мультибайтового символа, а `\` + не-u копируется посимвольно ниже)
        let c = s[i..].chars().next().expect("i на границе символа");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Читает эскейп `\uXXXX` начиная с позиции i (bytes[i] == b'\\'); возвращает
/// (значение, длина эскейпа в байтах) при ровно 4 hex-цифрах.
fn read_hex_escape(bytes: &[u8], i: usize) -> Option<(u32, usize)> {
    if bytes.get(i) != Some(&b'\\') || bytes.get(i + 1) != Some(&b'u') {
        return None;
    }
    let hex = bytes.get(i + 2..i + 6)?;
    if hex.len() < 4 || !hex.iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
    Some((v, 6))
}

/// Парсинг JSON с паритетом V8: arbitrary_precision (1e999 → сырой литерал →
/// Infinity при повторном JSON.parse на стороне JS), unbounded_depth + guard,
/// санитизация непарных суррогатов.
fn parse_json(s: &str) -> Result<Value, serde_json::Error> {
    if max_nesting_depth(s) > MAX_NESTING_DEPTH {
        // Чистая ошибка вместо stack overflow serde_json; V8 здесь бы спарсил —
        // задокументированное отклонение для глубины > MAX_NESTING_DEPTH.
        return Err(serde_json::from_str::<Value>("!").unwrap_err());
    }
    let sanitized = sanitize_unpaired_surrogates(s);
    let mut de = serde_json::Deserializer::from_str(&sanitized);
    de.disable_recursion_limit();
    Value::deserialize(&mut de)
}

/// tryParseJson из JS: parse → при ошибке repairBadEscapes → parse снова.
/// Возвращает (value, escape_repaired). Постоянный ремонт без эффекта = badEscape.
fn try_parse(s: &str) -> Result<(Value, bool), JsonRepairError> {
    match parse_json(s) {
        Ok(v) => Ok((v, false)),
        Err(_) => {
            let fixed = repair_bad_escapes(s);
            match parse_json(&fixed) {
                Ok(v) => Ok((v, true)),
                Err(_) => Err(JsonRepairError::new(
                    "невалидные escape в JSON ответа модели",
                    true,
                    false,
                )),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// extractJsonObject
// ---------------------------------------------------------------------------

/// Извлечение fence ```(?:json)?\s*([\s\S]*?)``` (регистронезависимо) с бэктрекингом
/// опционального `json`: первый opening-```, у которого есть closing-```.
fn extract_fence(s: &str) -> Option<&str> {
    let mut from = 0usize;
    while let Some(rel) = s[from..].find("```") {
        let p = from + rel;
        let after = p + 3;
        // JS-regex бэктрекинг: сначала (?:json)? жадно съедает "json", при неудаче
        // всего матча — пробуем без него, затем следующий opening-```.
        let mut starts: Vec<usize> = Vec::with_capacity(2);
        if s[after..].len() >= 4 && s[after..after + 4].eq_ignore_ascii_case("json") {
            starts.push(skip_js_ws(s, after + 4));
        }
        starts.push(skip_js_ws(s, after));
        for cs in starts {
            if let Some(rel2) = s[cs..].find("```") {
                return Some(&s[cs..cs + rel2]);
            }
        }
        from = after;
    }
    None
}

/// Толерантное извлечение JSON-объекта из вывода LLM. Порт extractJsonObject:
/// fence → первый '{' → балансировка скобок с учётом строк → ремонт обрыва/эскейпов.
pub fn extract_json_object(text: &str) -> Result<ExtractOutcome, JsonRepairError> {
    let mut s = js_trim(text);
    if let Some(fence) = extract_fence(s) {
        s = js_trim(fence);
    }
    let start = match s.find('{') {
        Some(p) => p,
        None => return Err(JsonRepairError::new("в ответе модели нет JSON", false, false)),
    };

    // Балансировка. Структурные символы ASCII → побайтовый скан безопасен.
    let bytes = s.as_bytes();
    let mut stack: Vec<u8> = Vec::new();
    let mut in_str = false;
    let mut esc = false;
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' | b'[' => stack.push(c),
            b'}' | b']' => {
                stack.pop(); // как JS: pop без проверки соответствия скобки
                if stack.is_empty() {
                    return match try_parse(&s[start..=i]) {
                        Ok((v, escape_repaired)) => Ok(ExtractOutcome {
                            value: v,
                            repaired: if escape_repaired { Some(RepairedKind::Escapes) } else { None },
                        }),
                        Err(_) => Err(JsonRepairError::new(
                            "невалидные escape в JSON ответа модели",
                            true,
                            false,
                        )),
                    };
                }
            }
            _ => {}
        }
        i += 1;
    }

    // Структурный ремонт оборванного хвоста (порядок операций — как в JS).
    let needs_structural_repair = in_str || !stack.is_empty();
    let mut repaired = s[start..].to_string();
    if in_str {
        repaired.push('"');
    }
    // replace(/,\s*$/, ''): последняя не-whitespace запятая у конца строки срезает
    // и запятую, и хвостовой whitespace.
    {
        let t = repaired.trim_end_matches(is_js_ws);
        if t.ends_with(',') {
            repaired.truncate(t.len() - 1);
        }
    }
    while let Some(c) = stack.pop() {
        repaired.push(if c == b'{' { '}' } else { ']' });
    }
    // replace(/,(\s*[}\]])/g, '$1') — глобально по chars (JS \s — Unicode).
    let repaired = remove_commas_before_closers(&repaired);

    match try_parse(&repaired) {
        Ok((v, escape_repaired)) => Ok(ExtractOutcome {
            value: v,
            repaired: Some(if escape_repaired { RepairedKind::Escapes } else { RepairedKind::Truncated }),
        }),
        Err(e) if e.bad_escape => Err(JsonRepairError::new(
            "не удалось исправить escape в оборванном JSON",
            true,
            needs_structural_repair,
        )),
        Err(_) => Err(JsonRepairError::new(
            "не удалось сбалансировать JSON из ответа модели",
            false,
            needs_structural_repair,
        )),
    }
}

/// Порт JS `repaired.replace(/,(\s*[}\]])/g, '$1')`.
fn remove_commas_before_closers(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == ',' {
            let mut j = i + 1;
            while j < chars.len() && is_js_ws(chars[j]) {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1; // запятую пропускаем, whitespace и скобку выведем ниже
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Тесты — edge-кейсы из test/pr-review.test.mjs и test/github.test.mjs
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(text: &str) -> Value {
        extract_json_object(text).expect("должно распарситься").value
    }

    #[test]
    fn clean_and_fenced() {
        assert_eq!(extract(r#"{"findings":[]}"#), serde_json::json!({ "findings": [] }));
        assert_eq!(extract("```json\n{\"a\":1}\n```"), serde_json::json!({ "a": 1 }));
    }

    #[test]
    fn truncated_tail_variants() {
        // оборванная строка + незакрытые скобки
        let o = extract(r#"{"findings":[{"file":"a.ts","line":10,"body":"partial no close"#);
        assert_eq!(o["findings"][0]["file"], "a.ts");
        // висячая запятая перед обрывом
        let o2 = extract(r#"{"findings":[{"file":"a.ts","line":1},"#);
        assert_eq!(o2["findings"].as_array().unwrap().len(), 1);
        // фигурная скобка ВНУТРИ строки не ломает баланс
        let o3 = extract(r#"{"findings":[{"body":"use rm -rf { danger""#);
        assert!(o3["findings"][0]["body"].as_str().unwrap().contains("danger"));
    }

    #[test]
    fn bad_escapes_regex_and_windows_paths() {
        let o = extract(r#"{"findings":[{"body":"match \\d+ in path C:\\Users\\foo"}]}"#);
        assert!(o["findings"][0]["body"].as_str().unwrap().contains("\\d"));
        let o2 = extract(r#"{"summary":"bad \q escape"}"#);
        assert_eq!(o2["summary"], "bad \\q escape");
    }

    #[test]
    fn repair_bad_escapes_idempotent_and_partial_u() {
        let raw = r#"{"title":"\d \q"}"#;
        let once = repair_bad_escapes(raw);
        let twice = repair_bad_escapes(&once);
        assert_eq!(once, twice);
        assert_eq!(serde_json::from_str::<Value>(&twice).unwrap()["title"], "\\d \\q");
        let partial = repair_bad_escapes(r#"{"c":"\u00"}"#);
        assert_eq!(serde_json::from_str::<Value>(&partial).unwrap()["c"], "\\u00");
        let no_hex = repair_bad_escapes(r#"{"c":"\uXYZ"}"#);
        assert_eq!(serde_json::from_str::<Value>(&no_hex).unwrap()["c"], "\\uXYZ");
    }

    #[test]
    fn hopeless_throws_with_flags() {
        let e = extract_json_object(r#"{"findings":[{"line":"#).unwrap_err();
        assert!(e.truncated);
        let e2 = extract_json_object("нет json тут").unwrap_err();
        assert_eq!(e2.message, "в ответе модели нет JSON");
        assert!(!e2.truncated && !e2.bad_escape);
    }

    #[test]
    fn repaired_kind_is_reported() {
        let o = extract_json_object(r#"{"summary":"bad \q escape"}"#).unwrap();
        assert_eq!(o.repaired, Some(RepairedKind::Escapes));
        let o2 = extract_json_object(r#"{"findings":[{"file":"a.ts","line":1},"#).unwrap();
        assert_eq!(o2.repaired, Some(RepairedKind::Truncated));
        let o3 = extract_json_object(r#"{"a":1}"#).unwrap();
        assert_eq!(o3.repaired, None);
    }

    #[test]
    fn truncated_sticky_footer_from_github_test() {
        // test/github.test.mjs: обрезанный футер чинится через extractJsonObject
        let raw = r#"{"findings":[{"file":"b.ts","line":2,"title":"t"}],"runs":[],"sha":"dead"#;
        let o = extract_json_object(raw).unwrap();
        assert_eq!(o.value["findings"][0]["file"], "b.ts");
        assert_eq!(o.value["sha"], "dead");
    }

    #[test]
    fn fence_without_json_tag_and_backtracking() {
        // ```js-фенс: (?:json)? не матчит, контент берётся от 'j'
        let o = extract("```js\n{\"a\":1}\n```");
        assert_eq!(o["a"], 1);
        // нет closing-фенса → фенс игнорируется, ищем '{' в целом тексте
        let o2 = extract("вокруг ```json {\"a\":2} без закрытия");
        assert_eq!(o2["a"], 2);
    }

    #[test]
    fn unpaired_surrogates_are_sanitized_to_replacement_char() {
        // V8 парсит успешно (lone surrogate), serde_json падает → U+FFFD;
        // важно: успех/ошибка совпадают с V8.
        let o = extract(r#"{"s":"\uD800"}"#);
        assert_eq!(o["s"], "\u{FFFD}");
        let o2 = extract(r#"{"s":"\uD83D\uDE00"}"#);
        assert_eq!(o2["s"], "😀");
    }

    #[test]
    fn depth_guard_avoids_stack_overflow() {
        let deep = format!("{}1{}", r#"{"a":"#.repeat(5000), "}".repeat(5000));
        // > MAX_NESTING_DEPTH: чистая ошибка, не stack overflow
        let e = extract_json_object(&deep).unwrap_err();
        assert!(e.bad_escape); // парсинг не удался → путь ремонта
    }

    #[test]
    fn number_parity_through_js_reparse() {
        // 1e999: serde нормализует литерал в "1e+999", но JSON.parse("1e+999") на
        // стороне JS даёт Infinity — как прямой V8 JSON.parse("1e999").
        let v = parse_json("1e999").unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), "1e+999");
        let o = extract(r#"{"n":12345678901234567890123}"#);
        assert_eq!(serde_json::to_string(&o["n"]).unwrap(), "12345678901234567890123");
    }

    #[test]
    fn commas_before_closers_removed_globally() {
        let o = extract(r#"{"a":[1,2,],"b":{"c":1,},"#);
        assert_eq!(o["a"], serde_json::json!([1, 2]));
        assert_eq!(o["b"]["c"], 1);
    }

    #[test]
    fn js_trim_excludes_nel_but_includes_nbsp_and_bom() {
        assert_eq!(js_trim("\u{0085}x\u{0085}"), "\u{0085}x\u{0085}");
        assert_eq!(js_trim("\u{00A0}x\u{00A0}"), "x");
        assert_eq!(js_trim("\u{FEFF}x\u{FEFF}"), "x");
    }
}
