//! Бинарный JSON-контракт порта lib/diff.mjs (Фаза 1, шаг 1).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат функции в JSON-формате, эквивалентном возврату JS
//!                 (null вместо undefined; условное поле unknownPaths как в JS).
//! Ошибки:        non-zero exit code + {"error": "..."} в stderr.
//!
//! Операции:
//!   unescapeGitPath        {s}
//!   parseDiffNewPath       {line}                       → string | null
//!   parseDiffGitPath       {line}                       → string | null
//!   parseDiff              {diff}                       → {files, annotated, changedText}
//!   extractChangedSymbols  {text, maxSymbols?}          → string[]
//!   collectBlastSymbols    {changedText, contentMap?, maxSymbols?} → string[]
//!   splitSegmentByHunks    {seg, maxBytes}              → string[]
//!   buildChunks            {annotated, contentMap?, limits} → Chunk[]

use std::io::Read;

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde_json::{json, Value};

use ai_reviewer::diff::{
    build_chunks, collect_blast_symbols, extract_changed_symbols, parse_diff, parse_diff_git_path,
    parse_diff_new_path, split_segment_by_hunks, unescape_git_path, ChunkLimits,
};

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", json!({ "error": format!("{e:#}") }));
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("чтение stdin")?;
    let v: Value = serde_json::from_str(&input).context("вход не является JSON")?;
    let obj = v.as_object().context("вход должен быть JSON-объектом")?;
    let op = obj.get("op").and_then(Value::as_str).unwrap_or("");

    let arg_str = |name: &str| -> Result<&str> {
        obj.get(name)
            .and_then(Value::as_str)
            .with_context(|| format!("аргумент '{name}' должен быть строкой"))
    };
    let arg_usize = |name: &str, default: usize| -> Result<usize> {
        match obj.get(name) {
            None | Some(Value::Null) => Ok(default),
            Some(n) => Ok(n
                .as_u64()
                .with_context(|| format!("аргумент '{name}' должен быть числом"))?
                as usize),
        }
    };
    let arg_content_map = |name: &str| -> Result<IndexMap<String, String>> {
        match obj.get(name) {
            None | Some(Value::Null) => Ok(IndexMap::new()),
            Some(Value::Object(m)) => m
                .iter()
                .map(|(k, v)| {
                    v.as_str()
                        .map(|s| (k.clone(), s.to_string()))
                        .with_context(|| format!("contentMap['{k}'] должен быть строкой"))
                })
                .collect(),
            Some(_) => bail!("аргумент '{name}' должен быть объектом"),
        }
    };

    let out = match op {
        "unescapeGitPath" => json!(unescape_git_path(arg_str("s")?)),
        "parseDiffNewPath" => match parse_diff_new_path(arg_str("line")?) {
            Some(p) => json!(p),
            None => Value::Null,
        },
        "parseDiffGitPath" => match parse_diff_git_path(arg_str("line")?) {
            Some(p) => json!(p),
            None => Value::Null,
        },
        "parseDiff" => serde_json::to_value(parse_diff(arg_str("diff")?))?,
        "extractChangedSymbols" => {
            json!(extract_changed_symbols(arg_str("text")?, arg_usize("maxSymbols", 25)?))
        }
        "collectBlastSymbols" => {
            let content_map = arg_content_map("contentMap")?;
            json!(collect_blast_symbols(
                arg_str("changedText")?,
                Some(&content_map),
                arg_usize("maxSymbols", 25)?,
            ))
        }
        "splitSegmentByHunks" => {
            json!(split_segment_by_hunks(arg_str("seg")?, arg_usize("maxBytes", 0)?))
        }
        "buildChunks" => {
            let content_map = arg_content_map("contentMap")?;
            let limits: ChunkLimits = serde_json::from_value(
                obj.get("limits")
                    .cloned()
                    .context("аргумент 'limits' обязателен")?,
            )
            .context("limits: нужны maxDiffBytes/maxContextBytes/chunkBytes/maxChunks")?;
            json!(build_chunks(arg_str("annotated")?, &content_map, &limits))
        }
        other => bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
