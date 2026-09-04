//! Порт lib/observability.mjs (чистые части) + lib/metrics.mjs (Фаза 3, шаг 6):
//! агрегация метрик из JSONL run-логов и текстовый отчёт.
//!
//! Паритет с JS (проверяется rust/parity/metrics.parity.mjs):
//!   - Number(x) || 0 — нечисловое/NaN → 0 (js_to_number из crate::findings);
//!   - стабильность: только конечные числа входят в среднее/тренд;
//!   - byModel — объект с порядком ключей «первое появление» (как
//!     Object.fromEntries(Map) — insertion order, preserve_order);
//!   - affectedFiles — отсортированный массив (union путей, не сумма длин);
//!   - skipReasons/модели сортируются по убыванию счётчика стабильной
//!     сортировкой (как Array#sort V8 — для равных счётчиков порядок вставки);
//!   - проценты — Math.round (положительные: half away from zero), avg —
//!     toFixed(1) (crate::report::js_to_fixed);
//!   - fs-запись логов и часы остаются в JS (createObservability не портирован).

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::findings::{js_to_number, js_to_string};
use crate::report::js_to_fixed;

const TITLE_MAX: usize = 120;
const STABILITY_RECENT_MAX: usize = 5;

/// countBySeverity (lib/observability.mjs): считаем только известные severities.
pub fn count_by_severity(findings: Option<&Value>) -> Value {
    let mut c = serde_json::Map::new();
    c.insert("critical".into(), json!(0));
    c.insert("major".into(), json!(0));
    c.insert("minor".into(), json!(0));
    c.insert("nit".into(), json!(0));
    if let Some(Value::Array(arr)) = findings {
        for f in arr {
            let Some(sev) = f.get("severity").and_then(Value::as_str) else { continue };
            if let Some(n) = c.get_mut(sev) {
                *n = json!(n.as_i64().unwrap_or(0) + 1);
            }
        }
    }
    Value::Object(c)
}

/// slimFindings (lib/observability.mjs): компактные findings для лога
/// (без body). undefined-поля (prescan) выпадают — как в JSON из JS.
pub fn slim_findings(findings: Option<&Value>, limit: usize) -> Value {
    let mut out = Vec::new();
    if let Some(Value::Array(arr)) = findings {
        for f in arr.iter().take(limit) {
            let mut m = serde_json::Map::new();
            if let Some(v) = f.get("file") {
                m.insert("file".into(), v.clone());
            }
            if let Some(v) = f.get("line") {
                m.insert("line".into(), v.clone());
            }
            if let Some(v) = f.get("severity") {
                m.insert("severity".into(), v.clone());
            }
            if let Some(v) = f.get("model") {
                m.insert("model".into(), v.clone());
            }
            let title = f.get("title").map(|x| crate::findings::js_to_string(x)).unwrap_or_default();
            m.insert("title".into(), Value::String(crate::report::js_slice_utf16(&title, TITLE_MAX)));
            if let Some(v @ Value::String(_)) = f.get("prescan") {
                m.insert("prescan".into(), v.clone());
            }
            out.push(Value::Object(m));
        }
    }
    Value::Array(out)
}

/// JSONL → records; битые строки пропускаем (parseJsonl из lib/metrics.mjs).
/// filter(Boolean) в JS отбрасывает и результат парсинга, если он falsy:
/// null / false / 0 / "" (на практике строки логов — объекты, но паритет
/// держим полным).
pub fn parse_jsonl(text: &str) -> Vec<Value> {
    fn js_truthy_parsed(v: &Value) -> bool {
        match v {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(false),
            Value::String(s) => !s.is_empty(),
            Value::Array(_) | Value::Object(_) => true,
        }
    }
    text.split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(js_truthy_parsed)
        .collect()
}

fn num_or_zero(v: Option<&Value>) -> f64 {
    match v.and_then(js_to_number) {
        Some(x) if x.is_finite() => x,
        _ => 0.0,
    }
}

/// JS Number→JSON: целые сериализуются без '.0' (как JSON.stringify в V8).
fn num_json(x: f64) -> Value {
    if x == x.trunc() && x.abs() < 9.0e15 {
        json!(x as i64)
    } else {
        json!(x)
    }
}

fn str_or(v: Option<&Value>, fallback: &str) -> String {
    match v {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => fallback.to_string(),
    }
}

/// aggregateMetrics (lib/metrics.mjs). Возвращает JSON-friendly структуру:
/// Set/Map уже материализованы (affectedFiles — sorted array, byModel — object).
pub fn aggregate_metrics(records: Option<&Value>) -> Result<Value> {
    let Some(Value::Array(records)) = records else {
        bail!("aggregateMetrics: ожидался массив records");
    };

    let mut runs = serde_json::Map::new();
    runs.insert("total".into(), json!(0));
    runs.insert("blocked".into(), json!(0));
    runs.insert("llmFailed".into(), json!(0));
    runs.insert("crashed".into(), json!(0));

    let mut router: Vec<(String, i64)> = Vec::new();
    let mut router_idx: HashMap<String, usize> = HashMap::new();

    let mut blast = serde_json::Map::new();
    blast.insert("plans".into(), json!(0));
    blast.insert("symbolsTotal".into(), json!(0));
    blast.insert("symbolsResolved".into(), json!(0));
    blast.insert("affectedNonempty".into(), json!(0));
    blast.insert("crossCommunity".into(), json!(0));
    let mut affected_files: HashSet<String> = HashSet::new();
    let mut graph_fresh = serde_json::Map::new();
    graph_fresh.insert("fresh".into(), json!(0));
    graph_fresh.insert("stale".into(), json!(0));
    graph_fresh.insert("unknown".into(), json!(0));
    let mut skip_reasons: Vec<(String, f64)> = Vec::new();
    let mut skip_idx: HashMap<String, usize> = HashMap::new();

    let mut llm = serde_json::Map::new();
    llm.insert("total".into(), json!(0));
    llm.insert("ok".into(), json!(0));
    llm.insert("prompt_tokens".into(), json!(0));
    llm.insert("completion_tokens".into(), json!(0));
    llm.insert("total_tokens".into(), json!(0));
    let mut by_model: Vec<(String, Value)> = Vec::new();
    let mut by_model_idx: HashMap<String, usize> = HashMap::new();

    let mut fp_filter_removed: f64 = 0.0;

    let mut verifier = serde_json::Map::new();
    for k in ["verified", "rejected", "downgraded", "unverified", "skipped", "errors"] {
        verifier.insert(k.to_string(), json!(0));
    }

    let mut stability_events: f64 = 0.0;
    let mut stability_runs: f64 = 0.0;
    let mut stability_sum: f64 = 0.0;
    let mut stability_kept: f64 = 0.0;
    let mut stability_added: f64 = 0.0;
    let mut stability_gone: f64 = 0.0;
    let mut stability_recent: VecDeque<f64> = VecDeque::new();

    let mut verdict_events: i64 = 0;
    let mut verdict_blocked: i64 = 0;
    let mut verdict_flips: i64 = 0;
    let mut verdict_repeated_shas: i64 = 0;
    let mut verdict_by_sha: HashMap<String, (bool, i64)> = HashMap::new();

    let mut time_budget_hits: i64 = 0;
    let mut time_budget_skipped: f64 = 0.0;

    let bump = |m: &mut serde_json::Map<String, Value>, key: &str, delta: f64| {
        let cur = m.get(key).and_then(js_to_number).unwrap_or(0.0);
        m.insert(key.to_string(), num_json(cur + delta));
    };

    for r in records {
        let Some(t) = r.get("type").and_then(Value::as_str) else { continue };
        match t {
            "run_end" => {
                bump(&mut runs, "total", 1.0);
                if crate::findings::js_truthy(r.get("wouldBlock").unwrap_or(&Value::Null)) {
                    bump(&mut runs, "blocked", 1.0);
                }
                if crate::findings::js_truthy(r.get("llmFailed").unwrap_or(&Value::Null)) {
                    bump(&mut runs, "llmFailed", 1.0);
                }
                if crate::findings::js_truthy(r.get("crashed").unwrap_or(&Value::Null)) {
                    bump(&mut runs, "crashed", 1.0);
                }
            }
            "router" => {
                if let Some(route) = r.get("route").and_then(Value::as_str) {
                    let i = *router_idx.entry(route.to_string()).or_insert_with(|| {
                        router.push((route.to_string(), 0));
                        router.len() - 1
                    });
                    router[i].1 += 1;
                }
            }
            "plan" => {
                let Some(b) = r.get("blast_radius") else { continue };
                // typeof blast === 'object' — массив тоже проходит (как в JS).
                if !(b.is_object() || b.is_array()) {
                    continue;
                }
                let b = if b.is_object() { b } else { &Value::Null };
                bump(&mut blast, "plans", 1.0);
                bump(&mut blast, "symbolsTotal", num_or_zero(b.get("symbols_total")));
                bump(&mut blast, "symbolsResolved", num_or_zero(b.get("symbols_resolved")));
                bump(&mut blast, "affectedNonempty", num_or_zero(b.get("affected_nonempty")));
                let cross = b.get("cross_community").and_then(js_to_number);
                if let Some(x) = cross {
                    if x.is_finite() && x > 0.0 {
                        bump(&mut blast, "crossCommunity", x);
                    }
                }
                if let Some(Value::Array(files)) = b.get("affected_files") {
                    for f in files {
                        if let Some(s) = f.as_str() {
                            if !s.is_empty() {
                                affected_files.insert(s.to_string());
                            }
                        }
                    }
                }
                let freshness = match b.get("graph_fresh") {
                    Some(Value::Bool(true)) => "fresh",
                    Some(Value::Bool(false)) => "stale",
                    _ => "unknown",
                };
                bump(&mut graph_fresh, freshness, 1.0);
                if let Some(Value::Object(reasons)) = b.get("skip_reasons") {
                    for (reason, count) in reasons {
                        let n = num_or_zero(Some(count));
                        let i = *skip_idx.entry(reason.to_string()).or_insert_with(|| {
                            skip_reasons.push((reason.to_string(), 0.0));
                            skip_reasons.len() - 1
                        });
                        skip_reasons[i].1 += n;
                    }
                }
            }
            "llm_call" => {
                bump(&mut llm, "total", 1.0);
                if crate::findings::js_truthy(r.get("ok").unwrap_or(&Value::Null)) {
                    bump(&mut llm, "ok", 1.0);
                }
                bump(&mut llm, "prompt_tokens", num_or_zero(r.get("prompt_tokens")));
                bump(&mut llm, "completion_tokens", num_or_zero(r.get("completion_tokens")));
                bump(&mut llm, "total_tokens", num_or_zero(r.get("total_tokens")));
                let key = format!(
                    "{}/{}",
                    str_or(r.get("callType"), "?"),
                    str_or(r.get("model"), "?")
                );
                let i = *by_model_idx.entry(key.clone()).or_insert_with(|| {
                    by_model.push((key, json!({ "calls": 0, "ok": 0, "ms": 0 })));
                    by_model.len() - 1
                });
                let entry = by_model[i].1.as_object_mut().unwrap();
                bump(entry, "calls", 1.0);
                if crate::findings::js_truthy(r.get("ok").unwrap_or(&Value::Null)) {
                    bump(entry, "ok", 1.0);
                }
                if let Some(ms) = r.get("duration_ms").and_then(js_to_number) {
                    if ms.is_finite() && ms >= 0.0 {
                        bump(entry, "ms", ms);
                    }
                }
            }
            "fp_filter" => {
                fp_filter_removed += num_or_zero(r.get("removed"));
            }
            "verifier" => {
                for k in ["verified", "rejected", "downgraded", "unverified"] {
                    bump(&mut verifier, k, num_or_zero(r.get(k)));
                }
            }
            "verifier_skip" => bump(&mut verifier, "skipped", 1.0),
            "verifier_error" => bump(&mut verifier, "errors", 1.0),
            "stability" => {
                stability_events += 1.0;
                stability_kept += num_or_zero(r.get("kept"));
                stability_added += num_or_zero(r.get("added"));
                stability_gone += num_or_zero(r.get("gone"));
                // Только конечные ЧИСЛА входят в среднее и тренд (typeof === 'number'
                // в JS: строка "0.8" и null исключаются, иначе null стал бы 0).
                if let Some(v) = r.get("stability").and_then(Value::as_f64) {
                    if v.is_finite() {
                        stability_runs += 1.0;
                        stability_sum += v;
                        stability_recent.push_back(v);
                        if stability_recent.len() > STABILITY_RECENT_MAX {
                            stability_recent.pop_front();
                        }
                    }
                }
            }
            "verdict" => {
                verdict_events += 1;
                let blocked = r.get("blocked") == Some(&Value::Bool(true));
                if blocked {
                    verdict_blocked += 1;
                }
                let sha = match r.get("sha") {
                    Some(v) if crate::findings::js_truthy(v) => crate::findings::js_to_string(v),
                    _ => String::new(),
                };
                if sha.is_empty() {
                    continue;
                }
                match verdict_by_sha.get_mut(&sha) {
                    None => {
                        verdict_by_sha.insert(sha, (blocked, 1));
                    }
                    Some(seen) => {
                        seen.1 += 1;
                        // Коммит попадает в знаменатель детерминизма ровно один раз — на втором прогоне.
                        if seen.1 == 2 {
                            verdict_repeated_shas += 1;
                        }
                        if seen.0 != blocked {
                            verdict_flips += 1;
                        }
                        seen.0 = blocked;
                    }
                }
            }
            "time_budget" => {
                time_budget_hits += 1;
                time_budget_skipped += num_or_zero(r.get("skippedChunks"));
            }
            _ => {}
        }
    }

    // JSON-friendly материализация в порядке ключей JS-объекта.
    let mut affected: Vec<String> = affected_files.into_iter().collect();
    sort_utf16(&mut affected);
    blast.insert(
        "affectedFiles".into(),
        Value::Array(affected.into_iter().map(Value::String).collect()),
    );
    blast.insert("graphFresh".into(), Value::Object(graph_fresh));
    let mut reasons_obj = serde_json::Map::new();
    for (k, v) in &skip_reasons {
        reasons_obj.insert(k.clone(), num_json(*v));
    }
    blast.insert("skipReasons".into(), Value::Object(reasons_obj));

    let mut by_model_obj = serde_json::Map::new();
    for (k, v) in by_model {
        by_model_obj.insert(k, v);
    }
    llm.insert("byModel".into(), Value::Object(by_model_obj));

    let stability = json!({
        "events": num_json(stability_events),
        "runs": num_json(stability_runs),
        "sum": num_json(stability_sum),
        "kept": num_json(stability_kept),
        "added": num_json(stability_added),
        "gone": num_json(stability_gone),
        "recent": stability_recent.iter().copied().map(num_json).collect::<Vec<Value>>(),
    });

    let verdict = json!({
        "events": verdict_events,
        "blocked": verdict_blocked,
        "flips": verdict_flips,
        "repeatedShas": verdict_repeated_shas,
    });

    let time_budget = json!({
        "hits": time_budget_hits,
        "skippedChunks": num_json(time_budget_skipped),
    });

    let mut router_obj = serde_json::Map::new();
    for (k, v) in router {
        router_obj.insert(k, json!(v));
    }

    Ok(json!({
        "runs": Value::Object(runs),
        "router": Value::Object(router_obj),
        "blastRadius": blast,
        "llm": llm,
        "fpFilterRemoved": num_json(fp_filter_removed),
        "verifier": verifier,
        "stability": stability,
        "verdict": verdict,
        "timeBudget": time_budget,
    }))
}

/// Дефолтный Array#sort V8: сравнение по UTF-16 code units.
fn sort_utf16(values: &mut [String]) {
    values.sort_by(|a, b| a.encode_utf16().collect::<Vec<u16>>().cmp(&b.encode_utf16().collect::<Vec<u16>>()));
}

/// Math.round для положительных значений — half away from zero (как V8).
fn js_round(x: f64) -> i64 {
    x.round() as i64
}

/// pct: ok/total в процентах или '—' (pct из lib/metrics.mjs).
fn pct(ok: f64, total: f64) -> String {
    if total != 0.0 {
        format!("{}%", js_round(ok / total * 100.0))
    } else {
        "—".to_string()
    }
}

/// formatMetricsReport (lib/metrics.mjs): текстовый отчёт по сводке.
/// Принимает agg после JSON-round-trip (byModel — объект, affectedFiles —
/// массив; поддержка Map на входе недостижима через JSON и не нужна).
pub fn format_metrics_report(agg: Option<&Value>, source: &str) -> Result<String> {
    let a = match agg {
        Some(Value::Object(o)) => o,
        _ => bail!("formatMetricsReport: ожидался объект agg"),
    };
    let get = |k: &str| a.get(k);
    let runs = get("runs").and_then(Value::as_object).cloned().unwrap_or_default();
    let num = |v: Option<&Value>| -> f64 { v.and_then(js_to_number).unwrap_or(0.0) };

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "=== pr-review metrics{} ===",
        if source.is_empty() { String::new() } else { format!(" ({source})") }
    ));
    lines.push(format!(
        "Прогоны: {} · блоков {} · llm-fail {} · crash {}",
        num(runs.get("total")),
        num(runs.get("blocked")),
        num(runs.get("llmFailed")),
        num(runs.get("crashed")),
    ));

    let router = get("router").and_then(Value::as_object).cloned().unwrap_or_default();
    let router_parts: Vec<String> = router
        .iter()
        .map(|(k, v)| format!("{} {}", k, js_to_string(v)))
        .collect();
    if !router_parts.is_empty() {
        lines.push(format!("Router: {}", router_parts.join(", ")));
    }

    let blast = get("blastRadius").and_then(Value::as_object).cloned().unwrap_or_default();
    let plans = num(blast.get("plans"));
    if plans != 0.0 {
        let graph_fresh = blast.get("graphFresh").and_then(Value::as_object).cloned().unwrap_or_default();
        lines.push(format!(
            "Graphify blast-radius: plans {} · symbols resolved {}/{} · non-empty {} · cross-community-edges(sum) {} · affected-files {} · graph fresh {}/{}",
            plans,
            num(blast.get("symbolsResolved")),
            num(blast.get("symbolsTotal")),
            num(blast.get("affectedNonempty")),
            num(blast.get("crossCommunity")),
            blast.get("affectedFiles").and_then(|v| v.as_array()).map(|x| x.len()).unwrap_or(0),
            num(graph_fresh.get("fresh")),
            plans,
        ));
        let stale = num(graph_fresh.get("stale"));
        let unknown = num(graph_fresh.get("unknown"));
        if stale != 0.0 || unknown != 0.0 {
            lines.push(format!("  Graph state: stale {}, unknown {}", stale, js_num_clean(unknown)));
        }
        let skip = blast.get("skipReasons").and_then(Value::as_object).cloned().unwrap_or_default();
        let mut reasons: Vec<(String, f64)> = skip.iter().map(|(k, v)| (k.clone(), num(Some(v)))).collect();
        reasons.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if !reasons.is_empty() {
            lines.push(format!(
                "  Empty/skip reasons: {}",
                reasons.iter().map(|(r, c)| format!("{} {}", r, js_num_clean(*c))).collect::<Vec<_>>().join(", ")
            ));
        }
    }

    let llm = get("llm").and_then(Value::as_object).cloned().unwrap_or_default();
    let llm_total = num(llm.get("total"));
    let llm_ok = num(llm.get("ok"));
    lines.push(format!("LLM вызовы: {}, ok {}", js_num_clean(llm_total), pct(llm_ok, llm_total)));
    if num(llm.get("total_tokens")) != 0.0 {
        lines.push(format!(
            "  Tokens: prompt {}, completion {}, total {}",
            js_num_clean(num(llm.get("prompt_tokens"))),
            js_num_clean(num(llm.get("completion_tokens"))),
            js_num_clean(num(llm.get("total_tokens"))),
        ));
    }
    let by_model = llm.get("byModel").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut models: Vec<(String, f64, f64, f64)> = by_model
        .iter()
        .map(|(k, v)| {
            let o = v.as_object().cloned().unwrap_or_default();
            (k.clone(), num(o.get("calls")), num(o.get("ok")), num(o.get("ms")))
        })
        .collect();
    models.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (key, calls, ok, ms) in models {
        let avg = if calls != 0.0 {
            js_to_fixed(ms / calls / 1000.0, 1)
        } else {
            "—".to_string()
        };
        lines.push(format!("  {}: {} вызов(ов), ok {}, avg {}с", key, js_num_clean(calls), pct(ok, calls), avg));
    }

    lines.push(format!("FP-filter: снято {} находок", js_num_clean(num(get("fpFilterRemoved")))));

    let verifier = get("verifier").and_then(Value::as_object).cloned().unwrap_or_default();
    lines.push(format!(
        "Verifier: verified {}, rejected {}, downgraded {}, unverified {}, skipped {}, errors {}",
        js_num_clean(num(verifier.get("verified"))),
        js_num_clean(num(verifier.get("rejected"))),
        js_num_clean(num(verifier.get("downgraded"))),
        js_num_clean(num(verifier.get("unverified"))),
        js_num_clean(num(verifier.get("skipped"))),
        js_num_clean(num(verifier.get("errors"))),
    ));

    let stability = get("stability").and_then(Value::as_object).cloned().unwrap_or_default();
    let stability_events = num(stability.get("events"));
    if stability_events != 0.0 {
        let runs_with_value = num(stability.get("runs"));
        let avg = if runs_with_value != 0.0 {
            format!("{}%", js_round(num(stability.get("sum")) / runs_with_value * 100.0))
        } else {
            "—".to_string()
        };
        let recent: Vec<f64> = stability
            .get("recent")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(js_to_number).collect())
            .unwrap_or_default();
        let trend = if !recent.is_empty() {
            format!(
                " · тренд {}",
                recent.iter().map(|v| format!("{}%", js_round(v * 100.0))).collect::<Vec<_>>().join(" → ")
            )
        } else {
            String::new()
        };
        lines.push(format!(
            "Стабильность находок: среднее {} по {}/{} прогон(ам) · kept {} / added {} / gone {} (точки кода){}",
            avg,
            js_num_clean(runs_with_value),
            js_num_clean(stability_events),
            js_num_clean(num(stability.get("kept"))),
            js_num_clean(num(stability.get("added"))),
            js_num_clean(num(stability.get("gone"))),
            trend,
        ));
    }

    let verdict = get("verdict").and_then(Value::as_object).cloned().unwrap_or_default();
    let verdict_events = num(verdict.get("events"));
    if verdict_events != 0.0 {
        let blocked = num(verdict.get("blocked"));
        let repeated = num(verdict.get("repeatedShas"));
        let flip_note = if repeated != 0.0 {
            format!(
                "смен вердикта на одном sha: {} (коммитов с ≥2 прогонами: {})",
                js_num_clean(num(verdict.get("flips"))),
                js_num_clean(repeated),
            )
        } else {
            "повторных прогонов на одном sha не было — детерминизм не измерен".to_string()
        };
        lines.push(format!(
            "Вердикт гейта: блокировали {}/{} прогон(ов) ({}) · {}",
            js_num_clean(blocked),
            js_num_clean(verdict_events),
            pct(blocked, verdict_events),
            flip_note,
        ));
    }

    let time_budget = get("timeBudget").and_then(Value::as_object).cloned().unwrap_or_default();
    let tb_hits = num(time_budget.get("hits"));
    if tb_hits != 0.0 {
        lines.push(format!(
            "⏱ Тайм-бюджет: {} прогон(ов) с пропусками, чанков пропущено {}",
            js_num_clean(tb_hits),
            js_num_clean(num(time_budget.get("skippedChunks"))),
        ));
    }
    Ok(lines.join("\n"))
}

/// Числа в отчёте печатаются как целые без '.0' (JS Number→string в шаблонах).
fn js_num_clean(x: f64) -> String {
    if x == x.trunc() && x.abs() < 1e21 {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

#[cfg(test)]
mod tests {
    const MAX_SLIM: usize = 40;
    use super::*;

    #[test]
    fn count_by_severity_known_only() {
        let c = count_by_severity(Some(&json!([
            { "severity": "critical" }, { "severity": "major" }, { "severity": "major" },
            { "severity": "nit" }, { "severity": "unknown" }, {},
        ])));
        assert_eq!(c, json!({ "critical": 1, "major": 2, "minor": 0, "nit": 1 }));
        assert_eq!(count_by_severity(Some(&json!([]))), json!({ "critical": 0, "major": 0, "minor": 0, "nit": 0 }));
    }

    #[test]
    fn slim_findings_caps_and_drops_body() {
        let slim = slim_findings(Some(&json!([
            {
                "file": "a.ts", "line": 1, "severity": "major", "model": "m1",
                "title": "x".repeat(200), "body": "secret", "prescan": "eslint",
            },
            { "file": "b.ts", "line": 2, "severity": "nit", "title": "t" },
        ])), MAX_SLIM);
        let arr = slim.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["title"].as_str().unwrap().len(), 120);
        assert!(!arr[0].as_object().unwrap().contains_key("body"));
        assert_eq!(arr[0]["prescan"], json!("eslint"));
        assert!(!arr[1].as_object().unwrap().contains_key("prescan"));
        // лимит
        let many: Vec<Value> = (0..50).map(|i| json!({ "file": "f", "line": i })).collect();
        assert_eq!(slim_findings(Some(&json!(many)), MAX_SLIM).as_array().unwrap().len(), 40);
    }

    #[test]
    fn parse_jsonl_skips_broken() {
        let recs = parse_jsonl("{\"a\":1}\n\nбитая\n{\"b\":2}\n");
        assert_eq!(recs, vec![json!({ "a": 1 }), json!({ "b": 2 })]);
    }

    #[test]
    fn aggregate_sums_all_event_types() {
        let records = parse_jsonl(
            r#"{"type":"run_end","wouldBlock":true,"llmFailed":true}
{"type":"run_end","wouldBlock":false,"crashed":true}
{"type":"router","route":"full"}
{"type":"router","route":"full"}
{"type":"router","route":"light"}
{"type":"plan","blast_radius":{"symbols_total":10,"symbols_resolved":5,"affected_nonempty":2,"cross_community":1,"affected_files":["b/a.ts","a/c.ts"],"graph_fresh":true,"skip_reasons":{"no_dependents":2}}}
{"type":"plan","blast_radius":{"symbols_total":4,"symbols_resolved":0,"affected_files":[],"graph_fresh":false,"skip_reasons":{"graph_stale":1,"no_dependents":1}}}
{"type":"llm_call","ok":true,"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"callType":"json","model":"m1","duration_ms":1200}
{"type":"llm_call","ok":false,"callType":"json","model":"m1","duration_ms":-5}
{"type":"llm_call","ok":true,"callType":"prose","model":"m2","duration_ms":"abc"}
{"type":"fp_filter","removed":3}
{"type":"verifier","verified":2,"rejected":1,"downgraded":1,"unverified":4}
{"type":"verifier_skip"}
{"type":"verifier_error"}
{"type":"stability","kept":10,"added":1,"gone":2,"stability":0.8}
{"type":"stability","kept":8,"added":0,"gone":0,"stability":null}
{"type":"stability","kept":9,"added":1,"gone":1,"stability":"bad"}
{"type":"verdict","blocked":true,"sha":"sha1"}
{"type":"verdict","blocked":false,"sha":"sha1"}
{"type":"verdict","blocked":true,"sha":"sha1"}
{"type":"verdict","blocked":"yes","sha":"sha2"}
{"type":"time_budget","skippedChunks":7}"#,
        );
        let agg = aggregate_metrics(Some(&json!(records))).unwrap();
        assert_eq!(agg["runs"], json!({ "total": 2, "blocked": 1, "llmFailed": 1, "crashed": 1 }));
        assert_eq!(agg["router"], json!({ "full": 2, "light": 1 }));
        let blast = &agg["blastRadius"];
        assert_eq!(blast["plans"], json!(2));
        assert_eq!(blast["symbolsTotal"], json!(14));
        assert_eq!(blast["symbolsResolved"], json!(5));
        assert_eq!(blast["crossCommunity"], json!(1));
        assert_eq!(blast["affectedFiles"], json!(["a/c.ts", "b/a.ts"]));
        assert_eq!(blast["graphFresh"], json!({ "fresh": 1, "stale": 1, "unknown": 0 }));
        assert_eq!(blast["skipReasons"], json!({ "no_dependents": 3, "graph_stale": 1 }));
        let llm = &agg["llm"];
        assert_eq!(llm["total"], json!(3));
        assert_eq!(llm["ok"], json!(2));
        assert_eq!(llm["total_tokens"], json!(150));
        assert_eq!(llm["byModel"]["json/m1"], json!({ "calls": 2, "ok": 1, "ms": 1200 }));
        assert_eq!(llm["byModel"]["prose/m2"], json!({ "calls": 1, "ok": 1, "ms": 0 }));
        assert_eq!(agg["fpFilterRemoved"], json!(3));
        assert_eq!(agg["verifier"], json!({ "verified": 2, "rejected": 1, "downgraded": 1, "unverified": 4, "skipped": 1, "errors": 1 }));
        // stability: среднее только по конечным числам (1 прогон из 3)
        assert_eq!(agg["stability"]["events"], json!(3));
        assert_eq!(agg["stability"]["runs"], json!(1));
        assert_eq!(agg["stability"]["sum"], json!(0.8));
        assert_eq!(agg["stability"]["recent"], json!([0.8]));
        // verdict: sha1 прогнался 3 раза → 1 повторный sha, 2 смены; blocked:"yes" ≠ true
        // (не-булево не считается блокировкой и не даёт смены на первом прогоне sha2).
        assert_eq!(agg["verdict"], json!({ "events": 4, "blocked": 2, "flips": 2, "repeatedShas": 1 }));
        assert_eq!(agg["timeBudget"], json!({ "hits": 1, "skippedChunks": 7 }));
    }

    #[test]
    fn format_report_snapshot() {
        let records = parse_jsonl(
            r#"{"type":"run_end","wouldBlock":true}
{"type":"llm_call","ok":true,"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"callType":"json","model":"m1","duration_ms":1500}
{"type":"stability","kept":5,"added":1,"gone":1,"stability":0.8333333}
{"type":"verdict","blocked":true,"sha":"a"}
{"type":"verdict","blocked":true,"sha":"a"}"#,
        );
        let agg = aggregate_metrics(Some(&json!(records))).unwrap();
        let report = format_metrics_report(Some(&agg), "runs.jsonl").unwrap();
        assert!(report.starts_with("=== pr-review metrics (runs.jsonl) ===\n"));
        assert!(report.contains("Прогоны: 1 · блоков 1 · llm-fail 0 · crash 0"));
        assert!(report.contains("LLM вызовы: 1, ok 100%"));
        assert!(report.contains("Tokens: prompt 10, completion 5, total 15"));
        assert!(report.contains("  json/m1: 1 вызов(ов), ok 100%, avg 1.5с"));
        assert!(report.contains("Стабильность находок: среднее 83% по 1/1 прогон(ам) · kept 5 / added 1 / gone 1 (точки кода) · тренд 83%"));
        assert!(report.contains("Вердикт гейта: блокировали 2/2 прогон(ов) (100%) · смен вердикта на одном sha: 0 (коммитов с ≥2 прогонами: 1)"));
        let report_no_src = format_metrics_report(Some(&agg), "").unwrap();
        assert!(report_no_src.starts_with("=== pr-review metrics ===\n"));
    }
}
