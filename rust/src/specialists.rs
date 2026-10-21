//! Порт lib/specialists.mjs (Фаза 3, шаг 2): R5 specialist advisors +
//! refutation jury — чистая логика без LLM-вызовов.
//!
//! Оркестрация (engine) и сами LLM-вызовы остаются в JS: эти функции
//! получают и возвращают только JSON-данные. normalizeAdvisorClaims
//! переиспользует порт из findings.rs (тот же код, что в normalizeFinding).
//!
//! Паритет с JS, включая:
//! - JS truthiness везде, где оригинал проверяет `||` / `?.` / filter(Boolean);
//! - шаблонные строки логов с коэрсией `${...}` через js_to_string
//!   (undefined → "undefined", как в JS);
//! - mutate-in-place семантику applyRefutation: мутации возвращаются
//!   списком (index + новые поля), JS-обёртка накладывает на исходные
//!   объекты (разделяемые ссылки пайплайна), отфильтрованные (drop)
//!   выбираются списком сохранённых индексов;
//! - порядок ключей в возвращаемых объектах (role/model/status/… —
//!   как в JS-литералах).
//!
//! Отклонение (см. rust/parity/specialists.parity.mjs): applyRefutation в JS
//! падает с TypeError, если advisorClaims — непустая строка ('x'.length truthy
//! → decideRefutation делает .map по строке). После normalizeFinding claims
//! всегда массив либо отсутствует — вход недостижим; Rust трактует не-массив
//! как «нет claims» (ветка noClaims) вместо падения.

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::findings::{js_to_number, js_to_string, js_trim, js_truthy, BLAST_FINDING_SOURCE};

pub const SPECIALIST_ROLES: [&str; 3] = ["security", "correctness", "architecture"];

/// Bulk-дефолт ролей (lib/constants.mjs DEFAULT_MOA_SPECIALIST_MODELS).
pub const DEFAULT_MOA_SPECIALIST_MODELS: &str =
    "security:glm-5.3-flash,correctness:kimi-k2.7-code,architecture:glm-5.3";

/// Семантические дефолты ролей (не зависят от порядка MOA_REFERENCE_MODELS).
pub const ROLE_DEFAULT_MODEL: [(&str, &str); 3] = [
    ("security", "glm-5.3-flash"),
    ("correctness", "kimi-k2.7-code"),
    ("architecture", "glm-5.3"),
];

fn role_default(role: &str) -> Option<&'static str> {
    ROLE_DEFAULT_MODEL
        .iter()
        .find(|(r, _)| *r == role)
        .map(|(_, m)| *m)
}

// ---------------------------------------------------------------------------
// parseSpecialistAssignments
// ---------------------------------------------------------------------------

/// Порт parseSpecialistBulk: "role:model,..." → map role → model.
/// Невалидные части (без ':', пустая роль/модель, роль не из SPECIALIST_ROLES)
/// пропускаются; поздний дубликат роли перезаписывает ранний (JS-объект).
fn parse_specialist_bulk(bulk: &str) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for part in bulk.split(',') {
        let part = js_trim(part);
        if part.is_empty() {
            continue; // filter(Boolean) после trim
        }
        let Some(i) = part.find(':') else { continue };
        if i == 0 {
            continue; // indexOf(':') <= 0 — пустая роль до двоеточия
        }
        let role = js_trim(&part[..i]);
        let model = js_trim(&part[i + 1..]);
        if SPECIALIST_ROLES.contains(&role) && !model.is_empty() {
            out.insert(role.to_string(), model.to_string());
        }
    }
    out
}

/// Порт parseSpecialistAssignments. env — подмножество env с ключами
/// MOA_SPECIALIST_MODELS / MOA_SPECIALIST_<ROLE>_MODEL (обёртка выделяет
/// их из process.env); JS-семантика: bulk falsy → DEFAULT, per-role
/// значение с непустым trim перезаписывает bulk. moaReferences — legacy
/// fallback только если роль не задана явно (на практике не достижим:
/// ROLE_DEFAULT_MODEL непуст для всех ролей — как и в JS).
pub fn parse_specialist_assignments(env: &Value, moa_references: &[Value]) -> Vec<Value> {
    let bulk = match env.get("MOA_SPECIALIST_MODELS") {
        Some(Value::String(s)) if js_truthy(&Value::String(s.clone())) => s.clone(),
        // falsy (в т.ч. отсутствие) → дефолт; не-строка truthy в JS уронила
        // бы .split — такие env в реальности не встречаются, трактуем как дефолт.
        _ => DEFAULT_MOA_SPECIALIST_MODELS.to_string(),
    };
    let mut from_env = parse_specialist_bulk(&bulk);

    for role in SPECIALIST_ROLES {
        let key = format!("MOA_SPECIALIST_{}_MODEL", role.to_uppercase());
        if let Some(Value::String(s)) = env.get(&key) {
            let trimmed = js_trim(s);
            if !trimmed.is_empty() {
                from_env.insert(role.to_string(), trimmed.to_string());
            }
        }
    }

    SPECIALIST_ROLES
        .iter()
        .enumerate()
        .map(|(i, role)| {
            let mut assignment = Map::new();
            assignment.insert("role".into(), Value::String((*role).to_string()));
            // fromEnv[role] || ROLE_DEFAULT_MODEL[role] || moaReferences[i]
            let from_bulk = from_env.get(*role).map(|s| Value::String(s.clone()));
            let default = role_default(role).map(|s| Value::String(s.to_string()));
            let legacy = moa_references.get(i).cloned();
            let model = [from_bulk, default, legacy]
                .into_iter()
                .flatten()
                .find(js_truthy)
                .unwrap_or(Value::Null);
            assignment.insert("model".into(), model);
            Value::Object(assignment)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// decideRefutation / applyRefutation
// ---------------------------------------------------------------------------

/// Порт decideRefutation: все rejected → drop; есть verified → keep;
/// downgraded без verified → downgrade (nit если хоть один nit, иначе minor).
pub fn decide_refutation(claims: &[Value]) -> Value {
    fn claim_status(c: &Value) -> Option<&str> {
        c.get("status").and_then(Value::as_str)
    }
    let status = claim_status;
    if claims.is_empty() {
        return json!({ "action": "keep" });
    }
    if claims.iter().all(|c| status(c) == Some("rejected")) {
        return json!({ "action": "drop" });
    }
    if claims.iter().any(|c| status(c) == Some("verified")) {
        return json!({ "action": "keep" });
    }
    if claims.iter().any(|c| status(c) == Some("downgraded")) {
        let severity = if claims
            .iter()
            .filter(|c| status(c) == Some("downgraded"))
            .any(|c| c.get("severity").and_then(Value::as_str) == Some("nit"))
        {
            "nit"
        } else {
            "minor"
        };
        return json!({ "action": "downgrade", "severity": severity });
    }
    json!({ "action": "keep" })
}

/// Мутация одной находки (applyRefutation): только изменённые поля.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefutationMutation {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refutation_status: Option<String>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefutationStats {
    pub kept: usize,
    pub dropped: usize,
    pub downgraded: usize,
    pub no_claims: usize,
    pub downgraded_no_claims: usize,
    /// RFC 21: blast-находки, пропущенные refutation'ом. Поле в выводе только при > 0 —
    /// прежние входы дают прежний JSON (parity с deprecated JS-копией).
    #[serde(skip_serializing_if = "is_zero")]
    pub skipped_blast: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

pub struct RefutationOutcome {
    /// Индексы исходного массива, оставшиеся после drop (в исходном порядке).
    pub kept: Vec<usize>,
    pub mutations: Vec<RefutationMutation>,
    pub stats: RefutationStats,
    /// Строки лога в порядке генерации (JS-обёртка шлёт их в log).
    pub logs: Vec<String>,
}

/// Коэрсия для шаблонных строк логов: отсутствующее поле → "undefined"
/// (как `${f.file}` в JS), null → "null".
fn js_tmpl(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(x) => js_to_string(x),
    }
}

/// Порт applyRefutation: jury-разбор advisorClaims по каждой находке.
/// strict: critical/major без advisorClaims → downgrade minor (не блок без jury).
/// skip_blast (RFC 21, `REVIEW_BLAST_VERIFY` ≠ 0): blast-находка проходит как prescan —
/// refutation меряет согласие advisors, а у детерминированной находки их нет; понижать её
/// за отсутствие голосов значит путать «никто не возразил» с «никто не спрашивал».
/// Судит её verifier. Rust-owned: JS-копия этого параметра не знает.
/// Мутации НЕ применяются к входу — возвращаются отдельно (обёртка накладывает
/// на исходные объекты, сохраняя семантику разделяемых ссылок).
pub fn apply_refutation(
    all_findings: &[Value],
    strict: bool,
    block_on: &[Value],
    skip_blast: bool,
) -> RefutationOutcome {
    let tiers: Vec<Value> = block_on.to_vec();
    let in_tiers = |f: &Value| {
        let sev = f.get("severity").unwrap_or(&Value::Null);
        tiers.iter().any(|t| t == sev)
    };
    let mut kept = Vec::new();
    let mut mutations = Vec::new();
    let mut stats = RefutationStats::default();
    let mut logs = Vec::new();

    for (i, f) in all_findings.iter().enumerate() {
        if js_truthy(f.get("prescan").unwrap_or(&Value::Null)) {
            kept.push(i);
            continue;
        }
        if skip_blast && f.get("source").and_then(Value::as_str) == Some(BLAST_FINDING_SOURCE) {
            mutations.push(RefutationMutation {
                index: i,
                severity: None,
                refutation_status: Some("not_applicable".to_string()),
            });
            stats.skipped_blast += 1;
            kept.push(i);
            continue;
        }
        let claims: &[Value] = match f.get("advisorClaims").and_then(Value::as_array) {
            Some(c) if !c.is_empty() => c,
            _ => {
                if strict && in_tiers(f) {
                    mutations.push(RefutationMutation {
                        index: i,
                        severity: Some(json!("minor")),
                        refutation_status: Some("downgraded_no_claims".to_string()),
                    });
                    stats.downgraded_no_claims += 1;
                    stats.downgraded += 1;
                    logs.push(format!(
                        "refutation strict: [block-tier без advisorClaims] {}:{} «{}» → minor",
                        js_tmpl(f.get("file")),
                        js_tmpl(f.get("line")),
                        js_tmpl(f.get("title")),
                    ));
                    kept.push(i);
                    continue;
                }
                stats.no_claims += 1;
                kept.push(i);
                continue;
            }
        };

        let decision = decide_refutation(claims);
        match decision.get("action").and_then(Value::as_str) {
            Some("drop") => {
                stats.dropped += 1;
                mutations.push(RefutationMutation {
                    index: i,
                    severity: None,
                    refutation_status: Some("dropped".to_string()),
                });
                logs.push(format!(
                    "refutation dropped: [{}] {}:{} «{}» — все advisors rejected",
                    js_tmpl(f.get("severity")),
                    js_tmpl(f.get("file")),
                    js_tmpl(f.get("line")),
                    js_tmpl(f.get("title")),
                ));
                // Не добавляем в kept — находка отбрасывается.
            }
            Some("downgrade") => {
                // decision.severity || 'minor' — severity у downgrade всегда непустой.
                let sev = match decision.get("severity") {
                    Some(s) if js_truthy(s) => s.clone(),
                    _ => json!("minor"),
                };
                mutations.push(RefutationMutation {
                    index: i,
                    severity: Some(sev.clone()),
                    refutation_status: Some("downgraded".to_string()),
                });
                stats.downgraded += 1;
                logs.push(format!(
                    "refutation downgraded: «{}» → {}",
                    js_tmpl(f.get("title")),
                    js_to_string(&sev),
                ));
                kept.push(i);
            }
            _ => {
                let status = if claims
                    .iter()
                    .any(|c| c.get("status").and_then(Value::as_str) == Some("verified"))
                {
                    "verified"
                } else {
                    "kept"
                };
                mutations.push(RefutationMutation {
                    index: i,
                    severity: None,
                    refutation_status: Some(status.to_string()),
                });
                stats.kept += 1;
                kept.push(i);
            }
        }
    }

    RefutationOutcome {
        kept,
        mutations,
        stats,
        logs,
    }
}

// ---------------------------------------------------------------------------
// mergeSpecialistReferences
// ---------------------------------------------------------------------------

/// Порт mergeSpecialistReferences: слив specialist references со всех чанков.
/// Ключ `${r.role || 'advisor'}::${r.model}` — role falsy → 'advisor', model
/// коэрсится в строку (undefined → "undefined", как в JS-шаблоне). Порядок —
/// первое появление ключа (JS Map). chunkCount инкрементится ДО сборки tag.
pub fn merge_specialist_references(chunk_results: &[Value]) -> Vec<Value> {
    let mut map: IndexMap<String, Value> = IndexMap::new();
    for res in chunk_results {
        let references = match res.as_object().and_then(|o| o.get("references")) {
            Some(Value::Array(a)) => a.clone(),
            // for..of по строке итерирует её символы (патологический вход,
            // но JS именно так и работает — повторяем для паритета).
            Some(Value::String(s)) => s.chars().map(|c| Value::String(c.to_string())).collect(),
            _ => Vec::new(), // res?.references || []
        };
        for r in references {
            let role = match r.get("role") {
                Some(v) if js_truthy(v) => js_to_string(v),
                _ => "advisor".to_string(),
            };
            let model = js_to_string(r.get("model").unwrap_or(&Value::Null));
            let key = format!("{role}::{model}");
            match map.get_mut(&key) {
                None => {
                    // {...r, chunkCount: 1} — spread строки даёт индексные ключи.
                    let mut entry = match &r {
                        Value::String(s) => {
                            let mut m = Map::new();
                            for (i, c) in s.chars().enumerate() {
                                m.insert(i.to_string(), Value::String(c.to_string()));
                            }
                            m
                        }
                        _ => r.as_object().cloned().unwrap_or_default(),
                    };
                    entry.insert("chunkCount".into(), json!(1));
                    map.insert(key, Value::Object(entry));
                }
                Some(prev) => {
                    let obj = prev.as_object_mut().expect("entry — объект");
                    // (prev.chunkCount || 1) + 1
                    let chunk_count = match obj.get("chunkCount").and_then(js_to_number) {
                        Some(n) if n.is_finite() && n > 0.0 => n + 1.0,
                        _ => 2.0,
                    };
                    obj.insert("chunkCount".into(), json!(chunk_count));
                    if js_truthy(r.get("text").unwrap_or(&Value::Null))
                        && !js_truthy(r.get("error").unwrap_or(&Value::Null))
                    {
                        let text = if js_truthy(obj.get("text").unwrap_or(&Value::Null)) {
                            let tag = if chunk_count > 1.0 {
                                format!(
                                    "\n\n[chunk {}]\n",
                                    js_to_string(&json!(chunk_count))
                                )
                            } else {
                                "\n\n".to_string()
                            };
                            // `${prev.text}${tag}${r.text}` — коэрсия шаблонной строки.
                            Value::String(format!(
                                "{}{}{}",
                                js_to_string(obj.get("text").unwrap()),
                                tag,
                                js_to_string(r.get("text").unwrap()),
                            ))
                        } else {
                            r.get("text").unwrap().clone()
                        };
                        obj.insert("text".into(), text);
                    }
                    if js_truthy(r.get("error").unwrap_or(&Value::Null))
                        && !js_truthy(obj.get("text").unwrap_or(&Value::Null))
                    {
                        obj.insert("error".into(), r.get("error").unwrap().clone());
                    }
                }
            }
        }
    }
    map.into_values().collect()
}

// ---------------------------------------------------------------------------
// formatRefutationLog
// ---------------------------------------------------------------------------

/// Порт formatRefutationLog.
pub fn format_refutation_log(stats: &Value) -> String {
    let n = |k: &str| js_to_string(stats.get(k).unwrap_or(&Value::Null));
    let extra = match stats.get("downgradedNoClaims") {
        Some(v) if js_truthy(v) => format!(" downgradedNoClaims={}", n("downgradedNoClaims")),
        _ => String::new(),
    };
    let blast = match stats.get("skippedBlast") {
        Some(v) if js_truthy(v) => format!(" skippedBlast={}", n("skippedBlast")),
        _ => String::new(),
    };
    format!(
        "kept={} dropped={} downgraded={} noClaims={}{extra}{blast}",
        n("kept"),
        n("dropped"),
        n("downgraded"),
        n("noClaims"),
    )
}

// ---------------------------------------------------------------------------
// Тесты — ключевые случаи из test/pr-review.engine.test.mjs / eval
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_assignments_defaults_and_overrides() {
        let env = json!({});
        let a = parse_specialist_assignments(&env, &[]);
        assert_eq!(a[0]["role"], "security");
        assert_eq!(a[0]["model"], "glm-5.3-flash");
        assert_eq!(a[1]["model"], "kimi-k2.7-code");
        assert_eq!(a[2]["model"], "glm-5.3");

        // bulk override + per-role override побеждает bulk
        let env = json!({
            "MOA_SPECIALIST_MODELS": "security:sec-x, architecture : arch-y , bogus:no, no-colon",
            "MOA_SPECIALIST_CORRECTNESS_MODEL": " corr-z ",
        });
        let a = parse_specialist_assignments(&env, &[]);
        assert_eq!(a[0]["model"], "sec-x");
        assert_eq!(a[1]["model"], "corr-z");
        assert_eq!(a[2]["model"], "arch-y");

        // пустой bulk → дефолт; пустой per-role → не override
        let env = json!({ "MOA_SPECIALIST_MODELS": "", "MOA_SPECIALIST_SECURITY_MODEL": "  " });
        let a = parse_specialist_assignments(&env, &[]);
        assert_eq!(a[0]["model"], "glm-5.3-flash");
    }

    #[test]
    fn decide_refutation_rules() {
        let v = |status: &str| json!({ "role": "security", "status": status });
        assert_eq!(decide_refutation(&[])["action"], "keep");
        assert_eq!(
            decide_refutation(&[v("rejected"), v("rejected")])["action"],
            "drop"
        );
        assert_eq!(
            decide_refutation(&[v("rejected"), v("verified")])["action"],
            "keep"
        );
        assert_eq!(
            decide_refutation(&[json!({"status": "downgraded", "severity": "minor"})])["action"],
            "downgrade"
        );
        assert_eq!(
            decide_refutation(&[
                json!({"status": "downgraded", "severity": "nit"}),
                json!({"status": "downgraded", "severity": "minor"}),
            ])["severity"],
            "nit"
        );
        assert_eq!(decide_refutation(&[v("pending")])["action"], "keep");
    }

    fn f(sev: &str, extra: serde_json::Value) -> Value {
        let mut m = Map::new();
        m.insert("file".into(), json!("a.ts"));
        m.insert("line".into(), json!(1));
        m.insert("severity".into(), json!(sev));
        m.insert("title".into(), json!("t"));
        if let Some(o) = extra.as_object() {
            for (k, v) in o {
                m.insert(k.clone(), v.clone());
            }
        }
        Value::Object(m)
    }

    #[test]
    fn apply_refutation_drop_downgrade_keep_strict() {
        let findings = vec![
            f("critical", json!({"prescan": true})),
            f("major", json!({"advisorClaims": [
                {"role": "security", "status": "rejected"},
                {"role": "correctness", "status": "rejected"},
            ]})),
            f("major", json!({"advisorClaims": [
                {"role": "security", "status": "downgraded", "severity": "nit"},
                {"role": "architecture", "status": "rejected"},
            ]})),
            f("major", json!({"advisorClaims": [
                {"role": "security", "status": "verified"},
            ]})),
            f("critical", json!({})),
            f("minor", json!({})),
        ];
        let out = apply_refutation(&findings, true, &[json!("critical"), json!("major")], false);
        // prescan прошёл, drop удалён, downgrade/verify/strict kept, minor без claims kept
        assert_eq!(out.kept, vec![0, 2, 3, 4, 5]);
        assert_eq!(out.stats.dropped, 1);
        assert_eq!(out.stats.downgraded, 2); // downgrade + strict noClaims
        assert_eq!(out.stats.downgraded_no_claims, 1);
        assert_eq!(out.stats.kept, 1, "только verified инкрементирует kept");
        assert_eq!(out.stats.no_claims, 1);
        let m = |i: usize| out.mutations.iter().find(|m| m.index == i).unwrap();
        assert_eq!(m(4).severity, Some(json!("minor")));
        assert_eq!(m(4).refutation_status.as_deref(), Some("downgraded_no_claims"));
        assert_eq!(m(1).refutation_status.as_deref(), Some("dropped"));
        assert_eq!(m(2).severity, Some(json!("nit")));
        assert_eq!(m(3).refutation_status.as_deref(), Some("verified"));
        assert!(out.logs.iter().any(|l| l.contains("refutation strict")));
        assert!(out.logs.iter().any(|l| l.contains("refutation dropped")));
        assert!(out.logs.iter().any(|l| l.contains("refutation downgraded")));
    }

    #[test]
    fn apply_refutation_blast_skipped_only_with_flag() {
        // RFC 21: blast-находка без advisorClaims — не повод для strict-понижения.
        let blast = || f("major", json!({"source": "blast-radius", "prescan": false}));
        let tiers = [json!("critical"), json!("major")];

        let off = apply_refutation(&[blast()], true, &tiers, false);
        assert_eq!(off.mutations[0].severity, Some(json!("minor")), "флаг выключен — прежнее поведение");
        assert_eq!(off.mutations[0].refutation_status.as_deref(), Some("downgraded_no_claims"));
        assert_eq!(off.stats.skipped_blast, 0);

        let on = apply_refutation(&[blast(), f("major", json!({}))], true, &tiers, true);
        assert_eq!(on.kept, vec![0, 1]);
        let m = |i: usize| on.mutations.iter().find(|m| m.index == i).unwrap();
        assert_eq!(m(0).severity, None, "blast-major остаётся major");
        assert_eq!(m(0).refutation_status.as_deref(), Some("not_applicable"));
        assert_eq!(m(1).severity, Some(json!("minor")), "LLM-находка без claims понижается как раньше");
        assert_eq!(on.stats.skipped_blast, 1);
        assert_eq!(on.stats.downgraded_no_claims, 1);
        assert!(serde_json::to_value(&on.stats).unwrap().get("skippedBlast").is_some());
        assert!(serde_json::to_value(&off.stats).unwrap().get("skippedBlast").is_none(),
            "без пропусков поле не сериализуется — прежний JSON");
        assert_eq!(
            format_refutation_log(&serde_json::to_value(&on.stats).unwrap()),
            "kept=0 dropped=0 downgraded=1 noClaims=0 downgradedNoClaims=1 skippedBlast=1"
        );
    }

    #[test]
    fn apply_refutation_not_strict_keeps_block_tier_without_claims() {
        let findings = vec![f("critical", json!({}))];
        let out = apply_refutation(&findings, false, &[json!("critical")], false);
        assert_eq!(out.kept, vec![0]);
        assert_eq!(out.stats.no_claims, 1);
        assert_eq!(out.stats.downgraded, 0);
        assert!(out.mutations.is_empty());
    }

    #[test]
    fn merge_references_accumulates_chunks() {
        let chunk_results = vec![
            json!({"references": [
                {"role": "security", "model": "s", "text": "one"},
                {"role": "correctness", "model": "c", "text": "cee"},
            ]}),
            json!({"references": [
                {"role": "security", "model": "s", "text": "two"},
                {"role": "correctness", "model": "c", "error": "down"},
                {"model": "x", "text": "no role"},
            ]}),
            Value::Null,
            json!({}),
        ];
        let merged = merge_specialist_references(&chunk_results);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0]["role"], "security");
        assert_eq!(merged[0]["chunkCount"], 2.0);
        assert_eq!(merged[0]["text"], "one\n\n[chunk 2]\ntwo");
        assert_eq!(merged[1]["chunkCount"], 2.0);
        assert_eq!(merged[1]["text"], "cee");
        // r.error не затирает непустой prev.text — поле error не появляется
        assert!(merged[1].as_object().unwrap().get("error").is_none());
        assert_eq!(merged[2]["model"], "x");
        assert!(merged[2].as_object().unwrap().get("role").is_none(), "advisor — только в ключе, не в записи");
        assert_eq!(merged[2]["chunkCount"], 1.0);
    }

    #[test]
    fn format_log_matches_js_template() {
        let stats = json!({"kept": 1, "dropped": 2, "downgraded": 3, "noClaims": 4});
        assert_eq!(
            format_refutation_log(&stats),
            "kept=1 dropped=2 downgraded=3 noClaims=4"
        );
        let stats = json!({"kept": 1, "dropped": 0, "downgraded": 1, "noClaims": 0, "downgradedNoClaims": 1});
        assert_eq!(
            format_refutation_log(&stats),
            "kept=1 dropped=0 downgraded=1 noClaims=0 downgradedNoClaims=1"
        );
    }
}
