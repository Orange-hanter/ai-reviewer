//! Порт lib/structure-lcov.mjs: парсер LCOV (SF/DA/FN/FNDA/BRDA), наблюдения
//! покрытия и эвристика сопоставления LCOV-функций сущностям структуры.
//!
//! Паритет с JS сохранён, включая:
//! - Number()/Number.isInteger/Number.isFinite для полей записей: js_string_to_number
//!   принимает ' 5', '1e2', '0x10', '.5' как JS и отвергает 'abc' (NaN);
//! - порядковую привязку FNDA к одноимённым FN (attribution 'order'), множественные
//!   объявления с разошедшимся числом записей → hits null + warning;
//! - node path.resolve/relative для абсолютных SF при заданном repoRoot
//!   (лексический разбор, cwd для относительного repoRoot);
//! - хиты и taken — f64 (JS Number), сериализация целых как i64 (7, не 7.0);
//! - сортировки localeCompare (аппроксимация ICU root из structure_scip),
//!   дефолтный Array.sort — побайтовый;
//! - stableId = `prefix:` + sha256(parts.join('\0'))[..24], null-части → ''.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use crate::findings::{js_is_whitespace, js_string_to_number, js_to_string, js_truthy};
use crate::structure_scip::{locale_compare, normalize_repo_path, sha256_hex, stable_id};

/// JSON-число: целые f64 сериализуем как i64 (JS видит 7, не 7.0); NaN/Infinity → null
/// (JSON.stringify(NaN) === 'null'). Отклонение от JS — только для |n| ≥ 9e15,
/// где serde печатает экспоненту ('1e16') вместо развёрнутой записи.
fn num_json(n: f64) -> Value {
    if n.is_nan() || n.is_infinite() {
        Value::Null
    } else if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// ratio(covered, total) из JS: total > 0 ? covered / total : null.
fn ratio(covered: usize, total: usize) -> Value {
    if total > 0 {
        num_json(covered as f64 / total as f64)
    } else {
        Value::Null
    }
}

/// String(v || '') из JS (объекты → '[object Object]', как в JS).
fn string_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// node path: resolve / relative (лексический posix-разбор)
// ---------------------------------------------------------------------------

/// node path.posix.normalize для абсолютных путей (resolve без cwd).
fn posix_normalize_absolute(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                }
            }
            c => parts.push(c),
        }
    }
    format!("/{}", parts.join("/"))
}

/// node path.resolve для одного аргумента: абсолютный нормализуется, относительный
/// клеится к cwd (как у JS-процесса, вызвавшего обёртку).
fn js_resolve(path: &str) -> String {
    let raw: String = path.trim_matches(js_is_whitespace).to_string();
    if raw.starts_with('/') {
        posix_normalize_absolute(&raw)
    } else {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        posix_normalize_absolute(&format!("{cwd}/{raw}"))
    }
}

/// node path.relative(from, to): обе стороны resolve'нутся, результат '../'-цепочка
/// + хвост to. '' при равенстве.
fn posix_relative(from: &str, to: &str) -> String {
    let from = js_resolve(from);
    let to = js_resolve(to);
    if from == to {
        return String::new();
    }
    // trimArray: срезаем пустые сегменты после split('/')
    let from_parts: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let to_parts: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let mut common = 0;
    while common < from_parts.len()
        && common < to_parts.len()
        && from_parts[common] == to_parts[common]
    {
        common += 1;
    }
    let mut out: Vec<&str> = Vec::new();
    for _ in common..from_parts.len() {
        out.push("..");
    }
    out.extend(&to_parts[common..]);
    out.join("/")
}

/// normalizeLcovPath из JS: относительный путь — через normalizeRepoPath,
/// абсолютный — требует repoRoot и режется до пути относительно него.
fn normalize_lcov_path(value: &str, repo_root: &str) -> Result<String> {
    let raw = value.trim_matches(js_is_whitespace);
    if raw.is_empty() {
        bail!("LCOV SF path is empty");
    }
    if raw.starts_with('/') {
        if repo_root.is_empty() {
            bail!("absolute LCOV SF path requires explicit repoRoot");
        }
        let rel = posix_relative(repo_root, raw);
        return normalize_repo_path(&rel);
    }
    normalize_repo_path(raw)
}

// ---------------------------------------------------------------------------
// parseLcov
// ---------------------------------------------------------------------------

struct RawRecord {
    path: String,
    /// Map<line, hits> — вставка с перезаписью (дубликат DA перекрывает).
    lines: Vec<(i64, f64)>,
    functions: Vec<(f64, String)>,
    function_hits: Vec<(String, f64)>,
    branches: Vec<RawBranch>,
}

struct RawBranch {
    line: i64,
    block: String,
    branch: String,
    taken: f64,
}

impl RawRecord {
    fn set_line(&mut self, line: i64, hits: f64) {
        if let Some(entry) = self.lines.iter_mut().find(|(l, _)| *l == line) {
            entry.1 = hits;
        } else {
            self.lines.push((line, hits));
        }
    }
}

/// Number.isInteger(Number(s)) для значений, влезающих в i64 (строки LCOV малы).
fn is_integer_str(s: &str) -> Option<f64> {
    let n = js_string_to_number(s)?;
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9.0e15 {
        Some(n)
    } else {
        None
    }
}

/// finishRecord из JS: привязка FNDA к FN (по имени или по порядку записей),
/// сортировки, сводки по строкам/функциям/ветвям.
fn finish_record(record: RawRecord) -> Value {
    let mut hits_by_name: HashMap<&str, Vec<f64>> = HashMap::new();
    for (name, hits) in &record.function_hits {
        hits_by_name.entry(name.as_str()).or_default().push(*hits);
    }
    let mut declared_by_name: HashMap<&str, usize> = HashMap::new();
    for (_, name) in &record.functions {
        *declared_by_name.entry(name.as_str()).or_default() += 1;
    }
    let mut ambiguous_function_names: Vec<String> = declared_by_name
        .iter()
        .filter(|(name, count)| {
            **count > 1 && hits_by_name.get(*name).map(|v| v.len()).unwrap_or(0) != **count
        })
        .map(|(name, _)| name.to_string())
        .collect();
    // дефолтный Array.sort — побайтовый порядок
    ambiguous_function_names.sort();
    let unresolvable: HashSet<&str> = ambiguous_function_names.iter().map(|s| s.as_str()).collect();

    let mut seen_by_name: HashMap<&str, usize> = HashMap::new();
    let mut functions: Vec<Value> = Vec::new();
    for (line, name) in &record.functions {
        let ordinal = {
            let entry = seen_by_name.entry(name.as_str()).or_default();
            let o = *entry;
            *entry += 1;
            o
        };
        let duplicated = declared_by_name.get(name.as_str()).copied().unwrap_or(0) > 1;
        let (hits, attribution) = if unresolvable.contains(name.as_str()) {
            (Value::Null, "unresolved")
        } else if duplicated {
            (
                num_json(hits_by_name.get(name.as_str()).map(|v| v[ordinal]).unwrap_or(0.0)),
                "order",
            )
        } else {
            (
                num_json(
                    hits_by_name
                        .get(name.as_str())
                        .and_then(|v| v.first())
                        .copied()
                        .unwrap_or(0.0),
                ),
                "name",
            )
        };
        let mut entry = Map::new();
        entry.insert(
            "id".into(),
            json!(stable_id("lcov-fn", &[record.path.clone(), sid_num(*line), name.clone()])),
        );
        entry.insert("name".into(), json!(name));
        entry.insert("line".into(), num_json(*line));
        entry.insert("hits".into(), hits);
        entry.insert("attribution".into(), json!(attribution));
        functions.push(Value::Object(entry));
    }
    functions.sort_by(|a, b| {
        let line_a = a.get("line").and_then(Value::as_f64).unwrap_or(0.0);
        let line_b = b.get("line").and_then(Value::as_f64).unwrap_or(0.0);
        line_a
            .partial_cmp(&line_b)
            .unwrap_or(Ordering::Equal)
            .then(locale_compare(
                a.get("name").and_then(Value::as_str).unwrap_or(""),
                b.get("name").and_then(Value::as_str).unwrap_or(""),
            ))
    });

    let mut line_entries = record.lines;
    line_entries.sort_by_key(|(line, _)| *line);
    let mut branch_entries = record.branches;
    branch_entries.sort_by(|a, b| {
        a.line
            .cmp(&b.line)
            .then(locale_compare(&a.block, &b.block))
            .then(locale_compare(&a.branch, &b.branch))
    });
    let line_hit = line_entries.iter().filter(|(_, hits)| *hits > 0.0).count();
    let branch_hit = branch_entries.iter().filter(|b| b.taken > 0.0).count();

    let function_hit = functions
        .iter()
        .filter(|f| f.get("hits").and_then(Value::as_f64).is_some_and(|h| h > 0.0))
        .count();
    let function_known = functions
        .iter()
        .filter(|f| !matches!(f.get("hits"), None | Some(Value::Null)))
        .count();

    let mut lines_map = Map::new();
    lines_map.insert("found".into(), json!(line_entries.len()));
    lines_map.insert("hit".into(), json!(line_hit));
    lines_map.insert("ratio".into(), ratio(line_hit, line_entries.len()));
    lines_map.insert(
        "entries".into(),
        json!(line_entries
            .iter()
            .map(|(line, hits)| json!({ "line": line, "hits": num_json(*hits) }))
            .collect::<Vec<_>>()),
    );

    let mut functions_map = Map::new();
    functions_map.insert("found".into(), json!(functions.len()));
    functions_map.insert("hit".into(), json!(function_hit));
    functions_map.insert("ratio".into(), ratio(function_hit, function_known));
    functions_map.insert("entries".into(), json!(functions));

    let mut branches_map = Map::new();
    branches_map.insert("found".into(), json!(branch_entries.len()));
    branches_map.insert("hit".into(), json!(branch_hit));
    branches_map.insert("ratio".into(), ratio(branch_hit, branch_entries.len()));
    branches_map.insert(
        "entries".into(),
        json!(branch_entries
            .iter()
            .map(|b| json!({
                "line": b.line,
                "block": b.block,
                "branch": b.branch,
                "taken": num_json(b.taken),
            }))
            .collect::<Vec<_>>()),
    );

    let mut out = Map::new();
    out.insert("path".into(), json!(record.path));
    out.insert("ambiguousFunctionNames".into(), json!(ambiguous_function_names));
    out.insert("lines".into(), Value::Object(lines_map));
    out.insert("functions".into(), Value::Object(functions_map));
    out.insert("branches".into(), Value::Object(branches_map));
    Value::Object(out)
}

/// parseLcov из lib/structure-lcov.mjs. Возвращает {hash, files, warnings, totals};
/// byPath — Map и в JSON не сериализуется: JS-обёртка пересобирает его из files.
pub fn parse_lcov(text: &str, repo_root: &str) -> Result<Map<String, Value>> {
    let mut records: Vec<Value> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut current: Option<RawRecord> = None;

    for raw_line in text.split('\n') {
        let line = raw_line.trim_matches(js_is_whitespace);
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("SF:") {
            if let Some(record) = current.take() {
                records.push(finish_record(record));
            }
            match normalize_lcov_path(rest, repo_root) {
                Ok(path) => {
                    current = Some(RawRecord {
                        path,
                        lines: Vec::new(),
                        functions: Vec::new(),
                        function_hits: Vec::new(),
                        branches: Vec::new(),
                    });
                }
                Err(error) => {
                    warnings.push(format!("{error:#}"));
                    current = None;
                }
            }
            continue;
        }
        if line == "end_of_record" {
            if let Some(record) = current.take() {
                records.push(finish_record(record));
            }
            continue;
        }
        let Some(record) = current.as_mut() else {
            continue;
        };
        if let Some(_rest) = line.strip_prefix("DA:") {
            // пустые поля (`DA:,0`, `DA:5,`) — мусор, а не строка 0 с 0 хитов
            let mut parts = line[3..].split(',');
            let line_no = parts.next().unwrap_or("");
            let hits = parts.next();
            if !line_no.is_empty() && !hits.is_none() && !hits.unwrap().is_empty() {
                if let Some(line_num) = is_integer_str(line_no) {
                    if let Some(hits_num) = js_string_to_number(hits.unwrap()) {
                        if hits_num.is_finite() {
                            record.set_line(line_num as i64, hits_num);
                        }
                    }
                }
            }
        } else if line.starts_with("FN:") {
            let Some(comma) = line.find(',') else { continue };
            let line_no = js_string_to_number(&line[3..comma]);
            let name = line[comma + 1..].trim_matches(js_is_whitespace);
            if comma > 3 && !name.is_empty() {
                if let Some(line_no) = line_no {
                    if line_no.is_finite() && line_no.fract() == 0.0 {
                        record.functions.push((line_no, name.to_string()));
                    }
                }
            }
        } else if line.starts_with("FNDA:") {
            let Some(comma) = line.find(',') else { continue };
            let hits = js_string_to_number(&line[5..comma]);
            let name = line[comma + 1..].trim_matches(js_is_whitespace);
            if comma > 5 && !name.is_empty() {
                if let Some(hits) = hits {
                    if hits.is_finite() {
                        record.function_hits.push((name.to_string(), hits));
                    }
                }
            }
        } else if line.starts_with("BRDA:") {
            let parts: Vec<&str> = line[5..].split(',').collect();
            let line_no = parts.first().copied().unwrap_or("");
            let block = parts.get(1).copied().unwrap_or("");
            let branch = parts.get(2).copied().unwrap_or("");
            let taken_raw = parts.get(3).copied().unwrap_or("");
            if line_no.is_empty() || is_integer_str(line_no).is_none() {
                continue;
            }
            let taken = if taken_raw == "-" {
                0.0
            } else {
                js_string_to_number(taken_raw).unwrap_or(f64::NAN)
            };
            if !taken_raw.is_empty() && taken.is_finite() {
                record.branches.push(RawBranch {
                    line: is_integer_str(line_no).unwrap() as i64,
                    block: block.to_string(),
                    branch: branch.to_string(),
                    taken,
                });
            }
        }
    }
    if let Some(record) = current.take() {
        records.push(finish_record(record));
    }

    records.sort_by(|a, b| {
        locale_compare(
            a.get("path").and_then(Value::as_str).unwrap_or(""),
            b.get("path").and_then(Value::as_str).unwrap_or(""),
        )
    });
    let mut duplicate_paths: Vec<String> = Vec::new();
    let mut seen_paths: HashSet<&str> = HashSet::new();
    for file in &records {
        let path = file.get("path").and_then(Value::as_str).unwrap_or("");
        if !seen_paths.insert(path) {
            duplicate_paths.push(path.to_string());
        }
    }
    if !duplicate_paths.is_empty() {
        let mut unique: Vec<String> = Vec::new();
        for path in duplicate_paths {
            if !unique.contains(&path) {
                unique.push(path);
            }
        }
        warnings.push(format!("duplicate LCOV paths: {}", unique.join(", ")));
    }
    for file in &records {
        let ambiguous = file
            .get("ambiguousFunctionNames")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !ambiguous.is_empty() {
            let path = file.get("path").and_then(Value::as_str).unwrap_or("");
            let names: Vec<String> = ambiguous
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            warnings.push(format!(
                "unresolvable LCOV function names in {path}: {}",
                names.join(", ")
            ));
        }
    }

    let sum = |key: &str, sub: &str| -> usize {
        records
            .iter()
            .filter_map(|f| f.get(key))
            .filter_map(|m| m.get(sub))
            .filter_map(Value::as_u64)
            .sum::<u64>() as usize
    };

    let mut totals = Map::new();
    totals.insert("files".into(), json!(records.len()));
    totals.insert(
        "lines".into(),
        json!({ "found": sum("lines", "found"), "hit": sum("lines", "hit") }),
    );
    totals.insert(
        "functions".into(),
        json!({ "found": sum("functions", "found"), "hit": sum("functions", "hit") }),
    );
    totals.insert(
        "branches".into(),
        json!({ "found": sum("branches", "found"), "hit": sum("branches", "hit") }),
    );

    let mut out = Map::new();
    out.insert("hash".into(), json!(sha256_hex(text.as_bytes())));
    out.insert("files".into(), json!(records));
    out.insert("warnings".into(), json!(warnings));
    out.insert("totals".into(), Value::Object(totals));
    Ok(out)
}

/// Строковая форма числа для stableId (String(10) === '10').
fn sid_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        format!("{}", n as i64)
    } else {
        js_to_string(&num_json(n))
    }
}

// ---------------------------------------------------------------------------
// lcovObservations + matchLcovFunction
// ---------------------------------------------------------------------------

/// normalizedName из JS: хвост после последнего .#/ (один проход ^.*[.#/]),
/// срез хвостовых "()" (один проход /\(\)+$/), одна пара кавычек по краям, lowercase.
fn normalized_name(value: &str) -> String {
    let mut s = value;
    if let Some(pos) = s.rfind(|c| c == '.' || c == '#' || c == '/') {
        s = &s[pos + 1..];
    }
    while let Some(stripped) = s.strip_suffix("()") {
        s = stripped;
    }
    let mut chars: Vec<char> = s.chars().collect();
    if chars.first().is_some_and(|c| *c == '"' || *c == '\'') {
        chars.remove(0);
    }
    if chars.last().is_some_and(|c| *c == '"' || *c == '\'') {
        chars.pop();
    }
    chars.into_iter().collect::<String>().to_lowercase()
}

/// matchLcovFunction: exact (имя+строка) → extracted (единственная callable на строке)
/// → inferred (имя и строка в ±3) → ambiguous/unresolved.
pub fn match_lcov_function(fn_line: Option<f64>, fn_name: &str, entities: &[Value]) -> Value {
    let callable: Vec<&Value> = entities
        .iter()
        .filter(|e| {
            matches!(
                e.get("kind").and_then(Value::as_str),
                Some("function") | Some("method") | Some("constructor")
            )
        })
        .collect();
    let exact_line: Vec<&Value> = callable
        .iter()
        .copied()
        .filter(|e| e.get("line").and_then(Value::as_f64) == fn_line)
        .collect();
    let name = normalized_name(fn_name);
    let exact: Vec<&Value> = exact_line
        .iter()
        .copied()
        .filter(|e| normalized_name(&string_or_empty(e.get("label"))) == name)
        .collect();
    if exact.len() == 1 {
        return json!({ "entity": exact[0], "confidence": "exact" });
    }
    if exact_line.len() == 1 {
        return json!({ "entity": exact_line[0], "confidence": "extracted" });
    }
    let near: Vec<&Value> = callable
        .iter()
        .copied()
        .filter(|e| {
            let line = e.get("line").and_then(Value::as_f64).unwrap_or(0.0);
            (line - fn_line.unwrap_or(0.0)).abs() <= 3.0
                && normalized_name(&string_or_empty(e.get("label"))) == name
        })
        .collect();
    if near.len() == 1 {
        return json!({ "entity": near[0], "confidence": "inferred" });
    }
    let confidence = if exact.len() > 1 || near.len() > 1 {
        "ambiguous"
    } else {
        "unresolved"
    };
    json!({ "entity": Value::Null, "confidence": confidence })
}

/// lcovObservations: по файлу — code_coverage, по функциям — function_execution.
/// entity_entries — содержимое entityByPath Map (путь → [сущности]) в порядке вставки.
pub fn lcov_observations(lcov: Option<&Value>, entity_entries: &[(String, Value)]) -> Vec<Value> {
    let mut entities_by_path: HashMap<&str, &[Value]> = HashMap::new();
    for (path, entities) in entity_entries {
        if let Value::Array(list) = entities {
            entities_by_path.insert(path.as_str(), list.as_slice());
        }
    }
    let mut observations = Vec::new();
    let files = lcov
        .and_then(|l| l.get("files"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for file in &files {
        let path = file.get("path").and_then(Value::as_str).unwrap_or("");
        let line_hit = file.pointer("/lines/hit").and_then(Value::as_u64).unwrap_or(0);
        let line_found = file.pointer("/lines/found").and_then(Value::as_u64).unwrap_or(0);
        let status = if line_hit > 0 {
            if line_hit == line_found {
                "covered"
            } else {
                "partial"
            }
        } else {
            "uncovered"
        };
        let mut entry = Map::new();
        entry.insert(
            "id".into(),
            json!(stable_id("coverage", &["file".into(), path.into()])),
        );
        entry.insert("level".into(), json!("code_coverage"));
        entry.insert("subjectRef".into(), json!(format!("file:{path}")));
        entry.insert("status".into(), json!(status));
        entry.insert("hitCount".into(), Value::Null);
        entry.insert("covered".into(), json!(line_hit));
        entry.insert("total".into(), json!(line_found));
        entry.insert("confidence".into(), json!("exact"));
        entry.insert("evidence".into(), json!([format!("lcov:{path}:DA")]));
        observations.push(Value::Object(entry));

        let entities: &[Value] = entities_by_path.get(path).copied().unwrap_or(&[]);
        let functions = file
            .pointer("/functions/entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for f in &functions {
            let fn_line = f.get("line").and_then(Value::as_f64);
            let fn_name = f.get("name").and_then(Value::as_str).unwrap_or("");
            let matched = match_lcov_function(fn_line, fn_name, entities);
            let matched_conf = matched
                .get("confidence")
                .and_then(Value::as_str)
                .unwrap_or("unresolved");
            let matched_observed = matched.get("entity").and_then(|e| e.get("observedId"));
            let hits = f.get("hits").and_then(Value::as_f64);
            let unknown_hits = hits.is_none();
            let attribution = f.get("attribution").and_then(Value::as_str).unwrap_or("");
            let confidence = if unknown_hits {
                "ambiguous"
            } else if attribution == "order" && matched_conf == "exact" {
                "extracted"
            } else {
                matched_conf
            };
            let fallback_ref = format!("lcov-fn:{path}:{}:{fn_name}", sid_num(fn_line.unwrap_or(0.0)));
            let subject_ref = match matched_observed {
                Some(v) if js_truthy(v) => js_to_string(v),
                _ => fallback_ref,
            };
            let mut entry = Map::new();
            entry.insert(
                "id".into(),
                json!(stable_id("coverage", &[
                    "function".into(),
                    path.into(),
                    sid_num(fn_line.unwrap_or(0.0)),
                    fn_name.into(),
                ])),
            );
            entry.insert("level".into(), json!("function_execution"));
            entry.insert("subjectRef".into(), json!(subject_ref));
            entry.insert(
                "status".into(),
                json!(if unknown_hits {
                    "unresolved"
                } else if hits.unwrap_or(0.0) > 0.0 {
                    "covered"
                } else {
                    "uncovered"
                }),
            );
            entry.insert(
                "hitCount".into(),
                if unknown_hits {
                    Value::Null
                } else {
                    num_json(hits.unwrap())
                },
            );
            entry.insert("covered".into(), Value::Null);
            entry.insert("total".into(), Value::Null);
            entry.insert("confidence".into(), json!(confidence));
            entry.insert(
                "evidence".into(),
                json!([format!("lcov:{path}:FN:{}:{fn_name}", sid_num(fn_line.unwrap_or(0.0)))]),
            );
            observations.push(Value::Object(entry));
        }
    }
    observations.sort_by(|a, b| {
        locale_compare(
            a.get("id").and_then(Value::as_str).unwrap_or(""),
            b.get("id").and_then(Value::as_str).unwrap_or(""),
        )
    });
    observations
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_files_with_totals() {
        let text = "TN:\nSF:src/auth.ts\nFN:10,validate\nFNDA:2,validate\nDA:10,2\nDA:11,0\nBRDA:11,0,0,0\nend_of_record\nSF:src/login.ts\nFN:4,login\nFNDA:1,login\nDA:4,1\nend_of_record\n";
        let parsed = parse_lcov(text, "/repo").unwrap();
        assert_eq!(parsed["totals"]["lines"]["found"], json!(3));
        assert_eq!(parsed["totals"]["lines"]["hit"], json!(2));
        assert_eq!(parsed["totals"]["functions"]["found"], json!(2));
        let files = parsed["files"].as_array().unwrap();
        let auth = files.iter().find(|f| f["path"] == "src/auth.ts").unwrap();
        assert_eq!(auth["functions"]["entries"][0]["hits"], json!(2));
        assert_eq!(auth["branches"]["hit"], json!(0));
    }

    #[test]
    fn same_named_functions_get_order_attribution() {
        // вектор из test/structure.test.mjs: N-я FNDA → N-я одноимённая FN
        let text = "SF:src/a.ts\nFN:10,handler\nFN:50,handler\nFNDA:7,handler\nFNDA:0,handler\nDA:10,7\nDA:50,0\nend_of_record\n";
        let parsed = parse_lcov(text, "").unwrap();
        let entries = parsed["files"][0]["functions"]["entries"].as_array().unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|f| (f["line"].clone(), f["hits"].clone(), f["attribution"].as_str().unwrap()))
                .collect::<Vec<_>>(),
            vec![(json!(10), json!(7), "order"), (json!(50), json!(0), "order")]
        );
        assert_eq!(parsed["files"][0]["functions"]["hit"], json!(1));
        assert_eq!(parsed["warnings"], json!([]));
    }

    #[test]
    fn diverged_fn_fnda_counts_leave_execution_unknown() {
        let text = "SF:src/a.ts\nFN:10,handler\nFN:50,handler\nFNDA:7,handler\nDA:10,7\nDA:50,0\nend_of_record\n";
        let parsed = parse_lcov(text, "").unwrap();
        let entries = parsed["files"][0]["functions"]["entries"].as_array().unwrap();
        assert!(entries.iter().all(|f| f["hits"].is_null()));
        assert_eq!(parsed["files"][0]["functions"]["hit"], json!(0));
        assert_eq!(parsed["files"][0]["functions"]["ratio"], Value::Null);
        assert!(parsed["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("unresolvable LCOV function names")));
    }

    #[test]
    fn absolute_sf_requires_repo_root() {
        let text = "TN:\nSF:/repo/src/x.ts\nDA:1,1\nend_of_record\n";
        let parsed = parse_lcov(text, "").unwrap();
        assert_eq!(parsed["files"].as_array().unwrap().len(), 0);
        assert!(parsed["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("repoRoot")));
        let rooted = parse_lcov(text, "/repo").unwrap();
        assert_eq!(rooted["files"][0]["path"], json!("src/x.ts"));
    }

    #[test]
    fn empty_da_brda_fields_are_rejected() {
        let parsed = parse_lcov("TN:\nSF:src/x.ts\nDA:,5\nDA:3,\nDA:4,2\nBRDA:4,0,0,\nBRDA:,0,0,1\nend_of_record\n", "").unwrap();
        let file = &parsed["files"][0];
        assert_eq!(file["lines"]["entries"], json!([{ "line": 4, "hits": 2 }]));
        assert_eq!(file["branches"]["found"], json!(0));
    }

    #[test]
    fn match_lcov_function_exact_extracted_inferred() {
        let entities = vec![
            json!({"kind": "constructor", "label": "Widget", "line": 3, "observedId": "ctor"}),
            json!({"kind": "function", "label": "obj.run", "line": 12, "observedId": "e1"}),
        ];
        let exact = match_lcov_function(Some(3.0), "Widget", &entities);
        assert_eq!(exact["entity"]["observedId"], json!("ctor"));
        assert_eq!(exact["confidence"], json!("exact"));
        let extracted = match_lcov_function(Some(12.0), "renamed", &entities);
        assert_eq!(extracted["confidence"], json!("extracted"));
        let inferred = match_lcov_function(Some(13.0), "run", &entities);
        assert_eq!(inferred["entity"]["observedId"], json!("e1"));
        assert_eq!(inferred["confidence"], json!("inferred"));
    }

    #[test]
    fn observations_report_unresolved_for_unknown_hits() {
        let text = "SF:src/a.ts\nFN:10,handler\nFN:50,handler\nFNDA:7,handler\nDA:10,7\nDA:50,0\nend_of_record\n";
        let parsed = parse_lcov(text, "").unwrap();
        let observations = lcov_observations(Some(&Value::Object(parsed)), &[]);
        let functions: Vec<&Value> = observations
            .iter()
            .filter(|o| o["level"] == "function_execution")
            .collect();
        assert_eq!(functions.len(), 2);
        for item in functions {
            assert_eq!(item["status"], json!("unresolved"));
            assert_eq!(item["hitCount"], Value::Null);
            assert_eq!(item["confidence"], json!("ambiguous"));
        }
    }
}
