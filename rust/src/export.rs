//! Порт lib/export.mjs (Фаза 3, шаг 6): машиночитаемый экспорт прогона —
//! JSON-артефакт (buildReviewJson) и SARIF 2.1.0 для GitHub Code Scanning
//! (buildSarif). Потребители: pipeline.mjs (writeReviewExports), тесты,
//! upload-sarif в CI.
//!
//! Паритет с JS (проверяется rust/parity/export.parity.mjs):
//!   - SARIF — внешний контракт для GitHub code scanning: сравнение
//!     ПОБАЙТОВОЕ по JSON.stringify(sarif, null, 2), т.е. важны порядок
//!     ключей (serde preserve_order, объекты собираются в порядке JS-литералов),
//!     опускание undefined-полей и семантика обрезок строк;
//!   - обрезки идут по UTF-16 code units (js_slice_utf16 из crate::report —
//!     тот же примитив, что в рендере sticky);
//!   - часы НЕ портированы: generatedAt (top-level и в swarm-вердикте)
//!     приходит аргументом из JS-обёртки (fs/часы живут на JS-стороне);
//!   - buildSwarmVerdict (lib/swarm.mjs) портирован сюда же — нужен только
//!     внутри buildReviewJson; E23 suggestedScopeFiles сортируется в порядке
//!     UTF-16 code units, как дефолтный Array#sort в V8;
//!   - sarifAutomationGuid — sha256(`${tool}:${seed}`) с RFC-4122 variant;
//!   - encodeURIComponent для artifactLocation.uri: алфавит
//!     A-Za-z0-9 - _ . ! ~ * ' ( ) без экранирования, остальное — %XX
//!     по UTF-8 байтам, hex в верхнем регистре.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::findings::{js_to_number, js_to_string, js_truthy};
use crate::report::{extract_reasoning_entries, js_slice_utf16, utf16_len};
use crate::structure_evidence::sha256_str;

pub const EXPORT_VERSION: i64 = 2;
pub const SARIF_SCHEMA: &str = "https://json.schemastore.org/sarif-2.1.0.json";
pub const TOOL_VERSION: &str = "1.0";

pub const BODY_EXPORT_MAX: usize = 400;
pub const REASONING_EXPORT_MAX: usize = 600;
pub const SARIF_MESSAGE_MAX: usize = 500;
pub const TITLE_MAX: usize = 200;

pub const SEVERITIES: [&str; 4] = ["critical", "major", "minor", "nit"];

/// SEV_TO_SARIF_LEVEL из lib/export.mjs.
pub fn sev_to_sarif_level(sev: &str) -> &'static str {
    match sev {
        "critical" => "error",
        "major" => "warning",
        "minor" | "nit" => "note",
        _ => "note",
    }
}

/// Stable RFC-4122-shaped GUID для SARIF automationDetails (Code Scanning
/// требует UUID): sha256(`${tool}:${seed}`), version 4 + variant из хеша.
pub fn sarif_automation_guid(seed: &str, tool: &str) -> String {
    let h = sha256_str(&format!("{tool}:{seed}"));
    let variant = ['8', '9', 'a', 'b'][usize::from_str_radix(&h[16..17], 16).unwrap_or(0) % 4];
    format!(
        "{}-{}-4{}-{}{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        variant,
        &h[17..20],
        &h[20..32],
    )
}

fn encode_uri_component(seg: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
    let mut out = String::new();
    for &b in seg.as_bytes() {
        if UNRESERVED.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-encode path segments для SARIF artifactLocation.uri.
pub fn sarif_artifact_uri(file: Option<&Value>) -> String {
    let raw = match file {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Null) | None => String::new(),
        Some(other) => js_to_string(other).trim().to_string(),
    };
    if raw.is_empty() {
        return "unknown".to_string();
    }
    raw.split('/').map(encode_uri_component).collect::<Vec<_>>().join("/")
}

/// JS-объект с undefined-полями: в JSON они выпадают. Вставляем ключ только
/// если значение "не undefined" (None = undefined).
fn insert_if_defined(m: &mut serde_json::Map<String, Value>, key: &str, v: Option<Value>) {
    if let Some(v) = v {
        m.insert(key.to_string(), v);
    }
}

/// `v || undefined` — falsy (null/0/""/false/NaN/отсутствующий) → поле выпадает.
fn truthy_or_undef(v: Option<&Value>) -> Option<Value> {
    match v {
        Some(x) if js_truthy(x) => Some(x.clone()),
        _ => None,
    }
}

/// Redacted finding для артефакта (body урезан — как R7 observability).
/// Порядок ключей — как в JS-литерале slimFinding.
pub fn slim_finding(f: &Value) -> Value {
    let obj = match f {
        Value::Object(o) => o,
        _ => return Value::Null,
    };
    let mut m = serde_json::Map::new();
    // file: f.file || '' — falsy → ''; иначе как есть.
    m.insert("file".into(), match obj.get("file") {
        Some(x) if js_truthy(x) => x.clone(),
        _ => Value::String(String::new()),
    });
    // line: f.line || 0
    m.insert("line".into(), match obj.get("line") {
        Some(x) if js_truthy(x) => x.clone(),
        _ => json!(0),
    });
    insert_if_defined(&mut m, "severity", obj.get("severity").cloned());
    let title = match obj.get("title") {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    };
    m.insert("title".into(), Value::String(js_slice_utf16(&title, TITLE_MAX)));
    let body = match obj.get("body") {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    };
    insert_if_defined(&mut m, "body", if body.is_empty() {
        None
    } else {
        Some(Value::String(js_slice_utf16(&body, BODY_EXPORT_MAX)))
    });
    m.insert("bodyTruncated".into(), Value::Bool(utf16_len(&body) > BODY_EXPORT_MAX));
    m.insert("model".into(), match obj.get("model") {
        Some(x) if js_truthy(x) => x.clone(),
        _ => Value::String(String::new()),
    });
    insert_if_defined(&mut m, "confidence", obj.get("confidence").cloned());
    m.insert("consensusBlock".into(), match truthy_or_undef(obj.get("consensusBlock")) {
        Some(v) => v,
        None => Value::Bool(false),
    });
    insert_if_defined(&mut m, "prescan", truthy_or_undef(obj.get("prescan")));
    if let Some(Value::Array(claims)) = obj.get("advisorClaims") {
        let mapped: Vec<Value> = claims
            .iter()
            .map(|c| {
                let cm = match c {
                    Value::Object(o) => o,
                    _ => return Value::Null,
                };
                let mut e = serde_json::Map::new();
                insert_if_defined(&mut e, "role", cm.get("role").cloned());
                insert_if_defined(&mut e, "status", cm.get("status").cloned());
                let reason = match cm.get("reason") {
                    Some(x) if js_truthy(x) => js_slice_utf16(&js_to_string(x), 200),
                    _ => String::new(),
                };
                e.insert("reason".into(), Value::String(reason));
                Value::Object(e)
            })
            .collect();
        m.insert("advisorClaims".into(), Value::Array(mapped));
    }
    insert_if_defined(&mut m, "refutationStatus", truthy_or_undef(obj.get("refutationStatus")));
    insert_if_defined(&mut m, "fp", truthy_or_undef(obj.get("fp")));
    insert_if_defined(&mut m, "verifierStatus", truthy_or_undef(obj.get("verifierStatus")));
    // actionabilityScore: только конечное число (Number.isFinite).
    match obj.get("actionabilityScore").and_then(js_to_number) {
        Some(x) if x.is_finite() => {
            m.insert("actionabilityScore".into(), obj.get("actionabilityScore").unwrap().clone());
        }
        _ => {}
    }
    insert_if_defined(&mut m, "actionabilityPromoted", truthy_or_undef(obj.get("actionabilityPromoted")));
    Value::Object(m)
}

// ---------------------------------------------------------------------------
// buildSwarmVerdict (lib/swarm.mjs) — машинный вердикт для swarm-оркестратора
// ---------------------------------------------------------------------------

pub const SWARM_CONTRACT_VERSION: i64 = 1;
pub const BLAST_FINDING_SOURCE: &str = "blast-radius";

/// Дефолтный Array#sort V8: сравнение строк по UTF-16 code units.
fn sort_utf16(values: &mut [String]) {
    values.sort_by(|a, b| a.encode_utf16().collect::<Vec<u16>>().cmp(&b.encode_utf16().collect::<Vec<u16>>()));
}

/// suggestedScopeFiles: уникальные пути вне changedPaths, отсортированные
/// в UTF-16-порядке (как дефолтный sort в V8).
pub fn suggested_scope_files(affected: &[Value], changed_paths: Option<&Value>) -> Vec<String> {
    let changed: std::collections::HashSet<String> = match changed_paths {
        Some(Value::Array(arr)) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        _ => Default::default(),
    };
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for v in affected {
        let Some(p) = v.as_str() else { continue };
        if p.is_empty() || changed.contains(p) || !seen.insert(p.to_string()) {
            continue;
        }
        out.push(p.to_string());
    }
    sort_utf16(&mut out);
    out
}

/// `v ?? 0` (nullish): None/Null → 0, иначе как есть.
fn nullish_zero(v: Option<&Value>) -> Value {
    match v {
        None | Some(Value::Null) => json!(0),
        Some(x) => x.clone(),
    }
}

fn is_strong_blocker(f: &Value) -> bool {
    let Some(obj) = f.as_object() else { return false };
    if js_truthy(obj.get("prescan").unwrap_or(&Value::Null)) {
        return true;
    }
    obj.get("verifierStatus").and_then(Value::as_str) == Some("verified")
}

/// Стабильный machine-readable вердикт для оркестратора. `generated_at`
/// приходит из JS (часы живут на JS-стороне).
pub fn build_swarm_verdict(input: Option<&Value>, generated_at: &str) -> Value {
    let obj = input.and_then(Value::as_object);
    let blockers: Vec<Value> = obj
        .and_then(|o| o.get("blockers"))
        .filter(|v| v.is_array())
        .cloned()
        .map(|v| match v {
            Value::Array(a) => a,
            _ => vec![],
        })
        .unwrap_or_default();
    let would_block = obj
        .and_then(|o| o.get("wouldBlock"))
        .map(js_truthy)
        .unwrap_or(false);
    let strong_blockers: Vec<&Value> = blockers.iter().filter(|f| is_strong_blocker(f)).collect();
    let strong = would_block && !blockers.is_empty() && strong_blockers.len() == blockers.len();

    let blast: Value = match obj.and_then(|o| o.get("blastMetrics")) {
        Some(v @ Value::Object(_)) => v.clone(),
        _ => json!({}),
    };
    let blast_obj = blast.as_object().cloned().unwrap_or_default();
    let affected: Vec<Value> = match blast_obj.get("affected_files") {
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    };
    let scope = suggested_scope_files(&affected, obj.and_then(|o| o.get("changedPaths")));

    let mut pr = serde_json::Map::new();
    let pr_in = obj.and_then(|o| o.get("pr"));
    pr.insert("number".into(), nullish_zero(pr_in.and_then(|p| p.get("number"))));
    let pr_title = match pr_in.and_then(|p| p.get("title")) {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    };
    pr.insert("title".into(), Value::String(js_slice_utf16(&pr_title, 200)));
    let pr_url = pr_in
        .and_then(|p| p.get("html_url").filter(|x| js_truthy(x)).or(p.get("url")))
        .map(js_to_string)
        .unwrap_or_default();
    pr.insert("url".into(), Value::String(pr_url));

    let head_sha_raw = match obj.and_then(|o| o.get("headSha")) {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    };

    let blockers_out: Vec<Value> = blockers
        .iter()
        .map(|f| {
            let fo = f.as_object().cloned().unwrap_or_default();
            let mut e = serde_json::Map::new();
            e.insert("file".into(), match fo.get("file") {
                Some(x) if js_truthy(x) => x.clone(),
                _ => Value::String(String::new()),
            });
            e.insert("line".into(), match fo.get("line").and_then(js_to_number) {
                Some(x) => json!(x),
                None => json!(0),
            });
            e.insert("severity".into(), match fo.get("severity") {
                Some(x) if js_truthy(x) => x.clone(),
                _ => Value::String(String::new()),
            });
            let title = match fo.get("title") {
                Some(x) if js_truthy(x) => js_to_string(x),
                _ => String::new(),
            };
            e.insert("title".into(), Value::String(js_slice_utf16(&title, 200)));
            insert_if_defined(&mut e, "source", truthy_or_undef(fo.get("source")));
            insert_if_defined(&mut e, "prescan", truthy_or_undef(fo.get("prescan")));
            insert_if_defined(&mut e, "verifierStatus", truthy_or_undef(fo.get("verifierStatus")));
            insert_if_defined(&mut e, "blastCaller", truthy_or_undef(fo.get("blastCaller")));
            insert_if_defined(&mut e, "confidence", fo.get("confidence").cloned());
            Value::Object(e)
        })
        .collect();

    let mut blast_scope = serde_json::Map::new();
    blast_scope.insert("affected_files".into(), Value::Array(affected.clone()));
    blast_scope.insert("symbols_total".into(), nullish_zero(blast_obj.get("symbols_total")));
    blast_scope.insert("symbols_resolved".into(), nullish_zero(blast_obj.get("symbols_resolved")));
    blast_scope.insert("affected_nonempty".into(), nullish_zero(blast_obj.get("affected_nonempty")));
    blast_scope.insert(
        "graph_status".into(),
        match blast_obj.get("graph_status") {
            Some(x) if js_truthy(x) => x.clone(),
            _ => Value::String("unknown".into()),
        },
    );
    blast_scope.insert("cross_community".into(), nullish_zero(blast_obj.get("cross_community")));
    insert_if_defined(&mut blast_scope, "skip_reason", truthy_or_undef(blast_obj.get("skip_reason")));

    let notes: Vec<Value> = [
        Some(Value::String(if strong {
            "strong: all blockers independently confirmed (prescan or verifier)".into()
        } else if would_block {
            "wouldBlock without full strong confirmation — treat as advisory for swarm".into()
        } else {
            "no merge block".into()
        })),
        Some(Value::String(if !scope.is_empty() {
            format!("blast scope: {} file(s) outside PR diff", scope.len())
        } else {
            "blast scope empty".into()
        })),
        match obj.and_then(|o| o.get("sourceHint")) {
            Some(x) if js_truthy(x) && js_to_string(x) == BLAST_FINDING_SOURCE => {
                Some(Value::String("blast-radius findings alone never set strong without verifier".into()))
            }
            _ => None,
        },
    ]
    .into_iter()
    .flatten()
    .collect();

    let mut out = serde_json::Map::new();
    out.insert("version".into(), json!(SWARM_CONTRACT_VERSION));
    out.insert("role".into(), Value::String("external-verdict".into()));
    out.insert("generatedAt".into(), Value::String(generated_at.to_string()));
    out.insert("headSha".into(), Value::String(js_slice_utf16(&head_sha_raw, 40)));
    out.insert("pr".into(), Value::Object(pr));
    out.insert("wouldBlock".into(), Value::Bool(would_block));
    out.insert("strong".into(), Value::Bool(strong));
    out.insert("blockers".into(), Value::Array(blockers_out));
    out.insert("strongBlockers".into(), json!(strong_blockers.len()));
    out.insert("blastScope".into(), Value::Object(blast_scope));
    out.insert(
        "suggestedScopeFiles".into(),
        Value::Array(scope.into_iter().map(Value::String).collect()),
    );
    insert_if_defined(&mut out, "verdict", truthy_or_undef(obj.and_then(|o| o.get("verdict"))));
    out.insert("notes".into(), Value::Array(notes));
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// buildReviewJson — полный JSON payload для CI artifact / интеграций
// ---------------------------------------------------------------------------

fn blast_export(blast_metrics: Option<&Value>) -> Option<Value> {
    let obj = match blast_metrics {
        Some(v) if (v.is_object() || v.is_array()) && js_truthy(v) => v.as_object()?,
        _ => return None,
    };
    // blastMetrics && typeof === 'object' — массив тоже typeof 'object' (как в JS).
    let mut m = serde_json::Map::new();
    m.insert("symbols_total".into(), nullish_zero(obj.get("symbols_total")));
    m.insert("symbols_resolved".into(), nullish_zero(obj.get("symbols_resolved")));
    m.insert("affected_nonempty".into(), nullish_zero(obj.get("affected_nonempty")));
    m.insert(
        "graph_status".into(),
        match obj.get("graph_status") {
            Some(x) if js_truthy(x) => x.clone(),
            _ => Value::String("unknown".into()),
        },
    );
    insert_if_defined(&mut m, "skip_reason", truthy_or_undef(obj.get("skip_reason")));
    m.insert(
        "affected_files".into(),
        match obj.get("affected_files") {
            Some(Value::Array(a)) => Value::Array(a.clone()),
            _ => Value::Array(vec![]),
        },
    );
    insert_if_defined(&mut m, "confidence", truthy_or_undef(obj.get("confidence")));
    m.insert("cross_community".into(), nullish_zero(obj.get("cross_community")));
    Some(Value::Object(m))
}

/// Полный JSON payload. `generated_at` — ISO-метка из JS (часы на JS-стороне).
pub fn build_review_json(input: Option<&Value>, generated_at: &str) -> Result<Value> {
    let obj = match input {
        Some(Value::Object(o)) => o,
        _ => bail!("buildReviewJson: ожидался объект input"),
    };
    let get = |k: &str| obj.get(k);

    let blast = blast_export(get("blastMetrics"));
    let swarm_input = {
        let mut m = serde_json::Map::new();
        m.insert("wouldBlock".into(), Value::Bool(js_truthy(get("wouldBlock").unwrap_or(&Value::Null))));
        m.insert("blockers".into(), get("blockers").cloned().unwrap_or(Value::Null));
        m.insert("blastMetrics".into(), blast.clone().or_else(|| get("blastMetrics").cloned()).unwrap_or(Value::Null));
        m.insert("changedPaths".into(), get("changedPaths").cloned().unwrap_or(Value::Null));
        m.insert("headSha".into(), get("headSha").cloned().unwrap_or(Value::Null));
        m.insert("pr".into(), get("pr").cloned().unwrap_or(Value::Null));
        m.insert("verdict".into(), get("verdict").cloned().unwrap_or(Value::Null));
        Value::Object(m)
    };
    let swarm = build_swarm_verdict(Some(&swarm_input), generated_at);

    let pr_in = get("pr");
    let mut pr = serde_json::Map::new();
    pr.insert("number".into(), nullish_zero(pr_in.and_then(|p| p.get("number"))));
    let pr_title = match pr_in.and_then(|p| p.get("title")) {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    };
    pr.insert("title".into(), Value::String(js_slice_utf16(&pr_title, 200)));
    pr.insert(
        "url".into(),
        Value::String(pr_in.and_then(|p| p.get("html_url")).filter(|x| js_truthy(x)).map(js_to_string).unwrap_or_default()),
    );

    let head_sha = match get("headSha") {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    };
    let review_mode_raw = get("reviewMode");
    let review_mode = match review_mode_raw {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => "consensus".to_string(),
    };

    let findings_arr: Vec<Value> = match get("findings") {
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    };
    let blockers_arr: Vec<Value> = match get("blockers") {
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    };

    let total = match get("total") {
        Some(Value::Null) | None => json!(findings_arr.len()),
        Some(v) => v.clone(),
    };

    let mut out = serde_json::Map::new();
    out.insert("version".into(), json!(EXPORT_VERSION));
    out.insert("generatedAt".into(), Value::String(generated_at.to_string()));
    out.insert("pr".into(), Value::Object(pr));
    out.insert("headSha".into(), Value::String(js_slice_utf16(&head_sha, 40)));
    out.insert("reviewMode".into(), Value::String(review_mode.clone()));
    if let Some(route) = get("route").filter(|v| js_truthy(v)) {
        let mut r = serde_json::Map::new();
        r.insert("route".into(), route.get("route").cloned().unwrap_or(Value::Null));
        r.insert("reason".into(), route.get("reason").cloned().unwrap_or(Value::Null));
        out.insert("router".into(), Value::Object(r));
    }
    out.insert("wouldBlock".into(), Value::Bool(js_truthy(get("wouldBlock").unwrap_or(&Value::Null))));
    insert_if_defined(&mut out, "verdict", truthy_or_undef(get("verdict")));
    insert_if_defined(&mut out, "llmFailed", truthy_or_undef(get("llmFailed")));
    match get("llmErrors") {
        Some(Value::Array(a)) if !a.is_empty() => {
            out.insert("llmErrors".into(), Value::Array(a.clone()));
        }
        _ => {}
    }
    out.insert(
        "counts".into(),
        match get("counts") {
            Some(v) if js_truthy(v) => v.clone(),
            _ => json!({}),
        },
    );
    out.insert("total".into(), total);
    out.insert(
        "findings".into(),
        Value::Array(findings_arr.iter().map(slim_finding).collect()),
    );
    out.insert(
        "blockers".into(),
        Value::Array(blockers_arr.iter().map(slim_finding).collect()),
    );
    out.insert(
        "reasoning".into(),
        Value::Array(extract_reasoning_entries(get("reviews"), &review_mode, REASONING_EXPORT_MAX, 200)?),
    );
    insert_if_defined(&mut out, "verifier", get("verifierStats").cloned());
    insert_if_defined(&mut out, "refutation", get("refutationStats").cloned());
    insert_if_defined(&mut out, "blast", blast);
    out.insert("swarm".into(), swarm);
    if let Some(prescan) = get("prescanResult").filter(|v| js_truthy(v)) {
        let mut p = serde_json::Map::new();
        p.insert(
            "findings".into(),
            match prescan.get("findings") {
                Some(Value::Array(a)) => json!(a.len()),
                _ => json!(0),
            },
        );
        insert_if_defined(&mut p, "scanned", prescan.get("scanned").cloned());
        insert_if_defined(&mut p, "skipped", prescan.get("skipped").cloned());
        insert_if_defined(&mut p, "byTool", prescan.get("byTool").cloned());
        insert_if_defined(&mut p, "error", prescan.get("error").cloned());
        out.insert("prescan".into(), Value::Object(p));
    }
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------------------
// SARIF 2.1.0 (GitHub Code Scanning)
// ---------------------------------------------------------------------------

/// Findings для SARIF: по умолчанию только blockers (шум minor/nit в Code Scanning).
pub fn select_sarif_findings(payload: Option<&Value>, sarif_all: bool) -> Vec<Value> {
    let obj = payload.and_then(Value::as_object);
    let get_arr = |k: &str| -> Vec<Value> {
        match obj.and_then(|o| o.get(k)) {
            Some(Value::Array(a)) => a.clone(),
            _ => vec![],
        }
    };
    if sarif_all {
        return get_arr("findings");
    }
    let blockers = get_arr("blockers");
    if !blockers.is_empty() {
        return blockers;
    }
    get_arr("findings")
        .into_iter()
        .filter(|f| js_truthy(f.get("consensusBlock").unwrap_or(&Value::Null)))
        .collect()
}

fn sarif_location(f: &Value) -> Value {
    let uri = sarif_artifact_uri(f.get("file"));
    let mut physical = serde_json::Map::new();
    physical.insert("artifactLocation".into(), json!({ "uri": uri }));
    if f.get("line").and_then(js_to_number).map(|x| x > 0.0).unwrap_or(false) {
        physical.insert("region".into(), json!({ "startLine": f.get("line").unwrap().clone() }));
    }
    json!({ "physicalLocation": Value::Object(physical) })
}

/// SARIF ruleId: `<prefix>/<severity>`; префикс — из профиля (JS передаёт
/// {toolName, sarifRulePrefix, repositoryUri}).
pub fn sarif_rule_ids(prefix: &str) -> Vec<(String, String)> {
    SEVERITIES
        .iter()
        .map(|sev| (sev.to_string(), format!("{prefix}/{sev}")))
        .collect()
}

/// SARIF 2.1.0 для upload-sarif / Code Scanning. Порядок ключей — часть
/// побайтового контракта (собирается в порядке JS-литералов).
pub fn build_sarif(
    payload: Option<&Value>,
    repository_uri: Option<&str>,
    sarif_all: bool,
    profile: Option<&Value>,
) -> Value {
    let findings = select_sarif_findings(payload, sarif_all);
    let prof = profile.and_then(Value::as_object);
    let prefix = prof
        .and_then(|p| p.get("sarifRulePrefix"))
        .map(js_to_string)
        .unwrap_or_default();
    let tool = prof.and_then(|p| p.get("toolName")).map(js_to_string).unwrap_or_default();
    let sev_to_rule: Vec<(String, String)> = sarif_rule_ids(&prefix);

    let mut rules = Vec::new();
    for (sev, rule_id) in &sev_to_rule {
        rules.push(json!({
            "id": rule_id,
            "name": sev,
            "shortDescription": { "text": format!("AI review — {sev}") },
            "defaultConfiguration": { "level": sev_to_sarif_level(sev) },
        }));
    }

    let results: Vec<Value> = findings
        .iter()
        .map(|f| {
            let sev_raw = f.get("severity").and_then(Value::as_str).unwrap_or("");
            let sev = if sev_to_rule.iter().any(|(s, _)| s == sev_raw) {
                sev_raw
            } else {
                "minor"
            };
            let level = sev_to_sarif_level(sev);
            let rule_id = sev_to_rule
                .iter()
                .find(|(s, _)| s == sev)
                .map(|(_, r)| r.clone())
                .unwrap_or_default();
            let title_raw = match f.get("title") {
                Some(x) if js_truthy(x) => js_to_string(x),
                _ => "finding".to_string(),
            };
            let title = js_slice_utf16(&title_raw, TITLE_MAX);
            let mut properties = serde_json::Map::new();
            insert_if_defined(&mut properties, "model", truthy_or_undef(f.get("model")));
            insert_if_defined(&mut properties, "confidence", f.get("confidence").cloned());
            properties.insert(
                "consensusBlock".into(),
                truthy_or_undef(f.get("consensusBlock")).unwrap_or(Value::Bool(false)),
            );
            insert_if_defined(&mut properties, "prescan", truthy_or_undef(f.get("prescan")));
            json!({
                "ruleId": rule_id,
                "level": level,
                "message": { "text": js_slice_utf16(&title, SARIF_MESSAGE_MAX) },
                "locations": [sarif_location(f)],
                "properties": Value::Object(properties),
            })
        })
        .collect();

    let pr_number = payload
        .and_then(|p| p.get("pr"))
        .and_then(|p| p.get("number"))
        .map(|v| js_to_string(v))
        .unwrap_or_else(|| "?".to_string());

    let mut driver = serde_json::Map::new();
    driver.insert("name".into(), Value::String(tool.clone()));
    driver.insert("version".into(), Value::String(TOOL_VERSION.into()));
    let information_uri = prof
        .and_then(|p| p.get("repositoryUri"))
        .filter(|x| js_truthy(x))
        .map(js_to_string)
        .or(repository_uri.filter(|s| !s.is_empty()).map(String::from));
    insert_if_defined(&mut driver, "informationUri", information_uri.map(Value::String));
    driver.insert("rules".into(), Value::Array(rules));

    let mut run = serde_json::Map::new();
    run.insert("tool".into(), json!({ "driver": Value::Object(driver) }));
    run.insert("results".into(), Value::Array(results));
    let mut automation = serde_json::Map::new();
    automation.insert(
        "description".into(),
        json!({ "text": format!("AI code review PR #{pr_number}") }),
    );
    if let Some(uri) = repository_uri.filter(|s| !s.is_empty()) {
        automation.insert("guid".into(), Value::String(sarif_automation_guid(uri, &tool)));
    }
    run.insert("automationDetails".into(), Value::Object(automation));

    json!({
        "$schema": SARIF_SCHEMA,
        "version": "2.1.0",
        "runs": [Value::Object(run)],
    })
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_shape_and_stability() {
        let g = sarif_automation_guid("https://github.com/o/r", "orakul-ai-review");
        assert!(regex::Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")
            .unwrap()
            .is_match(&g));
        assert_eq!(g, sarif_automation_guid("https://github.com/o/r", "orakul-ai-review"));
        assert_ne!(g, sarif_automation_guid("https://github.com/o/r2", "orakul-ai-review"));
    }

    #[test]
    fn artifact_uri_encodes_segments() {
        assert_eq!(sarif_artifact_uri(Some(&json!("app/server/a.ts"))), "app/server/a.ts");
        assert_eq!(sarif_artifact_uri(Some(&json!("app/my file.ts"))), "app/my%20file.ts");
        assert_eq!(sarif_artifact_uri(Some(&json!(""))), "unknown");
        assert_eq!(sarif_artifact_uri(Some(&json!("dir/м файл.ts"))), "dir/%D0%BC%20%D1%84%D0%B0%D0%B9%D0%BB.ts");
        assert_eq!(sarif_artifact_uri(Some(&json!(" a.ts "))), "a.ts");
        // encodeURIComponent не трогает - _ . ! ~ * ' ( )
        assert_eq!(sarif_artifact_uri(Some(&json!("a-b_c.d!e~f*g'h(i)"))), "a-b_c.d!e~f*g'h(i)");
    }

    #[test]
    fn slim_finding_truncates_and_drops() {
        let f = json!({
            "file": "a.ts", "line": 5, "severity": "major", "title": "x".repeat(300),
            "body": "y".repeat(500), "model": "m1", "confidence": 0.9,
            "consensusBlock": false,
        });
        let slim = slim_finding(&f);
        let obj = slim.as_object().unwrap();
        assert_eq!(obj.get("title").unwrap().as_str().unwrap().len(), 200);
        assert_eq!(obj.get("body").unwrap().as_str().unwrap().len(), 400);
        assert_eq!(obj.get("bodyTruncated").unwrap(), &json!(true));
        assert!(!obj.contains_key("prescan"));
        assert!(!obj.contains_key("fp"));
        // пустое body → поле выпадает, bodyTruncated=false
        let slim2 = slim_finding(&json!({"file": "a.ts", "line": 0, "severity": "nit"}));
        let obj2 = slim2.as_object().unwrap();
        assert!(!obj2.contains_key("body"));
        assert_eq!(obj2.get("bodyTruncated").unwrap(), &json!(false));
        assert_eq!(obj2.get("line").unwrap(), &json!(0));
        assert_eq!(obj2.get("severity").unwrap(), &json!("nit"));
    }

    #[test]
    fn select_sarif_findings_prefers_blockers() {
        let payload = json!({
            "findings": [
                {"file": "a.ts", "line": 1, "severity": "major", "consensusBlock": true},
                {"file": "b.ts", "line": 2, "severity": "minor"},
            ],
            "blockers": [{"file": "c.ts", "line": 3, "severity": "critical"}],
        });
        let sel = select_sarif_findings(Some(&payload), false);
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].get("file").unwrap(), &json!("c.ts"));
        assert_eq!(select_sarif_findings(Some(&payload), true).len(), 2);
        // без blockers — consensusBlock findings
        let payload2 = json!({ "findings": payload["findings"].clone(), "blockers": [] });
        assert_eq!(select_sarif_findings(Some(&payload2), false).len(), 1);
    }

    #[test]
    fn build_sarif_structure_and_rules() {
        let profile = json!({
            "toolName": "orakul-ai-review",
            "sarifRulePrefix": "orakul",
            "repositoryUri": "https://github.com/o/r",
        });
        let payload = json!({
            "pr": { "number": 42 },
            "findings": [{"file": "a.ts", "line": 7, "severity": "critical", "title": "IDOR"}],
            "blockers": [{"file": "a.ts", "line": 7, "severity": "critical", "title": "IDOR", "model": "m1", "confidence": 0.95}],
        });
        let sarif = build_sarif(Some(&payload), Some("https://github.com/o/r"), false, Some(&profile));
        assert_eq!(sarif["$schema"], json!(SARIF_SCHEMA));
        assert_eq!(sarif["version"], json!("2.1.0"));
        let run = &sarif["runs"][0];
        assert_eq!(run["tool"]["driver"]["name"], json!("orakul-ai-review"));
        assert_eq!(run["tool"]["driver"]["version"], json!(TOOL_VERSION));
        assert_eq!(run["tool"]["driver"]["rules"].as_array().unwrap().len(), 4);
        assert_eq!(run["tool"]["driver"]["rules"][0]["id"], json!("orakul/critical"));
        assert_eq!(run["tool"]["driver"]["rules"][0]["defaultConfiguration"]["level"], json!("error"));
        assert_eq!(run["results"].as_array().unwrap().len(), 1);
        assert_eq!(run["results"][0]["level"], json!("error"));
        assert_eq!(run["results"][0]["ruleId"], json!("orakul/critical"));
        assert_eq!(run["results"][0]["locations"][0]["physicalLocation"]["region"]["startLine"], json!(7));
        assert!(run["automationDetails"]["guid"].as_str().unwrap().len() == 36);
        assert_eq!(run["automationDetails"]["description"]["text"], json!("AI code review PR #42"));
        // line=0 → региона нет
        let payload2 = json!({
            "findings": [], "blockers": [{"file": "a.ts", "line": 0, "severity": "major", "title": "t"}],
        });
        let sarif2 = build_sarif(Some(&payload2), None, false, Some(&profile));
        let loc = &sarif2["runs"][0]["results"][0]["locations"][0]["physicalLocation"];
        assert!(loc.get("region").is_none());
        assert!(sarif2["runs"][0]["automationDetails"].get("guid").is_none());
        // неизвестная severity → minor
        let payload3 = json!({
            "findings": [], "blockers": [{"file": "a.ts", "line": 1, "severity": "weird", "title": "t"}],
        });
        let sarif3 = build_sarif(Some(&payload3), None, false, Some(&profile));
        assert_eq!(sarif3["runs"][0]["results"][0]["ruleId"], json!("orakul/minor"));
    }

    #[test]
    fn build_review_json_defaults_and_slim() {
        let input = json!({
            "pr": { "number": 7, "title": "T", "html_url": "https://github.com/o/r/pull/7" },
            "headSha": "abc123",
            "findings": [{"file": "a.ts", "line": 1, "severity": "major", "title": "t", "body": "b"}],
        });
        let out = build_review_json(Some(&input), "2026-09-04T00:00:00.000Z").unwrap();
        assert_eq!(out["version"], json!(2));
        assert_eq!(out["generatedAt"], json!("2026-09-04T00:00:00.000Z"));
        assert_eq!(out["reviewMode"], json!("consensus"));
        assert_eq!(out["total"], json!(1));
        assert_eq!(out["counts"], json!({}));
        assert_eq!(out["wouldBlock"], json!(false));
        assert_eq!(out["swarm"]["role"], json!("external-verdict"));
        assert_eq!(out["swarm"]["pr"]["number"], json!(7));
        assert_eq!(out["swarm"]["generatedAt"], json!("2026-09-04T00:00:00.000Z"));
        assert!(!out.as_object().unwrap().contains_key("router"));
        assert!(!out.as_object().unwrap().contains_key("verdict"));
        // total ?? — явный 0 сохраняется (nullish, не falsy)
        let input0 = json!({ "total": 0, "findings": [{"file": "a.ts", "line": 1}] });
        let out0 = build_review_json(Some(&input0), "ts").unwrap();
        assert_eq!(out0["total"], json!(0));
    }

    #[test]
    fn swarm_verdict_strong_and_scope() {
        let input = json!({
            "wouldBlock": true,
            "blockers": [
                {"file": "a.ts", "line": 1, "severity": "critical", "title": "t", "prescan": true},
                {"file": "b.ts", "line": 2, "severity": "major", "title": "t2", "verifierStatus": "verified"},
            ],
            "blastMetrics": { "affected_files": ["z/out.ts", "a/out.ts", "a/in-diff.ts"] },
            "changedPaths": ["a/in-diff.ts"],
            "headSha": "sha1", "pr": { "number": 1 }, "verdict": { "blocked": 1 },
        });
        let v = build_swarm_verdict(Some(&input), "ts");
        assert_eq!(v["strong"], json!(true));
        assert_eq!(v["strongBlockers"], json!(2));
        assert_eq!(v["suggestedScopeFiles"], json!(["a/out.ts", "z/out.ts"]));
        assert_eq!(v["verdict"], json!({ "blocked": 1 }));
        // advisory-блокер без prescan/verifier → strong=false
        let input2 = json!({
            "wouldBlock": true,
            "blockers": [{"file": "a.ts", "line": 1, "severity": "major", "title": "t"}],
        });
        let v2 = build_swarm_verdict(Some(&input2), "ts");
        assert_eq!(v2["strong"], json!(false));
        assert_eq!(v2["notes"][0], json!("wouldBlock without full strong confirmation — treat as advisory for swarm"));
    }
}
