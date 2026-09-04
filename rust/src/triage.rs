//! Порт lib/triage.mjs (Фаза 3, шаг 7): auto-triage unresolved review threads
//! (known FP + advisory-only + superseded findings). Потребители: pipeline.mjs
//! (runAutoTriage над github-клиентом), тесты.
//!
//! Паритет с JS (проверяется rust/parity/triage.parity.mjs):
//!   - findingLocationKey: линия схлопывается в бакет по 3 строки
//!     (`Math.floor((ln - 1) / 3)`), ln <= 0/NaN → 'out';
//!   - parseInlineReviewTitle: регекс с (?i)(?m) — leftmost-first семантика
//!     regex-crate совпадает с V8 (жадный [^\n]* и (.*) с бэктрекингом);
//!   - matchesKnownFalsePositive НЕ портирован (JS lookahead в FP-паттернах,
//!     см. lib/findings.mjs) — обёртка считает FP-совпадение на JS-стороне и
//!     передаёт бинарю булевым аргументом knownFp; матрица решений сравнивается
//!     при одинаковом knownFp;
//!   - MARKERS.inline приходит аргументом (профиль живёт на JS-стороне),
//!     hasMarker — startsWith в начале body;
//!   - trustedAuthors: exact allowlist (case-insensitive) + user.type Bot;
//!   - buildTriageContext возвращает ключи МНОЖЕСТВ — через JSON они идут
//!     массивами; JS-обёртка собирает Set обратно (публичный API прежний);
//!   - runAutoTriage (оркестрация над github-клиентом с DI) не портирован —
//!     остался чистым JS.

use std::collections::HashSet;

use serde_json::{json, Value};

use crate::findings::{js_to_number, js_to_string, js_truthy};

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
    let Some(login) = login else { return false };
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

/// Decide whether to auto-resolve a review thread. `known_fp` считает
/// JS-обёртка (matchesKnownFalsePositive остался в JS — lookahead в паттернах).
pub fn decide_auto_triage(
    thread: Option<&Value>,
    ctx: Option<&Value>,
    inline_marker: &str,
    known_fp: bool,
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

    let _ = text; // text нужен JS-обёртке для knownFp; здесь решение приходит аргументом.
    if known_fp {
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
        .filter(|f| js_truthy(f.get("severity").unwrap_or(&Value::Null)) && js_truthy(f.get("actionabilityPromoted").unwrap_or(&Value::Null)))
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
        let d = decide_auto_triage(Some(&mk_thread("чужой комментарий", author, "a.ts", 1)), Some(&ctx), MARKER, false);
        assert_eq!(d, json!({ "action": "skip", "reason": "not_ai_inline" }));

        // недоверенный автор
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n**critical: x**"), "human", "a.ts", 1)),
            Some(&ctx), MARKER, false,
        );
        assert_eq!(d, json!({ "action": "skip", "reason": "untrusted_author" }));

        // активный блокер
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n🔴 **critical: blocker**"), author, "a.ts", 2)),
            Some(&ctx), MARKER, false,
        );
        assert_eq!(d, json!({ "action": "skip", "reason": "active_blocker" }));

        // advisory (minor, не поднятая)
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n🟡 **minor: advisory**"), author, "b.ts", 2)),
            Some(&ctx), MARKER, false,
        );
        assert_eq!(d["action"], json!("resolve"));
        assert_eq!(d["reason"], json!("advisory_only"));
        assert!(d["reply"].as_str().unwrap().contains("critical/major"));
        assert!(d["reply"].as_str().unwrap().contains("abcdef1"));

        // advisory поднятая в строку — не резолвится
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n🔵 **nit: promoted**"), author, "d.ts", 4)),
            Some(&ctx), MARKER, false,
        );
        assert_eq!(d, json!({ "action": "skip", "reason": "still_active" }));

        // superseded — нет в текущем прогоне
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n🟠 **major: gone**"), author, "zzz.ts", 9)),
            Some(&ctx), MARKER, false,
        );
        assert_eq!(d["action"], json!("resolve"));
        assert_eq!(d["reason"], json!("superseded"));

        // known_fp
        let d = decide_auto_triage(
            Some(&mk_thread(&format!("{MARKER}\n🟡 **minor: fp**"), author, "zzz.ts", 9)),
            Some(&ctx), MARKER, true,
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
            Some(&ctx2), MARKER, false,
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
            Some(&ctx3), MARKER, false,
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
}
