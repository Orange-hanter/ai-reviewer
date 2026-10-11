//! Подкоманда `pr-review feedback-promote` — порт feedback/promote-feedback.mjs,
//! promote-candidate.mjs и promote-patterns.mjs (RFC 16, T30).
//!
//! Два режима, как в JS:
//!
//! - по умолчанию — кандидаты `feedback.candidatesDir/*.json` → eval-fixtures: кандидат
//!   становится каталогом `<fixtures>/<id>/` (meta.json, findings.json, diff.patch)
//!   только если [`run_fixture_case`] на нём проходит; exit 1 — хоть один провал;
//! - `--aggregate` — журнал `fp-feedback.jsonl`: группы повторяющихся FP
//!   (`file|severity|title`, не меньше `REVIEW_FP_PROMOTE_MIN`) → известный FP-паттерн
//!   в `feedback.patternsFile` (если эвристика его узнала) + fixture из последней записи
//!   группы; exit 1 — провалилась хоть одна fixture (пропуск «уже есть» — не провал).
//!
//! Каталог fixtures — `REVIEW_FIXTURES_DIR` от cwd, иначе `fixtures/` пакета.
//!
//! Безопасность: имя fixture из кандидата (`suggestedId`) — недоверенный вход,
//! [`sanitize_fixture_id`] + [`resolve_fixture_dir`] не выпускают запись за пределы
//! каталога fixtures. Путь файла из находки уходит в `gh pr diff` только после `--`,
//! поэтому опцией argv он стать не может; токен в логи и ошибки не попадает.
//!
//! Отклонения от JS:
//! - кандидаты обходятся в отсортированном порядке имён (в JS — порядок `readdir`);
//! - API-фолбэк диффа идёт на `github.api` конфига (`GITHUB_API_URL`), а не на
//!   захардкоженный `https://api.github.com` — тот же адрес, что у остального клиента
//!   (нужен GHES и тестам без сети); ответ не-массив — пустой дифф, а не TypeError;
//! - сетевой сбой API-фолбэка — ошибка команды (exit 1), как отклонённый `fetch` в JS;
//!   HTTP-ошибка — пустой дифф и placeholder, как `!res.ok` в JS.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Result};
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::cli::feedback::{env_strings, parse_cli, resolve_profile};
use crate::config::load_review_config_parts;
use crate::eval::{default_fixtures_dir, run_fixture_case};
use crate::feedback::{build_fixture_candidate, js_stringify, js_stringify_pretty, read_feedback_log};
use crate::findings::{js_to_string, js_truthy};
use crate::fixtures::FixtureCase;
use crate::fp_patterns::parse_fp_patterns_text;
use crate::github::{build_http_client, gh_raw_with, GhRequest, GithubConfig};
use crate::pipeline::load_review_config;
use crate::profile::Profile;

/// Аргументы `pr-review feedback-promote`: сырые argv, как в JS.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// `[--aggregate] [--pr N] [--repo owner/repo]`.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub argv: Vec<String>,
}

// ---------------------------------------------------------------------------
// Пути
// ---------------------------------------------------------------------------

/// `path.resolve(base, p)`: абсолютный путь с лексически снятыми `.`/`..`.
pub fn js_resolve(base: &Path, p: &str) -> PathBuf {
    let joined = base.join(p);
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `sanitizeFixtureId`: безопасное имя каталога fixture — без обхода путей.
pub fn sanitize_fixture_id(id: &str) -> Option<String> {
    let s = id.trim();
    if s.is_empty() || s.contains("..") || s.contains('/') || s.contains('\\') {
        return None;
    }
    let ok = s.len() <= 64
        && s.bytes().enumerate().all(|(i, b)| {
            b.is_ascii_alphanumeric() || (i > 0 && matches!(b, b'.' | b'_' | b'-'))
        });
    ok.then(|| s.to_string())
}

/// `resolveFixtureDir`: каталог fixture обязан лежать внутри `fixtures_dir`.
pub fn resolve_fixture_dir(fixtures_dir: &Path, id: &str) -> Option<PathBuf> {
    let safe = sanitize_fixture_id(id)?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let root = js_resolve(&cwd, &fixtures_dir.to_string_lossy());
    let target = js_resolve(&root, &safe);
    (target != root && target.starts_with(&root)).then_some(target)
}

/// `matchPrFilePatch`: файл находки ↔ файл PR, с префиксом `appDir` и без.
pub fn match_pr_file_patch<'a>(files: &'a [Value], file: &str, app_dir: &str) -> Option<&'a Value> {
    let prefix = format!("{app_dir}/");
    let bare = if !app_dir.is_empty() && file.starts_with(&prefix) { &file[prefix.len()..] } else { file };
    let with_app = if app_dir.is_empty() { bare.to_string() } else { format!("{app_dir}/{bare}") };
    let variants = [file, bare, with_app.as_str()];
    files.iter().find(|f| f.get("filename").and_then(Value::as_str).is_some_and(|n| variants.contains(&n)))
}

// ---------------------------------------------------------------------------
// Дифф файла PR
// ---------------------------------------------------------------------------

/// Откуда брать дифф файла: `gh` CLI, затем REST API.
pub struct DiffSource {
    /// Бинарь `gh` (`gh` из PATH; в тестах — фейк).
    pub gh_bin: String,
    /// `github.api`.
    pub api: String,
    pub token: String,
    pub tool_name: String,
    pub app_dir: String,
}

impl DiffSource {
    /// `fetchPrFileDiff({ pr, repo, file, token })`.
    pub fn fetch(&self, pr: &str, repo: &str, file: &str) -> Result<String> {
        if !pr.is_empty() && !file.is_empty() {
            // `--` до пути: путь из находки не может стать опцией gh.
            let out = Command::new(&self.gh_bin)
                .args(["pr", "diff", pr, "--", file])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output();
            if let Ok(o) = out {
                if o.status.success() {
                    let s = String::from_utf8_lossy(&o.stdout).into_owned();
                    if !s.trim().is_empty() {
                        return Ok(s);
                    }
                }
            }
        }
        let mut parts = repo.split('/');
        let (owner, name) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        if owner.is_empty() || name.is_empty() || pr.is_empty() || file.is_empty() || self.token.is_empty() {
            return Ok(String::new());
        }
        let req = GhRequest {
            api: self.api.clone(),
            token: self.token.clone(),
            tool_name: self.tool_name.clone(),
            http_retries: 0,
            method: "GET".into(),
            path: format!("/repos/{owner}/{name}/pulls/{pr}/files?per_page=100"),
            body: None,
            accept: None,
        };
        let client = build_http_client().map_err(|e| anyhow!("{}", e.error))?;
        let text = match gh_raw_with(&client, &req, None) {
            Ok(r) => r.text,
            Err(e) if e.status.is_some() => return Ok(String::new()),
            Err(e) => return Err(anyhow!("{}", e.error)),
        };
        let files: Value = serde_json::from_str(&text)?;
        let hit = files.as_array().and_then(|a| match_pr_file_patch(a, file, &self.app_dir));
        Ok(match hit.and_then(|h| h.get("patch")).filter(|p| js_truthy(p)) {
            Some(p) => format!("{}\n", js_to_string(p)),
            None => String::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// promote-candidate
// ---------------------------------------------------------------------------

/// Итог promote одного кандидата (`{ ok, id, skipped?, error?, errors?, wouldBlock? }`).
#[derive(Debug, Clone, Default)]
pub struct PromoteResult {
    pub ok: bool,
    pub id: String,
    pub skipped: bool,
    pub error: Option<String>,
    pub errors: Option<Vec<String>>,
    pub would_block: Option<bool>,
}

impl PromoteResult {
    fn fail(id: impl Into<String>, error: impl Into<String>) -> Self {
        PromoteResult { id: id.into(), error: Some(error.into()), ..Default::default() }
    }

    /// `r.error || r.errors?.join('; ')`; оба пусты — `undefined`, как в шаблоне JS.
    fn reason(&self) -> String {
        if let Some(e) = self.error.as_ref().filter(|e| !e.is_empty()) {
            return e.clone();
        }
        match &self.errors {
            Some(list) => list.join("; "),
            None => "undefined".into(),
        }
    }
}

/// Общий контекст promote: куда писать fixtures, gate/prescan для проверки.
pub struct PromoteCtx<'a> {
    pub fixtures_dir: PathBuf,
    pub gate: Value,
    pub prescan: Value,
    pub repo: String,
    pub diff: &'a dyn Fn(&str, &str, &str) -> Result<String>,
    pub log: &'a dyn Fn(&str),
}

/// `x || fallback` → строка шаблона.
fn tpl_or(v: Option<&Value>) -> Option<String> {
    v.filter(|x| js_truthy(x)).map(js_to_string)
}

/// `promoteCandidateToFixture(candidate, { pr, … })`.
pub fn promote_candidate_to_fixture(candidate: &Value, pr: Option<&Value>, ctx: &PromoteCtx) -> Result<PromoteResult> {
    let raw_id = tpl_or(candidate.get("suggestedId")).or_else(|| tpl_or(candidate.pointer("/meta/id")));
    let id = raw_id.as_deref().and_then(sanitize_fixture_id);
    let findings = candidate.get("findings").and_then(Value::as_array);
    let file = findings.and_then(|f| f.first()).and_then(|f| tpl_or(f.get("file")));
    let (Some(id), Some(file), Some(findings)) = (id, file, findings) else {
        return Ok(PromoteResult::fail(raw_id.unwrap_or_else(|| "?".into()), "invalid id or missing file"));
    };
    let Some(target) = resolve_fixture_dir(&ctx.fixtures_dir, &id) else {
        return Ok(PromoteResult::fail(id, "invalid fixture path"));
    };
    if target.join("meta.json").exists() {
        return Ok(PromoteResult { skipped: true, ..PromoteResult::fail(id, "fixture exists") });
    }

    let pr_num = pr.filter(|p| js_truthy(p)).or_else(|| candidate.pointer("/fromFeedback/pr").filter(|p| js_truthy(p)));
    let pr_str = pr_num.map(js_to_string);
    let mut diff = String::new();
    if let Some(p) = pr_str.as_deref() {
        if !ctx.repo.is_empty() {
            diff = (ctx.diff)(p, &ctx.repo, &file)?;
        }
    }
    if diff.trim().is_empty() {
        diff = format!(
            "diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n@@ -1 +1 @@\n+# placeholder — replace with gh pr diff {} -- {file}\n",
            pr_str.as_deref().unwrap_or("N")
        );
    }

    let expected = candidate
        .pointer("/meta/expected")
        .filter(|e| js_truthy(e))
        .cloned()
        .unwrap_or_else(|| json!({ "shouldBlock": false, "blockerCount": 0 }));
    let case = FixtureCase {
        id: id.clone(),
        dir: target.clone(),
        description: String::new(),
        diff: diff.clone(),
        findings: findings.clone(),
        expected,
        gate: json!({}),
        verifier_verdicts: None,
        repo_files: None,
    };
    let result = run_fixture_case(&case, &ctx.gate, &ctx.prescan);
    let errors: Vec<String> = result
        .get("errors")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(js_to_string).collect())
        .unwrap_or_default();
    let would_block = result.get("wouldBlock").and_then(Value::as_bool);
    if result.get("pass") != Some(&Value::Bool(true)) {
        return Ok(PromoteResult { id, errors: Some(errors), would_block, ..Default::default() });
    }

    let meta = candidate.get("meta").filter(|m| js_truthy(m)).cloned().unwrap_or_else(|| json!({ "id": id }));
    let write = || -> std::io::Result<()> {
        fs::create_dir_all(&target)?;
        fs::write(target.join("meta.json"), format!("{}\n", js_stringify_pretty(&meta)))?;
        fs::write(
            target.join("findings.json"),
            format!("{}\n", js_stringify_pretty(&json!({ "findings": findings }))),
        )?;
        fs::write(target.join("diff.patch"), if diff.ends_with('\n') { diff.clone() } else { format!("{diff}\n") })?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = fs::remove_dir_all(&target);
        return Ok(PromoteResult::fail(id, e.to_string()));
    }
    (ctx.log)(&format!("promoted fixture {id} ({file})"));
    Ok(PromoteResult { ok: true, id, errors: Some(errors), would_block, ..Default::default() })
}

/// Итог `promoteCandidatesFromDir`.
#[derive(Debug, Default)]
pub struct DirPromoteResult {
    pub promoted: Vec<PromoteResult>,
    pub skipped: Vec<PromoteResult>,
    pub failed: Vec<PromoteResult>,
}

/// `promoteCandidatesFromDir(candidatesDir, { pr, … })`.
pub fn promote_candidates_from_dir(dir: &Path, pr: Option<&Value>, ctx: &PromoteCtx) -> Result<DirPromoteResult> {
    let mut out = DirPromoteResult::default();
    let Ok(rd) = fs::read_dir(dir) else { return Ok(out) };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json"))
        .collect();
    names.sort();
    for name in names {
        let candidate = match fs::read_to_string(dir.join(&name))
            .map_err(anyhow::Error::from)
            .and_then(|raw| serde_json::from_str::<Value>(&raw).map_err(anyhow::Error::from))
        {
            Ok(c) => c,
            Err(e) => {
                out.failed.push(PromoteResult::fail(name, format!("parse: {e}")));
                continue;
            }
        };
        let r = promote_candidate_to_fixture(&candidate, pr, ctx)?;
        if r.skipped {
            out.skipped.push(r);
        } else if r.ok {
            out.promoted.push(r);
        } else {
            out.failed.push(r);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// promote-patterns
// ---------------------------------------------------------------------------

/// `${v || ''}`.
fn or_empty(v: Option<&Value>) -> String {
    tpl_or(v).unwrap_or_default()
}

/// `fpAggregateKey`: группа без строки — тот же title+file+severity в разных PR.
pub fn fp_aggregate_key(entry: &Value) -> String {
    let f = |k: &str| or_empty(entry.get("finding").and_then(|x| x.get(k)));
    format!("{}|{}|{}", f("file"), f("severity"), f("title"))
}

/// `groupFpEntries`: группы FP в порядке первого появления, размером ≥ `min_count`.
pub fn group_fp_entries(entries: &[Value], min_count: f64) -> Vec<(String, Vec<Value>)> {
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    for e in entries {
        if e.get("signal").and_then(Value::as_str) != Some("fp") {
            continue;
        }
        let k = fp_aggregate_key(e);
        match groups.iter_mut().find(|(key, _)| *key == k) {
            Some((_, list)) => list.push(e.clone()),
            None => groups.push((k, vec![e.clone()])),
        }
    }
    groups.into_iter().filter(|(_, list)| list.len() as f64 >= min_count).collect()
}

/// `inferPatternId`: повторяющийся FP-заголовок → id известного паттерна.
pub fn infer_pattern_id(entry: &Value) -> Option<&'static str> {
    let f = |k: &str| or_empty(entry.get("finding").and_then(|x| x.get(k)));
    let text = format!("{} {}", f("title"), f("body")).to_lowercase();
    let re = |s: &str| Regex::new(s).expect("валидный регексп");
    if re("(?:e2e|playwright)").is_match(&text) && re(r"(?:нет|отсутств|missing|no\s+e2e)").is_match(&text) {
        return Some("e2e-in-diff");
    }
    if re(r"(?:файл|file).{0,20}(?:не\s+в|not\s+in|absent|missing)").is_match(&text) {
        return Some("file-absent-in-diff");
    }
    None
}

/// `KNOWN_PATTERNS[id]` — строка JSONL в том же порядке ключей, что в JS.
fn known_pattern(id: &str) -> Option<Value> {
    match id {
        "e2e-in-diff" => Some(json!({
            "id": "e2e-in-diff",
            "description": "e2e spec в диффе, но модель пишет «нет e2e»",
            "regex": "(?:e2e|playwright)[^.]{0,25}(?:^|\\s)(?:нет|отсутствует|без|не\\s+добавлен[а-я]*)(?=\\s|$)|(?:^|\\s)(?:нет|отсутствует|без|не\\s+добавлен[а-я]*)(?=\\s)[^.]{0,25}(?:e2e|playwright)|\\b(?:no|missing)\\s+(?:e2e|playwright)(?:\\s+tests?)?\\b|\\b(?:e2e|playwright)\\s+tests?\\s+(?:not\\s+added|missing|absent)\\b",
            "requiresPath": "(?:^|/)e2e/.*\\.spec\\.ts$",
        })),
        "file-absent-in-diff" => Some(json!({
            "id": "file-absent-in-diff",
            "description": "модель: файл не в PR, но basename есть в тексте и в changedPaths",
            "regex": "(?:^|\\s)файл(?=\\s).{0,30}?(?:отсутствует\\s+в|не\\s+(?:входит\\s+в|найден\\s+в|в))\\s+(?:дифф\\w*|pr\\b)|\\bfile\\b.{0,30}?(?:(?:not\\s+in|absent\\s+from|not\\s+found\\s+in|missing\\s+from|missing\\s+in)\\s+(?:the\\s+)?(?:pr|diff))\\b",
            "matchChangedBasename": true,
        })),
        _ => None,
    }
}

/// `appendPatternIfMissing`: `true` — паттерн дописан.
pub fn append_pattern_if_missing(patterns_file: &Path, pattern_id: &str, log: &dyn Fn(&str)) -> Result<bool> {
    let Some(known) = known_pattern(pattern_id) else { return Ok(false) };
    let existing = fs::read_to_string(patterns_file).map(|t| parse_fp_patterns_text(&t)).unwrap_or_default();
    if existing.iter().any(|p| p.id == pattern_id) {
        return Ok(false);
    }
    use std::io::Write as _;
    let mut f = fs::OpenOptions::new().create(true).append(true).open(patterns_file)?;
    f.write_all(format!("{}\n", js_stringify(&known)).as_bytes())?;
    log(&format!("appended fp pattern {pattern_id}"));
    Ok(true)
}

/// Fixture агрегата с ключом группы.
#[derive(Debug)]
pub struct AggregateFixture {
    pub result: PromoteResult,
    pub aggregate_key: String,
    pub count: usize,
}

/// Итог `promoteFromFeedbackLog`.
#[derive(Debug, Default)]
pub struct AggregateResult {
    pub patterns: Vec<String>,
    pub fixtures: Vec<AggregateFixture>,
    pub groups: usize,
}

/// `promoteFromFeedbackLog`.
pub fn promote_from_feedback_log(
    log_file: &Path,
    patterns_file: &Path,
    min_count: f64,
    ctx: &PromoteCtx,
) -> Result<AggregateResult> {
    let sink = |m: &str| (ctx.log)(m);
    let entries = read_feedback_log(log_file, &|m: &str| println!("{m}"));
    let groups = group_fp_entries(&entries, min_count);
    let mut out = AggregateResult { groups: groups.len(), ..Default::default() };
    for (_, list) in &groups {
        let sample = &list[list.len() - 1];
        if let Some(pid) = infer_pattern_id(sample) {
            if append_pattern_if_missing(patterns_file, pid, &sink)? {
                out.patterns.push(pid.to_string());
            }
        }
        let Some(candidate) = build_fixture_candidate(sample) else { continue };
        let r = promote_candidate_to_fixture(&candidate, sample.get("pr"), ctx)?;
        out.fixtures.push(AggregateFixture { result: r, aggregate_key: fp_aggregate_key(sample), count: list.len() });
    }
    Ok(out)
}

/// `formatPromoteReport`.
pub fn format_promote_report(r: &AggregateResult) -> String {
    let fx = || r.fixtures.iter().map(|f| &f.result);
    let mut lines = vec![
        "=== feedback promote (aggregate) ===".to_string(),
        format!("groups ≥ threshold: {}", r.groups),
        format!("patterns appended: {}", r.patterns.len()),
        format!("fixtures promoted: {}", fx().filter(|f| f.ok).count()),
        format!("fixtures failed: {}", fx().filter(|f| !f.ok && !f.skipped).count()),
        format!("fixtures skipped: {}", fx().filter(|f| f.skipped).count()),
    ];
    for f in fx() {
        let status = if f.ok {
            "ok".to_string()
        } else if f.skipped {
            "skip".to_string()
        } else {
            format!("fail {}", f.reason())
        };
        lines.push(format!("- {}: {status}", f.id));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// gate/prescan для проверки кандидата: `loadReviewConfig({ OLLAMA_API_KEY: 'eval' })`.
fn eval_gate(profile: &Profile) -> (Value, Value) {
    let mut env = Map::new();
    env.insert("OLLAMA_API_KEY".into(), json!("eval"));
    let parts = load_review_config_parts(&env, &profile.config_input());
    (parts.get("gate").cloned().unwrap_or_else(|| json!({})), parts.get("prescan").cloned().unwrap_or(Value::Null))
}

fn run_inner(argv: &[String]) -> Result<i32> {
    let aggregate = argv.iter().any(|a| a == "--aggregate");
    let env = env_strings();
    let cwd = std::env::current_dir()?;
    let cli = parse_cli(argv, &env);
    let profile = resolve_profile(&env, &cwd)?;
    let cfg = load_review_config(&env, &cli, &profile);
    let s = |p: &str| cfg.pointer(p).and_then(Value::as_str).unwrap_or("").to_string();
    let env_get = |k: &str| env.get(k).filter(|v| !v.is_empty()).cloned();

    let repo = if cli.repo.is_empty() { env_get("GITHUB_REPOSITORY").unwrap_or_default() } else { cli.repo.clone() };
    let token = Some(s("/github/token")).filter(|t| !t.is_empty()).or_else(|| env_get("GITHUB_TOKEN")).unwrap_or_default();
    let fixtures_dir = match env_get("REVIEW_FIXTURES_DIR") {
        Some(d) => js_resolve(&cwd, &d),
        None => default_fixtures_dir(),
    };
    let candidates_dir = js_resolve(&cwd, &s("/feedback/candidatesDir"));
    let log_file = js_resolve(&cwd, &s("/feedback/logFile"));
    let patterns_file = js_resolve(&cwd, &s("/feedback/patternsFile"));

    let source = DiffSource {
        gh_bin: "gh".into(),
        api: GithubConfig::from_cfg(&cfg).api,
        token,
        tool_name: s("/profile/toolName"),
        app_dir: profile.layout().app_dir.clone(),
    };
    let diff = |pr: &str, repo: &str, file: &str| source.fetch(pr, repo, file);
    let log = |m: &str| println!("{m}");
    let (gate, prescan) = eval_gate(&profile);
    let ctx = PromoteCtx { fixtures_dir, gate, prescan, repo, diff: &diff, log: &log };

    if aggregate {
        let min = cfg.pointer("/feedback/promoteMinFpCount").and_then(Value::as_f64).unwrap_or(2.0);
        let r = promote_from_feedback_log(&log_file, &patterns_file, min, &ctx)?;
        println!("{}", format_promote_report(&r));
        return Ok(if r.fixtures.iter().any(|f| !f.result.ok && !f.result.skipped) { 1 } else { 0 });
    }

    let pr = (cli.pr_number > 0).then(|| json!(cli.pr_number));
    let r = promote_candidates_from_dir(&candidates_dir, pr.as_ref(), &ctx)?;
    println!("=== promote candidates ===");
    println!("promoted: {}, skipped: {}, failed: {}", r.promoted.len(), r.skipped.len(), r.failed.len());
    for x in r.promoted.iter().chain(&r.skipped).chain(&r.failed) {
        let status = if x.ok {
            "ok".to_string()
        } else if x.skipped {
            "skip".to_string()
        } else {
            x.reason()
        };
        println!("- {}: {status}", x.id);
    }
    Ok(if r.failed.is_empty() { 0 } else { 1 })
}

pub fn run(args: Args) -> Result<i32> {
    match run_inner(&args.argv) {
        Ok(code) => Ok(code),
        Err(e) => {
            eprintln!("[promote-feedback] {e:#}");
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_и_resolve_не_выпускают_за_корень() {
        assert_eq!(sanitize_fixture_id("fp-pr-7-x-L12").as_deref(), Some("fp-pr-7-x-L12"));
        assert_eq!(sanitize_fixture_id("fp-pr-7-unsafe-eval-L----------escape").as_deref(), Some("fp-pr-7-unsafe-eval-L----------escape"));
        for bad in ["", "..", "a/b", "a\\b", "-x", ".hidden", "a..b", &"a".repeat(65)] {
            assert_eq!(sanitize_fixture_id(bad), None, "{bad}");
        }
        let root = Path::new("/tmp/fx");
        assert_eq!(resolve_fixture_dir(root, "ok"), Some(PathBuf::from("/tmp/fx/ok")));
        assert_eq!(resolve_fixture_dir(root, "../x"), None);
    }

    #[test]
    fn match_pr_file_patch_с_app_dir_и_без() {
        let files = vec![json!({"filename": "server/a.ts", "patch": "p"})];
        assert!(match_pr_file_patch(&files, "app/server/a.ts", "app").is_some());
        assert!(match_pr_file_patch(&files, "server/a.ts", "app").is_some());
        assert!(match_pr_file_patch(&files, "server/b.ts", "").is_none());
    }

    #[test]
    fn group_и_infer_как_в_js() {
        let mk = |pr: u64, title: &str| {
            json!({ "signal": "fp", "pr": pr, "finding": { "file": "a.ts", "severity": "major", "title": title, "line": 1 } })
        };
        let groups = group_fp_entries(&[mk(1, "same"), mk(2, "same"), mk(3, "other")], 2.0);
        assert_eq!(groups.len(), 1);
        assert_eq!(fp_aggregate_key(&mk(1, "same")), "a.ts|major|same");
        assert_eq!(infer_pattern_id(&mk(1, "нет e2e теста")), Some("e2e-in-diff"));
        assert_eq!(infer_pattern_id(&mk(1, "Файл preps.ts не в PR")), Some("file-absent-in-diff"));
        assert_eq!(infer_pattern_id(&mk(1, "unsafe eval")), None);
    }

    #[test]
    fn known_patterns_разбираются_как_fp_patterns() {
        for id in ["e2e-in-diff", "file-absent-in-diff"] {
            let line = js_stringify(&known_pattern(id).unwrap());
            let parsed = parse_fp_patterns_text(&line);
            assert_eq!(parsed.len(), 1, "{id}");
            assert_eq!(parsed[0].id, id);
        }
    }

    #[test]
    fn js_resolve_снимает_точки() {
        assert_eq!(js_resolve(Path::new("/a/b"), "../c/./d"), PathBuf::from("/a/c/d"));
        assert_eq!(js_resolve(Path::new("/a"), "/abs"), PathBuf::from("/abs"));
    }
}
