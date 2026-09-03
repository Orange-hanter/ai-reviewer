//! Бинарный JSON-контракт порта lib/structure-protocol.mjs (Фаза 2, шаг 4).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате JS-возврата (null вместо undefined).
//! Ошибки:        exit code 2 + {"error": "..."} на stderr.
//!
//! Операции:
//!   assertPayloadSize        {text, limits?} → байты (number)
//!   validateStructureFacts   {payload, limits?} → payload (ошибки — exit 2)
//!   validateObligationsExport {payload, limits?} → payload
//!   validateStructureAssessment {payload, limits?} → payload
//!   parseStructurePayload    {text, limits?} → payload
//!   factsFingerprint         {headSha, providerVersion, configHash, scope} → hex
//!   assessmentFingerprint    {facts, obligationsRevision?, coverageHash, thresholdsHash} → hex
//!   temporalAlignment        {factsHeadSha, testHeadSha, obligationsHeadSha} → строка
//!   (аргументы фингерпринтов/alignment шлются плоско в корень, как у JS-функций)
//!
//! limits: отсутствие ключа — «нет проверки» (семантика undefined в JS),
//! null — 0, строка — Number(строка); в текстах ошибок лимит подставляется
//! raw-значением (null → "null"), как в JS-шаблоне. Схемы с диска не читаются
//! — ни JS, ни Rust (ручная валидация, см. шапку structure_protocol.rs).

use std::io::Read;

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

use ai_reviewer::structure_protocol::{
    assert_payload_size, assessment_fingerprint_op, facts_fingerprint_op, parse_structure_payload,
    payload_text, temporal_alignment_op, validate_obligations_export,
    validate_structure_assessment, validate_structure_facts,
};

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", json!({ "error": format!("{e:#}") }));
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).context("чтение stdin")?;
    let v: Value = serde_json::from_str(&input).context("вход не является JSON")?;
    let obj = v.as_object().context("вход должен быть JSON-объектом")?;
    let op = obj.get("op").and_then(Value::as_str).unwrap_or("");

    let out = match op {
        "assertPayloadSize" => {
            let bytes = assert_payload_size(&text_arg(obj), obj.get("limits"))?;
            num_json(bytes)
        }
        "validateStructureFacts" => {
            validate_structure_facts(&arg(&v, "payload"), obj.get("limits"))?
        }
        "validateObligationsExport" => {
            validate_obligations_export(&arg(&v, "payload"), obj.get("limits"))?
        }
        "validateStructureAssessment" => {
            validate_structure_assessment(&arg(&v, "payload"), obj.get("limits"))?
        }
        "parseStructurePayload" => {
            parse_structure_payload(&text_arg(obj), obj.get("limits"))?
        }
        "factsFingerprint" => json!(facts_fingerprint_op(&v)),
        "assessmentFingerprint" => json!(assessment_fingerprint_op(&v)),
        "temporalAlignment" => json!(temporal_alignment_op(&v)),
        other => anyhow::bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

/// Аргумент text как строка: JS String(text || '') — null/falsy → ''.
fn text_arg(obj: &Map<String, Value>) -> String {
    payload_text(obj.get("text"))
}

/// Аргумент как значение (None → Null): поля в JSON не бывают undefined.
fn arg(v: &Value, name: &str) -> Value {
    v.as_object()
        .and_then(|o| o.get(name))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Целое число — через i64 (как JS-число без дробной части), иначе f64.
fn num_json(n: f64) -> Value {
    if n.fract() == 0.0 && n.is_finite() && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
    }
}
