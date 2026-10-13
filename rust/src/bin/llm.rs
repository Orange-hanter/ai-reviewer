//! Бинарный JSON-контракт порта lib/llm.mjs `chatRequest` (Фаза 3, шаг 1).
//!
//! Вход (stdin):  ChatRequest (см. rust/src/llm.rs) — один запрос:
//!   {"url","key","model","messages","maxTokens","temperature","effort",
//!    "jsonMode","seed","httpRetries","timeoutMs","includeUsage"}
//! Выход (stdout): {"content","usage","throttled"} — usage после normalizeUsage (null,
//!                 если usage не пришёл/непригоден); throttled — 429 встречался (JS
//!                 обёртка режет адаптивные семафоры).
//! Ошибки:        exit 2 + {"error","status","empty","networky","throttled"} в stderr —
//!                 JS-обёртка reconstruct'ит Error с флагами status/empty (как у JS-ветки).
//!
//! Режим каскада (с 0.6): во входе есть `"cascade": "json" | "prose"` — бинарь делает
//! весь каскад ретраев chatJson/chatText (rust/src/llm/cascade.rs), а не один запрос.
//!   json  → stdout {"value","repaired","usage","usages","throttled","log"}
//!   prose → stdout {"content","usage","usages","throttled","log"}
//!   сбой  → exit 2 + {"error","status","empty","networky","truncated","badEscape",
//!           "throttled","usages","log"} в stderr.
//! `log` — строки для лога обёртки, `usages` — usage каждого дошедшего ответа (учёт цены).
//!
//! Семафоры намеренно в JS-обёртке (lib/llm.mjs): бинарь — одноразовый процесс на
//! запрос, сквозное состояние семафоров живёт только в JS.

use std::io::Read;

use anyhow::{Context, Result};
use serde_json::json;

use ai_reviewer::llm::cascade::{run_cascade, CascadeRequest};
use ai_reviewer::llm::{chat, ChatRequest, LlmError};

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", json!({ "error": format!("{e:#}") }));
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).context("чтение stdin")?;
    let raw: serde_json::Value = serde_json::from_str(&input).context("вход должен быть JSON")?;
    if raw.get("cascade").is_some() {
        let req: CascadeRequest = serde_json::from_value(raw).context("вход должен быть CascadeRequest JSON")?;
        let (ok, payload) = run_cascade(&req).to_json(req.cascade);
        if ok {
            println!("{payload}");
            return Ok(());
        }
        eprintln!("{payload}");
        std::process::exit(2);
    }
    let req: ChatRequest = serde_json::from_value(raw).context("вход должен быть ChatRequest JSON")?;
    match chat(&req) {
        Ok(out) => {
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "content": out.content,
                    "usage": out.usage,
                    "throttled": out.throttled,
                }))
                .expect("сериализация результата")
            );
            Ok(())
        }
        Err(LlmError { error, status, empty, networky, throttled }) => {
            eprintln!(
                "{}",
                json!({
                    "error": error,
                    "status": status,
                    "empty": empty,
                    "networky": networky,
                    "throttled": throttled,
                })
            );
            std::process::exit(2);
        }
    }
}
