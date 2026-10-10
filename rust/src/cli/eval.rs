//! Подкоманда `pr-review eval` — прогон golden-фикстур без LLM (порт bin/pr-review-eval.mjs).
//!
//! Код возврата — как у JS: 0, если все фикстуры прошли, иначе 1. Это осознанный
//! ненулевой код (гейт смены модели/промпта), поэтому он возвращается через `Ok(1)`,
//! а не ошибкой: ошибку `main` превратил бы в fail-open exit 0.

use std::path::PathBuf;

use crate::eval::{default_fixtures_dir, json_output, process_env_map, run_eval_harness};

/// Аргументы `pr-review eval`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Каталог фикстур; по умолчанию — `fixtures/` пакета.
    #[arg(long, value_name = "DIR")]
    pub fixtures: Option<PathBuf>,
    /// Печатать `{ ok, metrics, results }` в JSON вместо текстового отчёта.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: Args) -> anyhow::Result<i32> {
    let dir = args.fixtures.unwrap_or_else(default_fixtures_dir);
    let out = run_eval_harness(&dir, &process_env_map());
    if args.json {
        println!("{}", json_output(&out));
    } else {
        println!("{}", out.report);
    }
    Ok(if out.ok { 0 } else { 1 })
}
