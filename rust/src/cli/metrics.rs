//! Подкоманда `pr-review metrics` — порт bin/pr-review-metrics.mjs (RFC 16, T29).
//!
//! Поток как в JS: конфиг (`REVIEW_PROFILE` → автопоиск в cwd → встроенный профиль,
//! поверх env) → путь лога = первый позиционный аргумент, а если его нет или он
//! пустой — `join(observability.dir, observability.file)` → чтение файла →
//! `parse_jsonl` → `aggregate_metrics` → `format_metrics_report` в stdout.
//!
//! Коды возврата — свои, а не fail-open диспетчера: отчёт ничего не блокирует, а
//! вызывающему (человеку или CI) нужен сигнал «лог не прочитан». Ошибка чтения или
//! агрегации — сообщение `[pr-review.metrics] не удалось прочитать <path>: …` в stderr
//! и exit 1, как `process.exit(1)` в JS.
//!
//! Отклонения от JS:
//! - текст системной ошибки — от Rust std, а не от Node (`ENOENT: …`); префикс
//!   сообщения и код возврата те же;
//! - `--help`/`-h` печатает справку clap; в JS это ушло бы в путь файла;
//! - лишние позиционные аргументы игнорируются, как `process.argv[3..]` в JS;
//! - профиль и конфиг грузятся только когда путь не передан (в JS — всегда): им
//!   нужен лишь путь по умолчанию, а сбой профиля в JS при явном пути дал бы exit 1
//!   с необработанным исключением. Лог профиля (`[ai-review] profile: …`) при явном
//!   пути поэтому не печатается.

use std::path::Path;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::config::load_review_config_parts;
use crate::metrics::{aggregate_metrics, format_metrics_report, parse_jsonl};
use crate::profile::Profile;

/// Аргументы `pr-review metrics`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Путь к JSONL-логу observability; по умолчанию — из конфига
    /// (REVIEW_OBSERVABILITY_DIR / профиль + REVIEW_OBSERVABILITY_FILE).
    /// Аргументы после первого игнорируются, как в JS.
    #[arg(value_name = "PATH", num_args = 0.., allow_hyphen_values = true)]
    pub argv: Vec<String>,
}

pub fn run(args: Args) -> Result<i32> {
    let env: Map<String, Value> = std::env::vars_os()
        .map(|(k, v)| {
            (k.to_string_lossy().into_owned(), Value::String(v.to_string_lossy().into_owned()))
        })
        .collect();
    let path = match args.argv.first().filter(|p| !p.is_empty()) {
        Some(p) => p.clone(),
        None => match default_path(&env) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[pr-review.metrics] не удалось определить путь лога: {e:#}");
                return Ok(1);
            }
        },
    };
    match report(&path) {
        Ok(text) => {
            println!("{text}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("[pr-review.metrics] не удалось прочитать {path}: {e:#}");
            Ok(1)
        }
    }
}

/// `join(cfg.observability.dir, cfg.observability.file)` из JS.
fn default_path(env: &Map<String, Value>) -> Result<String> {
    let cwd = std::env::current_dir()?;
    let os_env = env
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
        .collect();
    let profile = Profile::resolve(&os_env, &cwd, |line| eprintln!("[ai-review] {line}"))?;
    let cfg = load_review_config_parts(env, &profile.config_input());
    let obs = cfg.get("observability");
    let field = |key: &str| {
        obs.and_then(|o| o.get(key)).and_then(Value::as_str).unwrap_or("").to_string()
    };
    Ok(js_join(&field("dir"), &field("file")))
}

/// Чтение лога и отчёт. Невалидный UTF-8 заменяется на U+FFFD, как
/// `readFile(path, 'utf8')` в Node, а не роняет чтение.
fn report(path: &str) -> Result<String> {
    let bytes = std::fs::read(Path::new(path))?;
    let text = String::from_utf8_lossy(&bytes);
    let records = Value::Array(parse_jsonl(&text));
    let agg = aggregate_metrics(Some(&records))?;
    format_metrics_report(Some(&agg), path)
}

/// `path.join` Node в POSIX-семантике: непустые части склеиваются через `/`, затем
/// лексическая нормализация (`.` выпадает, `..` снимает предыдущий сегмент, у
/// относительного пути лишние `..` остаются в начале). Пустой результат — `.`.
fn js_join(a: &str, b: &str) -> String {
    let joined: Vec<&str> = [a, b].into_iter().filter(|s| !s.is_empty()).collect();
    if joined.is_empty() {
        return ".".to_string();
    }
    let source = joined.join("/");
    let absolute = source.starts_with('/');
    let trailing = source.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in source.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let mut out = parts.join("/");
    if absolute {
        out.insert(0, '/');
    }
    if out.is_empty() {
        out.push('.');
    }
    if trailing && out != "/" {
        out.push('/');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_join_matches_node_path_join() {
        // значения сверены с node -e "console.log(require('path').join(a, b))"
        let cases = [
            (".ai-review/observability", "pr-review-runs.jsonl", ".ai-review/observability/pr-review-runs.jsonl"),
            ("", "runs.jsonl", "runs.jsonl"),
            ("", "", "."),
            ("./a//b/", "c", "a/b/c"),
            ("../x", "../../y", "../../y"),
            ("/a/b", "../../../c", "/c"),
            ("/a", "/b", "/a/b"),
            ("a", "b/", "a/b/"),
            ("a", "..", "."),
        ];
        for (a, b, want) in cases {
            assert_eq!(js_join(a, b), want, "join({a:?}, {b:?})");
        }
    }

    #[test]
    fn report_reads_jsonl_and_formats_with_source() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("runs.jsonl");
        std::fs::write(&file, b"{\"type\":\"run_end\",\"wouldBlock\":true}\n\xff broken\n").unwrap();
        let path = file.to_string_lossy().into_owned();
        let text = report(&path).unwrap();
        assert!(text.contains(&path), "в отчёте нет источника:\n{text}");
    }

    #[test]
    fn report_missing_file_is_error() {
        assert!(report("/nonexistent/definitely/runs.jsonl").is_err());
    }
}
