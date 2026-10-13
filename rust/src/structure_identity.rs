//! Порт lib/structure-identity.mjs (RFC 20, T18): сверка идентичности сущностей
//! между прошлым и текущим structure_facts-снимком.
//!
//! `reconcile_identity(current, previous)` → `{entities, events}`:
//! - точное совпадение по `observedId` — сущность наследует `lineageId`
//!   (`exact.lineageId || exact.observedId`), события нет;
//! - иначе ключ непрерывности `qualifiedName||label \0 kind \0 signature||''`:
//!   ровно один несопоставленный кандидат → `moved` / `renamed` / `moved_renamed`
//!   (по смене path/label) и наследование lineage; больше одного →
//!   `identity_unresolved` с `lineageId: null`; ни одного → `created`;
//! - несопоставленные прошлые сущности → `removed`;
//! - события сортируются по `JSON.stringify([kind, from||'', to||''])` через
//!   `localeCompare` — кортеж, а не ':'-конкат, чтобы id с ':' не ломали порядок.
//!
//! Паритет с JS:
//! - Map/Set по observedId — SameValueZero через `value_key`;
//! - шаблонная строка ключа непрерывности — `String(v)` (`js_to_string`), JS
//!   truthiness для `||`;
//! - `{...entity, lineageId}`: существующий ключ остаётся на своей позиции,
//!   новый дописывается в конец (порядок ключей как в V8);
//! - `localeCompare` (ICU root) аппроксимирован `icu_compare`: для печатного
//!   ASCII веса взяты из реальной сортировки V8 (пунктуация ICU идёт в порядке
//!   ` _-,;:!?.'"()[]{}@*/\&#%`^+<=>|~$`, затем цифры и буквы без учёта
//!   регистра), остальные символы — по кодовой точке после ASCII. Порядок важен
//!   именно здесь: в ключе `"` соседствует с ':' из id, и побайтовое сравнение
//!   поставило бы `x` раньше `x:y`, а ICU — наоборот.
//!
//! Отклонение: не-объектные элементы входа (в JS `entity.observedId` на `null`
//! бросил бы TypeError) трактуются как объекты без полей.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::findings::{js_to_string, js_truthy};
use crate::structure_evidence::{get, js_json_string, value_key};

/// Порядок печатного ASCII в ICU root (как отдаёт V8 `localeCompare`).
const ICU_ASCII_ORDER: &str =
    " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789abcdefghijklmnopqrstuvwxyz";

fn primary_weight(c: char) -> u32 {
    let lower = if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c };
    if lower.is_ascii() {
        if let Some(pos) = ICU_ASCII_ORDER.find(lower) {
            return pos as u32 + 1;
        }
        // управляющие символы ASCII: в ICU игнорируемы на primary-уровне; в ключах
        // событий их нет (JSON.stringify экранирует), ставим перед пробелом
        return 0;
    }
    let folded = c.to_lowercase().next().unwrap_or(c);
    0x100 + folded as u32
}

/// Аппроксимация `a.localeCompare(b)` (ICU root): primary — веса символов без
/// учёта регистра, tertiary — строчная раньше заглавной, затем кодовые точки.
pub fn icu_compare(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().map(primary_weight);
    let mut bi = b.chars().map(primary_weight);
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => break,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x != y => return x.cmp(&y),
            _ => {}
        }
    }
    for (x, y) in a.chars().zip(b.chars()) {
        if x != y {
            let rank = |c: char| u8::from(c.is_uppercase());
            return (rank(x), x).cmp(&(rank(y), y));
        }
    }
    a.cmp(b)
}

fn field<'a>(entity: &'a Value, key: &str) -> Option<&'a Value> {
    get(entity, key)
}

/// `a || b` для опциональных JSON-значений; undefined → None.
fn or_value<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    match a {
        Some(v) if js_truthy(v) => Some(v),
        _ => b,
    }
}

/// `${v}` в шаблонной строке: undefined → "undefined".
fn template(v: Option<&Value>) -> String {
    v.map(js_to_string).unwrap_or_else(|| "undefined".into())
}

/// continuityKey: `${qualifiedName || label}\0${kind}\0${signature || ''}`.
fn continuity_key(entity: &Value) -> String {
    let name = or_value(field(entity, "qualifiedName"), field(entity, "label"));
    let signature = match field(entity, "signature") {
        Some(v) if js_truthy(v) => js_to_string(v),
        _ => String::new(),
    };
    format!(
        "{}\0{}\0{}",
        template(name),
        template(field(entity, "kind")),
        signature
    )
}

fn observed_key(entity: &Value) -> String {
    value_key(field(entity, "observedId").unwrap_or(&Value::Null))
}

/// Значение для JSON: undefined → null (после round-trip JS даёт то же).
fn val(v: Option<&Value>) -> Value {
    v.cloned().unwrap_or(Value::Null)
}

/// `{...entity, lineageId: lineage}`.
fn with_lineage(entity: &Value, lineage: Value) -> Value {
    let mut map = match entity {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    map.insert("lineageId".into(), lineage);
    Value::Object(map)
}

fn event(kind: &str, from: Value, to: Value, confidence: &str, evidence: Vec<String>) -> Value {
    json!({
        "kind": kind,
        "from": from,
        "to": to,
        "confidence": confidence,
        "evidence": evidence,
    })
}

/// `JSON.stringify([event.kind, event.from || '', event.to || ''])`.
fn sort_key(event: &Value) -> String {
    let part = |key: &str| -> String {
        match get(event, key) {
            Some(Value::String(s)) if !s.is_empty() => js_json_string(s),
            Some(v) if js_truthy(v) => serde_json::to_string(v).unwrap_or_default(),
            _ => "\"\"".into(),
        }
    };
    format!("[{},{},{}]", part("kind"), part("from"), part("to"))
}

/// Порт reconcileIdentity из lib/structure-identity.mjs. Возвращает
/// `{entities, events}`.
pub fn reconcile_identity(current: &[Value], previous: &[Value]) -> Value {
    // new Map(previous.map(e => [e.observedId, e])): последний дубликат побеждает
    let mut previous_by_observed: HashMap<String, &Value> = HashMap::new();
    for entity in previous {
        previous_by_observed.insert(observed_key(entity), entity);
    }
    let mut previous_by_continuity: HashMap<String, Vec<&Value>> = HashMap::new();
    for entity in previous {
        previous_by_continuity
            .entry(continuity_key(entity))
            .or_default()
            .push(entity);
    }
    let mut matched_previous: HashSet<String> = HashSet::new();
    let mut events: Vec<Value> = Vec::new();
    let mut entities: Vec<Value> = Vec::with_capacity(current.len());

    for entity in current {
        let observed = val(field(entity, "observedId"));
        if let Some(exact) = previous_by_observed.get(&observed_key(entity)) {
            matched_previous.insert(observed_key(exact));
            let lineage = val(or_value(field(exact, "lineageId"), field(exact, "observedId")));
            entities.push(with_lineage(entity, lineage));
            continue;
        }
        let candidates: Vec<&Value> = previous_by_continuity
            .get(&continuity_key(entity))
            .map(|list| {
                list.iter()
                    .copied()
                    .filter(|c| !matched_previous.contains(&observed_key(c)))
                    .collect()
            })
            .unwrap_or_default();
        if candidates.len() == 1 {
            let prev = candidates[0];
            matched_previous.insert(observed_key(prev));
            let path_changed = !strict_eq(field(prev, "path"), field(entity, "path"));
            let label_changed = !strict_eq(field(prev, "label"), field(entity, "label"));
            let kind = if path_changed && label_changed {
                "moved_renamed"
            } else if path_changed {
                "moved"
            } else {
                "renamed"
            };
            events.push(event(
                kind,
                val(field(prev, "observedId")),
                observed.clone(),
                "extracted",
                vec![format!(
                    "identity:{}:{}->{}:{}",
                    template(field(prev, "path")),
                    template(field(prev, "label")),
                    template(field(entity, "path")),
                    template(field(entity, "label")),
                )],
            ));
            let lineage = val(or_value(field(prev, "lineageId"), field(prev, "observedId")));
            entities.push(with_lineage(entity, lineage));
            continue;
        }
        if candidates.len() > 1 {
            let mut evidence: Vec<String> = candidates
                .iter()
                .map(|c| format!("identity-candidate:{}", template(field(c, "observedId"))))
                .collect();
            // дефолтный Array.sort — по UTF-16 code units
            evidence.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            events.push(event(
                "identity_unresolved",
                Value::Null,
                observed.clone(),
                "ambiguous",
                evidence,
            ));
            entities.push(with_lineage(entity, Value::Null));
            continue;
        }
        events.push(event(
            "created",
            Value::Null,
            observed.clone(),
            "exact",
            vec![format!("identity:new:{}", template(field(entity, "observedId")))],
        ));
        entities.push(entity.clone());
    }

    for prev in previous {
        if matched_previous.contains(&observed_key(prev)) {
            continue;
        }
        events.push(event(
            "removed",
            val(field(prev, "observedId")),
            Value::Null,
            "extracted",
            vec![format!("identity:absent:{}", template(field(prev, "observedId")))],
        ));
    }

    // Array.prototype.sort стабилен — sort_by тоже
    let mut keyed: Vec<(String, Value)> = events.into_iter().map(|e| (sort_key(&e), e)).collect();
    keyed.sort_by(|a, b| icu_compare(&a.0, &b.0));
    let events: Vec<Value> = keyed.into_iter().map(|(_, e)| e).collect();
    json!({ "entities": entities, "events": events })
}

/// Строгое `!==`-сравнение двух опциональных полей (undefined === undefined).
fn strict_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (Some(Value::Object(_)), _) | (Some(Value::Array(_)), _) => false,
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn load(rel: &str) -> Value {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join(rel);
        serde_json::from_str(&std::fs::read_to_string(&path).expect(rel)).expect(rel)
    }

    fn arr(v: &Value) -> Vec<Value> {
        v.as_array().cloned().unwrap_or_default()
    }

    #[test]
    fn fixture_entities_reconcile_to_themselves() {
        let facts = load("fixtures/structure/structure-facts.v1.json");
        let entities = arr(&facts["facts"]["entities"]);
        let out = reconcile_identity(&entities, &entities);
        assert_eq!(out["entities"], json!(entities));
        assert_eq!(out["events"], json!([]));
        // identityEvents фикстуры согласованы с пустой сверкой
        assert_eq!(facts["identityEvents"], json!([]));
    }

    /// Эталон JS: rust/parity/golden-structure/t18.json (генератор рядом).
    #[test]
    fn golden_parity_with_js() {
        let golden = load("rust/parity/golden-structure/t18.json");
        let cases = golden["identity"].as_array().unwrap();
        assert!(cases.len() >= 12);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let out = reconcile_identity(&arr(&case["current"]), &arr(&case["previous"]));
            assert_eq!(
                serde_json::to_string(&out).unwrap(),
                serde_json::to_string(&case["expected"]).unwrap(),
                "кейс {name}"
            );
        }
    }

    #[test]
    fn colon_ids_sort_like_icu() {
        let e = |id: &str, path: &str, label: &str, q: &str| {
            json!({"observedId": id, "lineageId": id, "path": path, "label": label,
                   "qualifiedName": q, "kind": "function", "signature": "s"})
        };
        let previous = vec![e("x:y", "src/one.ts", "one", "pkg/one"), e("x", "src/two.ts", "two", "pkg/two")];
        let current = vec![e("p", "src/one.ts", "oneR", "pkg/one"), e("y:p", "src/two.ts", "twoR", "pkg/two")];
        let pairs = |prev: &[Value], cur: &[Value]| -> Vec<String> {
            arr(&reconcile_identity(cur, prev)["events"])
                .iter()
                .map(|ev| format!("{}→{}", ev["from"].as_str().unwrap(), ev["to"].as_str().unwrap()))
                .collect()
        };
        let first = pairs(&previous, &current);
        assert_eq!(first, vec!["x:y→p", "x→y:p"]);
        let rev_prev: Vec<Value> = previous.iter().rev().cloned().collect();
        let rev_cur: Vec<Value> = current.iter().rev().cloned().collect();
        assert_eq!(pairs(&rev_prev, &rev_cur), first);
    }

    #[test]
    fn icu_compare_ascii_punctuation() {
        assert_eq!(icu_compare(":", "\""), Ordering::Less);
        assert_eq!(icu_compare("_", "-"), Ordering::Less);
        assert_eq!(icu_compare("$", "0"), Ordering::Less);
        assert_eq!(icu_compare("a", "A"), Ordering::Less);
        assert_eq!(icu_compare("A", "b"), Ordering::Less);
        assert_eq!(icu_compare("ab", "a"), Ordering::Greater);
    }
}
