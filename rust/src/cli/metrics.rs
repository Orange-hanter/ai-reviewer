//! Подкоманда `pr-review metrics` — порт bin/pr-review-metrics.mjs.
//! Заполняется в задаче T29 (RFC 16).

use std::path::PathBuf;

/// Аргументы `pr-review metrics`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Путь к JSONL-логу observability; по умолчанию — из профиля.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
}

pub fn run(_args: Args) -> anyhow::Result<i32> {
    anyhow::bail!("подкоманда ещё не реализована (см. RFC 16)")
}
