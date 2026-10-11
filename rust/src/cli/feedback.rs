//! Подкоманда `pr-review feedback` — порт bin/pr-review-feedback.mjs (RFC 16, T30).
//!
//! Тонкий CLI поверх [`crate::feedback::harvest_feedback`]: argv разбирается так же,
//! как `parseCli` (`--dry-run`, `--repo`, `--pr`; `GITHUB_REPOSITORY`/`PR_NUMBER` —
//! запасные значения), `--dry-run` выставляет `REVIEW_FEEDBACK_WRITE=0` до сборки
//! конфига.
//!
//! Коды возврата — как у JS, а не fail-open диспетчера: harvest ничего не блокирует,
//! но вызывающему workflow нужен сигнал сбоя.
//!
//! | ситуация | stdout/stderr | exit |
//! |---|---|---|
//! | `REVIEW_FEEDBACK=0` | `REVIEW_FEEDBACK=0 — harvest отключён` | 0 |
//! | нет токена / repo / PR | `[pr-review.feedback] нужны …` в stderr | 1 |
//! | harvest упал (GitHub-чтение) | `[pr-review.feedback] <ошибка>` в stderr | 1 |
//! | `ok === false` (сбой записи) | отчёт | 1 |
//! | иначе | отчёт | 0 |
//!
//! Отклонения от JS: в stderr на сбое — текст ошибки без стека Node; `--help`/`-h`
//! в первой позиции печатает справку clap.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::eval::process_env_map;
use crate::feedback::{format_harvest_report, harvest_feedback};
use crate::github::{GithubConfig, RealGithubClient};
use crate::pipeline::{load_review_config, CliArgs};
use crate::profile::Profile;

/// Аргументы `pr-review feedback`: сырые argv, разбираются как `parseCli` в JS
/// (флаги ищутся по позиции, неизвестные игнорируются).
#[derive(Debug, clap::Args)]
pub struct Args {
    /// `[--dry-run] [--pr N] [--repo owner/repo]`.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub argv: Vec<String>,
}

/// `flag(name)` из `parseCli`: значение после флага; флаг последний — `''`.
pub(crate) fn flag<'a>(argv: &'a [String], name: &str) -> Option<&'a str> {
    let i = argv.iter().position(|a| a == name)?;
    Some(argv.get(i + 1).map(String::as_str).unwrap_or(""))
}

/// `Number(x)` для номера PR: целое ≥ 0 или `0` (NaN, дробь, мусор — falsy).
fn pr_number(raw: &str) -> u64 {
    raw.trim().parse::<u64>().unwrap_or(0)
}

/// `parseCli(argv)` (`lib/config.mjs:62-74`); `env` — это `process.env` в JS.
pub(crate) fn parse_cli(argv: &[String], env: &HashMap<String, String>) -> CliArgs {
    let non_empty = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_string);
    let env_get = |k: &str| non_empty(env.get(k).map(String::as_str));
    CliArgs {
        dry_run: argv.iter().any(|a| a == "--dry-run"),
        diff_file: flag(argv, "--diff").map(str::to_string),
        head_sha: non_empty(flag(argv, "--head-sha")).unwrap_or_default(),
        repo: non_empty(flag(argv, "--repo")).or_else(|| env_get("GITHUB_REPOSITORY")).unwrap_or_default(),
        pr_number: non_empty(flag(argv, "--pr"))
            .or_else(|| env_get("PR_NUMBER"))
            .map(|s| pr_number(&s))
            .unwrap_or(0),
    }
}

/// `process.env` строками (не-UTF-8 переменные пропускаются).
pub(crate) fn env_strings() -> HashMap<String, String> {
    process_env_map()
        .into_iter()
        .map(|(k, v)| (k, v.as_str().unwrap_or("").to_string()))
        .collect()
}

/// `resolveProfile(env)`: `REVIEW_PROFILE` → автопоиск в cwd → встроенный профиль.
pub(crate) fn resolve_profile(env: &HashMap<String, String>, cwd: &Path) -> Result<Profile> {
    Profile::resolve(env, cwd, |line| eprintln!("[ai-review] {line}"))
}

/// Логгер `console.log` для клиента GitHub и harvest.
pub(crate) fn stdout_log() -> Arc<dyn Fn(&str) + Send + Sync> {
    Arc::new(|m: &str| println!("{m}"))
}

/// Клиент GitHub из собранного конфига, логи — в stdout (как `console.log` в JS).
pub(crate) fn github_client(cfg: &Value, profile: &Profile) -> Result<RealGithubClient> {
    let log = stdout_log();
    Ok(RealGithubClient::new(&GithubConfig::from_cfg(cfg), profile.markers().clone())?
        .with_log(Box::new(move |m: &str| log(m))))
}

pub fn run(args: Args) -> Result<i32> {
    let mut env = env_strings();
    let cli = parse_cli(&args.argv, &env);
    if cli.dry_run {
        env.insert("REVIEW_FEEDBACK_WRITE".into(), "0".into());
    }
    match harvest(&env, &cli) {
        Ok(code) => Ok(code),
        Err(e) => {
            eprintln!("[pr-review.feedback] {e:#}");
            Ok(1)
        }
    }
}

fn harvest(env: &HashMap<String, String>, cli: &CliArgs) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let profile = resolve_profile(env, &cwd)?;
    let cfg = load_review_config(env, cli, &profile);

    if cfg.pointer("/feedback/enabled") == Some(&Value::Bool(false)) {
        println!("REVIEW_FEEDBACK=0 — harvest отключён");
        return Ok(0);
    }
    let token_set = cfg.pointer("/github/token").and_then(Value::as_str).is_some_and(|t| !t.is_empty());
    if !token_set || cli.repo.is_empty() || cli.pr_number == 0 {
        eprintln!("[pr-review.feedback] нужны GITHUB_TOKEN, GITHUB_REPOSITORY (или --repo), PR_NUMBER (или --pr)");
        return Ok(1);
    }

    let github = github_client(&cfg, &profile)?;
    let log = |m: &str| println!("{m}");
    let result = harvest_feedback(&cfg, profile.markers(), &github, &cwd, &log)?;
    println!("{}", format_harvest_report(&result));
    Ok(if result.ok { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_cli_как_в_js() {
        let mut env = HashMap::new();
        env.insert("GITHUB_REPOSITORY".to_string(), "o/env".to_string());
        env.insert("PR_NUMBER".to_string(), "9".to_string());
        let c = parse_cli(&argv(&["--dry-run", "--pr", "42"]), &env);
        assert!(c.dry_run);
        assert_eq!(c.pr_number, 42);
        assert_eq!(c.repo, "o/env");
        // флаг последним → '' → запасное значение из env
        let c = parse_cli(&argv(&["--repo"]), &env);
        assert_eq!(c.repo, "o/env");
        assert_eq!(c.pr_number, 9);
        // Number('abc') — NaN, falsy
        let c = parse_cli(&argv(&["--pr", "abc", "--repo", "a/b"]), &HashMap::new());
        assert_eq!(c.pr_number, 0);
        assert_eq!(c.repo, "a/b");
        assert!(!c.dry_run);
    }
}
