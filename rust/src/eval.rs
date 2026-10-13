//! Порт lib/eval.mjs: eval-харнес golden-фикстур без LLM (RFC 20, T28).
//!
//! Каждая фикстура (`fixtures/<id>/`, загрузка — [`crate::fixtures`]) прогоняется через
//! post-processing гейта ровно в порядке JS: FP-фильтр (с FP-семейством 7 по
//! `repo-files.txt`) → strict-refutation (если у находок есть `advisorClaims`) →
//! verifier по готовым вердиктам (`verifier.json`) → кластеризация и `resolveBlockers`.
//! Итог сверяется с `meta.json.expected`.
//!
//! Результаты — `serde_json::Value` с порядком ключей JS-объекта: `--json` печатает их
//! так же, как `JSON.stringify(…, null, 2)` в bin/pr-review-eval.mjs, и паритет сверяет
//! вывод байт в байт (`rust/parity/eval.parity.mjs`).
//!
//! Имя совпадает с бинарём-мостом rust/src/bin/eval.rs — это разные цели сборки
//! (lib-модуль и bin-таргет), конфликта нет; мост удаляется в cutover (T33).

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::absent_path::build_repo_path_index;
use crate::config::load_review_config_parts;
use crate::diff::parse_diff;
use crate::findings::{filter_false_positive_findings, js_to_string, js_truthy, resolve_blockers, Gate};
use crate::fixtures::{load_fixtures, FixtureCase};
use crate::fp_patterns::default_fp_patterns;
use crate::profile::{package_root, Profile};
use crate::report::js_json_stringify;
use crate::specialists::apply_refutation;
use crate::verifier::{apply_verifier_verdicts, select_verifier_candidates};

/// Сколько кандидатов verifier берёт в eval (литерал `12` в lib/eval.mjs).
const EVAL_VERIFIER_MAX: usize = 12;

/// `DEFAULT_FIXTURES_DIR`: `fixtures/` от корня пакета. Фикстуры dev-only и в бинарь
/// не вшиваются (RFC 20, решение 5) — у prebuilt-бинаря каталог задают `--fixtures`.
pub fn default_fixtures_dir() -> PathBuf {
    package_root().join("fixtures")
}

/// Итог прогона харнеса (`runEvalHarness`).
#[derive(Debug)]
pub struct EvalOutcome {
    pub results: Vec<Value>,
    pub metrics: Value,
    pub ok: bool,
    pub report: String,
}

/// `process.env` как `Map` строк — вход `load_review_config_parts`.
/// Не-UTF-8 переменные пропускаются (`std::env::vars` на них паникует).
pub fn process_env_map() -> Map<String, Value> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, Value::String(v.into_string().ok()?))))
        .collect()
}

/// `runEvalHarness`: конфиг (env + `REVIEW_MODE=consensus`, `OLLAMA_MODELS=m1,m2`) на
/// встроенном профиле — JS-харнес профиль не резолвит, `getProfile()` отдаёт дефолт.
pub fn run_eval_harness(fixtures_dir: &Path, env: &Map<String, Value>) -> EvalOutcome {
    let mut env = env.clone();
    env.insert("REVIEW_MODE".into(), json!("consensus"));
    env.insert("OLLAMA_MODELS".into(), json!("m1,m2"));
    let profile = Profile::default_builtin(&package_root());
    let cfg = load_review_config_parts(&env, &profile.config_input());
    let base_gate = cfg.get("gate").cloned().unwrap_or_else(|| json!({}));
    let prescan_cfg = cfg.get("prescan").cloned().unwrap_or(Value::Null);

    // readdir упал → [] (как catch в loadFixtures): ошибка чтения не роняет харнес.
    let fixtures = load_fixtures(fixtures_dir).unwrap_or_default();
    let results: Vec<Value> = fixtures
        .iter()
        .map(|f| {
            let mut r = run_fixture_case(f, &base_gate, &prescan_cfg);
            // r.expectedShouldBlock = f.expected.shouldBlock — undefined в JSON не попадает.
            if let (Some(obj), Some(v)) = (r.as_object_mut(), f.expected.get("shouldBlock")) {
                obj.insert("expectedShouldBlock".into(), v.clone());
            }
            r
        })
        .collect();
    let metrics = compute_eval_metrics(&results);
    let ok = metrics["failed"].as_u64() == Some(0);
    let report = format_eval_report(&results, &metrics);
    EvalOutcome { results, metrics, ok, report }
}

/// `{...baseGate, ...fixture.gateOverride}`: спред не-объекта ничего не добавляет.
fn merge_gate(base: &Value, over: &Value) -> Value {
    let mut out = base.as_object().cloned().unwrap_or_default();
    if let Some(o) = over.as_object() {
        for (k, v) in o {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

/// `x?.length` истинно: непустой массив или непустая строка.
fn has_length(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::String(s)) => !s.is_empty(),
        _ => false,
    }
}

/// Элементы `exp.titles`/`exp.noTitles` для `for…of` (строка итерируется по символам).
fn iter_items(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::String(s) => s.chars().map(|c| Value::String(c.to_string())).collect(),
        _ => Vec::new(),
    }
}

/// `x !== n` для числового поля контракта: только число с тем же значением равно.
fn strict_ne_count(v: &Value, n: usize) -> bool {
    v.as_f64() != Some(n as f64)
}

/// `runFixtureCase`: post-processing одной фикстуры и сверка с `expected`.
pub fn run_fixture_case(fixture: &FixtureCase, base_gate: &Value, prescan_cfg: &Value) -> Value {
    let gate_val = merge_gate(base_gate, &fixture.gate);
    let gate = Gate::from_value(&gate_val);
    let parsed = parse_diff(&fixture.diff);
    let changed_paths: Vec<String> = parsed.files.keys().cloned().collect();
    let repo_index = build_repo_path_index(fixture.repo_files.as_deref(), &changed_paths);

    let fp = filter_false_positive_findings(&fixture.findings, &changed_paths, default_fp_patterns(), repo_index.as_ref());
    let mut working: Vec<Value> = fp.kept.iter().map(|&i| fixture.findings[i].clone()).collect();

    let mut refutation_stats = Value::Null;
    let mut verifier_stats = Value::Null;
    if working.iter().any(|f| has_length(f.get("advisorClaims"))) {
        // applyRefutation({ blockOn: gate.blockOn }): undefined → дефолт параметра,
        // falsy → `blockOn || []`.
        let block_on: Vec<Value> = match gate_val.get("blockOn") {
            None => vec![json!("critical"), json!("major")],
            Some(Value::Array(a)) => a.clone(),
            Some(_) => Vec::new(),
        };
        let applied = apply_refutation(&working, true, &block_on);
        for m in &applied.mutations {
            if let Some(obj) = working[m.index].as_object_mut() {
                if let Some(sev) = &m.severity {
                    obj.insert("severity".into(), sev.clone());
                }
                if let Some(st) = &m.refutation_status {
                    obj.insert("refutationStatus".into(), json!(st));
                }
            }
        }
        working = applied.kept.iter().map(|&i| working[i].clone()).collect();
        refutation_stats = serde_json::to_value(&applied.stats).unwrap_or(Value::Null);
    }
    if let Some(verdicts) = fixture.verifier_verdicts.as_ref().filter(|v| !v.is_empty()) {
        let block_on: Vec<Value> = gate_val.get("blockOn").and_then(Value::as_array).cloned().unwrap_or_default();
        let candidates = select_verifier_candidates(&working, &block_on, EVAL_VERIFIER_MAX, &[], 0);
        let applied = apply_verifier_verdicts(&working, &candidates, verdicts, false);
        for m in &applied.mutations {
            if let Some(obj) = working[m.index].as_object_mut() {
                for (k, v) in &m.set {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        working = applied.kept.iter().map(|&i| working[i].clone()).collect();
        verifier_stats = Value::Object(applied.stats);
    }

    let resolved = resolve_blockers(&working, &gate, Some(prescan_cfg));
    for m in &resolved.mutations {
        if let Some(obj) = working[m.index].as_object_mut() {
            obj.insert("confidence".into(), json!(m.confidence));
            obj.insert("consensus".into(), json!(m.consensus));
            obj.insert("clusterModels".into(), Value::Array(m.cluster_models.clone()));
            obj.insert("confidenceBlock".into(), json!(m.confidence_block));
            obj.insert("consensusBlock".into(), json!(m.consensus_block));
        }
    }
    for &(i, b) in &resolved.prescan_mutations {
        if let Some(obj) = working[i].as_object_mut() {
            obj.insert("consensusBlock".into(), json!(b));
        }
    }
    let findings = &working;
    let blockers: Vec<&Value> = resolved.blockers.iter().map(|&i| &working[i]).collect();
    let would_block = resolved.would_block;

    let exp = &fixture.expected;
    let mut errors: Vec<String> = Vec::new();
    if let Some(v) = exp.get("shouldBlock") {
        if *v != Value::Bool(would_block) {
            errors.push(format!("shouldBlock: ожидали {}, получили {would_block}", js_to_string(v)));
        }
    }
    if let Some(v) = exp.get("findingCount") {
        if strict_ne_count(v, findings.len()) {
            errors.push(format!("findingCount: ожидали {}, получили {}", js_to_string(v), findings.len()));
        }
    }
    if let Some(v) = exp.get("blockerCount") {
        if strict_ne_count(v, blockers.len()) {
            errors.push(format!("blockerCount: ожидали {}, получили {}", js_to_string(v), blockers.len()));
        }
    }
    if let Some(titles) = exp.get("titles").filter(|v| has_length(Some(v))) {
        let got: Vec<&Value> = blockers.iter().filter_map(|b| b.get("title")).collect();
        for t in iter_items(titles) {
            if !got.contains(&&t) {
                errors.push(format!("нет блокера с title «{}»", js_to_string(&t)));
            }
        }
    }
    if let Some(titles) = exp.get("noTitles").filter(|v| has_length(Some(v))) {
        // Смотрим все оставшиеся findings, не только blockers: иначе FP-фильтр, который
        // понижает severity вместо удаления, оставляет кейс зелёным.
        let got: Vec<&Value> = findings.iter().filter_map(|f| f.get("title")).collect();
        for t in iter_items(titles) {
            if got.contains(&&t) {
                errors.push(format!("неожиданная находка «{}»", js_to_string(&t)));
            }
        }
    }

    let blockers_json: Vec<Value> = blockers
        .iter()
        .map(|b| {
            // Поля-undefined JSON.stringify опускает — копируем только присутствующие.
            let mut o = Map::new();
            for k in ["file", "line", "severity", "title", "confidence", "consensusBlock"] {
                if let Some(v) = b.get(k) {
                    o.insert(k.into(), v.clone());
                }
            }
            Value::Object(o)
        })
        .collect();

    let mut out = Map::new();
    out.insert("id".into(), json!(fixture.id));
    out.insert("description".into(), json!(fixture.description));
    out.insert("pass".into(), json!(errors.is_empty()));
    out.insert("errors".into(), json!(errors));
    out.insert("wouldBlock".into(), json!(would_block));
    out.insert("blockerCount".into(), json!(blockers.len()));
    out.insert("findingCount".into(), json!(findings.len()));
    out.insert("clusterCount".into(), json!(resolved.clusters.len()));
    out.insert("verifierStats".into(), verifier_stats);
    out.insert("refutationStats".into(), refutation_stats);
    out.insert("blockers".into(), Value::Array(blockers_json));
    Value::Object(out)
}

/// `computeEvalMetrics`: агрегат для CI-порога.
pub fn compute_eval_metrics(results: &[Value]) -> Value {
    let total = results.len();
    let pass = |r: &Value| r.get("pass") == Some(&Value::Bool(true));
    let passed = results.iter().filter(|r| pass(r)).count();
    let failed = total - passed;
    // expectedShouldBlock !== undefined: ключ присутствует (null тоже считается).
    let block_cases: Vec<&Value> = results.iter().filter(|r| r.get("expectedShouldBlock").is_some()).collect();
    let block_pass = block_cases.iter().filter(|r| pass(r)).count();
    let pass_rate = if total > 0 { passed as f64 / total as f64 } else { 1.0 };
    let precision_note = if block_cases.is_empty() {
        "n/a".to_string()
    } else {
        format!("block-cases pass: {block_pass}/{}", block_cases.len())
    };
    json!({
        "total": total,
        "passed": passed,
        "failed": failed,
        "passRate": pass_rate,
        "precisionNote": precision_note,
    })
}

/// `Number.prototype.toFixed(0)` для неотрицательного конечного x: половина — вверх
/// (Rust `{:.0}` округляет к чётному: 12.5 → "12", JS даёт "13").
fn to_fixed0(x: f64) -> String {
    format!("{}", (x + 0.5).floor() as i64)
}

/// `${v}` шаблонной строки: отсутствующее поле → "undefined".
fn tmpl(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(x) => js_to_string(x),
    }
}

/// `formatEvalReport`: человекочитаемый отчёт для stdout / CI log.
pub fn format_eval_report(results: &[Value], metrics: &Value) -> String {
    let num = |k: &str| metrics.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let mut lines = vec![
        "=== pr-review eval harness ===".to_string(),
        format!(
            "Итого: {}/{} passed ({}%)",
            tmpl(metrics.get("passed")),
            tmpl(metrics.get("total")),
            to_fixed0(num("passRate") * 100.0)
        ),
        tmpl(metrics.get("precisionNote")),
        String::new(),
    ];
    for r in results {
        let pass = r.get("pass") == Some(&Value::Bool(true));
        let mark = if pass { "✅" } else { "❌" };
        let desc = r.get("description").filter(|d| js_truthy(d));
        lines.push(match desc {
            Some(d) => format!("{mark} {} — {}", tmpl(r.get("id")), js_to_string(d)),
            None => format!("{mark} {}", tmpl(r.get("id"))),
        });
        if pass {
            lines.push(format!(
                "   → wouldBlock={}, blockers={}, findings={}",
                tmpl(r.get("wouldBlock")),
                tmpl(r.get("blockerCount")),
                tmpl(r.get("findingCount"))
            ));
            let blockers = r.get("blockers").and_then(Value::as_array).cloned().unwrap_or_default();
            if blockers.is_empty() {
                lines.push("   · блокеров нет (ожидаемо для non-block кейсов)".into());
            } else {
                for b in &blockers {
                    // `${b.confidence ?? '—'}`: null и undefined → прочерк.
                    let conf = match b.get("confidence") {
                        None | Some(Value::Null) => "—".to_string(),
                        Some(v) => js_to_string(v),
                    };
                    lines.push(format!(
                        "   · [{}] {}:{} conf={conf} «{}»",
                        tmpl(b.get("severity")),
                        tmpl(b.get("file")),
                        tmpl(b.get("line")),
                        tmpl(b.get("title"))
                    ));
                }
            }
            if let Some(v) = r.get("verifierStats").and_then(|s| s.get("rejected")).filter(|v| js_truthy(v)) {
                lines.push(format!("   · verifier отклонил {} FP", js_to_string(v)));
            }
            if let Some(v) = r.get("refutationStats").and_then(|s| s.get("dropped")).filter(|v| js_truthy(v)) {
                lines.push(format!("   · refutation dropped {} FP", js_to_string(v)));
            }
        } else {
            for e in r.get("errors").and_then(Value::as_array).into_iter().flatten() {
                lines.push(format!("   ✗ {}", js_to_string(e)));
            }
            lines.push(format!(
                "   факт: wouldBlock={}, blockers={}",
                tmpl(r.get("wouldBlock")),
                tmpl(r.get("blockerCount"))
            ));
        }
        lines.push(String::new());
    }
    let failed = metrics.get("failed").and_then(Value::as_u64).unwrap_or(0);
    if failed > 0 {
        lines.push(format!("⛔ {failed} fixture(s) FAILED — не меняйте модель/промпт без зелёного eval"));
    } else {
        lines.push("✅ все fixtures passed — гейт post-processing стабилен".into());
    }
    lines.join("\n")
}

/// `JSON.stringify(value, null, 2)`: скаляры и строки — через V8-совместимый
/// [`js_json_stringify`] (числа как в JS: `1`, а не `1.0`).
pub fn js_json_pretty(value: &Value) -> String {
    let mut out = String::new();
    write_pretty(value, 0, &mut out);
    out
}

fn write_pretty(value: &Value, depth: usize, out: &mut String) {
    let pad = |n: usize| "  ".repeat(n);
    match value {
        Value::Array(items) if !items.is_empty() => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&pad(depth + 1));
                write_pretty(item, depth + 1, out);
            }
            out.push('\n');
            out.push_str(&pad(depth));
            out.push(']');
        }
        Value::Object(map) if !map.is_empty() => {
            out.push_str("{\n");
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&pad(depth + 1));
                js_json_stringify(&Value::String(key.clone()), out);
                out.push_str(": ");
                write_pretty(item, depth + 1, out);
            }
            out.push('\n');
            out.push_str(&pad(depth));
            out.push('}');
        }
        // Скаляры и пустые контейнеры ("[]", "{}") одинаковы в компактной форме.
        other => js_json_stringify(other, out),
    }
}

/// Тело `--json`: `{ ok, metrics, results }`.
pub fn json_output(outcome: &EvalOutcome) -> String {
    js_json_pretty(&json!({
        "ok": outcome.ok,
        "metrics": outcome.metrics,
        "results": outcome.results,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn repo_fixtures() -> PathBuf {
        default_fixtures_dir()
    }

    #[test]
    fn all_repo_fixtures_pass() {
        let out = run_eval_harness(&repo_fixtures(), &Map::new());
        let failed: Vec<String> = out
            .results
            .iter()
            .filter(|r| r["pass"] != json!(true))
            .map(|r| format!("{}: {}", r["id"], r["errors"]))
            .collect();
        assert!(failed.is_empty(), "упавшие фикстуры: {failed:?}");
        assert!(out.ok);
        // Счётчик seed-кейсов держит и JS-тест `eval harness: все fixtures green`.
        assert_eq!(out.metrics["total"], json!(20));
        assert!(out.report.contains("✅ все fixtures passed"));
    }

    #[test]
    fn missing_dir_is_empty_and_ok() {
        let out = run_eval_harness(Path::new("/nonexistent/fixtures-t28"), &Map::new());
        assert!(out.ok);
        assert_eq!(out.metrics["passRate"], json!(1.0));
        assert_eq!(out.metrics["precisionNote"], json!("n/a"));
        assert!(out.report.contains("Итого: 0/0 passed (100%)"));
    }

    #[test]
    fn failing_expectation_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let case = dir.path().join("bad-case");
        fs::create_dir(&case).unwrap();
        fs::write(case.join("meta.json"), r#"{"id":"bad-case","expected":{"shouldBlock":true,"blockerCount":1,"titles":["нет такого"],"findingCount":5}}"#).unwrap();
        fs::write(case.join("diff.patch"), "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -0,0 +1,1 @@\n+x\n").unwrap();
        fs::write(
            case.join("findings.json"),
            r#"{"findings":[{"file":"a.ts","line":1,"severity":"minor","title":"мелочь","body":"b","model":"m1","confidence":0.9}]}"#,
        )
        .unwrap();
        let out = run_eval_harness(dir.path(), &Map::new());
        assert!(!out.ok);
        let r = &out.results[0];
        assert_eq!(r["pass"], json!(false));
        let errors: Vec<String> = r["errors"].as_array().unwrap().iter().map(|e| e.as_str().unwrap().to_string()).collect();
        assert_eq!(
            errors,
            vec![
                "shouldBlock: ожидали true, получили false",
                "findingCount: ожидали 5, получили 1",
                "blockerCount: ожидали 1, получили 0",
                "нет блокера с title «нет такого»",
            ]
        );
        assert_eq!(r["expectedShouldBlock"], json!(true));
        assert_eq!(out.metrics["precisionNote"], json!("block-cases pass: 0/1"));
        assert!(out.report.contains("⛔ 1 fixture(s) FAILED"));
        assert!(out.report.contains("   факт: wouldBlock=false, blockers=0"));
    }

    #[test]
    fn to_fixed0_rounds_half_up() {
        assert_eq!(to_fixed0(12.5), "13");
        assert_eq!(to_fixed0(93.75), "94");
        assert_eq!(to_fixed0(100.0), "100");
    }

    #[test]
    fn pretty_matches_json_stringify_shape() {
        let v = json!({"a": 1.0, "b": [], "c": {}, "d": [0.5, "x"], "e": null});
        assert_eq!(js_json_pretty(&v), "{\n  \"a\": 1,\n  \"b\": [],\n  \"c\": {},\n  \"d\": [\n    0.5,\n    \"x\"\n  ],\n  \"e\": null\n}");
    }
}
