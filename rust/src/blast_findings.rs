//! Порт `lib/blast-findings.mjs` (RFC 04, задача T15): детерминированные
//! находки по blast-radius — caller вне диффа, живущий в другом слое/сервисе
//! или в другой community AST-графа.
//!
//! Находки soft по построению: блокировать merge они могут только после
//! verifier (`source == BLAST_FINDING_SOURCE` — см. `clusterFindings`
//! / `crate::findings`), поэтому здесь нет ни гейта, ни LLM.
//!
//! Отличия от JS, осознанные:
//!
//! * `getProfile()` в Rust нет (решение 3 RFC 04): раскладка каталогов
//!   приходит явным `&Layout` в [`BlastFindingsOpts`] — в JS это
//!   необязательный `opts.layout`, который по умолчанию брался из
//!   process-wide профиля.
//! * `opts.dropAmbiguous !== false` (то есть «включено, пока явно не выключили»)
//!   выражено обычным `bool` со значением `true` в [`BlastFindingsOpts::new`].
//!
//! Паритет держим по:
//!
//! * форме находки — порядок ключей как в объектном литерале JS
//!   (`file, line, severity, confidence, title, body, model, prescan, source,
//!   blastCaller, blastCrossCommunity, blastEdgeConfidence`);
//! * regex-разбору блока: JS-классы `\s`/`\w`/`\d` воспроизведены явными
//!   ASCII/JS-наборами, потому что в Rust они unicode-широкие;
//! * сортировке: `confidence` по убыванию, дальше `localeCompare`
//!   (аппроксимация ICU root из `crate::structure_scip`), номер строки,
//!   метка confidence ребра; срез до [`BLAST_FINDINGS_MAX`].
//!
//! Известная граница аппроксимации: `(?i)` в Rust — полный Unicode case folding,
//! а JS-флаг `i` без `u` канонизирует проще (U+212A KELVIN не сворачивается в `k`).
//! На входе парсера — пути файлов и ASCII-метки, поэтому расхождение недостижимо.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::constants::{
    BLAST_CONF_AMBIGUOUS, BLAST_CONF_EXTRACTED, BLAST_CONF_INFERRED, BLAST_CONF_UNKNOWN,
    BLAST_FINDINGS_MAX, BLAST_FINDING_SOURCE, BLAST_PATH_EXTS,
};
use crate::profile::Layout;
use crate::structure_scip::locale_compare;

/// Содержимое JS-класса `\s` (regex без флага `u`): отличается от Unicode
/// White_Space наличием U+FEFF. Вставляется внутрь `[...]`/`[^...]`.
const JS_WS_SET: &str =
    r"\t\n\x0B\f\r \x{00A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// Одна зависимость из текстового blast-блока Graphify.
///
/// В JS это анонимный объект; порядок полей здесь совпадает с порядком его
/// ключей, чтобы [`BlastDependent::to_value`] давал тот же JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct BlastDependent {
    /// Имя вызывающего символа (`` `caller()` `` перед `@`), либо пусто.
    pub caller: String,
    /// Путь зависимого файла.
    pub file: String,
    /// Номер строки; `Number(m[3]) || 1`, поэтому всегда ≥ 1.
    pub line: f64,
    /// Тип ребра в квадратных скобках (`calls`, `uses`, …), либо пусто.
    pub relation: String,
    /// Метка confidence ребра в верхнем регистре (`EXTRACTED`/`INFERRED`/`AMBIGUOUS`), либо пусто.
    pub confidence: String,
    /// Ребро помечено `cross-community`.
    pub cross_community: bool,
    /// Символ из заголовка блока (`- \`symbol\``), либо пусто.
    pub symbol: String,
}

impl BlastDependent {
    /// JSON в форме JS-объекта (порядок ключей сохранён, `line` — целое число).
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("caller".into(), json!(self.caller));
        m.insert("file".into(), json!(self.file));
        m.insert("line".into(), num_json(self.line));
        m.insert("relation".into(), json!(self.relation));
        m.insert("confidence".into(), json!(self.confidence));
        m.insert("crossCommunity".into(), json!(self.cross_community));
        m.insert("symbol".into(), json!(self.symbol));
        Value::Object(m)
    }
}

/// Опции [`emit_blast_radius_findings`] — аналог `opts` в JS.
#[derive(Debug, Clone)]
pub struct BlastFindingsOpts<'a> {
    /// Символы плана; `symbols[0]` — запасной символ в теле находки.
    pub symbols: &'a [String],
    /// Метка модели находки (`opts.model || BLAST_FINDING_SOURCE`).
    pub model: Option<&'a str>,
    /// Выбрасывать ли рёбра с меткой `AMBIGUOUS` (в JS — `opts.dropAmbiguous !== false`).
    pub drop_ambiguous: bool,
    /// Раскладка каталогов репозитория (в JS — `opts.layout || getProfile().layout`).
    pub layout: &'a Layout,
}

impl<'a> BlastFindingsOpts<'a> {
    /// Дефолты JS: символов нет, модель — `blast-radius`, AMBIGUOUS выбрасываем.
    pub fn new(layout: &'a Layout) -> Self {
        Self { symbols: &[], model: None, drop_ambiguous: true, layout }
    }
}

/// JS Number→JSON: целые печатаются без `.0` (как `JSON.stringify` в V8).
fn num_json(x: f64) -> Value {
    if x == x.trunc() && x.abs() < 9.0e15 {
        json!(x as i64)
    } else {
        json!(x)
    }
}

/// `DEP_ENTRY_RE` из JS: запись зависимости в блоке.
///
/// ```text
/// `caller` @ file:42 [calls][EXTRACTED] cross-community
/// file:L42 [calls]
/// ```
fn dep_entry_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Расширения — алфавитные токены без regex-метасимволов, escapeRegex из JS
        // на них тождественен (см. комментарий у BLAST_PATH_EXTS).
        let ext_alt = BLAST_PATH_EXTS.join("|");
        let pattern = concat!(
            r"(?i)(?:`([^`]+)`[{ws}]*@[{ws}]*)?",
            r"([^{ws},`]+?\.(?:{ext})):(?:L)?([0-9]+)",
            r"(?:[{ws}]*\[([A-Za-z0-9_]+)\])?",
            r"(?:\[(EXTRACTED|INFERRED|AMBIGUOUS)\])?",
            r"(?:[{ws}]*(cross-community))?",
        )
        .replace("{ws}", JS_WS_SET)
        .replace("{ext}", &ext_alt);
        Regex::new(&pattern).expect("regex записи blast-зависимости компилируется")
    })
}

/// Заголовок блока: `- \`symbol\`` в любой строке (`/^- \`([^\`]+)\`/m`).
fn symbol_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^- `([^`]+)`").expect("regex символа компилируется"))
}

/// `s.replace(/^[`'"]+|[`'"]+$/g, '')` — снимаем обрамляющие кавычки/бэктики.
fn trim_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '`' || c == '\'' || c == '"').to_string()
}

/// `s.replace(/`+/g, '')`: убираем backticks (не заменяем на `'`), чтобы не
/// искажать идентификаторы в отчёте.
fn escape_md_ticks(s: &str) -> String {
    s.replace('`', "")
}

/// Снимает `layout.appDir` с пути: `app/server/x.ts` → `server/x.ts`.
fn strip_app_dir(file_path: &str, app_dir: &str) -> String {
    // JS: p.replace(/^\.\/+/, '') — только ведущее «./» с любым числом слэшей.
    let p = match file_path.strip_prefix('.') {
        Some(rest) if rest.starts_with('/') => rest.trim_start_matches('/').to_string(),
        _ => file_path.to_string(),
    };
    // JS: dir.replace(/^\.\/+|\/+$/g, '') — ведущее «./» и хвостовые слэши.
    let dir = match app_dir.strip_prefix('.') {
        Some(rest) if rest.starts_with('/') => rest.trim_start_matches('/'),
        _ => app_dir,
    };
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        return p;
    }
    let prefix = format!("{dir}/");
    match p.strip_prefix(&prefix) {
        Some(rest) => rest.to_string(),
        None => p,
    }
}

/// Слой/сервис пути: сначала совпадение с `layout.layerDirs` (после strip appDir),
/// иначе первый сегмент пути.
fn path_layer_prefix(file_path: &str, layout: &Layout) -> String {
    let p = strip_app_dir(file_path, &layout.app_dir);
    // JS: [...layers].sort((a, b) => b.length - a.length) — длина в UTF-16,
    // сортировка стабильная (TimSort в V8, sort_by в Rust).
    let mut layers: Vec<&str> = layout.layer_dirs.iter().map(String::as_str).collect();
    layers.sort_by_key(|d| std::cmp::Reverse(d.encode_utf16().count()));
    for d in layers {
        if p == d || p.starts_with(&format!("{d}/")) {
            return d.to_string();
        }
    }
    match p.find('/') {
        Some(i) => p[..i].to_string(),
        None => p,
    }
}

/// Лежит ли путь внутри одного из `layerDirs` (после strip appDir).
fn in_layer(raw: &str, layout: &Layout) -> bool {
    let p = strip_app_dir(raw, &layout.app_dir);
    layout.layer_dirs.iter().any(|d| p == *d || p.starts_with(&format!("{d}/")))
}

/// Разные ли слои у изменённого файла и зависимого пути.
fn is_different_layer(changed_file: &str, dependent_path: &str, layout: &Layout) -> bool {
    if dependent_path.is_empty() {
        return false;
    }
    let changed_layer = path_layer_prefix(changed_file, layout);
    let dep_layer = path_layer_prefix(dependent_path, layout);
    if !changed_layer.is_empty() && !dep_layer.is_empty() && changed_layer != dep_layer {
        return true;
    }
    if !layout.layer_dirs.is_empty() {
        // JS: changedFile проверяется на truthiness — пустой путь не «в слое».
        if in_layer(dependent_path, layout) && !changed_file.is_empty() && !in_layer(changed_file, layout) {
            return true;
        }
        if in_layer(changed_file, layout) && !in_layer(dependent_path, layout) {
            return true;
        }
    }
    false
}

/// Confidence неизвестного ребра ≤ INFERRED (не «обгоняет» явную пониженную метку).
fn finding_confidence(confidence: &str) -> f64 {
    match confidence.to_uppercase().as_str() {
        "EXTRACTED" => BLAST_CONF_EXTRACTED,
        "INFERRED" => BLAST_CONF_INFERRED,
        "AMBIGUOUS" => BLAST_CONF_AMBIGUOUS,
        _ => BLAST_CONF_UNKNOWN,
    }
}

/// Парсит зависимости из текстовых блоков Graphify blast-radius.
pub fn parse_blast_dependents(blocks: &[String]) -> Vec<BlastDependent> {
    let mut items = Vec::new();
    for block in blocks {
        let symbol = symbol_re()
            .captures(block)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        for m in dep_entry_re().captures_iter(block) {
            let group = |i: usize| m.get(i).map(|g| g.as_str()).unwrap_or("");
            // Number(m[3]) || 1: цифры всегда парсятся, но «0» falsy → 1.
            let line = group(3).parse::<f64>().unwrap_or(f64::NAN);
            let line = if line.is_finite() && line != 0.0 { line } else { 1.0 };
            items.push(BlastDependent {
                caller: group(1).to_string(),
                file: trim_quotes(group(2)),
                line,
                relation: group(4).to_string(),
                confidence: group(5).to_uppercase(),
                cross_community: m.get(6).is_some(),
                symbol: symbol.clone(),
            });
        }
    }
    items
}

/// Пути зависимостей из текстовых блоков blast-radius (`Set` в JS).
pub fn parse_blast_dependent_paths(blocks: &[String]) -> HashSet<String> {
    parse_blast_dependents(blocks).into_iter().map(|d| d.file).collect()
}

/// Промежуточная находка: держим поля сортировки рядом, чтобы не читать их из `Value`.
struct Emitted {
    file: String,
    line: f64,
    confidence: f64,
    edge_confidence: String,
    value: Value,
}

/// Эмитит major-находки для callers вне changed-files из другого слоя/community.
///
/// Блокировка merge — только после verifier (см. `clusterFindings`); сами по себе soft.
pub fn emit_blast_radius_findings(
    blast_blocks: &[String],
    changed_paths: &[String],
    opts: &BlastFindingsOpts<'_>,
) -> Vec<Value> {
    // JS: new Set(changedPaths) — дедуп с сохранением порядка первого появления.
    let mut changed_list: Vec<&str> = Vec::new();
    let mut changed: HashSet<&str> = HashSet::new();
    for p in changed_paths {
        if changed.insert(p.as_str()) {
            changed_list.push(p.as_str());
        }
    }
    if blast_blocks.is_empty() || changed.is_empty() {
        return Vec::new();
    }

    let dependents = parse_blast_dependents(blast_blocks);
    let outside: Vec<&BlastDependent> =
        dependents.iter().filter(|d| !changed.contains(d.file.as_str())).collect();
    if outside.is_empty() {
        return Vec::new();
    }

    // JS: opts.symbols?.[0] || 'changed-symbol' — пустая строка тоже falsy.
    let fallback_symbol = match opts.symbols.first() {
        Some(s) if !s.is_empty() => s.as_str(),
        _ => "changed-symbol",
    };
    let model = match opts.model {
        Some(m) if !m.is_empty() => m,
        _ => BLAST_FINDING_SOURCE,
    };

    let mut findings: Vec<Emitted> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();

    for dep in outside {
        if seen.contains(dep.file.as_str()) {
            continue;
        }
        if opts.drop_ambiguous && dep.confidence.to_uppercase() == "AMBIGUOUS" {
            continue;
        }

        let cross_layer =
            changed_list.iter().any(|cf| is_different_layer(cf, &dep.file, opts.layout));
        let cross_community = dep.cross_community;
        if !cross_layer && !cross_community {
            continue;
        }

        seen.insert(dep.file.as_str());
        let file = escape_md_ticks(&dep.file);
        let symbol = escape_md_ticks(if dep.symbol.is_empty() { fallback_symbol } else { &dep.symbol });
        let caller_raw = escape_md_ticks(&dep.caller);
        let caller = if caller_raw.is_empty() { String::new() } else { format!(" (`{caller_raw}`)") };
        let why = [
            if cross_community { Some("другая community в AST-графе") } else { None },
            if cross_layer { Some("другой слой/сервис относительно изменений") } else { None },
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ");
        let conf_tag = if dep.confidence.is_empty() {
            String::new()
        } else {
            format!(" [{}]", dep.confidence)
        };
        let body = format!(
            "Graphify blast-radius: зависимый файл `{file}`{caller} не входит в изменённые файлы PR \
             ({why}){conf_tag}. Проверьте совместимость контракта символа `{symbol}` с этим caller."
        );
        let confidence = finding_confidence(&dep.confidence);
        let line = if dep.line == 0.0 { 1.0 } else { dep.line };

        // Порядок ключей — как в объектном литерале JS.
        let mut m = Map::new();
        m.insert("file".into(), json!(dep.file));
        m.insert("line".into(), num_json(line));
        m.insert("severity".into(), json!("major"));
        m.insert("confidence".into(), json!(confidence));
        m.insert("title".into(), json!("Breaking change: caller вне диффа не обновлён (blast-radius)"));
        m.insert("body".into(), json!(body));
        m.insert("model".into(), json!(model));
        m.insert("prescan".into(), json!(false));
        m.insert("source".into(), json!(BLAST_FINDING_SOURCE));
        m.insert("blastCaller".into(), json!(true));
        m.insert("blastCrossCommunity".into(), json!(cross_community));
        m.insert("blastEdgeConfidence".into(), json!(dep.confidence));

        findings.push(Emitted {
            file: dep.file.clone(),
            line,
            confidence,
            edge_confidence: dep.confidence.clone(),
            value: Value::Object(m),
        });
    }

    // b.confidence - a.confidence || localeCompare(file) || line || localeCompare(edge).
    findings.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| locale_compare(&a.file, &b.file))
            .then_with(|| a.line.partial_cmp(&b.line).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| locale_compare(&a.edge_confidence, &b.edge_confidence))
    });
    findings.into_iter().take(BLAST_FINDINGS_MAX).map(|f| f.value).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;
    use std::path::Path;

    /// Layout из JSON-профиля: конструктор `Layout` приватный, а нам нужны
    /// ровно те же поля, что `setProfile({ layout })` в JS-тестах.
    fn layout_profile(layout: Value) -> Profile {
        Profile::normalize(
            &json!({ "layout": layout }),
            Path::new("/nowhere"),
            "",
            Path::new("/nowhere"),
        )
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn парсер_зависимостей_читает_caller_строку_метки_и_community() {
        // Эталон снят с node -e на lib/blast-findings.mjs.
        let blocks = strs(&["- `exportFn` → 2 зависимостей вне диффа: \
             `api()` @ apps/web/api.ts:10 [calls][EXTRACTED], packages/core/x.ts:3 [calls]"]);
        let deps = parse_blast_dependents(&blocks);
        assert_eq!(deps.len(), 2);
        assert_eq!(
            deps[0],
            BlastDependent {
                caller: "api()".into(),
                file: "apps/web/api.ts".into(),
                line: 10.0,
                relation: "calls".into(),
                confidence: "EXTRACTED".into(),
                cross_community: false,
                symbol: "exportFn".into(),
            }
        );
        assert_eq!(deps[1].caller, "");
        assert_eq!(deps[1].confidence, "");
        assert_eq!(deps[1].line, 3.0);
        assert_eq!(deps[0].to_value()["line"], json!(10), "целое без .0");
    }

    #[test]
    fn парсер_понимает_префикс_строки_и_метку_cross_community() {
        let blocks = strs(&[
            "- `sharedFn` → 1: `other()` @ app/other.ts:L5 [calls][EXTRACTED] cross-community",
        ]);
        let deps = parse_blast_dependents(&blocks);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].line, 5.0);
        assert!(deps[0].cross_community);
    }

    #[test]
    fn пути_зависимостей_уникальны() {
        let blocks = strs(&[
            "- `a` → x: apps/web/a.ts:1 [calls], apps/web/a.ts:2 [calls], apps/web/b.ts:3 [calls]",
        ]);
        let paths = parse_blast_dependent_paths(&blocks);
        assert_eq!(paths.len(), 2);
        assert!(paths.contains("apps/web/a.ts"));
        assert!(paths.contains("apps/web/b.ts"));
    }

    #[test]
    fn cross_layer_caller_вне_диффа() {
        // JS: test('emitBlastRadiusFindings: cross-layer caller outside diff').
        let p = layout_profile(json!({ "layerDirs": ["apps", "packages"] }));
        let blocks = strs(&["- `exportFn` → 2 зависимостей вне диффа: \
             `api()` @ apps/web/api.ts:10 [calls][EXTRACTED], packages/core/x.ts:3 [calls]"]);
        let symbols = strs(&["exportFn"]);
        let opts = BlastFindingsOpts { symbols: &symbols, ..BlastFindingsOpts::new(p.layout()) };
        let findings = emit_blast_radius_findings(&blocks, &strs(&["packages/core/changed.ts"]), &opts);

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0]["file"], json!("apps/web/api.ts"));
        assert_eq!(findings[0]["source"], json!("blast-radius"));
        assert_eq!(findings[0]["severity"], json!("major"));
        assert_eq!(findings[0]["line"], json!(10));
        assert_eq!(findings[0]["model"], json!("blast-radius"));
        assert_eq!(findings[0]["prescan"], json!(false));
        assert_eq!(findings[0]["blastCaller"], json!(true));
        // Байт-в-байт с прогоном JS.
        assert_eq!(
            findings[0]["body"],
            json!(
                "Graphify blast-radius: зависимый файл `apps/web/api.ts` (`api()`) не входит \
                 в изменённые файлы PR (другой слой/сервис относительно изменений) [EXTRACTED]. \
                 Проверьте совместимость контракта символа `exportFn` с этим caller."
            )
        );
        // Порядок ключей — как в JS-литерале (важно для JSON-артефакта).
        let keys: Vec<&str> = findings[0].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "file",
                "line",
                "severity",
                "confidence",
                "title",
                "body",
                "model",
                "prescan",
                "source",
                "blastCaller",
                "blastCrossCommunity",
                "blastEdgeConfidence",
            ]
        );
    }

    #[test]
    fn drop_ambiguous_выключен_оставляет_ambiguous() {
        // JS: test('emitBlastRadiusFindings: DROP_AMBIGUOUS=0 оставляет AMBIGUOUS').
        let p = layout_profile(json!({ "layerDirs": ["apps", "packages"] }));
        let blocks =
            strs(&["- `exportFn` → 1 зависимостей вне диффа: `maybe()` @ apps/web/x.ts:1 [uses][AMBIGUOUS]"]);
        let symbols = strs(&["exportFn"]);
        let changed = strs(&["packages/core/a.ts"]);

        let kept = emit_blast_radius_findings(
            &blocks,
            &changed,
            &BlastFindingsOpts { symbols: &symbols, drop_ambiguous: false, ..BlastFindingsOpts::new(p.layout()) },
        );
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0]["blastEdgeConfidence"], json!("AMBIGUOUS"));
        assert_eq!(kept[0]["confidence"], json!(BLAST_CONF_AMBIGUOUS));

        let dropped = emit_blast_radius_findings(
            &blocks,
            &changed,
            &BlastFindingsOpts { symbols: &symbols, ..BlastFindingsOpts::new(p.layout()) },
        );
        assert!(dropped.is_empty(), "по умолчанию AMBIGUOUS выбрасывается");
    }

    #[test]
    fn app_dir_снимается_до_сравнения_слоёв() {
        // JS: test('emitBlastRadiusFindings: Orakul appDir strips before layer compare').
        let p = layout_profile(json!({ "appDir": "app/", "layerDirs": ["server", "shared", "client"] }));
        assert_eq!(p.layout().app_dir, "app", "профиль срезал хвостовой слэш");
        let blocks = strs(&["- `makeDataStore` → 2 зависимостей вне диффа: \
             `render()` @ app/client/ui.ts:L42 [calls][EXTRACTED], app/server/other.ts:3 [calls]"]);
        let symbols = strs(&["makeDataStore"]);
        let findings = emit_blast_radius_findings(
            &blocks,
            &strs(&["app/server/store.ts"]),
            &BlastFindingsOpts { symbols: &symbols, ..BlastFindingsOpts::new(p.layout()) },
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0]["file"], json!("app/client/ui.ts"));
        assert_eq!(findings[0]["line"], json!(42));
        let body = findings[0]["body"].as_str().unwrap();
        assert!(body.contains("`makeDataStore`"));
        assert!(body.contains("`render()`"));
    }

    #[test]
    fn cross_community_без_смены_слоя() {
        // JS: test('emitBlastRadiusFindings: cross-community без смены layerDirs').
        let p = layout_profile(json!({ "layerDirs": ["app"] }));
        let blocks = strs(&[
            "- `sharedFn` → 1 зависимостей вне диффа: `other()` @ app/other.ts:5 [calls][EXTRACTED] cross-community",
        ]);
        let symbols = strs(&["sharedFn"]);
        let findings = emit_blast_radius_findings(
            &blocks,
            &strs(&["app/changed.ts"]),
            &BlastFindingsOpts { symbols: &symbols, ..BlastFindingsOpts::new(p.layout()) },
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0]["file"], json!("app/other.ts"));
        assert_eq!(findings[0]["blastCrossCommunity"], json!(true));
        assert_eq!(findings[0]["confidence"], json!(0.9));
        let body = findings[0]["body"].as_str().unwrap();
        assert!(body.contains("другая community в AST-графе"), "{body}");
        assert!(
            !body.contains("другой слой"),
            "слой тот же — в why только community: {body}"
        );
    }

    #[test]
    fn ambiguous_пропускается_по_умолчанию() {
        // JS: test('emitBlastRadiusFindings: AMBIGUOUS пропускается').
        let p = layout_profile(json!({ "layerDirs": ["apps", "packages"] }));
        let blocks =
            strs(&["- `exportFn` → 1 зависимостей вне диффа: `maybe()` @ apps/web/x.ts:1 [uses][AMBIGUOUS]"]);
        let symbols = strs(&["exportFn"]);
        let findings = emit_blast_radius_findings(
            &blocks,
            &strs(&["packages/core/a.ts"]),
            &BlastFindingsOpts { symbols: &symbols, ..BlastFindingsOpts::new(p.layout()) },
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn сортировка_по_confidence_и_символ_из_многострочного_заголовка() {
        // JS: test('emitBlastRadiusFindings: sort by confidence then cap; symbol after preamble').
        let p = layout_profile(json!({ "layerDirs": ["apps", "packages"] }));
        let preamble = "Note:\n\n- `exportFn` → 3 зависимостей вне диффа: \
             `low()` @ apps/web/a.ts:1 [calls][INFERRED], \
             `hi()` @ apps/web/b.ts:2 [calls][EXTRACTED], \
             `mid()` @ apps/web/c.ts:3 [calls]";
        let symbols = strs(&["exportFn"]);
        let findings = emit_blast_radius_findings(
            &[preamble.to_string()],
            &strs(&["packages/core/x.ts"]),
            &BlastFindingsOpts { symbols: &symbols, ..BlastFindingsOpts::new(p.layout()) },
        );
        assert_eq!(findings.len(), 3);
        assert_eq!(findings[0]["file"], json!("apps/web/b.ts"));
        assert_eq!(findings[0]["confidence"], json!(0.9));
        assert_eq!(findings[1]["file"], json!("apps/web/a.ts"));
        assert_eq!(findings[1]["confidence"], json!(0.75));
        assert_eq!(findings[2]["confidence"], json!(0.7), "unknown ≤ INFERRED");
        assert!(
            findings.iter().all(|f| f["body"].as_str().unwrap().contains("`exportFn`")),
            "символ берётся из заголовка после преамбулы"
        );
    }

    #[test]
    fn лимит_находок_и_дедуп_по_файлу() {
        let p = layout_profile(json!({ "layerDirs": ["apps", "packages"] }));
        // 10 разных файлов + повтор первого: дедуп по файлу и срез до BLAST_FINDINGS_MAX.
        let mut block = String::from("- `exportFn` → много: ");
        for i in 0..10 {
            block.push_str(&format!("`c{i}()` @ apps/web/f{i}.ts:{}, ", i + 1));
        }
        block.push_str("`again()` @ apps/web/f0.ts:99");
        let symbols = strs(&["exportFn"]);
        let findings = emit_blast_radius_findings(
            &[block],
            &strs(&["packages/core/x.ts"]),
            &BlastFindingsOpts { symbols: &symbols, ..BlastFindingsOpts::new(p.layout()) },
        );
        assert_eq!(findings.len(), BLAST_FINDINGS_MAX);
        assert_eq!(findings[0]["file"], json!("apps/web/f0.ts"));
        assert_eq!(findings[0]["line"], json!(1), "повтор файла не перебивает первую запись");
    }

    #[test]
    fn пустой_вход_даёт_пустой_результат() {
        let p = layout_profile(json!({ "layerDirs": ["apps"] }));
        let opts = BlastFindingsOpts::new(p.layout());
        assert!(emit_blast_radius_findings(&[], &strs(&["a.ts"]), &opts).is_empty());
        assert!(
            emit_blast_radius_findings(&strs(&["- `s` → 1: apps/web/a.ts:1 [calls]"]), &[], &opts).is_empty(),
            "без изменённых файлов находок нет"
        );
        // Зависимость внутри диффа — не находка.
        assert!(emit_blast_radius_findings(
            &strs(&["- `s` → 1: apps/web/a.ts:1 [calls]"]),
            &strs(&["apps/web/a.ts"]),
            &opts,
        )
        .is_empty());
    }

    #[test]
    fn запасной_символ_когда_заголовка_нет() {
        let p = layout_profile(json!({ "layerDirs": ["apps", "packages"] }));
        let blocks = strs(&["зависимости: `api()` @ apps/web/api.ts:10 [calls]"]);
        let opts = BlastFindingsOpts::new(p.layout());
        let findings = emit_blast_radius_findings(&blocks, &strs(&["packages/core/x.ts"]), &opts);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]["body"].as_str().unwrap().contains("`changed-symbol`"),
            "без symbols и заголовка — 'changed-symbol'"
        );
    }

    #[test]
    fn модель_из_opts_перебивает_метку_источника() {
        let p = layout_profile(json!({ "layerDirs": ["apps", "packages"] }));
        let blocks = strs(&["- `s` → 1: `api()` @ apps/web/api.ts:10 [calls]"]);
        let findings = emit_blast_radius_findings(
            &blocks,
            &strs(&["packages/core/x.ts"]),
            &BlastFindingsOpts { model: Some("graphify"), ..BlastFindingsOpts::new(p.layout()) },
        );
        assert_eq!(findings[0]["model"], json!("graphify"));
        assert_eq!(findings[0]["source"], json!("blast-radius"), "source не перебивается");
    }
}
