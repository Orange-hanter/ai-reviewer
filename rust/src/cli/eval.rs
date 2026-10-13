//! Подкоманда `pr-review eval` — прогон golden-фикстур без LLM (порт bin/pr-review-eval.mjs).
//!
//! Код возврата — как у JS: 0, если все фикстуры прошли, иначе 1. Это осознанный
//! ненулевой код (гейт смены модели/промпта), поэтому он возвращается через `Ok(1)`,
//! а не ошибкой: ошибку `main` превратил бы в fail-open exit 0.
//!
//! Разбор argv — как в JS (`includes`/`indexOf`), без строгого clap: лишний флаг или
//! `--fixtures` без значения не роняют прогон с кодом 2.

use std::path::PathBuf;

use crate::eval::{default_fixtures_dir, json_output, process_env_map, run_eval_harness};

/// Аргументы `pr-review eval`: сырой argv, разбор как в bin/pr-review-eval.mjs.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// `[--fixtures <dir>] [--json]`; неизвестные аргументы игнорируются.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub argv: Vec<String>,
}

/// `argv.includes('--fixtures') ? argv[indexOf + 1] : DEFAULT` — флаг без значения
/// даёт `undefined`, и дефолтный параметр `runEvalHarness` берёт фикстуры пакета.
fn fixtures_dir(argv: &[String]) -> PathBuf {
    match argv.iter().position(|a| a == "--fixtures") {
        Some(i) => argv.get(i + 1).map(PathBuf::from).unwrap_or_else(default_fixtures_dir),
        None => default_fixtures_dir(),
    }
}

pub fn run(args: Args) -> anyhow::Result<i32> {
    let dir = fixtures_dir(&args.argv);
    let as_json = args.argv.iter().any(|a| a == "--json");
    let out = run_eval_harness(&dir, &process_env_map(), &std::env::current_dir().unwrap_or_default());
    if as_json {
        println!("{}", json_output(&out));
    } else {
        println!("{}", out.report);
    }
    Ok(if out.ok { 0 } else { 1 })
}
