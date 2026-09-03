//! Порт lib/structure-tests.mjs: экстрактор тест-кейсов из исходников, слияние
//! кейсов из разных источников, драфты property-тестов и парсер property-отчётов.
//!
//! Паритет с JS сохранён, включая:
//! - ручной сканер TEST_DECLARATION_RE /\b(?:it|test|specify)\s*\(\s*(['"`])(.+?)\1/g:
//!   regex-крейт не поддерживает backreference, поэтому левосторонний перебор
//!   стартовых позиций и поиск первого того же кавычки-символа до LineTerminator
//!   (\n \r \u{2028} \u{2029} — `.` без s-флага их не пересекает) повторяют семантику
//!   exec() в цикле: lastIndex после матча, при неудаче — перебор со start+1;
//!   \b — ASCII word boundary ([A-Za-z0-9_]), как в JS без unicode-флага;
//!   \s — JS-класс из findings::js_is_whitespace (включает U+FEFF, не включает U+0085);
//! - body теста — до начала следующей декларации либо min(len, start+6000);
//! - framework: \bvitest\b / \bjest\b / \bnode:test\b по исходнику, затем
//!   /\.spec\./ по пути → unknown-spec/unknown-test;
//! - assertions: пять regex-паттернов с ASCII \b (набор JS \w), порядок массива —
//!   порядок ASSERTION_PATTERNS;
//! - контрактные матчеры: label из contract.label || entityLabel || entity.label
//!   (по entityRef), normalizedLabel (хвостовые "()", leading '.', lowercase),
//!   экранирование regex::escape ≡ JS-escape, \b…\b с (?i);
//! - mergeTestCases: {...previous, ...testCase} + объединение targetContractRefs
//!   (Set-union, побайтовая сортировка), assertions (дедуп по kind, первый выигрывает),
//!   evidence (Set-union, побайтовая сортировка); итог — path/line/name localeCompare;
//! - parsePropertyReport: stale по semanticHash, seed/shrinkPath → String, numRuns/
//!   discardCount Number(x || 0) (NaN → null при сериализации).

use std::cmp::Ordering;
use std::collections::HashMap;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::findings::{js_is_whitespace, js_number_or_zero, js_to_number, js_to_string, js_truthy};
use crate::structure_scip::{locale_compare, stable_id};

const KEYWORDS: &[&str] = &["it", "test", "specify"];

fn js_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// JS LineTerminator: `.` без s-флага их не пересекает.
fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

struct Declaration {
    start: usize,
    end: usize,
    name: String,
}

/// Поиск декларации начиная с `from`: возвращает самый левый матч
/// /\b(?:it|test|specify)\s*\(\s*(['"`])(.+?)\1/ ≥ from.
fn find_declaration(source: &str, from: usize) -> Option<Declaration> {
    let bytes = source.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        // \b перед keyword: предыдущий символ не word-char (или start)
        let boundary = i == 0 || !js_word_char(bytes[i - 1] as char);
        if boundary {
            for keyword in KEYWORDS {
                if source[i..].starts_with(keyword) {
                    if let Some(decl) = try_declaration(source, i, keyword) {
                        return Some(decl);
                    }
                    break;
                }
            }
        }
        i += 1;
    }
    None
}

/// Попытка матча с фиксированной стартовой позицией (keyword уже совпал с \b).
fn try_declaration(source: &str, start: usize, keyword: &str) -> Option<Declaration> {
    let mut pos = start + keyword.len();
    // \s* — JS-класс
    pos = skip_js_space(source, pos);
    if source[pos..].chars().next() != Some('(') {
        return None;
    }
    pos += 1;
    pos = skip_js_space(source, pos);
    let quote = source[pos..].chars().next()?;
    if !matches!(quote, '\'' | '"' | '`') {
        return None;
    }
    // (.+?)\1: ≥1 символ до первого того же кавычки; LineTerminator обрывает матч.
    // Кавычка на offset 0 — НЕ закрывающая: `.+?` может съесть её как обычный
    // символ имени (`it('')` матчится с именем "'" — как в JS-регекспе).
    let mut iter = source[pos + quote.len_utf8()..].char_indices();
    let name_start = pos + quote.len_utf8();
    while let Some((offset, c)) = iter.next() {
        if c == quote {
            if offset == 0 {
                continue;
            }
            let name_end = name_start + offset;
            return Some(Declaration {
                start,
                end: name_end + quote.len_utf8(),
                name: source[name_start..name_end].to_string(),
            });
        }
        if is_line_terminator(c) {
            return None;
        }
    }
    None
}

fn skip_js_space(source: &str, mut pos: usize) -> usize {
    while let Some(c) = source[pos..].chars().next() {
        if js_is_whitespace(c) {
            pos += c.len_utf8();
        } else {
            break;
        }
    }
    pos
}

/// lineAt: source.slice(0, offset).split('\n').length.
fn line_at(source: &str, offset: usize) -> usize {
    source[..offset].bytes().filter(|b| *b == b'\n').count() + 1
}

/// Поиск подстроки с ASCII-\b по обеим сторонам (/\bneedle\b/).
fn contains_word(source: &str, needle: &str) -> bool {
    let bytes = source.as_bytes();
    let mut i = 0;
    while let Some(found) = source[i..].find(needle) {
        let at = i + found;
        let before_ok = at == 0 || !js_word_char(bytes[at - 1] as char);
        let after = at + needle.len();
        let after_ok = after >= bytes.len() || !js_word_char(bytes[after] as char);
        if before_ok && after_ok {
            return true;
        }
        i = at + 1;
    }
    false
}

/// frameworkForPath: vitest → jest → node:test по исходнику, .spec. по пути.
fn framework_for_path(path: &str, source: &str) -> &'static str {
    if contains_word(source, "vitest") {
        return "vitest";
    }
    if contains_word(source, "jest") {
        return "jest";
    }
    if contains_word(source, "node:test") {
        return "node:test";
    }
    if path.contains(".spec.") {
        return "unknown-spec";
    }
    "unknown-test"
}

fn assertion_regexes() -> Vec<(&'static str, Regex)> {
    let word = |pattern: &str| format!("(?-u:\\b)(?:{pattern})(?-u:\\b)");
    // \s в JS-паттерне http_status — JS-класс
    let js_space = r"[\t\n\v\f\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";
    vec![
        ("throws", Regex::new(&word(r"toThrow|throws|assert\.throws")).unwrap()),
        ("rejects", Regex::new(&word(r"rejects|assert\.rejects")).unwrap()),
        (
            "http_status",
            Regex::new(&format!(
                "{}(?:status|statusCode|toHaveStatus){}*(?:\\(|[=:]){}*(?:4[0-9][0-9]|5[0-9][0-9])",
                "(?-u:\\b)", js_space, js_space
            ))
            .unwrap(),
        ),
        ("equality", Regex::new(&word(r"toEqual|toBe|deepEqual|strictEqual|assert\.equal")).unwrap()),
        ("property", Regex::new(&word(r"fc\.assert|property|forall|forAll")).unwrap()),
    ]
}

fn assertions_in(body: &str) -> Vec<Value> {
    assertion_regexes()
        .iter()
        .filter(|(_, re)| re.is_match(body))
        .map(|(kind, _)| json!({ "kind": kind, "confidence": "heuristic" }))
        .collect()
}

/// normalizedLabel: хвостовые "()", leading '.', lowercase.
fn normalized_label(value: &str) -> String {
    let mut s = value;
    while let Some(rest) = s.strip_suffix("()") {
        s = rest;
    }
    let s = s.strip_prefix('.').unwrap_or(s);
    s.to_lowercase()
}

fn regex_escape_js(s: &str) -> String {
    // JS-экранирует [.*+?^${}()|[\]\\] — regex::escape покрывает те же метасимволы
    regex::escape(s)
}

/// extractTestCasesFromSource: сканирует исходник на it/test/specify-декларации,
/// привязывает контракты по имени и собирает assertion-паттерны тела.
pub fn extract_test_cases_from_source(
    path: &str,
    source: &str,
    contracts: &[Value],
    entities: &[Value],
) -> Vec<Value> {
    // label контракта может отсутствовать (SCIP-контракты: entityRef/kind/signature) —
    // выводим из сущности по entityRef
    let mut label_by_entity_ref: HashMap<String, Value> = HashMap::new();
    for entity in entities {
        if let Some(id) = entity.get("observedId") {
            label_by_entity_ref.insert(json_key(id), entity.get("label").cloned().unwrap_or(Value::Null));
        }
    }
    let mut contract_matchers: Vec<(String, Regex)> = Vec::new();
    for contract in contracts {
        let label_value = match contract.get("label") {
            Some(x) if js_truthy(x) => Some(x.clone()),
            _ => match contract.get("entityLabel") {
                Some(x) if js_truthy(x) => Some(x.clone()),
                _ => label_by_entity_ref
                    .get(&json_key_opt(contract.get("entityRef")))
                    .cloned(),
            },
        };
        let label = normalized_label(&match label_value {
            Some(x) if js_truthy(&x) => js_to_string(&x),
            _ => String::new(),
        });
        if label.is_empty() {
            continue;
        }
        let Ok(pattern) = Regex::new(&format!(
            "(?i)(?-u:\\b){}(?-u:\\b)",
            regex_escape_js(&label)
        )) else {
            continue;
        };
        let id = contract
            .get("id")
            .map(js_to_string)
            .unwrap_or_default();
        contract_matchers.push((id, pattern));
    }

    let mut out = Vec::new();
    let mut search_from = 0usize;
    while let Some(decl) = find_declaration(source, search_from) {
        // тело: до следующей декларации либо min(len, start+6000)
        let next = find_declaration(source, decl.end);
        let end = match next {
            Some(next_decl) => next_decl.start,
            None => (decl.start + 6000).min(source.len()),
        };
        let body = &source[decl.start..end];
        let mut target_contract_refs: Vec<String> = Vec::new();
        for (id, pattern) in &contract_matchers {
            if pattern.is_match(body) && !target_contract_refs.contains(id) {
                target_contract_refs.push(id.clone());
            }
        }
        // [...new Set(...)].sort() — побайтовая сортировка
        target_contract_refs.sort();
        let name = decl.name.trim_matches(js_is_whitespace).to_string();
        let line = line_at(source, decl.start);
        let framework = framework_for_path(path, source);
        out.push(json!({
            "id": stable_id("test", &[path.into(), line.to_string(), name.clone()]),
            "name": name,
            "path": path,
            "line": line,
            "framework": framework,
            "targetContractRefs": target_contract_refs,
            "assertions": assertions_in(body),
            "confidence": "heuristic",
            "evidence": [format!("source:test-declaration:{path}:{line}")],
        }));
        search_from = decl.end;
    }
    out
}

/// Ключ Map по JSON-значению (JS Map использует SameValueZero; строки — основной кейс).
fn json_key(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

fn json_key_opt(v: Option<&Value>) -> String {
    match v {
        Some(v) => json_key(v),
        None => "undefined".to_string(),
    }
}

/// mergeTestCases: первый кейс по id основа, последующие перекрывают поля и
/// объединяют targetContractRefs/assertions/evidence; итог — path/line/name.
pub fn merge_test_cases(groups: Vec<Vec<Value>>) -> Vec<Value> {
    let mut by_id: Vec<(String, Value)> = Vec::new();
    for test_case in groups.into_iter().flatten() {
        let id = test_case
            .get("id")
            .map(js_to_string)
            .unwrap_or_default();
        match by_id.iter_mut().find(|(k, _)| *k == id) {
            None => by_id.push((id, test_case)),
            Some((_, previous)) => {
                let merged = merge_case(previous, &test_case);
                *previous = merged;
            }
        }
    }
    let mut out: Vec<Value> = by_id.into_iter().map(|(_, v)| v).collect();
    out.sort_by(|a, b| {
        locale_compare(
            a.get("path").and_then(Value::as_str).unwrap_or(""),
            b.get("path").and_then(Value::as_str).unwrap_or(""),
        )
        .then(
            (js_number_or_zero(a.get("line")) - js_number_or_zero(b.get("line")))
                .partial_cmp(&0.0)
                .unwrap_or(Ordering::Equal),
        )
        .then(locale_compare(
            a.get("name").and_then(Value::as_str).unwrap_or(""),
            b.get("name").and_then(Value::as_str).unwrap_or(""),
        ))
    });
    out
}

/// {...previous, ...testCase} + явные объединения списков.
fn merge_case(previous: &Value, test_case: &Value) -> Value {
    let mut merged = previous.as_object().cloned().unwrap_or_default();
    for (k, v) in test_case.as_object().cloned().unwrap_or_default() {
        merged.insert(k, v);
    }
    let union_sorted = |a: Option<&Value>, b: Option<&Value>| -> Value {
        let mut out: Vec<String> = Vec::new();
        for item in a.iter().chain(b.iter()).flat_map(|arr| match arr {
            Value::Array(items) => items.iter().map(js_to_string).collect::<Vec<_>>(),
            _ => Vec::new(),
        }) {
            if !out.contains(&item) {
                out.push(item);
            }
        }
        out.sort();
        json!(out)
    };
    merged.insert(
        "targetContractRefs".into(),
        union_sorted(previous.get("targetContractRefs"), test_case.get("targetContractRefs")),
    );
    // assertions: конкатенация с дедупом по kind (первый выигрывает)
    let mut assertions: Vec<Value> = Vec::new();
    for arr in [previous.get("assertions"), test_case.get("assertions")] {
        if let Some(Value::Array(items)) = arr {
            for item in items {
                let kind = item.get("kind").map(js_to_string).unwrap_or_default();
                if !assertions
                    .iter()
                    .any(|existing| existing.get("kind").map(js_to_string).as_deref() == Some(kind.as_str()))
                {
                    assertions.push(item.clone());
                }
            }
        }
    }
    merged.insert("assertions".into(), Value::Array(assertions));
    merged.insert(
        "evidence".into(),
        union_sorted(previous.get("evidence"), test_case.get("evidence")),
    );
    Value::Object(merged)
}

/// generatePropertyDrafts: драфты только для confirmed boundary_value/invariant/
/// error_path обязательств.
pub fn generate_property_drafts(obligations: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = obligations
        .iter()
        .filter(|obligation| {
            obligation.get("status").and_then(Value::as_str) == Some("confirmed")
                && matches!(
                    obligation.get("kind").and_then(Value::as_str),
                    Some("boundary_value") | Some("invariant") | Some("error_path")
                )
        })
        .map(|obligation| {
            let kind = obligation.get("kind").and_then(Value::as_str).unwrap_or("");
            let generators = match kind {
                "boundary_value" => vec!["minimum", "maximum", "below_minimum", "above_maximum", "empty"],
                "error_path" => vec!["invalid", "empty", "malformed"],
                _ => vec!["valid_domain"],
            };
            let id_str = js_to_string(obligation.get("id").unwrap_or(&Value::Null));
            let hash = js_to_string(obligation.get("contentHash").unwrap_or(&Value::Null));
            let mut draft = Map::new();
            draft.insert(
                "id".into(),
                json!(stable_id("property", &[id_str.clone(), hash.clone()])),
            );
            draft.insert("obligationRef".into(), obligation.get("id").cloned().unwrap_or(Value::Null));
            draft.insert("status".into(), json!("draft"));
            draft.insert("kind".into(), json!(kind));
            if let Some(statement) = obligation.get("statement") {
                draft.insert("statement".into(), statement.clone());
                draft.insert("invariant".into(), statement.clone());
            }
            draft.insert("preconditions".into(), Value::Array(vec![]));
            draft.insert("generators".into(), json!(generators));
            draft.insert(
                "expectedFailures".into(),
                if kind == "error_path" {
                    json!(["invalid_input_rejected"])
                } else {
                    Value::Array(vec![])
                },
            );
            draft.insert("semanticHash".into(), obligation.get("contentHash").cloned().unwrap_or(Value::Null));
            draft.insert("requiresConfirmation".into(), Value::Bool(true));
            draft.insert("evidence".into(), json!([format!("obligation:{id_str}:{hash}")]));
            Value::Object(draft)
        })
        .collect();
    out.sort_by(|a, b| {
        locale_compare(
            a.get("id").and_then(Value::as_str).unwrap_or(""),
            b.get("id").and_then(Value::as_str).unwrap_or(""),
        )
    });
    out
}

fn num_or_zero_json(v: Option<&Value>) -> Value {
    let raw = match v {
        Some(x) if js_truthy(x) => x,
        _ => return json!(0),
    };
    match js_to_number(raw) {
        Some(n) if n.is_nan() || n.is_infinite() => Value::Null, // JSON.stringify(NaN) === 'null'
        Some(n) => num_json(n),
        None => Value::Null,
    }
}

fn num_json(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// JS String(x) для seed/shrinkPath: массивы — join(',') (null/undefined → '',
/// вложенные массивы рекурсивно), объекты — '[object Object]'.
fn js_string_full(v: &Value) -> String {
    match v {
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_string_full(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        other => js_to_string(other),
    }
}

/// parsePropertyReport: сверка результатов property-прогона с драфтами
/// (stale по semanticHash/отсутствию драфта).
pub fn parse_property_report(report: Option<&Value>, drafts: &[Value]) -> Vec<Value> {
    let mut by_id: Vec<(String, Value)> = Vec::new();
    for draft in drafts {
        if let Some(id) = draft.get("id") {
            by_id.push((json_key(id), draft.clone()));
        }
    }
    let properties = report
        .and_then(|r| r.get("properties"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<Value> = properties
        .iter()
        .map(|property| {
            let property_id = property.get("propertyId").cloned().unwrap_or(Value::Null);
            let draft = by_id
                .iter()
                .find(|(k, _)| *k == json_key(&property_id))
                .map(|(_, d)| d);
            let stale = match draft {
                None => true,
                Some(d) => d.get("semanticHash") != property.get("semanticHash"),
            };
            let obligation_ref = match draft {
                Some(d) => match d.get("obligationRef") {
                    Some(x) if js_truthy(x) => x.clone(),
                    _ => match property.get("obligationRef") {
                        Some(x) if js_truthy(x) => x.clone(),
                        _ => Value::Null,
                    },
                },
                None => match property.get("obligationRef") {
                    Some(x) if js_truthy(x) => x.clone(),
                    _ => Value::Null,
                },
            };
            let mut entry = Map::new();
            entry.insert("propertyId".into(), property_id.clone());
            entry.insert("obligationRef".into(), obligation_ref);
            entry.insert(
                "status".into(),
                json!(if stale {
                    "stale"
                } else if js_truthy(property.get("failed").unwrap_or(&Value::Null)) {
                    "failed"
                } else {
                    "passed"
                }),
            );
            entry.insert("numRuns".into(), num_or_zero_json(property.get("numRuns")));
            entry.insert("discardCount".into(), num_or_zero_json(property.get("discardCount")));
            entry.insert(
                "seed".into(),
                match property.get("seed") {
                    Some(Value::Null) | None => Value::Null,
                    Some(other) => json!(js_string_full(other)),
                },
            );
            entry.insert(
                "shrinkPath".into(),
                match property.get("shrinkPath") {
                    Some(Value::Null) | None => Value::Null,
                    Some(other) => json!(js_string_full(other)),
                },
            );
            entry.insert(
                "counterexample".into(),
                match property.get("counterexample") {
                    Some(Value::Null) | None => Value::Null,
                    Some(other) => other.clone(),
                },
            );
            entry.insert(
                "semanticHash".into(),
                match property.get("semanticHash") {
                    Some(Value::String(s)) => json!(s),
                    _ => json!(""),
                },
            );
            entry.insert(
                "confidence".into(),
                json!(if stale { "unresolved" } else { "exact" }),
            );
            entry.insert(
                "evidence".into(),
                json!([format!("property-report:{}", js_to_string(&property_id))]),
            );
            Value::Object(entry)
        })
        .collect();
    out.sort_by(|a, b| {
        locale_compare(
            a.get("propertyId").map(js_to_string).unwrap_or_default().as_str(),
            b.get("propertyId").map(js_to_string).unwrap_or_default().as_str(),
        )
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_declarations_with_quotes_lines_and_framework() {
        let source = "import { it } from 'vitest';\nit('does x', () => {\n  expect(fn).toThrow();\n});\ntest(\"other\", () => {});\ncontest('no', () => {});\n";
        let cases = extract_test_cases_from_source("a.test.ts", source, &[], &[]);
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0]["name"], json!("does x"));
        assert_eq!(cases[0]["line"], json!(2));
        assert_eq!(cases[0]["framework"], json!("vitest"));
        assert_eq!(cases[0]["assertions"], json!([{ "kind": "throws", "confidence": "heuristic" }]));
        assert_eq!(cases[1]["name"], json!("other"));
        // пустое имя it('') — JS-регексп не захватывает декларацию
        let empty = extract_test_cases_from_source("a.ts", "it('')", &[], &[]);
        assert!(empty.is_empty());
        // многострочное имя обрывается LineTerminator — декларации нет
        assert!(extract_test_cases_from_source("a.ts", "it('a\nb', () => {});", &[], &[]).is_empty());
    }

    #[test]
    fn binds_contracts_via_label_entity_label_and_entity_ref() {
        let contracts = vec![
            json!({ "id": "c1", "label": "AuthService.login()" }),
            json!({ "id": "c2", "kind": "signature", "entityRef": "scip:1" }),
        ];
        let entities = vec![json!({ "observedId": "scip:1", "label": "fromScip" })];
        let source = "it('one', () => { AuthService.login(); });\nit('two', () => { fromScip(); });";
        let cases = extract_test_cases_from_source("a.test.ts", source, &contracts, &entities);
        assert_eq!(cases[0]["targetContractRefs"], json!(["c1"]));
        assert_eq!(cases[1]["targetContractRefs"], json!(["c2"]));
        // без entities у c2 нет label — привязка невозможна
        let bare = extract_test_cases_from_source("a.test.ts", source, &contracts, &[]);
        assert_eq!(bare[1]["targetContractRefs"], json!([]));
    }

    #[test]
    fn merges_cases_deduping_and_uniting() {
        let a = json!({ "id": "t", "path": "p", "line": 1, "name": "n", "framework": "jest",
            "targetContractRefs": ["b", "a"], "assertions": [{ "kind": "throws" }], "evidence": ["e2"] });
        let b = json!({ "id": "t", "path": "p", "line": 1, "name": "n", "framework": "vitest",
            "targetContractRefs": ["a", "c"], "assertions": [{ "kind": "throws" }, { "kind": "equality" }],
            "evidence": ["e1"] });
        let merged = merge_test_cases(vec![vec![a], vec![b]]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["framework"], json!("vitest"));
        assert_eq!(merged[0]["targetContractRefs"], json!(["a", "b", "c"]));
        assert_eq!(merged[0]["assertions"], json!([{ "kind": "throws" }, { "kind": "equality" }]));
        assert_eq!(merged[0]["evidence"], json!(["e1", "e2"]));
    }

    #[test]
    fn generates_drafts_only_for_confirmed_property_kinds() {
        let obligations = vec![
            json!({ "id": "o1", "status": "confirmed", "kind": "boundary_value", "statement": "s", "contentHash": "h1" }),
            json!({ "id": "o2", "status": "confirmed", "kind": "error_path", "statement": "s", "contentHash": "h2" }),
            json!({ "id": "o3", "status": "draft", "kind": "invariant", "statement": "s", "contentHash": "h3" }),
            json!({ "id": "o4", "status": "confirmed", "kind": "entrypoint", "statement": "s", "contentHash": "h4" }),
        ];
        let drafts = generate_property_drafts(&obligations);
        assert_eq!(drafts.len(), 2);
        // порядок как в JS: сортировка по сгенерированному id (o2 идёт раньше o1)
        assert_eq!(drafts[0]["generators"], json!(["invalid", "empty", "malformed"]));
        assert_eq!(drafts[0]["expectedFailures"], json!(["invalid_input_rejected"]));
        assert_eq!(drafts[1]["generators"], json!(["minimum", "maximum", "below_minimum", "above_maximum", "empty"]));
    }

    #[test]
    fn parses_property_report_marking_stale() {
        let drafts = vec![json!({ "id": "p1", "obligationRef": "o1", "semanticHash": "h1" })];
        let report = json!({ "properties": [
            { "propertyId": "p1", "semanticHash": "h1", "numRuns": 10, "failed": true, "seed": 7 },
            { "propertyId": "p2", "semanticHash": "hx", "numRuns": "bad" },
        ] });
        let parsed = parse_property_report(Some(&report), &drafts);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["status"], json!("failed"));
        assert_eq!(parsed[0]["seed"], json!("7"));
        assert_eq!(parsed[0]["obligationRef"], json!("o1"));
        assert_eq!(parsed[1]["status"], json!("stale"));
        assert_eq!(parsed[1]["numRuns"], Value::Null);
        assert_eq!(parsed[1]["obligationRef"], Value::Null);
    }
}
