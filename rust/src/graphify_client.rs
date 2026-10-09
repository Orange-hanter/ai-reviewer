//! Порт `lib/graphify.mjs` (задача T13 RFC 16): индекс графа кода и blast-radius.
//!
//! Модуль делится надвое:
//!
//! * **чистые функции** — [`bare_label`], [`build_graph_index`], [`resolve_unique_node`],
//!   [`enrich_affected_dep`], [`format_dep_entry`], [`format_blast_block`],
//!   [`dedupe_deps_by_file`]. Они портированы 1:1, потому что их вывод попадает
//!   в промпт модели и в golden-фикстуры: расхождение в один байт — это другой
//!   промпт и другой ответ LLM;
//! * **клиент** — [`RealGraphify`], реализация [`GraphifyApi`] (порт
//!   `createGraphifyClient`, `lib/graphify.mjs:299-401`).
//!
//! **Fail-open — контракт, а не удобство.** Ни одна ветка `build_blast_radius`
//! не возвращает `Err`: нет графа, граф устарел, `graphify` не установлен, вышел
//! таймаут — всё это пустые блоки плюс причина в `metrics.skip_reasons`
//! (`lib/graphify.mjs:341-347`, `lib/graphify.mjs:336-338`). Blast-radius — это
//! обогащение контекста, а не источник вердикта; его отказ не имеет права
//! останавливать ревью.
//!
//! **Исполнитель процессов инжектируется** ([`RealGraphify::with_exec`]) — так же,
//! как JS-тесты подменяли `deps.execFileP` (`test/pr-review.test.mjs:119`). Файлы
//! читаются реальной ФС: в JS `readFile`/`access` тоже подменялись, но в Rust
//! тесты пишут `tempfile::TempDir`, и путь чтения графа проверяется настоящий.
//!
//! Классификация ошибок процесса (`queryErrorReason`, `lib/graphify.mjs:22-26`):
//!
//! | JS | Rust | причина skip |
//! |---|---|---|
//! | `error.code === 'ENOENT'` | [`ExecError::NotFound`] | `graphify_unavailable` |
//! | `error.code === 'ETIMEDOUT'` \|\| `error.killed` | [`ExecError::Killed`] | `graphify_timeout` |
//! | остальное (ненулевой код возврата, I/O) | [`ExecError::Failed`] | `graphify_error` |
//!
//! Превышение `maxBuffer` Node тоже помечает `killed: true`, поэтому в JS оно
//! попадает в `graphify_timeout`; [`default_exec`] воспроизводит это, возвращая
//! [`ExecError::Killed`] и на переполнение буфера.
//!
//! Известные расхождения с JS, все — на входах, которых `loadReviewConfig` не
//! порождает:
//!
//! * `depth` в [`GraphifyConfig`] — `u32`: дробная глубина усечётся, тогда как
//!   JS отдал бы `--depth 2.5`. Конфиг движка всегда даёт целое из `[1;5]`
//!   (`lib/config.mjs:357`);
//! * `\s` в [`affected_line_re`] и в regex свежести — Unicode-класс crate `regex`:
//!   он включает U+0085 (NEL) и не включает U+FEFF, JS — наоборот. Обе строки
//!   генерирует сам `graphify`, там только `\r`/`\n`/пробел;
//! * строгое сравнение community (`seedCommunity !== community`) для массивов и
//!   объектов в JS сравнивает ссылки и всегда даёт «не равны»; здесь тот же
//!   результат получается явной веткой [`js_strict_ne`], а не сравнением значений.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value;

use crate::concurrency::run_limited;
use crate::structure_evidence::js_number_to_string;
use crate::traits::{BlastConfidence, BlastMetrics, BlastRadius, GraphifyApi};

// ---------------------------------------------------------------------------
// Константы прогона (`lib/graphify.mjs:272`, `lib/graphify.mjs:312-313`)
// ---------------------------------------------------------------------------

/// Бюджет одного `graphify affected` (`lib/graphify.mjs:313`).
pub const AFFECTED_TIMEOUT_MS: u64 = 20_000;

/// `maxBuffer` одного `graphify affected` — 4 МиБ (`lib/graphify.mjs:313`).
pub const AFFECTED_MAX_BUFFER: usize = 4 * 1024 * 1024;

/// Бюджет `git rev-parse HEAD` для проверки свежести (`lib/graphify.mjs:272`).
pub const GIT_TIMEOUT_MS: u64 = 5_000;

/// `maxBuffer` `git rev-parse HEAD` — 1 КиБ (`lib/graphify.mjs:272`).
pub const GIT_MAX_BUFFER: usize = 1024;

/// Ранг confidence ребра (`CONF_RANK`, `lib/graphify.mjs:16`); всё остальное — 0.
fn conf_rank(c: &str) -> u8 {
    match c.to_uppercase().as_str() {
        "EXTRACTED" => 3,
        "INFERRED" => 2,
        "AMBIGUOUS" => 1,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// JS-семантика значений
// ---------------------------------------------------------------------------

/// JS `\s` (`String#trim`): Unicode WhiteSpace ∪ LineTerminator ∪ U+FEFF, без U+0085.
fn is_js_ws(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{0B}'
            | '\u{0C}'
            | '\r'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_ws)
}

/// JS-truthiness JSON-значения: `null`/`false`/`0`/`NaN`/`""` — ложь.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0 && !x.is_nan()).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS `String(v)` для JSON-значения: массив — `join(',')` с `null` → `''`,
/// объект — `[object Object]`, число — по ECMA `Number::toString`.
fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.as_f64().map(js_number_to_string).unwrap_or_else(|| n.to_string()),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// JS `String(v || '')`: falsy (в т.ч. отсутствующее поле) → пустая строка.
fn js_string_truthy(v: Option<&Value>) -> String {
    match v {
        Some(x) if is_truthy(x) => js_string(x),
        _ => String::new(),
    }
}

/// JS `String(v ?? '')`: только `null`/`undefined` → пустая строка.
fn js_string_nullish(v: Option<&Value>) -> String {
    match v {
        Some(Value::Null) | None => String::new(),
        Some(x) => js_string(x),
    }
}

/// JS `a !== b` для значений графа. Числа сравниваются по величине (`1` и `1.0` —
/// одно число), массивы и объекты в JS сравниваются по ссылке и у разных узлов
/// графа всегда различны.
fn js_strict_ne(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x != y,
            _ => x.to_string() != y.to_string(),
        },
        (Value::String(x), Value::String(y)) => x != y,
        (Value::Bool(x), Value::Bool(y)) => x != y,
        (Value::Null, Value::Null) => false,
        (Value::Array(_) | Value::Object(_), _) | (_, Value::Array(_) | Value::Object(_)) => true,
        _ => true,
    }
}

/// Длина строки в единицах UTF-16 — `String#length` из JS (`lib/graphify.mjs:249`).
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

// ---------------------------------------------------------------------------
// Чистые функции разбора и рендера
// ---------------------------------------------------------------------------

/// Строка stdout `graphify affected`: `- label [relation] file:L42`
/// (`AFFECTED_LINE_RE`, `lib/graphify.mjs:14`).
///
/// `[\w_]` заменён на явный ASCII-класс: `\w` в crate `regex` Unicode-aware,
/// а в JS это ровно `[A-Za-z0-9_]`.
pub fn affected_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^- (.+?) \[([0-9A-Za-z_]+)\] (.+?):L?([0-9]+)\s*$")
            .expect("AFFECTED_LINE_RE компилируется")
    })
}

/// `Built from commit: <sha>` из GRAPH_REPORT.md (`lib/graphify.mjs:274`).
fn built_commit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)Built from commit:\s*`?([0-9a-f]{7,40})`?")
            .expect("built_commit_re компилируется")
    })
}

/// Снимает все хвостовые `()` (graphify иногда отдаёт `foo()()`),
/// `lib/graphify.mjs:49-53`.
pub fn bare_label(label: &str) -> String {
    let mut s = js_trim(label);
    while let Some(rest) = s.strip_suffix("()") {
        s = rest;
    }
    s.to_string()
}

/// `bareLabel(v)` для значения, которого может не быть: `String(undefined || '')`.
fn bare_label_of(v: Option<&Value>) -> String {
    bare_label(&js_string_truthy(v))
}

/// Экранирование фрагмента для markdown-тела блока (`lib/graphify.mjs:65-72`).
///
/// Подменяются на визуально похожие символы, а не вырезаются: блок читает и
/// человек в sticky, и `parseBlastDependents` при обратном разборе.
fn escape_md_fragment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '`' => out.push('\''),
            '[' => out.push('('),
            ']' => out.push(')'),
            '*' => out.push('\u{2217}'),
            '_' => out.push('\u{02CD}'),
            other => out.push(other),
        }
    }
    out
}

/// Одна зависимость вне диффа — строка `affected` плюс обогащение из graph.json.
///
/// `line` хранится как `f64`, потому что в JS это `Number(m[4])`, и рендер идёт
/// через `String(number)`: так `- x [uses] a.ts:L007` даёт `a.ts:7` в обоих
/// портах без отдельной ветки на переполнение.
///
/// Поля `Option<Value>` различают «не ставили» (`None`, у JS — `undefined`) и
/// «поставили null» (`Some(Value::Null)`): `enrichAffectedDep` пишет туда
/// `seed.community ?? null`, и разница видна в JS-тестах
/// (`test/pr-review.test.mjs:296-299`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AffectedDep {
    /// Метка вызывающего узла (`m[1]`).
    pub label: String,
    /// Тип ребра (`m[2]`): `calls`, `uses`, …
    pub relation: String,
    /// Файл вызывающего (`m[3]`), ключ дедупа.
    pub file: String,
    /// Строка вызова (`Number(m[4])`).
    pub line: f64,
    /// `EXTRACTED` / `INFERRED` / `AMBIGUOUS`; `None` — ребро к seed не нашлось.
    pub confidence: Option<String>,
    /// `confidence_score` ребра, если graphify его отдал.
    pub confidence_score: Option<f64>,
    /// Сообщество вызывающего узла.
    pub community: Option<Value>,
    /// Имя сообщества вызывающего (`community_name || community_label || null`).
    pub community_name: Option<Value>,
    /// Сообщество seed-узла.
    pub seed_community: Option<Value>,
    /// Имя сообщества seed (`community_name || community_label || null`).
    pub seed_community_name: Option<Value>,
    /// Вызывающий из другого сообщества, чем seed. В JS поле ставится только в
    /// `true` и иначе отсутствует — `false` здесь равносилен `undefined`.
    pub cross_community: bool,
    /// Seed помечен god-node в `.graphify_analysis.json`. Как и в JS, флаг живёт
    /// на dep только до сборки блока и снимается перед рендером
    /// (`lib/graphify.mjs:334`).
    pub seed_is_god_node: bool,
}

impl AffectedDep {
    /// Сырая запись строки `affected` до обогащения (`lib/graphify.mjs:325`).
    pub fn raw(label: &str, relation: &str, file: &str, line: f64) -> Self {
        AffectedDep {
            label: label.to_string(),
            relation: relation.to_string(),
            file: file.to_string(),
            line,
            ..Default::default()
        }
    }
}

/// Пороги рендера блока — поля `cfg.limits`, которые читает graphify-контур
/// (`lib/config.mjs:338-347`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlastLimits {
    /// `limits.concurrency` — сколько символов опрашиваем параллельно.
    pub concurrency: usize,
    /// `limits.godNodeThreshold` — с какого числа зависимостей блок помечается god-node.
    pub god_node_threshold: usize,
    /// `limits.maxDepsPerSymbol` — сколько зависимостей печатаем до `… (+N)`.
    pub max_deps_per_symbol: usize,
}

impl Default for BlastLimits {
    /// Дефолты движка (`lib/config.mjs:340-346`), а не нули: собранный вручную
    /// клиент обязан вести себя как боевой.
    fn default() -> Self {
        BlastLimits { concurrency: 10, god_node_threshold: 20, max_deps_per_symbol: 20 }
    }
}

/// Индекс `graph.json` (`buildGraphIndex`, `lib/graphify.mjs:80-118`).
///
/// Узлы и рёбра лежат плоскими векторами, а карты держат индексы: так
/// `byId.values()` сохраняет JS-семантику `Map` (позиция — по первой вставке
/// ключа, значение — по последней), и при этом одноимённые узлы не теряются
/// в label-картах.
#[derive(Debug, Clone, Default)]
pub struct GraphIndex {
    nodes: Vec<Value>,
    edges: Vec<Value>,
    /// `String(node.id)` → индекс последнего узла с этим id.
    by_id: IndexMap<String, usize>,
    /// casefold точной метки (вместе с `()`) → узлы.
    by_exact_label: IndexMap<String, Vec<usize>>,
    /// casefold метки без `()` — как graphify `resolve_seed`.
    by_bare_label: IndexMap<String, Vec<usize>>,
    /// `String(edge.target)` → входящие рёбра.
    inbound: IndexMap<String, Vec<usize>>,
    /// casefold меток god-узлов из `.graphify_analysis.json`.
    gods: HashSet<String>,
}

impl GraphIndex {
    /// Узел по `String(id)`; `None` — такого id в графе нет.
    pub fn node_by_id(&self, id: &str) -> Option<&Value> {
        self.by_id.get(id).map(|&i| &self.nodes[i])
    }

    /// Сколько узлов попало в индекс (узлы без `id` отбрасываются).
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Сколько рёбер попало в индекс входящих (рёбра без `target` отбрасываются).
    pub fn inbound_count(&self, target: &str) -> usize {
        self.inbound.get(target).map(Vec::len).unwrap_or(0)
    }

    /// Помечена ли метка god-node в `.graphify_analysis.json`.
    pub fn is_god(&self, label: &str) -> bool {
        self.gods.contains(&label.to_lowercase())
    }

    fn exact(&self, key: &str) -> &[usize] {
        self.by_exact_label.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    fn bare(&self, key: &str) -> &[usize] {
        self.by_bare_label.get(key).map(Vec::as_slice).unwrap_or(&[])
    }
}

fn push_label(map: &mut IndexMap<String, Vec<usize>>, key: String, node: usize) {
    if key.is_empty() {
        return;
    }
    map.entry(key).or_default().push(node);
}

/// Строит индекс `graph.json` (`lib/graphify.mjs:80-118`).
///
/// `raw` — node-link JSON (`nodes` + `links`|`edges`); `analysis` — опциональный
/// `.graphify_analysis.json`, из него читается только поле `gods`.
pub fn build_graph_index(raw: &Value, analysis: Option<&Value>) -> GraphIndex {
    let mut index = GraphIndex::default();

    let empty: Vec<Value> = Vec::new();
    let nodes = raw.get("nodes").and_then(Value::as_array).unwrap_or(&empty);
    let links = match raw.get("links").and_then(Value::as_array) {
        Some(l) => l,
        None => raw.get("edges").and_then(Value::as_array).unwrap_or(&empty),
    };

    for n in nodes {
        // `!n || n.id == null` — узлы без id не попадают ни в одну карту.
        if n.is_null() {
            continue;
        }
        match n.get("id") {
            None | Some(Value::Null) => continue,
            Some(_) => {}
        }
        let id = js_string(n.get("id").expect("id проверен выше"));
        let label = js_string_truthy(n.get("label"));
        let at = index.nodes.len();
        index.nodes.push(n.clone());
        index.by_id.insert(id, at);
        if !label.is_empty() {
            push_label(&mut index.by_exact_label, label.to_lowercase(), at);
            push_label(&mut index.by_bare_label, bare_label(&label).to_lowercase(), at);
        }
    }

    for e in links {
        if e.is_null() {
            continue;
        }
        let target = js_string_nullish(e.get("target"));
        if target.is_empty() {
            continue;
        }
        let at = index.edges.len();
        index.edges.push(e.clone());
        index.inbound.entry(target).or_default().push(at);
    }

    if let Some(gods) = analysis.and_then(|a| a.get("gods")).and_then(Value::as_array) {
        for g in gods {
            let label = match g {
                Value::String(s) => Some(Value::String(s.clone())),
                other => other.get("label").cloned(),
            };
            // `if (label)` — falsy-метки в gods игнорируются.
            if let Some(l) = label.filter(is_truthy) {
                index.gods.insert(js_string(&l).to_lowercase());
            }
        }
    }

    index
}

/// Уникальный узел по query — зеркало graphify `resolve_seed`
/// (`lib/graphify.mjs:124-136`).
///
/// Омонимы дают `None`: лучше без обогащения, чем чужой seed.
pub fn resolve_unique_node<'a>(index: &'a GraphIndex, query: &str) -> Option<&'a Value> {
    // JS: `if (!index || !query) return null` — пустой query отсекается сразу.
    if query.is_empty() {
        return None;
    }
    if let Some(node) = index.node_by_id(query) {
        return Some(node);
    }

    let exact = index.exact(&query.to_lowercase());
    if exact.len() == 1 {
        return Some(&index.nodes[exact[0]]);
    }
    if exact.len() > 1 {
        return None;
    }

    let bare = index.bare(&bare_label(query).to_lowercase());
    if bare.len() == 1 {
        return Some(&index.nodes[bare[0]]);
    }
    None
}

/// Caller: уникальная метка, иначе метка + файл, иначе единственный узел файла
/// (`resolveCallerNode`, `lib/graphify.mjs:139-159`).
fn resolve_caller_node<'a>(index: &'a GraphIndex, dep: &AffectedDep) -> Option<&'a Value> {
    let file = dep.file.clone();
    let has_label = !dep.label.is_empty();

    let pick_in_file = |candidates: &[usize]| -> Option<usize> {
        if file.is_empty() {
            return None;
        }
        let in_file: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| js_string_truthy(index.nodes[i].get("source_file")) == file)
            .collect();
        if in_file.len() == 1 {
            Some(in_file[0])
        } else {
            None
        }
    };

    let exact = if has_label { index.exact(&dep.label.to_lowercase()) } else { &[][..] };
    if exact.len() == 1 {
        return Some(&index.nodes[exact[0]]);
    }
    if exact.len() > 1 {
        if let Some(i) = pick_in_file(exact) {
            return Some(&index.nodes[i]);
        }
    }

    let bare = if has_label { index.bare(&bare_label(&dep.label).to_lowercase()) } else { &[][..] };
    if bare.len() == 1 {
        return Some(&index.nodes[bare[0]]);
    }
    if bare.len() > 1 {
        if let Some(i) = pick_in_file(bare) {
            return Some(&index.nodes[i]);
        }
    }

    if file.is_empty() {
        return None;
    }
    let by_file: Vec<usize> = index
        .by_id
        .values()
        .copied()
        .filter(|&i| js_string_truthy(index.nodes[i].get("source_file")) == file)
        .collect();
    if by_file.len() == 1 {
        Some(&index.nodes[by_file[0]])
    } else {
        None
    }
}

/// Обогащает dep из `affected` confidence/community из graph.json
/// (`lib/graphify.mjs:166-211`).
///
/// Входящее ребро к seed сопоставляется по `relation` + (`source_file` | `label`).
/// Без уникального seed обогащения не происходит вовсе: омоним подмешал бы
/// чужие цифры, а это хуже, чем их отсутствие.
pub fn enrich_affected_dep(
    dep: &AffectedDep,
    seed_query: &str,
    index: Option<&GraphIndex>,
) -> AffectedDep {
    let mut out = dep.clone();
    let index = match index {
        Some(i) => i,
        None => return out,
    };
    let seed = match resolve_unique_node(index, seed_query) {
        Some(s) => s.clone(),
        None => return out,
    };

    out.seed_community = Some(match seed.get("community") {
        Some(v) if !v.is_null() => v.clone(),
        _ => Value::Null,
    });
    out.seed_community_name = Some(first_truthy(&seed, "community_name", "community_label"));

    if let Some(caller) = resolve_caller_node(index, dep).cloned() {
        out.community = Some(match caller.get("community") {
            Some(v) if !v.is_null() => v.clone(),
            _ => Value::Null,
        });
        out.community_name = Some(first_truthy(&caller, "community_name", "community_label"));
        let seed_c = out.seed_community.as_ref().filter(|v| !v.is_null());
        let call_c = out.community.as_ref().filter(|v| !v.is_null());
        if let (Some(a), Some(b)) = (seed_c, call_c) {
            if js_strict_ne(a, b) {
                out.cross_community = true;
            }
        }
    }

    let seed_id = js_string_truthy(seed.get("id"));
    let edges = index.inbound.get(&seed_id).cloned().unwrap_or_default();
    let file_norm = dep.file.clone();
    let label_norm = bare_label(&dep.label).to_lowercase();
    let mut best: Option<&Value> = None;
    for &ei in &edges {
        let e = &index.edges[ei];
        // `if (dep.relation && …)` — пустой relation не фильтрует.
        if !dep.relation.is_empty() && js_string_truthy(e.get("relation")) != dep.relation {
            continue;
        }
        let src = index.node_by_id(&js_string_nullish(e.get("source")));
        let src_file = match js_string_truthy(e.get("source_file")) {
            s if !s.is_empty() => s,
            _ => src.map(|n| js_string_truthy(n.get("source_file"))).unwrap_or_default(),
        };
        let src_label = bare_label_of(src.and_then(|n| n.get("label"))).to_lowercase();
        let file_match = src_file == file_norm;
        let label_match = !src_label.is_empty() && src_label == label_norm;
        if !file_match && !label_match {
            continue;
        }
        let better = match best {
            None => true,
            Some(b) => {
                conf_rank(&js_string_truthy(e.get("confidence")))
                    > conf_rank(&js_string_truthy(b.get("confidence")))
            }
        };
        if better {
            best = Some(e);
        }
    }
    if let Some(b) = best {
        let conf = match js_string_truthy(b.get("confidence")) {
            s if s.is_empty() => "EXTRACTED".to_string(),
            s => s,
        };
        out.confidence = Some(conf.to_uppercase());
        match b.get("confidence_score") {
            Some(v) if !v.is_null() => {
                out.confidence_score = Some(js_number(v));
            }
            _ => {}
        }
    }

    // god-nodes: только по exact-метке уникально разрешённого seed (не по bare —
    // иначе `create()` помечался бы от омонима `create` в gods).
    if index.is_god(&js_string_truthy(seed.get("label"))) {
        out.seed_is_god_node = true;
    }
    out
}

/// JS `a || b || null` для двух полей узла.
fn first_truthy(node: &Value, a: &str, b: &str) -> Value {
    for key in [a, b] {
        if let Some(v) = node.get(key) {
            if is_truthy(v) {
                return v.clone();
            }
        }
    }
    Value::Null
}

/// JS `Number(v)` для значения графа: не-числа дают `NaN`.
fn js_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => {
            let t = js_trim(s);
            if t.is_empty() {
                0.0
            } else {
                t.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        _ => f64::NAN,
    }
}

/// Одна зависимость в тексте blast-блока (`formatDepEntry`, `lib/graphify.mjs:214-221`).
///
/// Формат совместим с обратным разбором `parseBlastDependents`, поэтому любые
/// правки здесь ломают harvest blast-находок.
pub fn format_dep_entry(d: &AffectedDep) -> String {
    let caller = if d.label.is_empty() {
        String::new()
    } else {
        format!("`{}` @ ", escape_md_fragment(&d.label))
    };
    let conf = match &d.confidence {
        Some(c) if !c.is_empty() => format!("[{}]", escape_md_fragment(c)),
        _ => String::new(),
    };
    let cross = if d.cross_community { " cross-community" } else { "" };
    let file = escape_md_fragment(&d.file);
    let relation = escape_md_fragment(&d.relation);
    format!(
        "{caller}{file}:{} [{relation}]{conf}{cross}",
        js_number_to_string(d.line)
    )
}

/// Блок blast-radius одного символа (`formatBlastBlock`, `lib/graphify.mjs:229-237`).
///
/// Байт-в-байт с JS: строка уходит в промпт модели и в golden-фикстуры.
pub fn format_blast_block(
    symbol: &str,
    deps: &[AffectedDep],
    limits: &BlastLimits,
    seed_is_god_node: bool,
) -> String {
    let god = if deps.len() >= limits.god_node_threshold || seed_is_god_node {
        " \u{26A0}\u{FE0F} god-node (высокий риск регрессий)"
    } else {
        ""
    };
    let shown = deps
        .iter()
        .take(limits.max_deps_per_symbol)
        .map(format_dep_entry)
        .collect::<Vec<_>>()
        .join(", ");
    let more = if deps.len() > limits.max_deps_per_symbol {
        format!(" \u{2026} (+{})", deps.len() - limits.max_deps_per_symbol)
    } else {
        String::new()
    };
    format!(
        "- `{}` \u{2192} {} зависимостей вне диффа{god}: {shown}{more}",
        escape_md_fragment(symbol),
        deps.len()
    )
}

/// Dedup по `file`: предпочитаем EXTRACTED, затем более конкретный label
/// (`dedupeDepsByFile`, `lib/graphify.mjs:240-253`).
///
/// Порядок результата — порядок первого появления файла (семантика `Map` из JS:
/// перезапись значения не двигает ключ).
pub fn dedupe_deps_by_file(deps: &[AffectedDep], drop_ambiguous: bool) -> Vec<AffectedDep> {
    let mut best: IndexMap<String, AffectedDep> = IndexMap::new();
    for d in deps {
        let conf = d.confidence.clone().unwrap_or_default().to_uppercase();
        if drop_ambiguous && conf == "AMBIGUOUS" {
            continue;
        }
        let prev = match best.get(&d.file) {
            None => {
                best.insert(d.file.clone(), d.clone());
                continue;
            }
            Some(p) => p,
        };
        let d_rank = conf_rank(d.confidence.as_deref().unwrap_or(""));
        let p_rank = conf_rank(prev.confidence.as_deref().unwrap_or(""));
        let better_conf = d_rank > p_rank;
        let better_label = d_rank == p_rank
            && !d.label.is_empty()
            && (prev.label.is_empty() || js_len(&d.label) > js_len(&prev.label));
        if better_conf || better_label {
            best.insert(d.file.clone(), d.clone());
        }
    }
    best.into_values().collect()
}

// ---------------------------------------------------------------------------
// Исполнитель внешних процессов
// ---------------------------------------------------------------------------

/// Бюджеты одного вызова процесса — порт третьего аргумента `execFile`
/// (`lib/graphify.mjs:272`, `lib/graphify.mjs:313`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecOpts {
    /// Таймаут в миллисекундах; `0` — без таймаута (как `timeout: 0` у Node).
    pub timeout_ms: u64,
    /// Потолок байт на каждый из потоков вывода; превышение убивает процесс.
    pub max_buffer: usize,
}

/// Вывод процесса; `stdout`/`stderr` декодированы как UTF-8 с заменой
/// битых байт — так же, как `execFile` с `encoding: 'utf8'`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    /// Ответ только со stdout — удобно для фейкового исполнителя в тестах.
    pub fn stdout(text: impl Into<String>) -> Self {
        ExecOutput { stdout: text.into(), stderr: String::new() }
    }
}

/// Сбой вызова процесса в терминах, на которые смотрит `queryErrorReason`
/// (`lib/graphify.mjs:22-26`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    /// Бинарь не найден (`error.code === 'ENOENT'`) → `graphify_unavailable`.
    NotFound { message: String },
    /// Процесс убит: таймаут или переполнение `maxBuffer` (`error.killed`) →
    /// `graphify_timeout`.
    Killed { message: String },
    /// Всё остальное — ненулевой код возврата, ошибка ввода-вывода →
    /// `graphify_error`.
    Failed { message: String },
}

impl ExecError {
    /// Текст сбоя (в JS — `error.message`); нигде не участвует в решениях,
    /// только в диагностике.
    pub fn message(&self) -> &str {
        match self {
            ExecError::NotFound { message }
            | ExecError::Killed { message }
            | ExecError::Failed { message } => message,
        }
    }

    /// Причина пропуска символа (`queryErrorReason`, `lib/graphify.mjs:22-26`).
    pub fn skip_reason(&self) -> &'static str {
        match self {
            ExecError::NotFound { .. } => "graphify_unavailable",
            ExecError::Killed { .. } => "graphify_timeout",
            ExecError::Failed { .. } => "graphify_error",
        }
    }
}

/// Тип инжектируемого исполнителя процессов.
pub type ExecFn = dyn Fn(&str, &[String], ExecOpts) -> Result<ExecOutput, ExecError> + Send + Sync;

/// Боевой исполнитель: `std::process::Command` с таймаутом и `maxBuffer`.
///
/// Внешних крейтов нет по требованию RFC 16, поэтому таймаут собран вручную:
/// два потока-читателя разгружают трубы (иначе процесс с большим выводом
/// заблокируется на записи), а основной поток опрашивает `try_wait` до дедлайна
/// и по его истечении убивает ребёнка. Переполнение `max_buffer` в любом из
/// потоков тоже убивает процесс — как это делает Node.
pub fn default_exec(bin: &str, args: &[String], opts: ExecOpts) -> Result<ExecOutput, ExecError> {
    let mut child = match Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ExecError::NotFound { message: format!("spawn {bin} ENOENT") });
        }
        Err(e) => return Err(ExecError::Failed { message: format!("spawn {bin}: {e}") }),
    };

    let overflow = Arc::new(AtomicBool::new(false));
    let out_pipe = child.stdout.take().expect("stdout запрошен как piped");
    let err_pipe = child.stderr.take().expect("stderr запрошен как piped");
    let out_reader = spawn_reader(out_pipe, opts.max_buffer, Arc::clone(&overflow));
    let err_reader = spawn_reader(err_pipe, opts.max_buffer, Arc::clone(&overflow));

    let deadline = (opts.timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(opts.timeout_ms));
    let mut killed_reason: Option<&str> = None;
    let mut status = None;
    let mut nap = Duration::from_millis(1);
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) => {}
            Err(e) => {
                kill_and_reap(&mut child);
                return Err(ExecError::Failed { message: format!("wait {bin}: {e}") });
            }
        }
        if overflow.load(Ordering::SeqCst) {
            killed_reason = Some("maxBuffer exceeded");
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            killed_reason = Some("ETIMEDOUT");
            break;
        }
        thread::sleep(nap);
        nap = (nap * 2).min(Duration::from_millis(20));
    }

    if killed_reason.is_some() {
        kill_and_reap(&mut child);
    }
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();

    // Гонка: закрытая труба даёт процессу SIGPIPE, и он успевает умереть раньше,
    // чем цикл увидит флаг. Джойн читателей делает значение флага окончательным,
    // и переполнение буфера обязано победить «упал с ненулевым кодом».
    if killed_reason.is_none() && overflow.load(Ordering::SeqCst) {
        killed_reason = Some("maxBuffer exceeded");
        kill_and_reap(&mut child);
    }

    if let Some(reason) = killed_reason {
        return Err(ExecError::Killed { message: format!("{bin}: {reason}") });
    }
    let status = status.expect("цикл выходит либо со статусом, либо с killed_reason");
    if !status.success() {
        return Err(ExecError::Failed {
            message: format!("Command failed: {bin} {}\n{}", args.join(" "), js_trim(&stderr)),
        });
    }
    Ok(ExecOutput { stdout, stderr })
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Поток-читатель трубы: копит вывод до `max_buffer`, при превышении поднимает
/// флаг и выходит (процесс убьёт основной поток).
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

// ---------------------------------------------------------------------------
// Конфиг клиента
// ---------------------------------------------------------------------------

/// Поля `cfg.graphify` + `cfg.limits`, которые читает клиент
/// (`lib/config.mjs:353-359`, `lib/config.mjs:338-347`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphifyConfig {
    /// `GRAPHIFY_BIN` — исполняемый файл graphify.
    pub bin: String,
    /// `GRAPH` — путь к `graph.json`; рядом с ним ищется `.graphify_analysis.json`.
    pub graph: String,
    /// `GRAPHIFY_REPORT` — `GRAPH_REPORT.md`, из него берётся commit сборки графа.
    pub report: String,
    /// `REVIEW_GRAPHIFY_DEPTH`, уже приведённая по правилу клиента
    /// `Math.max(1, Number(depth) || 2)` (`lib/graphify.mjs:304`).
    pub depth: u32,
    /// `REVIEW_GRAPHIFY_DROP_AMBIGUOUS`: выбрасывать ли AMBIGUOUS-рёбра.
    pub drop_ambiguous: bool,
    /// Пороги рендера и параллелизма.
    pub limits: BlastLimits,
}

impl Default for GraphifyConfig {
    /// Дефолты движка (`lib/config.mjs:353-358`).
    fn default() -> Self {
        GraphifyConfig {
            bin: ".graphify/gf".to_string(),
            graph: ".graphify/graphify-out/graph.json".to_string(),
            report: ".graphify/graphify-out/GRAPH_REPORT.md".to_string(),
            depth: 2,
            drop_ambiguous: true,
            limits: BlastLimits::default(),
        }
    }
}

impl GraphifyConfig {
    /// Читает поля из замороженного конфига — как [`crate::llm::LlmConfig::from_cfg`].
    ///
    /// Отсутствующие поля дают дефолты движка, а не панику: конфиг собирает
    /// `load_review_config_parts`, но клиент обязан переживать урезанный объект
    /// (в JS это делают `||`).
    pub fn from_cfg(cfg: &Value) -> Self {
        let def = GraphifyConfig::default();
        let g = cfg.get("graphify");
        let limits = cfg.get("limits");
        let get = |root: Option<&Value>, key: &str| -> Option<Value> {
            root.and_then(|v| v.get(key)).cloned()
        };
        let string_or = |v: Option<Value>, def: String| -> String {
            match v {
                Some(x) if is_truthy(&x) => js_string(&x),
                _ => def,
            }
        };
        // JS `Math.max(1, Number(v) || def)`: NaN/0 → def, дробное усекается
        // (loadReviewConfig такого не порождает — см. док модуля).
        let count_or = |v: Option<Value>, def: usize| -> usize {
            let n = v.as_ref().map(js_number).unwrap_or(f64::NAN);
            let n = if n.is_nan() || n == 0.0 { def as f64 } else { n };
            if n < 1.0 {
                1
            } else {
                n as usize
            }
        };
        GraphifyConfig {
            bin: string_or(get(g, "bin"), def.bin),
            graph: string_or(get(g, "graph"), def.graph),
            report: string_or(get(g, "report"), def.report),
            depth: count_or(get(g, "depth"), def.depth as usize) as u32,
            // `g.dropAmbiguous !== false` (`lib/graphify.mjs:305`): выключает
            // только литеральный false, строка "false" оставляет фильтр включённым.
            drop_ambiguous: get(g, "dropAmbiguous") != Some(Value::Bool(false)),
            limits: BlastLimits {
                concurrency: count_or(get(limits, "concurrency"), def.limits.concurrency),
                god_node_threshold: match get(limits, "godNodeThreshold").as_ref().map(js_number) {
                    // Порог читается как `deps.length >= threshold`, и ноль тут
                    // осмыслен («всё — god-node»), поэтому count_or не подходит.
                    Some(n) if n.is_finite() && n >= 0.0 => n as usize,
                    _ => def.limits.god_node_threshold,
                },
                max_deps_per_symbol: count_or(
                    get(limits, "maxDepsPerSymbol"),
                    def.limits.max_deps_per_symbol,
                ),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Клиент
// ---------------------------------------------------------------------------

/// Статус опроса одного символа (`lib/graphify.mjs:316-337`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AffectedStatus {
    /// Символ найден в графе (даже если зависимостей нет).
    Resolved,
    /// `No unique node match` / `No node matching`.
    Unresolved,
    /// Вывод есть, но ни одна строка не разобралась.
    Unknown,
    /// Процесс упал.
    Error,
}

#[derive(Debug, Clone)]
struct AffectedOutcome {
    symbol: String,
    status: AffectedStatus,
    reason: Option<String>,
    deps: Vec<AffectedDep>,
    seed_is_god_node: bool,
}

/// Свежесть графа относительно HEAD (`readGraphFreshness`, `lib/graphify.mjs:266-283`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Freshness {
    fresh: Option<bool>,
    status: String,
    reason: Option<String>,
}

/// Клиент blast-radius — порт `createGraphifyClient` (`lib/graphify.mjs:299-401`).
///
/// Исполнитель процессов инжектируется, файлы читаются с реальной ФС.
pub struct RealGraphify {
    cfg: GraphifyConfig,
    exec: Box<ExecFn>,
}

impl std::fmt::Debug for RealGraphify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealGraphify").field("cfg", &self.cfg).finish_non_exhaustive()
    }
}

impl RealGraphify {
    /// Боевой клиент: процессы запускает [`default_exec`].
    pub fn new(cfg: GraphifyConfig) -> Self {
        Self::with_exec(cfg, Box::new(default_exec))
    }

    /// Клиент с подменённым исполнителем — аналог `deps.execFileP`
    /// (`test/pr-review.test.mjs:119`).
    pub fn with_exec(mut cfg: GraphifyConfig, exec: Box<ExecFn>) -> Self {
        // `Math.max(1, Number(g.depth) || 2)` (`lib/graphify.mjs:304`) применяется
        // и к собранному вручную конфигу, а не только к пути через from_cfg.
        cfg.depth = if cfg.depth == 0 { 2 } else { cfg.depth };
        RealGraphify { cfg, exec }
    }

    /// Конфиг клиента (полезно тестам и диагностике).
    pub fn config(&self) -> &GraphifyConfig {
        &self.cfg
    }

    /// Один `graphify affected <symbol>` (`lib/graphify.mjs:307-339`).
    fn affected(&self, symbol: &str, index: Option<&GraphIndex>) -> AffectedOutcome {
        // Порядок аргументов задан graphify CLI: `affected <query> [--graph] [--depth]`
        // (`query = sys.argv[2]`). Запуск без shell — символ, начинающийся с `-`,
        // не превращается во флаг.
        let args = vec![
            "affected".to_string(),
            symbol.to_string(),
            "--graph".to_string(),
            self.cfg.graph.clone(),
            "--depth".to_string(),
            self.cfg.depth.to_string(),
        ];
        let opts = ExecOpts { timeout_ms: AFFECTED_TIMEOUT_MS, max_buffer: AFFECTED_MAX_BUFFER };
        let stdout = match (self.exec)(&self.cfg.bin, &args, opts) {
            Ok(out) => out.stdout,
            Err(e) => {
                return AffectedOutcome {
                    symbol: symbol.to_string(),
                    status: AffectedStatus::Error,
                    reason: Some(e.skip_reason().to_string()),
                    deps: Vec::new(),
                    seed_is_god_node: false,
                };
            }
        };

        if stdout.contains("No unique node match") || stdout.contains("No node matching") {
            return AffectedOutcome {
                symbol: symbol.to_string(),
                status: AffectedStatus::Unresolved,
                reason: Some("symbol_unresolved".to_string()),
                deps: Vec::new(),
                seed_is_god_node: false,
            };
        }
        if stdout.contains("No affected nodes found") {
            return AffectedOutcome {
                symbol: symbol.to_string(),
                status: AffectedStatus::Resolved,
                reason: Some("no_dependents".to_string()),
                deps: Vec::new(),
                seed_is_god_node: false,
            };
        }

        let re = affected_line_re();
        let mut deps = Vec::new();
        for line in stdout.split('\n') {
            let Some(m) = re.captures(line) else { continue };
            let raw = AffectedDep::raw(
                m.get(1).map_or("", |g| g.as_str()),
                m.get(2).map_or("", |g| g.as_str()),
                m.get(3).map_or("", |g| g.as_str()),
                m.get(4).map_or("", |g| g.as_str()).parse::<f64>().unwrap_or(f64::NAN),
            );
            deps.push(enrich_affected_dep(&raw, symbol, index));
        }
        let mut unique = dedupe_deps_by_file(&deps, self.cfg.drop_ambiguous);
        if unique.is_empty() {
            let (status, reason) = if deps.is_empty() {
                (AffectedStatus::Unknown, "graphify_unparsed_output")
            } else {
                (AffectedStatus::Resolved, "ambiguous_only")
            };
            return AffectedOutcome {
                symbol: symbol.to_string(),
                status,
                reason: Some(reason.to_string()),
                deps: Vec::new(),
                seed_is_god_node: false,
            };
        }
        let seed_is_god_node = unique.iter().any(|d| d.seed_is_god_node);
        // JS: `delete d.seedIsGodNode` — флаг сводный, на самой зависимости он
        // ничего не значит и в рендер попасть не должен.
        for d in &mut unique {
            d.seed_is_god_node = false;
        }
        AffectedOutcome {
            symbol: symbol.to_string(),
            status: AffectedStatus::Resolved,
            reason: None,
            deps: unique,
            seed_is_god_node,
        }
    }

    /// Свежесть графа: commit из отчёта против `git rev-parse HEAD`
    /// (`lib/graphify.mjs:266-283`).
    ///
    /// В JS оба эффекта стартуют одновременно (`Promise.all`), поэтому `git`
    /// вызывается даже когда отчёт не читается; здесь тот же порядок — чтение
    /// отчёта не прерывает вызов git.
    fn read_graph_freshness(&self) -> Freshness {
        let unknown = |reason: &str| Freshness {
            fresh: None,
            status: "unknown".to_string(),
            reason: Some(reason.to_string()),
        };

        let git_cwd = safe_git_cwd(if self.cfg.graph.is_empty() {
            &self.cfg.report
        } else {
            &self.cfg.graph
        });
        let report = read_text(&self.cfg.report);
        let args = vec![
            "-C".to_string(),
            git_cwd.to_string_lossy().into_owned(),
            "rev-parse".to_string(),
            "HEAD".to_string(),
        ];
        let head = (self.exec)(
            "git",
            &args,
            ExecOpts { timeout_ms: GIT_TIMEOUT_MS, max_buffer: GIT_MAX_BUFFER },
        );

        let (Some(report), Ok(head)) = (report, head) else {
            return unknown("graph_freshness_unknown");
        };
        let Some(built) = built_commit_re()
            .captures(&report)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
        else {
            return unknown("graph_build_commit_unknown");
        };
        if js_trim(&head.stdout).starts_with(&built) {
            Freshness { fresh: Some(true), status: "fresh".to_string(), reason: None }
        } else {
            Freshness {
                fresh: Some(false),
                status: "stale".to_string(),
                reason: Some("graph_stale".to_string()),
            }
        }
    }

    /// Индекс графа + опциональный `.graphify_analysis.json` рядом с ним
    /// (`loadGraphEnrichment`, `lib/graphify.mjs:285-297`).
    fn load_graph_enrichment(&self) -> Option<GraphIndex> {
        let raw: Value = serde_json::from_str(&read_text(&self.cfg.graph)?).ok()?;
        let analysis_path =
            Path::new(&node_dirname(&self.cfg.graph)).join(".graphify_analysis.json");
        let analysis = read_text(&analysis_path.to_string_lossy())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        Some(build_graph_index(&raw, analysis.as_ref()))
    }
}

impl GraphifyApi for RealGraphify {
    fn build_blast_radius(&self, symbols: &[String]) -> Result<BlastRadius> {
        // `access(g.graph, R_OK)` (`lib/graphify.mjs:343`): нет графа — нет ни
        // одного внешнего вызова, включая git.
        if std::fs::File::open(&self.cfg.graph).is_err() {
            let mut reasons = IndexMap::new();
            reasons.insert("graph_missing".to_string(), 1);
            return Ok(empty_blast(symbols.len(), None, "missing", reasons));
        }

        let freshness = self.read_graph_freshness();
        let index = self.load_graph_enrichment();
        let results = run_limited(symbols.to_vec(), self.cfg.limits.concurrency, |s| {
            self.affected(&s, index.as_ref())
        });

        // Порядок вставки значим: при единственной причине именно она попадает
        // в `skip_reason` (`lib/graphify.mjs:391`).
        let mut skip_reasons: IndexMap<String, u64> = IndexMap::new();
        if let Some(r) = &freshness.reason {
            add_reason(&mut skip_reasons, r);
        }
        if symbols.is_empty() {
            add_reason(&mut skip_reasons, "no_symbols_in_diff");
        }
        if index.is_none() {
            add_reason(&mut skip_reasons, "graph_enrichment_unavailable");
        }

        let mut symbols_resolved = 0usize;
        let mut affected_nonempty = 0usize;
        let mut cross_community = 0u64;
        let mut confidence = BlastConfidence::default();
        let mut affected_files: Vec<String> = Vec::new();
        let mut blocks = Vec::new();

        for r in &results {
            if r.status == AffectedStatus::Resolved {
                symbols_resolved += 1;
            }
            if let Some(reason) = &r.reason {
                add_reason(&mut skip_reasons, reason);
            }
            if r.deps.is_empty() {
                continue;
            }
            affected_nonempty += 1;
            for d in &r.deps {
                if !affected_files.contains(&d.file) {
                    affected_files.push(d.file.clone());
                }
                let c = match d.confidence.as_deref() {
                    Some(s) if !s.is_empty() => s.to_uppercase(),
                    _ => "UNKNOWN".to_string(),
                };
                match c.as_str() {
                    "EXTRACTED" => confidence.extracted += 1,
                    "INFERRED" => confidence.inferred += 1,
                    "AMBIGUOUS" => confidence.ambiguous += 1,
                    _ => confidence.unknown += 1,
                }
                if d.cross_community {
                    cross_community += 1;
                }
            }
            blocks.push(format_blast_block(
                &r.symbol,
                &r.deps,
                &self.cfg.limits,
                r.seed_is_god_node,
            ));
        }
        affected_files.sort();

        Ok(BlastRadius {
            graph_index: index.map(std::sync::Arc::new),
            blocks,
            metrics: BlastMetrics {
                symbols_total: symbols.len(),
                symbols_resolved: Some(symbols_resolved),
                affected_nonempty,
                graph_fresh: freshness.fresh,
                graph_status: freshness.status,
                skip_reason: skip_reason_of(&skip_reasons),
                skip_reasons,
                affected_files,
                confidence,
                cross_community,
            },
        })
    }
}

/// `addReason` (`lib/graphify.mjs:18-20`).
fn add_reason(reasons: &mut IndexMap<String, u64>, reason: &str) {
    *reasons.entry(reason.to_string()).or_insert(0) += 1;
}

/// `'none'` / единственная причина / `'mixed'` (`lib/graphify.mjs:391`).
fn skip_reason_of(reasons: &IndexMap<String, u64>) -> String {
    match reasons.len() {
        0 => "none".to_string(),
        1 => reasons.keys().next().expect("len == 1").clone(),
        _ => "mixed".to_string(),
    }
}

/// `emptyMetrics` (`lib/graphify.mjs:28-46`): пустые блоки плюс причина пропуска.
fn empty_blast(
    symbols_total: usize,
    graph_fresh: Option<bool>,
    graph_status: &str,
    skip_reasons: IndexMap<String, u64>,
) -> BlastRadius {
    BlastRadius {
        graph_index: None,
        blocks: Vec::new(),
        metrics: BlastMetrics {
            symbols_total,
            symbols_resolved: Some(0),
            affected_nonempty: 0,
            graph_fresh,
            graph_status: graph_status.to_string(),
            skip_reason: skip_reason_of(&skip_reasons),
            skip_reasons,
            affected_files: Vec::new(),
            confidence: BlastConfidence::default(),
            cross_community: 0,
        },
    }
}

/// `readFile(path, 'utf8')`: битые байты заменяются, как в Node, а не ломают чтение.
fn read_text(path: &str) -> Option<String> {
    std::fs::read(path).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

// ---------------------------------------------------------------------------
// Пути
// ---------------------------------------------------------------------------

/// POSIX-`path.dirname` из Node (включая причуду `'//'` для корня).
fn node_dirname(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return ".".to_string();
    }
    let has_root = bytes[0] == b'/';
    let mut end: Option<usize> = None;
    let mut matched_slash = true;
    for i in (1..bytes.len()).rev() {
        if bytes[i] == b'/' {
            if !matched_slash {
                end = Some(i);
                break;
            }
        } else {
            matched_slash = false;
        }
    }
    match end {
        None => {
            if has_root {
                "/".to_string()
            } else {
                ".".to_string()
            }
        }
        Some(1) if has_root => "//".to_string(),
        Some(e) => path[..e].to_string(),
    }
}

/// Лексический `path.resolve`: абсолютизация относительно cwd и свёртка `.`/`..`
/// без обращения к ФС (симлинки Node тоже не разворачивает).
fn resolve_lexical(path: &str) -> PathBuf {
    let p = Path::new(path);
    let base = if p.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    };
    let joined = base.join(p);
    let mut out = PathBuf::new();
    for c in joined.components() {
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
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Каталог для `git -C`: абсолютный путь без управляющих символов
/// (`safeGitCwd`, `lib/graphify.mjs:256-264`).
fn safe_git_cwd(graph_path: &str) -> PathBuf {
    let source = if graph_path.is_empty() { "." } else { graph_path };
    let raw = js_trim(source);
    if raw.is_empty() || raw.contains(['\0', '\r', '\n']) {
        return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    }
    resolve_lexical(&node_dirname(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    // -----------------------------------------------------------------------
    // Чистые функции
    // -----------------------------------------------------------------------

    #[test]
    fn bare_label_снимает_все_хвостовые_скобки() {
        // Порт JS-теста «bareLabel: снимает все хвостовые ()»
        // (test/pr-review.test.mjs:1342).
        assert_eq!(bare_label("foo()()"), "foo");
        assert_eq!(bare_label("bar()"), "bar");
        assert_eq!(bare_label("baz"), "baz");
        assert_eq!(bare_label("  spaced()  "), "spaced");
        assert_eq!(bare_label("()"), "");
        assert_eq!(bare_label(""), "");
    }

    #[test]
    fn affected_line_re_разбирает_строки_вывода() {
        let re = affected_line_re();
        let c = re.captures("- caller [calls] app/server.ts:L42").unwrap();
        assert_eq!(&c[1], "caller");
        assert_eq!(&c[2], "calls");
        assert_eq!(&c[3], "app/server.ts");
        assert_eq!(&c[4], "42");

        // `L` необязательна, хвостовые пробелы и \r съедаются.
        let c = re.captures("- x() [uses] a.ts:7\r").unwrap();
        assert_eq!(&c[3], "a.ts");
        assert_eq!(&c[4], "7");

        // Путь с двоеточиями: ленивый `.+?` доходит до последней группы цифр.
        let c = re.captures("- h [uses] pkg:mod/a.ts:L9").unwrap();
        assert_eq!(&c[3], "pkg:mod/a.ts");
        assert_eq!(&c[4], "9");

        // Не строка зависимости — не матчится.
        assert!(re.captures("No affected nodes found").is_none());
        assert!(re.captures("- broken [calls] a.ts").is_none());
        // relation ограничена ASCII [\w_], как в JS.
        assert!(re.captures("- x [вызов] a.ts:L1").is_none());
    }

    #[test]
    fn build_graph_index_читает_links_и_edges_и_чистит_мусор() {
        // `links` — основной ключ.
        let by_links = build_graph_index(
            &json!({
                "nodes": [{ "id": "a", "label": "A" }],
                "links": [{ "source": "a", "target": "a" }],
            }),
            None,
        );
        assert_eq!(by_links.inbound_count("a"), 1);

        // `edges` — запасной; используется только при отсутствии `links`.
        let by_edges = build_graph_index(
            &json!({
                "nodes": [{ "id": "a", "label": "A" }],
                "edges": [{ "source": "a", "target": "a" }],
            }),
            None,
        );
        assert_eq!(by_edges.inbound_count("a"), 1);

        // links присутствует, но пуст — edges игнорируются (JS: тернарник по links).
        let both = build_graph_index(
            &json!({
                "nodes": [{ "id": "a" }],
                "links": [],
                "edges": [{ "source": "a", "target": "a" }],
            }),
            None,
        );
        assert_eq!(both.inbound_count("a"), 0);

        // Узлы без id и рёбра без target выбрасываются.
        let dirty = build_graph_index(
            &json!({
                "nodes": [{ "label": "no-id" }, Value::Null, { "id": "ok", "label": "Ok" }],
                "links": [{ "source": "ok" }, { "source": "ok", "target": "" }],
            }),
            None,
        );
        assert_eq!(dirty.node_count(), 1);
        assert!(dirty.node_by_id("ok").is_some());
        assert_eq!(dirty.inbound_count(""), 0);

        // Числовой id приводится через String(): 5 → "5".
        let numeric = build_graph_index(&json!({ "nodes": [{ "id": 5, "label": "N" }] }), None);
        assert!(numeric.node_by_id("5").is_some());

        // Некорректная форма графа не роняет индекс.
        let broken = build_graph_index(&json!({ "nodes": "нет", "links": 7 }), None);
        assert_eq!(broken.node_count(), 0);
    }

    #[test]
    fn build_graph_index_читает_gods_строками_и_объектами() {
        let index = build_graph_index(
            &json!({ "nodes": [{ "id": "a", "label": "Create" }] }),
            Some(&json!({ "gods": ["Store", { "label": "Create" }, { "nope": 1 }, "", Value::Null] })),
        );
        assert!(index.is_god("create"), "casefold: Create → create");
        assert!(index.is_god("STORE"));
        assert!(!index.is_god("other"));
    }

    #[test]
    fn resolve_unique_node_exact_побеждает_bare_омоним() {
        // Порт JS-теста «Graphify resolveUniqueNode: exact побеждает bare-омоним
        // Foo vs Foo()» (test/pr-review.test.mjs:221).
        let index = build_graph_index(
            &json!({
                "nodes": [
                    { "id": "plain", "label": "Foo", "source_file": "a.ts", "community": 1, "community_name": "A" },
                    { "id": "call", "label": "Foo()", "source_file": "b.ts", "community": 2, "community_name": "B" },
                    { "id": "caller", "label": "use()", "source_file": "c.ts", "community": 3 },
                ],
                "links": [
                    { "source": "caller", "target": "plain", "relation": "calls", "confidence": "EXTRACTED", "source_file": "c.ts" },
                    { "source": "caller", "target": "call", "relation": "calls", "confidence": "INFERRED", "source_file": "c.ts" },
                ],
            }),
            None,
        );
        assert_eq!(resolve_unique_node(&index, "Foo").unwrap()["id"], json!("plain"));
        assert_eq!(resolve_unique_node(&index, "Foo()").unwrap()["id"], json!("call"));
        // id имеет приоритет над меткой.
        assert_eq!(resolve_unique_node(&index, "caller").unwrap()["id"], json!("caller"));
        // Пустой query — сразу None.
        assert!(resolve_unique_node(&index, "").is_none());

        // Два одинаковых exact — не угадываем.
        let dup = build_graph_index(
            &json!({
                "nodes": [
                    { "id": "a", "label": "handler()", "source_file": "a.ts", "community": 1 },
                    { "id": "b", "label": "handler()", "source_file": "b.ts", "community": 2 },
                ],
                "links": [],
            }),
            None,
        );
        assert!(resolve_unique_node(&dup, "handler").is_none());
        assert!(resolve_unique_node(&dup, "handler()").is_none());
    }

    #[test]
    fn enrich_affected_dep_омонимный_seed_не_подмешивает_чужие_цифры() {
        // Порт JS-теста «Graphify enrichAffectedDep: омонимный seed не подмешивает
        // чужой confidence/community» (test/pr-review.test.mjs:247).
        let index = build_graph_index(
            &json!({
                "nodes": [
                    { "id": "wrong", "label": "create", "source_file": "wrong.ts", "community": 99, "community_name": "wrong-comm" },
                    { "id": "right", "label": "create()", "source_file": "right.ts", "community": 1, "community_name": "right-comm" },
                    { "id": "caller", "label": "ui()", "source_file": "ui.ts", "community": 2, "community_name": "ui" },
                ],
                // Входящие есть только у `wrong` (первого в индексе): EXTRACTED + cross-community.
                "links": [
                    { "source": "caller", "target": "wrong", "relation": "calls", "confidence": "EXTRACTED", "source_file": "ui.ts" },
                ],
            }),
            Some(&json!({ "gods": [{ "label": "create" }] })),
        );

        let dep = AffectedDep::raw("ui()", "calls", "ui.ts", 1.0);

        // bare-омоним create + create(): query `create()` уникален по exact.
        let ok = enrich_affected_dep(&dep, "create()", Some(&index));
        assert_eq!(ok.seed_community, Some(json!(1)));
        assert_eq!(ok.confidence, None, "нет ребра к create() — не берём от wrong");
        assert!(ok.cross_community, "community seed≠caller — ок даже без ребра");
        assert!(!ok.seed_is_god_node, "gods держит exact \"create\", а seed — create()");

        // Регрессия first-wins: query `create()` не должен взять inbound от plain `create`.
        let as_wrong = enrich_affected_dep(&dep, "create", Some(&index));
        assert_eq!(as_wrong.seed_community, Some(json!(99)));
        assert_eq!(as_wrong.confidence.as_deref(), Some("EXTRACTED"));
        assert!(as_wrong.seed_is_god_node, "seed create помечен god-node");

        // Полная коллизия двух make(): обогащения нет вовсе.
        let amb = build_graph_index(
            &json!({
                "nodes": [
                    { "id": "a", "label": "make()", "source_file": "a.ts", "community": 1 },
                    { "id": "b", "label": "make()", "source_file": "b.ts", "community": 2 },
                    { "id": "c", "label": "caller()", "source_file": "c.ts", "community": 3 },
                ],
                "links": [
                    { "source": "c", "target": "a", "relation": "calls", "confidence": "EXTRACTED", "source_file": "c.ts" },
                ],
            }),
            None,
        );
        let skipped = enrich_affected_dep(
            &AffectedDep::raw("caller()", "calls", "c.ts", 3.0),
            "make",
            Some(&amb),
        );
        assert_eq!(skipped.confidence, None);
        assert_eq!(skipped.seed_community, None);
        assert!(!skipped.cross_community);
        assert!(!skipped.seed_is_god_node);
    }

    #[test]
    fn enrich_affected_dep_без_индекса_возвращает_dep_как_есть() {
        let dep = AffectedDep::raw("x()", "calls", "a.ts", 1.0);
        assert_eq!(enrich_affected_dep(&dep, "seed", None), dep);
    }

    #[test]
    fn enrich_affected_dep_берёт_ребро_с_максимальным_confidence() {
        let index = build_graph_index(
            &json!({
                "nodes": [
                    { "id": "seed", "label": "seed", "source_file": "s.ts", "community": 1 },
                    { "id": "c", "label": "c()", "source_file": "c.ts", "community": 1 },
                ],
                "links": [
                    { "source": "c", "target": "seed", "relation": "calls", "confidence": "INFERRED", "source_file": "c.ts", "confidence_score": 0.5 },
                    { "source": "c", "target": "seed", "relation": "calls", "confidence": "EXTRACTED", "source_file": "c.ts", "confidence_score": 0.9 },
                    // relation не совпадает — ребро не рассматривается.
                    { "source": "c", "target": "seed", "relation": "uses", "confidence": "EXTRACTED", "source_file": "c.ts" },
                ],
            }),
            None,
        );
        let out = enrich_affected_dep(
            &AffectedDep::raw("c()", "calls", "c.ts", 2.0),
            "seed",
            Some(&index),
        );
        assert_eq!(out.confidence.as_deref(), Some("EXTRACTED"));
        assert_eq!(out.confidence_score, Some(0.9));
        assert_eq!(out.community, Some(json!(1)));
        assert!(!out.cross_community, "одно сообщество — не cross");

        // Ребро без confidence → дефолт EXTRACTED (`lib/graphify.mjs:200`).
        let no_conf = build_graph_index(
            &json!({
                "nodes": [
                    { "id": "seed", "label": "seed", "source_file": "s.ts" },
                    { "id": "c", "label": "c()", "source_file": "c.ts" },
                ],
                "links": [{ "source": "c", "target": "seed", "relation": "calls", "source_file": "c.ts" }],
            }),
            None,
        );
        let out = enrich_affected_dep(
            &AffectedDep::raw("c()", "calls", "c.ts", 2.0),
            "seed",
            Some(&no_conf),
        );
        assert_eq!(out.confidence.as_deref(), Some("EXTRACTED"));
        assert_eq!(out.confidence_score, None);
    }

    #[test]
    fn enrich_affected_dep_ставит_seed_community_name_с_фолбэком() {
        let index = build_graph_index(
            &json!({
                "nodes": [
                    { "id": "seed", "label": "seed", "community": 1, "community_label": "лейбл" },
                    { "id": "c", "label": "c()", "source_file": "c.ts" },
                ],
                "links": [],
            }),
            None,
        );
        let out = enrich_affected_dep(
            &AffectedDep::raw("c()", "calls", "c.ts", 1.0),
            "seed",
            Some(&index),
        );
        assert_eq!(out.seed_community_name, Some(json!("лейбл")), "community_label — запасной ключ");
        assert_eq!(out.community_name, Some(Value::Null), "у caller имени нет → null, не отсутствие");
    }

    #[test]
    fn resolve_caller_node_доразрешает_омонима_по_файлу() {
        let index = build_graph_index(
            &json!({
                "nodes": [
                    { "id": "seed", "label": "seed", "community": 1 },
                    { "id": "h1", "label": "handler()", "source_file": "a.ts", "community": 7 },
                    { "id": "h2", "label": "handler()", "source_file": "b.ts", "community": 8 },
                ],
                "links": [],
            }),
            None,
        );
        let out = enrich_affected_dep(
            &AffectedDep::raw("handler()", "calls", "b.ts", 1.0),
            "seed",
            Some(&index),
        );
        assert_eq!(out.community, Some(json!(8)), "омоним доразрешён по source_file");
        assert!(out.cross_community, "1 ≠ 8");

        // Единственный узел файла — последний шанс (метки не совпали вовсе).
        let out = enrich_affected_dep(
            &AffectedDep::raw("неизвестный", "calls", "a.ts", 1.0),
            "seed",
            Some(&index),
        );
        assert_eq!(out.community, Some(json!(7)));
    }

    #[test]
    fn format_dep_entry_байт_в_байт() {
        let mut d = AffectedDep::raw("caller", "calls", "app/server.ts", 42.0);
        d.confidence = Some("EXTRACTED".to_string());
        d.cross_community = true;
        assert_eq!(
            format_dep_entry(&d),
            "`caller` @ app/server.ts:42 [calls][EXTRACTED] cross-community"
        );

        // Без метки, без confidence, без cross-community.
        let bare = AffectedDep::raw("", "uses", "a.ts", 7.0);
        assert_eq!(format_dep_entry(&bare), "a.ts:7 [uses]");

        // Пустой relation печатается пустыми скобками (JS: `d.relation || ''`).
        let no_rel = AffectedDep::raw("x", "", "a.ts", 1.0);
        assert_eq!(format_dep_entry(&no_rel), "`x` @ a.ts:1 []");
    }

    #[test]
    fn format_dep_entry_экранирует_markdown() {
        // ` → ', [ → (, ] → ), * → ∗ (U+2217), _ → ˍ (U+02CD).
        let d = AffectedDep::raw("a`b[c]d*e_f", "calls", "we[i]rd_*.ts", 3.0);
        assert_eq!(
            format_dep_entry(&d),
            "`a'b(c)d\u{2217}e\u{02CD}f` @ we(i)rd\u{02CD}\u{2217}.ts:3 [calls]"
        );
    }

    #[test]
    fn format_blast_block_байт_в_байт() {
        let limits = BlastLimits { concurrency: 2, god_node_threshold: 20, max_deps_per_symbol: 20 };
        let mut d = AffectedDep::raw("caller", "calls", "app/server.ts", 42.0);
        d.confidence = Some("EXTRACTED".to_string());
        assert_eq!(
            format_blast_block("usedElsewhere", std::slice::from_ref(&d), &limits, false),
            "- `usedElsewhere` \u{2192} 1 зависимостей вне диффа: \
             `caller` @ app/server.ts:42 [calls][EXTRACTED]"
        );

        // seedIsGodNode поднимает маркер и без превышения порога.
        assert!(format_blast_block("s", std::slice::from_ref(&d), &limits, true)
            .contains(" \u{26A0}\u{FE0F} god-node (высокий риск регрессий): "));
    }

    #[test]
    fn format_blast_block_режет_по_max_deps_и_помечает_god_node() {
        let limits = BlastLimits { concurrency: 2, god_node_threshold: 3, max_deps_per_symbol: 2 };
        let deps: Vec<AffectedDep> = (1..=5)
            .map(|i| AffectedDep::raw("c", "calls", &format!("f{i}.ts"), i as f64))
            .collect();
        assert_eq!(
            format_blast_block("sym", &deps, &limits, false),
            "- `sym` \u{2192} 5 зависимостей вне диффа \u{26A0}\u{FE0F} god-node \
             (высокий риск регрессий): `c` @ f1.ts:1 [calls], `c` @ f2.ts:2 [calls] \u{2026} (+3)"
        );
    }

    #[test]
    fn dedupe_deps_by_file_дропает_ambiguous_и_выбирает_лучшего() {
        let mk = |file: &str, conf: &str, label: &str| {
            let mut d = AffectedDep::raw(label, "uses", file, 1.0);
            d.confidence = if conf.is_empty() { None } else { Some(conf.to_string()) };
            d
        };
        let deps = vec![
            mk("a.ts", "INFERRED", "short"),
            mk("a.ts", "EXTRACTED", "x"),   // выше confidence — побеждает
            mk("b.ts", "AMBIGUOUS", "amb"), // дропается
            mk("c.ts", "INFERRED", "ab"),
            mk("c.ts", "INFERRED", "abcdef"), // тот же ранг, длиннее метка
            mk("d.ts", "", ""),
            mk("d.ts", "", "named"), // пустая метка проигрывает любой
        ];
        let out = dedupe_deps_by_file(&deps, true);
        assert_eq!(
            out.iter().map(|d| (d.file.as_str(), d.label.as_str())).collect::<Vec<_>>(),
            vec![("a.ts", "x"), ("c.ts", "abcdef"), ("d.ts", "named")],
            "порядок — по первому появлению файла (семантика Map)"
        );

        // dropAmbiguous=false оставляет AMBIGUOUS.
        let kept = dedupe_deps_by_file(&deps, false);
        assert!(kept.iter().any(|d| d.file == "b.ts"));
    }

    // -----------------------------------------------------------------------
    // Пути
    // -----------------------------------------------------------------------

    #[test]
    fn node_dirname_повторяет_path_dirname() {
        assert_eq!(node_dirname(""), ".");
        assert_eq!(node_dirname("graph.json"), ".");
        assert_eq!(node_dirname("a/b.json"), "a");
        assert_eq!(node_dirname("/a/b.json"), "/a");
        assert_eq!(node_dirname("/a"), "/");
        assert_eq!(node_dirname("a/"), ".");
        assert_eq!(node_dirname("/"), "/");
        assert_eq!(node_dirname("a/b/"), "a");
    }

    #[test]
    fn safe_git_cwd_отбивает_управляющие_символы() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(safe_git_cwd("bad\nname/graph.json"), cwd, "\\n → cwd");
        assert_eq!(safe_git_cwd("graph.json"), cwd, "dirname='.' → cwd");
        assert_eq!(safe_git_cwd(""), cwd, "пустой путь → '.' → cwd");
        assert_eq!(safe_git_cwd("/tmp/out/graph.json"), PathBuf::from("/tmp/out"));
        assert_eq!(safe_git_cwd("/tmp/out/../graph.json"), PathBuf::from("/tmp"));
    }

    // -----------------------------------------------------------------------
    // Конфиг
    // -----------------------------------------------------------------------

    #[test]
    fn graphify_config_from_cfg_читает_поля_и_держит_дефолты() {
        let cfg = GraphifyConfig::from_cfg(&json!({
            "graphify": { "bin": "gf", "graph": "g.json", "report": "R.md", "depth": 4, "dropAmbiguous": false },
            "limits": { "concurrency": 2, "godNodeThreshold": 3, "maxDepsPerSymbol": 5 },
        }));
        assert_eq!(cfg.bin, "gf");
        assert_eq!(cfg.graph, "g.json");
        assert_eq!(cfg.report, "R.md");
        assert_eq!(cfg.depth, 4);
        assert!(!cfg.drop_ambiguous);
        assert_eq!(
            cfg.limits,
            BlastLimits { concurrency: 2, god_node_threshold: 3, max_deps_per_symbol: 5 }
        );

        // Пустой конфиг — дефолты движка.
        assert_eq!(GraphifyConfig::from_cfg(&json!({})), GraphifyConfig::default());

        // `g.dropAmbiguous !== false`: выключает только литеральный false.
        for v in [json!("false"), json!(0), Value::Null] {
            let c = GraphifyConfig::from_cfg(&json!({ "graphify": { "dropAmbiguous": v } }));
            assert!(c.drop_ambiguous, "выключает только литеральный false");
        }

        // `Number(depth) || 2` — falsy глубина даёт 2, нулевой порог god-node сохраняется.
        assert_eq!(GraphifyConfig::from_cfg(&json!({ "graphify": { "depth": 0 } })).depth, 2);
        assert_eq!(
            GraphifyConfig::from_cfg(&json!({ "limits": { "godNodeThreshold": 0 } }))
                .limits
                .god_node_threshold,
            0
        );
    }

    // -----------------------------------------------------------------------
    // Клиент
    // -----------------------------------------------------------------------

    /// Журнал вызовов фейкового исполнителя: `[bin, ...args]` — как `calls`
    /// в JS-тестах (test/pr-review.test.mjs:97).
    type Calls = Arc<Mutex<Vec<Vec<String>>>>;

    fn fake_exec(
        calls: &Calls,
        handler: impl Fn(&str, &[String]) -> Result<ExecOutput, ExecError> + Send + Sync + 'static,
    ) -> Box<ExecFn> {
        let calls = Arc::clone(calls);
        Box::new(move |bin, args, _opts| {
            let mut row = vec![bin.to_string()];
            row.extend(args.iter().cloned());
            calls.lock().unwrap().push(row);
            handler(bin, args)
        })
    }

    /// Каталог с graph.json и GRAPH_REPORT.md; возвращает готовый конфиг.
    fn fixture(graph: &str, report: &str) -> (tempfile::TempDir, GraphifyConfig) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("graph.json"), graph).unwrap();
        std::fs::write(dir.path().join("GRAPH_REPORT.md"), report).unwrap();
        let cfg = GraphifyConfig {
            bin: "gf".to_string(),
            graph: dir.path().join("graph.json").to_string_lossy().into_owned(),
            report: dir.path().join("GRAPH_REPORT.md").to_string_lossy().into_owned(),
            depth: 2,
            drop_ambiguous: true,
            limits: BlastLimits { concurrency: 2, god_node_threshold: 20, max_deps_per_symbol: 20 },
        };
        (dir, cfg)
    }

    const REPORT_STALE: &str = "# Graph Report\n- Built from commit: `12345678`\n";

    #[test]
    fn клиент_различает_stale_граф_неразрешимый_символ_и_реальные_зависимости() {
        // Порт JS-теста «Graphify: пустой blast-radius различает stale graph,
        // неразрешимый символ и реальные зависимости» (test/pr-review.test.mjs:96).
        let graph = json!({
            "nodes": [
                { "id": "seed", "label": "usedElsewhere", "source_file": "lib/x.ts", "community": 1, "community_name": "core" },
                { "id": "caller", "label": "caller", "source_file": "app/server.ts", "community": 2, "community_name": "app" },
            ],
            "links": [{
                "source": "caller", "target": "seed", "relation": "calls",
                "confidence": "EXTRACTED", "source_file": "app/server.ts", "source_location": "L42",
            }],
        })
        .to_string();
        let (_dir, cfg) = fixture(&graph, REPORT_STALE);

        let calls: Calls = Arc::default();
        let exec = fake_exec(&calls, |bin, args| {
            if bin == "git" {
                return Ok(ExecOutput::stdout("abcdef123456\n"));
            }
            match args[1].as_str() {
                "unresolved" => Ok(ExecOutput::stdout("No unique node match for unresolved\n")),
                "usedElsewhere" => Ok(ExecOutput::stdout("- caller [calls] app/server.ts:L42\n")),
                _ => Ok(ExecOutput::stdout("No affected nodes found\n")),
            }
        });
        let client = RealGraphify::with_exec(cfg, exec);

        let out = client
            .build_blast_radius(&[
                "unresolved".to_string(),
                "noDeps".to_string(),
                "usedElsewhere".to_string(),
            ])
            .unwrap();

        assert_eq!(out.blocks.len(), 1);
        assert!(out.blocks[0].contains("`caller` @ app/server.ts:42"), "{}", out.blocks[0]);
        assert!(out.blocks[0].contains("[EXTRACTED]"), "{}", out.blocks[0]);
        assert!(out.blocks[0].contains("cross-community"), "{}", out.blocks[0]);

        let m = &out.metrics;
        assert_eq!(m.symbols_total, 3);
        assert_eq!(m.symbols_resolved, Some(2));
        assert_eq!(m.affected_nonempty, 1);
        assert_eq!(m.graph_fresh, Some(false));
        assert_eq!(m.graph_status, "stale");
        assert_eq!(m.cross_community, 1);
        assert_eq!(m.confidence.extracted, 1);
        assert_eq!(m.affected_files, vec!["app/server.ts".to_string()]);
        assert_eq!(m.skip_reasons.get("symbol_unresolved"), Some(&1));
        assert_eq!(m.skip_reasons.get("no_dependents"), Some(&1));
        assert_eq!(m.skip_reasons.get("graph_stale"), Some(&1));
        assert_eq!(m.skip_reason, "mixed");

        let calls = calls.lock().unwrap();
        assert!(
            calls.iter().any(|c| c[0] == "git" && c.contains(&"-C".to_string())),
            "git -C для freshness"
        );
        assert!(
            calls.iter().any(|c| c[0] == "gf"
                && c.contains(&"--depth".to_string())
                && c.contains(&"2".to_string())),
            "передаём --depth"
        );
    }

    #[test]
    fn клиент_без_графа_даёт_graph_missing_и_не_зовёт_процессы() {
        // Порт JS-теста «Graphify: отсутствует graph.json → graph_missing, без
        // вызова affected» (test/pr-review.test.mjs:148).
        let dir = tempfile::tempdir().unwrap();
        let cfg = GraphifyConfig {
            bin: "gf".to_string(),
            graph: dir.path().join("missing-graph.json").to_string_lossy().into_owned(),
            report: dir.path().join("missing-report.md").to_string_lossy().into_owned(),
            ..GraphifyConfig::default()
        };
        let calls: Calls = Arc::default();
        let client =
            RealGraphify::with_exec(cfg, fake_exec(&calls, |_, _| Ok(ExecOutput::default())));

        let out = client.build_blast_radius(&["makeDataStore".to_string()]).unwrap();
        assert!(out.blocks.is_empty());
        assert_eq!(out.metrics.graph_status, "missing");
        assert_eq!(out.metrics.graph_fresh, None);
        assert_eq!(out.metrics.skip_reasons.get("graph_missing"), Some(&1));
        assert_eq!(out.metrics.skip_reason, "graph_missing");
        assert_eq!(out.metrics.symbols_resolved, Some(0));
        assert_eq!(out.metrics.symbols_total, 1);
        assert!(calls.lock().unwrap().is_empty(), "affected/git не вызываем без графа");
        assert!(out.graph_index.is_none(), "графа нет — индекса для карточек символов нет");
    }

    #[test]
    fn клиент_без_символов_отчитывается_no_symbols_in_diff() {
        // Порт JS-теста «Graphify: нет символов в диффе → честный skip
        // no_symbols_in_diff» (test/pr-review.test.mjs:166).
        let (_dir, cfg) = fixture(
            r#"{"nodes":[],"links":[]}"#,
            "# Graph Report\n- Built from commit: `abcdef12`\n",
        );
        let calls: Calls = Arc::default();
        let client = RealGraphify::with_exec(
            cfg,
            fake_exec(&calls, |bin, _| {
                Ok(ExecOutput::stdout(if bin == "git" { "abcdef12\n" } else { "" }))
            }),
        );

        let out = client.build_blast_radius(&[]).unwrap();
        assert!(out.graph_index.is_some(), "прочитанный граф отдаётся наружу (graphIndex)");
        assert_eq!(out.metrics.skip_reasons.get("no_symbols_in_diff"), Some(&1));
        assert_eq!(out.metrics.symbols_total, 0);
        assert_eq!(out.metrics.skip_reason, "no_symbols_in_diff");
        assert_eq!(out.metrics.graph_fresh, Some(true), "commit из отчёта совпал с HEAD");
    }

    #[test]
    fn клиент_дропает_ambiguous_и_не_выдумывает_cross_community() {
        // Порт JS-теста «Graphify: AMBIGUOUS drop + INFERRED без cross-community»
        // (test/pr-review.test.mjs:183).
        let graph = json!({
            "nodes": [
                { "id": "seed", "label": "foo", "source_file": "a.ts", "community": 1 },
                { "id": "amb", "label": "amb()", "source_file": "b.ts", "community": 1 },
                { "id": "inf", "label": "inf()", "source_file": "c.ts", "community": 1 },
            ],
            "links": [
                { "source": "amb", "target": "seed", "relation": "uses", "confidence": "AMBIGUOUS", "source_file": "b.ts" },
                { "source": "inf", "target": "seed", "relation": "uses", "confidence": "INFERRED", "source_file": "c.ts" },
            ],
        })
        .to_string();
        let (_dir, cfg) = fixture(&graph, "# Graph Report\n- Built from commit: `abcdef12`\n");
        let calls: Calls = Arc::default();
        let client = RealGraphify::with_exec(
            cfg,
            fake_exec(&calls, |bin, _| {
                Ok(ExecOutput::stdout(if bin == "git" {
                    "abcdef12\n"
                } else {
                    "- amb() [uses] b.ts:L1\n- inf() [uses] c.ts:L2\n"
                }))
            }),
        );

        let out = client.build_blast_radius(&["foo".to_string()]).unwrap();
        assert_eq!(out.blocks.len(), 1);
        assert!(!out.blocks[0].contains("b.ts"), "{}", out.blocks[0]);
        assert!(out.blocks[0].contains("`inf()` @ c.ts:2"), "{}", out.blocks[0]);
        assert!(out.blocks[0].contains("[INFERRED]"), "{}", out.blocks[0]);
        assert!(!out.blocks[0].contains("cross-community"), "{}", out.blocks[0]);
        assert_eq!(out.metrics.confidence.inferred, 1);
        assert_eq!(out.metrics.cross_community, 0);
        assert_eq!(out.metrics.graph_status, "fresh");
        assert_eq!(out.metrics.skip_reason, "none", "свежий граф — ни одной причины");
    }

    #[test]
    fn клиент_классифицирует_сбои_процесса() {
        // queryErrorReason (lib/graphify.mjs:22-26) — три класса ошибок.
        let cases: [(ExecError, &str); 3] = [
            (ExecError::NotFound { message: "ENOENT".into() }, "graphify_unavailable"),
            (ExecError::Killed { message: "ETIMEDOUT".into() }, "graphify_timeout"),
            (ExecError::Failed { message: "exit 1".into() }, "graphify_error"),
        ];
        for (err, expected) in cases {
            let (_dir, cfg) =
                fixture(r#"{"nodes":[],"links":[]}"#, "# R\n- Built from commit: `abcdef12`\n");
            let calls: Calls = Arc::default();
            let client = RealGraphify::with_exec(
                cfg,
                fake_exec(&calls, move |bin, _| {
                    if bin == "git" {
                        return Ok(ExecOutput::stdout("abcdef12\n"));
                    }
                    Err(err.clone())
                }),
            );
            let out = client.build_blast_radius(&["sym".to_string()]).unwrap();
            assert!(out.blocks.is_empty(), "fail-open: блоков нет, ошибки наружу нет");
            assert_eq!(out.metrics.skip_reasons.get(expected), Some(&1), "{expected}");
            assert_eq!(out.metrics.skip_reason, expected);
            assert_eq!(out.metrics.symbols_resolved, Some(0), "error не считается resolved");
        }
    }

    #[test]
    fn клиент_отчитывается_о_непонятной_свежести_графа() {
        // Отчёт без commit → graph_build_commit_unknown.
        let (_dir, cfg) = fixture(r#"{"nodes":[],"links":[]}"#, "# Graph Report\nбез коммита\n");
        let calls: Calls = Arc::default();
        let client = RealGraphify::with_exec(
            cfg,
            fake_exec(&calls, |_, _| Ok(ExecOutput::stdout("abcdef12\n"))),
        );
        let out = client.build_blast_radius(&["s".to_string()]).unwrap();
        assert_eq!(out.metrics.graph_status, "unknown");
        assert_eq!(out.metrics.graph_fresh, None);
        assert_eq!(out.metrics.skip_reasons.get("graph_build_commit_unknown"), Some(&1));

        // git упал → graph_freshness_unknown.
        let (_dir2, cfg2) = fixture(r#"{"nodes":[],"links":[]}"#, REPORT_STALE);
        let calls2: Calls = Arc::default();
        let client2 = RealGraphify::with_exec(
            cfg2,
            fake_exec(&calls2, |bin, _| {
                if bin == "git" {
                    return Err(ExecError::Failed { message: "not a git repo".into() });
                }
                Ok(ExecOutput::stdout("No affected nodes found\n"))
            }),
        );
        let out2 = client2.build_blast_radius(&["s".to_string()]).unwrap();
        assert_eq!(out2.metrics.graph_status, "unknown");
        assert_eq!(out2.metrics.skip_reasons.get("graph_freshness_unknown"), Some(&1));
        // git всё равно был вызван (в JS обе половины Promise.all стартуют).
        assert!(calls2.lock().unwrap().iter().any(|c| c[0] == "git"));

        // Отсутствующий отчёт при живом графе — тоже freshness_unknown, но граф читается.
        let (dir3, mut cfg3) = fixture(r#"{"nodes":[],"links":[]}"#, REPORT_STALE);
        cfg3.report = dir3.path().join("нет.md").to_string_lossy().into_owned();
        let calls3: Calls = Arc::default();
        let client3 = RealGraphify::with_exec(
            cfg3,
            fake_exec(&calls3, |_, _| Ok(ExecOutput::stdout("No affected nodes found\n"))),
        );
        let out3 = client3.build_blast_radius(&["s".to_string()]).unwrap();
        assert_eq!(out3.metrics.skip_reasons.get("graph_freshness_unknown"), Some(&1));
        assert_eq!(
            out3.metrics.skip_reasons.get("graph_enrichment_unavailable"),
            None,
            "граф валиден — обогащение доступно"
        );
    }

    #[test]
    fn клиент_переживает_битый_graph_json() {
        // JSON не парсится → индекса нет, но опрос символов идёт.
        let (_dir, cfg) = fixture("{не json", REPORT_STALE);
        let calls: Calls = Arc::default();
        let client = RealGraphify::with_exec(
            cfg,
            fake_exec(&calls, |bin, _| {
                Ok(ExecOutput::stdout(if bin == "git" {
                    "12345678\n"
                } else {
                    "- c() [uses] c.ts:L2\n"
                }))
            }),
        );
        let out = client.build_blast_radius(&["foo".to_string()]).unwrap();
        assert_eq!(out.metrics.skip_reasons.get("graph_enrichment_unavailable"), Some(&1));
        assert_eq!(out.metrics.graph_fresh, Some(true), "свежесть считается независимо от индекса");
        assert_eq!(out.blocks.len(), 1, "без обогащения блок всё равно строится");
        assert!(out.blocks[0].contains("`c()` @ c.ts:2 [uses]"), "{}", out.blocks[0]);
        assert!(!out.blocks[0].contains("[uses]["), "нет confidence — нет второй пары скобок: {}", out.blocks[0]);
        assert_eq!(out.metrics.confidence.unknown, 1, "confidence отсутствует → unknown");
    }

    #[test]
    fn клиент_отличает_неразобранный_вывод_от_only_ambiguous() {
        // Вывод есть, но ни одна строка не подошла под regex.
        let (_dir, cfg) = fixture(r#"{"nodes":[],"links":[]}"#, REPORT_STALE);
        let calls: Calls = Arc::default();
        let client = RealGraphify::with_exec(
            cfg,
            fake_exec(&calls, |bin, _| {
                Ok(ExecOutput::stdout(if bin == "git" { "12345678\n" } else { "мусор\n" }))
            }),
        );
        let out = client.build_blast_radius(&["foo".to_string()]).unwrap();
        assert_eq!(out.metrics.skip_reasons.get("graphify_unparsed_output"), Some(&1));
        assert_eq!(out.metrics.symbols_resolved, Some(0), "status=unknown, не resolved");

        // Все зависимости AMBIGUOUS и дропнуты — символ считается resolved.
        let graph = json!({
            "nodes": [
                { "id": "seed", "label": "foo", "source_file": "a.ts" },
                { "id": "amb", "label": "amb()", "source_file": "b.ts" },
            ],
            "links": [{ "source": "amb", "target": "seed", "relation": "uses", "confidence": "AMBIGUOUS", "source_file": "b.ts" }],
        })
        .to_string();
        let (_dir2, cfg2) = fixture(&graph, REPORT_STALE);
        let calls2: Calls = Arc::default();
        let client2 = RealGraphify::with_exec(
            cfg2,
            fake_exec(&calls2, |bin, _| {
                Ok(ExecOutput::stdout(if bin == "git" {
                    "12345678\n"
                } else {
                    "- amb() [uses] b.ts:L1\n"
                }))
            }),
        );
        let out2 = client2.build_blast_radius(&["foo".to_string()]).unwrap();
        assert_eq!(out2.metrics.skip_reasons.get("ambiguous_only"), Some(&1));
        assert_eq!(out2.metrics.symbols_resolved, Some(1));
        assert!(out2.blocks.is_empty());
    }

    #[test]
    fn клиент_поднимает_god_node_из_analysis_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("graph.json"),
            json!({
                "nodes": [
                    { "id": "seed", "label": "God", "source_file": "a.ts" },
                    { "id": "c", "label": "c()", "source_file": "c.ts" },
                ],
                "links": [{ "source": "c", "target": "seed", "relation": "uses", "confidence": "EXTRACTED", "source_file": "c.ts" }],
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".graphify_analysis.json"),
            json!({ "gods": [{ "label": "God" }] }).to_string(),
        )
        .unwrap();
        std::fs::write(dir.path().join("GRAPH_REPORT.md"), REPORT_STALE).unwrap();

        let cfg = GraphifyConfig {
            bin: "gf".to_string(),
            graph: dir.path().join("graph.json").to_string_lossy().into_owned(),
            report: dir.path().join("GRAPH_REPORT.md").to_string_lossy().into_owned(),
            depth: 3,
            drop_ambiguous: true,
            limits: BlastLimits { concurrency: 1, god_node_threshold: 99, max_deps_per_symbol: 20 },
        };
        let calls: Calls = Arc::default();
        let client = RealGraphify::with_exec(
            cfg,
            fake_exec(&calls, |bin, _| {
                Ok(ExecOutput::stdout(if bin == "git" {
                    "12345678\n"
                } else {
                    "- c() [uses] c.ts:L5\n"
                }))
            }),
        );
        let out = client.build_blast_radius(&["God".to_string()]).unwrap();
        assert_eq!(out.blocks.len(), 1);
        assert!(
            out.blocks[0].contains("god-node (высокий риск регрессий)"),
            "порог 99 не достигнут, маркер пришёл из sidecar: {}",
            out.blocks[0]
        );
        // depth уезжает в аргументы как есть.
        assert!(calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c[0] == "gf" && c.windows(2).any(|w| w == ["--depth", "3"])));
    }

    #[test]
    fn клиент_передаёт_символ_аргументом_а_не_флагом() {
        // execFile без shell: символ, начинающийся с `-`, остаётся значением
        // второго позиционного аргумента graphify CLI.
        let (_dir, cfg) = fixture(r#"{"nodes":[],"links":[]}"#, REPORT_STALE);
        let calls: Calls = Arc::default();
        let client = RealGraphify::with_exec(
            cfg,
            fake_exec(&calls, |_, _| Ok(ExecOutput::stdout("No affected nodes found\n"))),
        );
        client.build_blast_radius(&["--rm-rf".to_string()]).unwrap();
        let calls = calls.lock().unwrap();
        let gf = calls.iter().find(|c| c[0] == "gf").expect("вызов graphify");
        assert_eq!(gf[1], "affected");
        assert_eq!(gf[2], "--rm-rf");
        assert_eq!(gf[3], "--graph");
    }

    #[test]
    fn клиент_обрабатывает_символы_параллельно_и_держит_порядок_блоков() {
        let (_dir, cfg) = fixture(r#"{"nodes":[],"links":[]}"#, REPORT_STALE);
        let calls: Calls = Arc::default();
        let client = RealGraphify::with_exec(
            cfg,
            fake_exec(&calls, |bin, args| {
                if bin == "git" {
                    return Ok(ExecOutput::stdout("12345678\n"));
                }
                Ok(ExecOutput::stdout(format!("- c [uses] {}.ts:L1\n", args[1])))
            }),
        );
        let symbols: Vec<String> = (0..6).map(|i| format!("s{i}")).collect();
        let out = client.build_blast_radius(&symbols).unwrap();
        assert_eq!(out.blocks.len(), 6);
        for (i, block) in out.blocks.iter().enumerate() {
            assert!(block.starts_with(&format!("- `s{i}` ")), "порядок символов сохранён: {block}");
        }
        assert_eq!(out.metrics.affected_files.len(), 6);
        assert!(
            out.metrics.affected_files.windows(2).all(|w| w[0] <= w[1]),
            "affected_files отсортированы"
        );
    }

    // -----------------------------------------------------------------------
    // Боевой исполнитель
    // -----------------------------------------------------------------------

    #[test]
    fn default_exec_возвращает_вывод_и_классифицирует_отказы() {
        let opts = ExecOpts { timeout_ms: 5_000, max_buffer: 1 << 20 };

        let ok = default_exec("/bin/echo", &["привет".to_string()], opts).unwrap();
        assert_eq!(ok.stdout, "привет\n");

        let missing = default_exec("нет-такого-бинаря-ai-reviewer", &[], opts).unwrap_err();
        assert!(matches!(missing, ExecError::NotFound { .. }), "{missing:?}");
        assert_eq!(missing.skip_reason(), "graphify_unavailable");

        let failed =
            default_exec("/bin/sh", &["-c".to_string(), "exit 3".to_string()], opts).unwrap_err();
        assert!(matches!(failed, ExecError::Failed { .. }), "{failed:?}");
        assert_eq!(failed.skip_reason(), "graphify_error");
    }

    #[test]
    fn default_exec_убивает_процесс_по_таймауту_и_по_max_buffer() {
        let started = Instant::now();
        let timed_out = default_exec(
            "/bin/sh",
            &["-c".to_string(), "sleep 30".to_string()],
            ExecOpts { timeout_ms: 200, max_buffer: 1 << 20 },
        )
        .unwrap_err();
        assert!(matches!(timed_out, ExecError::Killed { .. }), "{timed_out:?}");
        assert_eq!(timed_out.skip_reason(), "graphify_timeout");
        assert!(started.elapsed() < Duration::from_secs(10), "процесс действительно убит");

        // Переполнение maxBuffer в Node тоже помечает killed → graphify_timeout.
        let overflow = default_exec(
            "/bin/sh",
            &["-c".to_string(), "yes ai-reviewer".to_string()],
            ExecOpts { timeout_ms: 10_000, max_buffer: 4 * 1024 },
        )
        .unwrap_err();
        assert!(matches!(overflow, ExecError::Killed { .. }), "{overflow:?}");
    }
}
