//! Порт lib/actionability.mjs: классификация находок inline vs advisory по
//! детерминированным сигналам доказуемости (scoreActionability/routeFindings)
//! и рендер инлайн-комментариев (findingBlock/classify из createFindingsFormatter).
//!
//! Паритет с JS-кодом сохранён построчно:
//! - порядок сигналов в reasons — порядок add() в scoreActionability;
//! - advisory сортируется по score desc стабильной сортировкой (Rust sort_by
//!   stable, как Array.prototype.sort в JS) — при равном score выигрывает
//!   более ранняя находка;
//! - мутации НЕ применяются к входным находкам: операции возвращают их
//!   списком (index + поля) — JS-обёртка накладывает на исходные объекты,
//!   сохраняя семантику разделяемых ссылок в пайплайне;
//! - чувствительные зоны: базовый регекс + `layout.sensitive` из профиля —
//!   его исходник приходит аргументом (как MARKERS в findings), компилируется
//!   с `(?i)` (профиль собирает `new RegExp(raw, 'i')`). JS-only синтаксис
//!   (lookahead и т.п.) в бинаре — ошибка; обёртка деградирует на чистый JS.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::findings::{js_number_or_zero, js_to_number, js_to_string, js_truthy};

/// Маркер детерминированных blast-radius находок (BLAST_FINDING_SOURCE в lib/constants.mjs).
pub const BLAST_FINDING_SOURCE: &str = "blast-radius";

/// Сигнал доказуемости: ключ, вес, текст объяснения. Порядок элементов — как
/// в JS ACTIONABILITY_SIGNALS (объект с тем же набором ключей).
pub const SIGNALS: &[(&str, i32, &str)] = &[
    ("prescan", 3, "подтверждено детерминированным prescan (eslint/ast-grep) на той же строке"),
    ("verified", 3, "verifier подтвердил находку"),
    ("repeated", 2, "та же находка была в прошлом прогоне (не дисперсия моделей)"),
    ("jury", 2, "жюри R5 подтвердило без возражений"),
    ("consensus", 2, "находку дали независимо несколько моделей"),
    ("confidence", 1, "модель уверена (confidence ≥ 0.8)"),
    ("sensitivePath", 1, "файл из чувствительной зоны (авторизация, деньги, миграции, гейты)"),
    ("suggestion", 1, "есть конкретный предлагаемый фикс"),
    ("logicFile", 1, "код, а не документация"),
    ("blastCaller", 2, "caller вне диффа подтверждён blast-radius (другой слой)"),
    ("outOfChunk", -4, "файл не был виден этой части ревью — доказать нечем"),
    ("juryDowngraded", -2, "жюри понизило или отклонило часть претензий"),
    ("docsOnly", -1, "замечание к документации"),
];

/// Вес и текст сигнала по ключу (как ACTIONABILITY_SIGNALS[key] в JS).
fn signal(key: &str) -> (&'static str, i32, &'static str) {
    SIGNALS
        .iter()
        .copied()
        .find(|(k, _, _)| *k == key)
        .expect("все ключи add() есть в SIGNALS")
}

#[derive(Debug, Clone, Serialize)]
pub struct SignalHit {
    pub key: &'static str,
    pub weight: i32,
    pub why: &'static str,
}

fn hit(key: &str) -> SignalHit {
    let (k, w, why) = signal(key);
    SignalHit { key: k, weight: w, why }
}

/// Ключ «файл:строка» — как findingLocationKey в JS.
pub fn finding_location_key(finding: &Value) -> String {
    let file = match finding.get("file") {
        Some(v) if js_truthy(v) => js_to_string(v),
        _ => String::new(),
    };
    let line = js_number_or_zero(finding.get("line"));
    format!("{}:{}", crate::findings::js_trim(&file), js_to_string(&Value::from(line)))
}

// ---------------------------------------------------------------------------
// Контекст сигналов (ctx у вызывающего в JS)
// ---------------------------------------------------------------------------

/// Карта файлов диффа: путь → validLines (номера RIGHT-стороны для инлайна).
pub type FilesMap = indexmap::IndexMap<String, BTreeSet<u64>>;

/// Базовые чувствительные зоны (SENSITIVE_PATH в lib/actionability.mjs).
fn base_sensitive_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(?:^|/)(?:auth|rbac|permissions?|billing|pricing|payments?|money|migrations?)(?:/|\.|$)|\.github/workflows/",
        )
        .unwrap()
    })
}

/// Сигнальный контекст: priorKeys/prescanKeys (множества «файл:строка»),
/// fileClasses (путь → класс; функцию fileClass из JS обёртка разворачивает
/// в карту заранее) и extra-регекс чувствительных зон из профиля.
pub struct ScoreCtx {
    pub prior_keys: HashSet<String>,
    pub prescan_keys: HashSet<String>,
    pub file_classes: HashMap<String, String>,
    sensitive_extra: Option<regex::Regex>,
}

impl ScoreCtx {
    pub fn new(
        prior_keys: HashSet<String>,
        prescan_keys: HashSet<String>,
        file_classes: HashMap<String, String>,
        sensitive_source: Option<&str>,
    ) -> Result<ScoreCtx> {
        let sensitive_extra = match sensitive_source {
            Some(src) if !src.is_empty() => Some(
                regex::Regex::new(&format!("(?i){src}"))
                    .context("layout.sensitive профиля не компилируется regex-крейтом")?,
            ),
            _ => None,
        };
        Ok(ScoreCtx { prior_keys, prescan_keys, file_classes, sensitive_extra })
    }

    fn is_sensitive_path(&self, path: &str) -> bool {
        if base_sensitive_re().is_match(path) {
            return true;
        }
        self.sensitive_extra.as_ref().map(|re| re.is_match(path)).unwrap_or(false)
    }
}

/// JSON-представление ctx (приходит из JS-обёртки).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScoreCtxArg {
    pub prior_keys: Vec<String>,
    pub prescan_keys: Vec<String>,
    pub file_classes: HashMap<String, String>,
    pub sensitive_source: Option<String>,
}

impl ScoreCtxArg {
    pub fn into_ctx(self) -> Result<ScoreCtx> {
        ScoreCtx::new(
            self.prior_keys.into_iter().collect(),
            self.prescan_keys.into_iter().collect(),
            self.file_classes,
            self.sensitive_source.as_deref(),
        )
    }
}

/// Инлайн физически возможен только на строке, которая есть в диффе.
pub fn is_inlineable(finding: &Value, files: &FilesMap) -> bool {
    let rec = match finding.get("file") {
        Some(v) if js_truthy(v) => files.get(&js_to_string(v)),
        _ => None,
    };
    let Some(valid) = rec else { return false };
    let line = js_number_or_zero(finding.get("line"));
    line > 0.0
        && line.fract() == 0.0
        && line <= u64::MAX as f64
        && valid.contains(&(line as u64))
}

/// Скор находки и список сработавших сигналов (в порядке add() из JS).
pub fn score_actionability(finding: &Value, ctx: &ScoreCtx) -> (i32, Vec<SignalHit>) {
    let mut reasons: Vec<SignalHit> = Vec::new();
    let mut add = |key: &str| reasons.push(hit(key));

    let empty: Vec<Value> = Vec::new();
    let claims: &[Value] = match finding.get("advisorClaims") {
        Some(Value::Array(a)) => a,
        _ => &empty,
    };
    let loc_key = finding_location_key(finding);

    if ctx.prescan_keys.contains(&loc_key) {
        add("prescan");
    }
    match finding.get("verifierStatus").and_then(Value::as_str) {
        Some("verified") => add("verified"),
        Some("rejected") | Some("downgraded") => add("juryDowngraded"),
        _ => {}
    }
    if ctx.prior_keys.contains(&loc_key) {
        add("repeated");
    }
    if !claims.is_empty()
        && claims
            .iter()
            .all(|c| c.get("status").and_then(Value::as_str) == Some("verified"))
    {
        add("jury");
    } else if claims.iter().any(|c| {
        matches!(
            c.get("status").and_then(Value::as_str),
            Some("rejected") | Some("downgraded")
        )
    }) {
        add("juryDowngraded");
    }
    if js_truthy(finding.get("consensus").unwrap_or(&Value::Null)) {
        add("consensus");
    }
    // `(finding.confidence ?? 0) >= 0.8` — отсутствующий/nullish → 0, NaN → false.
    let confidence = finding
        .get("confidence")
        .map(js_to_number)
        .unwrap_or(Some(0.0))
        .unwrap_or(0.0);
    if confidence >= 0.8 {
        add("confidence");
    }
    let file_str = match finding.get("file") {
        Some(v) if js_truthy(v) => Some(js_to_string(v)),
        _ => None,
    };
    if let Some(f) = &file_str {
        if ctx.is_sensitive_path(f) {
            add("sensitivePath");
        }
    }
    if js_truthy(finding.get("suggestion").unwrap_or(&Value::Null)) {
        add("suggestion");
    }
    if js_truthy(finding.get("blastCaller").unwrap_or(&Value::Null))
        || finding.get("source").and_then(Value::as_str) == Some(BLAST_FINDING_SOURCE)
    {
        add("blastCaller");
    }

    let cls = file_str.as_ref().and_then(|f| ctx.file_classes.get(f));
    if cls.map(String::as_str) == Some("logic") {
        add("logicFile");
    }
    if cls.map(String::as_str) == Some("docs") {
        add("docsOnly");
    }
    if js_truthy(finding.get("outOfChunk").unwrap_or(&Value::Null)) {
        add("outOfChunk");
    }

    let score = reasons.iter().map(|r| r.weight).sum();
    (score, reasons)
}

// ---------------------------------------------------------------------------
// routeFindings
// ---------------------------------------------------------------------------

/// Конфиг маршрутизации (деструктурированные cfg в JS; None → JS-дефолты).
#[derive(Debug, Clone)]
pub struct RouteCfg {
    pub inline_on: Vec<String>,
    pub threshold: f64,
    pub max_advisory_inline: usize,
    pub strict: bool,
}

impl Default for RouteCfg {
    fn default() -> Self {
        RouteCfg {
            inline_on: vec!["critical".to_string(), "major".to_string()],
            threshold: 4.0,
            max_advisory_inline: 5,
            strict: false,
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RouteCfgArg {
    pub inline_on: Option<Vec<String>>,
    pub threshold: Option<f64>,
    pub max_advisory_inline: Option<usize>,
    pub strict: Option<bool>,
}

impl RouteCfgArg {
    pub fn into_cfg(self) -> RouteCfg {
        RouteCfg {
            inline_on: self.inline_on.unwrap_or_else(|| RouteCfg::default().inline_on),
            threshold: self.threshold.unwrap_or(4.0),
            max_advisory_inline: self.max_advisory_inline.unwrap_or(5),
            strict: self.strict.unwrap_or(false),
        }
    }
}

/// Результат маршрутизации одной находки.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoredItem {
    pub index: usize,
    pub score: i32,
    pub reasons: Vec<SignalHit>,
    pub inlineable: bool,
    pub route: &'static str,
    pub promoted: bool,
}

fn severity_of(f: &Value) -> Option<&str> {
    f.get("severity").and_then(Value::as_str)
}

fn known_severity(f: &Value) -> bool {
    matches!(
        severity_of(f),
        Some("critical") | Some("major") | Some("minor") | Some("nit")
    )
}

/**
 * Решение по каждой находке: `inline` (в строку) или `sticky` (в свод).
 * Блокеры идут в строку всегда — там их читают. Advisory попадает в строку, если её
 * доказуемость набрала порог; лимит `max_advisory_inline` не даёт залить PR замечаниями,
 * причём в него отбираются самые доказуемые, а не первые попавшиеся.
 *
 * Мутации (actionabilityScore/Reasons/Promoted) НЕ накладываются на вход: вызывающий
 * (JS-обёртка) применяет их по index, сохраняя семантику разделяемых ссылок.
 */
pub fn route_findings(
    findings: &[Value],
    files: &FilesMap,
    cfg: &RouteCfg,
    ctx: &ScoreCtx,
) -> Vec<ScoredItem> {
    let tiers: HashSet<&str> = cfg.inline_on.iter().map(String::as_str).collect();
    let effective_threshold = if cfg.strict { f64::INFINITY } else { cfg.threshold };

    let mut scored: Vec<ScoredItem> = findings
        .iter()
        .enumerate()
        .map(|(index, f)| {
            let (score, reasons) = score_actionability(f, ctx);
            ScoredItem {
                index,
                score,
                reasons,
                inlineable: is_inlineable(f, files),
                route: "sticky",
                promoted: false,
            }
        })
        .collect();

    let mut advisory: Vec<usize> = scored
        .iter()
        .filter(|s| {
            let f = &findings[s.index];
            s.inlineable
                && known_severity(f)
                && !tiers.contains(severity_of(f).unwrap_or(""))
                && (s.score as f64) >= effective_threshold
        })
        .map(|s| s.index)
        .collect();
    // JS: stable sort по score desc → при равенстве выигрывает более ранняя находка.
    advisory.sort_by(|&a, &b| scored[b].score.cmp(&scored[a].score));
    advisory.truncate(cfg.max_advisory_inline);
    let promoted: HashSet<usize> = advisory.into_iter().collect();

    for s in &mut scored {
        let f = &findings[s.index];
        let sev = severity_of(f).unwrap_or("");
        let block_inline = tiers.contains(sev)
            && s.inlineable
            && (!cfg.strict
                || js_truthy(f.get("prescan").unwrap_or(&Value::Null))
                || f.get("verifierStatus").and_then(Value::as_str) == Some("verified"));
        s.promoted = promoted.contains(&s.index);
        s.route = if block_inline || s.promoted { "inline" } else { "sticky" };
    }
    scored
}

// ---------------------------------------------------------------------------
// findingBlock / classify (createFindingsFormatter из lib/findings.mjs)
// ---------------------------------------------------------------------------

/// SEV из lib/constants.mjs (движковая константа, не профиль).
fn severity_emoji(sev: Option<&str>) -> &'static str {
    match sev {
        Some("critical") => "🔴",
        Some("major") => "🟠",
        Some("minor") => "🟡",
        Some("nit") => "🔵",
        _ => "⚪",
    }
}

/// Один блок находки для инлайн-комментария (findingBlock в JS).
pub fn finding_block(f: &Value) -> String {
    let sev_template = f
        .get("severity")
        .map(js_to_string)
        .unwrap_or_else(|| "undefined".to_string());
    let title = match f.get("title") {
        Some(v) if js_truthy(v) => js_to_string(v),
        _ => "замечание".to_string(),
    };
    let head = format!("{} **{}: {}**", severity_emoji(severity_of(f)), sev_template, title);
    // Advisory, поднятая в строку по доказуемости, объясняет, почему она здесь.
    let why = if js_truthy(f.get("actionabilityPromoted").unwrap_or(&Value::Null)) {
        match f.get("actionabilityReasons") {
            Some(Value::Array(a)) if !a.is_empty() => {
                let joined = a.iter().map(js_to_string).collect::<Vec<_>>().join("; ");
                format!("\n\n<sub>📌 в строке, потому что: {joined}. Не блокирует merge.</sub>")
            }
            _ => String::new(),
        }
    } else {
        String::new()
    };
    let sug = if js_truthy(f.get("suggestion").unwrap_or(&Value::Null)) {
        format!("\n\n```suggestion\n{}\n```", js_to_string(f.get("suggestion").unwrap()))
    } else {
        String::new()
    };
    let body = match f.get("body") {
        Some(v) if js_truthy(v) => js_to_string(v),
        _ => String::new(),
    };
    format!("{head}\n\n{body}{sug}{why}")
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineComment {
    pub path: String,
    pub line: Value,
    pub side: &'static str,
    pub body: String,
}

pub struct ClassifyOutcome {
    pub inline: Vec<InlineComment>,
    pub outside: Vec<usize>,
    pub scored: Vec<ScoredItem>,
}

/// Маршрут каждой находки (route_findings) + группировка инлайнов по `file:line`.
/// `marker_inline` — MARKERS.inline активного профиля (аргумент, как MARKERS в findings).
pub fn classify_findings(
    findings: &[Value],
    files: &FilesMap,
    cfg: &RouteCfg,
    ctx: &ScoreCtx,
    marker_inline: &str,
) -> ClassifyOutcome {
    let scored = route_findings(findings, files, cfg, ctx);
    let mut groups: indexmap::IndexMap<String, Vec<usize>> = indexmap::IndexMap::new();
    let mut outside: Vec<usize> = Vec::new();
    for s in &scored {
        if s.route == "inline" {
            let f = &findings[s.index];
            let key = format!(
                "{}:{}",
                f.get("file").map(js_to_string).unwrap_or_else(|| "undefined".to_string()),
                js_to_string(&Value::from(js_number_or_zero(f.get("line"))))
            );
            groups.entry(key).or_default().push(s.index);
        } else {
            outside.push(s.index);
        }
    }
    // finding_block читает actionabilityPromoted/actionabilityReasons, которые в JS
    // routeFindings пишет в находку ДО рендера — воспроизводим пост-мутационное
    // состояние (копия; исходные Value не трогаем, мутации уходят вызывающему).
    let block_value = |i: usize| -> Value {
        let mut f = findings[i].clone();
        let s = &scored[i];
        if let Some(obj) = f.as_object_mut() {
            obj.insert("actionabilityScore".into(), json!(s.score));
            obj.insert(
                "actionabilityReasons".into(),
                json!(s.reasons.iter().map(|r| r.why).collect::<Vec<_>>()),
            );
            if s.promoted {
                obj.insert("actionabilityPromoted".into(), Value::Bool(true));
            } else {
                obj.remove("actionabilityPromoted");
            }
        }
        f
    };
    let inline = groups
        .values()
        .map(|g| {
            let f0 = &findings[g[0]];
            let blocks: Vec<String> = g.iter().map(|&i| finding_block(&block_value(i))).collect();
            InlineComment {
                path: f0
                    .get("file")
                    .map(js_to_string)
                    .unwrap_or_else(|| "undefined".to_string()),
                line: f0.get("line").cloned().unwrap_or(Value::Null),
                side: "RIGHT",
                body: format!("{marker_inline}\n{}", blocks.join("\n\n---\n\n")),
            }
        })
        .collect();
    ClassifyOutcome { inline, outside, scored }
}

// ---------------------------------------------------------------------------
// Тесты — ключевые случаи из test/pr-review.eval.test.mjs
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> ScoreCtx {
        ScoreCtxArg::default().into_ctx().unwrap()
    }

    fn files(entries: &[(&str, &[u64])]) -> FilesMap {
        entries
            .iter()
            .map(|(p, lines)| (p.to_string(), lines.iter().copied().collect()))
            .collect()
    }

    #[test]
    fn finding_location_key_format() {
        assert_eq!(
            finding_location_key(&json!({"file": " a.ts ", "line": 5})),
            "a.ts:5"
        );
        assert_eq!(finding_location_key(&json!({})), ":0");
        assert_eq!(
            finding_location_key(&json!({"file": "a.ts", "line": "3"})),
            "a.ts:3"
        );
        assert_eq!(
            finding_location_key(&json!({"file": "a.ts", "line": "x"})),
            "a.ts:0"
        );
    }

    #[test]
    fn proven_minor_inline_bare_nit_sticky() {
        // кейс из test 'actionability: доказуемый minor уходит в строку…'
        let f = files(&[("server/auth.ts", &[10, 20])]);
        let proven = json!({
            "file": "server/auth.ts", "line": 10, "severity": "minor", "title": "p", "model": "m1",
            "confidence": 0.9, "consensus": true, "suggestion": "fix()",
        });
        // сигналы: consensus(2) + confidence(1) + sensitivePath(1: auth) + suggestion(1) + logicFile(1) = 6 ≥ 4
        let bare = json!({
            "file": "server/auth.ts", "line": 20, "severity": "nit", "title": "b", "model": "m1",
            "confidence": 0.4,
        });
        let mut ctx = ctx();
        ctx.file_classes.insert("server/auth.ts".to_string(), "logic".to_string());
        let scored = route_findings(&[proven, bare], &f, &RouteCfg::default(), &ctx);
        assert_eq!(scored[0].route, "inline");
        assert_eq!(scored[0].score, 6);
        assert_eq!(scored[1].route, "sticky");
    }

    #[test]
    fn out_of_chunk_overrides_positive_signals() {
        let f = files(&[("server/auth.ts", &[10])]);
        let finding = json!({
            "file": "server/auth.ts", "line": 10, "severity": "minor", "model": "m1",
            "confidence": 0.9, "consensus": true, "suggestion": "fix()", "outOfChunk": true,
        });
        let mut ctx = ctx();
        ctx.file_classes.insert("server/auth.ts".to_string(), "logic".to_string());
        let scored = route_findings(&[finding], &f, &RouteCfg::default(), &ctx);
        assert_eq!(scored[0].route, "sticky", "outOfChunk (-4) перевешивает +6");
    }

    #[test]
    fn advisory_limit_takes_most_provable_not_first() {
        let f = files(&[("server/auth.ts", &[1, 2, 3])]);
        let mk = |line, extra: serde_json::Map<String, Value>| {
            let mut m = serde_json::Map::new();
            m.insert("file".into(), json!("server/auth.ts"));
            m.insert("line".into(), json!(line));
            m.insert("severity".into(), json!("minor"));
            m.insert("title".into(), json!(format!("t{line}")));
            m.insert("model".into(), json!("m1"));
            m.insert("confidence".into(), json!(0.9));
            m.insert("consensus".into(), json!(true));
            m.extend(extra);
            Value::Object(m)
        };
        let findings = vec![
            mk(1, serde_json::Map::new()),
            mk(2, serde_json::from_str(r#"{"suggestion":"fix()"}"#).unwrap()),
            mk(3, serde_json::Map::new()),
        ];
        let mut ctx = ctx();
        ctx.file_classes.insert("server/auth.ts".to_string(), "logic".to_string());
        let cfg = RouteCfg { max_advisory_inline: 1, ..RouteCfg::default() };
        let scored = route_findings(&findings, &f, &cfg, &ctx);
        let inline: Vec<&str> = scored
            .iter()
            .filter(|s| s.route == "inline")
            .map(|s| findings[s.index]["title"].as_str().unwrap())
            .collect();
        assert_eq!(inline, vec!["t2"], "выиграла находка с конкретным фиксом");
    }

    #[test]
    fn strict_needs_prescan_or_verified_for_block_inline() {
        let f = files(&[("a.ts", &[1, 2, 3])]);
        let unverified = json!({"file": "a.ts", "line": 1, "severity": "major", "confidence": 0.95, "model": "m", "verifierStatus": "unverified"});
        let prescan = json!({"file": "a.ts", "line": 2, "severity": "major", "confidence": 0.95, "model": "m", "prescan": true});
        let verified = json!({"file": "a.ts", "line": 3, "severity": "major", "confidence": 0.95, "model": "m", "verifierStatus": "verified"});
        let cfg = RouteCfg { strict: true, ..RouteCfg::default() };
        let scored = route_findings(&[unverified, prescan, verified], &f, &cfg, &ctx());
        assert_eq!(scored[0].route, "sticky");
        assert_eq!(scored[1].route, "inline");
        assert_eq!(scored[2].route, "inline");
    }

    #[test]
    fn signals_order_and_score() {
        let mut ctx = ctx();
        ctx.prescan_keys.insert("a.ts:1".to_string());
        ctx.prior_keys.insert("a.ts:1".to_string());
        ctx.file_classes.insert("a.ts".to_string(), "logic".to_string());
        let finding = json!({
            "file": "a.ts", "line": 1, "severity": "major", "model": "m",
            "verifierStatus": "verified", "confidence": 0.9, "suggestion": "s",
            "advisorClaims": [{"status": "verified"}],
        });
        let (score, reasons) = score_actionability(&finding, &ctx);
        let keys: Vec<&str> = reasons.iter().map(|r| r.key).collect();
        assert_eq!(
            keys,
            vec!["prescan", "verified", "repeated", "jury", "confidence", "suggestion", "logicFile"]
        );
        assert_eq!(score, 3 + 3 + 2 + 2 + 1 + 1 + 1);
    }

    #[test]
    fn jury_downgraded_twice_possible() {
        // verifierStatus=rejected даёт juryDowngraded, claims all-verified добавляют jury —
        // цепочки if/else в JS независимы, оба сигнала возможны одновременно.
        let finding = json!({
            "file": "a.ts", "line": 1, "severity": "major", "model": "m",
            "verifierStatus": "rejected",
            "advisorClaims": [{"status": "verified"}, {"status": "verified"}],
        });
        let (score, reasons) = score_actionability(&finding, &ctx());
        let keys: Vec<&str> = reasons.iter().map(|r| r.key).collect();
        assert_eq!(keys, vec!["juryDowngraded", "jury"]);
        assert_eq!(score, 0);
    }

    #[test]
    fn sensitive_path_base_and_profile_extra() {
        let base = json!({"file": "app/server/auth/login.ts", "line": 1, "severity": "major", "model": "m"});
        let (score, _) = score_actionability(&base, &ctx());
        assert_eq!(score, 1, "base sensitivePath");
        let ctx = ScoreCtxArg {
            sensitive_source: Some(r"(?:^|/)ops/(?:pr-merge|pr-review)".to_string()),
            ..ScoreCtxArg::default()
        }
        .into_ctx()
        .unwrap();
        let extra = json!({"file": "ops/pr-review/gate.sh", "line": 1, "severity": "major", "model": "m"});
        let (score, _) = score_actionability(&extra, &ctx);
        assert_eq!(score, 1, "profile extra sensitivePath");
    }

    #[test]
    fn is_inlineable_semantics() {
        let f = files(&[("a.ts", &[1, 10])]);
        assert!(is_inlineable(&json!({"file": "a.ts", "line": 10}), &f));
        assert!(!is_inlineable(&json!({"file": "a.ts", "line": 5}), &f));
        assert!(!is_inlineable(&json!({"file": "a.ts", "line": 0}), &f));
        assert!(!is_inlineable(&json!({"file": "a.ts", "line": 10.5}), &f));
        assert!(!is_inlineable(&json!({"line": 10}), &f), "без файла — неинлайнабельно");
        assert!(!is_inlineable(&json!({"file": "gone.ts", "line": 10}), &f));
    }

    #[test]
    fn finding_block_render() {
        let f = json!({
            "severity": "minor", "title": "доказуемая", "body": "текст",
            "suggestion": "fix()", "actionabilityPromoted": true,
            "actionabilityReasons": ["жюри R5 подтвердило без возражений", "модель уверена (confidence ≥ 0.8)"],
        });
        let block = finding_block(&f);
        assert!(block.starts_with("🟡 **minor: доказуемая**\n\nтекст\n\n```suggestion\nfix()\n```"));
        assert!(block.contains("<sub>📌 в строке, потому что: жюри R5 подтвердило без возражений; модель уверена (confidence ≥ 0.8). Не блокирует merge.</sub>"));

        let plain = finding_block(&json!({"severity": "weird", "body": "b"}));
        assert!(plain.starts_with("⚪ **weird: замечание**\n\nb"), "неизвестная severity → ⚪ и дефолтный заголовок");
        let no_body = finding_block(&json!({"severity": "critical"}));
        assert_eq!(no_body, "🔴 **critical: замечание**\n\n");
    }

    #[test]
    fn classify_groups_inline_by_file_line() {
        let f = files(&[("app/client/src/foo.ts", &[1])]);
        let findings = vec![
            json!({"file": "app/client/src/foo.ts", "line": 1, "severity": "critical", "title": "block", "body": "x", "model": "m"}),
            json!({"file": "app/client/src/foo.ts", "line": 1, "severity": "major", "title": "second", "body": "y", "model": "m"}),
            json!({"file": "app/client/src/foo.ts", "line": 1, "severity": "minor", "title": "nitpick", "body": "z", "model": "m"}),
        ];
        let out = classify_findings(&findings, &f, &RouteCfg::default(), &ctx(), "<!-- ai-review-inline -->");
        assert_eq!(out.inline.len(), 1, "две блокирующие на одной строке — один комментарий");
        assert_eq!(out.inline[0].side, "RIGHT");
        assert_eq!(out.inline[0].line, json!(1));
        assert!(out.inline[0].body.starts_with("<!-- ai-review-inline -->\n"));
        assert!(out.inline[0].body.contains("\n\n---\n\n"), "два блока через разделитель");
        assert_eq!(out.outside, vec![2]);
    }
}
