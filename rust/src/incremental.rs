//! Порт `lib/incremental.mjs` (RFC 05, задача T39 RFC 20): инкрементальный прогон —
//! не ревьюить заново то, что не менялось с прошлого прогона.
//!
//! Единица сравнения — **вход модели по файлу** (секция диффа + приложенный контекст +
//! вход карточек символов), а не SHA коммита: перебазирование меняет SHA, не меняя входа.
//! Совпал хэш — модель увидела бы то же самое, вызов можно не делать, а находку взять
//! из прошлого прогона.
//!
//! Состояние лежит своим файлом (`paths.incrementalState`), а не в футере sticky и не в
//! артефакте экспорта: футер урезан и это контракт с потребителем, экспорт версионирован
//! и читается снаружи.
//!
//! ## Порядок переноса — часть API
//!
//! Перенесённые находки подмешиваются **после** refutation и verifier: обе стадии они
//! прошли в своём прогоне и несут вердикты с собой. Пропусти их ещё раз — strict-refutation
//! понизит подтверждённый блокер до minor только потому, что жюри в этом прогоне по нему не
//! высказывалось. Поэтому перенос выдаётся не голым `Vec`, а [`CarryOver`]: наружу он
//! отдаёт только срез для чтения (лог, observability), а в набор находок попадает
//! единственным путём — [`CarryOver::append_after_verifier`], который поглощает себя и
//! принимает уже прошедший verifier набор. Прогнать перенесённое через refutation можно
//! только явным `clone()` — это видно на ревью.
//!
//! Хэши (`file_input_hashes`, `config_fingerprint`) совпадают с JS байт в байт: состояние,
//! записанное JS-движком, читается Rust-движком без сброса в полный прогон.

use std::path::Path;

use indexmap::{IndexMap, IndexSet};
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::actionability::BLAST_FINDING_SOURCE;
use crate::findings::{js_to_number, js_to_string, js_truthy};
use crate::report::js_json_stringify;
use crate::structure_evidence::js_number_to_string;
use crate::util::sha256_str;

/// v2: хэш входа без NUL; отпечаток включает пороги actionability и таймаут LLM.
pub const INCREMENTAL_STATE_VERSION: u32 = 2;

/// `sha256(s).slice(0, 32)` из JS-модуля.
fn sha256_32(s: &str) -> String {
    let mut h = sha256_str(s);
    h.truncate(32);
    h
}

fn stringify(v: &Value) -> String {
    let mut out = String::new();
    js_json_stringify(v, &mut out);
    out
}

/// `a?.b?.c` с семантикой JS: `None` — undefined (ключа нет или по пути не объект),
/// `Some(Null)` — явный null.
fn get_path<'a>(v: Option<&'a Value>, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v?;
    for key in path {
        cur = cur.as_object()?.get(*key)?;
    }
    Some(cur)
}

/// Элемент массива в `JSON.stringify`: undefined → null.
fn arr_item(v: Option<&Value>) -> Value {
    v.cloned().unwrap_or(Value::Null)
}

/// `a ?? b`: undefined и null уступают.
fn nullish(a: Option<&Value>) -> Option<&Value> {
    a.filter(|v| !v.is_null())
}

/// Число как в JSON.stringify JS: целое — без `.0`.
fn num_json(x: f64) -> Value {
    if !x.is_finite() {
        return Value::Null;
    }
    if x == x.trunc() && x.abs() < 9.0e15 {
        json!(x as i64)
    } else {
        json!(x)
    }
}

// ---------------------------------------------------------------------------
// Отпечатки
// ---------------------------------------------------------------------------

/// `fileInputHashes`: отпечаток входа модели по каждому файлу.
///
/// Дифф и контекст — разные ключи JSON, а не конкатенация: иначе правка контекста,
/// «склеившаяся» с диффом, осталась бы незамеченной. `cards` — вход карточек символов
/// (`symbol_cards_hash_input`, без номеров строк); пустой или отсутствующий вход — ключа
/// `cards` в хэше нет вовсе, и хэш совпадает с прогоном без фичи байт в байт.
pub fn file_input_hashes(
    changed_paths: &[String],
    sections: &IndexMap<String, String>,
    content_map: &IndexMap<String, String>,
    cards: Option<&IndexMap<String, String>>,
) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for path in changed_paths {
        let diff = sections.get(path).map(String::as_str).unwrap_or("");
        let context = content_map.get(path).map(String::as_str).unwrap_or("");
        let file_cards = cards.and_then(|c| c.get(path)).filter(|s| !s.is_empty());
        let input = match file_cards {
            Some(c) => json!({ "diff": diff, "context": context, "cards": c }),
            None => json!({ "diff": diff, "context": context }),
        };
        out.insert(path.clone(), sha256_32(&stringify(&input)));
    }
    out
}

/// `configFingerprint`: отпечаток конфигурации, влияющей на результат. Сменились модели,
/// роли, раскладка промпта, нарезка, пороги или таймаут — прогон обязан быть полным.
///
/// `cfg` — замороженный конфиг прогона с `version` (= `profile.version`) на верхнем
/// уровне, как его собирает пайплайн: `{ ...cfg, version: cfg.profile?.version }`.
pub fn config_fingerprint(cfg: &Value) -> String {
    let c = Some(cfg);
    let mut o = Map::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            o.insert(k.into(), v);
        }
    };
    put(
        "version",
        Some(nullish(get_path(c, &["version"])).cloned().unwrap_or_else(|| json!(""))),
    );
    put("reviewMode", get_path(c, &["reviewMode"]).cloned());
    put("promptLayout", get_path(c, &["promptLayout"]).cloned());
    put("aggregator", get_path(c, &["moa", "aggregator"]).cloned());
    put("references", get_path(c, &["moa", "references"]).cloned());
    // `assignments?.map(a => `${a.role}:${a.model}`)`; не-массив в JS бросил бы TypeError —
    // здесь он трактуется как отсутствие (fail-open, а не падение прогона).
    let specialists = get_path(c, &["specialists", "assignments"]).and_then(Value::as_array).map(|list| {
        Value::Array(
            list.iter()
                .map(|a| {
                    let part = |k: &str| match a.as_object().and_then(|m| m.get(k)) {
                        Some(v) => js_to_string(v),
                        None => "undefined".to_string(),
                    };
                    Value::String(format!("{}:{}", part("role"), part("model")))
                })
                .collect(),
        )
    });
    put("specialists", specialists);
    put("merge", get_path(c, &["specialists", "merge"]).cloned());
    put("models", get_path(c, &["models", "list"]).cloned());
    let mut verifier: Vec<Value> = ["enabled", "model", "strictBlocks", "contextMargin"]
        .iter()
        .map(|k| arr_item(get_path(c, &["verifier", k])))
        .collect();
    // Карточки verifier'у (RFC 14) — только при включении: выключенный флаг отпечаток не меняет.
    if get_path(c, &["verifier", "symbolCards"]).is_some_and(js_truthy) {
        verifier.push(json!("symbolCards"));
    }
    put("verifier", Some(Value::Array(verifier)));
    let arr = |section: &str, keys: &[&str]| -> Option<Value> {
        Some(Value::Array(keys.iter().map(|k| arr_item(get_path(c, &[section, k]))).collect()))
    };
    put("gate", arr("gate", &["blockOn", "consensusMin", "confidenceMin", "strict"]));
    put("actionability", arr("actionability", &["threshold", "verifyMin", "verifyAdvisory"]));
    put("timeoutMs", get_path(c, &["ollama", "requestTimeoutMs"]).cloned());
    put(
        "limits",
        arr("limits", &["chunkBytes", "maxFileLines", "maxContextBytes", "skipAddedFileContext", "chunkRouting"]),
    );
    let profile = nullish(get_path(c, &["profile", "name"]))
        .or_else(|| nullish(get_path(c, &["profile", "source"])))
        .cloned()
        .unwrap_or_else(|| json!(""));
    put("profile", Some(profile));
    // Лимиты карточек (RFC 14) — ключ только при включённой фиче: выключенная не меняет
    // отпечаток, и обновление движка не сбрасывает состояние у тех, кто её не включал.
    if get_path(c, &["limits", "symbolCards"]).is_some_and(js_truthy) {
        put("symbolCards", arr("limits", &["symbolCardsMax", "symbolCardsMaxChars", "symbolCardsBodyMaxLines"]));
    }
    sha256_32(&stringify(&Value::Object(o)))
}

// ---------------------------------------------------------------------------
// Состояние
// ---------------------------------------------------------------------------

/// `readIncrementalState`: состояние прошлого прогона. Любая проблема (нет файла, битый
/// JSON, чужая версия) — `None`: инкрементальность не критична, прогон будет полным.
pub fn read_incremental_state(file: Option<&Path>) -> Option<Value> {
    let file = file.filter(|p| !p.as_os_str().is_empty())?;
    let bytes = std::fs::read(file).ok()?;
    // readFile(…, 'utf8') в Node заменяет битые последовательности на U+FFFD, а не падает.
    let text = String::from_utf8_lossy(&bytes);
    let state: Value = serde_json::from_str(&text).ok()?;
    let version_ok = state
        .as_object()
        .and_then(|m| m.get("version"))
        .and_then(Value::as_f64)
        .is_some_and(|v| v == f64::from(INCREMENTAL_STATE_VERSION));
    version_ok.then_some(state)
}

/// `writeIncrementalState`: пишет `{ ...state, version }` одной строкой JSON. Fail-open:
/// не смогли — лог и `false`, следующий прогон просто будет полным.
pub fn write_incremental_state(file: Option<&Path>, state: &Value, log: &mut dyn FnMut(&str)) -> bool {
    let Some(file) = file.filter(|p| !p.as_os_str().is_empty()) else {
        return false;
    };
    let mut obj = state.as_object().cloned().unwrap_or_default();
    // Как у spread: существующий ключ остаётся на своём месте, новый — в конце.
    obj.insert("version".into(), json!(INCREMENTAL_STATE_VERSION));
    let mut text = stringify(&Value::Object(obj));
    text.push('\n');
    let res = (|| -> std::io::Result<()> {
        if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(file, text)
    })();
    match res {
        Ok(()) => true,
        Err(e) => {
            log(&format!("инкрементальное состояние не сохранено (следующий прогон будет полным): {e}"));
            false
        }
    }
}

// ---------------------------------------------------------------------------
// План
// ---------------------------------------------------------------------------

/// Режим прогона.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IncrementalMode {
    /// Состояние неприменимо — ревьюим всё.
    Full,
    /// Ревьюим `review_paths`, `carried` переносим из прошлого прогона.
    Incremental,
}

/// Решение `planIncremental`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IncrementalPlan {
    pub mode: IncrementalMode,
    /// Файлы, которые ревьюятся в этом прогоне (в полном — все пути PR).
    pub review_paths: IndexSet<String>,
    /// Находки прошлого прогона по неизменившимся файлам — ещё без пометки переноса;
    /// в набор находок они попадают только через [`CarryOver`].
    pub carried: Vec<Value>,
    pub unchanged: Vec<String>,
    pub reason: String,
}

impl IncrementalPlan {
    fn full(all: IndexSet<String>, reason: impl Into<String>) -> Self {
        Self { mode: IncrementalMode::Full, review_paths: all, carried: Vec::new(), unchanged: Vec::new(), reason: reason.into() }
    }

    /// План при выключенной инкрементальности (`REVIEW_INCREMENTAL=0`).
    pub fn disabled(changed_paths: &[String]) -> Self {
        Self::full(changed_paths.iter().cloned().collect(), "выключено")
    }

    pub fn is_incremental(&self) -> bool {
        self.mode == IncrementalMode::Incremental
    }

    /// Попадает ли файл в вызов модели этого прогона. Дифф и контекст для LLM сужаются
    /// по нему; карта PR, отпечатки, гейт и Δ-блок остаются на полном наборе путей.
    pub fn reviews(&self, path: &str) -> bool {
        !self.is_incremental() || self.review_paths.contains(path)
    }
}

/// Вход `planIncremental`.
#[derive(Debug, Clone, Copy)]
pub struct PlanInput<'a> {
    pub state: Option<&'a Value>,
    pub hashes: &'a IndexMap<String, String>,
    pub changed_paths: &'a [String],
    pub config_fp: &'a str,
    /// `cfg.incremental.fullEvery` (дефолт JS — 10); `<= 0` или NaN — периодического
    /// полного прогона нет.
    pub full_every: f64,
}

/// `planIncremental`: что ревьюить в этом прогоне.
///
/// Переносятся только находки по файлам, которые (а) остались в PR и (б) не изменились.
/// Находка по изменённому файлу не переносится никогда: её место в коде могло уехать.
pub fn plan_incremental(input: PlanInput<'_>) -> IncrementalPlan {
    let all: IndexSet<String> = input.changed_paths.iter().cloned().collect();
    let Some(state) = input.state.filter(|s| js_truthy(s)) else {
        return IncrementalPlan::full(all, "нет состояния прошлого прогона");
    };
    let field = |k: &str| state.as_object().and_then(|m| m.get(k));
    if field("configFp").and_then(Value::as_str) != Some(input.config_fp) {
        return IncrementalPlan::full(all, "изменилась конфигурация прогона (модели/роли/лимиты)");
    }
    // Number(state.sinceFull || 0) + 1 >= fullEvery; NaN сравнение не проходит.
    let since_full = match field("sinceFull") {
        Some(v) if js_truthy(v) => js_to_number(v),
        _ => Some(0.0),
    };
    if input.full_every > 0.0 && since_full.is_some_and(|n| n + 1.0 >= input.full_every) {
        return IncrementalPlan::full(
            all,
            format!(
                "каждый {}-й прогон полный (защита от накопления дрейфа)",
                js_number_to_string(input.full_every)
            ),
        );
    }

    let prev_hashes = field("hashes").and_then(Value::as_object);
    // Файл, которого в прошлом прогоне не было, — новый для нас: его нужно ревьюить.
    let unchanged: Vec<String> = all
        .iter()
        .filter(|p| {
            let prev = prev_hashes.and_then(|m| m.get(p.as_str()));
            match (prev, input.hashes.get(p.as_str())) {
                (Some(Value::String(a)), Some(b)) => !a.is_empty() && a == b,
                _ => false,
            }
        })
        .cloned()
        .collect();
    if unchanged.is_empty() {
        return IncrementalPlan::full(all, "изменились все файлы PR");
    }

    let unchanged_set: IndexSet<&str> = unchanged.iter().map(String::as_str).collect();
    let review_paths: IndexSet<String> = all.iter().filter(|p| !unchanged_set.contains(p.as_str())).cloned().collect();
    // `state.findings || []`: не-массив в JS бросил бы на .filter — здесь пусто (fail-open).
    let carried: Vec<Value> = field("findings")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter(|f| {
                    f.as_object()
                        .and_then(|m| m.get("file"))
                        .and_then(Value::as_str)
                        .is_some_and(|file| !file.is_empty() && unchanged_set.contains(file))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let reason = format!("{} из {} файл(ов) не изменились с прошлого прогона", unchanged.len(), all.len());
    IncrementalPlan { mode: IncrementalMode::Incremental, review_paths, carried, unchanged, reason }
}

// ---------------------------------------------------------------------------
// Перенос
// ---------------------------------------------------------------------------

/// `markCarried`: пометка говорит пайплайну, что находка УЖЕ прошла жюри и verifier в
/// прошлом прогоне (статусы при ней) — её нельзя ни понижать как «без advisorClaims»,
/// ни проверять повторно.
pub fn mark_carried(findings: &[Value], from_sha: &str) -> Vec<Value> {
    findings
        .iter()
        .map(|f| {
            let mut m = f.as_object().cloned().unwrap_or_default();
            m.insert("carriedOver".into(), json!(true));
            if !from_sha.is_empty() {
                m.insert("carriedFromSha".into(), json!(from_sha));
            }
            Value::Object(m)
        })
        .collect()
}

/// Перенесённые находки прогона. Единственный путь в набор находок —
/// [`CarryOver::append_after_verifier`]: порядок «refutation → verifier → перенос»
/// закреплён типом, а не комментарием.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CarryOver {
    findings: Vec<Value>,
}

impl CarryOver {
    /// Помеченные находки плана; при полном прогоне — пусто. `from_sha` — `headSha`
    /// прошлого прогона из состояния.
    pub fn from_plan(plan: &IncrementalPlan, from_sha: &str) -> Self {
        if !plan.is_incremental() {
            return Self::default();
        }
        Self { findings: mark_carried(&plan.carried, from_sha) }
    }

    pub fn len(&self) -> usize {
        self.findings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }

    /// Только чтение — для лога и observability.
    pub fn findings(&self) -> &[Value] {
        &self.findings
    }

    /// Подмешивает перенесённые находки к набору, уже прошедшему refutation и verifier
    /// ЭТОГО прогона. Перенесённое идёт в конец — как в JS (`[...current, ...carried]`).
    pub fn append_after_verifier(self, mut after_verifier: Vec<Value>) -> Vec<Value> {
        after_verifier.extend(self.findings);
        after_verifier
    }
}

/// Находки для состояния следующего прогона: итоговый набор без prescan и blast-radius —
/// те детерминированы, дёшевы и пересчитываются каждый прогон.
pub fn carryable_findings(all: &[Value]) -> Vec<Value> {
    all.iter()
        .filter(|f| {
            let m = f.as_object();
            let prescan = m.and_then(|m| m.get("prescan")).is_some_and(js_truthy);
            let blast = m.and_then(|m| m.get("source")).and_then(Value::as_str) == Some(BLAST_FINDING_SOURCE);
            !prescan && !blast
        })
        .cloned()
        .collect()
}

/// Вход карточек, переиспользованный из прошлого прогона, когда граф недоступен.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReusedCardsInput {
    pub cards_input: IndexMap<String, String>,
    pub cards_targets: Map<String, Value>,
}

/// Переиспользование входа карточек при недоступном графе (аудит RFC 14, п. 1.9).
/// Вызывать, только когда фича карточек включена, а граф не построен.
///
/// Сужено, чтобы не перенести находку мимо реального изменения: файл, чья карточка
/// указывает на файл ЭТОГО PR, не переиспользуется; и не больше одного прогона подряд
/// (`cardsInputReused` в прошлом состоянии — `None`).
pub fn reuse_prior_cards_input(prior: Option<&Value>, changed_paths: &[String]) -> Option<ReusedCardsInput> {
    let prior = prior?.as_object()?;
    let cards_input = prior.get("cardsInput").filter(|v| js_truthy(v))?;
    if prior.get("cardsInputReused").is_some_and(js_truthy) {
        return None;
    }
    let in_pr: IndexSet<&str> = changed_paths.iter().map(String::as_str).collect();
    let empty = Map::new();
    let targets = prior.get("cardsTargets").and_then(Value::as_object).unwrap_or(&empty);
    let mut out = ReusedCardsInput::default();
    for (path, input) in cards_input.as_object()? {
        let Some(input) = input.as_str() else { continue };
        let Some(t) = targets.get(path).and_then(Value::as_array) else { continue };
        if !in_pr.contains(path.as_str()) || t.iter().any(|x| x.as_str().is_some_and(|s| in_pr.contains(s))) {
            continue;
        }
        out.cards_input.insert(path.clone(), input.to_string());
        out.cards_targets.insert(path.clone(), Value::Array(t.clone()));
    }
    Some(out)
}

/// Вход состояния следующего прогона (`pipeline.mjs`, запись после гейта).
#[derive(Debug, Clone, Copy)]
pub struct NextStateInput<'a> {
    pub pr_number: &'a Value,
    pub head_sha: &'a str,
    pub config_fp: &'a str,
    pub plan: &'a IncrementalPlan,
    pub prior: Option<&'a Value>,
    pub hashes: &'a IndexMap<String, String>,
    pub cards_input: Option<&'a IndexMap<String, String>>,
    pub cards_targets: Option<&'a Map<String, Value>>,
    pub cards_input_reused: usize,
    /// Итоговый набор находок прогона (виден человеку); фильтр [`carryable_findings`]
    /// применяется здесь.
    pub findings: &'a [Value],
    /// Файлы, которые следующий прогон обязан ревьюить заново (RFC 18/19): с находками,
    /// до которых verifier не дошёл (`verifierStatus` unavailable/overflow), и файлы
    /// чанков, не отревьюенных ни одной моделью. Их находки не переносятся, а хэши не
    /// пишутся — иначе перенос считал бы непроверенное проверенным.
    pub unverified_paths: Option<&'a IndexSet<String>>,
}

/// Файлы, которые следующий прогон обязан ревьюить заново: находки с
/// `verifierStatus` unavailable/overflow плюс `uncovered` — файлы непокрытых чанков.
/// Порядок — как у `new Set([...findings, ...uncovered])` в JS.
pub fn unverified_paths(findings: &[Value], uncovered: &IndexSet<String>) -> IndexSet<String> {
    let mut out: IndexSet<String> = findings
        .iter()
        .filter(|f| matches!(f.get("verifierStatus").and_then(Value::as_str), Some("unavailable" | "overflow")))
        .filter_map(|f| f.get("file").and_then(Value::as_str).map(str::to_string))
        .collect();
    out.extend(uncovered.iter().cloned());
    out
}

/// Состояние для следующего прогона.
pub fn build_next_state(input: NextStateInput<'_>) -> Value {
    let since_full = if input.plan.is_incremental() {
        let prev = input.prior.and_then(|p| p.as_object()).and_then(|m| m.get("sinceFull"));
        let n = match prev {
            Some(v) if js_truthy(v) => js_to_number(v),
            _ => Some(0.0),
        };
        n.map(|n| num_json(n + 1.0)).unwrap_or(Value::Null)
    } else {
        json!(0)
    };
    let mut o = Map::new();
    o.insert("pr".into(), input.pr_number.clone());
    o.insert("headSha".into(), json!(input.head_sha));
    o.insert("configFp".into(), json!(input.config_fp));
    o.insert("sinceFull".into(), since_full);
    let skip = input.unverified_paths.filter(|u| !u.is_empty());
    match skip {
        Some(u) => {
            let kept: IndexMap<&String, &String> = input.hashes.iter().filter(|(p, _)| !u.contains(p.as_str())).collect();
            o.insert("hashes".into(), json!(kept));
        }
        None => {
            o.insert("hashes".into(), json!(input.hashes));
        }
    }
    if let Some(ci) = input.cards_input.filter(|c| !c.is_empty()) {
        o.insert("cardsInput".into(), json!(ci));
        o.insert("cardsTargets".into(), input.cards_targets.map(|t| Value::Object(t.clone())).unwrap_or(Value::Null));
    }
    if input.cards_input_reused > 0 {
        o.insert("cardsInputReused".into(), json!(true));
    }
    let mut carry = carryable_findings(input.findings);
    if let Some(u) = skip {
        carry.retain(|f| !f.get("file").and_then(Value::as_str).is_some_and(|p| u.contains(p)));
    }
    o.insert("findings".into(), Value::Array(carry));
    Value::Object(o)
}

/// `formatIncrementalNote`: строка для sticky и лога; при полном прогоне — пусто.
pub fn format_incremental_note(plan: Option<&IncrementalPlan>) -> String {
    match plan {
        Some(p) if p.is_incremental() => format!(
            "♻️ Инкрементальный прогон: {} — по ним перенесены находки прошлого прогона ({}), заново ревьюились только изменившиеся.",
            p.reason,
            p.carried.len()
        ),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_review_config_parts, normalize_profile_facts};
    use crate::context_window::window_chunk_file_contents;
    use crate::specialists::apply_refutation;

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn smap(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// Аналог `fingerprint(extraEnv)` из test/incremental.test.mjs: конфиг из env, `version`
    /// и `profile` — как их кладёт пайплайн.
    fn fingerprint(extra: &[(&str, &str)]) -> String {
        let profile = normalize_profile_facts(&json!({}), "/pkg", "(default)");
        let bits = json!({ "strict": false, "appDir": null, "paths": profile.get("paths") });
        let mut env: Map<String, Value> = [
            ("OLLAMA_API_KEY", "k"),
            ("GITHUB_TOKEN", "t"),
            ("REVIEW_OBSERVABILITY", "0"),
            ("REVIEW_EXPORT", "0"),
            ("REVIEW_AUTO_TRIAGE", "0"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), json!(v)))
        .collect();
        for (k, v) in extra {
            env.insert(k.to_string(), json!(v));
        }
        let mut cfg = load_review_config_parts(&env, &bits);
        cfg["profile"] = json!({ "name": "reference", "version": "1" });
        cfg["version"] = json!("1");
        config_fingerprint(&cfg)
    }

    fn state(hashes: Value, findings: Value) -> Value {
        json!({ "version": 2, "configFp": "fp", "sinceFull": 0, "headSha": "abc", "hashes": hashes, "findings": findings })
    }

    #[test]
    fn исходник_без_nul() {
        assert!(!include_bytes!("incremental.rs").contains(&0u8), "NUL прячет файл в GitHub-диффе");
    }

    #[test]
    fn file_input_hashes_совпадают_с_js() {
        // node: fileInputHashes(['a.ts','b/ü.md','c.ts'], …) — см. lib/incremental.mjs
        let h = file_input_hashes(
            &paths(&["a.ts", "b/ü.md", "c.ts"]),
            &smap(&[("a.ts", "DIFF \"q\"\n\t"), ("b/ü.md", "+текст")]),
            &smap(&[("a.ts", "CTX")]),
            Some(&smap(&[("a.ts", "x\ny"), ("c.ts", "")])),
        );
        assert_eq!(h["a.ts"], "5dec478ea2d9b67de0970881a9bf4d5d");
        assert_eq!(h["b/ü.md"], "2022b72062a49ffbc6c7c988f5f14bcd");
        // пустой вход карточек — ключа cards нет, хэш как без фичи
        assert_eq!(h["c.ts"], "9d316fef03f492b1340c96b342955c5f");
        assert_eq!(
            file_input_hashes(&paths(&["c.ts"]), &IndexMap::new(), &IndexMap::new(), None)["c.ts"],
            h["c.ts"]
        );
    }

    #[test]
    fn config_fingerprint_совпадает_с_js() {
        assert_eq!(config_fingerprint(&json!({})), "fad4a6ed8278533dccd53bca8f38d2a0");
        let cfg = json!({
            "version": "0.4.0", "reviewMode": "moa", "promptLayout": "cache",
            "moa": { "aggregator": "agg", "references": ["r1", "r2"] },
            "specialists": { "assignments": [{ "role": "security", "model": "m1" }, { "role": "x" }], "merge": true },
            "models": { "list": ["moa"] },
            "verifier": { "enabled": true, "model": "v", "strictBlocks": false, "contextMargin": 40, "symbolCards": true },
            "gate": { "blockOn": ["critical", "major"], "consensusMin": 2, "confidenceMin": 0.8, "strict": true },
            "actionability": { "threshold": 3, "verifyMin": 1.5 },
            "ollama": { "requestTimeoutMs": 120000 },
            "limits": { "chunkBytes": 1e5, "maxFileLines": 2000, "maxContextBytes": 300000,
                "skipAddedFileContext": true, "chunkRouting": false, "symbolCards": true, "symbolCardsMax": 8 },
            "profile": { "source": "profiles/orakul.json" }
        });
        assert_eq!(config_fingerprint(&cfg), "9d8b609eb3f3899bf3db669af51a265b");
    }

    #[test]
    fn read_incremental_state_отбрасывает_v1_и_мусор() {
        assert_eq!(INCREMENTAL_STATE_VERSION, 2);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        std::fs::write(&file, r#"{"version":1,"hashes":{"a.ts":"x"},"findings":[{"file":"a.ts"}]}"#).unwrap();
        assert_eq!(read_incremental_state(Some(&file)), None);
        std::fs::write(&file, "{битый").unwrap();
        assert_eq!(read_incremental_state(Some(&file)), None);
        assert_eq!(read_incremental_state(Some(&dir.path().join("нет.json"))), None);
        assert_eq!(read_incremental_state(None), None);
    }

    #[test]
    fn write_затем_read_даёт_то_же_состояние() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deep/incremental/state.json");
        let st = json!({ "pr": 7, "version": 99, "hashes": { "a.ts": "h" }, "findings": [] });
        let mut logs = Vec::new();
        assert!(write_incremental_state(Some(&file), &st, &mut |m: &str| logs.push(m.to_string())));
        let text = std::fs::read_to_string(&file).unwrap();
        // version перезаписан на своём месте (как у spread), строка JSON + перевод строки
        assert_eq!(text, "{\"pr\":7,\"version\":2,\"hashes\":{\"a.ts\":\"h\"},\"findings\":[]}\n");
        assert_eq!(read_incremental_state(Some(&file)).unwrap()["pr"], json!(7));
        assert!(logs.is_empty());
    }

    #[test]
    fn write_fail_open() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let mut logs = Vec::new();
        let ok = write_incremental_state(Some(&blocker.join("state.json")), &json!({}), &mut |m: &str| {
            logs.push(m.to_string())
        });
        assert!(!ok);
        assert!(logs[0].starts_with("инкрементальное состояние не сохранено"));
        assert!(!write_incremental_state(None, &json!({}), &mut |_: &str| {}));
    }

    #[test]
    fn file_input_hashes_дифф_и_контекст_разные_слагаемые() {
        let p = paths(&["a.ts"]);
        let a = file_input_hashes(&p, &smap(&[("a.ts", "DIFF")]), &smap(&[("a.ts", "CTX")]), None);
        let glued = file_input_hashes(&p, &smap(&[("a.ts", "DIFFCTX")]), &smap(&[("a.ts", "")]), None);
        let swapped = file_input_hashes(&p, &smap(&[("a.ts", "CTX")]), &smap(&[("a.ts", "DIFF")]), None);
        assert_ne!(a["a.ts"], glued["a.ts"]);
        assert_ne!(a["a.ts"], swapped["a.ts"]);
    }

    #[test]
    fn file_input_hashes_по_оконному_контексту() {
        // RFC 07: окна ставятся ДО хэша — смена margin инвалидирует файл сама, а margin=0
        // (откат) даёт ровно тот же отпечаток, что и до окон.
        let body: Vec<String> = (1..=300).map(|i| format!("строка {i}")).collect();
        let content = smap(&[("a.ts", &format!("### a.ts\n```\n{}\n```", body.join("\n")))]);
        let files: Map<String, Value> = [("a.ts".to_string(), json!({ "validLines": [150] }))].into_iter().collect();
        let sections = smap(&[("a.ts", "DIFF")]);
        let p = paths(&["a.ts"]);
        let h = |margin: f64| {
            let w = window_chunk_file_contents(&content, Some(&files), margin, 0.0);
            file_input_hashes(&p, &sections, &w.map, None)["a.ts"].clone()
        };
        assert_ne!(h(40.0), h(60.0));
        assert_eq!(h(0.0), file_input_hashes(&p, &sections, &content, None)["a.ts"]);
    }

    #[test]
    fn config_fingerprint_пороги_и_таймаут_сбрасывают() {
        let base = fingerprint(&[]);
        assert_ne!(fingerprint(&[("REVIEW_VERIFY_ADVISORY_MIN", "99")]), base);
        assert_ne!(fingerprint(&[("REVIEW_ACTIONABILITY_MIN", "9")]), base);
        assert_ne!(fingerprint(&[("REVIEW_LLM_TIMEOUT_MS", "60000")]), base);
    }

    #[test]
    fn config_fingerprint_ollama_models_в_moa_не_сбрасывает() {
        assert_eq!(fingerprint(&[]), fingerprint(&[("OLLAMA_MODELS", "other-a,other-b")]));
    }

    #[test]
    fn config_fingerprint_смена_агрегатора_сбрасывает() {
        assert_ne!(fingerprint(&[]), fingerprint(&[("MOA_AGGREGATOR", "other-agg")]));
    }

    #[test]
    fn config_fingerprint_карточки_только_при_включении() {
        let off = json!({ "limits": { "symbolCards": false, "symbolCardsMax": 8 }, "verifier": { "symbolCards": false } });
        assert_eq!(config_fingerprint(&off), config_fingerprint(&json!({ "verifier": {} })));
        let on = json!({ "limits": { "symbolCards": true, "symbolCardsMax": 8 } });
        assert_ne!(config_fingerprint(&on), config_fingerprint(&off));
    }

    #[test]
    fn plan_полный_без_состояния_и_при_смене_конфига() {
        let h = smap(&[("a.ts", "h1")]);
        let p = paths(&["a.ts"]);
        let plan = plan_incremental(PlanInput { state: None, hashes: &h, changed_paths: &p, config_fp: "fp", full_every: 10.0 });
        assert_eq!(plan.mode, IncrementalMode::Full);
        assert_eq!(plan.reason, "нет состояния прошлого прогона");
        assert!(plan.reviews("a.ts"));
        let st = state(json!({ "a.ts": "h1" }), json!([]));
        let plan = plan_incremental(PlanInput { state: Some(&st), hashes: &h, changed_paths: &p, config_fp: "other", full_every: 10.0 });
        assert_eq!(plan.reason, "изменилась конфигурация прогона (модели/роли/лимиты)");
    }

    #[test]
    fn plan_каждый_n_й_полный() {
        let h = smap(&[("a.ts", "h1"), ("b.ts", "h2")]);
        let p = paths(&["a.ts", "b.ts"]);
        let mut st = state(json!({ "a.ts": "h1" }), json!([]));
        st["sinceFull"] = json!(9);
        let plan = plan_incremental(PlanInput { state: Some(&st), hashes: &h, changed_paths: &p, config_fp: "fp", full_every: 10.0 });
        assert_eq!(plan.reason, "каждый 10-й прогон полный (защита от накопления дрейфа)");
        // fullEvery=0 — периодического полного нет
        let plan = plan_incremental(PlanInput { state: Some(&st), hashes: &h, changed_paths: &p, config_fp: "fp", full_every: 0.0 });
        assert!(plan.is_incremental());
        // нечисловой sinceFull — NaN, сравнение не проходит
        st["sinceFull"] = json!("abc");
        let plan = plan_incremental(PlanInput { state: Some(&st), hashes: &h, changed_paths: &p, config_fp: "fp", full_every: 2.0 });
        assert!(plan.is_incremental());
    }

    #[test]
    fn plan_переносит_только_по_неизменившимся_файлам() {
        let h = smap(&[("a.ts", "h1"), ("b.ts", "NEW"), ("c.ts", "h3")]);
        let p = paths(&["a.ts", "b.ts", "c.ts"]);
        let st = state(
            json!({ "a.ts": "h1", "b.ts": "h2", "gone.ts": "h9" }),
            json!([
                { "file": "a.ts", "line": 1, "severity": "critical" },
                { "file": "b.ts", "line": 2 },
                { "file": "gone.ts", "line": 3 },
                { "line": 4 },
                null
            ]),
        );
        let plan = plan_incremental(PlanInput { state: Some(&st), hashes: &h, changed_paths: &p, config_fp: "fp", full_every: 10.0 });
        assert!(plan.is_incremental());
        assert_eq!(plan.unchanged, paths(&["a.ts"]));
        assert_eq!(plan.review_paths.iter().cloned().collect::<Vec<_>>(), paths(&["b.ts", "c.ts"]));
        assert_eq!(plan.carried, vec![json!({ "file": "a.ts", "line": 1, "severity": "critical" })]);
        assert_eq!(plan.reason, "1 из 3 файл(ов) не изменились с прошлого прогона");
        assert!(!plan.reviews("a.ts") && plan.reviews("b.ts"));
        assert_eq!(
            format_incremental_note(Some(&plan)),
            "♻️ Инкрементальный прогон: 1 из 3 файл(ов) не изменились с прошлого прогона — по ним перенесены находки прошлого прогона (1), заново ревьюились только изменившиеся."
        );
        assert_eq!(format_incremental_note(Some(&IncrementalPlan::disabled(&p))), "");
    }

    #[test]
    fn plan_все_изменились_и_битые_поля() {
        let h = smap(&[("a.ts", "h1")]);
        let p = paths(&["a.ts"]);
        let st = state(json!({ "a.ts": "old" }), json!([]));
        let plan = plan_incremental(PlanInput { state: Some(&st), hashes: &h, changed_paths: &p, config_fp: "fp", full_every: 10.0 });
        assert_eq!(plan.reason, "изменились все файлы PR");
        // findings не массив — JS бросил бы TypeError; порт переносит пустоту
        let st = state(json!({ "a.ts": "h1" }), json!({ "file": "a.ts" }));
        let plan = plan_incremental(PlanInput { state: Some(&st), hashes: &h, changed_paths: &p, config_fp: "fp", full_every: 10.0 });
        assert!(plan.is_incremental() && plan.carried.is_empty());
    }

    #[test]
    fn перенос_после_refutation_и_verifier() {
        // Перенесённый блокер без advisorClaims: прогони его через strict-refutation этого
        // прогона — он стал бы minor. CarryOver подмешивает его после стадий, нетронутым.
        let h = smap(&[("a.ts", "h1"), ("b.ts", "NEW")]);
        let p = paths(&["a.ts", "b.ts"]);
        let blocker = json!({ "file": "a.ts", "line": 1, "severity": "critical", "verifierVerdict": "confirmed" });
        let st = state(json!({ "a.ts": "h1" }), json!([blocker.clone()]));
        let plan = plan_incremental(PlanInput { state: Some(&st), hashes: &h, changed_paths: &p, config_fp: "fp", full_every: 10.0 });
        let carry = CarryOver::from_plan(&plan, "abc");
        assert_eq!(carry.len(), 1);
        assert_eq!(carry.findings()[0]["carriedOver"], json!(true));
        assert_eq!(carry.findings()[0]["carriedFromSha"], json!("abc"));

        let block_on = [json!("critical"), json!("major")];
        let wrong = apply_refutation(carry.findings(), true, &block_on);
        assert!(
            wrong.mutations.iter().any(|m| m.severity == Some(json!("minor"))),
            "повторная refutation понизила бы перенесённый блокер — поэтому перенос идёт после неё"
        );

        let fresh = vec![json!({ "file": "b.ts", "line": 5, "severity": "minor" })];
        let out = carry.append_after_verifier(fresh.clone());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], fresh[0]);
        assert_eq!(out[1]["severity"], json!("critical"));
        assert_eq!(out[1]["verifierVerdict"], json!("confirmed"));

        // полный прогон ничего не переносит
        assert!(CarryOver::from_plan(&IncrementalPlan::disabled(&p), "abc").is_empty());
    }

    #[test]
    fn mark_carried_без_sha_без_поля() {
        let out = mark_carried(&[json!({ "file": "a.ts" })], "");
        assert_eq!(out[0], json!({ "file": "a.ts", "carriedOver": true }));
    }

    #[test]
    fn следующее_состояние_без_prescan_и_blast() {
        let h = smap(&[("a.ts", "h1"), ("b.ts", "h2")]);
        let p = paths(&["a.ts", "b.ts"]);
        let mut prior = state(json!({ "a.ts": "h1" }), json!([]));
        prior["sinceFull"] = json!(3);
        let plan = plan_incremental(PlanInput { state: Some(&prior), hashes: &h, changed_paths: &p, config_fp: "fp", full_every: 10.0 });
        let findings = [
            json!({ "file": "a.ts", "severity": "major" }),
            json!({ "file": "a.ts", "prescan": true }),
            json!({ "file": "b.ts", "source": BLAST_FINDING_SOURCE }),
        ];
        let cards = smap(&[("a.ts", "lib/x.ts\nfn")]);
        let targets: Map<String, Value> = [("a.ts".to_string(), json!(["lib/x.ts"]))].into_iter().collect();
        let next = build_next_state(NextStateInput {
            pr_number: &json!(12),
            head_sha: "def",
            config_fp: "fp",
            plan: &plan,
            prior: Some(&prior),
            hashes: &h,
            cards_input: Some(&cards),
            cards_targets: Some(&targets),
            cards_input_reused: 0,
            findings: &findings,
            unverified_paths: None,
        });
        assert_eq!(next["sinceFull"], json!(4));
        assert_eq!(next["findings"], json!([{ "file": "a.ts", "severity": "major" }]));
        assert_eq!(next["cardsTargets"], json!({ "a.ts": ["lib/x.ts"] }));
        assert!(next.get("cardsInputReused").is_none());
        let disabled = IncrementalPlan::disabled(&p);
        let full = build_next_state(NextStateInput {
            pr_number: &json!(12),
            head_sha: "def",
            config_fp: "fp",
            plan: &disabled,
            prior: Some(&prior),
            hashes: &h,
            cards_input: None,
            cards_targets: None,
            cards_input_reused: 1,
            findings: &[],
            unverified_paths: None,
        });
        assert_eq!(full["sinceFull"], json!(0));
        assert_eq!(full["cardsInputReused"], json!(true));
        assert!(full.get("cardsInput").is_none());
    }

    /// RFC 18/19: находка, до которой verifier не дошёл, и файлы непокрытых чанков не
    /// переносятся, их хэши не пишутся — следующий прогон ревьюит эти файлы заново.
    #[test]
    fn следующее_состояние_без_непроверенных_файлов() {
        let h = smap(&[("a.ts", "h1"), ("b.ts", "h2"), ("c.ts", "h3"), ("d.ts", "h4")]);
        let p = paths(&["a.ts", "b.ts", "c.ts", "d.ts"]);
        let plan = IncrementalPlan::disabled(&p);
        let findings = [
            json!({ "file": "a.ts", "severity": "minor", "verifierStatus": "unavailable" }),
            json!({ "file": "a.ts", "severity": "major" }),
            json!({ "file": "b.ts", "severity": "minor", "verifierStatus": "overflow" }),
            json!({ "file": "c.ts", "severity": "major", "verifierStatus": "verified" }),
            json!({ "file": "d.ts", "severity": "nit" }),
        ];
        let uncovered: IndexSet<String> = ["d.ts".to_string()].into_iter().collect();
        let u = unverified_paths(&findings, &uncovered);
        assert_eq!(u.iter().cloned().collect::<Vec<_>>(), vec!["a.ts", "b.ts", "d.ts"]);
        let next = build_next_state(NextStateInput {
            pr_number: &json!(1),
            head_sha: "s",
            config_fp: "fp",
            plan: &plan,
            prior: None,
            hashes: &h,
            cards_input: None,
            cards_targets: None,
            cards_input_reused: 0,
            findings: &findings,
            unverified_paths: Some(&u),
        });
        assert_eq!(next["hashes"], json!({ "c.ts": "h3" }));
        assert_eq!(next["findings"], json!([{ "file": "c.ts", "severity": "major", "verifierStatus": "verified" }]));
    }

    #[test]
    fn вход_карточек_переиспользуется_узко() {
        let prior = json!({
            "cardsInput": { "a.ts": "A", "b.ts": "B", "gone.ts": "G", "c.ts": "C" },
            "cardsTargets": { "a.ts": ["lib/x.ts"], "b.ts": ["c.ts"], "gone.ts": [] }
        });
        let p = paths(&["a.ts", "b.ts", "c.ts"]);
        let r = reuse_prior_cards_input(Some(&prior), &p).unwrap();
        // b.ts указывает на файл PR, gone.ts вне PR, у c.ts нет targets
        assert_eq!(r.cards_input, smap(&[("a.ts", "A")]));
        assert_eq!(Value::Object(r.cards_targets), json!({ "a.ts": ["lib/x.ts"] }));
        let mut twice = prior.clone();
        twice["cardsInputReused"] = json!(true);
        assert_eq!(reuse_prior_cards_input(Some(&twice), &p), None, "не больше одного прогона подряд");
        assert_eq!(reuse_prior_cards_input(None, &p), None);
    }
}
