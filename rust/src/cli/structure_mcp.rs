//! Подкоманда `pr-review structure-mcp` — порт bin/pr-review-structure-mcp.mjs
//! (stdio-транспорт с Content-Length framing). Заполняется в задаче T23 (RFC 16).

/// Аргументы `pr-review structure-mcp` (заполняются в T23).
#[derive(Debug, clap::Args)]
pub struct Args {}

pub fn run(_args: Args) -> anyhow::Result<i32> {
    anyhow::bail!("подкоманда ещё не реализована (см. RFC 16)")
}
