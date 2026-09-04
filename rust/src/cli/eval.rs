//! Подкоманда `pr-review eval` — прогон golden-фикстур без LLM (порт lib/eval.mjs).
//! Заполняется в задаче T28 (RFC 04).

/// Аргументы `pr-review eval` (заполняются в T28).
#[derive(Debug, clap::Args)]
pub struct Args {}

pub fn run(_args: Args) -> anyhow::Result<i32> {
    anyhow::bail!("подкоманда ещё не реализована (см. RFC 04)")
}
