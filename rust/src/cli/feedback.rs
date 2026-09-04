//! Подкоманда `pr-review feedback` — порт bin/pr-review-feedback.mjs.
//! Заполняется в задаче T30 (RFC 04).

/// Аргументы `pr-review feedback` (заполняются в T30).
#[derive(Debug, clap::Args)]
pub struct Args {}

pub fn run(_args: Args) -> anyhow::Result<i32> {
    anyhow::bail!("подкоманда ещё не реализована (см. RFC 04)")
}
