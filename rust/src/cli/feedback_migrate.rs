//! Подкоманда `pr-review feedback-migrate` — порт feedback/migrate-feedback.mjs.
//! Заполняется в задаче T30 (RFC 16).

/// Аргументы `pr-review feedback-migrate` (заполняются в T30).
#[derive(Debug, clap::Args)]
pub struct Args {}

pub fn run(_args: Args) -> anyhow::Result<i32> {
    anyhow::bail!("подкоманда ещё не реализована (см. RFC 16)")
}
