//! Порт lib/triage.mjs: auto-triage unresolved review threads (known FP +
//! advisory-only + superseded findings). Потребитель — стадия публикации
//! пайплайна (`pipeline.mjs:1003-1021`): `build_triage_context` →
//! `run_auto_triage` над [`GithubApi`].
//!
//! Паритет с JS:
//!   - findingLocationKey: линия схлопывается в бакет по 3 строки
//!     (`Math.floor((ln - 1) / 3)`), ln <= 0/NaN → 'out';
//!   - parseInlineReviewTitle: регекс с (?i)(?m) — leftmost-first семантика
//!     regex-crate совпадает с V8 (жадный [^\n]* и (.*) с бэктрекингом);
//!   - matchesKnownFalsePositive считается здесь же (`fp_patterns`, fancy-regex
//!     с lookahead): `ctx.patternsFile` → `load_fp_patterns`, иначе вшитый набор
//!     пакета — тот же файл, что `DEFAULT_FP_PATTERNS_PATH` в JS. Паттерны
//!     грузятся один раз на прогон (в JS — кеш `getFpPatterns`);
//!   - MARKERS.inline приходит аргументом (профиль — `profile::Markers`),
//!     hasMarker — startsWith в начале body;
//!   - trustedAuthors: exact allowlist (case-insensitive) + user.type Bot;
//!   - advisory, поднятая в строку по доказуемости (`actionabilityPromoted`),
//!     в `advisoryOnly` не попадает: бот не закрывает тред раньше человека;
//!   - buildTriageContext возвращает ключи МНОЖЕСТВ массивами (JSON не несёт Set);
//!   - runAutoTriage fail-open: сбой загрузки тредов → `{0, 0, 1}`, сбой
//!     ответа/резолва одного треда → `errors++` и следующий тред.

use std::collections::HashSet;

use serde::Serialize;
use serde_json::{json, Value};

use crate::findings::{js_to_number, js_to_string, js_truthy, matches_known_false_positive};
use crate::fp_patterns::{default_fp_patterns, load_fp_patterns, FpPattern};
use crate::traits::{GithubApi, ThreadRef};

pub const AUTO_TRIAGE_MARKER: &str = "🤖 auto-triage";
pub const DEFAULT_TRUSTED_AUTHORS: &str = "github-actions[bot]";

/// `<file>|<bucket>`: бакет — `Math.floor((ln - 1) / 3)` для ln > 0, иначе 'out'.
pub fn finding_location_key(file: Option<&Value>, line: Option<&Value>) -> String {
    let ln = line.and_then(js_to_number).unwrap_or(0.0);
    let bucket = if ln > 0.0 {
        format!("{}", ((ln - 1.0) / 3.0).floor() as i64)
    } else {
        "out".to_string()
    };
    format!("{}|{}", js_to_string_opt(file), bucket)
}

fn js_to_string_opt(v: Option<&Value>) -> String {
    match v {
        Some(x) => js_to_string(x),
        None => "undefined".to_string(),
    }
}

fn title_re() -> regex::Regex {
    // /^\s*[^\n]*\*\*(?:critical|major|minor|nit)(?:\s*\(.+\))?:\s*(.*)\*\*/im
    regex::Regex::new(r"(?im)^\s*[^\n]*\*\*(?:critical|major|minor|nit)(?:\s*\(.+\))?:\s*(.*)\*\*")
        .expect("валидный регекс parseInlineReviewTitle")
}

/// Заголовок находки из тела inline-комментария движка.
pub fn parse_inline_review_title(body: Option<&Value>) -> String {
    let text = js_to_string_opt(body);
    match title_re().captures(&text) {
        Some(caps) => caps.get(1).map(|m| m.as_str().trim().to_string()).unwrap_or_default(),
        None => String::new(),
    }
}

/// split(',').trim().filter(Boolean).
/// None — это JS-значение по умолчанию из сигнатуры
/// `parseTrustedAuthors(raw = DEFAULT_TRUSTED_AUTHORS)` (lib/feedback.mjs:30):
/// default-параметр срабатывает на undefined, поэтому отсутствие аргумента даёт
/// дефолтный allowlist, а не строку "undefined".
pub fn parse_trusted_authors(raw: Option<&Value>) -> Vec<String> {
    let s = match raw {
        Some(v) => js_to_string(v),
        None => DEFAULT_TRUSTED_AUTHORS.to_string(),
    };
    s.split(',').map(str::trim).filter(|p| !p.is_empty()).map(String::from).collect()
}

/// Доверенный автор — только exact allowlist (case-insensitive) + user.type Bot.
pub fn is_trusted_review_author(login: Option<&Value>, trusted_authors: &[String], user: Option<&Value>) -> bool {
    if trusted_authors.is_empty() {
        return true;
    }
    // `if (!login) return false`: null/'' — не автор.
    let Some(login) = login.filter(|l| js_truthy(l)) else { return false };
    let l = js_to_string(login).to_lowercase();
    if !trusted_authors.iter().any(|a| l == a.to_lowercase()) {
        return false;
    }
    if let Some(Value::Object(u)) = user {
        if let Some(t) = u.get("type") {
            if js_truthy(t) && js_to_string(t) != "Bot" {
                return false;
            }
        }
    }
    true
}

fn str_set(v: Option<&Value>) -> HashSet<String> {
    match v {
        Some(Value::Array(a)) => a.iter().map(js_to_string).collect(),
        _ => HashSet::new(),
    }
}

fn str_vec(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().map(js_to_string).collect(),
        _ => vec![],
    }
}

fn sha7(ctx: Option<&Value>) -> String {
    let sha = ctx
        .and_then(|c| c.get("headSha"))
        .filter(|x| js_truthy(x))
        .map(js_to_string)
        .unwrap_or_default();
    // ctx.headSha?.slice(0, 7) || '?' — по UTF-16 code units (ascii в практике).
    let cut: String = sha.chars().take(7).collect();
    if cut.is_empty() {
        "?".to_string()
    } else {
        cut
    }
}

/// Decide whether to auto-resolve a review thread (`decideAutoTriage`).
/// `patterns` — FP-набор прогона (см. [`fp_patterns_for_ctx`]).
pub fn decide_auto_triage(
    thread: Option<&Value>,
    ctx: Option<&Value>,
    inline_marker: &str,
    patterns: &[FpPattern],
) -> Value {
    let t = thread.and_then(Value::as_object);
    let first_comment = t
        .and_then(|t| t.get("comments"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first());
    let first = first_comment
        .and_then(|c| c.get("body"))
        .filter(|x| js_truthy(x))
        .map(js_to_string)
        .unwrap_or_default();

    // hasMarker: маркер в НАЧАЛЕ body.
    if !first.starts_with(inline_marker) {
        return json!({ "action": "skip", "reason": "not_ai_inline" });
    }

    let ctx_obj = ctx.and_then(Value::as_object);
    let trusted_authors = match ctx_obj.and_then(|c| c.get("trustedAuthors")) {
        Some(Value::Array(a)) if !a.is_empty() => a.iter().map(js_to_string).collect::<Vec<_>>(),
        _ => parse_trusted_authors(Some(&Value::String(DEFAULT_TRUSTED_AUTHORS.into()))),
    };
    let author = first_comment.and_then(|c| c.get("author"));
    let user = first_comment.and_then(|c| c.get("user"));
    if !is_trusted_review_author(author, &trusted_authors, user) {
        return json!({ "action": "skip", "reason": "untrusted_author" });
    }

    let path = t
        .and_then(|t| t.get("path"))
        .filter(|x| js_truthy(x))
        .map(js_to_string)
        .unwrap_or_default();
    let line = t.and_then(|t| t.get("line")).and_then(js_to_number).unwrap_or(0.0);
    let key = finding_location_key(Some(&Value::String(path)), Some(&json!(line)));
    let text = format!("{} {}", parse_inline_review_title(Some(&Value::String(first.clone()))), first);

    if str_set(ctx_obj.and_then(|c| c.get("blockerKeys"))).contains(&key) {
        return json!({ "action": "skip", "reason": "active_blocker" });
    }

    // matchesKnownFalsePositive(text, ctx.changedPaths || [], …): после
    // blockerKeys — активный блокер не снимается даже совпавшим FP-паттерном.
    let changed_paths = str_vec(ctx_obj.and_then(|c| c.get("changedPaths")));
    if matches_known_false_positive(&text, &changed_paths, patterns) {
        return json!({
            "action": "resolve",
            "reason": "known_fp",
            "reply": format!("{inline_marker}\n✅ **auto-triage:** известный false positive (e2e-in-diff / file-in-diff pattern). SHA `{}`.\n\n<sub>🤖 auto-triage</sub>", sha7(ctx)),
        });
    }

    if str_set(ctx_obj.and_then(|c| c.get("advisoryOnly"))).contains(&key) {
        let block_on = str_vec(ctx_obj.and_then(|c| c.get("blockOn"))).join("/");
        let block_on = if block_on.is_empty() { "non-blocker".to_string() } else { block_on };
        return json!({
            "action": "resolve",
            "reason": "advisory_only",
            "reply": format!("{inline_marker}\n✅ **auto-triage:** advisory ({block_on}) — не блокирует merge. SHA `{}`.\n\n<sub>🤖 auto-triage</sub>", sha7(ctx)),
        });
    }

    if !str_set(ctx_obj.and_then(|c| c.get("currentKeys"))).contains(&key) {
        return json!({
            "action": "resolve",
            "reason": "superseded",
            "reply": format!("{inline_marker}\n✅ **auto-triage:** finding не воспроизведён в последнем прогоне (исправлено или отфильтровано). SHA `{}`.\n\n<sub>🤖 auto-triage</sub>", sha7(ctx)),
        });
    }

    json!({ "action": "skip", "reason": "still_active" })
}

/// buildTriageContext: ключи множеств уходят массивами (JSON не несёт Set);
/// JS-обёртка собирает Set обратно — публичный API прежний.
pub fn build_triage_context(input: Option<&Value>) -> Value {
    let obj = input.and_then(Value::as_object);
    let get = |k: &str| obj.and_then(|o| o.get(k));

    let all_findings: Vec<Value> = match get("allFindings") {
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    };
    let blockers: Vec<Value> = match get("blockers") {
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    };
    let key_of = |f: &Value| finding_location_key(f.get("file"), f.get("line"));

    let current_keys: Vec<String> = all_findings.iter().map(key_of).collect();
    let blocker_keys: Vec<String> = blockers.iter().map(key_of).collect();
    let blocker_set: HashSet<String> = blocker_keys.iter().cloned().collect();

    // `new Set(blockOn || ['critical','major'])` + `blockOn: [...tiers]`: JS-Set
    // хранит порядок вставки, и он виден наружу (`ctx.blockOn.join('/')` в reply).
    // Поэтому здесь упорядоченный Vec с дедупликацией, а HashSet — только для
    // проверок принадлежности.
    let tiers_order: Vec<String> = match get("blockOn") {
        Some(Value::Array(a)) => a.iter().map(js_to_string).collect(),
        _ => vec!["critical".to_string(), "major".to_string()],
    };
    let mut seen: HashSet<String> = HashSet::new();
    let tiers_order: Vec<String> = tiers_order.into_iter().filter(|t| seen.insert(t.clone())).collect();
    let tiers: HashSet<String> = tiers_order.iter().cloned().collect();
    let block_on_keys: HashSet<String> = all_findings
        .iter()
        .filter(|f| {
            f.get("severity")
                .map(|s| tiers.contains(&js_to_string(s)))
                .unwrap_or(false)
        })
        .map(key_of)
        .collect();
    let promoted_keys: HashSet<String> = all_findings
        .iter()
        .filter(|f| js_truthy(f.get("actionabilityPromoted").unwrap_or(&Value::Null)))
        .map(key_of)
        .collect();
    // Advisory, поднятая в строку по доказуемости (actionabilityPromoted), из
    // авто-резолва исключается — тред ждёт человека.
    let advisory_only: Vec<String> = all_findings
        .iter()
        .filter(|f| {
            !f.get("severity")
                .map(|s| tiers.contains(&js_to_string(s)))
                .unwrap_or(false)
        })
        .map(key_of)
        .filter(|k| !blocker_set.contains(k) && !block_on_keys.contains(k) && !promoted_keys.contains(k))
        .collect();

    let trusted_authors = match get("trustedAuthors") {
        Some(Value::Array(a)) if !a.is_empty() => Value::Array(a.clone()),
        _ => Value::Array(
            parse_trusted_authors(Some(&Value::String(DEFAULT_TRUSTED_AUTHORS.into())))
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
    };

    let mut out = serde_json::Map::new();
    out.insert("currentKeys".into(), Value::Array(current_keys.into_iter().map(Value::String).collect()));
    out.insert("blockerKeys".into(), Value::Array(blocker_keys.into_iter().map(Value::String).collect()));
    out.insert("advisoryOnly".into(), Value::Array(advisory_only.into_iter().map(Value::String).collect()));
    out.insert("blockOn".into(), Value::Array(tiers_order.into_iter().map(Value::String).collect()));
    out.insert("changedPaths".into(), get("changedPaths").cloned().unwrap_or(Value::Array(vec![])));
    out.insert("headSha".into(), get("headSha").cloned().unwrap_or(Value::Null));
    out.insert("patternsFile".into(), get("patternsFile").cloned().unwrap_or(Value::Null));
    out.insert("trustedAuthors".into(), trusted_authors);
    Value::Object(out)
}

/// FP-набор прогона: `ctx.patternsFile || DEFAULT_FP_PATTERNS_PATH`. Пустой или
/// нечитаемый файл откатывается на вшитый набор (`load_fp_patterns`) — как в JS.
/// Путь приходит из конфига (`cfg.feedback.patternsFile`), не от автора PR.
/// Вшитый набор не копируется: `FpPattern` держит скомпилированные регексы.
pub enum FpSet {
    Embedded(&'static [FpPattern]),
    Owned(Vec<FpPattern>),
}

impl std::ops::Deref for FpSet {
    type Target = [FpPattern];
    fn deref(&self) -> &[FpPattern] {
        match self {
            FpSet::Embedded(p) => p,
            FpSet::Owned(v) => v,
        }
    }
}

pub fn fp_patterns_for_ctx(ctx: &Value) -> FpSet {
    match ctx.get("patternsFile") {
        Some(v) if js_truthy(v) => FpSet::Owned(load_fp_patterns(std::path::Path::new(&js_to_string(v)))),
        _ => FpSet::Embedded(default_fp_patterns()),
    }
}

/// Итог `runAutoTriage` — уходит в `obs.logEvent('auto_triage', …)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TriageStats {
    pub resolved: usize,
    pub skipped: usize,
    pub errors: usize,
}

/// `${thread.line}` в логе: `null` у треда на удалённой строке.
fn line_label(line: Option<u64>) -> String {
    line.map_or_else(|| "null".to_string(), |l| l.to_string())
}

/// Resolve eligible threads; fail-open (`runAutoTriage`, `lib/triage.mjs:73-110`).
///
/// Треды, решение по которым `skip` (в т.ч. активный блокер и поднятая
/// по доказуемости advisory — `still_active`), не трогаются вовсе. Ответ в тред
/// пишется один раз: если в нём уже есть [`AUTO_TRIAGE_MARKER`], повторный
/// прогон только резолвит.
pub fn run_auto_triage(
    github: &dyn GithubApi,
    ctx: &Value,
    inline_marker: &str,
    log: &dyn Fn(&str),
) -> TriageStats {
    let threads: Vec<ThreadRef> = match github.list_unresolved_review_threads() {
        Ok(t) => t,
        Err(e) => {
            log(&format!("auto-triage: не удалось загрузить threads (fail-open): {e}"));
            return TriageStats { resolved: 0, skipped: 0, errors: 1 };
        }
    };
    let patterns = fp_patterns_for_ctx(ctx);
    let mut stats = TriageStats::default();

    for thread in &threads {
        let tv = serde_json::to_value(thread).unwrap_or(Value::Null);
        let decision = decide_auto_triage(Some(&tv), Some(ctx), inline_marker, &patterns);
        if decision.get("action").and_then(Value::as_str) != Some("resolve") {
            stats.skipped += 1;
            continue;
        }
        let already_triaged = thread.comments.iter().any(|c| c.body.contains(AUTO_TRIAGE_MARKER));
        let reply = decision.get("reply").and_then(Value::as_str).filter(|r| !r.is_empty());
        let res = (|| -> anyhow::Result<()> {
            if let Some(reply) = reply {
                if !already_triaged {
                    github.reply_review_thread(&thread.id, reply)?;
                }
            }
            github.resolve_review_thread(&thread.id)
        })();
        match res {
            Ok(()) => {
                stats.resolved += 1;
                let reason = decision.get("reason").and_then(Value::as_str).unwrap_or("");
                log(&format!(
                    "auto-triage: resolved {}:{} ({reason})",
                    thread.path,
                    line_label(thread.line)
                ));
            }
            Err(e) => {
                stats.errors += 1;
                log(&format!("auto-triage: thread {} fail-open: {e}", thread.id));
            }
        }
    }
    stats
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_key_buckets() {
        assert_eq!(finding_location_key(Some(&json!("a.ts")), Some(&json!(1))), "a.ts|0");
        assert_eq!(finding_location_key(Some(&json!("a.ts")), Some(&json!(3))), "a.ts|0");
        assert_eq!(finding_location_key(Some(&json!("a.ts")), Some(&json!(4))), "a.ts|1");
        assert_eq!(finding_location_key(Some(&json!("a.ts")), Some(&json!(0))), "a.ts|out");
        assert_eq!(finding_location_key(Some(&json!("a.ts")), Some(&json!(-5))), "a.ts|out");
        assert_eq!(finding_location_key(Some(&json!("a.ts")), Some(&json!("bad"))), "a.ts|out");
        assert_eq!(finding_location_key(Some(&json!("a.ts")), None), "a.ts|out");
        assert_eq!(finding_location_key(None, Some(&json!(2))), "undefined|0");
    }

    #[test]
    fn inline_title_regex() {
        assert_eq!(
            parse_inline_review_title(Some(&json!("<!-- m -->\n🟡 **minor (m): x**"))),
            "x"
        );
        assert_eq!(
            parse_inline_review_title(Some(&json!("**critical: plain**"))),
            "plain"
        );
        assert_eq!(
            parse_inline_review_title(Some(&json!("pre\n**MAJOR: case** tail"))),
            "case"
        );
        assert_eq!(
            parse_inline_review_title(Some(&json!("🟠 **major (m1): two words**"))),
            "two words"
        );
        assert_eq!(parse_inline_review_title(Some(&json!("no finding here"))), "");
        assert_eq!(parse_inline_review_title(Some(&json!(""))), "");
        assert_eq!(parse_inline_review_title(None), "");
        // жадный захват до последнего `**`
        assert_eq!(
            parse_inline_review_title(Some(&json!("**minor: a ** b**"))),
            "a ** b"
        );
    }

    #[test]
    fn trusted_authors_matrix() {
        let authors = parse_trusted_authors(Some(&json!(" a , B ,,c ")));
        assert_eq!(authors, vec!["a", "B", "c"]);
        assert_eq!(parse_trusted_authors(None), vec!["github-actions[bot]"]);
        // пустой список — всё доверенно
        assert!(is_trusted_review_author(Some(&json!("anyone")), &[], None));
        assert!(!is_trusted_review_author(None, &authors, None));
        assert!(is_trusted_review_author(Some(&json!("A")), &authors, None));
        assert!(!is_trusted_review_author(Some(&json!("d")), &authors, None));
        // user.type !== Bot отклоняет даже из allowlist
        assert!(!is_trusted_review_author(
            Some(&json!("a")),
            &authors,
            Some(&json!({ "type": "User" }))
        ));
        assert!(is_trusted_review_author(
            Some(&json!("a")),
            &authors,
            Some(&json!({ "type": "Bot" }))
        ));
    }

    fn mk_thread(body: &str, author: &str, path: &str, line: i64) -> Value {
        json!({
            "id": "t1",
            "path": path,
            "line": line,
            "comments": [{ "body": body, "author": author, "user": { "type": "Bot", "login": author } }],
        })
    }

    const MARKER: &str = "<!-- ai-review-inline -->";

    #[test]
    fn decide_matrix() {
        let ctx = build_triage_context(Some(&json!({
            "allFindings": [
                { "file": "a.ts", "line": 1, "severity": "critical" },
                { "file": "b.ts", "line": 2, "severity": "minor" },
                { "file": "d.ts", "line": 4, "severity": "nit", "actionabilityPromoted": true },
            ],
            "blockers": [{ "file": "a.ts", "line": 1, "severity": "critical" }],
            "changedPaths": [],
            "headSha": "abcdef1234",
        })));
        let author = "github-actions[bot]";

        // не наш комментарий
        let d = decide_auto_triage(Some(&mk_thread("чужой комментарий", author, "a.ts", 1)), Some(&ctx), MARKER, &[]);
        assert_eq!(d, json!({ "action": "skip", "reason": "not_ai_inline" }));

        // недоверенный автор
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n**critical: x**"), "human", "a.ts", 1)),
            Some(&ctx), MARKER, &[],
        );
        assert_eq!(d, json!({ "action": "skip", "reason": "untrusted_author" }));

        // активный блокер
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n🔴 **critical: blocker**"), author, "a.ts", 2)),
            Some(&ctx), MARKER, &[],
        );
        assert_eq!(d, json!({ "action": "skip", "reason": "active_blocker" }));

        // advisory (minor, не поднятая)
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n🟡 **minor: advisory**"), author, "b.ts", 2)),
            Some(&ctx), MARKER, &[],
        );
        assert_eq!(d["action"], json!("resolve"));
        assert_eq!(d["reason"], json!("advisory_only"));
        assert!(d["reply"].as_str().unwrap().contains("critical/major"));
        assert!(d["reply"].as_str().unwrap().contains("abcdef1"));

        // advisory поднятая в строку — не резолвится
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n🔵 **nit: promoted**"), author, "d.ts", 4)),
            Some(&ctx), MARKER, &[],
        );
        assert_eq!(d, json!({ "action": "skip", "reason": "still_active" }));

        // superseded — нет в текущем прогоне
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n🟠 **major: gone**"), author, "zzz.ts", 9)),
            Some(&ctx), MARKER, &[],
        );
        assert_eq!(d["action"], json!("resolve"));
        assert_eq!(d["reason"], json!("superseded"));

        // known_fp: вшитый набор пакета (e2e-in-diff), requiresPath — по changedPaths ctx
        let ctx_fp = build_triage_context(Some(&json!({
            "allFindings": [], "changedPaths": ["e2e/login.spec.ts"], "headSha": "abcdef1234",
        })));
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n🟡 **minor: no e2e tests**"), author, "zzz.ts", 9)),
            Some(&ctx_fp), MARKER, default_fp_patterns(),
        );
        assert_eq!(d["action"], json!("resolve"));
        assert_eq!(d["reason"], json!("known_fp"));
        assert!(d["reply"].as_str().unwrap().contains("false positive"));

        // major при blockOn=['critical'] — advisory: порядок проверок в JS
        // (blockerKeys → knownFp → advisoryOnly → currentKeys) отдаёт advisory_only
        // раньше, чем still_active; пустой headSha → SHA `?`.
        let ctx2 = build_triage_context(Some(&json!({
            "allFindings": [{ "file": "e.ts", "line": 7, "severity": "major" }],
            "blockers": [],
            "blockOn": ["critical"],
            "changedPaths": [],
            "headSha": "",
        })));
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n🟠 **major: active advisory**"), author, "e.ts", 7)),
            Some(&ctx2), MARKER, &[],
        );
        assert_eq!(d["action"], json!("resolve"));
        assert_eq!(d["reason"], json!("advisory_only"));
        assert!(d["reply"].as_str().unwrap().contains("advisory (critical)"));
        assert!(d["reply"].as_str().unwrap().contains("SHA `?`"));
        // пустой прогон: находок нет вовсе → superseded; headSha отсутствует → SHA `?`
        let ctx3 = build_triage_context(Some(&json!({
            "allFindings": [], "blockers": [], "blockOn": ["critical", "major"],
        })));
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n🟡 **minor: m**"), author, "e.ts", 7)),
            Some(&ctx3), MARKER, &[],
        );
        assert_eq!(d["reason"], json!("superseded"));
        assert!(d["reply"].as_str().unwrap().contains("SHA `?`"));
    }

    #[test]
    fn context_build_sets() {
        let ctx = build_triage_context(Some(&json!({
            "allFindings": [
                { "file": "a.ts", "line": 1, "severity": "critical" },
                { "file": "b.ts", "line": 2, "severity": "major" },
                { "file": "c.ts", "line": 3, "severity": "minor" },
                { "file": "d.ts", "line": 4, "severity": "nit", "actionabilityPromoted": true },
                { "file": "e.ts", "line": 5, "severity": "nit" },
            ],
            "blockers": [{ "file": "a.ts", "line": 2, "severity": "critical" }],
            "blockOn": ["critical"],
        })));
        // blockOn=['critical'] → advisory всё, кроме critical: major(b, строка 2 → бакет 0),
        // minor(c, строка 3 → бакет 0), nit(e, строка 5 → бакет 1); nit promoted (d) исключена;
        // a.ts|0 одновременно blocker и blockOn.
        let advisory: Vec<String> = ctx["advisoryOnly"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert_eq!(advisory, vec!["b.ts|0", "c.ts|0", "e.ts|1"]);
        assert_eq!(ctx["blockOn"], json!(["critical"]));
        assert_eq!(ctx["trustedAuthors"], json!(["github-actions[bot]"]));
        // пустой blockOn → дефолт critical/major
        let ctx2 = build_triage_context(Some(&json!({ "allFindings": [{ "file": "x", "line": 1, "severity": "major" }] })));
        assert_eq!(ctx2["blockOn"], json!(["critical", "major"]));
        assert!(ctx2["advisoryOnly"].as_array().unwrap().is_empty());
    }

    // ---- run_auto_triage над GithubApi ----

    use crate::testutil::{CallRecord, FakeGithub};
    use crate::traits::ThreadComment;
    use std::sync::Mutex;

    fn tref(id: &str, path: &str, line: Option<u64>, bodies: &[(&str, Option<&str>)]) -> ThreadRef {
        ThreadRef {
            id: id.into(),
            path: path.into(),
            line,
            comments: bodies
                .iter()
                .map(|(b, a)| ThreadComment { body: b.to_string(), author: a.map(str::to_string) })
                .collect(),
        }
    }

    fn collect_log() -> (std::sync::Arc<Mutex<Vec<String>>>, impl Fn(&str)) {
        let buf = std::sync::Arc::new(Mutex::new(Vec::new()));
        let b2 = buf.clone();
        (buf, move |m: &str| b2.lock().unwrap().push(m.to_string()))
    }

    #[test]
    fn run_list_failure_is_fail_open() {
        let gh = FakeGithub::new().failing(|r| match r {
            CallRecord::ListUnresolvedReviewThreads => Some("graphql 502".into()),
            _ => None,
        });
        let (buf, log) = collect_log();
        let stats = run_auto_triage(&gh, &build_triage_context(None), MARKER, &log);
        assert_eq!(stats, TriageStats { resolved: 0, skipped: 0, errors: 1 });
        assert_eq!(
            buf.lock().unwrap().as_slice(),
            ["auto-triage: не удалось загрузить threads (fail-open): graphql 502"]
        );
        assert_eq!(serde_json::to_value(stats).unwrap(), json!({ "resolved": 0, "skipped": 0, "errors": 1 }));
    }

    #[test]
    fn run_matrix_over_fake_github() {
        let bot = Some("github-actions[bot]");
        let ctx = build_triage_context(Some(&json!({
            "allFindings": [
                { "file": "a.ts", "line": 1, "severity": "critical" },
                { "file": "b.ts", "line": 2, "severity": "minor" },
                // поднятая в строку advisory — тред ждёт человека
                { "file": "d.ts", "line": 4, "severity": "nit", "actionabilityPromoted": true, "actionabilityScore": 3 },
            ],
            "blockers": [{ "file": "a.ts", "line": 1, "severity": "critical" }],
            "headSha": "abcdef1234",
        })));
        let m = |t: &str| format!("{MARKER}\n{t}");
        let threads = vec![
            tref("t-blocker", "a.ts", Some(1), &[(&m("🔴 **critical: x**"), bot)]),
            tref("t-promoted", "d.ts", Some(4), &[(&m("🔵 **nit: read me**"), bot)]),
            tref("t-advisory", "b.ts", Some(2), &[(&m("🟡 **minor: adv**"), bot)]),
            // уже отвечали — только резолв, без второго ответа
            tref(
                "t-again",
                "gone.ts",
                None,
                &[(&m("🟠 **major: old**"), bot), ("✅ …\n<sub>🤖 auto-triage</sub>", bot)],
            ),
            tref("t-human", "gone.ts", Some(9), &[(&m("🟠 **major: spoof**"), Some("mallory"))]),
            tref("t-noauthor", "gone.ts", Some(9), &[(&m("🟠 **major: ghost**"), None)]),
            tref("t-foreign", "gone.ts", Some(9), &[("обычный комментарий", bot)]),
            // ответ упал — errors++, цикл идёт дальше
            tref("t-broken", "gone.ts", Some(12), &[(&m("🟠 **major: y**"), bot)]),
            tref("t-last", "gone.ts", Some(30), &[(&m("🟡 **minor: z**"), bot)]),
        ];
        let gh = FakeGithub::new().with_threads(threads).failing(|r| match r {
            CallRecord::ReplyReviewThread { thread_id, .. } if thread_id == "t-broken" => {
                Some("mutation failed".into())
            }
            _ => None,
        });
        let (buf, log) = collect_log();
        let stats = run_auto_triage(&gh, &ctx, MARKER, &log);
        assert_eq!(stats, TriageStats { resolved: 3, skipped: 5, errors: 1 });

        let calls: Vec<String> = gh
            .core
            .calls
            .records()
            .into_iter()
            .filter_map(|r| match r {
                CallRecord::ReplyReviewThread { thread_id, body } => {
                    assert!(body.starts_with(MARKER) && body.contains("SHA `abcdef1`"));
                    Some(format!("reply:{thread_id}"))
                }
                CallRecord::ResolveReviewThread { thread_id } => Some(format!("resolve:{thread_id}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls,
            [
                "reply:t-advisory",
                "resolve:t-advisory",
                "resolve:t-again",
                "reply:t-broken",
                "reply:t-last",
                "resolve:t-last",
            ]
        );
        let logs = buf.lock().unwrap().clone();
        assert_eq!(
            logs,
            [
                "auto-triage: resolved b.ts:2 (advisory_only)",
                "auto-triage: resolved gone.ts:null (superseded)",
                "auto-triage: thread t-broken fail-open: mutation failed",
                "auto-triage: resolved gone.ts:30 (superseded)",
            ]
        );
    }

    #[test]
    fn run_known_fp_uses_patterns_file_and_falls_back() {
        let dir = std::env::temp_dir().join(format!("triage-fp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("fp.jsonl");
        std::fs::write(&file, r#"{"id":"custom","regex":"нужен\\s+мьютекс(?=\\s|$)"}"#).unwrap();
        let bot = Some("github-actions[bot]");
        // находка жива в текущем прогоне (иначе superseded), но совпала с FP — known_fp
        let ctx = build_triage_context(Some(&json!({
            "allFindings": [{ "file": "c.ts", "line": 5, "severity": "major" }],
            "headSha": "1234567890",
            "patternsFile": file.to_string_lossy(),
        })));
        let thread = tref("t1", "c.ts", Some(5), &[(&format!("{MARKER}\n🟠 **major: нужен мьютекс here**"), bot)]);
        let gh = FakeGithub::new().with_threads(vec![thread.clone()]);
        let (buf, log) = collect_log();
        assert_eq!(run_auto_triage(&gh, &ctx, MARKER, &log).resolved, 1);
        assert_eq!(buf.lock().unwrap().as_slice(), ["auto-triage: resolved c.ts:5 (known_fp)"]);

        // нечитаемый файл → вшитый набор: кастомного паттерна там нет, тред остаётся
        let ctx_missing = build_triage_context(Some(&json!({
            "allFindings": [{ "file": "c.ts", "line": 5, "severity": "major" }],
            "patternsFile": dir.join("absent.jsonl").to_string_lossy(),
        })));
        let gh = FakeGithub::new().with_threads(vec![thread]);
        let stats = run_auto_triage(&gh, &ctx_missing, MARKER, &|_| {});
        assert_eq!(stats, TriageStats { resolved: 0, skipped: 1, errors: 0 });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn promoted_without_severity_is_not_advisory() {
        // JS: promotedKeys не смотрит на severity; находка без severity — не в tiers,
        // но поднята — значит не advisory.
        let ctx = build_triage_context(Some(&json!({
            "allFindings": [{ "file": "x.ts", "line": 1, "actionabilityPromoted": true }],
        })));
        assert!(ctx["advisoryOnly"].as_array().unwrap().is_empty());
    }

    #[test]
    fn empty_login_is_untrusted() {
        let authors = vec!["".to_string(), "bot".to_string()];
        assert!(!is_trusted_review_author(Some(&json!("")), &authors, None));
        assert!(!is_trusted_review_author(Some(&Value::Null), &authors, None));
    }
}
