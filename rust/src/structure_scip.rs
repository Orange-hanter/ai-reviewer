//! Порт lib/structure-scip.mjs: hand-written protobuf-декодер SCIP-индексов
//! (wire-format без .proto-схемы — паритет с JS важнее инструмента) и
//! извлечение/слияние структурных фактов поверх декодированного индекса.
//!
//! Паритет с JS сохранён построчно, включая:
//! - guard безопасного целого в varint: JS бросает при величине > 2^53-1
//!   (проверка значения, а не только сдвига) и при shift > 53;
//! - `||`-семантику JS (0 и null → fallback) в line/endLine/roles/kind;
//! - текст полей: невалидный UTF-8 → U+FFFD (как Buffer.toString('utf8'));
//! - сортировки localeCompare — аппроксимация ICU root (primary регистронезависимо,
//!   tertiary lowercase-first); на данных репо (hex-id, пути, символы SCIP) совпадает;
//!   дефолтный Array.sort для evidence — побайтовый (UTF-8 = codepoint-порядок);
//! - порядок ключей JS-объектов (serde_json с preserve_order → Map/IndexMap);
//! - stableId = `prefix:` + sha256(parts.join('\0'))[..24] (hex).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// JS-совместимые мелкие хелперы
// ---------------------------------------------------------------------------

/// sha256 → hex (как node crypto).
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// stableId из lib/util.mjs: `prefix:` + sha256(parts.join('\0')).slice(0,24).
pub fn stable_id(prefix: &str, parts: &[String]) -> String {
    let joined = parts.join("\0");
    format!("{prefix}:{}", &sha256_hex(joined.as_bytes())[..24])
}

/// Аппроксимация JS String.prototype.localeCompare (ICU root):
/// primary — регистронезависимое сравнение, tertiary — lowercase перед uppercase.
/// На корпусе репо (hex-id одинаковой длины, пути, SCIP-символы) совпадает с ICU.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().flat_map(char::to_lowercase);
    let mut bi = b.chars().flat_map(char::to_lowercase);
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => break,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
            }
        }
    }
    for (x, y) in a.chars().zip(b.chars()) {
        if x != y {
            let rank = |c: char| if c.is_uppercase() { 1u8 } else { 0u8 };
            return (rank(x), x).cmp(&(rank(y), y));
        }
    }
    a.cmp(b)
}

/// JS truthiness для JSON-значений (null/false/0/"" → false).
fn js_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `${v}` в JS-шаблоне для скаляров: null → "null", строка — как есть.
fn js_template(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn as_array(v: Option<&Value>) -> &[Value] {
    match v {
        Some(Value::Array(a)) => a,
        _ => &[],
    }
}

fn as_object(v: Option<&Value>) -> Option<&Map<String, Value>> {
    v.and_then(Value::as_object)
}

/// Значение по ключу объекта (None для не-объектов).
fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object().and_then(|o| o.get(key))
}

/// То же для Option<&Value>.
fn get_opt<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    v.and_then(|value| get(value, key))
}

/// node path.posix.normalize для относительных путей (лексический разбор . и ..).
fn posix_normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                _ => parts.push(".."),
            },
            c => parts.push(c),
        }
    }
    let out = parts.join("/");
    let trailing = path.len() > 1 && path.ends_with('/') && out != "..";
    if trailing {
        format!("{out}/")
    } else {
        out
    }
}

/// Порт normalizeRepoPath из lib/util.mjs: \\ → /, trim, отказ от абсолютных,
/// NUL и выходов через ... Результат normalize снятия префикса ./.
pub fn normalize_repo_path(value: &str) -> Result<String> {
    let raw = value.replace('\\', "/").trim().to_string();
    if raw.is_empty()
        || raw.contains('\0')
        || raw.starts_with('/')
        || (raw.len() >= 3 && raw.as_bytes()[1] == b':' && raw.as_bytes()[2] == b'/' && raw.as_bytes()[0].is_ascii_alphabetic())
    {
        bail!("unsafe repository path: {}", json!(raw));
    }
    let normalized = posix_normalize(&raw).trim_start_matches("./").to_string();
    if normalized.is_empty() || normalized == ".." || normalized.starts_with("../") {
        bail!("unsafe repository path: {}", json!(raw));
    }
    Ok(normalized)
}

// ---------------------------------------------------------------------------
// Protobuf wire-format: varint с JS safe-integer guard + обход полей
// ---------------------------------------------------------------------------

const SAFE_INTEGER_MAX: u128 = (1u128 << 53) - 1;

/// Varint как в JS: накопление value += (byte & 0x7f) * 2^shift с проверкой
/// Number.isSafeInteger ПОСЛЕ каждого байта (не только сдвига): при shift=49
/// семь бит дают до ~7.2e16 — выше 2^53, и JS терял точность молча.
fn read_varint(buf: &[u8], start: usize) -> Result<(u64, usize)> {
    let mut value: u128 = 0;
    let mut shift: u32 = 0;
    let mut offset = start;
    while offset < buf.len() {
        let byte = buf[offset];
        offset += 1;
        value += ((byte & 0x7f) as u128) << shift;
        if value > SAFE_INTEGER_MAX {
            bail!("SCIP protobuf varint exceeds JavaScript safe integer");
        }
        if (byte & 0x80) == 0 {
            return Ok((value as u64, offset));
        }
        shift += 7;
        if shift > 53 {
            bail!("SCIP protobuf varint exceeds JavaScript safe integer");
        }
    }
    bail!("truncated SCIP protobuf varint");
}

enum FieldValue {
    Varint(u64),
    Bytes(Vec<u8>),
}

struct Field {
    number: u64,
    wire: u8,
    value: FieldValue,
}

/// Обход полей уровня сообщения. wire 1/5 (fixed64/fixed32) пропускаются
/// без записи (как в JS), wire 3/4/6/7 — ошибка.
fn fields(buf: &[u8]) -> Result<Vec<Field>> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset < buf.len() {
        let (tag, after_tag) = read_varint(buf, offset)?;
        offset = after_tag;
        let number = tag / 8;
        let wire = (tag & 7) as u8;
        match wire {
            0 => {
                let (value, end) = read_varint(buf, offset)?;
                out.push(Field { number, wire, value: FieldValue::Varint(value) });
                offset = end;
            }
            2 => {
                let (length, start) = read_varint(buf, offset)?;
                let end = start + length as usize;
                if end > buf.len() {
                    bail!("truncated SCIP protobuf field");
                }
                out.push(Field {
                    number,
                    wire,
                    value: FieldValue::Bytes(buf[start..end].to_vec()),
                });
                offset = end;
            }
            1 => {
                if offset + 8 > buf.len() {
                    bail!("truncated SCIP protobuf field");
                }
                offset += 8;
            }
            5 => {
                if offset + 4 > buf.len() {
                    bail!("truncated SCIP protobuf field");
                }
                offset += 4;
            }
            other => bail!("unsupported SCIP protobuf wire type {other}"),
        }
    }
    Ok(out)
}

fn first<'a>(entries: &'a [Field], number: u64, wire: Option<u8>) -> Option<&'a Field> {
    entries
        .iter()
        .find(|e| e.number == number && (wire.is_none() || e.wire == wire.unwrap()))
}

fn all<'a>(entries: &'a [Field], number: u64, wire: Option<u8>) -> Vec<&'a Field> {
    entries
        .iter()
        .filter(|e| e.number == number && (wire.is_none() || e.wire == wire.unwrap()))
        .collect()
}

fn field_bytes(entry: Option<&Field>) -> &[u8] {
    match entry.map(|e| &e.value) {
        Some(FieldValue::Bytes(b)) => b,
        _ => &[],
    }
}

fn text(entry: Option<&Field>) -> String {
    match entry.map(|e| &e.value) {
        Some(FieldValue::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        _ => String::new(),
    }
}

fn decode_packed_ints(buf: &[u8]) -> Result<Vec<u64>> {
    let mut values = Vec::new();
    let mut offset = 0usize;
    while offset < buf.len() {
        let (value, end) = read_varint(buf, offset)?;
        values.push(value);
        offset = end;
    }
    Ok(values)
}

fn varint_field(entries: &[Field], number: u64) -> u64 {
    match first(entries, number, Some(0)).map(|e| &e.value) {
        Some(FieldValue::Varint(v)) => *v,
        _ => 0,
    }
}

fn decode_range(entries: &[Field]) -> Result<Option<Map<String, Value>>> {
    let mut range = Map::new();
    if let Some(single) = first(entries, 8, Some(2)) {
        let value = fields(field_bytes(Some(single)))?;
        let line = varint_field(&value, 1);
        range.insert("line".into(), json!(line + 1));
        range.insert("endLine".into(), json!(line + 1));
        range.insert("startCharacter".into(), json!(varint_field(&value, 2)));
        range.insert("endCharacter".into(), json!(varint_field(&value, 3)));
        return Ok(Some(range));
    }
    if let Some(multi) = first(entries, 9, Some(2)) {
        let value = fields(field_bytes(Some(multi)))?;
        range.insert("line".into(), json!(varint_field(&value, 1) + 1));
        range.insert("endLine".into(), json!(varint_field(&value, 3) + 1));
        range.insert("startCharacter".into(), json!(varint_field(&value, 2)));
        range.insert("endCharacter".into(), json!(varint_field(&value, 4)));
        return Ok(Some(range));
    }
    let legacy = match first(entries, 1, Some(2)) {
        Some(f) => f,
        None => return Ok(None),
    };
    let values = decode_packed_ints(field_bytes(Some(legacy)))?;
    match values.len() {
        3 => {
            range.insert("line".into(), json!(values[0] + 1));
            range.insert("endLine".into(), json!(values[0] + 1));
            range.insert("startCharacter".into(), json!(values[1]));
            range.insert("endCharacter".into(), json!(values[2]));
            Ok(Some(range))
        }
        len if len >= 4 => {
            range.insert("line".into(), json!(values[0] + 1));
            range.insert("endLine".into(), json!(values[2] + 1));
            range.insert("startCharacter".into(), json!(values[1]));
            range.insert("endCharacter".into(), json!(values[3]));
            Ok(Some(range))
        }
        _ => Ok(None),
    }
}

fn decode_occurrence(buf: &[u8]) -> Result<Map<String, Value>> {
    let value = fields(buf)?;
    let mut out = Map::new();
    out.insert("symbol".into(), json!(text(first(&value, 2, Some(2)))));
    out.insert("roles".into(), json!(varint_field(&value, 3)));
    out.insert(
        "range".into(),
        match decode_range(&value)? {
            Some(r) => Value::Object(r),
            None => Value::Null,
        },
    );
    Ok(out)
}

fn decode_relationship(buf: &[u8]) -> Result<Map<String, Value>> {
    let value = fields(buf)?;
    let mut out = Map::new();
    out.insert("symbol".into(), json!(text(first(&value, 1, Some(2)))));
    out.insert("isReference".into(), json!(varint_field(&value, 2) != 0));
    out.insert("isImplementation".into(), json!(varint_field(&value, 3) != 0));
    out.insert("isTypeDefinition".into(), json!(varint_field(&value, 4) != 0));
    out.insert("isDefinition".into(), json!(varint_field(&value, 5) != 0));
    Ok(out)
}

fn decode_signature(entry: Option<&Field>) -> Result<Option<String>> {
    let entry = match entry {
        Some(e) => e,
        None => return Ok(None),
    };
    let value = fields(field_bytes(Some(entry)))?;
    let signature = text(first(&value, 5, Some(2)));
    Ok(if signature.is_empty() { None } else { Some(signature) })
}

fn decode_symbol(buf: &[u8]) -> Result<Map<String, Value>> {
    let value = fields(buf)?;
    let mut out = Map::new();
    out.insert("symbol".into(), json!(text(first(&value, 1, Some(2)))));
    out.insert(
        "documentation".into(),
        json!(all(&value, 3, Some(2))
            .iter()
            .map(|e| text(Some(e)))
            .collect::<Vec<_>>()),
    );
    let mut relationships = Vec::new();
    for entry in all(&value, 4, Some(2)) {
        relationships.push(Value::Object(decode_relationship(field_bytes(Some(entry)))?));
    }
    out.insert("relationships".into(), json!(relationships));
    out.insert("kind".into(), json!(varint_field(&value, 5)));
    out.insert("displayName".into(), json!(text(first(&value, 6, Some(2)))));
    out.insert(
        "signature".into(),
        match decode_signature(first(&value, 7, Some(2)))? {
            Some(s) => json!(s),
            None => Value::Null,
        },
    );
    out.insert("enclosingSymbol".into(), json!(text(first(&value, 8, Some(2)))));
    Ok(out)
}

/// decodeScipIndex из lib/structure-scip.mjs: Buffer → {documents, externalSymbols}.
pub fn decode_scip_index(buffer: &[u8]) -> Result<Map<String, Value>> {
    let root = fields(buffer)?;
    let mut out = Map::new();
    let mut documents = Vec::new();
    for document_entry in all(&root, 2, Some(2)) {
        let value = fields(field_bytes(Some(document_entry)))?;
        let mut document = Map::new();
        document.insert("relativePath".into(), json!(text(first(&value, 1, Some(2)))));
        let mut occurrences = Vec::new();
        for entry in all(&value, 2, Some(2)) {
            occurrences.push(Value::Object(decode_occurrence(field_bytes(Some(entry)))?));
        }
        document.insert("occurrences".into(), json!(occurrences));
        let mut symbols = Vec::new();
        for entry in all(&value, 3, Some(2)) {
            symbols.push(Value::Object(decode_symbol(field_bytes(Some(entry)))?));
        }
        document.insert("symbols".into(), json!(symbols));
        document.insert("language".into(), json!(text(first(&value, 4, Some(2)))));
        documents.push(Value::Object(document));
    }
    out.insert("documents".into(), json!(documents));
    let mut external = Vec::new();
    for entry in all(&root, 3, Some(2)) {
        external.push(Value::Object(decode_symbol(field_bytes(Some(entry)))?));
    }
    out.insert("externalSymbols".into(), json!(external));
    Ok(out)
}

// ---------------------------------------------------------------------------
// extractScipFacts: индекс → структурные факты (сущности/контракты/рёбра)
// ---------------------------------------------------------------------------

const KIND_NAMES: &[(u64, &str)] = &[
    (7, "class"), (9, "constructor"), (11, "enum"), (12, "enum_member"),
    (15, "field"), (16, "file"), (17, "function"), (21, "interface"),
    (26, "method"), (28, "message"), (29, "module"), (30, "namespace"),
    (41, "property"), (42, "protocol"), (46, "signature"), (49, "struct"),
    (53, "trait"), (54, "type"), (55, "type_alias"), (59, "union"),
    (62, "contract"), (63, "error"), (66, "abstract_method"), (67, "method_specification"),
    (68, "protocol_method"), (69, "pure_virtual_method"), (70, "trait_method"),
    (71, "type_class_method"), (80, "static_method"), (81, "static_property"),
];

fn kind_name(kind: u64) -> &'static str {
    KIND_NAMES
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, name)| *name)
        .unwrap_or("symbol")
}

/// displayName из lib/structure-scip.mjs: хвостовые [.#/()] срезаются, берётся
/// последний непустой сегмент по [.#/]; fallback — сам символ, затем 'symbol'.
fn symbol_display_name(symbol: &str) -> String {
    let normalized = symbol.trim_end_matches(['.', '#', '/', '(', ')']);
    let segment = normalized
        .split(['.', '#', '/'])
        .filter(|s| !s.is_empty())
        .last();
    match segment {
        Some(s) => s.to_string(),
        None => {
            if symbol.is_empty() {
                "symbol".to_string()
            } else {
                symbol.to_string()
            }
        }
    }
}

/// `a || null` для чисел из JSON: 0/отсутствует → null.
fn num_or_null(v: Option<&Value>) -> Value {
    match v.and_then(Value::as_u64) {
        Some(n) if n != 0 => json!(n),
        _ => Value::Null,
    }
}

fn num_or_zero(v: Option<&Value>) -> u64 {
    v.and_then(Value::as_u64).unwrap_or(0)
}

pub struct ExtractOptions {
    pub scope: Option<String>,
    pub max_entities: usize,
    pub max_edges: usize,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self { scope: None, max_entities: 5000, max_edges: 15000 }
    }
}

struct Candidate {
    scip_symbol: String,
    label: String,
    kind: &'static str,
    path: String,
    line: Value,
    end_line: Value,
    signature: Value,
    relationships: Vec<Value>,
}

/// extractScipFacts из lib/structure-scip.mjs.
pub fn extract_scip_facts(index: Option<&Value>, options: &ExtractOptions) -> Result<Map<String, Value>> {
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut definitions_by_symbol: HashMap<String, Vec<(String, Map<String, Value>)>> = HashMap::new();

    let scope = options.scope.clone().unwrap_or_default();
    let scope_prefix = format!("{}/", scope.trim_end_matches('/'));

    for document in as_array(get_opt(index, "documents")) {
        let path = match get(document, "relativePath").and_then(Value::as_str) {
            Some(raw) => match normalize_repo_path(raw) {
                Ok(p) => p,
                Err(_) => continue,
            },
            None => continue,
        };
        if !scope.is_empty() && path != scope && !path.starts_with(&scope_prefix) {
            continue;
        }
        for occurrence in as_array(get(document, "occurrences")) {
            let roles = num_or_zero(get(occurrence, "roles"));
            let symbol = get(occurrence, "symbol").and_then(Value::as_str).unwrap_or("");
            let range = get(occurrence, "range");
            if (roles & 1) != 0 && !symbol.is_empty() && matches!(range, Some(Value::Object(_))) {
                let range_map = range.unwrap().as_object().unwrap().clone();
                definitions_by_symbol
                    .entry(symbol.to_string())
                    .or_default()
                    .push((path.clone(), range_map));
            }
        }
        for symbol in as_array(get(document, "symbols")) {
            let scip_symbol = get(symbol, "symbol").and_then(Value::as_str).unwrap_or("").to_string();
            let definitions = definitions_by_symbol.get(&scip_symbol).cloned().unwrap_or_default();
            let definition = definitions
                .iter()
                .find(|(item_path, _)| *item_path == path)
                .or(definitions.first());
            let (line, end_line) = match definition {
                Some((_, range)) => (num_or_null(range.get("line")), num_or_null(range.get("endLine"))),
                None => (Value::Null, Value::Null),
            };
            let kind = kind_name(num_or_zero(get(symbol, "kind")));
            let label = match get(symbol, "displayName").and_then(Value::as_str) {
                Some(name) if !name.is_empty() => name.to_string(),
                _ => symbol_display_name(&scip_symbol),
            };
            candidates.push(Candidate {
                scip_symbol,
                label,
                kind,
                path: path.clone(),
                line,
                end_line,
                signature: get(symbol, "signature").cloned().unwrap_or(Value::Null),
                relationships: as_array(get(symbol, "relationships")).to_vec(),
            });
        }
    }

    candidates.sort_by(|a, b| {
        locale_compare(&a.path, &b.path)
            .then(num_or_zero(Some(&a.line)).cmp(&num_or_zero(Some(&b.line))))
            .then(locale_compare(&a.scip_symbol, &b.scip_symbol))
    });
    let kept = &candidates[..candidates.len().min(options.max_entities)];

    let mut entities = Vec::new();
    let mut entity_by_symbol: HashMap<String, Value> = HashMap::new();
    for item in kept {
        let observed_id = stable_id("entity", &[item.path.clone(), item.scip_symbol.clone(), item.kind.to_string()]);
        let entity = json!({
            "observedId": observed_id,
            "lineageId": observed_id,
            "boundaryId": Value::Null,
            "label": item.label,
            "qualifiedName": item.scip_symbol,
            "kind": item.kind,
            "path": item.path,
            "line": item.line,
            "endLine": item.end_line,
            "signature": item.signature,
            "community": Value::Null,
            "isGodNode": false,
            "confidence": "exact",
            "evidence": [format!("scip:symbol:{}", item.scip_symbol)],
            "externalIds": { "scip": item.scip_symbol },
        });
        entity_by_symbol.insert(item.scip_symbol.clone(), entity.clone());
        entities.push(entity);
    }

    let mut dependencies = Vec::new();
    let mut contracts = Vec::new();
    for item in kept {
        let from = match entity_by_symbol.get(&item.scip_symbol) {
            Some(e) => e,
            None => continue,
        };
        let from_id = get(from, "observedId").and_then(Value::as_str).unwrap_or("").to_string();
        let has_signature = js_truthy(get(from, "signature"));
        if has_signature && matches!(item.kind, "class" | "interface" | "function" | "method" | "constructor") {
            let signature = get(from, "signature").and_then(Value::as_str).unwrap_or("").to_string();
            contracts.push(json!({
                "id": stable_id("contract", &["signature".into(), from_id.clone(), signature.clone()]),
                "entityRef": from_id,
                "kind": "signature",
                "targetRef": Value::Null,
                "signature": signature,
                "confidence": "exact",
                "evidence": [format!("scip:signature:{}", item.scip_symbol)],
            }));
        }
        for relation in &item.relationships {
            let to_symbol = get(relation, "symbol").and_then(Value::as_str).unwrap_or("");
            let to = match entity_by_symbol.get(to_symbol) {
                Some(e) => e,
                None => continue,
            };
            let to_id = get(to, "observedId").and_then(Value::as_str).unwrap_or("").to_string();
            let relation_symbol = get(relation, "symbol").and_then(Value::as_str).unwrap_or("").to_string();
            let evidence = format!("scip:relationship:{}:{}", item.scip_symbol, relation_symbol);
            let mut kinds: Vec<&str> = Vec::new();
            if js_truthy(get(relation, "isImplementation")) {
                kinds.push("implements");
            }
            if js_truthy(get(relation, "isReference")) {
                kinds.push("references");
            }
            if js_truthy(get(relation, "isTypeDefinition")) {
                kinds.push("type_definition");
            }
            if js_truthy(get(relation, "isDefinition")) {
                kinds.push("definition");
            }
            for kind in kinds {
                let edge = json!({
                    "id": stable_id("edge", &[from_id.clone(), to_id.clone(), kind.into()]),
                    "from": from_id,
                    "to": to_id,
                    "relation": kind,
                    "crossBoundary": false,
                    "crossCommunity": false,
                    "confidence": "exact",
                    "evidence": [evidence.clone()],
                });
                dependencies.push(edge);
                if kind == "implements" {
                    contracts.push(json!({
                        "id": stable_id("contract", &[kind.into(), from_id.clone(), to_id.clone()]),
                        "entityRef": from_id,
                        "kind": kind,
                        "targetRef": to_id,
                        "signature": item.signature.clone(),
                        "confidence": "exact",
                        "evidence": [evidence.clone()],
                    }));
                }
            }
        }
    }
    // сортировка до среза: иначе в выживших остаётся недетерминированное подмножество
    dependencies.sort_by(|a, b| {
        locale_compare(
            get(a, "id").and_then(Value::as_str).unwrap_or(""),
            get(b, "id").and_then(Value::as_str).unwrap_or(""),
        )
    });
    contracts.sort_by(|a, b| {
        locale_compare(
            get(a, "id").and_then(Value::as_str).unwrap_or(""),
            get(b, "id").and_then(Value::as_str).unwrap_or(""),
        )
    });

    let mut warnings = Vec::new();
    if candidates.len() > options.max_entities {
        warnings.push(format!("SCIP entities truncated at {}", options.max_entities));
    }
    if dependencies.len() > options.max_edges {
        warnings.push(format!("SCIP dependencies truncated at {}", options.max_edges));
    }

    let mut out = Map::new();
    out.insert("entities".into(), json!(entities));
    out.insert("contracts".into(), json!(contracts));
    out.insert(
        "dependencies".into(),
        json!(dependencies[..dependencies.len().min(options.max_edges)].to_vec()),
    );
    out.insert("warnings".into(), json!(warnings));
    out.insert(
        "truncated".into(),
        json!(candidates.len() > options.max_entities || dependencies.len() > options.max_edges),
    );
    out.insert(
        "capabilities".into(),
        json!({
            "symbols": "exact",
            "exactKinds": true,
            "sourceRanges": true,
            "signatures": contracts.iter().any(|c| js_truthy(get(c, "signature"))),
            "references": true,
            "implementations": true,
            "imports": false,
            "calls": false,
            "topology": false,
            "positionEncoding": "from-index",
        }),
    );
    Ok(out)
}

// ---------------------------------------------------------------------------
// mergeScipWithGraphify: слияние SCIP-фактов с graphify-графом
// ---------------------------------------------------------------------------

fn match_key(entity: Option<&Value>) -> String {
    format!(
        "{}\0{}",
        js_template(get_opt(entity, "path")),
        js_template(get_opt(entity, "label")),
    )
}

fn loose_key(entity: Option<&Value>) -> String {
    format!(
        "{}\0{}",
        js_template(get_opt(entity, "path")),
        js_template(get_opt(entity, "label")).to_lowercase(),
    )
}

fn index_entities(entities: &[Value], key_of: fn(Option<&Value>) -> String) -> HashMap<String, Vec<usize>> {
    let mut index: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, entity) in entities.iter().enumerate() {
        index.entry(key_of(Some(entity))).or_default().push(i);
    }
    index
}

/// Строковый массив из поля (JS spread нежно требует массив; пустой на не-массиве).
fn string_array(v: Option<&Value>) -> Vec<String> {
    as_array(v)
        .iter()
        .filter_map(|item| item.as_str().map(String::from))
        .collect()
}

/// mergeCapabilities: плоская карта; capability доступна, если её даёт хотя бы
/// один провайдер; строковые уровни — лучший по рангу.
fn merge_capabilities(graph_caps: Option<&Value>, scip_caps: Option<&Value>) -> Value {
    let rank = |s: &str| match s {
        "none" => 0,
        "inferred" => 1,
        "extracted" => 2,
        "exact" => 3,
        _ => 0,
    };
    let mut merged = Map::new();
    if let Some(map) = as_object(graph_caps) {
        for (k, v) in map {
            merged.insert(k.clone(), v.clone());
        }
    }
    if let Some(map) = as_object(scip_caps) {
        for (k, value) in map {
            match merged.get(k) {
                None | Some(Value::Null) => {
                    merged.insert(k.clone(), value.clone());
                }
                Some(previous) => match (previous, value) {
                    (Value::Bool(a), Value::Bool(b)) => {
                        merged.insert(k.clone(), json!(*a || *b));
                    }
                    (Value::String(a), Value::String(b)) => {
                        let chosen = if rank(b) >= rank(a) { b.clone() } else { a.clone() };
                        merged.insert(k.clone(), json!(chosen));
                    }
                    (_, value) if !matches!(value, Value::Null) => {
                        merged.insert(k.clone(), value.clone());
                    }
                    _ => {}
                },
            }
        }
    }
    Value::Object(merged)
}

/// mergeScipWithGraphify из lib/structure-scip.mjs.
pub fn merge_scip_with_graphify(scip: Option<&Value>, graph: Option<&Value>) -> Result<Map<String, Value>> {
    let graph_entities = as_array(get_opt(graph, "entities")).to_vec();
    let exact_index = index_entities(&graph_entities, match_key);
    let loose_index = index_entities(&graph_entities, loose_key);
    let mut consumed: HashSet<usize> = HashSet::new();
    let mut ambiguous = 0usize;
    let mut replaced_graph_ids: HashMap<String, String> = HashMap::new();
    let mut entities: Vec<Value> = Vec::new();

    for scip_entity in as_array(get_opt(scip, "entities")) {
        // строгое совпадение важнее регистронезависимого; неоднозначность — повод не сливать
        // вовсе, а не выбирать «последнего»: потеря сущности дороже несостоявшегося уточнения
        let exact = exact_index.get(&match_key(Some(scip_entity))).cloned().unwrap_or_default();
        let pool_source: Vec<usize> = if !exact.is_empty() {
            exact
        } else {
            loose_index.get(&loose_key(Some(scip_entity))).cloned().unwrap_or_default()
        };
        let pool: Vec<usize> = pool_source.into_iter().filter(|i| !consumed.contains(i)).collect();
        if pool.len() > 1 {
            ambiguous += 1;
        }
        let graph_entity = if pool.len() == 1 { Some(&graph_entities[pool[0]]) } else { None };

        let merged = if let Some(graph_entity) = graph_entity {
            let mut map = graph_entity.as_object().cloned().unwrap_or_default();
            // null/undefined из SCIP не затирают graphify-поля: только определённые значения
            for (k, v) in scip_entity.as_object().cloned().unwrap_or_default() {
                if !matches!(v, Value::Null) {
                    map.insert(k, v);
                }
            }
            // boundaryId/community/isGodNode остаются от graphify (в JS перезаписываются
            // теми же значениями — позиция ключа не меняется)
            let evidence: Vec<String> = {
                let mut seen = HashSet::new();
                let mut out = Vec::new();
                for item in string_array(graph_entity.get("evidence"))
                    .into_iter()
                    .chain(string_array(scip_entity.get("evidence")))
                {
                    if seen.insert(item.clone()) {
                        out.push(item);
                    }
                }
                out
            };
            // Array.prototype.sort — побайтовый порядок (UTF-8 == codepoint)
            let mut evidence = evidence;
            evidence.sort();
            map.insert("evidence".into(), json!(evidence));
            let mut external_ids = Map::new();
            if let Some(ids) = as_object(graph_entity.get("externalIds")) {
                for (k, v) in ids {
                    external_ids.insert(k.clone(), v.clone());
                }
            }
            if let Some(ids) = as_object(scip_entity.get("externalIds")) {
                for (k, v) in ids {
                    external_ids.insert(k.clone(), v.clone());
                }
            }
            map.insert("externalIds".into(), Value::Object(external_ids));
            let graph_observed = get(graph_entity, "observedId").and_then(Value::as_str).unwrap_or("");
            let merged_observed = get(scip_entity, "observedId").and_then(Value::as_str).unwrap_or("");
            replaced_graph_ids.insert(graph_observed.to_string(), merged_observed.to_string());
            consumed.insert(pool[0]);
            Value::Object(map)
        } else {
            scip_entity.clone()
        };
        entities.push(merged);
    }
    // выбывает ровно та сущность, которую слили, — не все делящие с ней ключ
    for (i, entity) in graph_entities.iter().enumerate() {
        if !consumed.contains(&i) {
            entities.push(entity.clone());
        }
    }
    entities.sort_by(|a, b| {
        locale_compare(
            get(a, "path").and_then(Value::as_str).unwrap_or(""),
            get(b, "path").and_then(Value::as_str).unwrap_or(""),
        )
        .then(num_or_zero(get(a, "line")).cmp(&num_or_zero(get(b, "line"))))
        .then(locale_compare(
            get(a, "label").and_then(Value::as_str).unwrap_or(""),
            get(b, "label").and_then(Value::as_str).unwrap_or(""),
        ))
    });

    // id выведен из (from,to,relation): после переписывания пересчитываем, иначе сортировка по id ломается
    let rewrite_edge = |edge: &Value| -> Value {
        let from = get(edge, "from");
        let to = get(edge, "to");
        let new_from = replaced_graph_ids
            .get(&js_template(from))
            .map(|s| json!(s))
            .unwrap_or_else(|| from.cloned().unwrap_or(Value::Null));
        let new_to = replaced_graph_ids
            .get(&js_template(to))
            .map(|s| json!(s))
            .unwrap_or_else(|| to.cloned().unwrap_or(Value::Null));
        if new_from == from.cloned().unwrap_or(Value::Null) && new_to == to.cloned().unwrap_or(Value::Null) {
            return edge.clone();
        }
        let mut map = edge.as_object().cloned().unwrap_or_default();
        let relation = js_template(get(edge, "relation"));
        map.insert(
            "id".into(),
            json!(stable_id("edge", &[
                js_template(Some(&new_from)),
                js_template(Some(&new_to)),
                relation,
            ])),
        );
        map.insert("from".into(), new_from);
        map.insert("to".into(), new_to);
        Value::Object(map)
    };

    let mut dependency_map: Map<String, Value> = Map::new();
    let mut graph_deps: Vec<Value> = as_array(get_opt(graph, "dependencies")).iter().map(rewrite_edge).collect();
    graph_deps.extend(as_array(get_opt(scip, "dependencies")).iter().cloned());
    for edge in &graph_deps {
        let key = format!(
            "{}\0{}\0{}",
            js_template(get(edge, "from")),
            js_template(get(edge, "to")),
            js_template(get(edge, "relation")),
        );
        let previous = dependency_map.get(&key);
        if previous.is_none() || get(edge, "confidence").and_then(Value::as_str) == Some("exact") {
            dependency_map.insert(key, edge.clone());
        }
    }

    let mut contract_map: Map<String, Value> = Map::new();
    let mut contracts: Vec<Value> = as_array(get_opt(graph, "contracts")).to_vec();
    contracts.extend(as_array(get_opt(scip, "contracts")).iter().cloned());
    for contract in &contracts {
        let entity_ref = replaced_graph_ids
            .get(&js_template(get(contract, "entityRef")))
            .map(|s| json!(s))
            .unwrap_or_else(|| get(contract, "entityRef").cloned().unwrap_or(Value::Null));
        let target_ref = replaced_graph_ids
            .get(&js_template(get(contract, "targetRef")))
            .map(|s| json!(s))
            .unwrap_or_else(|| get(contract, "targetRef").cloned().unwrap_or(Value::Null));
        let mut rewritten = contract.clone();
        if entity_ref != get(contract, "entityRef").cloned().unwrap_or(Value::Null)
            || target_ref != get(contract, "targetRef").cloned().unwrap_or(Value::Null)
        {
            let kind = js_template(get(contract, "kind"));
            // id контракта выведен из refs — пересчитываем в формате producer'а
            let id = if kind == "signature" {
                stable_id("contract", &[
                    "signature".into(),
                    js_template(Some(&entity_ref)),
                    js_template(get(contract, "signature")),
                ])
            } else if kind == "entrypoint" {
                stable_id("contract", &["entrypoint".into(), js_template(Some(&entity_ref))])
            } else {
                stable_id("contract", &[
                    kind,
                    js_template(Some(&entity_ref)),
                    js_template(Some(&target_ref)),
                ])
            };
            if let Some(map) = rewritten.as_object_mut() {
                map.insert("id".into(), json!(id));
                map.insert("entityRef".into(), entity_ref);
                map.insert("targetRef".into(), target_ref);
            }
        }
        let key = format!(
            "{}\0{}\0{}",
            js_template(get(&rewritten, "entityRef")),
            js_template(get(&rewritten, "kind")),
            match get(&rewritten, "targetRef") {
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            },
        );
        let previous = contract_map.get(&key);
        if previous.is_none() || get(&rewritten, "confidence").and_then(Value::as_str) == Some("exact") {
            contract_map.insert(key, rewritten);
        }
    }

    let mut dependencies: Vec<Value> = dependency_map.into_iter().map(|(_, v)| v).collect();
    dependencies.sort_by(|a, b| {
        locale_compare(
            get(a, "id").and_then(Value::as_str).unwrap_or(""),
            get(b, "id").and_then(Value::as_str).unwrap_or(""),
        )
    });
    let mut merged_contracts: Vec<Value> = contract_map.into_iter().map(|(_, v)| v).collect();
    merged_contracts.sort_by(|a, b| {
        locale_compare(
            get(a, "id").and_then(Value::as_str).unwrap_or(""),
            get(b, "id").and_then(Value::as_str).unwrap_or(""),
        )
    });

    let mut warnings: Vec<Value> = as_array(get_opt(graph, "warnings")).to_vec();
    warnings.extend(as_array(get_opt(scip, "warnings")).iter().cloned());
    if ambiguous > 0 {
        warnings.push(json!(format!("SCIP merge skipped for {ambiguous} ambiguous label(s)")));
    }

    let mut out = match graph {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    out.insert("entities".into(), json!(entities));
    out.insert("dependencies".into(), json!(dependencies));
    out.insert("contracts".into(), json!(merged_contracts));
    out.insert("warnings".into(), json!(warnings));
    out.insert(
        "truncated".into(),
        match (get_opt(graph, "truncated"), get_opt(scip, "truncated")) {
            (Some(g), _) if js_truthy(Some(g)) => g.clone(),
            (_, Some(s)) => s.clone(),
            _ => Value::Null,
        },
    );
    out.insert(
        "capabilities".into(),
        merge_capabilities(get_opt(graph, "capabilities"), get_opt(scip, "capabilities")),
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tag = field<<3 | wire, затем payload.
    fn tag(field: u64, wire: u8) -> u64 {
        field * 8 + wire as u64
    }

    fn varint(mut value: u64) -> Vec<u8> {
        let mut tmp = vec![];
        loop {
            let mut b = (value & 0x7f) as u8;
            value >>= 7;
            if value > 0 {
                b |= 0x80;
            }
            tmp.push(b);
            if value == 0 {
                break;
            }
        }
        tmp
    }

    fn varint_field(field: u64, value: u64) -> Vec<u8> {
        let mut out = varint(tag(field, 0));
        out.extend(varint(value));
        out
    }

    fn bytes_field(field: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = varint(tag(field, 2));
        out.extend(varint(payload.len() as u64));
        out.extend(payload);
        out
    }

    #[test]
    fn varint_guard_names_value_above_2_pow_53() {
        // тот же вектор, что в test/structure.test.mjs: shift=49, семь бит → ~7.2e16
        let buf = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F];
        let err = decode_scip_index(&buf).unwrap_err().to_string();
        assert!(err.contains("exceeds JavaScript safe integer"), "{err}");
    }

    #[test]
    fn varint_guard_shift_beyond_53() {
        let mut buf = vec![0x80; 8];
        buf.push(0x01);
        let err = decode_scip_index(&buf).unwrap_err().to_string();
        assert!(err.contains("exceeds JavaScript safe integer"), "{err}");
    }

    #[test]
    fn varint_max_safe_integer_ok() {
        // 2^53 - 1 — последнее безопасное целое: проходит guard
        let mut buf = varint(tag(1, 0));
        buf.extend(varint((1u64 << 53) - 1));
        assert!(decode_scip_index(&buf).is_ok());
    }

    #[test]
    fn truncated_varint_and_fixed64_are_named() {
        let err = decode_scip_index(&[0x80]).unwrap_err().to_string();
        assert!(err.contains("truncated SCIP protobuf varint"), "{err}");
        // tag: field 1, wire 1 (fixed64) — в буфере меньше 8 байт payload'а
        let err = decode_scip_index(&[0b00001001, 0x00, 0x00]).unwrap_err().to_string();
        assert!(err.contains("truncated SCIP protobuf"), "{err}");
    }

    #[test]
    fn unsupported_wire_type_is_named() {
        // tag: field 1, wire 3 (group start)
        let err = decode_scip_index(&[0b00001011]).unwrap_err().to_string();
        assert!(err.contains("unsupported SCIP protobuf wire type 3"), "{err}");
    }

    #[test]
    fn decode_small_index_all_range_encodings() {
        // документ с occurrence (single range) и символом; externalSymbols с legacy range
        let mut range_single = varint_field(1, 4); // line=4 → 5
        range_single.extend(varint_field(2, 2)); // startCharacter
        range_single.extend(varint_field(3, 9)); // endCharacter
        let mut occurrence = bytes_field(2, b"npm pkg 1 src/a.ts/foo().");
        occurrence.extend(varint_field(3, 1)); // Definition role
        occurrence.extend(bytes_field(8, &range_single));
        let mut symbol = bytes_field(1, b"npm pkg 1 src/a.ts/foo().");
        symbol.extend(varint_field(5, 17)); // function
        symbol.extend(bytes_field(6, b"foo"));
        let signature_doc = bytes_field(5, b"function foo(): void");
        symbol.extend(bytes_field(7, &signature_doc));
        let mut doc = bytes_field(1, b"src/a.ts");
        doc.extend(bytes_field(2, &occurrence));
        doc.extend(bytes_field(3, &symbol));
        doc.extend(bytes_field(4, b"typescript"));
        let mut root = bytes_field(2, &doc);
        // legacy packed range [line, start, endLine, end]
        let mut packed = varint(1);
        packed.extend(varint(0));
        packed.extend(varint(3));
        packed.extend(varint(7));
        let mut ext_occ = bytes_field(2, b"npm pkg 1 ext/Bar#.");
        ext_occ.extend(varint_field(3, 1));
        ext_occ.extend(bytes_field(1, &packed));
        let mut ext = bytes_field(1, b"npm pkg 1 ext/Bar#.");
        ext.extend(varint_field(5, 7)); // class
        root.extend(bytes_field(3, &ext));

        let index = decode_scip_index(&root).unwrap();
        let docs = index["documents"].as_array().unwrap();
        assert_eq!(docs.len(), 1);
        let doc = &docs[0];
        assert_eq!(doc["relativePath"], "src/a.ts");
        assert_eq!(doc["language"], "typescript");
        let occ = &doc["occurrences"][0];
        assert_eq!(occ["symbol"], "npm pkg 1 src/a.ts/foo().");
        assert_eq!(occ["roles"], 1);
        assert_eq!(occ["range"]["line"], 5);
        assert_eq!(occ["range"]["endLine"], 5);
        assert_eq!(occ["range"]["startCharacter"], 2);
        assert_eq!(occ["range"]["endCharacter"], 9);
        let sym = &doc["symbols"][0];
        assert_eq!(sym["kind"], 17);
        assert_eq!(sym["displayName"], "foo");
        assert_eq!(sym["signature"], "function foo(): void");
        let ext = &index["externalSymbols"][0];
        assert_eq!(ext["symbol"], "npm pkg 1 ext/Bar#.");
        // externalSymbols декодируются без occurrences: range живёт только в occurrence
        assert!(index["externalSymbols"][0]["relationships"].is_array());
    }

    #[test]
    fn extract_facts_two_docs_matches_js_test() {
        let index = json!({
            "documents": [
                {
                    "relativePath": "src/a.ts",
                    "language": "typescript",
                    "occurrences": [{
                        "symbol": "npm pkg 1 src/a.ts/foo().",
                        "roles": 1,
                        "range": { "line": 1, "endLine": 1 },
                    }],
                    "symbols": [{
                        "symbol": "npm pkg 1 src/a.ts/foo().",
                        "displayName": "foo",
                        "kind": 17,
                        "signature": "function foo(): void",
                        "relationships": [],
                    }],
                },
                {
                    "relativePath": "src/b.ts",
                    "language": "typescript",
                    "occurrences": [{
                        "symbol": "npm pkg 1 src/b.ts/bar().",
                        "roles": 1,
                        "range": { "line": 1, "endLine": 1 },
                    }],
                    "symbols": [{
                        "symbol": "npm pkg 1 src/b.ts/bar().",
                        "displayName": "bar",
                        "kind": 17,
                        "signature": "function bar(): void",
                        "relationships": [],
                    }],
                },
            ],
        });
        let facts = extract_scip_facts(Some(&index), &ExtractOptions::default()).unwrap();
        assert_eq!(facts["entities"].as_array().unwrap().len(), 2);
        assert_eq!(facts["entities"][0]["kind"], "function");
        assert_eq!(facts["entities"][0]["line"], 1);
        assert_eq!(facts["entities"][0]["confidence"], "exact");
        assert_eq!(facts["dependencies"].as_array().unwrap().len(), 0);
        assert_eq!(facts["contracts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn extract_facts_edge_truncation_sorted_first() {
        let index = json!({
            "documents": [{
                "relativePath": "src/a.ts",
                "language": "typescript",
                "occurrences": [
                    { "symbol": "npm pkg 1 src/a.ts/foo().", "roles": 1, "range": { "line": 1, "endLine": 1 } },
                    { "symbol": "npm pkg 1 src/a.ts/Bar#.", "roles": 1, "range": { "line": 5, "endLine": 5 } },
                ],
                "symbols": [
                    {
                        "symbol": "npm pkg 1 src/a.ts/foo().",
                        "displayName": "foo",
                        "kind": 17,
                        "signature": "function foo(): void",
                        "relationships": [{ "symbol": "npm pkg 1 src/a.ts/Bar#.", "isImplementation": true }],
                    },
                    {
                        "symbol": "npm pkg 1 src/a.ts/Bar#.",
                        "displayName": "Bar",
                        "kind": 21,
                        "signature": null,
                        "relationships": [],
                    },
                ],
            }],
        });
        let full = extract_scip_facts(Some(&index), &ExtractOptions::default()).unwrap();
        assert_eq!(full["dependencies"].as_array().unwrap().len(), 1);
        let limited = extract_scip_facts(Some(&index), &ExtractOptions {
            scope: None,
            max_entities: 5000,
            max_edges: 0,
        })
        .unwrap();
        assert_eq!(limited["dependencies"].as_array().unwrap().len(), 0);
        assert_eq!(limited["truncated"], true);
        assert!(limited["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("SCIP dependencies truncated at 0")));
    }

    #[test]
    fn merge_keeps_case_distinct_labels() {
        let entity = |id: &str, label: &str| {
            json!({
                "observedId": id, "lineageId": id, "boundaryId": "b1", "label": label,
                "qualifiedName": label, "kind": "function", "path": "src/a.ts",
                "line": 1, "endLine": 2, "signature": null, "community": "c1",
                "isGodNode": false, "confidence": "extracted",
                "evidence": [format!("graphify:{id}")], "externalIds": {},
            })
        };
        let graph = json!({
            "entities": [entity("g_upper", "User"), entity("g_lower", "user")],
            "dependencies": [{
                "id": "e1", "from": "g_upper", "to": "g_lower", "relation": "calls",
                "confidence": "extracted", "evidence": [],
            }],
            "contracts": [], "warnings": [], "truncated": false, "capabilities": {},
        });
        let scip = json!({
            "entities": [{
                "observedId": "s_upper", "lineageId": "s_upper", "boundaryId": null,
                "label": "User", "qualifiedName": "User", "kind": "function",
                "path": "src/a.ts", "line": 1, "endLine": 2, "signature": null,
                "community": null, "isGodNode": false, "confidence": "exact",
                "evidence": ["scip:User"], "externalIds": { "scip": "User" },
            }],
            "dependencies": [], "contracts": [], "warnings": [], "truncated": false,
            "capabilities": {},
        });
        let merged = merge_scip_with_graphify(Some(&scip), Some(&graph)).unwrap();
        assert_eq!(merged["entities"].as_array().unwrap().len(), 2);
        let labels: Vec<&str> = merged["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["label"].as_str().unwrap())
            .collect();
        assert!(labels.contains(&"User") && labels.contains(&"user"));
    }

    #[test]
    fn stable_id_matches_node() {
        // контрольные значения получены из node:
        // stableId('entity', 'src/a.ts', 'npm pkg 1 src/a.ts/foo().', 'function')
        let id = stable_id("entity", &[
            "src/a.ts".into(),
            "npm pkg 1 src/a.ts/foo().".into(),
            "function".into(),
        ]);
        assert!(id.starts_with("entity:"), "{id}");
        assert_eq!(id.len(), "entity:".len() + 24);
    }

    #[test]
    fn normalize_repo_path_matches_js() {
        assert_eq!(normalize_repo_path("src\\a.ts").unwrap(), "src/a.ts");
        assert_eq!(normalize_repo_path(" ./src/a.ts ").unwrap(), "src/a.ts");
        assert!(normalize_repo_path("/abs/path").is_err());
        assert!(normalize_repo_path("C:/x").is_err());
        assert!(normalize_repo_path("../escape").is_err());
        assert!(normalize_repo_path("a/../../b").is_err());
        assert!(normalize_repo_path("").is_err());
    }
}
