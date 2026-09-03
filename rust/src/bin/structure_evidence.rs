//! Бинарный JSON-контракт порта lib/structure-evidence.mjs (Фаза 2, шаг 3).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате JS-возврата (null вместо undefined).
//! Ошибки:        exit code 2 + {"error": "..."} на stderr.
//!
//! Операции:
//!   canonicalJson          {value} → строка (побайтовый паритет с lib/util.mjs)
//!   sha256                 {value} → hex (строка — utf8-байты, иначе canonicalJson)
//!   stableId               {prefix, parts} → строка
//!   normalizeRepoPath      {path} → строка
//!   validateEvidenceReceipt {receipt} → [ошибок] (пустой — валиден)
//!   createEvidenceReceipt  {input, now} → receipt (ошибки — exit 2)
//!   receiptRef             {receipt} → строка

use std::io::Read;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use ai_reviewer::findings::{js_to_string, js_truthy};
use ai_reviewer::structure_evidence::{
    canonical_json, create_evidence_receipt, receipt_ref, sha256_value, stable_id_op,
    validate_evidence_receipt,
};
use ai_reviewer::structure_scip::normalize_repo_path;

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
        "canonicalJson" => json!(canonical_json(&arg(&v, "value"))),
        "sha256" => json!(sha256_value(&arg(&v, "value"))),
        "stableId" => {
            let prefix = obj.get("prefix").and_then(Value::as_str).unwrap_or("");
            let parts = match obj.get("parts") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(a)) => a.clone(),
                Some(other) => vec![other.clone()], // stableId(prefix, ...parts) с одним part
            };
            json!(stable_id_op(prefix, &parts))
        }
        "normalizeRepoPath" => {
            let raw = match obj.get("path") {
                Some(p) if !p.is_null() && js_truthy(p) => js_to_string(p),
                _ => String::new(), // String(value || '')
            };
            json!(normalize_repo_path(&raw)?)
        }
        "validateEvidenceReceipt" => json!(validate_evidence_receipt(obj.get("receipt"))?),
        "createEvidenceReceipt" => {
            let now = obj.get("now").and_then(Value::as_str).unwrap_or("");
            create_evidence_receipt(obj.get("input"), now)?
        }
        "receiptRef" => json!(receipt_ref(&arg(&v, "receipt"))),
        other => anyhow::bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

/// Аргумент как значение (None → Null): поля в JSON не бывают undefined.
fn arg(v: &Value, name: &str) -> Value {
    v.as_object()
        .and_then(|o| o.get(name))
        .cloned()
        .unwrap_or(Value::Null)
}
