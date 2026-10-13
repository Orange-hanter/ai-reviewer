//! Подкоманда `pr-review review` — порт `bin/pr-review.mjs` и `runReview` из `pipeline.mjs`
//! (RFC 20, задача T27).
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

use super::feedback::flag;
use super::wiring::{process_env, stdout_log, ReviewWiring};
use crate::pipeline::{soft_fail_line, CliArgs};

/// Аргументы `pr-review review` — сырые argv, разбираются как `parseCli` (`lib/config.mjs:62-74`):
/// флаг ищется по первому вхождению, значение — следующий элемент (флаг последний — `''`),
/// неизвестные и повторённые флаги молча игнорируются. Строгий разбор clap здесь нельзя:
/// он завершал бы процесс с exit 2 до fail-open блока, и лишний флаг в вызове CI
/// блокировал бы merge без прогона ревью. Остальная конфигурация — из env и профиля.
///
/// Флаги: `--dry-run`, `--diff PATH`, `--head-sha SHA`, `--repo OWNER/REPO`, `--pr N`.
/// Отклонение от JS: `--help`/`-h` в первой позиции печатает справку clap.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// `[--dry-run] [--diff PATH] [--head-sha SHA] [--repo OWNER/REPO] [--pr N]`.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub argv: Vec<String>,
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
        let argv = &self.argv;
        let env_of = |k: &str| env.get(k).cloned().unwrap_or_default();
        let non_empty = |name: &str| flag(argv, name).filter(|s| !s.is_empty()).map(str::to_string);
        CliArgs {
            dry_run: argv.iter().any(|a| a == "--dry-run"),
            diff_file: flag(argv, "--diff").map(str::to_string),
            head_sha: non_empty("--head-sha").unwrap_or_default(),
            repo: non_empty("--repo").unwrap_or_else(|| env_of("GITHUB_REPOSITORY")),
            pr_number: pr_number(&non_empty("--pr").unwrap_or_else(|| env_of("PR_NUMBER"))),
        }
    }
}

pub fn run(args: Args) -> anyhow::Result<i32> {
    let env = process_env();
    let cli = args.to_cli_args(&env);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let log = stdout_log();
    let outcome = ReviewWiring::from_env(&env, &cli, cwd, log.clone()).and_then(|w| w.run_detailed());
    match outcome {
        Ok(o) => {
            // Дефолтный `die` CLI (`pipeline.mjs:316`): пайплайн сам мягкий отказ не логирует.
            if o.soft_fail {
                log(&soft_fail_line(o.reason.as_deref().unwrap_or("")));
            }
            Ok(o.exit_code)
        }
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
        let mut argv = Vec::new();
        if let Some(r) = repo {
            argv.extend(["--repo".to_string(), r.to_string()]);
        }
        if let Some(p) = pr {
            argv.extend(["--pr".to_string(), p.to_string()]);
        }
        Args { argv }
    }

    /// Разбор argv так же, как его делает `pr-review review` в main.
    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        use clap::Parser;
        #[derive(clap::Parser)]
        struct T {
            #[command(flatten)]
            a: Args,
        }
        T::try_parse_from(std::iter::once("review").chain(argv.iter().copied())).map(|t| t.a)
    }

    #[test]
    fn argv_терпит_лишние_повторные_и_пустые_флаги_как_parse_cli() {
        // JS parseCli игнорирует неизвестное и повторы; clap не должен падать с exit 2.
        let c = parse(&["--dry-run", "--diff", "c.patch", "--verbose"]).expect("лишний флаг").to_cli_args(&HashMap::new());
        assert!(c.dry_run);
        assert_eq!(c.diff_file.as_deref(), Some("c.patch"));
        let c = parse(&["--dry-run", "--dry-run", "--diff", "a", "--diff", "b"]).expect("повторы").to_cli_args(&HashMap::new());
        assert_eq!(c.diff_file.as_deref(), Some("a"), "первое вхождение, как argv.indexOf");
        let c = parse(&["--dry-run", "--diff"]).expect("--diff без значения").to_cli_args(&HashMap::new());
        assert_eq!(c.diff_file.as_deref(), Some(""), "флаг последний — ''");
        let c = parse(&["--head-sha", "abc1234", "--pr", "9", "--repo", "o/r"]).unwrap().to_cli_args(&HashMap::new());
        assert_eq!((c.head_sha.as_str(), c.repo.as_str(), c.pr_number), ("abc1234", "o/r", 9));
        assert!(!parse(&[]).unwrap().to_cli_args(&HashMap::new()).dry_run);
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
