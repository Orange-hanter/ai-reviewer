//! Подкоманда `pr-review structure` — порт bin/pr-review-structure.mjs.
//! Заполняется в задаче T22 (RFC 16).

/// Аргументы `pr-review structure` (заполняются в T22).
#[derive(Debug, clap::Args)]
pub struct Args {}

pub fn run(_args: Args) -> anyhow::Result<i32> {
    anyhow::bail!("подкоманда ещё не реализована (см. RFC 16)")
}
