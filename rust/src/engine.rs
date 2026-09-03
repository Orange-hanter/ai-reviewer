//! Порт чистых частей lib/engine.mjs (Фаза 3, шаг 2): скоуп prior findings
//! по чанку, демоут находок про невидимые чанку файлы, агрегация ответов
//! модели по чанкам (verdict-ранк + дедуп внутри модели).
//!
//! Оркестрация (параллелизм моделей×чанков, дедлайн-бюджет, fallback, MoA)
//! намеренно остаётся в JS-обёртке lib/engine.mjs:
//! - DI-моки тестов/пайплайна подменяют ollama/prompt — они должны остаться
//!   внутри процесса Node;
//! - адаптивные семафоры и троттлинг 429 живут в lib/llm.mjs (JS);
//! - дедлайн — Date.now() на стороне JS (бинарь одноразовый, у него нет
//!   общих часов с оркестратором).
//!
//! Паритет с JS, включая:
//! - chunkScope: состав чанка полон только если paths непуст И нет truthy
//!   unknownPaths; скоуп — Set исходных JSON-значений (строгое равенство);
//! - демоут: только findings с truthy file вне скоупа и severity !== 'nit';
//!   confidence = min(confidence ?? 0.5, 0.5); chunkIndex/demotedOutOfChunk
//!   дописываются ТОЛЬКО когда демоутилось > 0;
//! - aggregateModel: verdict — максимум VERDICT_RANK (ties → первый, как
//!   стабильный Array.sort в V8); summary — первое truthy; все части с
//!   ошибкой → error parts[0] || 'нет ответа', skipped только если ВСЕ части
//!   skipped; дедуп — findings::dedupe_same_model_findings (копии, как {...f}).

use serde_json::{json, Map, Value};

use crate::findings::{dedupe_same_model_findings, js_to_number, js_truthy};

/// lib/constants.mjs VERDICT_RANK.
fn verdict_rank(v: Option<&Value>) -> f64 {
    match v.and_then(Value::as_str) {
        Some("changes_requested") => 2.0,
        Some("comment") => 1.0,
        _ => 0.0, // approve и всё прочее (VERDICT_RANK[b] ?? 0)
    }
}

/// Состав чанка известен, только если paths непуст и нет unknownPaths
/// (chunk?.paths?.length && !chunk.unknownPaths — JS truthiness).
fn chunk_scope(chunk: &Value) -> Option<Vec<Value>> {
    let paths = chunk.get("paths")?.as_array()?;
    if paths.is_empty() || js_truthy(chunk.get("unknownPaths").unwrap_or(&Value::Null)) {
        return None;
    }
    Some(paths.clone())
}

/// `!f.file || scope.has(f.file)` — file falsy → виден всем; иначе строгое
/// равенство значения одному из путей скоупа.
fn file_in_scope(scope: &[Value], f: &Value) -> bool {
    let file = f.get("file").unwrap_or(&Value::Null);
    !js_truthy(file) || scope.iter().any(|p| p == file)
}

/// Порт scopePriors: prior findings только по файлам ЭТОГО чанка.
/// Скоуп неполон/пуст или priors пусты — возвращаем вход как есть (null в
/// JSON-контракте батча planChunks — JS-обёртка подставляет оригинал).
pub fn scope_priors(prior_findings: &[Value], chunk: &Value) -> Option<Vec<Value>> {
    let scope = chunk_scope(chunk)?;
    if prior_findings.is_empty() {
        return None; // вход возвращается без изменений
    }
    Some(
        prior_findings
            .iter()
            .filter(|f| file_in_scope(&scope, f))
            .cloned()
            .collect(),
    )
}

/// Порт demoteOutOfChunk: находка про файл, которого чанк не видит, понижается
/// до nit и метится (слепой блокер merge невозможен). Файл, видимый хотя бы
/// частично, не трогается. Возвращает КОПИЮ res при демоуте > 0, иначе клон
/// входа (JS мутировал бы res — на результаты агрегации это не влияет, а
/// JSON-контракт не даёт отличить «та же ссылка» от копии).
pub fn demote_out_of_chunk(res: &Value, chunk: &Value, chunk_index: usize) -> Value {
    let Some(scope) = chunk_scope(chunk) else {
        return res.clone();
    };
    let findings = match res.get("findings").and_then(Value::as_array) {
        Some(f) if !f.is_empty() => f,
        _ => return res.clone(),
    };
    let mut demoted = 0usize;
    let mut out = Vec::with_capacity(findings.len());
    for f in findings {
        let sev_nit = f.get("severity").and_then(Value::as_str) == Some("nit");
        if file_in_scope(&scope, f) || sev_nit {
            out.push(f.clone());
            continue;
        }
        demoted += 1;
        let mut m = f.as_object().cloned().unwrap_or_default();
        m.insert("severity".into(), json!("nit"));
        m.insert("outOfChunk".into(), json!(true));
        // Math.min(f.confidence ?? 0.5, 0.5); после normalizeFinding confidence
        // всегда число 0..1 — не-число (Math.min → NaN) недостижимо.
        let conf = match f.get("confidence") {
            Some(c) => match js_to_number(c) {
                Some(n) if n.is_finite() => n.min(0.5),
                _ => 0.5,
            },
            None => 0.5,
        };
        m.insert("confidence".into(), json!(conf));
        out.push(Value::Object(m));
    }
    if demoted == 0 {
        return res.clone();
    }
    let mut r = res.as_object().cloned().unwrap_or_default();
    r.insert("findings".into(), Value::Array(out));
    // (res.demotedOutOfChunk || 0) + demoted — поле ставит сама эта функция,
    // поэтому число всегда (не-число в JS дало бы string-конкат — недостижимо).
    let prev = match res.get("demotedOutOfChunk").and_then(js_to_number) {
        Some(n) if n.is_finite() && n > 0.0 => n,
        _ => 0.0,
    };
    r.insert("demotedOutOfChunk".into(), json!(prev + demoted as f64));
    r.insert("chunkIndex".into(), json!(chunk_index));
    Value::Object(r)
}

/// Порт aggregateModel: склейка частей одной модели по всем чанкам.
/// Возвращает агрегат и число схлопнутых дублей (для лога обёртки —
/// в JS его печатал dedupeSameModelFindings через log).
pub fn aggregate_model(model: &str, done: &[Value]) -> (Value, usize) {
    let parts: Vec<&Value> = done
        .iter()
        .filter(|d| d.get("model").map(|m| m.as_str() == Some(model)).unwrap_or(false))
        .map(|d| d.get("res").unwrap_or(&Value::Null))
        .collect();
    let ok_parts: Vec<&Value> = parts
        .iter()
        .copied()
        .filter(|p| !js_truthy(p.get("error").unwrap_or(&Value::Null)))
        .collect();

    if ok_parts.is_empty() {
        let all_skipped = !parts.is_empty()
            && parts
                .iter()
                .all(|p| js_truthy(p.get("skipped").unwrap_or(&Value::Null)));
        let mut out = Map::new();
        out.insert("model".into(), Value::String(model.to_string()));
        let err = parts
            .first()
            .and_then(|p| p.get("error"))
            .filter(|e| js_truthy(e))
            .cloned()
            .unwrap_or_else(|| Value::String("нет ответа".to_string()));
        out.insert("error".into(), err);
        if all_skipped {
            out.insert("skipped".into(), json!(true));
        }
        return (Value::Object(out), 0);
    }

    // Вердикт — максимум ранга; равенство → первый (стабильный V8 sort).
    // Отсутствующий ключ у части (undefined) и explicit null различаем:
    // undefined-вердикт в JS-агрегате выпадает при JSON.stringify, null — нет.
    let mut verdict: Option<&Value> = None;
    let mut seen = false;
    for p in &ok_parts {
        let v = p.get("verdict");
        if !seen || verdict_rank(v) > verdict_rank(verdict) {
            verdict = v;
            seen = true;
        }
    }

    let summary = ok_parts
        .iter()
        .map(|p| p.get("summary").unwrap_or(&Value::Null))
        .find(|s| js_truthy(s))
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()));

    // flatMap(p => p.findings): у успешных частей findings всегда массив
    // (reviewWithModel/reviewChunkMoA) — не-массив трактуем как пусто.
    let all: Vec<Value> = ok_parts
        .iter()
        .flat_map(|p| match p.get("findings") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        })
        .collect();
    let (kept, dropped) = dedupe_same_model_findings(&all);

    let mut out = Map::new();
    out.insert("model".into(), Value::String(model.to_string()));
    out.insert("summary".into(), summary);
    if let Some(v) = verdict {
        out.insert("verdict".into(), v.clone());
    }
    out.insert("findings".into(), Value::Array(kept));
    (Value::Object(out), dropped)
}

// ---------------------------------------------------------------------------
// Тесты — ключевые случаи из test/pr-review.engine.test.mjs
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(paths: serde_json::Value, extra: serde_json::Value) -> Value {
        let mut m = Map::new();
        if let Some(p) = paths.as_array() {
            m.insert("paths".into(), Value::Array(p.clone()));
        }
        if let Some(o) = extra.as_object() {
            for (k, v) in o {
                m.insert(k.clone(), v.clone());
            }
        }
        Value::Object(m)
    }

    #[test]
    fn scope_priors_filters_by_chunk_files() {
        let priors = vec![
            json!({"file": "a.ts", "title": "a"}),
            json!({"file": "b.ts", "title": "b"}),
            json!({"file": "", "title": "no file"}),
        ];
        let c = chunk(json!(["a.ts"]), json!({}));
        let scoped = scope_priors(&priors, &c).unwrap();
        assert_eq!(scoped.len(), 2);
        assert_eq!(scoped[0]["title"], "a");
        assert_eq!(scoped[1]["title"], "no file");

        // пустые paths / unknownPaths → None (скоуп неполон)
        assert!(scope_priors(&priors, &chunk(json!([]), json!({}))).is_none());
        assert!(scope_priors(&priors, &chunk(json!(["a.ts"]), json!({"unknownPaths": true}))).is_none());
        assert!(scope_priors(&priors, &chunk(serde_json::Value::Null, json!({}))).is_none());
        // пустые priors → None (вход без изменений)
        assert!(scope_priors(&[], &c).is_none());
    }

    #[test]
    fn demote_out_of_chunk_lowers_invisible_blockers() {
        let res = json!({
            "model": "m1",
            "findings": [
                {"file": "b.ts", "line": 3, "severity": "critical", "title": "слепой", "confidence": 0.9},
                {"file": "a.ts", "line": 1, "severity": "major", "title": "виден", "confidence": 0.9},
                {"file": "", "line": 0, "severity": "minor", "title": "без файла", "confidence": 0.5},
                {"file": "b.ts", "line": 4, "severity": "nit", "title": "уже nit", "confidence": 0.3},
            ],
        });
        let c = chunk(json!(["a.ts"]), json!({}));
        let out = demote_out_of_chunk(&res, &c, 0);
        let f = out["findings"].as_array().unwrap();
        assert_eq!(f[0]["severity"], "nit");
        assert_eq!(f[0]["outOfChunk"], true);
        assert_eq!(f[0]["confidence"], 0.5);
        assert_eq!(f[1]["severity"], "major", "видимый файл не тронут");
        assert!(!f[1].as_object().unwrap().contains_key("outOfChunk"));
        assert_eq!(f[2]["severity"], "minor", "без файла не демоутится");
        assert_eq!(f[3]["severity"], "nit", "nit не демоутится повторно");
        assert_eq!(out["demotedOutOfChunk"], 1.0);
        assert_eq!(out["chunkIndex"], 0.0);

        // без демоута — никаких меток
        let clean = demote_out_of_chunk(&res, &chunk(json!(["a.ts", "b.ts"]), json!({})), 1);
        assert!(clean.as_object().unwrap().get("demotedOutOfChunk").is_none());
        assert!(clean.as_object().unwrap().get("chunkIndex").is_none());
        // скоуп неполон — как есть
        let untouched = demote_out_of_chunk(&res, &chunk(json!([]), json!({})), 0);
        assert_eq!(untouched["findings"].as_array().unwrap().len(), 4);
    }

    fn done_item(model: &str, res: serde_json::Value) -> Value {
        json!({ "model": model, "res": res })
    }

    #[test]
    fn aggregate_merges_chunks_dedupes_and_ranks_verdict() {
        let done = vec![
            done_item("m1", json!({
                "model": "m1", "summary": "s1", "verdict": "comment",
                "findings": [{"file": "a.ts", "line": 1, "severity": "minor", "title": "X", "confidence": 0.5, "model": "m1"}],
            })),
            done_item("m1", json!({
                "model": "m1", "summary": "", "verdict": "changes_requested",
                // та же находка из второго чанка — схлопывается
                "findings": [{"file": "a.ts", "line": 1, "severity": "minor", "title": "x", "confidence": 0.5, "model": "m1"}],
            })),
        ];
        let (agg, dropped) = aggregate_model("m1", &done);
        assert_eq!(dropped, 1);
        assert_eq!(agg["verdict"], "changes_requested", "максимум ранга");
        assert_eq!(agg["summary"], "s1", "первое непустое summary");
        assert_eq!(agg["findings"].as_array().unwrap().len(), 1);
        assert_eq!(agg["findings"][0]["dupCount"], 2.0);
    }

    #[test]
    fn aggregate_all_failed_and_all_skipped() {
        let done = vec![
            done_item("m1", json!({"model": "m1", "error": "down"})),
            done_item("m1", json!({"model": "m1", "error": "down2"})),
        ];
        let (agg, _) = aggregate_model("m1", &done);
        assert_eq!(agg["error"], "down", "первая ошибка");
        assert!(agg.get("skipped").is_none());

        let skipped_done = vec![
            done_item("m1", json!({"model": "m1", "error": "тайм-бюджет", "skipped": true})),
            done_item("m1", json!({"model": "m1", "error": "тайм-бюджет", "skipped": true})),
        ];
        let (agg, _) = aggregate_model("m1", &skipped_done);
        assert_eq!(agg["skipped"], true);

        // одна часть skipped, другая упала по ошибке — skipped НЕ выставляется
        let mixed = vec![
            done_item("m1", json!({"model": "m1", "error": "тайм", "skipped": true})),
            done_item("m1", json!({"model": "m1", "error": "boom"})),
        ];
        let (agg, _) = aggregate_model("m1", &mixed);
        assert_eq!(agg["error"], "тайм");
        assert!(agg.get("skipped").is_none(), "не все части skipped");

        let empty: Vec<Value> = vec![];
        let (agg, _) = aggregate_model("m1", &empty);
        assert_eq!(agg["error"], "нет ответа");
    }
}
