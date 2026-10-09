//! Порт `lib/astgrep.mjs` (задача T14 RFC 16): ast-grep prescan — AST-антипаттерны
//! рядом с eslint.
//!
//! **Вшитый ruleset (RFC 16, решение 5).** Рядом с одиночным бинарём дерева пакета
//! нет, поэтому `ast-grep/` (sgconfig.yml + правила) вшит через `include_dir!`.
//! Читает его внешний процесс `ast-grep`, а не мы, — поэтому при первом
//! обращении он распаковывается в `std::env::temp_dir()` ([`embedded_config_path`]).
//! Каталог назван по sha256 содержимого: две версии бинаря не делят один каталог,
//! а повторный прогон той же версии переиспользует распакованное. Распаковка идёт
//! во временный каталог-сосед и переименовывается атомарно — параллельные
//! прогоны не видят полузаписанный ruleset.
//!
//! Пустой `prescan.astgrep.configPath` (дефолт `loadReviewConfig` без профиля) —
//! вшитый ruleset, как `DEFAULT_ASTGREP_CONFIG` в JS. Явный путь (env
//! `REVIEW_ASTGREP_CONFIG` или `paths.astgrepConfig` профиля) имеет приоритет:
//! эмбед — дефолт, а не запрет на подмену.
//!
//! **Fail-open (риск 3 RFC 16).** Нет конфига, не распаковался эмбед, нет бинаря,
//! бинарь упал, вывод не JSON — пустой результат с `skipped`/`error`, не `Err`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use include_dir::{include_dir, Dir};
use serde_json::{json, Map, Value};

use crate::findings::{js_to_number, js_to_string, js_trim, js_truthy};
use crate::prescan::{
    build_prescan_path_map, build_prescan_prompt_section, empty_stdout_error, js_num_value, js_resolve,
    log_tool_failure, normalize_app_path, resolve_prescan_diff_key, run_command, select_lintable_entries,
    str_list, strip_app_dir, tool_path_to_app_path, LogFn, PrescanConfig, PrescanPathMap, DEFAULT_MAX_BUFFER,
};
use crate::profile::{Layout, Profile};
use crate::traits::ToolPrescanResult;

/// Ruleset пакета целиком: `sgconfig.yml`, `rules/`, `fixtures/`.
pub static EMBEDDED_RULESET: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../ast-grep");

/// Имя конфига внутри ruleset.
pub const SGCONFIG: &str = "sgconfig.yml";

/// Блок-правила ast-grep по умолчанию (`lib/astgrep.mjs:20`).
pub const DEFAULT_ASTGREP_BLOCK_RULES: [&str; 1] = ["raw-x-forwarded-for"];

/// `ASTGREP_SEV` (`lib/astgrep.mjs:22`).
fn astgrep_sev(s: &str) -> Option<&'static str> {
    match s {
        "error" => Some("critical"),
        "warning" => Some("major"),
        "info" => Some("minor"),
        "hint" => Some("nit"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Вшитый ruleset
// ---------------------------------------------------------------------------

fn collect_files<'a>(dir: &'a Dir<'a>, out: &mut Vec<&'a include_dir::File<'a>>) {
    for f in dir.files() {
        out.push(f);
    }
    for d in dir.dirs() {
        collect_files(d, out);
    }
}

/// sha256 содержимого ruleset (пути + байты), первые 16 hex.
pub fn embedded_ruleset_digest() -> String {
    let mut files = Vec::new();
    collect_files(&EMBEDDED_RULESET, &mut files);
    files.sort_by(|a, b| a.path().cmp(b.path()));
    let mut acc = String::new();
    for f in files {
        acc.push_str(&f.path().to_string_lossy());
        acc.push('\0');
        acc.push_str(&String::from_utf8_lossy(f.contents()));
        acc.push('\0');
    }
    crate::util::sha256_str(&acc)[..16].to_string()
}

/// Сериализует распаковку внутри процесса: параллельные тесты/потоки иначе
/// гоняют лишние копии (корректность держит атомарный rename, это экономия).
static UNPACK_LOCK: Mutex<()> = Mutex::new(());

/// Распаковать вшитый ruleset в `base` и вернуть путь к `sgconfig.yml`.
///
/// Идемпотентно: каталог `ai-reviewer-astgrep-<digest>` уже есть — переиспользуем.
pub fn unpack_embedded_ruleset(base: &Path) -> std::io::Result<PathBuf> {
    let _guard = UNPACK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let target = base.join(format!("ai-reviewer-astgrep-{}", embedded_ruleset_digest()));
    let config = target.join(SGCONFIG);
    if config.is_file() {
        return Ok(config);
    }
    fs::create_dir_all(base)?;
    let staging = tempfile::Builder::new().prefix(".ai-reviewer-astgrep-").tempdir_in(base)?;
    EMBEDDED_RULESET.extract(staging.path())?;
    match fs::rename(staging.path(), &target) {
        Ok(()) => {
            // Каталог уже переименован; TempDir не должен пытаться его удалить.
            let _ = staging.keep();
        }
        Err(e) => {
            // Гонка с другим процессом: он успел первым — берём его копию.
            if !config.is_file() {
                // Битый целевой каталог без sgconfig (кто-то почистил частично):
                // работаем из своей копии, она остаётся в temp_dir.
                let kept = staging.keep();
                let alt = kept.join(SGCONFIG);
                if alt.is_file() {
                    return Ok(alt);
                }
                return Err(e);
            }
        }
    }
    Ok(config)
}

/// Путь к `sgconfig.yml` вшитого ruleset в `std::env::temp_dir()`.
pub fn embedded_config_path() -> std::io::Result<PathBuf> {
    unpack_embedded_ruleset(&std::env::temp_dir())
}

// ---------------------------------------------------------------------------
// Конфиг
// ---------------------------------------------------------------------------

/// `cfg.prescan.astgrep` (`lib/config.mjs`, порт — ключ `prescan.astgrep` в
/// `config::load_review_config_parts`).
#[derive(Debug, Clone, PartialEq)]
pub struct AstgrepPrescanConfig {
    pub enabled: bool,
    /// Пусто — вшитый ruleset.
    pub config_path: String,
    /// `"ast-grep"` по умолчанию; путь с `/` или `.` — от каталога прогона.
    pub bin: String,
    /// `None` — ключа нет (дефолт); `Some([])` — блок ast-grep выключен.
    pub block_rules: Option<Vec<String>>,
}

impl Default for AstgrepPrescanConfig {
    fn default() -> Self {
        AstgrepPrescanConfig { enabled: false, config_path: String::new(), bin: "ast-grep".into(), block_rules: None }
    }
}

impl AstgrepPrescanConfig {
    pub fn from_obj(obj: &Map<String, Value>) -> Self {
        let s = |k: &str| obj.get(k).filter(|v| js_truthy(v)).map(js_to_string);
        AstgrepPrescanConfig {
            enabled: obj.get("enabled").is_some_and(js_truthy),
            config_path: s("configPath").unwrap_or_default(),
            bin: s("bin").unwrap_or_else(|| "ast-grep".into()),
            block_rules: str_list(obj.get("blockRules")),
        }
    }
}

/// Множество блок-правил ast-grep: массив (даже пустой) — как есть, иначе дефолт
/// (`Array.isArray(ast.blockRules) ? … : DEFAULT`, `lib/astgrep.mjs:153-155`).
pub fn astgrep_block_rule_set(rules: Option<&[String]>) -> HashSet<String> {
    match rules {
        Some(r) => r.iter().filter(|x| !x.is_empty()).cloned().collect(),
        None => DEFAULT_ASTGREP_BLOCK_RULES.iter().map(|s| s.to_string()).collect(),
    }
}

// ---------------------------------------------------------------------------
// Разбор вывода
// ---------------------------------------------------------------------------

/// ast-grep ruleId → severity; блок-правила → critical (`mapAstgrepRuleSeverity`).
pub fn map_astgrep_rule_severity(rule_id: &str, ast_severity: &str, block_rules: &HashSet<String>) -> &'static str {
    if block_rules.contains(rule_id) {
        return "critical";
    }
    astgrep_sev(ast_severity).unwrap_or("major")
}

/// Пост-фильтр: scope правил + allowlist из профиля (`filterAstgrepFindings`,
/// `lib/astgrep.mjs:47-54`). Globs в sgconfig не знают слоёв проекта.
pub fn filter_astgrep_findings(findings: Vec<Value>, profile: &Profile) -> Vec<Value> {
    let layout = profile.layout();
    let ag = profile.astgrep();
    findings
        .into_iter()
        .filter(|f| {
            let file = f.get("file").filter(|v| js_truthy(v)).map(js_to_string).unwrap_or_default();
            let norm = normalize_app_path(&file, layout);
            let rule = f.get("ruleId").map(js_to_string).unwrap_or_else(|| "undefined".into());
            if let Some(scope) = ag.scope(&rule) {
                if !scope.is_match(&norm).unwrap_or(false) {
                    return false;
                }
            }
            !ag.is_allowed(&rule, &norm)
        })
        .collect()
}

/// Строка → `JSON.parse`; `row?.ruleId && row?.file` обязательны.
fn parse_row(line: &str) -> Option<Value> {
    let row: Value = serde_json::from_str(line).ok()?;
    let ok = row.get("ruleId").is_some_and(js_truthy) && row.get("file").is_some_and(js_truthy);
    ok.then_some(row)
}

/// Парсит stdout `ast-grep --json=stream` → находки (`astgrepStreamToFindings`,
/// `lib/astgrep.mjs:57-86`). Битые строки молча пропускаются.
pub fn astgrep_stream_to_findings(
    stdout: &str,
    block_rules: &HashSet<String>,
    path_map: Option<&PrescanPathMap>,
    cwd: &Path,
    profile: &Profile,
) -> Vec<Value> {
    let layout: &Layout = profile.layout();
    let mut findings = Vec::new();
    for line in stdout.split('\n') {
        let trimmed = js_trim(line);
        if trimmed.is_empty() {
            continue;
        }
        let Some(row) = parse_row(trimmed) else { continue };
        let rule_id = js_to_string(&row["ruleId"]);
        let app_path = tool_path_to_app_path(&js_to_string(&row["file"]), cwd, layout);
        let file_key = resolve_prescan_diff_key(&app_path, path_map, layout);
        // `Number(row.range?.start?.line ?? 0) + 1`: null/нет → 0; NaN → null в JSON.
        let start = row.get("range").and_then(|r| r.get("start")).and_then(|s| s.get("line"));
        let line_no = match start {
            None | Some(Value::Null) => Some(0.0),
            Some(v) => js_to_number(v),
        }
        .map(|n| n + 1.0)
        .map(js_num_value)
        .unwrap_or(Value::Null);
        let ast_sev = row
            .get("severity")
            .filter(|v| js_truthy(v))
            .map(js_to_string)
            .unwrap_or_else(|| "warning".into())
            .to_lowercase();
        let severity = map_astgrep_rule_severity(&rule_id, &ast_sev, block_rules);
        let body = js_trim(&row.get("message").filter(|v| js_truthy(v)).map(js_to_string).unwrap_or_default())
            .to_string();
        findings.push(json!({
            "file": file_key,
            "line": line_no,
            "severity": severity,
            "confidence": 1,
            "title": format!("ast-grep: {rule_id}"),
            "body": body,
            "model": "astgrep-prescan",
            "prescan": true,
            "ruleId": rule_id,
            "prescanBlock": block_rules.contains(&rule_id),
        }));
    }
    filter_astgrep_findings(findings, profile)
}

// ---------------------------------------------------------------------------
// Запуск
// ---------------------------------------------------------------------------

fn skipped(tag: &str) -> ToolPrescanResult {
    ToolPrescanResult { skipped: Some(tag.into()), ..ToolPrescanResult::default() }
}

/// Где взять `sgconfig.yml`: явный путь (абсолютный или от `cwd`) либо вшитый.
/// `None` — конфига нет (в лог уже написано почему).
fn resolve_config(config_path: &str, cwd: &Path, log: LogFn<'_>) -> Option<PathBuf> {
    if config_path.is_empty() {
        return match embedded_config_path() {
            Ok(p) => Some(p),
            Err(e) => {
                log(&format!("prescan: вшитый ast-grep ruleset не распаковался ({e}) — skip"));
                None
            }
        };
    }
    let p = js_resolve(cwd, config_path);
    if p.exists() {
        Some(p)
    } else {
        log("prescan: ast-grep config не найден — skip");
        None
    }
}

/// Запуск ast-grep на изменённых lintable-файлах (`runAstgrepPrescan`,
/// `lib/astgrep.mjs:92-175`). Строго fail-open.
pub fn run_astgrep_prescan(
    changed_paths: &[String],
    cfg: &PrescanConfig,
    profile: &Profile,
    cwd: &Path,
    log: LogFn<'_>,
) -> ToolPrescanResult {
    let Some(ast) = cfg.astgrep.as_ref() else { return ToolPrescanResult::default() };
    if !cfg.enabled || !ast.enabled {
        return ToolPrescanResult::default();
    }

    let app_dir = js_resolve(cwd, &cfg.app_dir);
    let Some(config_path) = resolve_config(&ast.config_path, cwd, log) else {
        return skipped("no-config");
    };

    let local_bin = app_dir.join("node_modules/.bin/ast-grep");
    let bin = if local_bin.exists() {
        local_bin.to_string_lossy().into_owned()
    } else if ast.bin.contains('/') || ast.bin.starts_with('.') {
        let abs = js_resolve(cwd, &ast.bin);
        if !abs.exists() {
            log(&format!("prescan: ast-grep bin not found ({}) — skip", ast.bin));
            return skipped("no-ast-grep");
        }
        abs.to_string_lossy().into_owned()
    } else {
        ast.bin.clone()
    };

    let sel = select_lintable_entries(changed_paths, cfg.max_files, profile.layout());
    if sel.truncated {
        log(&format!(
            "prescan ast-grep: lintable файлов {} > лимита {}",
            sel.total,
            js_to_string(&js_num_value(cfg.max_files))
        ));
    }
    if sel.entries.is_empty() {
        return ToolPrescanResult::default();
    }

    let path_map = build_prescan_path_map(&sel.entries);
    let mut args: Vec<String> = vec![
        "scan".into(),
        "-c".into(),
        config_path.to_string_lossy().into_owned(),
        "--json=stream".into(),
    ];
    args.extend(sel.entries.iter().map(|e| strip_app_dir(&e.app_path, &cfg.app_dir)));

    let out = match run_command(&bin, &args, &app_dir, DEFAULT_MAX_BUFFER) {
        Ok(o) => o,
        Err(e) => {
            log(&format!("prescan ast-grep failed (fail-open): {e}"));
            return ToolPrescanResult { error: Some(e), ..ToolPrescanResult::default() };
        }
    };
    if js_trim(&out.stdout).is_empty() {
        log_tool_failure(log, "ast-grep", &out);
        return ToolPrescanResult {
            scanned: sel.entries.len(),
            lintable_total: Some(sel.total),
            truncated: sel.truncated,
            error: empty_stdout_error(&out),
            ..ToolPrescanResult::default()
        };
    }
    log_tool_failure(log, "ast-grep", &out);
    let block = astgrep_block_rule_set(ast.block_rules.as_deref());
    let findings = astgrep_stream_to_findings(&out.stdout, &block, Some(&path_map), &app_dir, profile);
    ToolPrescanResult {
        prompt_section: build_prescan_prompt_section(&findings),
        findings,
        scanned: sel.entries.len(),
        lintable_total: Some(sel.total),
        truncated: sel.truncated,
        ..ToolPrescanResult::default()
    }
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prescan::tests::{assert_fixture_gate, fixture_findings, neutral, no_log, orakul};
    use crate::prescan::LintableEntry;

    fn xff_row(line: u64) -> String {
        json!({
            "ruleId": "raw-x-forwarded-for", "severity": "error", "message": "Use clientIp(req)",
            "file": "server/auth.ts", "range": { "start": { "line": line } },
        })
        .to_string()
    }

    fn auth_map() -> PrescanPathMap {
        build_prescan_path_map(&[LintableEntry { app_path: "app/server/auth.ts".into(), diff_key: "server/auth.ts".into() }])
    }

    fn cfg(app_dir: &str, bin: &str, config_path: &str) -> PrescanConfig {
        PrescanConfig {
            enabled: true,
            app_dir: app_dir.into(),
            astgrep: Some(AstgrepPrescanConfig {
                enabled: true,
                config_path: config_path.into(),
                bin: bin.into(),
                block_rules: None,
            }),
            ..PrescanConfig::default()
        }
    }

    /// Каталог с `ast-grep` в PATH или `None` (тест тогда пропускается с причиной).
    fn real_astgrep() -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).map(|d| d.join("ast-grep")).find(|p| p.is_file())
    }

    /// Строка app/server/auth.ts, на которой стоит находка фикстуры (1-based 42).
    fn auth_ts() -> String {
        let mut lines: Vec<String> = (1..42).map(|i| format!("// line {i}")).collect();
        lines.push("export const ip = (req: any) => req.header('x-forwarded-for') || '';".into());
        lines.push(String::new());
        lines.join("\n")
    }

    #[test]
    fn stream_xff_critical_block() {
        let p = orakul();
        let f = astgrep_stream_to_findings(&xff_row(41), &astgrep_block_rule_set(None), Some(&auth_map()), Path::new("/repo/app"), &p);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["severity"], "critical");
        assert_eq!(f[0]["prescanBlock"], true);
        assert_eq!(f[0]["file"], "server/auth.ts");
        assert_eq!(f[0]["model"], "astgrep-prescan");
        assert_eq!(f[0]["line"], 42);
    }

    #[test]
    fn stream_empty_block_rules_disable_block() {
        let p = orakul();
        let f = astgrep_stream_to_findings(&xff_row(41), &astgrep_block_rule_set(Some(&[])), Some(&auth_map()), Path::new("/repo/app"), &p);
        assert_eq!(f[0]["prescanBlock"], false);
        assert_eq!(f[0]["severity"], "critical", "severity error → critical и без блока");
    }

    #[test]
    fn stream_skips_garbage_and_missing_fields() {
        let p = neutral();
        let s = format!("not json\n{{}}\n{{\"ruleId\":\"r\"}}\n\n{}\n", json!({"ruleId": "r", "file": "a.ts", "severity": "HINT"}));
        let f = astgrep_stream_to_findings(&s, &astgrep_block_rule_set(None), None, Path::new("/r"), &p);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["line"], 1, "нет range → 0 + 1");
        assert_eq!(f[0]["severity"], "nit");
    }

    #[test]
    fn severity_mapping() {
        let block = astgrep_block_rule_set(Some(&["raw-x-forwarded-for".into()]));
        assert_eq!(map_astgrep_rule_severity("no-math-round-ts", "warning", &block), "major");
        assert_eq!(map_astgrep_rule_severity("x", "info", &block), "minor");
        assert_eq!(map_astgrep_rule_severity("x", "weird", &block), "major");
        assert_eq!(map_astgrep_rule_severity("raw-x-forwarded-for", "hint", &block), "critical");
    }

    #[test]
    fn filter_scopes_and_allow() {
        let p = orakul();
        let mk = |file: &str, rule: &str| json!({"file": file, "ruleId": rule, "line": 1});
        let out = filter_astgrep_findings(
            vec![
                mk("client/src/foo.tsx", "no-math-round-ts"),
                mk("shared/dishCost.ts", "no-math-round-ts"),
                mk("server/auth.ts", "raw-x-forwarded-for"),
                mk("server/net.ts", "raw-x-forwarded-for"),
            ],
            &p,
        );
        let files: Vec<&str> = out.iter().map(|f| f["file"].as_str().unwrap()).collect();
        assert_eq!(files, vec!["shared/dishCost.ts", "server/auth.ts"]);
        let layered = vec![
            mk("client/src/foo.tsx", "client-imports-server"),
            mk("shared/x.ts", "shared-imports-upper"),
            mk("server/auth.ts", "backend-imports-client"),
            mk("client/src/foo.tsx", "no-dangerously-set-inner-html"),
            mk("server/db.ts", "no-prisma-raw-unsafe"),
        ];
        assert_eq!(filter_astgrep_findings(layered, &p).len(), 5);
        // Нейтральный профиль: правило действует везде.
        assert_eq!(filter_astgrep_findings(vec![mk("client/x.ts", "no-math-round-ts")], &neutral()).len(), 1);
    }

    #[test]
    fn embedded_ruleset_unpacks_readable_and_idempotent() {
        assert!(EMBEDDED_RULESET.get_file(SGCONFIG).is_some());
        assert!(EMBEDDED_RULESET.get_file("rules/raw-x-forwarded-for.yml").is_some());
        let tmp = tempfile::tempdir().unwrap();
        let a = unpack_embedded_ruleset(tmp.path()).unwrap();
        let b = unpack_embedded_ruleset(tmp.path()).unwrap();
        assert_eq!(a, b);
        assert_eq!(fs::read_to_string(&a).unwrap(), std::str::from_utf8(EMBEDDED_RULESET.get_file(SGCONFIG).unwrap().contents()).unwrap());
        let rule = a.parent().unwrap().join("rules/raw-x-forwarded-for.yml");
        assert!(fs::read_to_string(rule).unwrap().contains("id: raw-x-forwarded-for"));
        // В base — ровно один каталог ruleset, staging-копий не осталось.
        let n = fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(n, 1);
        assert!(embedded_config_path().unwrap().is_file());
    }

    #[test]
    fn fails_open_without_config_or_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let p = orakul();
        let paths = vec!["server/auth.ts".to_string()];
        // Явный путь, которого нет → no-config.
        let r = run_astgrep_prescan(&paths, &cfg("app", "ast-grep", "ops/missing/sgconfig.yml"), &p, tmp.path(), &no_log);
        assert_eq!(r.skipped.as_deref(), Some("no-config"));
        // Бинарь-путь, которого нет → no-ast-grep.
        let r = run_astgrep_prescan(&paths, &cfg("app", "./bin/ast-grep", ""), &p, tmp.path(), &no_log);
        assert_eq!(r.skipped.as_deref(), Some("no-ast-grep"));
        // Голое имя, которого нет в PATH → error, а не Err/паника.
        std::fs::create_dir_all(tmp.path().join("app")).unwrap();
        let r = run_astgrep_prescan(&paths, &cfg("app", "definitely-missing-ast-grep-ai-reviewer", ""), &p, tmp.path(), &no_log);
        assert!(r.findings.is_empty());
        assert_eq!(r.scanned, 0);
        assert!(r.error.as_deref().unwrap().contains("ENOENT"));
        // Выключен → пусто.
        let mut off = cfg("app", "ast-grep", "");
        off.astgrep.as_mut().unwrap().enabled = false;
        assert_eq!(run_astgrep_prescan(&paths, &off, &p, tmp.path(), &no_log).skipped, None);
        // Нет lintable → пусто без запуска.
        let r = run_astgrep_prescan(&["README.md".into()], &cfg("app", "definitely-missing", ""), &p, tmp.path(), &no_log);
        assert_eq!(r.error, None);
    }

    #[cfg(unix)]
    #[test]
    fn fails_open_on_crash_with_empty_stdout() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("fake-sg");
        crate::prescan::tests::write_script(&bin, "echo 'panic: boom' >&2\nexit 101\n");
        std::fs::create_dir_all(tmp.path().join("app")).unwrap();
        let p = orakul();
        let r = run_astgrep_prescan(&["server/auth.ts".into()], &cfg("app", "./fake-sg", ""), &p, tmp.path(), &no_log);
        assert_eq!(r.error.as_deref(), Some("panic: boom"));
        assert_eq!(r.scanned, 1);
        assert!(r.findings.is_empty());
    }

    /// Фикстура `prescan-astgrep-xff` (DoD T14) без внешнего бинаря: фейковый
    /// ast-grep проверяет, что ему передан распакованный вшитый sgconfig и путь
    /// относительно appDir, и отдаёт stream-строку. Результат — ровно
    /// `findings.json` фикстуры, гейт — как в `meta.json`.
    #[cfg(unix)]
    #[test]
    fn fixture_prescan_astgrep_xff_fake_bin() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("app")).unwrap();
        let row = json!({
            "ruleId": "raw-x-forwarded-for", "severity": "error",
            "message": "Use clientIp(req) from server/net.ts — raw X-Forwarded-For is spoofable and bypasses anti-brute.\n",
            "file": "server/auth.ts", "range": { "start": { "line": 41 } },
        });
        let script = format!(
            "[ \"$1\" = scan ] && [ -f \"$3\" ] && [ \"$4\" = --json=stream ] && [ \"$5\" = server/auth.ts ] \
             && grep -q ruleDirs \"$3\" || {{ echo \"bad args: $*\" >&2; exit 9; }}\ncat <<'EOF'\n{row}\nEOF\n"
        );
        crate::prescan::tests::write_script(&tmp.path().join("sg"), &script);
        let p = orakul();
        let r = run_astgrep_prescan(&["server/auth.ts".into()], &cfg("app", "./sg", ""), &p, tmp.path(), &no_log);
        assert_eq!(r.error, None, "{r:?}");
        assert_eq!(r.findings, fixture_findings("prescan-astgrep-xff"));
        assert_fixture_gate("prescan-astgrep-xff", &r.findings, &p);
    }

    /// Интеграция с настоящим `ast-grep` (риск 3 RFC 16): вшитый ruleset после
    /// распаковки читается внешним процессом, и фикстура `prescan-astgrep-xff`
    /// воспроизводится на реальном файле. Без бинаря в PATH — пропуск с причиной.
    #[test]
    fn integration_real_astgrep_fixture_xff() {
        let Some(bin) = real_astgrep() else {
            eprintln!("SKIP integration_real_astgrep_fixture_xff: ast-grep не найден в PATH");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("app/server/auth.ts");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, auth_ts()).unwrap();
        let p = orakul();
        let logs = std::sync::Mutex::new(Vec::<String>::new());
        let log = |s: &str| logs.lock().unwrap().push(s.to_string());
        let r = run_astgrep_prescan(&["server/auth.ts".into()], &cfg("app", &bin.to_string_lossy(), ""), &p, tmp.path(), &log);
        assert_eq!(r.error, None, "{r:?} logs={:?}", logs.lock().unwrap());
        assert_eq!(r.findings, fixture_findings("prescan-astgrep-xff"));
        assert_fixture_gate("prescan-astgrep-xff", &r.findings, &p);
    }

    /// Все 12 вариантов сырого X-Forwarded-For из вшитого `fixtures/xff-variants.ts`
    /// ловятся настоящим `ast-grep` по вшитому ruleset (порт JS-теста
    /// `raw-x-forwarded-for: 12 header/get/headers quote+case variants`).
    #[test]
    fn integration_real_astgrep_xff_variants() {
        let Some(bin) = real_astgrep() else {
            eprintln!("SKIP integration_real_astgrep_xff_variants: ast-grep не найден в PATH");
            return;
        };
        let config = embedded_config_path().unwrap();
        let fixture = config.parent().unwrap().join("fixtures/xff-variants.ts");
        let out = run_command(
            &bin.to_string_lossy(),
            &["scan".into(), "-c".into(), config.to_string_lossy().into_owned(), "--json=stream".into(), fixture.to_string_lossy().into_owned()],
            config.parent().unwrap(),
            DEFAULT_MAX_BUFFER,
        )
        .unwrap();
        let rows: Vec<Value> = out.stdout.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(rows.len(), 12, "stderr={}", out.stderr);
        assert!(rows.iter().all(|r| r["ruleId"] == "raw-x-forwarded-for"));
    }
}
