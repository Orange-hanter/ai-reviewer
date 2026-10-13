//! Подкоманда `pr-review feedback-batch` — порт bin/pr-review-feedback-batch.mjs
//! (RFC 20, T30).
//!
//! Плановый/ручной harvest по нескольким PR: явный `--pr`/`PR_NUMBER` — один PR,
//! иначе дискавери `GET /repos/{repo}/pulls?state=all&sort=updated…` одной страницей
//! и отбор [`select_feedback_pulls`] (не боты, обновлены за `REVIEW_FEEDBACK_SINCE_DAYS`,
//! не больше `REVIEW_FEEDBACK_PR_LIMIT`). Сбой harvest одного PR не прерывает пакет —
//! он идёт в отчёт строкой `failed`.
//!
//! Коды возврата как у JS: нет `GITHUB_REPOSITORY`/`GITHUB_TOKEN` или упала
//! дискавери — сообщение `[pr-review.feedback-batch] …` в stderr и exit 1; хотя бы
//! один PR провалился — exit 1; иначе 0.

use std::collections::HashMap;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::cli::feedback::{env_strings, github_client, parse_cli, resolve_profile};
use crate::feedback::{harvest_feedback, HarvestResult};
use crate::pipeline::{load_review_config, CliArgs};
use crate::structure_drift::js_date_parse_ms;
use crate::traits::{GhCallOpts, GithubApi};

/// Аргументы `pr-review feedback-batch`: сырые argv, как `parseCli` в JS.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// `[--dry-run] [--pr N] [--repo owner/repo]`.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub argv: Vec<String>,
}

/// `positiveInt(value, fallback)`: `Number(value)` — целое > 0, иначе fallback.
fn positive_int(raw: Option<&str>, fallback: u64) -> u64 {
    let Some(s) = raw else { return fallback };
    let t = s.trim();
    // Number('') === 0 → fallback; дробная запись целого ('5.0') — целое, как в JS.
    match t.parse::<f64>() {
        Ok(n) if n.is_finite() && n.fract() == 0.0 && n > 0.0 && !t.is_empty() => n as u64,
        _ => fallback,
    }
}

/// `positiveInt` для поля JSON (`pr.number`).
fn positive_int_value(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => positive_int(Some(&n.to_string()), 0),
        Some(Value::String(s)) => positive_int(Some(s), 0),
        _ => 0,
    }
}

/// `selectFeedbackPulls`: явный PR — он один; иначе свежие PR людей.
pub fn select_feedback_pulls(pulls: &Value, explicit_pr: u64, now_ms: i64, since_days: u64, limit: u64) -> Vec<u64> {
    if explicit_pr > 0 {
        return vec![explicit_pr];
    }
    let since = if since_days > 0 { since_days } else { 14 };
    let limit = if limit > 0 { limit } else { 30 };
    let cutoff = now_ms - since as i64 * 86_400_000;
    pulls
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|pr| {
            positive_int_value(pr.get("number")) > 0
                && pr.pointer("/user/type").and_then(Value::as_str) != Some("Bot")
                && pr
                    .get("updated_at")
                    .and_then(Value::as_str)
                    .and_then(js_date_parse_ms)
                    .is_some_and(|t| t >= cutoff)
        })
        .take(limit as usize)
        .map(|pr| positive_int_value(pr.get("number")))
        .collect()
}

/// Итог пакета (`runFeedbackBatch`).
#[derive(Debug)]
pub struct BatchResult {
    pub repo: String,
    pub dry_run: bool,
    pub selected: usize,
    pub results: Vec<HarvestResult>,
}

impl BatchResult {
    fn failed(&self) -> usize {
        self.results.iter().filter(|r| !r.ok).count()
    }
}

/// Результат-заглушка для PR, чей harvest бросил ошибку.
fn failed_result(pr: u64, repo: &str, error: String) -> HarvestResult {
    HarvestResult {
        pr: json!(pr),
        repo: repo.to_string(),
        new_entries: Vec::new(),
        persisted_entries: 0,
        total_in_log: 0,
        candidates_written: 0,
        warnings: Vec::new(),
        write_errors: vec![error],
        dry_run: false,
        ok: false,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `runFeedbackBatch(env, argv)`.
pub fn run_feedback_batch(env: &HashMap<String, String>, argv: &[String]) -> Result<BatchResult> {
    let dry_run = argv.iter().any(|a| a == "--dry-run");
    let mut run_env = env.clone();
    if dry_run {
        run_env.insert("REVIEW_FEEDBACK_WRITE".into(), "0".into());
    }
    let cli = parse_cli(argv, env);
    let get = |k: &str| run_env.get(k).map(String::as_str).filter(|s| !s.is_empty());
    let repo = if cli.repo.is_empty() { get("GITHUB_REPOSITORY").unwrap_or("").to_string() } else { cli.repo.clone() };
    let token_set = get("GITHUB_TOKEN").is_some();
    if repo.is_empty() || !token_set {
        bail!("batch harvest требует GITHUB_REPOSITORY и GITHUB_TOKEN");
    }

    let cwd = std::env::current_dir()?;
    let profile = resolve_profile(&run_env, &cwd)?;
    let base_cli = CliArgs { repo: repo.clone(), pr_number: 0, dry_run, ..cli.clone() };
    let base_cfg = load_review_config(&run_env, &base_cli, &profile);
    let discovery = github_client(&base_cfg, &profile)?;
    let explicit_pr = if cli.pr_number > 0 { cli.pr_number } else { positive_int(get("PR_NUMBER"), 0) };
    let pulls = if explicit_pr > 0 {
        json!([])
    } else {
        discovery.gh(
            &format!("/repos/{repo}/pulls?state=all&sort=updated&direction=desc&per_page=100"),
            &GhCallOpts::default(),
        )?
    };
    let pr_numbers = select_feedback_pulls(
        &pulls,
        explicit_pr,
        now_ms(),
        positive_int(get("REVIEW_FEEDBACK_SINCE_DAYS"), 14),
        positive_int(get("REVIEW_FEEDBACK_PR_LIMIT"), 30),
    );

    let log = |m: &str| println!("{m}");
    let mut results = Vec::new();
    for &pr in &pr_numbers {
        let pr_cli = CliArgs { pr_number: pr, ..base_cli.clone() };
        let cfg = load_review_config(&run_env, &pr_cli, &profile);
        let outcome = github_client(&cfg, &profile)
            .and_then(|github| harvest_feedback(&cfg, profile.markers(), &github, &cwd, &log));
        results.push(match outcome {
            Ok(r) => r,
            Err(e) => failed_result(pr, &repo, format!("{e:#}")),
        });
    }
    Ok(BatchResult { repo, dry_run, selected: pr_numbers.len(), results })
}

/// `formatBatchReport`.
pub fn format_batch_report(result: &BatchResult) -> String {
    let new_entries: usize = result.results.iter().map(|r| r.new_entries.len()).sum();
    let persisted: usize = result.results.iter().map(|r| r.persisted_entries).sum();
    let mut lines = vec![
        "=== pr-review feedback batch ===".to_string(),
        format!("repo: {}", result.repo),
        format!(
            "selected: {}, processed: {}, failed: {}",
            result.selected,
            result.results.len(),
            result.failed()
        ),
        format!("new entries: {new_entries}, persisted: {persisted}"),
    ];
    for item in &result.results {
        let pr = match &item.pr {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        lines.push(format!(
            "- PR #{pr}: {}; new={}",
            if item.ok { "ok" } else { "failed" },
            item.new_entries.len()
        ));
    }
    lines.join("\n")
}

pub fn run(args: Args) -> Result<i32> {
    match run_feedback_batch(&env_strings(), &args.argv) {
        Ok(result) => {
            println!("{}", format_batch_report(&result));
            Ok(if result.failed() > 0 { 1 } else { 0 })
        }
        Err(e) => {
            eprintln!("[pr-review.feedback-batch] {e:#}");
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_pr_wins_schedule_filters_old_and_bot() {
        let pulls = json!([
            { "number": 9, "updated_at": "2026-07-09T00:00:00.000Z", "user": { "type": "User" } },
            { "number": 8, "updated_at": "2026-07-08T00:00:00.000Z", "user": { "type": "Bot" } },
            { "number": 7, "updated_at": "2026-06-01T00:00:00.000Z", "user": { "type": "User" } },
        ]);
        let now = js_date_parse_ms("2026-07-10T00:00:00.000Z").unwrap();
        assert_eq!(select_feedback_pulls(&pulls, 0, now, 14, 30), vec![9]);
        assert_eq!(select_feedback_pulls(&pulls, 42, now, 14, 30), vec![42]);
        assert_eq!(select_feedback_pulls(&json!(null), 0, now, 14, 30), Vec::<u64>::new());
    }

    #[test]
    fn limit_and_bad_rows() {
        let pulls = json!([
            { "number": 3, "updated_at": "2026-07-09T00:00:00Z" },
            { "number": "x", "updated_at": "2026-07-09T00:00:00Z" },
            { "number": 2, "updated_at": "" },
            { "number": 1, "updated_at": "2026-07-09T00:00:00Z" },
        ]);
        let now = js_date_parse_ms("2026-07-10T00:00:00.000Z").unwrap();
        assert_eq!(select_feedback_pulls(&pulls, 0, now, 14, 1), vec![3]);
        assert_eq!(select_feedback_pulls(&pulls, 0, now, 0, 0), vec![3, 1]);
    }

    #[test]
    fn positive_int_как_number_is_integer() {
        assert_eq!(positive_int(Some("5"), 1), 5);
        assert_eq!(positive_int(Some("5.0"), 1), 5);
        assert_eq!(positive_int(Some("5.5"), 1), 1);
        assert_eq!(positive_int(Some("-3"), 1), 1);
        assert_eq!(positive_int(Some(""), 1), 1);
        assert_eq!(positive_int(None, 7), 7);
    }
}
