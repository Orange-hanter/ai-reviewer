//! Бинарь-мост eval: тот же харнес, что `pr-review eval` (порт bin/pr-review-eval.mjs).
//!
//! Стаб Фазы 0 заменён вызовом [`ai_reviewer::eval`]; аргументы те же, что у
//! `node bin/pr-review-eval.mjs`: `--fixtures <dir>`, `--json`. Мост удаляется в
//! cutover (T33) вместе с остальными src/bin/*.

use std::path::PathBuf;

use clap::Parser;

use ai_reviewer::eval::{default_fixtures_dir, json_output, process_env_map, run_eval_harness};

#[derive(Parser)]
#[command(name = "eval", about = "Eval-харнес golden-фикстур без LLM (мост до cutover)")]
struct Cli {
    /// Каталог фикстур; по умолчанию — fixtures/ пакета.
    #[arg(long, value_name = "DIR")]
    fixtures: Option<PathBuf>,
    /// Печатать JSON `{ ok, metrics, results }`.
    #[arg(long)]
    json: bool,
}

fn main() {
    let cli = Cli::parse();
    let dir = cli.fixtures.unwrap_or_else(default_fixtures_dir);
    let out = run_eval_harness(&dir, &process_env_map());
    if cli.json {
        println!("{}", json_output(&out));
    } else {
        println!("{}", out.report);
    }
    std::process::exit(if out.ok { 0 } else { 1 });
}
