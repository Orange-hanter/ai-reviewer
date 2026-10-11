//! Подкоманда `pr-review feedback-migrate` — порт feedback/migrate-feedback.mjs
//! (RFC 16, T30).
//!
//! `[--file <path>] [--write]`: проверяет журнал `fp-feedback.jsonl` на схему v1
//! ([`crate::feedback::migrate_feedback_text`]) и с `--write` переписывает его
//! атомарно (временный файл рядом + rename). Путь по умолчанию — `feedback.logFile`
//! конфига (`REVIEW_FEEDBACK_LOG` / профиль), относительный — от cwd.
//!
//! Stdout — одна строка JSON `{path, write, changed, total, valid, migrated, invalid}`.
//! Exit 1 — есть невалидные строки (с `--write` журнал тогда не трогается:
//! `migration refused`), либо ошибка чтения/записи (`[migrate-feedback] …` в stderr).
//! Отсутствующий файл — пустой журнал, не ошибка.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::cli::feedback::{env_strings, flag, parse_cli, resolve_profile};
use crate::feedback::{js_stringify, migrate_feedback_text, MigrateStats};
use crate::pipeline::load_review_config;

/// Аргументы `pr-review feedback-migrate`: сырые argv, как в JS.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// `[--file <path>] [--write]`.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub argv: Vec<String>,
}

/// Итог `migrateFeedbackLog`.
#[derive(Debug)]
pub struct MigrateLogResult {
    pub path: PathBuf,
    pub changed: bool,
    pub stats: MigrateStats,
}

/// `migrateFeedbackLog(path, { write })`.
pub fn migrate_feedback_log(path: &Path, write: bool) -> Result<MigrateLogResult> {
    let target = std::path::absolute(path)?;
    let raw = match fs::read_to_string(&target) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let result = migrate_feedback_text(&raw);
    if write && result.stats.invalid > 0 {
        bail!("migration refused: {} invalid row(s)", result.stats.invalid);
    }
    let changed = result.text != raw;
    if write && changed {
        let mut tmp = target.clone().into_os_string();
        tmp.push(format!(".tmp-{}", std::process::id()));
        fs::write(&tmp, &result.text)?;
        fs::rename(&tmp, &target)?;
    }
    Ok(MigrateLogResult { path: target, changed, stats: result.stats })
}

fn run_inner(argv: &[String]) -> Result<i32> {
    let path = match flag(argv, "--file").filter(|s| !s.is_empty()) {
        Some(p) => p.to_string(),
        None => {
            let env = env_strings();
            let cwd = std::env::current_dir()?;
            let profile = resolve_profile(&env, &cwd)?;
            let cfg = load_review_config(&env, &parse_cli(argv, &env), &profile);
            cfg.pointer("/feedback/logFile").and_then(Value::as_str).unwrap_or("").to_string()
        }
    };
    let write = argv.iter().any(|a| a == "--write");
    let r = migrate_feedback_log(Path::new(&path), write)?;
    println!(
        "{}",
        js_stringify(&json!({
            "path": r.path.to_string_lossy(),
            "write": write,
            "changed": r.changed,
            "total": r.stats.total,
            "valid": r.stats.valid,
            "migrated": r.stats.migrated,
            "invalid": r.stats.invalid,
        }))
    );
    Ok(if r.stats.invalid > 0 { 1 } else { 0 })
}

pub fn run(args: Args) -> Result<i32> {
    match run_inner(&args.argv) {
        Ok(code) => Ok(code),
        Err(e) => {
            eprintln!("[migrate-feedback] {e:#}");
            Ok(1)
        }
    }
}
