//! Подкоманда `pr-review review` — порт `bin/pr-review.mjs` и `runReview` из `pipeline.mjs`
//! (RFC 16, задача T27).
//!
//! Коды возврата — контракт гейта:
//! - `1` — только осознанный: подтверждённые блокеры или strict-режим
//!   (`REVIEW_LLM_REQUIRED=1` при недоступном LLM, `REVIEW_VERIFIER_STRICT`). Эти решения
//!   принимает пайплайн и отдаёт в `PipelineOutcome::exit_code`;
//! - `0` — всё остальное, включая мягкий отказ (нет ключа, пустой дифф, черновик) и аварию
//!   самого движка: ревью советует, инфра-сбой не блокирует merge
//!   (`bin/pr-review.mjs:9-14`, `runReview` `catch → process.exit(0)`).
//!
//! Сборка зависимостей — в [`super::wiring`]: её же переиспользует `review-mcp`.

use std::path::PathBuf;

use super::wiring::{process_env, stdout_log, ReviewWiring};
use crate::pipeline::CliArgs;

/// Аргументы `pr-review review` — `parseCli` (`lib/config.mjs:62-74`).
/// Остальная конфигурация приходит из env и профиля, как в JS.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Прогон без постинга в GitHub: результат только в лог/артефакт.
    #[arg(long)]
    pub dry_run: bool,

    /// Путь к готовому diff-файлу вместо загрузки диффа из PR.
    #[arg(long, value_name = "PATH")]
    pub diff: Option<PathBuf>,

    /// SHA головы для file-diff прогона.
    #[arg(long, value_name = "SHA")]
    pub head_sha: Option<String>,

    /// Репозиторий `owner/repo` (по умолчанию — `GITHUB_REPOSITORY`).
    #[arg(long, value_name = "OWNER/REPO")]
    pub repo: Option<String>,

    /// Номер PR (по умолчанию — `PR_NUMBER`).
    #[arg(long, value_name = "N")]
    pub pr: Option<String>,
}

/// `Number(x || 0)` для номера PR: нечисловое (NaN в JS) и пустое — `0`, то есть «нет PR».
fn pr_number(raw: &str) -> u64 {
    let t = raw.trim();
    if t.is_empty() {
        return 0;
    }
    t.parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0).map(|n| n as u64).unwrap_or(0)
}

impl Args {
    /// `parseCli(argv)` с env-фолбэками `GITHUB_REPOSITORY`/`PR_NUMBER`.
    /// Пустое значение флага в JS — falsy и уступает env (`flag('--repo') || env`).
    pub fn to_cli_args(&self, env: &std::collections::HashMap<String, String>) -> CliArgs {
        let env_of = |k: &str| env.get(k).cloned().unwrap_or_default();
        let non_empty = |v: &Option<String>| v.clone().filter(|s| !s.is_empty());
        CliArgs {
            dry_run: self.dry_run,
            diff_file: self.diff.as_ref().map(|p| p.to_string_lossy().into_owned()),
            head_sha: self.head_sha.clone().unwrap_or_default(),
            repo: non_empty(&self.repo).unwrap_or_else(|| env_of("GITHUB_REPOSITORY")),
            pr_number: pr_number(&non_empty(&self.pr).unwrap_or_else(|| env_of("PR_NUMBER"))),
        }
    }
}

pub fn run(args: Args) -> anyhow::Result<i32> {
    let env = process_env();
    let cli = args.to_cli_args(&env);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let outcome = ReviewWiring::from_env(&env, &cli, cwd, stdout_log()).and_then(|w| w.run_detailed());
    match outcome {
        Ok(o) => Ok(o.exit_code),
        Err(e) => {
            // Авария движка — exit 0. Текст ошибки не несёт секретов: ключи ходят
            // в заголовках, а не в URL, и в сообщения ошибок клиентов не попадают.
            eprintln!("[pr-review] {e:#}");
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn args(repo: Option<&str>, pr: Option<&str>) -> Args {
        Args { dry_run: false, diff: None, head_sha: None, repo: repo.map(Into::into), pr: pr.map(Into::into) }
    }

    #[test]
    fn parse_cli_фолбэк_на_env_и_number() {
        let env: HashMap<String, String> =
            [("GITHUB_REPOSITORY", "o/r"), ("PR_NUMBER", "42")].into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        let c = args(None, None).to_cli_args(&env);
        assert_eq!((c.repo.as_str(), c.pr_number), ("o/r", 42));
        let c = args(Some("a/b"), Some("7")).to_cli_args(&env);
        assert_eq!((c.repo.as_str(), c.pr_number), ("a/b", 7));
        let c = args(Some(""), Some("")).to_cli_args(&env);
        assert_eq!((c.repo.as_str(), c.pr_number), ("o/r", 42), "пустой флаг уступает env");
        assert_eq!(args(None, Some("abc")).to_cli_args(&HashMap::new()).pr_number, 0, "NaN → нет PR");
        assert_eq!(args(None, Some("1.5")).to_cli_args(&HashMap::new()).pr_number, 0);
        assert!(args(None, None).to_cli_args(&HashMap::new()).diff_file.is_none());
    }
}
