//! Бинарный JSON-контракт порта lib/github.mjs (Фаза 3, шаг 4) — транспортный слой.
//!
//! Вход (stdin):
//!   {"op":"request", ...GhRequest} → stdout {"text","link"} — один ghRaw-вызов
//!                                    (method/path/body/accept, ретраи внутри);
//!   {"op":"list", ...GhRequest}    → stdout = JSON-значение: ghList-склейка
//!                                    страниц по Link rel="next"; первая
//!                                    не-массивная страница возвращается как есть.
//! Ошибки: exit 2 + {"error","status","networky"} в stderr — JS-обёртка
//! reconstruct'ит Error с флагом status (как у JS-ветки ghRaw).
//!
//! Оркестрация (upsert, post-then-delete, sticky-футер, graphql-треды) — в
//! JS-обёртке lib/github.mjs: маркеры профиля и ремонт JSON живут в JS-процессе.

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use ai_reviewer::github::{gh_list, gh_raw, GhError, GhRequest};

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
    let req: GhRequest =
        serde_json::from_value(Value::Object(obj.clone())).context("вход должен быть GhRequest")?;

    match op {
        "request" => match gh_raw(&req) {
            Ok(res) => println!(
                "{}",
                json!({ "text": res.text, "link": res.link })
            ),
            Err(e) => fail(e),
        },
        "list" => match gh_list(&req) {
            Ok(value) => println!("{}", serde_json::to_string(&value).expect("сериализация результата")),
            Err(e) => fail(e),
        },
        other => bail!("неизвестный op '{other}'"),
    }
    Ok(())
}

fn fail(e: GhError) -> ! {
    eprintln!("{}", json!({ "error": e.error, "status": e.status, "networky": e.networky }));
    std::process::exit(2);
}
