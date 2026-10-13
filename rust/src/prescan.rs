//! Порт `lib/prescan.mjs`, `lib/prescan-runner.mjs`, `lib/prescan-shared.mjs`
//! (задача T14 RFC 20): детерминированный prescan до LLM — eslint + ast-grep.
//!
//! Раскладка повторяет JS:
//!
//! * общие помощники путей и промпта (`prescan-shared`) — [`normalize_app_path`],
//!   [`select_lintable_entries`], [`tool_path_to_app_path`],
//!   [`build_prescan_prompt_section`], [`run_command`];
//! * eslint (`prescan`) — [`eslint_json_to_findings`], [`dedupe_against_prescan`],
//!   [`run_eslint_prescan`];
//! * оркестрация (`prescan-runner`) — [`run_combined_prescan`], [`RealPrescan`]
//!   (реализация [`PrescanApi`]), [`format_prescan_log`].
//!
//! ast-grep — в [`crate::astgrep`].
//!
//! **Fail-open — контракт модуля (риск 3 RFC 20).** Отсутствие `eslint`/`ast-grep`,
//! ненулевой код возврата, мусор в stdout, переполнение буфера, несуществующий
//! каталог приложения — всё это пустой результат плюс тег `skipped` или текст
//! `error`, но не `Err`. Поэтому ни одна функция запуска здесь не возвращает
//! `Result`, а [`run_command`] возвращает `Result` ровно затем, чтобы вызывающий
//! был вынужден явно превратить сбой в поле `error` (в JS это `try/catch` вокруг
//! `runCommand`). `?` на `Command::spawn` в этом модуле нет и быть не должно.
//!
//! Профиль в JS читается синглтоном `getProfile()`; здесь (RFC 20, решение 3)
//! раскладка [`Layout`] и правила [`Profile`] передаются явно.
//!
//! Осознанные расхождения с JS:
//!
//! * `maxBuffer` у [`run_command`] считается в байтах UTF-8, в JS — в UTF-16
//!   code units склеенной строки. Порог (10 МБ) — защита от взрыва памяти, а не
//!   часть поведения, которое видит ревью;
//! * без `maxFiles` в конфиге (ключа нет) действует дефолт 30 — как дефолтный
//!   параметр `selectLintableEntries`; `loadReviewConfig` ключ пишет всегда.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Map, Value};

use crate::astgrep::{run_astgrep_prescan, AstgrepPrescanConfig};
use crate::findings::{js_number_or_zero, js_to_string, js_trim, js_truthy};
use crate::profile::{Layout, Profile};
use crate::report::js_slice_utf16;
use crate::traits::{PrescanApi, PrescanByTool, PrescanResult, ToolPrescanResult};

/// Блок-правила eslint по умолчанию (`lib/prescan.mjs:33`).
pub const DEFAULT_BLOCK_RULES: [&str; 2] = ["no-restricted-imports", "no-restricted-globals"];

/// Правила eslint, которые всегда `nit` (`lib/prescan.mjs:34`).
const NIT_RULES: [&str; 1] = ["max-len"];

/// Дефолт `maxBuffer` у `runCommand` (`lib/prescan-shared.mjs:106`).
pub const DEFAULT_MAX_BUFFER: usize = 10_000_000;

/// Дефолт `max` у `selectLintableEntries` (`lib/prescan-shared.mjs:35`).
pub const DEFAULT_MAX_FILES: f64 = 30.0;

// ---------------------------------------------------------------------------
// Мелкие JS-помощники
// ---------------------------------------------------------------------------

/// Число в JSON так, как его записал бы `JSON.stringify`: целое — без `.0`
/// (с `arbitrary_precision` `json!(42.0)` печатается как `42.0`), NaN/±∞ → `null`.
pub(crate) fn js_num_value(n: f64) -> Value {
    if !n.is_finite() {
        return Value::Null;
    }
    if n == n.trunc() && n.abs() < 9.0e15 {
        // `-0` JSON.stringify печатает как `0`.
        return Value::from(n as i64);
    }
    serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
}

/// JS `` `${v}` `` для поля находки: отсутствующий ключ — `"undefined"`.
fn js_template(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(v) => js_to_string(v),
    }
}

/// `(a || b || fallback)` по строкам: пустая строка ложна.
fn first_non_empty<'a>(parts: &[&'a str], fallback: &'a str) -> &'a str {
    parts.iter().copied().find(|s| !s.is_empty()).unwrap_or(fallback)
}

/// `fancy_regex::Regex::test` в JS-смысле: ошибка исполнения (лимит backtracking)
/// — «не совпало», как у сломанного паттерна в профиле.
fn re_test(re: &fancy_regex::Regex, s: &str) -> bool {
    re.is_match(s).unwrap_or(false)
}

/// Лексическая нормализация пути: `.` выбрасывается, `..` съедает компонент.
/// Аналог `path.resolve` без обращения к ФС.
pub(crate) fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path.resolve(base, p)`: абсолютный `p` — как есть, иначе от `base`.
pub(crate) fn js_resolve(base: &Path, p: &str) -> PathBuf {
    let p = if p.is_empty() { "." } else { p };
    let joined = if Path::new(p).is_absolute() { PathBuf::from(p) } else { base.join(p) };
    lexical_normalize(&joined)
}

/// `path.relative(from, to)` для абсолютных путей в posix-нотации.
fn js_relative(from: &str, to: &str) -> String {
    let norm = |s: &str| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for part in s.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    out.pop();
                }
                p => out.push(p.to_string()),
            }
        }
        out
    };
    let a = norm(from);
    let b = norm(to);
    let common = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), a.len() - common).collect();
    parts.extend(b[common..].iter().cloned());
    parts.join("/")
}

/// JS `entries.slice(0, max)` для неотрицательной длины и произвольного `max`.
fn js_slice_len(len: usize, max: f64) -> usize {
    if max.is_nan() {
        return 0;
    }
    let k = max.trunc();
    if k < 0.0 {
        (len as f64 + k).max(0.0) as usize
    } else {
        (k.min(len as f64)) as usize
    }
}

// ---------------------------------------------------------------------------
// prescan-shared: пути
// ---------------------------------------------------------------------------

/// Путь как в git diff → путь от корня репозитория (`normalizeAppPath`,
/// `lib/prescan-shared.mjs:16-25`).
pub fn normalize_app_path(path: &str, layout: &Layout) -> String {
    let app_dir = layout.app_dir.as_str();
    if app_dir.is_empty() {
        return path.to_string();
    }
    if path == app_dir || path.starts_with(&format!("{app_dir}/")) {
        return path.to_string();
    }
    // Diff иногда приходит путями без каталога приложения (монорепо-инструменты):
    // вернуть префикс можно только известным слоям, иначе `docs/x.ts` уехал бы в
    // `app/docs/x.ts`.
    if layout.app_prefix().is_some_and(|re| re_test(re, path)) {
        return format!("{app_dir}/{path}");
    }
    path.to_string()
}

/// `isLintablePath` (`lib/prescan-shared.mjs:27-30`).
pub fn is_lintable_path(path: &str, layout: &Layout) -> bool {
    layout
        .lintable()
        .is_some_and(|re| re_test(re, &normalize_app_path(path, layout)))
}

/// Lintable-файл: путь от корня репозитория и ключ из diff (`+++ b/...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintableEntry {
    pub app_path: String,
    pub diff_key: String,
}

/// Результат [`select_lintable_entries`].
#[derive(Debug, Clone, PartialEq)]
pub struct LintableSelection {
    pub entries: Vec<LintableEntry>,
    /// Сколько lintable-файлов было до обрезки.
    pub total: usize,
    pub truncated: bool,
}

/// `selectLintableEntries` (`lib/prescan-shared.mjs:35-47`): lintable-файлы с
/// сохранением ключа diff, без дублей по пути приложения, не больше `max`.
pub fn select_lintable_entries(changed_paths: &[String], max: f64, layout: &Layout) -> LintableSelection {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for diff_key in changed_paths {
        if !is_lintable_path(diff_key, layout) {
            continue;
        }
        let app_path = normalize_app_path(diff_key, layout);
        if !seen.insert(app_path.clone()) {
            continue;
        }
        entries.push(LintableEntry { app_path, diff_key: diff_key.clone() });
    }
    let total = entries.len();
    let truncated = (total as f64) > max;
    entries.truncate(js_slice_len(total, max));
    LintableSelection { entries, total, truncated }
}

/// Карта «путь приложения → ключ diff» (`buildPrescanPathMap`).
pub type PrescanPathMap = HashMap<String, String>;

/// `buildPrescanPathMap` (`lib/prescan-shared.mjs:49-53`).
pub fn build_prescan_path_map(entries: &[LintableEntry]) -> PrescanPathMap {
    let mut map = HashMap::new();
    for e in entries {
        // Map.set: последняя запись побеждает (дублей select_lintable_entries не даёт).
        map.insert(e.app_path.clone(), e.diff_key.clone());
    }
    map
}

/// Ключ `finding.file` = путь как в diff (`resolvePrescanDiffKey`,
/// `lib/prescan-shared.mjs:56-60`).
pub fn resolve_prescan_diff_key(app_path: &str, path_map: Option<&PrescanPathMap>, layout: &Layout) -> String {
    let norm = normalize_app_path(app_path, layout);
    if let Some(key) = path_map.and_then(|m| m.get(&norm)) {
        return key.clone();
    }
    norm
}

fn is_windows_abs(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Путь из вывода eslint / ast-grep → путь от корня репозитория
/// (`toolPathToAppPath`, `lib/prescan-shared.mjs:63-78`).
pub fn tool_path_to_app_path(abs_or_rel: &str, cwd: &Path, layout: &Layout) -> String {
    let app_prefix = if layout.app_dir.is_empty() { String::new() } else { format!("{}/", layout.app_dir) };
    let norm = abs_or_rel.replace('\\', "/");
    if !app_prefix.is_empty() && norm.starts_with(&app_prefix) {
        return norm;
    }
    if !app_prefix.is_empty() {
        if let Some(idx) = norm.find(&format!("/{app_prefix}")) {
            return norm[idx + 1..].to_string();
        }
    }
    if !norm.starts_with('/') && !is_windows_abs(&norm) {
        return normalize_app_path(&norm, layout);
    }
    let cwd_s = cwd.to_string_lossy().replace('\\', "/");
    let rel = js_relative(&cwd_s, &norm);
    if !app_prefix.is_empty() && rel.starts_with(&app_prefix) {
        return rel;
    }
    if !rel.is_empty() && !rel.starts_with("..") {
        return normalize_app_path(&rel, layout);
    }
    norm
}

// ---------------------------------------------------------------------------
// prescan-shared: промпт, склейка, процессы
// ---------------------------------------------------------------------------

/// Секция промпта с находками prescan (`buildPrescanPromptSection`,
/// `lib/prescan-shared.mjs:80-86`).
pub fn build_prescan_prompt_section(findings: &[Value]) -> String {
    if findings.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = findings
        .iter()
        .map(|f| {
            format!(
                "- [{}] `{}:{}` — {}: {}",
                js_template(f.get("severity")),
                js_template(f.get("file")),
                js_template(f.get("line")),
                js_template(f.get("title")),
                js_template(f.get("body")),
            )
        })
        .collect();
    format!(
        "{}\n\nDo NOT repeat these deterministic pre-scan findings (eslint/ast-grep) — they are already reported.",
        lines.join("\n")
    )
}

/// `mergePrescanSkipped` (`lib/prescan-shared.mjs:88-91`): теги через `+`.
pub fn merge_prescan_skipped(parts: &[Option<&str>]) -> Option<String> {
    let s: Vec<&str> = parts.iter().flatten().copied().filter(|p| !p.is_empty()).collect();
    if s.is_empty() { None } else { Some(s.join("+")) }
}

/// `mergeLintableTotal` (`lib/prescan-shared.mjs:99-105`): максимум, ноль → нет.
pub fn merge_lintable_total(values: &[Option<usize>]) -> Option<usize> {
    let max = values.iter().flatten().copied().max().unwrap_or(0);
    if max > 0 { Some(max) } else { None }
}

/// `logToolFailure` (`lib/prescan-shared.mjs:93-97`).
pub fn log_tool_failure(log: &dyn Fn(&str), label: &str, out: &CommandOutput) {
    if out.code == 0 {
        return;
    }
    let msg = js_slice_utf16(js_trim(first_non_empty(&[&out.stderr, &out.stdout], "")), 300);
    if msg.is_empty() {
        log(&format!("prescan {label} exit {}", out.code));
    } else {
        log(&format!("prescan {label} exit {}: {msg}", out.code));
    }
}

/// Итог процесса: код возврата и вывод. Ненулевой код — **не** ошибка: eslint
/// выходит с `1`, когда нашёл нарушения, и его stdout при этом нужен.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// `code ?? 1`: процесс, убитый сигналом, считается кодом `1`.
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// `runCommand` (`lib/prescan-shared.mjs:106-153`): спавн без shell, stdin закрыт,
/// stdout/stderr копятся до `max_buffer`.
///
/// `Err` — только сбой самого запуска (бинаря нет, каталога нет) или переполнение
/// буфера; текст ошибки повторяет Node (`spawn <cmd> ENOENT`, `prescan stdout/stderr
/// exceeded N bytes`). Вызывающий обязан превратить его в `error` результата.
pub fn run_command(cmd: &str, args: &[String], cwd: &Path, max_buffer: usize) -> Result<CommandOutput, String> {
    let mut child = match Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(format!("spawn {cmd} ENOENT")),
        Err(e) => return Err(format!("spawn {cmd}: {e}")),
    };

    let overflow = Arc::new(AtomicBool::new(false));
    let (Some(out_pipe), Some(err_pipe)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("spawn {cmd}: нет труб stdout/stderr"));
    };
    let out_reader = spawn_reader(out_pipe, max_buffer, Arc::clone(&overflow));
    let err_reader = spawn_reader(err_pipe, max_buffer, Arc::clone(&overflow));

    let mut nap = Duration::from_millis(1);
    let status = loop {
        if overflow.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_reader.join();
            let _ = err_reader.join();
            return Err(format!("prescan stdout/stderr exceeded {max_buffer} bytes"));
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("wait {cmd}: {e}"));
            }
        }
        thread::sleep(nap);
        nap = (nap * 2).min(Duration::from_millis(20));
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    // Процесс мог успеть завершиться раньше, чем цикл увидел флаг: переполнение
    // побеждает «вышел сам», как reject до close в JS.
    if overflow.load(Ordering::SeqCst) {
        return Err(format!("prescan stdout/stderr exceeded {max_buffer} bytes"));
    }
    Ok(CommandOutput { code: status.code().unwrap_or(1), stdout, stderr })
}

/// Читатель трубы: копит вывод до `max_buffer`, при превышении поднимает флаг.
fn spawn_reader<R: Read + Send + 'static>(
    mut pipe: R,
    max_buffer: usize,
    overflow: Arc<AtomicBool>,
) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut acc: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8 * 1024];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    acc.extend_from_slice(&buf[..n]);
                    if acc.len() > max_buffer {
                        overflow.store(true, Ordering::SeqCst);
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&acc).into_owned()
    })
}

/// Текст `error` результата при пустом stdout (`lib/prescan.mjs:162`,
/// `lib/astgrep.mjs:149`): только при ненулевом коде.
pub(crate) fn empty_stdout_error(out: &CommandOutput) -> Option<String> {
    if out.code == 0 {
        return None;
    }
    let raw = first_non_empty(&[&out.stderr, &out.stdout], "empty stdout");
    Some(js_slice_utf16(js_trim(raw), 200))
}

/// Тип журнала стадий: строка в лог прогона.
pub type LogFn<'a> = &'a (dyn Fn(&str) + Sync);

// ---------------------------------------------------------------------------
// Конфиг prescan
// ---------------------------------------------------------------------------

/// Поля `cfg.prescan`, которые читают eslint и ast-grep (`lib/config.mjs`,
/// порт — `config::load_review_config_parts`, ключ `prescan`).
#[derive(Debug, Clone, PartialEq)]
pub struct PrescanConfig {
    pub enabled: bool,
    /// `prescan.appDir`: каталог, от которого запускаются линтеры. Пусто — корень.
    pub app_dir: String,
    /// `prescan.eslintBin` — запасной бинарь, если локального eslint нет.
    pub eslint_bin: String,
    pub max_files: f64,
    /// `None` — ключа нет (дефолтные блок-правила).
    pub block_rules: Option<Vec<String>>,
    /// `None` — подсекции `astgrep` нет: ast-grep выключен.
    pub astgrep: Option<AstgrepPrescanConfig>,
}

impl Default for PrescanConfig {
    fn default() -> Self {
        PrescanConfig {
            enabled: false,
            app_dir: String::new(),
            eslint_bin: "npx".into(),
            max_files: DEFAULT_MAX_FILES,
            block_rules: None,
            astgrep: None,
        }
    }
}

/// Строки массива без пустых (`.filter(Boolean)` по строкам).
pub(crate) fn str_list(v: Option<&Value>) -> Option<Vec<String>> {
    v.and_then(Value::as_array).map(|a| {
        a.iter()
            .filter(|x| js_truthy(x))
            .map(js_to_string)
            .collect()
    })
}

impl PrescanConfig {
    /// Из объекта конфига целиком (`cfg`, ключ `prescan`) или из самой секции.
    pub fn from_cfg(cfg: &Value) -> Self {
        let p = cfg.get("prescan").unwrap_or(cfg);
        let Some(obj) = p.as_object() else { return Self::default() };
        let s = |k: &str| obj.get(k).filter(|v| js_truthy(v)).map(js_to_string);
        PrescanConfig {
            enabled: obj.get("enabled").is_some_and(js_truthy),
            app_dir: s("appDir").unwrap_or_default(),
            eslint_bin: s("eslintBin").unwrap_or_else(|| "npx".into()),
            max_files: obj
                .get("maxFiles")
                .and_then(crate::findings::js_to_number)
                .unwrap_or(DEFAULT_MAX_FILES),
            block_rules: str_list(obj.get("blockRules")),
            astgrep: obj.get("astgrep").and_then(Value::as_object).map(AstgrepPrescanConfig::from_obj),
        }
    }
}

// ---------------------------------------------------------------------------
// eslint
// ---------------------------------------------------------------------------

/// Путь от корня репозитория → путь относительно `appDir` (`stripAppDir`).
/// Путь файла → позиционный аргумент линтера (`toolPathArg`,
/// `lib/prescan-shared.mjs`). Пути приходят из диффа PR: файл `--config=evil.ts`
/// без префикса eslint/ast-grep прочли бы как опцию. `./` вместо `--` — eslint
/// может идти через `npx`, который `--` трактует сам; обычные пути не меняются.
pub(crate) fn tool_path_arg(path: String) -> String {
    if path.starts_with('-') { format!("./{path}") } else { path }
}

pub(crate) fn strip_app_dir(path: &str, app_dir: &str) -> String {
    if app_dir.is_empty() {
        return path.to_string();
    }
    match path.strip_prefix(&format!("{app_dir}/")) {
        Some(rest) => rest.to_string(),
        None => path.to_string(),
    }
}

/// `eslint ruleId → severity` (`mapEslintRuleSeverity`, `lib/prescan.mjs:57-62`).
/// `eslint_severity`: `None` — поля нет (дефолтный параметр JS = 2).
pub fn map_eslint_rule_severity(rule_id: &str, eslint_severity: Option<&Value>, block_rules: &HashSet<String>) -> &'static str {
    if block_rules.contains(rule_id) {
        return "critical";
    }
    if NIT_RULES.contains(&rule_id) {
        return "nit";
    }
    // `eslintSeverity === 2` — строгое сравнение с числом.
    let is_two = match eslint_severity {
        None => true,
        Some(v) => v.as_f64() == Some(2.0),
    };
    if is_two { "major" } else { "minor" }
}

/// Множество блок-правил eslint: пустой/отсутствующий список → дефолт
/// (`blockList?.length ? … : DEFAULT_BLOCK_RULES`, `lib/prescan.mjs:178-179`).
pub fn eslint_block_rule_set(rules: Option<&[String]>) -> HashSet<String> {
    match rules {
        Some(r) if r.iter().any(|x| !x.is_empty()) => r.iter().filter(|x| !x.is_empty()).cloned().collect(),
        _ => DEFAULT_BLOCK_RULES.iter().map(|s| s.to_string()).collect(),
    }
}

/// Парсит `eslint --format json` → находки (`eslintJsonToFindings`,
/// `lib/prescan.mjs:65-90`). Не массив → пусто.
pub fn eslint_json_to_findings(
    results: &Value,
    block_rules: &HashSet<String>,
    path_map: Option<&PrescanPathMap>,
    cwd: &Path,
    layout: &Layout,
) -> Vec<Value> {
    let Some(files) = results.as_array() else { return Vec::new() };
    let mut findings = Vec::new();
    for file in files {
        let file_path = file.get("filePath").filter(|v| js_truthy(v)).map(js_to_string).unwrap_or_default();
        let app_path = tool_path_to_app_path(&file_path, cwd, layout);
        let file_key = resolve_prescan_diff_key(&app_path, path_map, layout);
        let messages = file.get("messages").filter(|v| js_truthy(v));
        let Some(messages) = messages.and_then(Value::as_array) else { continue };
        for msg in messages {
            if !js_truthy(msg) {
                continue;
            }
            // `msg.severity === 0` — строгое сравнение.
            if msg.get("severity").and_then(Value::as_f64) == Some(0.0) {
                continue;
            }
            let rule_id = msg
                .get("ruleId")
                .filter(|v| js_truthy(v))
                .map(js_to_string)
                .unwrap_or_else(|| "eslint".to_string());
            // Дефолт `= 2` только для отсутствующего поля: null уходит как есть (→ minor).
            let sev_arg = msg.get("severity");
            let severity = map_eslint_rule_severity(&rule_id, sev_arg, block_rules);
            let body = js_trim(&msg.get("message").filter(|v| js_truthy(v)).map(js_to_string).unwrap_or_default())
                .to_string();
            findings.push(json!({
                "file": file_key,
                "line": js_num_value(js_number_or_zero(msg.get("line"))),
                "severity": severity,
                "confidence": 1,
                "title": format!("eslint: {rule_id}"),
                "body": body,
                "model": "eslint-prescan",
                "prescan": true,
                "ruleId": rule_id,
                "prescanBlock": block_rules.contains(&rule_id),
            }));
        }
    }
    findings
}

/// `findingLocationKey` (`lib/prescan.mjs:92-94`): файл + корзина по 3 строки.
pub fn finding_location_key(f: &Value) -> String {
    let file = js_template(f.get("file"));
    let line = f.get("line").and_then(crate::findings::js_to_number);
    match line {
        Some(l) if l > 0.0 => format!("{file}|{}", ((l - 1.0) / 3.0).floor() as i64),
        _ => format!("{file}|out"),
    }
}

fn location_key_aliases(f: &Value, layout: &Layout) -> Vec<String> {
    let mut keys = vec![finding_location_key(f)];
    let p = f.get("file").filter(|v| js_truthy(v)).map(js_to_string).unwrap_or_default();
    let app_dir = layout.app_dir.as_str();
    // Одна и та же точка кода приходит и с префиксом appDir (от линтера), и без
    // него (из diff): сравниваем оба варианта.
    if app_dir.is_empty() || p.is_empty() {
        return keys;
    }
    let mut alt = f.clone();
    let other = match p.strip_prefix(&format!("{app_dir}/")) {
        Some(rest) => rest.to_string(),
        None => format!("{app_dir}/{p}"),
    };
    if let Some(o) = alt.as_object_mut() {
        o.insert("file".into(), Value::String(other));
    }
    keys.push(finding_location_key(&alt));
    keys
}

/// `SEVERITY_RANK[s] ?? 9` (`lib/prescan.mjs`): critical 0 … nit 3, неизвестная — 9.
fn severity_rank(f: &Value) -> u8 {
    match f.get("severity").and_then(Value::as_str) {
        Some("critical") => 0,
        Some("major") => 1,
        Some("minor") => 2,
        Some("nit") => 3,
        _ => 9,
    }
}

/// Убирает LLM-находки в тех же `file|line-bucket`, что уже покрыты prescan
/// (`dedupeAgainstPrescan`, `lib/prescan.mjs`).
///
/// Покрытие — только prescan-находкой НЕ НИЖЕ по severity (RFC 17): бакет — три строки, а
/// не тема, и `max-len` рядом ничего не говорит о critical «SQL-инъекция».
pub fn dedupe_against_prescan(llm_findings: &[Value], prescan_findings: &[Value], layout: &Layout) -> Vec<Value> {
    if prescan_findings.is_empty() {
        return llm_findings.to_vec();
    }
    let mut covered: HashMap<String, u8> = HashMap::new();
    for f in prescan_findings {
        let rank = severity_rank(f);
        for k in location_key_aliases(f, layout) {
            let e = covered.entry(k).or_insert(rank);
            if rank < *e {
                *e = rank;
            }
        }
    }
    llm_findings
        .iter()
        .filter(|f| {
            let rank = severity_rank(f);
            !location_key_aliases(f, layout).iter().any(|k| covered.get(k).is_some_and(|&c| c <= rank))
        })
        .cloned()
        .collect()
}

fn empty_tool() -> ToolPrescanResult {
    ToolPrescanResult::default()
}

/// Запуск eslint на изменённых lintable-файлах (`runEslintPrescan`,
/// `lib/prescan.mjs:127-193`). Строго fail-open: сбой — поле `error`/`skipped`.
pub fn run_eslint_prescan(
    changed_paths: &[String],
    cfg: &PrescanConfig,
    layout: &Layout,
    cwd: &Path,
    log: LogFn<'_>,
) -> ToolPrescanResult {
    if !cfg.enabled {
        return empty_tool();
    }
    let app_dir = js_resolve(cwd, &cfg.app_dir);
    if !app_dir.join("node_modules/eslint/package.json").exists() {
        let shown = if cfg.app_dir.is_empty() { "." } else { cfg.app_dir.as_str() };
        log(&format!("prescan: eslint недоступен (нет node_modules в {shown}) — skip"));
        return ToolPrescanResult { skipped: Some("no-eslint".into()), ..empty_tool() };
    }

    let sel = select_lintable_entries(changed_paths, cfg.max_files, layout);
    if sel.truncated {
        let m = js_to_string(&js_num_value(cfg.max_files));
        log(&format!(
            "prescan: lintable файлов {} > лимита {m} — сканируем первые {m}",
            sel.total
        ));
    }
    if sel.entries.is_empty() {
        return empty_tool();
    }

    let path_map = build_prescan_path_map(&sel.entries);
    let rel_paths: Vec<String> =
        sel.entries.iter().map(|e| tool_path_arg(strip_app_dir(&e.app_path, &cfg.app_dir))).collect();
    let local_bin = app_dir.join("node_modules/.bin/eslint");
    let (bin, mut args): (String, Vec<String>) = if local_bin.exists() {
        (local_bin.to_string_lossy().into_owned(), Vec::new())
    } else {
        (cfg.eslint_bin.clone(), vec!["eslint".into()])
    };
    args.extend(["--format", "json", "--no-error-on-unmatched-pattern"].map(String::from));
    args.extend(rel_paths);

    let out = match run_command(&bin, &args, &app_dir, DEFAULT_MAX_BUFFER) {
        Ok(o) => o,
        Err(e) => {
            log(&format!("prescan eslint failed (fail-open): {e}"));
            return ToolPrescanResult {
                lintable_total: Some(sel.total),
                truncated: sel.truncated,
                error: Some(e),
                ..empty_tool()
            };
        }
    };
    if js_trim(&out.stdout).is_empty() {
        log_tool_failure(log, "eslint", &out);
        return ToolPrescanResult {
            scanned: sel.entries.len(),
            lintable_total: Some(sel.total),
            truncated: sel.truncated,
            error: empty_stdout_error(&out),
            ..empty_tool()
        };
    }
    let parsed: Value = match serde_json::from_str(&out.stdout) {
        Ok(v) => v,
        Err(_) => {
            log("prescan eslint: не JSON stdout");
            return ToolPrescanResult {
                scanned: sel.entries.len(),
                lintable_total: Some(sel.total),
                truncated: sel.truncated,
                error: Some("bad json".into()),
                ..empty_tool()
            };
        }
    };
    let block = eslint_block_rule_set(cfg.block_rules.as_deref());
    let findings = eslint_json_to_findings(&parsed, &block, Some(&path_map), cwd, layout);
    ToolPrescanResult {
        prompt_section: build_prescan_prompt_section(&findings),
        findings,
        scanned: sel.entries.len(),
        lintable_total: Some(sel.total),
        truncated: sel.truncated,
        ..empty_tool()
    }
}

// ---------------------------------------------------------------------------
// prescan-runner
// ---------------------------------------------------------------------------

/// eslint + ast-grep параллельно; склейка находок и секций промпта
/// (`runCombinedPrescan`, `lib/prescan-runner.mjs:8-33`).
pub fn run_combined_prescan(
    changed_paths: &[String],
    cfg: &PrescanConfig,
    profile: &Profile,
    cwd: &Path,
    log: LogFn<'_>,
) -> PrescanResult {
    let (eslint, astgrep) = thread::scope(|s| {
        let a = s.spawn(|| run_astgrep_prescan(changed_paths, cfg, profile, cwd, log));
        let e = run_eslint_prescan(changed_paths, cfg, profile.layout(), cwd, log);
        // Паника потока ast-grep — тоже сбой инструмента, а не прогона.
        let a = a.join().unwrap_or_else(|_| ToolPrescanResult {
            error: Some("ast-grep prescan panicked".into()),
            ..empty_tool()
        });
        (e, a)
    });
    combine_tool_results(eslint, astgrep)
}

/// Склейка двух подрезультатов — чистая часть `runCombinedPrescan`.
pub fn combine_tool_results(eslint: ToolPrescanResult, astgrep: ToolPrescanResult) -> PrescanResult {
    let mut findings = eslint.findings.clone();
    findings.extend(astgrep.findings.iter().cloned());
    let prompt_section = [eslint.prompt_section.as_str(), astgrep.prompt_section.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let errors: Vec<&str> = [eslint.error.as_deref(), astgrep.error.as_deref()]
        .into_iter()
        .flatten()
        .filter(|e| !e.is_empty())
        .collect();
    let scanned = if eslint.scanned != 0 { eslint.scanned } else { astgrep.scanned };
    PrescanResult {
        prompt_section,
        scanned,
        lintable_total: merge_lintable_total(&[eslint.lintable_total, astgrep.lintable_total]),
        truncated: eslint.truncated || astgrep.truncated,
        skipped: merge_prescan_skipped(&[eslint.skipped.as_deref(), astgrep.skipped.as_deref()]),
        by_tool: Some(PrescanByTool { eslint: eslint.findings.len(), astgrep: astgrep.findings.len() }),
        error: if errors.is_empty() { None } else { Some(errors.join("; ")) },
        findings,
        eslint: Some(eslint),
        astgrep: Some(astgrep),
    }
}

/// Строка лога prescan (`formatPrescanLog`, `lib/prescan-runner.mjs:42-48`).
pub fn format_prescan_log(result: &PrescanResult) -> String {
    let trunc = if result.truncated {
        format!(
            " (truncated from {})",
            result.lintable_total.map(|n| n.to_string()).unwrap_or_else(|| "undefined".into())
        )
    } else {
        String::new()
    };
    let by_tool = result
        .by_tool
        .map(|b| format!(" [eslint:{}, ast-grep:{}]", b.eslint, b.astgrep))
        .unwrap_or_default();
    let err = match result.error.as_deref() {
        Some(e) if !e.is_empty() => format!(" (warn: {})", js_slice_utf16(e, 80)),
        _ => String::new(),
    };
    format!("{} issue(s) on {} file(s){trunc}{by_tool}{err}", result.findings.len(), result.scanned)
}

/// Боевой prescan — порт `createPrescanRunner(cfg, { log })`
/// (`lib/prescan-runner.mjs:35-40`).
///
/// `run` никогда не возвращает `Err`: сбой инструмента — поле `error` результата
/// (контракт [`PrescanApi`]).
pub struct RealPrescan<'p> {
    cfg: PrescanConfig,
    profile: &'p Profile,
    cwd: PathBuf,
    log: Box<dyn Fn(&str) + Send + Sync + 'p>,
}

impl std::fmt::Debug for RealPrescan<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealPrescan").field("cfg", &self.cfg).field("cwd", &self.cwd).finish_non_exhaustive()
    }
}

impl<'p> RealPrescan<'p> {
    /// `cwd` — каталог прогона (`process.cwd()` в JS): от него считаются `appDir`,
    /// относительный `configPath` ast-grep и пути из вывода линтеров.
    pub fn new(
        cfg: PrescanConfig,
        profile: &'p Profile,
        cwd: PathBuf,
        log: impl Fn(&str) + Send + Sync + 'p,
    ) -> Self {
        RealPrescan { cfg, profile, cwd, log: Box::new(log) }
    }

    /// Из полного объекта конфига прогона (`cfg.prescan`).
    pub fn from_cfg(cfg: &Value, profile: &'p Profile, cwd: PathBuf, log: impl Fn(&str) + Send + Sync + 'p) -> Self {
        Self::new(PrescanConfig::from_cfg(cfg), profile, cwd, log)
    }

    pub fn config(&self) -> &PrescanConfig {
        &self.cfg
    }
}

impl PrescanApi for RealPrescan<'_> {
    fn run(&self, changed_paths: &[String]) -> Result<PrescanResult> {
        let log = |s: &str| (self.log)(s);
        Ok(run_combined_prescan(changed_paths, &self.cfg, self.profile, &self.cwd, &log))
    }
}

/// Мелочь для тестов и диагностики: объект `{findings,...}` → JSON, как его
/// пишет `obs.logEvent('prescan')`.
pub fn prescan_result_to_value(r: &PrescanResult) -> Value {
    serde_json::to_value(r).unwrap_or_else(|_| Value::Object(Map::new()))
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::findings::{resolve_blockers, Gate};
    use std::sync::Mutex;

    pub(crate) fn repo_root() -> PathBuf {
        crate::profile::package_root()
    }

    pub(crate) fn orakul() -> Profile {
        Profile::load_file_at(&repo_root().join("profiles/orakul.json"), &repo_root()).unwrap()
    }

    pub(crate) fn neutral() -> Profile {
        Profile::default_builtin(&repo_root())
    }

    pub(crate) fn no_log(_: &str) {}

    pub(crate) fn fixture_findings(id: &str) -> Vec<Value> {
        let text = std::fs::read_to_string(repo_root().join("fixtures").join(id).join("findings.json")).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        v["findings"].as_array().unwrap().clone()
    }

    /// Гейт + prescan-конфиг так, как их строит eval-harness (`lib/eval.mjs:194`),
    /// и проверка `expected` из `meta.json` фикстуры.
    pub(crate) fn assert_fixture_gate(id: &str, findings: &[Value], profile: &Profile) {
        let meta: Value = serde_json::from_str(
            &std::fs::read_to_string(repo_root().join("fixtures").join(id).join("meta.json")).unwrap(),
        )
        .unwrap();
        let mut env = Map::new();
        env.insert("REVIEW_MODE".into(), json!("consensus"));
        env.insert("OLLAMA_MODELS".into(), json!("m1,m2"));
        let parts = crate::config::load_review_config_parts(&env, &profile.config_input());
        let gate = Gate::from_value(&parts["gate"]);
        let out = resolve_blockers(findings, &gate, Some(&parts["prescan"]));
        let exp = &meta["expected"];
        assert_eq!(out.would_block, exp["shouldBlock"].as_bool().unwrap(), "{id}: shouldBlock");
        assert_eq!(out.blockers.len() as u64, exp["blockerCount"].as_u64().unwrap(), "{id}: blockerCount");
        let titles: Vec<&str> = out.blockers.iter().map(|&i| findings[i]["title"].as_str().unwrap()).collect();
        for t in exp["titles"].as_array().unwrap() {
            assert!(titles.contains(&t.as_str().unwrap()), "{id}: нет блокера «{t}», есть {titles:?}");
        }
    }

    #[cfg(unix)]
    pub(crate) fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn cfg_app(app_dir: &str) -> PrescanConfig {
        PrescanConfig { enabled: true, app_dir: app_dir.into(), ..PrescanConfig::default() }
    }

    #[test]
    fn normalize_app_path_returns_prefix_only_to_layers() {
        let p = orakul();
        let l = p.layout();
        assert_eq!(normalize_app_path("server/auth.ts", l), "app/server/auth.ts");
        assert_eq!(normalize_app_path("app/server/auth.ts", l), "app/server/auth.ts");
        assert_eq!(normalize_app_path("docs/x.ts", l), "docs/x.ts");
        assert_eq!(normalize_app_path("app", l), "app");
        let n = neutral();
        assert_eq!(normalize_app_path("server/auth.ts", n.layout()), "server/auth.ts");
    }

    #[test]
    fn select_lintable_entries_dedup_and_cap() {
        let p = orakul();
        let paths: Vec<String> = ["server/a.ts", "app/server/a.ts", "README.md", "client/b.tsx", "shared/c.ts"]
            .map(String::from)
            .to_vec();
        let sel = select_lintable_entries(&paths, 2.0, p.layout());
        assert_eq!(sel.total, 3);
        assert!(sel.truncated);
        assert_eq!(
            sel.entries,
            vec![
                LintableEntry { app_path: "app/server/a.ts".into(), diff_key: "server/a.ts".into() },
                LintableEntry { app_path: "app/client/b.tsx".into(), diff_key: "client/b.tsx".into() },
            ]
        );
        let all = select_lintable_entries(&paths, 30.0, p.layout());
        assert!(!all.truncated);
        assert_eq!(all.entries.len(), 3);
    }

    #[test]
    fn tool_path_to_app_path_variants() {
        let p = orakul();
        let l = p.layout();
        let cwd = Path::new("/repo/app");
        assert_eq!(tool_path_to_app_path("app/server/x.ts", cwd, l), "app/server/x.ts");
        assert_eq!(tool_path_to_app_path("/repo/Orakul/app/client/src/foo.ts", cwd, l), "app/client/src/foo.ts");
        assert_eq!(tool_path_to_app_path("server/auth.ts", cwd, l), "app/server/auth.ts");
        assert_eq!(tool_path_to_app_path("C:\\r\\app\\server\\x.ts", cwd, l), "app/server/x.ts");
        // Абсолютный путь вне app/ → relative от cwd.
        let n = neutral();
        assert_eq!(tool_path_to_app_path("/repo/src/x.ts", Path::new("/repo"), n.layout()), "src/x.ts");
        assert_eq!(tool_path_to_app_path("/other/x.ts", Path::new("/repo"), n.layout()), "/other/x.ts");
    }

    #[test]
    fn eslint_json_to_findings_no_restricted_imports_critical() {
        let p = orakul();
        let map = build_prescan_path_map(&[LintableEntry {
            app_path: "app/client/src/foo.ts".into(),
            diff_key: "app/client/src/foo.ts".into(),
        }]);
        let res = json!([{ "filePath": "/repo/app/client/src/foo.ts",
            "messages": [{ "ruleId": "no-restricted-imports", "severity": 2, "message": "layer", "line": 3 }] }]);
        let f = eslint_json_to_findings(&res, &eslint_block_rule_set(None), Some(&map), Path::new("/repo"), p.layout());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["severity"], "critical");
        assert_eq!(f[0]["prescan"], true);
        assert_eq!(f[0]["prescanBlock"], true);
        assert_eq!(f[0]["file"], "app/client/src/foo.ts");
        assert_eq!(f[0]["line"], 3);
    }

    #[test]
    fn eslint_json_to_findings_diff_key_without_app_prefix() {
        let p = orakul();
        let map = build_prescan_path_map(&[LintableEntry {
            app_path: "app/client/src/foo.ts".into(),
            diff_key: "client/src/foo.ts".into(),
        }]);
        let res = json!([{ "filePath": "/repo/Orakul/app/client/src/foo.ts",
            "messages": [{ "ruleId": "no-restricted-imports", "severity": 2, "message": "layer", "line": 1 }] }]);
        let f = eslint_json_to_findings(&res, &eslint_block_rule_set(None), Some(&map), Path::new("/repo"), p.layout());
        assert_eq!(f[0]["file"], "client/src/foo.ts");
    }

    #[test]
    fn eslint_null_severity_is_not_default_two() {
        // JS: дефолт параметра `eslintSeverity = 2` срабатывает только на undefined;
        // `null === 2` ложно → minor, а отсутствующее поле → major.
        let p = neutral();
        let res = json!([{ "filePath": "a.ts", "messages": [
            { "ruleId": "no-console", "severity": null, "line": 3, "message": "x" },
            { "ruleId": "no-console", "line": 4, "message": "y" },
        ] }]);
        let f = eslint_json_to_findings(&res, &eslint_block_rule_set(None), None, Path::new("/r"), p.layout());
        assert_eq!(f[0]["severity"], "minor");
        assert_eq!(f[1]["severity"], "major");
    }

    #[test]
    fn eslint_severity_mapping() {
        let block = eslint_block_rule_set(None);
        assert_eq!(map_eslint_rule_severity("max-len", Some(&json!(1)), &block), "nit");
        assert_eq!(map_eslint_rule_severity("no-unused-vars", Some(&json!(2)), &block), "major");
        assert_eq!(map_eslint_rule_severity("no-unused-vars", Some(&json!(1)), &block), "minor");
        assert_eq!(map_eslint_rule_severity("no-unused-vars", None, &block), "major", "дефолтный параметр = 2");
        // severity 0 и null-ruleId.
        let p = neutral();
        let res = json!([{ "filePath": "a.ts", "messages": [
            { "ruleId": "x", "severity": 0, "line": 1, "message": "off" },
            { "ruleId": null, "severity": 2, "line": "abc", "message": " parse " },
        ] }]);
        let f = eslint_json_to_findings(&res, &block, None, Path::new("/r"), p.layout());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["title"], "eslint: eslint");
        assert_eq!(f[0]["line"], 0);
        assert_eq!(f[0]["body"], "parse");
        // Пустой список блок-правил → дефолт; непустой — свой.
        assert!(eslint_block_rule_set(Some(&[])).contains("no-restricted-globals"));
        assert!(!eslint_block_rule_set(Some(&["x".into()])).contains("no-restricted-globals"));
        assert!(eslint_json_to_findings(&json!({}), &block, None, Path::new("/r"), p.layout()).is_empty());
    }

    #[test]
    fn dedupe_against_prescan_bucket_and_alias() {
        let n = neutral();
        let pre = vec![json!({"file": "a.ts", "line": 10, "severity": "critical", "model": "eslint-prescan"})];
        let llm = vec![json!({"file": "a.ts", "line": 11, "severity": "major", "title": "dup", "model": "m1"})];
        assert!(dedupe_against_prescan(&llm, &pre, n.layout()).is_empty());
        let far = vec![json!({"file": "a.ts", "line": 20, "title": "far"})];
        assert_eq!(dedupe_against_prescan(&far, &pre, n.layout()).len(), 1);

        let o = orakul();
        let pre = vec![json!({"file": "app/client/x.ts", "line": 10, "severity": "critical", "model": "eslint-prescan"})];
        let llm = vec![json!({"file": "client/x.ts", "line": 11, "severity": "major", "title": "dup", "model": "m1"})];
        assert!(dedupe_against_prescan(&llm, &pre, o.layout()).is_empty());
        assert_eq!(finding_location_key(&json!({"file": "a.ts"})), "a.ts|out");
    }

    /// RFC 17: prescan-находка ниже по severity не покрывает бакет — critical рядом с
    /// `max-len` остаётся; равная или выше — покрывает.
    #[test]
    fn dedupe_against_prescan_только_не_ниже_по_severity() {
        let n = neutral();
        let pre = vec![json!({"file": "a.ts", "line": 10, "severity": "nit", "model": "eslint-prescan"})];
        let crit = vec![json!({"file": "a.ts", "line": 11, "severity": "critical", "title": "sqli", "model": "m1"})];
        assert_eq!(dedupe_against_prescan(&crit, &pre, n.layout()).len(), 1);
        let nit = vec![json!({"file": "a.ts", "line": 11, "severity": "nit", "title": "len", "model": "m1"})];
        assert!(dedupe_against_prescan(&nit, &pre, n.layout()).is_empty());
        // в бакете несколько prescan-находок: решает самая высокая
        let pre2 = vec![
            json!({"file": "a.ts", "line": 10, "severity": "nit", "model": "eslint-prescan"}),
            json!({"file": "a.ts", "line": 11, "severity": "major", "model": "ast-grep-prescan"}),
        ];
        let major = vec![json!({"file": "a.ts", "line": 11, "severity": "major", "model": "m1"})];
        assert!(dedupe_against_prescan(&major, &pre2, n.layout()).is_empty());
        assert_eq!(dedupe_against_prescan(&crit, &pre2, n.layout()).len(), 1);
    }

    #[test]
    fn merge_helpers_and_format_log() {
        assert_eq!(merge_lintable_total(&[Some(0), Some(5)]), Some(5));
        assert_eq!(merge_lintable_total(&[None, Some(3)]), Some(3));
        assert_eq!(merge_lintable_total(&[Some(0), Some(0)]), None);
        assert_eq!(merge_prescan_skipped(&[Some("no-eslint"), None, Some("no-config")]).as_deref(), Some("no-eslint+no-config"));
        assert_eq!(merge_prescan_skipped(&[None, None]), None);

        let r = PrescanResult {
            findings: vec![json!({}), json!({})],
            scanned: 3,
            by_tool: Some(PrescanByTool { eslint: 1, astgrep: 1 }),
            ..PrescanResult::default()
        };
        assert_eq!(format_prescan_log(&r), "2 issue(s) on 3 file(s) [eslint:1, ast-grep:1]");
        let r2 = PrescanResult { truncated: true, lintable_total: Some(40), error: Some("x".repeat(100)), ..PrescanResult::default() };
        assert_eq!(format_prescan_log(&r2), format!("0 issue(s) on 0 file(s) (truncated from 40) (warn: {})", "x".repeat(80)));
    }

    #[test]
    fn prompt_section_lists_findings() {
        let f = vec![json!({"severity": "critical", "file": "a.ts", "line": 3, "title": "eslint: x", "body": "b"})];
        let s = build_prescan_prompt_section(&f);
        assert!(s.starts_with("- [critical] `a.ts:3` — eslint: x: b\n\nDo NOT repeat"));
        assert_eq!(build_prescan_prompt_section(&[]), "");
    }

    #[test]
    fn combine_tool_results_merges() {
        let e = ToolPrescanResult {
            findings: vec![json!({"title": "e"})],
            prompt_section: "E".into(),
            scanned: 0,
            skipped: Some("no-eslint".into()),
            error: Some("boom".into()),
            ..ToolPrescanResult::default()
        };
        let a = ToolPrescanResult { findings: vec![json!({"title": "a"})], prompt_section: "A".into(), scanned: 2, lintable_total: Some(4), ..ToolPrescanResult::default() };
        let r = combine_tool_results(e, a);
        assert_eq!(r.findings.len(), 2);
        assert_eq!(r.prompt_section, "E\n\nA");
        assert_eq!(r.scanned, 2);
        assert_eq!(r.lintable_total, Some(4));
        assert_eq!(r.skipped.as_deref(), Some("no-eslint"));
        assert_eq!(r.error.as_deref(), Some("boom"));
        assert_eq!(r.by_tool, Some(PrescanByTool { eslint: 1, astgrep: 1 }));
    }

    #[test]
    fn prescan_config_from_cfg() {
        let mut env = Map::new();
        env.insert("REVIEW_PRESCAN_ASTGREP_BLOCK_RULES".into(), json!(""));
        let parts = crate::config::load_review_config_parts(&env, &json!({}));
        let c = PrescanConfig::from_cfg(&parts);
        assert!(c.enabled);
        assert_eq!(c.max_files, 30.0);
        assert_eq!(c.eslint_bin, "npx");
        let ag = c.astgrep.unwrap();
        assert!(ag.enabled);
        assert_eq!(ag.block_rules, Some(vec![]));
        assert_eq!(ag.bin, "ast-grep");
        let mut env = Map::new();
        env.insert("REVIEW_PRESCAN".into(), json!("0"));
        let c = PrescanConfig::from_cfg(&crate::config::load_review_config_parts(&env, &json!({})));
        assert!(!c.enabled);
        assert!(!c.astgrep.unwrap().enabled);
    }

    #[test]
    fn run_command_captures_nonzero_exit_and_fails_open_on_missing_bin() {
        let out = run_command("sh", &["-c".into(), "echo out; echo err >&2; exit 3".into()], Path::new("."), 1000).unwrap();
        assert_eq!(out.code, 3);
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
        let e = run_command("definitely-missing-bin-ai-reviewer", &[], Path::new("."), 1000).unwrap_err();
        assert!(e.contains("ENOENT"), "{e}");
        let e = run_command("sh", &["-c".into(), "head -c 500 /dev/zero".into()], Path::new("."), 100).unwrap_err();
        assert_eq!(e, "prescan stdout/stderr exceeded 100 bytes");
        assert!(run_command("sh", &[], Path::new("/definitely/missing/dir"), 100).is_err());
    }

    #[test]
    fn eslint_fails_open_without_node_modules() {
        let tmp = tempfile::tempdir().unwrap();
        let p = orakul();
        let logs = Mutex::new(Vec::<String>::new());
        let log = |s: &str| logs.lock().unwrap().push(s.to_string());
        let r = run_eslint_prescan(&["server/a.ts".into()], &cfg_app("app"), p.layout(), tmp.path(), &log);
        assert_eq!(r.skipped.as_deref(), Some("no-eslint"));
        assert!(r.findings.is_empty());
        assert!(logs.lock().unwrap()[0].contains("eslint недоступен (нет node_modules в app)"));
        // Выключенный prescan — пусто без логов.
        let r = run_eslint_prescan(&["server/a.ts".into()], &PrescanConfig::default(), p.layout(), tmp.path(), &no_log);
        assert_eq!(r.skipped, None);
    }

    #[cfg(unix)]
    #[test]
    fn eslint_fails_open_on_missing_fallback_bin_and_bad_json() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(app.join("node_modules/eslint")).unwrap();
        std::fs::write(app.join("node_modules/eslint/package.json"), "{}").unwrap();
        let p = orakul();
        let mut cfg = cfg_app("app");
        cfg.eslint_bin = "definitely-missing-npx-ai-reviewer".into();
        let r = run_eslint_prescan(&["server/a.ts".into()], &cfg, p.layout(), tmp.path(), &no_log);
        assert!(r.findings.is_empty());
        assert_eq!(r.scanned, 0);
        assert!(r.error.as_deref().unwrap().contains("ENOENT"));

        write_script(&app.join("node_modules/.bin/eslint"), "echo 'not json'\nexit 2\n");
        let r = run_eslint_prescan(&["server/a.ts".into()], &cfg, p.layout(), tmp.path(), &no_log);
        assert_eq!(r.error.as_deref(), Some("bad json"));
        assert_eq!(r.scanned, 1);

        write_script(&app.join("node_modules/.bin/eslint"), "echo 'config error' >&2\nexit 2\n");
        let r = run_eslint_prescan(&["server/a.ts".into()], &cfg, p.layout(), tmp.path(), &no_log);
        assert_eq!(r.error.as_deref(), Some("config error"));
        assert!(r.findings.is_empty());
    }

    /// Фикстура `prescan-eslint-layer-block` (DoD T14): фейковый локальный eslint
    /// отдаёт JSON с абсолютным путём и кодом 1 (как настоящий при нарушениях) —
    /// весь путь `run_eslint_prescan` даёт ровно `findings.json` фикстуры, и гейт
    /// блокирует так, как ждёт `meta.json`.
    #[cfg(unix)]
    #[test]
    fn fixture_prescan_eslint_layer_block() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(app.join("node_modules/eslint")).unwrap();
        std::fs::write(app.join("node_modules/eslint/package.json"), "{}").unwrap();
        let abs = app.join("client/src/foo.ts");
        let payload = json!([{ "filePath": abs.to_string_lossy(), "messages": [
            { "ruleId": "no-restricted-imports", "severity": 2, "line": 1,
              "message": "Граница слоёв: client/ не импортирует серверный код" }] }]);
        // Аргументы обязаны прийти путями относительно appDir.
        write_script(
            &app.join("node_modules/.bin/eslint"),
            &format!(
                "[ \"$4\" = client/src/foo.ts ] || {{ echo \"bad args: $*\" >&2; exit 9; }}\ncat <<'EOF'\n{payload}\nEOF\nexit 1\n"
            ),
        );
        let p = orakul();
        let r = run_eslint_prescan(&["client/src/foo.ts".into()], &cfg_app("app"), p.layout(), tmp.path(), &no_log);
        assert_eq!(r.error, None, "{r:?}");
        assert_eq!(r.findings, fixture_findings("prescan-eslint-layer-block"));
        assert!(r.prompt_section.contains("`client/src/foo.ts:1` — eslint: no-restricted-imports"));
        assert_fixture_gate("prescan-eslint-layer-block", &r.findings, &p);
    }

    /// Путь из диффа PR с ведущим дефисом (`app/--config=evil.ts`) уходит в argv
    /// позиционным аргументом, а не опцией eslint (RFC 20 SEC).
    #[cfg(unix)]
    #[test]
    fn eslint_path_with_leading_dash_is_not_an_option() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(app.join("node_modules/eslint")).unwrap();
        std::fs::write(app.join("node_modules/eslint/package.json"), "{}").unwrap();
        write_script(
            &app.join("node_modules/.bin/eslint"),
            "[ \"$4\" = ./--config=evil.ts ] || { echo \"bad args: $*\" >&2; exit 9; }\necho '[]'\n",
        );
        let p = orakul();
        let r = run_eslint_prescan(&["app/--config=evil.ts".into()], &cfg_app("app"), p.layout(), tmp.path(), &no_log);
        assert_eq!(r.error, None, "{r:?}");
        assert_eq!(r.scanned, 1);
    }

    #[test]
    fn tool_path_arg_prefixes_only_leading_dash() {
        assert_eq!(tool_path_arg("--config=evil.ts".into()), "./--config=evil.ts");
        assert_eq!(tool_path_arg("-r.ts".into()), "./-r.ts");
        assert_eq!(tool_path_arg("server/a.ts".into()), "server/a.ts");
        assert_eq!(tool_path_arg("a-b.ts".into()), "a-b.ts");
    }

    #[test]
    fn real_prescan_never_errs_when_tools_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let p = orakul();
        let mut env = Map::new();
        env.insert("REVIEW_ASTGREP_BIN".into(), json!("definitely-missing-ast-grep-ai-reviewer"));
        env.insert("REVIEW_ASTGREP_CONFIG".into(), json!(""));
        let mut parts = crate::config::load_review_config_parts(&env, &p.config_input());
        parts["prescan"]["astgrep"]["configPath"] = json!("");
        let runner = RealPrescan::from_cfg(&parts, &p, tmp.path().to_path_buf(), no_log);
        let r = runner.run(&["server/auth.ts".into()]).expect("prescan fail-open: Ok даже без инструментов");
        assert!(r.findings.is_empty());
        assert_eq!(r.skipped.as_deref(), Some("no-eslint"));
        assert!(r.error.as_deref().unwrap_or("").contains("ENOENT"), "{r:?}");
        assert!(format_prescan_log(&r).starts_with("0 issue(s)"));
        let _ = prescan_result_to_value(&r);
    }
}
