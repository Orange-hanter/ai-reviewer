//! Подкоманда `pr-review feedback-promote` — порт feedback/promote-feedback.mjs,
//! promote-candidate.mjs и promote-patterns.mjs. Заполняется в задаче T30 (RFC 16).

/// Аргументы `pr-review feedback-promote` (заполняются в T30).
#[derive(Debug, clap::Args)]
pub struct Args {}

pub fn run(_args: Args) -> anyhow::Result<i32> {
    anyhow::bail!("подкоманда ещё не реализована (см. RFC 16)")
}
