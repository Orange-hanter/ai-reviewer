//! JSON-контракт порта lib/symbol-cards.mjs (RFC 16, T38) — только для parity.
//!
//! JS-обёртки у модуля нет (его зовёт pipeline, который портирует T26); бинарь нужен
//! `rust/parity/symbol-cards.parity.mjs`, чтобы сверить линейные разборы с JS-регулярками
//! на корпусе реальных файлов.
//!
//! Вход (stdin): {"op": "batch", "corpus": [[строки]…], "calls": [{"op": "<функция>", ...аргументы}, …]};
//! вместо `lines`/`text` вызов может сослаться на элемент корпуса: `linesRef`/`textRef`.
//! Выход (stdout): массив результатов в порядке вызовов.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde_json::{json, Value};

use ai_reviewer::graphify_client::build_graph_index;
use ai_reviewer::symbol_cards::*;

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", json!({ "error": format!("{e:#}") }));
        std::process::exit(2);
    }
}

fn strs(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect())
        .unwrap_or_default()
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn map(v: &Value, key: &str) -> HashMap<String, String> {
    v.get(key)
        .and_then(Value::as_object)
        .map(|o| o.iter().map(|(k, x)| (k.clone(), x.as_str().unwrap_or("").to_string())).collect())
        .unwrap_or_default()
}

fn decl(d: Declaration) -> Value {
    json!({ "header": d.header, "endIdx": d.end_idx })
}

fn call(c: &Value) -> Result<Value> {
    Ok(match s(c, "op") {
        "extractUsedIdentifiers" => json!(extract_used_identifiers(&strs(c, "lines")).into_iter().collect::<Vec<_>>()),
        "packageImportedNames" => json!(package_imported_names(&strs(c, "lines"))),
        "relativeImportBindings" => json!(relative_import_bindings(&strs(c, "lines"))),
        "extractNamespaceCalls" => {
            let ns: HashSet<String> = strs(c, "namespaces").into_iter().collect();
            json!(extract_namespace_calls(&strs(c, "lines"), &ns).into_values().collect::<Vec<_>>())
        }
        "maskNonCode" => json!(mask_non_code(&strs(c, "lines"))),
        "scanDeclaration" => {
            let max = c.get("maxLines").and_then(Value::as_u64).map_or(MAX_SCAN_LINES, |x| x as usize);
            decl(scan_declaration(&strs(c, "lines"), c["startIdx"].as_u64().unwrap_or(0) as usize, max))
        }
        "cardDeclaration" => {
            decl(card_declaration(&strs(c, "lines"), c["startIdx"].as_u64().unwrap_or(0) as usize, s(c, "name")))
        }
        "declarationTest" => json!(declaration_test(s(c, "name"), s(c, "text"))),
        "hasLocalBinding" => json!(has_local_binding(&strs(c, "lines"), s(c, "name"))),
        "addedLinesOfSection" => json!(added_lines_of_section(s(c, "section"))),
        "isAbsenceClaim" => json!(is_absence_claim(&c["finding"])),
        "isSymbolCardPath" => json!(is_symbol_card_path(s(c, "path"))),
        "resolveImportPath" => {
            let files: HashSet<String> = strs(c, "files").into_iter().collect();
            json!(resolve_import_path(s(c, "caller"), s(c, "spec"), &|f| files.contains(f)))
        }
        "resolveCardNode" => {
            let g = build_graph_index(&c["graph"], None);
            let r = resolve_card_node(Some(&g), s(c, "name"));
            json!({ "id": r.node.map(|n| n["id"].clone()), "reason": r.reason })
        }
        "buildSymbolCards" => {
            let g = c.get("graph").filter(|g| !g.is_null()).map(|g| build_graph_index(g, None));
            let files: HashMap<String, Arc<Vec<String>>> = c["files"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| {
                            let lines = v.as_array().map(|a| a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect()).unwrap_or_default();
                            (k.clone(), Arc::new(lines))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let read = |p: &str| files.get(p).cloned();
            let (sections, windows) = (map(c, "sections"), map(c, "windows"));
            let root = Path::new(s(c, "root"));
            let r = build_symbol_cards(
                &strs(c, "paths"),
                &BuildOpts {
                    graph_index: g.as_ref(),
                    sections: &sections,
                    windows: &windows,
                    read_lines: &read,
                    body_max_lines: c.get("bodyMaxLines").and_then(Value::as_f64).unwrap_or(0.0),
                    root,
                    app_dir: s(c, "appDir"),
                },
            );
            json!({ "byPath": r.by_path.into_iter().collect::<Vec<_>>(), "metrics": r.metrics })
        }
        "selectChunkCards" => {
            let by_path: IndexMap<String, Vec<SymbolCard>> =
                serde_json::from_value(c["byPath"].clone()).context("byPath")?;
            let r = select_chunk_cards(
                &by_path,
                &strs(c, "chunkPaths"),
                c.get("max").and_then(Value::as_f64).unwrap_or(20.0),
                c.get("maxChars").and_then(Value::as_f64).unwrap_or(3000.0),
                s(c, "visibleText"),
            );
            json!(r)
        }
        "symbolCardsHashInput" => {
            let cards: Vec<SymbolCard> = serde_json::from_value(c["cards"].clone()).context("cards")?;
            json!(symbol_cards_hash_input(&cards))
        }
        "renderSymbolCard" => {
            let card: SymbolCard = serde_json::from_value(c["card"].clone()).context("card")?;
            json!(render_symbol_card(&card))
        }
        "symbolCardsOutcome" => {
            let arr = |k: &str| c[k].as_array().cloned().unwrap_or_default();
            let inline: Vec<usize> = c["inline"].as_array().map(|a| a.iter().filter_map(Value::as_u64).map(|x| x as usize).collect()).unwrap_or_default();
            symbol_cards_outcome(&arr("chunks"), &arr("llmFindings"), &arr("finalFindings"), &inline)
        }
        other => bail!("неизвестная операция '{other}'"),
    })
}

fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).context("чтение stdin")?;
    let v: Value = serde_json::from_str(&input).context("вход не является JSON")?;
    let calls = v.get("calls").and_then(Value::as_array).context("ожидается {op: batch, calls: [...]}")?;
    // `corpus` — общие массивы строк: вызов ссылается на них `linesRef`/`textRef`, чтобы
    // не повторять файл в каждом вызове.
    let corpus = v.get("corpus").and_then(Value::as_array).cloned().unwrap_or_default();
    let resolve = |c: &Value| -> Value {
        let mut c = c.clone();
        if let Some(i) = c.get("linesRef").and_then(Value::as_u64) {
            c["lines"] = corpus.get(i as usize).cloned().unwrap_or(Value::Null);
        }
        if let Some(i) = c.get("textRef").and_then(Value::as_u64) {
            let lines: Vec<&str> = corpus
                .get(i as usize)
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|x| x.as_str().unwrap_or("")).collect())
                .unwrap_or_default();
            c["text"] = json!(lines.join("\n"));
        }
        c
    };
    let out: Result<Vec<Value>> = calls.iter().map(|c| call(&resolve(c))).collect();
    println!("{}", Value::Array(out?));
    Ok(())
}
