//! Порт lib/verifier.mjs (Фаза 3, шаг 3): чистые части Pass B —
//! selectVerifierCandidates (выбор кандидатов critical/major + advisory) и
//! applyVerifierVerdicts (prove/downgrade: вердикты verifier'а к находкам).
//!
//! LLM-runner (createVerifierRunner) намеренно остаётся в JS: он ходит через
//! инжектимый ollama (llm-бинарь Фазы 3, шаг 1) и инжектимый prompt — DI-моки
//! тестов/пайплайна должны остаться внутри процесса Node.
//!
//! Паритет с JS, включая:
//! - SEV_ORDER: critical 0, major 1, minor 2, nit 3, прочее 9; сортировка
//!   стабильная (Rust sort_by stable, как Array.prototype.sort в V8);
//! - dedup advisory-кандидатов — по идентичности объекта; в JSON-контракте
//!   это индексы в массиве findings (обёртка сопоставляет по ссылке, а при
//!   объекте вне массива деградирует на чистый JS);
//! - вердикты: keyed map по candidateIndex (последний побеждает), только
//!   целочисленные индексы; отсутствующий/ложный decision → missing
//!   (fail-open: находка остаётся; strictUnverified → minor + unverified);
//! - downgrade: severity minor/nit как есть, иначе minor;
//! - мутации НЕ применяются к входным находкам: операция возвращает
//!   kept-индексы, мутации по индексу и строки лога — JS-обёртка накладывает
//!   на исходные объекты, сохраняя семантику разделяемых ссылок пайплайна.

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

use crate::findings::{js_to_string, js_truthy};
use crate::llm::{ChatJsonOpts, LlmApi};
use crate::prompt::{self, PromptCtx};
use crate::traits::VerifyContext;

/// lib/verifier.mjs SEV_ORDER (прочее/отсутствующее → 9).
fn sev_order(sev: Option<&Value>) -> i32 {
    match sev.and_then(Value::as_str) {
        Some("critical") => 0,
        Some("major") => 1,
        Some("minor") => 2,
        Some("nit") => 3,
        _ => 9,
    }
}

/// Порт selectVerifierCandidates. Возвращает ИНДЕКСЫ findings: blockers
/// (severity из blockOn, не prescan, стабильная сортировка по SEV_ORDER,
/// не более max) + advisory (не из blockers, не prescan, не более advisoryMax).
/// advisoryCandidates — индексы candidates-объектов в findings (обёртка
/// сопоставляет по ссылке).
pub fn select_verifier_candidates(
    findings: &[Value],
    block_on: &[Value],
    max: usize,
    advisory_candidates: &[usize],
    advisory_max: usize,
) -> Vec<usize> {
    let tiers: Vec<Value> = block_on.to_vec();
    let in_tiers = |f: &Value| {
        let sev = f.get("severity").unwrap_or(&Value::Null);
        tiers.iter().any(|t| t == sev)
    };
    let mut blockers: Vec<usize> = (0..findings.len())
        .filter(|i| in_tiers(&findings[*i]) && !js_truthy(findings[*i].get("prescan").unwrap_or(&Value::Null)))
        .collect();
    blockers.sort_by_key(|i| sev_order(findings[*i].get("severity")));
    blockers.truncate(max);

    if advisory_max == 0 || advisory_candidates.is_empty() {
        return blockers;
    }
    let seen: Vec<usize> = blockers.clone();
    let mut extra: Vec<usize> = advisory_candidates
        .iter()
        .copied()
        .filter(|i| !seen.contains(i) && !js_truthy(findings[*i].get("prescan").unwrap_or(&Value::Null)))
        .collect();
    extra.truncate(advisory_max);
    blockers.extend(extra);
    blockers
}

/// Мутация одной находки (накладывается обёрткой на исходный объект).
#[derive(Debug, Clone, PartialEq)]
pub struct VerdictMutation {
    /// Индекс в allFindings.
    pub index: usize,
    /// Поля для установки (severity/verifierStatus/verifierReason).
    pub set: Map<String, Value>,
}

/// Результат applyVerifierVerdicts (структура как у JS-возврата + kept-индексы).
#[derive(Debug, Clone)]
pub struct AppliedVerdicts {
    /// Индексы allFindings, оставшиеся после отсева rejected.
    pub kept: Vec<usize>,
    pub mutations: Vec<VerdictMutation>,
    pub stats: Map<String, Value>,
    /// Строки лога в порядке генерации (обёртка печатает их через log()).
    pub logs: Vec<String>,
}

/// `x || ''` — falsy → пустая строка.
fn or_empty(v: Option<&Value>) -> String {
    match v {
        Some(x) if js_truthy(x) => js_to_string(x),
        _ => String::new(),
    }
}

/// JS template literal `${obj.field}`: отсутствующий ключ → "undefined",
/// null → "null", дальше — String(v).
fn tmpl(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(x) => js_to_string(x),
    }
}

/// Порт applyVerifierVerdicts. candidates — ИНДЕКСЫ в allFindings (обёртка
/// сопоставляет объекты по ссылке; объект вне массива → деградация на JS).
pub fn apply_verifier_verdicts(
    all_findings: &[Value],
    candidate_indices: &[usize],
    verdicts: &[Value],
    strict_unverified: bool,
) -> AppliedVerdicts {
    // verdictMap: candidateIndex → verdict; последний побеждает, только
    // целочисленные индексы (Number.isInteger в JS).
    let mut verdict_map: std::collections::HashMap<usize, &Value> = std::collections::HashMap::new();
    for v in verdicts {
        let Some(idx) = v.get("candidateIndex").and_then(Value::as_i64) else { continue };
        if idx < 0 || idx as usize >= candidate_indices.len() {
            continue;
        }
        verdict_map.insert(idx as usize, v);
    }

    let mut removed: Vec<usize> = Vec::new();
    let mut mutations: Vec<VerdictMutation> = Vec::new();
    let mut logs: Vec<String> = Vec::new();
    let mut stats = Map::new();
    for k in ["verified", "rejected", "downgraded", "missing", "unverified"] {
        stats.insert(k.to_string(), json!(0));
    }
    let bump = |stats: &mut Map<String, Value>, key: &str| {
        let n = stats.get(key).and_then(Value::as_i64).unwrap_or(0) + 1;
        stats.insert(key.to_string(), json!(n));
    };

    for (i, &fi) in candidate_indices.iter().enumerate() {
        let f = &all_findings[fi];
        let v = verdict_map.get(&i).copied();
        let decision_raw = v.and_then(|x| x.get("decision"));
        // JS: `const decision = v?.decision; if (!decision) → missing`. Truthy не-строка
        // (true, 1) — это НЕ missing: она не равна 'rejected'/'downgrade' и уходит
        // в ветку verified.
        if !decision_raw.map(js_truthy).unwrap_or(false) {
            bump(&mut stats, "missing");
            if strict_unverified {
                let mut set = Map::new();
                set.insert("severity".into(), json!("minor"));
                set.insert("verifierStatus".into(), json!("unverified"));
                set.insert(
                    "verifierReason".into(),
                    json!("verifier не подтвердил — понижено до minor (REVIEW_VERIFIER_STRICT)"),
                );
                mutations.push(VerdictMutation { index: fi, set });
                bump(&mut stats, "unverified");
                logs.push(format!(
                    "verifier unverified → minor: [{}:{}] «{}»",
                    tmpl(f.get("file")),
                    tmpl(f.get("line")),
                    tmpl(f.get("title")),
                ));
            }
            continue;
        }
        let reason = v.map(|x| or_empty(x.get("reason"))).unwrap_or_default();
        if decision_raw.and_then(Value::as_str) == Some("rejected") {
            removed.push(fi);
            let mut set = Map::new();
            set.insert("verifierStatus".into(), json!("rejected"));
            set.insert("verifierReason".into(), json!(reason));
            mutations.push(VerdictMutation { index: fi, set });
            bump(&mut stats, "rejected");
            let why = if reason.is_empty() { "без причины".to_string() } else { reason };
            logs.push(format!(
                "verifier rejected: [{}] {}:{} «{}» — {}",
                tmpl(f.get("severity")),
                tmpl(f.get("file")),
                tmpl(f.get("line")),
                tmpl(f.get("title")),
                why,
            ));
        } else if decision_raw.and_then(Value::as_str) == Some("downgrade") {
            let sev = v
                .and_then(|x| x.get("severity"))
                .and_then(Value::as_str)
                .filter(|s| *s == "minor" || *s == "nit")
                .unwrap_or("minor");
            let mut set = Map::new();
            set.insert("severity".into(), json!(sev));
            set.insert("verifierStatus".into(), json!("downgraded"));
            set.insert("verifierReason".into(), json!(reason));
            mutations.push(VerdictMutation { index: fi, set });
            bump(&mut stats, "downgraded");
            logs.push(format!(
                "verifier downgraded: «{}» → {}",
                tmpl(f.get("title")),
                sev,
            ));
        } else {
            let mut set = Map::new();
            set.insert("verifierStatus".into(), json!("verified"));
            set.insert("verifierReason".into(), json!(reason));
            mutations.push(VerdictMutation { index: fi, set });
            bump(&mut stats, "verified");
        }
    }

    let kept: Vec<usize> = (0..all_findings.len()).filter(|i| !removed.contains(i)).collect();
    AppliedVerdicts { kept, mutations, stats, logs }
}

// ---------------------------------------------------------------------------
// LLM-раннер Pass B (T12): порт createVerifierRunner (lib/verifier.mjs:166-206)
// ---------------------------------------------------------------------------

/// `s.slice(0, end)` по UTF-16 юнитам (не-BMP символ считается за два).
fn js_slice_utf16(s: &str, end: usize) -> String {
    let mut units = 0usize;
    let mut out = String::new();
    for c in s.chars() {
        let n = c.len_utf16();
        if units + n > end {
            break;
        }
        units += n;
        out.push(c);
    }
    out
}

/// Второй проход отдельной моделью: один batch-вызов на весь список кандидатов.
///
/// Отдельная структура, а не метод движка, потому что в JS это отдельная фабрика
/// (`createVerifierRunner`), которую `createReviewEngine` только подмешивает в
/// свой возврат (`lib/engine.mjs:412`). Граница та же: `RealEngine` строит раннер
/// на месте вызова, а сам раннер ничего не знает про MoA/consensus.
///
/// Все зависимости — по ссылке: раннер живёт ровно один вызов `verify_findings`.
pub struct VerifierRunner<'a> {
    /// `cfg.verifier.model` — модель Pass B, намеренно ВНЕ пула advisors.
    pub model: &'a str,
    /// `cfg.verifier.temperature`.
    pub temperature: Option<f64>,
    pub llm: &'a dyn LlmApi,
    pub prompt_ctx: &'a PromptCtx,
    /// `observability?.logEvent`; `None` — observability выключена.
    pub on_event: Option<&'a (dyn Fn(&str, &Value) + Send + Sync)>,
    /// Часы прогона (epoch-мс) — те же, что у движка; инъекция ради golden.
    pub now: &'a (dyn Fn() -> u64 + Send + Sync),
}

impl VerifierRunner<'_> {
    fn emit(&self, payload: Map<String, Value>) {
        if let Some(sink) = self.on_event {
            sink("llm_call", &Value::Object(payload));
        }
    }

    /// Порт `verifyFindings` (`lib/verifier.mjs:167-203`).
    ///
    /// Семантика, которую нельзя менять:
    /// - пустой список кандидатов → пустой ответ БЕЗ вызова LLM;
    /// - ответ без массива `verdicts` → пустой список, а не ошибка
    ///   (fail-open здесь, fail-closed решает `strictUnverified` снаружи);
    /// - сбой LLM → `Err`: стадию глушит пайплайн, а не движок.
    pub fn verify_findings(&self, candidates: &[Value], ctx: &VerifyContext) -> Result<Vec<Value>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let args = json!({
            "candidates": candidates,
            "annotated": ctx.annotated,
            "fileContents": ctx.file_contents,
            "blastBlocks": ctx.blast_blocks,
            "pr": ctx.pr,
            "changedFilesSection": ctx.changed_files_section,
        });
        let messages = prompt::build_verifier_messages(&args, self.prompt_ctx)?;
        let t0 = (self.now)();
        let opts = ChatJsonOpts { temperature: self.temperature, call_type: None };
        // Порядок ключей события — как в JS-литерале (lib/verifier.mjs:184-191).
        let mut ev = Map::new();
        ev.insert("callType".into(), json!("verifier"));
        ev.insert("model".into(), json!(self.model));
        ev.insert("candidates".into(), json!(candidates.len()));
        match self.llm.chat_json(&Value::Array(messages), self.model, &opts) {
            Ok(r) => {
                let verdicts = match r.value.get("verdicts") {
                    Some(Value::Array(a)) => a.clone(),
                    _ => Vec::new(), // Array.isArray(...) ? ... : []
                };
                ev.insert("duration_ms".into(), json!((self.now)().saturating_sub(t0)));
                ev.insert("ok".into(), json!(true));
                ev.insert("verdicts".into(), json!(verdicts.len()));
                self.emit(ev);
                Ok(verdicts)
            }
            Err(e) => {
                ev.insert("duration_ms".into(), json!((self.now)().saturating_sub(t0)));
                ev.insert("ok".into(), json!(false));
                // JS: `e.message?.slice(0, 200)` — без маскировки, в отличие от trackLlm.
                ev.insert("error".into(), json!(js_slice_utf16(&e.message, 200)));
                self.emit(ev);
                Err(anyhow!(e.message))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Тесты — ключевые случаи из test/pr-review.pipeline.test.mjs
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn findings() -> Vec<Value> {
        vec![
            json!({"file": "a.ts", "line": 1, "severity": "major", "title": "m1"}),
            json!({"file": "b.ts", "line": 2, "severity": "critical", "title": "c1", "prescan": true}),
            json!({"file": "c.ts", "line": 3, "severity": "critical", "title": "c2"}),
            json!({"file": "d.ts", "line": 4, "severity": "minor", "title": "n1"}),
        ]
    }

    #[test]
    fn select_tiers_sorts_critical_first_skips_prescan() {
        let f = findings();
        let idx = select_verifier_candidates(&f, &[json!("critical"), json!("major")], 12, &[], 0);
        assert_eq!(idx, vec![2, 0], "critical первым, prescan отсечён");
        // кап
        let idx = select_verifier_candidates(&f, &[json!("critical"), json!("major")], 1, &[], 0);
        assert_eq!(idx, vec![2]);
    }

    #[test]
    fn select_advisory_appends_without_dupes() {
        let f = findings();
        // advisory: minor (3) и critical (2, уже blocker) и prescan (1)
        let idx = select_verifier_candidates(
            &f,
            &[json!("critical"), json!("major")],
            12,
            &[3, 2, 1],
            5,
        );
        assert_eq!(idx, vec![2, 0, 3], "advisory без повторов blockers и без prescan");
        // advisoryMax = 0 → только blockers
        let idx = select_verifier_candidates(&f, &[json!("critical"), json!("major")], 12, &[3], 0);
        assert_eq!(idx, vec![2, 0]);
    }

    #[test]
    fn apply_verified_rejected_downgraded_missing() {
        let all = vec![
            json!({"file": "a.ts", "line": 1, "severity": "critical", "title": "keep"}),
            json!({"file": "b.ts", "line": 2, "severity": "major", "title": "drop"}),
            json!({"file": "c.ts", "line": 3, "severity": "critical", "title": "down"}),
            json!({"file": "d.ts", "line": 4, "severity": "major", "title": "miss"}),
        ];
        let verdicts = vec![
            json!({"candidateIndex": 0, "decision": "verified", "reason": "ок"}),
            json!({"candidateIndex": 1, "decision": "rejected", "reason": "ложняк"}),
            json!({"candidateIndex": 2, "decision": "downgrade", "reason": "мелочь", "severity": "nit"}),
            // кандидат 3 без вердикта → missing, fail-open
        ];
        let out = apply_verifier_verdicts(&all, &[0, 1, 2, 3], &verdicts, false);
        assert_eq!(out.kept, vec![0, 2, 3], "rejected вычеркнут");
        assert_eq!(out.stats["verified"], 1);
        assert_eq!(out.stats["rejected"], 1);
        assert_eq!(out.stats["downgraded"], 1);
        assert_eq!(out.stats["missing"], 1);
        assert_eq!(out.stats["unverified"], 0);
        // мутации по индексам
        let m = &out.mutations[0];
        assert_eq!(m.index, 0);
        assert_eq!(m.set["verifierStatus"], "verified");
        let m = &out.mutations[2];
        assert_eq!(m.index, 2);
        assert_eq!(m.set["severity"], "nit");
        assert_eq!(m.set["verifierStatus"], "downgraded");
        assert_eq!(
            out.logs,
            vec![
                "verifier rejected: [major] b.ts:2 «drop» — ложняк",
                "verifier downgraded: «down» → nit",
            ]
        );
    }

    #[test]
    fn apply_strict_missing_demotes_to_minor() {
        let all = vec![
            json!({"file": "a.ts", "line": 1, "severity": "critical", "title": "x"}),
            json!({"file": "b.ts", "line": 2, "severity": "major", "title": "y"}),
        ];
        let verdicts = vec![json!({"candidateIndex": 1, "decision": "verified"})];
        let out = apply_verifier_verdicts(&all, &[0, 1], &verdicts, true);
        assert_eq!(out.kept, vec![0, 1]);
        assert_eq!(out.stats["missing"], 1);
        assert_eq!(out.stats["unverified"], 1);
        let m = &out.mutations[0];
        assert_eq!(m.index, 0);
        assert_eq!(m.set["severity"], "minor");
        assert_eq!(m.set["verifierStatus"], "unverified");
        assert_eq!(
            out.logs[0],
            "verifier unverified → minor: [a.ts:1] «x»"
        );
        // без strict — missing не трогает находку
        let out = apply_verifier_verdicts(&all, &[0, 1], &verdicts, false);
        assert_eq!(out.mutations.len(), 1, "только verified-кандидат");
        assert_eq!(out.stats["unverified"], 0);
    }

    #[test]
    fn apply_downgrade_severity_defaults_to_minor() {
        let all = vec![json!({"file": "a.ts", "line": 1, "severity": "critical", "title": "x"})];
        // severity не minor/nit → minor
        let out = apply_verifier_verdicts(&all, &[0], &[json!({"candidateIndex": 0, "decision": "downgrade", "severity": "major"})], false);
        assert_eq!(out.mutations[0].set["severity"], "minor");
        // severity отсутствует → minor
        let out = apply_verifier_verdicts(&all, &[0], &[json!({"candidateIndex": 0, "decision": "downgrade"})], false);
        assert_eq!(out.mutations[0].set["severity"], "minor");
    }

    #[test]
    fn apply_last_verdict_wins_and_ignores_non_integer() {
        let all = vec![json!({"file": "a.ts", "line": 1, "severity": "critical", "title": "x"})];
        let verdicts = vec![
            json!({"candidateIndex": 0, "decision": "rejected"}),
            json!({"candidateIndex": 0, "decision": "verified", "reason": "последний"}),
            json!({"candidateIndex": 0.5, "decision": "rejected"}),
        ];
        let out = apply_verifier_verdicts(&all, &[0], &verdicts, false);
        assert_eq!(out.kept, vec![0]);
        assert_eq!(out.mutations[0].set["verifierStatus"], "verified");
        assert_eq!(out.mutations[0].set["verifierReason"], "последний");
    }
}
