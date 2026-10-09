//! Подкоманда `pr-review review` — порт bin/pr-review.mjs и runReview из pipeline.mjs.
//! Заполняется в задаче T27 (RFC 16).

use std::path::PathBuf;

/// Аргументы `pr-review review`.
///
/// Пока заведены только два флага offline-прогона (`bin/pr-review.mjs:2-3`);
/// остальная конфигурация приходит из env и профиля, как в JS.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Прогон без GitHub: результат печатается локально, комментарии не постятся.
    #[arg(long)]
    pub dry_run: bool,

    /// Путь к готовому diff-файлу вместо загрузки диффа из PR.
    #[arg(long, value_name = "PATH")]
    pub diff: Option<PathBuf>,
}

pub fn run(_args: Args) -> anyhow::Result<i32> {
    anyhow::bail!("подкоманда ещё не реализована (см. RFC 16)")
}
