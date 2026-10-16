//! Порт lib/config.mjs (мердж profile + env + CLI) и чистой части lib/profile.mjs
//! (нормализация профиля) — Фаза 1, шаг 6.
//!
//! Граница порта: сюда уходит ВСЯ логика приоритетов (env → профиль → дефолты)
//! и сериализуемая нормализация профиля. За границей остаётся в JS:
//!   - resolveProfile/loadProfileFile — fs + process-wide singleton (lib/profile.mjs);
//!   - компиляция RegExp/Map/Set и getter `dataRe` — не круглится через JSON;
//!   - parseCli — читает process.argv;
//!   - readRepoRules — fs (AGENTS.md/repoRulesFile);
//!   - сборка замороженного конфига (lib/config.mjs, общая для Rust/JS веток).
//!
//! load_review_config_parts возвращает «plain parts»: всё поля конфигурации
//! КРОМЕ cli/profile/markers (их JS добавляет при сборке) и github.repo/prNumber
//! (приходят из cli на стороне JS). Поля идут в том же порядке, что в JS-оригинале.

use serde_json::{json, Map, Value};

// Дефолты движка (lib/constants.mjs + lib/config.mjs).
pub const DEFAULT_MOA_REFERENCES: &str = "kimi-k2.7-code,glm-5.3,glm-5.3-flash";
pub const DEFAULT_MOA_SPECIALIST_MODELS: &str =
    "security:glm-5.3-flash,correctness:kimi-k2.7-code,architecture:glm-5.3";
pub const DEFAULT_MOA_AGGREGATOR: &str = "minimax-m3";
pub const DEFAULT_REVIEW_VERIFIER_MODEL: &str = "deepseek-v4-pro";
pub const DEFAULT_OLLAMA_MODELS_FALLBACK: &str = "deepseek-v4.1-flash";
pub const DEFAULT_STRUCTURE_TRUST_TIER: &str = "trusted_local";
pub const SPECIALIST_ROLES: [&str; 3] = ["security", "correctness", "architecture"];
/// Вес сигнала `verified` из ACTIONABILITY_SIGNALS (lib/actionability.mjs): на него
/// опускается порог допуска к verifier'у. Дублируется числом с JS-стороной
/// (VERIFIED_SIGNAL_WEIGHT в lib/config.mjs) — паритет держит config.parity.mjs.
pub const VERIFIED_SIGNAL_WEIGHT: f64 = 3.0;

// Дефолты профиля (lib/profile.mjs DEFAULT_PROFILE).
pub const DEFAULT_MARKER_NAMESPACE: &str = "ai-review";
pub const DEFAULT_LINTABLE: &str = r"\.(?:ts|tsx|mts|cts|js|jsx|mjs|cjs)$";
pub const DEFAULT_E2E_SPEC: &str = r"(?:^|/)(?:e2e|tests?)/.+\.(?:spec|test)\.[cm]?[jt]sx?$";
pub const DEFAULT_LOGIC: &str = r"\.(?:ts|tsx|js|jsx|mjs|cjs|vue|svelte)$";

// ---------------------------------------------------------------------------
// Env-хелперы — повторяют семантику JS (lib/util.mjs, lib/cost.mjs) оператор-в-оператор.
// ---------------------------------------------------------------------------

/// JS String(env[key]) для скаляров; null/undefined/объекты/массивы → None.
fn env_str(env: &Map<String, Value>, key: &str) -> Option<String> {
    match env.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

/// JS Number() над значением env: '' → 0, null → 0, true/false → 1/0,
/// 'abc' → NaN (None). Объекты/массивы → NaN (None). Отличия от JS ограничены
/// числовыми литералами 0x/0b/0o (JS парсит, Rust — нет): в реальном env
/// (всегда строки) недостижимо.
fn js_number_env(env: &Map<String, Value>, key: &str) -> Option<f64> {
    match env.get(key) {
        None => None,
        Some(Value::Null) => Some(0.0),
        Some(Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                Some(0.0)
            } else {
                t.parse::<f64>().ok()
            }
        }
        Some(_) => None,
    }
}

/// numEnv из lib/util.mjs: конечное и строго > 0, иначе def.
fn num_env(env: &Map<String, Value>, key: &str, def: f64) -> f64 {
    match js_number_env(env, key) {
        Some(n) if n.is_finite() && n > 0.0 => n,
        _ => def,
    }
}

/// nonNegEnv из lib/util.mjs: конечное и >= 0, где НОЛЬ — значащее значение
/// («выключено»), а не повод взять дефолт. Отсутствие переменной и пустая строка —
/// «не задано» → def (в отличие от js_number_env, где пустая строка это 0).
fn non_neg_env(env: &Map<String, Value>, key: &str, def: f64) -> f64 {
    match env.get(key) {
        None | Some(Value::Null) => return def,
        Some(Value::String(s)) if s.trim().is_empty() => return def,
        _ => {}
    }
    match js_number_env(env, key) {
        Some(n) if n.is_finite() && n >= 0.0 => n,
        _ => def,
    }
}

/// ratioEnv из lib/util.mjs: конечное в [0, 1], иначе def.
fn ratio_env(env: &Map<String, Value>, key: &str, def: f64) -> f64 {
    match js_number_env(env, key) {
        Some(n) if n.is_finite() && (0.0..=1.0).contains(&n) => n,
        _ => def,
    }
}

/// envFlag из lib/util.mjs: пусто/неизвестно → default_on; 0/false/no/off → false;
/// 1/true/yes/on → true.
pub fn env_flag(env: &Map<String, Value>, key: &str, default_on: bool) -> bool {
    match env_str(env, key) {
        None => default_on,
        Some(raw) => {
            let v = raw.trim().to_lowercase();
            if v.is_empty() {
                return default_on;
            }
            match v.as_str() {
                "0" | "false" | "no" | "off" => false,
                "1" | "true" | "yes" | "on" => true,
                _ => default_on,
            }
        }
    }
}

/// `a || b` над env-строкой: пустая строка (JS falsy) → def.
fn or_env(env: &Map<String, Value>, key: &str, def: &str) -> String {
    match env_str(env, key) {
        Some(s) if !s.is_empty() => s,
        _ => def.to_string(),
    }
}

/// `a ?? b` над env-строкой: пустая строка сохраняется (nullish-only).
fn nullish_env(env: &Map<String, Value>, key: &str, def: &str) -> String {
    env_str(env, key).unwrap_or_else(|| def.to_string())
}

/// Первый непустой из списка ключей, иначе def — для цепочек `a || b || def`.
fn first_or(env: &Map<String, Value>, keys: &[&str], def: &str) -> String {
    for key in keys {
        if let Some(s) = env_str(env, key) {
            if !s.is_empty() {
                return s;
            }
        }
    }
    def.to_string()
}

fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// parseModelList из lib/util.mjs: split/trim/dedupe (порядок первого вхождения),
/// затем исключение `exclude`.
pub fn parse_model_list(raw: &str, exclude: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if out.iter().any(|x| x == part) {
            continue;
        }
        out.push(part.to_string());
    }
    out.retain(|m| !exclude.contains(m));
    out
}

/// loadPriceMap из lib/cost.mjs: JSON из REVIEW_PRICE_JSON; объект или массив
/// (JS typeof '[]' === 'object') → как есть, иначе null.
fn parse_price_map(env: &Map<String, Value>) -> Value {
    match env_str(env, "REVIEW_PRICE_JSON") {
        Some(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(v) if v.is_object() || v.is_array() => v,
            _ => Value::Null,
        },
        None => Value::Null,
    }
}

/// JS `s.replace(/\/$/, '')` — ровно один завершающий слэш.
/// Допустимые значения effort в индивидуальных настройках модели.
const MODEL_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high"];

/// LLM_MODEL_SETTINGS — индивидуальные настройки моделей (RFC 20, § 6): JSON-объект
/// `{ "<model>": { "effort", "jsonEffort", "maxTokens", "provider" } }`.
///
/// Модели ведут себя по-разному при одних и тех же параметрах: deepseek-v4.1-flash на
/// effort=low уходит в рассуждение до конца бюджета и отдаёт пустой ответ, а glm-5.3-flash
/// без рассуждения отвечает 400. Глобальные REVIEW_REASONING_EFFORT и
/// REVIEW_AGGREGATOR_REASONING_EFFORT такие модели в одном ростере не совмещают.
///
/// Разбор строгий по полям и мягкий по входу: неизвестное поле, недопустимое значение или
/// битый JSON не роняют конфиг, а попадают в `errors` — их пишет в лог клиент LLM. Иначе
/// опечатка в имени поля молча возвращала бы поведение по умолчанию.
pub fn parse_model_settings(raw: Option<&str>) -> (Value, Vec<String>) {
    let mut out = Map::new();
    let mut errors = Vec::new();
    let raw = match raw.map(str::trim) {
        None | Some("") => return (Value::Object(out), errors),
        Some(r) => r,
    };
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            errors.push(format!("LLM_MODEL_SETTINGS: не JSON ({e})"));
            return (Value::Object(out), errors);
        }
    };
    let Some(obj) = parsed.as_object() else {
        errors.push("LLM_MODEL_SETTINGS: ожидался объект { модель: настройки }".to_string());
        return (Value::Object(out), errors);
    };
    for (model, settings) in obj {
        let Some(s) = settings.as_object() else {
            errors.push(format!("LLM_MODEL_SETTINGS[{model}]: ожидался объект"));
            continue;
        };
        let mut clean = Map::new();
        for (k, v) in s {
            match k.as_str() {
                "effort" | "jsonEffort" => match v.as_str().map(str::trim) {
                    Some(e) if MODEL_EFFORTS.contains(&e) => {
                        clean.insert(k.clone(), json!(e));
                    }
                    _ => errors.push(format!("LLM_MODEL_SETTINGS[{model}].{k}: одно из {}", MODEL_EFFORTS.join("|"))),
                },
                "maxTokens" => match v.as_f64() {
                    Some(n) if n.is_finite() && n >= 1.0 && n.fract() == 0.0 => {
                        clean.insert(k.clone(), json!(n as u64));
                    }
                    _ => errors.push(format!("LLM_MODEL_SETTINGS[{model}].maxTokens: целое ≥ 1")),
                },
                // Маршрутизация провайдеров OpenRouter (order / ignore / quantizations /
                // allow_fallbacks …) — уходит в тело запроса как есть.
                "provider" => {
                    if v.is_object() {
                        clean.insert(k.clone(), v.clone());
                    } else {
                        errors.push(format!("LLM_MODEL_SETTINGS[{model}].provider: ожидался объект"));
                    }
                }
                other => errors.push(format!("LLM_MODEL_SETTINGS[{model}]: неизвестное поле «{other}»")),
            }
        }
        out.insert(model.clone(), Value::Object(clean));
    }
    (Value::Object(out), errors)
}

fn strip_trailing_slash(s: &str) -> String {
    s.strip_suffix('/').unwrap_or(s).to_string()
}

/// path.posix.dirname для posix-путей конфига ('a/b/c' → 'a/b', 'c' → '.', '/c' → '/').
fn js_dirname(p: &str) -> String {
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() {
        return if p.starts_with('/') { "/".into() } else { ".".into() };
    }
    match trimmed.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
    }
}

/// path.posix.join(base, name) без '..'/'-обработки (входы — dirname + фикс. имя).
fn js_join(base: &str, name: &str) -> String {
    let joined = if base.is_empty() || base == "." {
        name.to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), name.trim_start_matches('/'))
    };
    let mut out = String::with_capacity(joined.len());
    let mut prev_slash = false;
    for c in joined.chars() {
        if c == '/' {
            if prev_slash {
                continue;
            }
            prev_slash = true;
        } else {
            prev_slash = false;
        }
        out.push(c);
    }
    out.trim_end_matches('/').to_string()
}

/// JSON-число: целые f64 сериализуем как i64 (JS-артifacts видят 240000, не 240000.0).
fn num_json(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

// ---------------------------------------------------------------------------
// Specialists (lib/specialists.mjs parseSpecialistAssignments)
// ---------------------------------------------------------------------------

/// ROLE_DEFAULT_MODEL — семантические дефолты; покрывают все роли, поэтому
/// fallback на moaReferences (как в JS) недостижим, но цепочка сохранена.
fn role_default_model(role: &str) -> &'static str {
    match role {
        "security" => "glm-5.3-flash",
        "correctness" => "kimi-k2.7-code",
        "architecture" => "glm-5.3",
        _ => "",
    }
}

pub fn parse_specialist_assignments(env: &Map<String, Value>, moa_references: &[String]) -> Vec<Value> {
    let mut from_env: Vec<(String, String)> = Vec::new();
    let bulk = match env_str(env, "MOA_SPECIALIST_MODELS") {
        Some(s) if !s.is_empty() => s,
        _ => DEFAULT_MOA_SPECIALIST_MODELS.to_string(),
    };
    for part in bulk.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let Some(i) = part.find(':') else { continue };
        if i == 0 {
            continue;
        }
        let role = part[..i].trim().to_string();
        let model = part[i + 1..].trim().to_string();
        if SPECIALIST_ROLES.contains(&role.as_str()) && !model.is_empty() {
            from_env.retain(|(r, _)| r != &role);
            from_env.push((role, model));
        }
    }
    for role in SPECIALIST_ROLES {
        let key = format!("MOA_SPECIALIST_{}_MODEL", role.to_uppercase());
        if let Some(v) = env_str(env, &key) {
            let t = v.trim();
            if !t.is_empty() {
                from_env.retain(|(r, _)| r != role);
                from_env.push((role.to_string(), t.to_string()));
            }
        }
    }
    SPECIALIST_ROLES
        .iter()
        .enumerate()
        .map(|(i, role)| {
            let model = from_env
                .iter()
                .find(|(r, _)| r == role)
                .map(|(_, m)| m.clone())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| role_default_model(role).to_string());
            let _ = moa_references.get(i); // legacy fallback недостижим, как в JS
            json!({ "role": role, "model": model })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Маркеры (lib/profile.mjs createMarkers) — dataRe getter остаётся в JS.
// ---------------------------------------------------------------------------

pub fn create_markers(namespace: Option<&str>) -> Value {
    let ns = namespace
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_MARKER_NAMESPACE)
        .to_string();
    json!({
        "ns": ns,
        "review": format!("<!-- {} -->", ns),
        "inline": format!("<!-- {}-inline -->", ns),
        "dataKey": format!("{}-data:", ns),
    })
}

// ---------------------------------------------------------------------------
// Нормализация профиля (lib/profile.mjs normalizeProfile) — сериализуемые факты.
// Регексы возвращаются ИСТОЧНИКАМИ (кандидат после выбора raw-vs-fallback):
// компиляция (валидация диалектом JS) и readRepoRules остаются в JS-обёртке.
// ---------------------------------------------------------------------------

fn value_str(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

/// JS String(raw.x || def).
fn str_or(raw: Option<&Value>, def: &str) -> String {
    match value_str(raw) {
        Some(s) if !s.is_empty() => s,
        _ => def.to_string(),
    }
}

/// JS String(v) — для полей, проходящих через String() без typeof-проверки
/// (layerDirs/astgrep.allow): null → 'null', объект → '[object Object]'.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(a) => a.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// compileRegex: выбор кандидата (raw, если это непустая строка, иначе fallback).
/// Пустой итог → null; валидация паттерна — на стороне JS.
fn regex_candidate(v: Option<&Value>, fallback: &str) -> Value {
    let raw = match v {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        _ => fallback.to_string(),
    };
    if raw.is_empty() {
        Value::Null
    } else {
        Value::String(raw)
    }
}

/// `String(raw.appDir ?? '').replace(/^\.\/+|\/+$/g, '')`:
/// ровно одно ведущее '.' + прогон '/', хвостовые '/' снимаются все.
fn strip_app_dir(s: &str) -> String {
    let mut rest = s;
    if let Some(stripped) = rest.strip_prefix('.') {
        let slashes = stripped.len() - stripped.trim_start_matches('/').len();
        if slashes > 0 {
            rest = &stripped[slashes..];
        }
    }
    rest.trim_end_matches('/').to_string()
}

/// prompts.str: только строки, trim, иначе ''.
fn prompt_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                String::new()
            } else {
                t.to_string()
            }
        }
        _ => String::new(),
    }
}

fn default_paths() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("feedbackLog".into(), json!(".ai-review/feedback/fp-feedback.jsonl"));
    m.insert("feedbackCandidatesDir".into(), json!(".ai-review/feedback/candidates"));
    m.insert("fpPatterns".into(), json!(""));
    m.insert("fpMarkdown".into(), json!("docs/ai-review-false-positives.md"));
    m.insert("observabilityDir".into(), json!(".ai-review/observability"));
    m.insert("exportDir".into(), json!(".ai-review/export"));
    // Состояние инкрементального прогона (lib/incremental.mjs).
    m.insert(
        "incrementalState".into(),
        json!(".ai-review/incremental/state.json"),
    );
    m.insert("structureExportDir".into(), json!(".ai-review/structure"));
    m.insert("coverageLcov".into(), json!("coverage/lcov.info"));
    m.insert("astgrepConfig".into(), json!(""));
    m
}

pub fn normalize_profile_facts(raw: &Value, base_dir: &str, source: &str) -> Value {
    let empty = Map::new();
    let raw = match raw {
        Value::Object(m) => m,
        _ => &empty,
    };
    let marker_namespace = {
        let m = str_or(raw.get("markerNamespace"), DEFAULT_MARKER_NAMESPACE);
        let m = m.trim();
        if m.is_empty() {
            DEFAULT_MARKER_NAMESPACE.to_string()
        } else {
            m.to_string()
        }
    };
    let tool_name = {
        let t = str_or(raw.get("toolName"), &marker_namespace);
        let t = t.trim();
        if t.is_empty() {
            "ai-review".to_string()
        } else {
            t.to_string()
        }
    };
    let sarif_rule_prefix = {
        let s = str_or(raw.get("sarifRulePrefix"), &tool_name);
        let s = s.trim();
        if s.is_empty() {
            "ai-review".to_string()
        } else {
            s.to_string()
        }
    };

    let layout_raw = match raw.get("layout") {
        Some(Value::Object(m)) => m,
        _ => &empty,
    };
    let app_dir = strip_app_dir(&str_or(layout_raw.get("appDir"), ""));
    let layer_dirs: Vec<String> = match layout_raw.get("layerDirs") {
        Some(Value::Array(items)) => items
            .iter()
            .map(js_string)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let app_prefix_source = if !app_dir.is_empty() && !layer_dirs.is_empty() {
        Value::String(format!("^(?:{})/", layer_dirs.join("|")))
    } else {
        Value::Null
    };

    let astgrep_raw = match raw.get("astgrep") {
        Some(Value::Object(m)) => m,
        _ => &empty,
    };
    let mut scopes = Map::new();
    if let Some(Value::Object(m)) = astgrep_raw.get("scopes") {
        for (rule_id, pattern) in m {
            if let Some(candidate) = regex_candidate(Some(pattern), "").as_str() {
                scopes.insert(rule_id.clone(), Value::String(candidate.to_string()));
            }
        }
    }
    let mut allow = Map::new();
    if let Some(Value::Object(m)) = astgrep_raw.get("allow") {
        for (rule_id, paths) in m {
            let list: Vec<Value> = match paths {
                Value::Array(items) => items.iter().map(|v| Value::String(js_string(v))).collect(),
                other => vec![Value::String(js_string(other))],
            };
            allow.insert(rule_id.clone(), Value::Array(list));
        }
    }

    let mut paths = default_paths();
    if let Some(Value::Object(m)) = raw.get("paths") {
        for (k, v) in m {
            paths.insert(k.clone(), v.clone());
        }
    }

    json!({
        "name": str_or(raw.get("name"), "default"),
        "source": source,
        "baseDir": base_dir,
        "markerNamespace": marker_namespace,
        "toolName": tool_name,
        "sarifRulePrefix": sarif_rule_prefix,
        "repositoryUri": str_or(raw.get("repositoryUri"), ""),
        "prompts": {
            "advisorExtra": prompt_str(raw.get("prompts").and_then(|p| p.get("advisorExtra"))),
            "specialistExtra": prompt_str(raw.get("prompts").and_then(|p| p.get("specialistExtra"))),
            "aggregatorExtra": prompt_str(raw.get("prompts").and_then(|p| p.get("aggregatorExtra"))),
            "verifierExtra": prompt_str(raw.get("prompts").and_then(|p| p.get("verifierExtra"))),
        },
        "markers": create_markers(Some(&marker_namespace)),
        "layout": {
            "appDir": app_dir,
            "layerDirs": layer_dirs,
            "appPrefixSource": app_prefix_source,
            "lintable": regex_candidate(layout_raw.get("lintable"), DEFAULT_LINTABLE),
            "sensitive": regex_candidate(layout_raw.get("sensitive"), ""),
            "e2eSpec": regex_candidate(layout_raw.get("e2eSpec"), DEFAULT_E2E_SPEC),
            "logic": regex_candidate(layout_raw.get("logic"), DEFAULT_LOGIC),
        },
        "astgrep": { "scopes": scopes, "allow": allow },
        "strict": raw.get("strict") == Some(&Value::Bool(true)),
        "paths": paths,
    })
}

// ---------------------------------------------------------------------------
// loadReviewConfig (lib/config.mjs) — plain parts.
// profile: {"strict": bool, "appDir": str, "paths": {...}} — resolved профиль.
// ---------------------------------------------------------------------------

fn ppath<'a>(paths: Option<&'a Map<String, Value>>, key: &str) -> String {
    paths
        .and_then(|m| m.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Температуры/подобные: Number.isFinite(n) && n >= 0 ? n : def (Number(env.X)).
fn nn_nonnegative(env: &Map<String, Value>, key: &str, def: f64) -> f64 {
    match js_number_env(env, key) {
        Some(n) if n.is_finite() && n >= 0.0 => n,
        _ => def,
    }
}

pub fn load_review_config_parts(env: &Map<String, Value>, profile: &Value) -> Value {
    let profile = match profile {
        Value::Object(m) => m,
        _ => &Map::new(),
    };
    let profile_strict = profile.get("strict").and_then(Value::as_bool).unwrap_or(false);
    let profile_app_dir = profile
        .get("appDir")
        .and_then(Value::as_str)
        .unwrap_or("");
    let paths = profile.get("paths").and_then(Value::as_object);

    let ollama_model = or_env(env, "OLLAMA_MODEL", "kimi-k2.7-code");
    let review_mode = or_env(env, "REVIEW_MODE", "moa").trim().to_lowercase();
    let moa_references = parse_model_list(&or_env(env, "MOA_REFERENCE_MODELS", DEFAULT_MOA_REFERENCES), &[]);
    let specialists_enabled = review_mode == "moa" && env_flag(env, "REVIEW_SPECIALISTS", true);
    let specialist_assignments = parse_specialist_assignments(env, &moa_references);
    let moa_aggregator = or_env(env, "MOA_AGGREGATOR", DEFAULT_MOA_AGGREGATOR).trim().to_string();
    let moa_label = if specialists_enabled {
        let roles: Vec<&str> = specialist_assignments
            .iter()
            .map(|a| a.get("role").and_then(Value::as_str).unwrap_or(""))
            .collect();
        format!("moa-specialist({}←{})", moa_aggregator, roles.join("+"))
    } else {
        format!("moa({}←{})", moa_aggregator, moa_references.join("+"))
    };
    let models = if review_mode == "moa" {
        vec![moa_label.clone()]
    } else {
        parse_model_list(&or_env(env, "OLLAMA_MODELS", &ollama_model), &[])
    };
    let block_on = split_csv(&nullish_env(env, "REVIEW_BLOCK_ON", "critical,major"));
    let inline_on = split_csv(&nullish_env(env, "REVIEW_INLINE_ON", "critical,major"));
    let block_single: Vec<String> = split_csv(&nullish_env(env, "REVIEW_BLOCK_SINGLE", "critical"))
        .into_iter()
        .filter(|s| block_on.contains(s))
        .collect();
    let consensus_default = if review_mode == "moa" { 1.0 } else { 2.0 };
    let consensus_num = match js_number_env(env, "REVIEW_CONSENSUS") {
        Some(n) if n.is_finite() && n != 0.0 => n,
        _ => consensus_default,
    };
    let consensus_cap = if review_mode == "moa" {
        1.0
    } else {
        models.len().max(1) as f64
    };
    let consensus_min = consensus_num.max(1.0).min(consensus_cap);
    let moa_ref_temp = nn_nonnegative(env, "MOA_REFERENCE_TEMPERATURE", 0.3);
    let moa_agg_temp = nn_nonnegative(env, "MOA_AGGREGATOR_TEMPERATURE", 0.4);
    let temperature = nn_nonnegative(env, "REVIEW_TEMPERATURE", 0.4);
    let seed = match js_number_env(env, "REVIEW_SEED") {
        Some(n) if n.is_finite() && n.fract() == 0.0 && n >= 0.0 => n as i64,
        _ => 7,
    };
    let http_retries = num_env(env, "REVIEW_HTTP_RETRIES", 3.0);
    let confidence_min = match js_number_env(env, "REVIEW_CONFIDENCE_MIN") {
        Some(n) if n.is_finite() && (0.0..=1.0).contains(&n) => n,
        _ => 0.8,
    };
    let verifier_enabled = env_flag(env, "REVIEW_VERIFIER", true);
    let verifier_model = or_env(env, "REVIEW_VERIFIER_MODEL", DEFAULT_REVIEW_VERIFIER_MODEL).trim().to_string();
    let verifier_temp = nn_nonnegative(env, "REVIEW_VERIFIER_TEMPERATURE", 0.2);
    let router_enabled = env_flag(env, "REVIEW_ROUTER", true);
    let prescan_enabled = env_flag(env, "REVIEW_PRESCAN", true);
    let prescan_block_rules = split_csv(&nullish_env(
        env,
        "REVIEW_PRESCAN_BLOCK_RULES",
        "no-restricted-imports,no-restricted-globals",
    ));
    let astgrep_enabled = env_flag(env, "REVIEW_PRESCAN_ASTGREP", true);
    let astgrep_block_rules = split_csv(&nullish_env(
        env,
        "REVIEW_PRESCAN_ASTGREP_BLOCK_RULES",
        "raw-x-forwarded-for",
    ));
    let observability_enabled = env_flag(env, "REVIEW_OBSERVABILITY", true);
    let feedback_enabled = env_flag(env, "REVIEW_FEEDBACK", true);
    let export_enabled = env_flag(env, "REVIEW_EXPORT", true);
    let triage_enabled = env_flag(env, "REVIEW_AUTO_TRIAGE", true);
    let strict = env_flag(env, "REVIEW_STRICT", false) || profile_strict;
    let require_quote = env_flag(env, "REVIEW_REQUIRE_QUOTE", strict);
    let llm_key = first_or(env, &["LLM_API_KEY", "OLLAMA_API_KEY"], "").trim().to_string();
    let llm_base = strip_trailing_slash(&first_or(
        env,
        &["LLM_BASE_URL", "OLLAMA_BASE_URL"],
        "https://ollama.com/v1",
    ));
    let price_map = parse_price_map(env);

    // Единый источник путей graphify: structure.* наследует их.
    let graphify_graph = or_env(env, "GRAPH", ".graphify/graphify-out/graph.json");
    let graphify_report = or_env(env, "GRAPHIFY_REPORT", ".graphify/graphify-out/GRAPH_REPORT.md");
    let structure_graph = or_env(env, "REVIEW_STRUCTURE_GRAPH", &graphify_graph);

    let mut out = Map::new();
    out.insert("reviewMode".into(), json!(review_mode));
    // Раскладка сообщений под префиксный кэш (RFC 04, решение 5; lib/prompt-layout.mjs).
    out.insert(
        "promptLayout".into(),
        json!(
            match or_env(env, "REVIEW_PROMPT_LAYOUT", "legacy")
                .trim()
                .to_ascii_lowercase()
                .as_str()
            {
                "shared-prefix" => "shared-prefix",
                _ => "legacy",
            }
        ),
    );
    out.insert("httpRetries".into(), num_json(http_retries));
    // Кто считается автором бота (sticky, inline, auto-triage). Пустая строка — значащее
    // значение «доверять всем» (откат), поэтому различаем «не задано» и «пусто» (как JS
    // `!== undefined`). REVIEW_FEEDBACK_TRUSTED_AUTHORS — алиас.
    let trusted_authors_raw = env_str(env, "REVIEW_TRUSTED_AUTHORS")
        .or_else(|| env_str(env, "REVIEW_FEEDBACK_TRUSTED_AUTHORS"))
        .unwrap_or_else(|| "github-actions[bot]".to_string());
    let trusted_authors: Vec<String> = trusted_authors_raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    out.insert(
        "github".into(),
        json!({
            "api": strip_trailing_slash(&or_env(env, "GITHUB_API_URL", "https://api.github.com")),
            "token": or_env(env, "GITHUB_TOKEN", ""),
            "trustedAuthors": trusted_authors,
        }),
    );
    out.insert(
        "llm".into(),
        json!({ "key": llm_key, "base": llm_base }),
    );
    let (model_settings, model_settings_errors) = parse_model_settings(env_str(env, "LLM_MODEL_SETTINGS").as_deref());
    out.insert(
        "ollama".into(),
        json!({
            "key": llm_key,
            "base": llm_base,
            "model": ollama_model,
            "temperature": num_json(temperature),
            "jsonMode": env_flag(env, "REVIEW_JSON_MODE", true),
            "seed": seed,
            "httpRetries": num_json(http_retries),
            // 240 с обрывали половину prose-вызовов (медиана kimi на замере — 331 с):
            // вход оплачен, ответа нет. 420 с покрывают p90 замера.
            "requestTimeoutMs": num_json(num_env(env, "REVIEW_LLM_TIMEOUT_MS", 420_000.0)),
            "effort": or_env(env, "REVIEW_REASONING_EFFORT", "low").trim(),
            // Reasoning effort первой попытки JSON-вызовов (агрегатор, verifier): рассуждение
            // считается completion-токенами (замер: 1903 на находку у minimax-m3 против 474
            // у sonnet), эксперименту нужен переключатель, а не правка кода.
            "aggregatorEffort": or_env(env, "REVIEW_AGGREGATOR_REASONING_EFFORT", "low").trim(),
            "providerConcurrency": num_json(num_env(env, "REVIEW_PROVIDER_CONCURRENCY", 12.0)),
            "providerPerModel": num_json(num_env(env, "REVIEW_PROVIDER_PER_MODEL", 6.0)),
            "modelSettings": model_settings,
            "modelSettingsErrors": model_settings_errors,
        }),
    );
    out.insert(
        "moa".into(),
        json!({
            "references": moa_references,
            "aggregator": moa_aggregator,
            "label": moa_label,
            // Страховка под лимит «не более 8 пунктов» из промпта (RFC 04, решение 3).
            "refMaxTokens": num_json(num_env(env, "MOA_REFERENCE_MAX_TOKENS", 6_000.0)),
            "refTemp": num_json(moa_ref_temp),
            "aggTemp": num_json(moa_agg_temp),
        }),
    );
    out.insert(
        "models".into(),
        json!({
            "list": models,
            "fallback": parse_model_list(
                &or_env(env, "OLLAMA_MODELS_FALLBACK", DEFAULT_OLLAMA_MODELS_FALLBACK),
                &models, // exclude = list (в moa — [moaLabel])
            ),
        }),
    );
    out.insert(
        "gate".into(),
        json!({
            "blockOn": block_on,
            "blockSingle": block_single,
            "consensusMin": num_json(consensus_min),
            "confidenceMin": num_json(confidence_min),
            "llmRequired": env_flag(env, "REVIEW_LLM_REQUIRED", true),
            "blastRequired": env_flag(env, "REVIEW_BLAST_REQUIRED", false),
            "strict": strict,
            "requireQuote": require_quote,
        }),
    );
    out.insert("cost".into(), json!({ "priceMap": price_map }));
    out.insert("inlineOn".into(), json!(inline_on));
    let actionability_min = num_env(env, "REVIEW_ACTIONABILITY_MIN", 4.0);
    out.insert(
        "actionability".into(),
        json!({
            "threshold": num_json(actionability_min),
            "maxAdvisoryInline": num_json(num_env(env, "REVIEW_MAX_ADVISORY_INLINE", 5.0)),
            "verifyAdvisory": num_json(num_env(env, "REVIEW_VERIFY_ADVISORY", 5.0)),
            // Порог ДОПУСКА к verifier'у, отдельный от порога маршрута: «порог минус вес
            // сигнала verified» — кандидатом становится всё, что СТАНЕТ доказуемым после
            // подтверждения (docs/review-quality.md §2.2, P1).
            "verifyMin": num_json(num_env(
                env,
                "REVIEW_VERIFY_ADVISORY_MIN",
                actionability_min - VERIFIED_SIGNAL_WEIGHT,
            )),
        }),
    );
    out.insert(
        "verifier".into(),
        json!({
            "enabled": verifier_enabled,
            "model": verifier_model,
            "maxCandidates": num_json(num_env(env, "REVIEW_VERIFIER_MAX", 12.0)),
            // Блокеры сверх капа одного вызова — следующими батчами, а не молча в minor (RFC 17).
            "maxBatches": num_json(num_env(env, "REVIEW_VERIFIER_BATCHES", 3.0)),
            "temperature": num_json(verifier_temp),
            // ±строк вокруг строки-кандидата в контексте verifier'а (lib/context-window.mjs):
            // второй проход смотрит 1–2 строки на файл, целый файл он не окупает.
            "contextMargin": num_json(num_env(env, "REVIEW_VERIFIER_CONTEXT_MARGIN", 80.0)),
            "strictBlocks": env_flag(env, "REVIEW_VERIFIER_STRICT", true),
            // Карточки символов verifier'у (RFC 14, решение 4): выключено до gold ×3.
            "symbolCards": env_flag(env, "REVIEW_VERIFIER_SYMBOL_CARDS", false),
        }),
    );
    out.insert("router".into(), json!({ "enabled": router_enabled }));
    out.insert(
        "specialists".into(),
        json!({
            "enabled": specialists_enabled,
            "assignments": specialist_assignments,
            // auto — слить роли в один вызов, когда модель у всех одна (RFC 04, решение 4);
            // always — сливать и при разных моделях; never — опрашивать по отдельности.
            "merge": match or_env(env, "REVIEW_SPECIALISTS_MERGE", "auto")
                .trim()
                .to_ascii_lowercase()
                .as_str()
            {
                "always" => "always",
                "never" => "never",
                _ => "auto",
            },
        }),
    );
    out.insert(
        "prescan".into(),
        json!({
            "enabled": prescan_enabled,
            "blockEnabled": env_flag(env, "REVIEW_PRESCAN_BLOCK", true),
            "appDir": nullish_env(env, "REVIEW_APP_DIR", profile_app_dir),
            "eslintBin": or_env(env, "REVIEW_ESLINT_BIN", "npx"),
            "maxFiles": num_json(num_env(env, "REVIEW_PRESCAN_MAX_FILES", 30.0)),
            "blockRules": prescan_block_rules,
            "astgrep": {
                "enabled": prescan_enabled && astgrep_enabled,
                "configPath": or_env(env, "REVIEW_ASTGREP_CONFIG", &ppath(paths, "astgrepConfig")),
                "bin": or_env(env, "REVIEW_ASTGREP_BIN", "ast-grep"),
                "blockRules": astgrep_block_rules,
            },
        }),
    );
    out.insert(
        "observability".into(),
        json!({
            "enabled": observability_enabled,
            "dir": or_env(env, "REVIEW_OBSERVABILITY_DIR", &ppath(paths, "observabilityDir")),
            "file": or_env(env, "REVIEW_OBSERVABILITY_FILE", "pr-review-runs.jsonl"),
        }),
    );
    // Инкрементальный прогон (RFC 05, lib/incremental.mjs): не ревьюить заново файлы,
    // чей вход для модели не изменился.
    out.insert(
        "incremental".into(),
        json!({
            "enabled": env_flag(env, "REVIEW_INCREMENTAL", false),
            "stateFile": or_env(env, "REVIEW_INCREMENTAL_STATE", &ppath(paths, "incrementalState")),
            "fullEvery": num_json(num_env(env, "REVIEW_INCREMENTAL_FULL_EVERY", 10.0)),
        }),
    );
    out.insert(
        "export".into(),
        json!({
            "enabled": export_enabled,
            "dir": or_env(env, "REVIEW_EXPORT_DIR", &ppath(paths, "exportDir")),
            "json": env_flag(env, "REVIEW_EXPORT_JSON", true),
            "sarif": env_flag(env, "REVIEW_EXPORT_SARIF", true),
            "sarifAll": env_flag(env, "REVIEW_EXPORT_SARIF_ALL", false),
            "dryRun": env_flag(env, "REVIEW_EXPORT_DRY_RUN", false),
            "reasoningInSticky": env_flag(env, "REVIEW_REASONING_STICKY", true),
        }),
    );
    out.insert("triage".into(), json!({ "enabled": triage_enabled }));
    out.insert(
        "feedback".into(),
        json!({
            "enabled": feedback_enabled,
            "label": or_env(env, "REVIEW_FEEDBACK_LABEL", "ai-review-fp"),
            "logFile": or_env(env, "REVIEW_FEEDBACK_LOG", &ppath(paths, "feedbackLog")),
            "candidatesDir": or_env(env, "REVIEW_FEEDBACK_CANDIDATES_DIR", &ppath(paths, "feedbackCandidatesDir")),
            "mdPath": or_env(env, "REVIEW_FEEDBACK_MD", &ppath(paths, "fpMarkdown")),
            "appendMarkdown": env_flag(env, "REVIEW_FEEDBACK_MD_APPEND", false),
            "writeCandidates": env_flag(env, "REVIEW_FEEDBACK_CANDIDATES", true),
            "write": env_flag(env, "REVIEW_FEEDBACK_WRITE", true),
            "trustedAuthors": trusted_authors_raw,
            "reactionConcurrency": num_json(num_env(env, "REVIEW_FEEDBACK_CONCURRENCY", 6.0)),
            "patternsFile": or_env(env, "REVIEW_FP_PATTERNS", &ppath(paths, "fpPatterns")),
            "promoteMinFpCount": num_json(num_env(env, "REVIEW_FP_PROMOTE_MIN", 2.0)),
        }),
    );
    out.insert(
        "limits".into(),
        json!({
            "maxDiffBytes": num_json(num_env(env, "REVIEW_MAX_DIFF_BYTES", 200_000.0)),
            "maxSymbols": num_json(num_env(env, "REVIEW_MAX_SYMBOLS", 25.0)),
            "maxDepsPerSymbol": num_json(num_env(env, "REVIEW_MAX_DEPS_PER_SYMBOL", 20.0)),
            "godNodeThreshold": num_json(num_env(env, "REVIEW_GOD_NODE_THRESHOLD", 20.0)),
            "maxFileLines": num_json(num_env(env, "REVIEW_MAX_FILE_LINES", 1500.0)),
            // Добавленный файл целиком лежит в диффе — второй экземпляр в контексте
            // оплачивается каждой ролью на каждом чанке (см. lib/diff.mjs addedFileMarker).
            "skipAddedFileContext": env_flag(env, "REVIEW_SKIP_ADDED_FILE_CONTEXT", true),
            // Чанк без исполняемого кода обслуживает один агрегатор вместо жюри
            // (RFC 04, lib/chunk-routing.mjs).
            "chunkRouting": env_flag(env, "REVIEW_CHUNK_ROUTING", true),
            // Окна контекста для advisors и жюри (RFC 07, lib/context-window.mjs):
            // НОЛЬ — «окна выключены» (откат), поэтому non_neg_env, а не num_env.
            "contextMargin": num_json(non_neg_env(env, "REVIEW_CONTEXT_MARGIN", 40.0)),
            "contextWindowMinChars": num_json(non_neg_env(env, "REVIEW_CONTEXT_WINDOW_MIN_CHARS", 8000.0)),
            // Карточки символов (RFC 14, lib/symbol-cards.mjs): включено по gold A/B.
            "symbolCards": env_flag(env, "REVIEW_SYMBOL_CARDS", true),
            "symbolCardsMax": num_json(num_env(env, "REVIEW_SYMBOL_CARDS_MAX", 20.0)),
            "symbolCardsMaxChars": num_json(num_env(env, "REVIEW_SYMBOL_CARDS_MAX_CHARS", 3000.0)),
            "symbolCardsBodyMaxLines": num_json(non_neg_env(env, "REVIEW_SYMBOL_CARDS_BODY_MAX_LINES", 0.0)),
            "maxContextBytes": num_json(num_env(env, "REVIEW_MAX_CONTEXT_BYTES", 180_000.0)),
            "chunkBytes": num_json(num_env(env, "REVIEW_CHUNK_BYTES", 56_000.0)),
            "maxChunks": num_json(num_env(env, "REVIEW_MAX_CHUNKS", 40.0)),
            "concurrency": num_json(num_env(env, "REVIEW_CONCURRENCY", 10.0)),
            "maxInline": num_json(num_env(env, "REVIEW_MAX_INLINE", 50.0)),
            "timeBudgetMs": num_json(num_env(env, "REVIEW_TIME_BUDGET_MIN", 20.0) * 60_000.0),
        }),
    );
    out.insert(
        "graphify".into(),
        json!({
            "bin": or_env(env, "GRAPHIFY_BIN", ".graphify/gf"),
            "graph": graphify_graph,
            "report": graphify_report,
            "depth": num_json(num_env(env, "REVIEW_GRAPHIFY_DEPTH", 2.0).max(1.0).min(5.0)),
            "dropAmbiguous": env_flag(env, "REVIEW_GRAPHIFY_DROP_AMBIGUOUS", true),
        }),
    );
    out.insert(
        "structure".into(),
        json!({
            "enabled": env_flag(env, "REVIEW_STRUCTURE", true),
            "graph": structure_graph,
            "graphReport": or_env(env, "REVIEW_STRUCTURE_GRAPH_REPORT", &graphify_report),
            "graphAnalysis": or_env(
                env,
                "REVIEW_STRUCTURE_GRAPH_ANALYSIS",
                &js_join(&js_dirname(&structure_graph), ".graphify_analysis.json"),
            ),
            "scip": or_env(env, "REVIEW_STRUCTURE_SCIP", ""),
            "lcov": or_env(env, "REVIEW_STRUCTURE_LCOV", &ppath(paths, "coverageLcov")),
            "obligations": or_env(env, "REVIEW_STRUCTURE_OBLIGATIONS", ""),
            "mutationReport": or_env(env, "REVIEW_STRUCTURE_MUTATION", ""),
            "propertyReport": or_env(env, "REVIEW_STRUCTURE_PROPERTIES", ""),
            "exportDir": or_env(env, "REVIEW_STRUCTURE_EXPORT_DIR", &ppath(paths, "structureExportDir")),
            "scope": or_env(env, "REVIEW_STRUCTURE_SCOPE", ""),
            "trustTier": or_env(env, "REVIEW_STRUCTURE_TRUST_TIER", DEFAULT_STRUCTURE_TRUST_TIER),
            "includeAmbiguous": env_flag(env, "REVIEW_STRUCTURE_INCLUDE_AMBIGUOUS", false),
            "maxEntities": num_json(num_env(env, "REVIEW_STRUCTURE_MAX_ENTITIES", 5000.0)),
            "maxEdges": num_json(num_env(env, "REVIEW_STRUCTURE_MAX_EDGES", 15_000.0)),
            "thresholds": {
                "highFanIn": num_json(num_env(env, "REVIEW_STRUCTURE_HIGH_FAN_IN", 5.0)),
                "unresolvedJoinRate": num_json(ratio_env(env, "REVIEW_STRUCTURE_MAX_UNRESOLVED_RATE", 0.1)),
            },
        }),
    );
    // Пиктограммы вместо эмодзи в PR-комментариях (lib/icons.mjs); пусто — эмодзи.
    out.insert(
        "render".into(),
        json!({ "iconBaseUrl": or_env(env, "REVIEW_ICON_BASE_URL", "") }),
    );
    out.insert(
        "telegram".into(),
        json!({
            "token": or_env(env, "TELEGRAM_BOT_TOKEN", ""),
            "chat": or_env(env, "TELEGRAM_CHAT_ID", ""),
        }),
    );
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// Тесты на приоритеты и дефолты (зеркало JS-тестов lib/config.mjs).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect()
    }

    fn default_profile() -> Value {
        normalize_profile_facts(&json!({}), "/pkg", "(default)")
    }

    fn profile_bits(profile: &Value) -> Value {
        json!({
            "strict": profile.get("strict").cloned().unwrap_or(Value::Bool(false)),
            "appDir": profile.pointer("/layout/appDir").cloned().unwrap_or(Value::Null),
            "paths": profile.get("paths").cloned().unwrap_or(Value::Null),
        })
    }

    #[test]
    fn env_flag_semantics() {
        let e = env(&[
            ("A", "0"), ("B", "false"), ("C", "no"), ("D", "off"),
            ("E", "1"), ("F", "true"), ("G", "yes"), ("H", "on"),
            ("I", "junk"), ("J", ""), ("K", " OFF "),
        ]);
        for k in ["A", "B", "C", "D", "K"] {
            assert!(!env_flag(&e, k, true), "{k} должно выключать");
        }
        for k in ["E", "F", "G", "H"] {
            assert!(env_flag(&e, k, false), "{k} должно включать");
        }
        assert!(env_flag(&e, "I", true), "junk → default_on");
        assert!(!env_flag(&e, "I", false), "junk → default_off");
        assert!(env_flag(&e, "J", true), "пусто → default_on");
        assert!(env_flag(&e, "MISSING", true), "отсутствует → default_on");
    }

    #[test]
    fn num_env_positive_only() {
        let e = env(&[("A", "2.5"), ("B", "0"), ("C", "abc"), ("D", ""), ("E", "-3")]);
        assert_eq!(num_env(&e, "A", 3.0), 2.5);
        assert_eq!(num_env(&e, "B", 3.0), 3.0, "0 не > 0 → def");
        assert_eq!(num_env(&e, "C", 3.0), 3.0);
        assert_eq!(num_env(&e, "D", 3.0), 3.0);
        assert_eq!(num_env(&e, "E", 3.0), 3.0);
        assert_eq!(num_env(&e, "MISSING", 3.0), 3.0);
    }

    #[test]
    fn non_neg_env_zero_is_meaningful() {
        let e = env(&[("A", "0"), ("B", ""), ("C", "abc"), ("D", "-3"), ("E", "40")]);
        assert_eq!(non_neg_env(&e, "A", 40.0), 0.0, "0 — значащее значение (выключено)");
        assert_eq!(non_neg_env(&e, "B", 40.0), 40.0, "пустая строка — «не задано»");
        assert_eq!(non_neg_env(&e, "C", 40.0), 40.0);
        assert_eq!(non_neg_env(&e, "D", 40.0), 40.0);
        assert_eq!(non_neg_env(&e, "E", 40.0), 40.0);
        assert_eq!(non_neg_env(&e, "MISSING", 40.0), 40.0);
    }

    #[test]
    fn ratio_env_range() {
        let e = env(&[("A", "0"), ("B", "1"), ("C", "0.5"), ("D", "1.5"), ("E", "-0.1")]);
        assert_eq!(ratio_env(&e, "A", 0.1), 0.0);
        assert_eq!(ratio_env(&e, "B", 0.1), 1.0);
        assert_eq!(ratio_env(&e, "C", 0.1), 0.5);
        assert_eq!(ratio_env(&e, "D", 0.1), 0.1);
        assert_eq!(ratio_env(&e, "E", 0.1), 0.1);
    }

    #[test]
    fn js_number_empty_and_null_are_zero() {
        let mut e = env(&[("A", "")]);
        assert_eq!(js_number_env(&e, "A"), Some(0.0), "Number('') === 0");
        e.insert("B".into(), Value::Null);
        assert_eq!(js_number_env(&e, "B"), Some(0.0), "Number(null) === 0");
        e.insert("C".into(), Value::Bool(true));
        assert_eq!(js_number_env(&e, "C"), Some(1.0));
        assert_eq!(js_number_env(&e, "MISSING"), None);
    }

    #[test]
    fn parse_model_list_dedupe_exclude() {
        let got = parse_model_list("m1, m2,m1, ,m3", &["m2".to_string()]);
        assert_eq!(got, vec!["m1", "m3"]);
    }

    #[test]
    fn strip_and_paths() {
        assert_eq!(strip_trailing_slash("https://x/api/"), "https://x/api");
        assert_eq!(strip_trailing_slash("https://x/api//"), "https://x/api/");
        assert_eq!(js_dirname("a/b/c"), "a/b");
        assert_eq!(js_dirname("c"), ".");
        assert_eq!(js_dirname("/c"), "/");
        assert_eq!(js_join(".", ".graphify_analysis.json"), ".graphify_analysis.json");
        assert_eq!(
            js_join(&js_dirname(".graphify/graphify-out/graph.json"), ".graphify_analysis.json"),
            ".graphify/graphify-out/.graphify_analysis.json"
        );
    }

    #[test]
    fn create_markers_defaults() {
        let m = create_markers(Some("  "));
        assert_eq!(m["ns"], "ai-review");
        assert_eq!(m["review"], "<!-- ai-review -->");
        assert_eq!(m["inline"], "<!-- ai-review-inline -->");
        assert_eq!(m["dataKey"], "ai-review-data:");
        let custom = create_markers(Some("acme "));
        assert_eq!(custom["dataKey"], "acme-data:");
    }

    #[test]
    fn normalize_profile_orakul_shape() {
        let raw = json!({
            "name": "orakul",
            "markerNamespace": "orakul-ai-review",
            "toolName": "orakul-ai-review",
            "sarifRulePrefix": "orakul",
            "layout": { "appDir": "app/", "layerDirs": ["server", "client"] },
            "paths": { "exportDir": "out/export" },
        });
        let f = normalize_profile_facts(&raw, "/repo", "/repo/profile.json");
        assert_eq!(f["name"], "orakul");
        assert_eq!(f["toolName"], "orakul-ai-review");
        assert_eq!(f["layout"]["appDir"], "app");
        assert_eq!(f["layout"]["appPrefixSource"], "^(?:server|client)/");
        assert_eq!(f["layout"]["lintable"], DEFAULT_LINTABLE);
        assert_eq!(f["layout"]["sensitive"], Value::Null);
        assert_eq!(f["paths"]["exportDir"], "out/export");
        assert_eq!(f["paths"]["feedbackLog"], ".ai-review/feedback/fp-feedback.jsonl");
        assert!(f["strict"] == Value::Bool(false));
    }

    #[test]
    fn normalize_profile_tool_name_fallbacks_and_broken_regex() {
        let raw = json!({
            "markerNamespace": "  ",
            "layout": { "appDir": "././app//", "lintable": "([unclosed" },
            "astgrep": { "allow": { "r1": "app/x.ts", "r2": null } },
            "strict": true,
        });
        let f = normalize_profile_facts(&raw, "/repo", "s");
        assert_eq!(f["markerNamespace"], "ai-review");
        assert_eq!(f["toolName"], "ai-review");
        assert_eq!(f["sarifRulePrefix"], "ai-review");
        // `^\.\/+` снимает ОДНО ведущее './' (не цикл), хвостовые '/' — все.
        assert_eq!(f["layout"]["appDir"], "./app");
        // Битый регекс: кандидат остаётся raw, валидация — за JS-обёрткой.
        assert_eq!(f["layout"]["lintable"], "([unclosed");
        assert_eq!(f["astgrep"]["allow"]["r1"], json!(["app/x.ts"]));
        assert_eq!(f["astgrep"]["allow"]["r2"], json!(["null"]), "JS String(null)");
        assert_eq!(f["strict"], Value::Bool(true));
    }

    #[test]
    fn load_trusted_authors_env_alias_and_empty_rollback() {
        let bits = profile_bits(&default_profile());
        let parts = load_review_config_parts(&Map::new(), &bits);
        assert_eq!(parts["github"]["trustedAuthors"], json!(["github-actions[bot]"]));
        assert_eq!(parts["feedback"]["trustedAuthors"], "github-actions[bot]");

        let mut e = Map::new();
        e.insert("REVIEW_FEEDBACK_TRUSTED_AUTHORS".into(), json!("legacy-bot[bot]"));
        let parts = load_review_config_parts(&e, &bits);
        assert_eq!(parts["github"]["trustedAuthors"], json!(["legacy-bot[bot]"]), "алиас");

        e.insert("REVIEW_TRUSTED_AUTHORS".into(), json!(" a[bot] , b[bot] ,"));
        let parts = load_review_config_parts(&e, &bits);
        assert_eq!(parts["github"]["trustedAuthors"], json!(["a[bot]", "b[bot]"]), "новое имя главнее алиаса");

        e.insert("REVIEW_TRUSTED_AUTHORS".into(), json!(""));
        let parts = load_review_config_parts(&e, &bits);
        assert_eq!(parts["github"]["trustedAuthors"], json!([]), "пусто = доверять всем, не дефолт");
        assert_eq!(parts["feedback"]["trustedAuthors"], "");
    }

    #[test]
    fn load_defaults() {
        let e = Map::new();
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["reviewMode"], "moa");
        assert_eq!(parts["httpRetries"], 3);
        assert_eq!(parts["github"]["api"], "https://api.github.com");
        assert_eq!(parts["llm"]["base"], "https://ollama.com/v1");
        assert_eq!(parts["ollama"]["model"], "kimi-k2.7-code");
        assert_eq!(parts["ollama"]["requestTimeoutMs"], 420000);
        assert_eq!(parts["moa"]["label"], "moa-specialist(minimax-m3←security+correctness+architecture)");
        assert_eq!(parts["models"]["fallback"], json!(["deepseek-v4.1-flash"]));
        assert_eq!(parts["gate"]["blockOn"], json!(["critical", "major"]));
        assert_eq!(parts["gate"]["consensusMin"], 1);
        assert_eq!(parts["gate"]["confidenceMin"], 0.8);
        assert_eq!(parts["cost"]["priceMap"], Value::Null);
        assert_eq!(parts["prescan"]["astgrep"]["configPath"], "");
        assert_eq!(parts["limits"]["timeBudgetMs"], 1_200_000);
        assert_eq!(parts["graphify"]["depth"], 2);
        assert_eq!(
            parts["structure"]["graphAnalysis"],
            ".graphify/graphify-out/.graphify_analysis.json"
        );
        assert_eq!(parts["structure"]["trustTier"], "trusted_local");
        assert_eq!(parts["feedback"]["patternsFile"], "");
    }

    #[test]
    fn load_env_beats_profile_and_defaults() {
        let orakul = normalize_profile_facts(
            &json!({ "name": "orakul", "paths": { "exportDir": "out/review", "astgrepConfig": "ops/sg.yml" } }),
            "/repo",
            "s",
        );
        let mut bits = profile_bits(&orakul);
        bits["appDir"] = json!("app");
        let e = env(&[
            ("REVIEW_EXPORT_DIR", "env/export"),
            ("REVIEW_APP_DIR", "packages/api"),
            ("REVIEW_ASTGREP_CONFIG", ""),
            ("OLLAMA_API_KEY", "k"),
        ]);
        let parts = load_review_config_parts(&e, &bits);
        assert_eq!(parts["export"]["dir"], "env/export", "env > профиль");
        assert_eq!(parts["prescan"]["appDir"], "packages/api");
        // `||` по пустой строке: astgrepConfig '' falsy → профиль.
        assert_eq!(parts["prescan"]["astgrep"]["configPath"], "ops/sg.yml");
        assert_eq!(parts["llm"]["key"], "k");
        // REVIEW_APP_DIR — `??`: пустая строка сохранилась бы.
        let e2 = env(&[("REVIEW_APP_DIR", "")]);
        let parts2 = load_review_config_parts(&e2, &bits);
        assert_eq!(parts2["prescan"]["appDir"], "");
    }

    #[test]
    fn load_nullish_vs_or_block_rules() {
        let e = env(&[("REVIEW_PRESCAN_BLOCK_RULES", "")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["prescan"]["blockRules"], json!([]), "?? : пустая строка → []");
        let e2 = env(&[("REVIEW_BLOCK_ON", "")]);
        let p2 = load_review_config_parts(&e2, &profile_bits(&default_profile()));
        assert_eq!(p2["gate"]["blockOn"], json!([]));
    }

    #[test]
    fn load_verify_min_derives_from_actionability_min() {
        // Дефолт — «порог маршрута минус вес сигнала verified» (3), а не константа.
        let parts = load_review_config_parts(&env(&[]), &profile_bits(&default_profile()));
        assert_eq!(parts["actionability"]["threshold"], 4);
        assert_eq!(parts["actionability"]["verifyMin"], 1);

        let e = env(&[("REVIEW_ACTIONABILITY_MIN", "6")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["actionability"]["verifyMin"], 3, "порог поехал — допуск едет за ним");

        // Явное значение перебивает вывод; порог маршрута при этом не меняется.
        let e = env(&[("REVIEW_VERIFY_ADVISORY_MIN", "99")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["actionability"]["verifyMin"], 99);
        assert_eq!(parts["actionability"]["threshold"], 4);
    }

    #[test]
    fn load_new_token_knobs_defaults() {
        let parts = load_review_config_parts(&env(&[]), &profile_bits(&default_profile()));
        assert_eq!(parts["ollama"]["aggregatorEffort"], "low");
        assert_eq!(parts["ollama"]["modelSettings"], json!({}));
        assert_eq!(parts["ollama"]["modelSettingsErrors"], json!([]));
        assert_eq!(parts["verifier"]["contextMargin"], 80);
        let e = env(&[
            ("REVIEW_AGGREGATOR_REASONING_EFFORT", " none "),
            ("REVIEW_VERIFIER_CONTEXT_MARGIN", "20"),
        ]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["ollama"]["aggregatorEffort"], "none", "trim как у REVIEW_REASONING_EFFORT");
        assert_eq!(parts["verifier"]["contextMargin"], 20);
    }

    #[test]
    fn load_block_single_intersects_block_on() {
        let e = env(&[("REVIEW_BLOCK_ON", "critical,minor"), ("REVIEW_BLOCK_SINGLE", "critical,major")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["gate"]["blockSingle"], json!(["critical"]));
    }

    #[test]
    fn load_consensus_clamps() {
        // moa: cap 1.
        let e = env(&[("REVIEW_CONSENSUS", "5")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["gate"]["consensusMin"], 1);
        // consensus: cap = число моделей; без OLLAMA_MODELS одна модель → min(2, 1) = 1.
        let e = env(&[("REVIEW_MODE", "consensus"), ("OLLAMA_MODELS", "m1,m2,m3")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["gate"]["consensusMin"], 2);
        let e = env(&[("REVIEW_MODE", "consensus"), ("OLLAMA_MODELS", "m1,m2,m3"), ("REVIEW_CONSENSUS", "9")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["gate"]["consensusMin"], 3);
        // 0/abc → дефолт режима (2), но cap = 1 модель → min(2, 1) = 1.
        let e = env(&[("REVIEW_MODE", "consensus"), ("REVIEW_CONSENSUS", "abc")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["gate"]["consensusMin"], 1);
    }

    #[test]
    fn load_llm_priority_and_trailing_slash() {
        let e = env(&[
            ("LLM_API_KEY", " llm-k "),
            ("OLLAMA_API_KEY", "ollama-k"),
            ("LLM_BASE_URL", "https://ghe/api/v3/"),
        ]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["llm"]["key"], "llm-k");
        assert_eq!(parts["llm"]["base"], "https://ghe/api/v3");
        let e = env(&[("OLLAMA_API_KEY", "ok"), ("OLLAMA_BASE_URL", "")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["llm"]["key"], "ok");
        assert_eq!(parts["llm"]["base"], "https://ollama.com/v1", "пустой OLLAMA_BASE_URL falsy → дефолт");
    }

    #[test]
    fn load_specialists_and_moa_label() {
        let e = env(&[
            ("MOA_SPECIALIST_MODELS", "security:custom-x"),
            ("MOA_SPECIALIST_ARCHITECTURE_MODEL", "arch-y"),
        ]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        let assignments = &parts["specialists"]["assignments"];
        assert_eq!(assignments[0], json!({ "role": "security", "model": "custom-x" }));
        assert_eq!(assignments[1]["model"], "kimi-k2.7-code");
        assert_eq!(assignments[2]["model"], "arch-y");
        assert_eq!(
            parts["moa"]["label"],
            "moa-specialist(minimax-m3←security+correctness+architecture)"
        );
        // REVIEW_SPECIALISTS=0 → generic label из MOA_REFERENCE_MODELS.
        let e = env(&[("REVIEW_SPECIALISTS", "0"), ("MOA_REFERENCE_MODELS", "a,b")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["moa"]["label"], "moa(minimax-m3←a+b)");
        assert_eq!(parts["specialists"]["enabled"], Value::Bool(false));
    }

    #[test]
    fn load_strict_from_env_or_profile() {
        let strict_profile = normalize_profile_facts(&json!({ "strict": true }), "/r", "s");
        let e = Map::new();
        let parts = load_review_config_parts(&e, &profile_bits(&strict_profile));
        assert_eq!(parts["gate"]["strict"], Value::Bool(true));
        // requireQuote наследует strict, env его перебивает.
        assert_eq!(parts["gate"]["requireQuote"], Value::Bool(true));
        let e = env(&[("REVIEW_REQUIRE_QUOTE", "0")]);
        let parts = load_review_config_parts(&e, &profile_bits(&strict_profile));
        assert_eq!(parts["gate"]["requireQuote"], Value::Bool(false));
        // env strict=1 при нейтральном профиле.
        let e = env(&[("REVIEW_STRICT", "1")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["gate"]["strict"], Value::Bool(true));
    }

    #[test]
    fn load_price_map_variants() {
        let e = env(&[("REVIEW_PRICE_JSON", "{\"m\":{\"in\":1,\"out\":2}}")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["cost"]["priceMap"]["m"]["in"], 1);
        let e = env(&[("REVIEW_PRICE_JSON", "{broken"), ("OLLAMA_API_KEY", "x")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["cost"]["priceMap"], Value::Null);
        let e = env(&[("REVIEW_PRICE_JSON", "[]")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["cost"]["priceMap"], json!([]), "массив — объект по typeof JS");
    }

    #[test]
    fn load_temperature_seed_and_depth_edges() {
        let e = env(&[("REVIEW_TEMPERATURE", ""), ("REVIEW_SEED", "3.5"), ("REVIEW_GRAPHIFY_DEPTH", "99")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["ollama"]["temperature"], 0, "Number('') === 0 → валидный 0");
        assert_eq!(parts["ollama"]["seed"], 7, "не-integer seed → дефолт");
        assert_eq!(parts["graphify"]["depth"], 5, "clamp max 5");
        let e = env(&[("REVIEW_GRAPHIFY_DEPTH", "0")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["graphify"]["depth"], 2, "numEnv 0 не > 0 → дефолт");
    }

    #[test]
    fn load_structure_graph_analysis_follows_override() {
        let e = env(&[("REVIEW_STRUCTURE_GRAPH", "out/custom/graph.json")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["structure"]["graph"], "out/custom/graph.json");
        assert_eq!(parts["structure"]["graphAnalysis"], "out/custom/.graphify_analysis.json");
        let e = env(&[("REVIEW_STRUCTURE_GRAPH_ANALYSIS", "explicit.json")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["structure"]["graphAnalysis"], "explicit.json");
    }

    #[test]
    fn load_models_mode_variants() {
        let e = env(&[("REVIEW_MODE", "CONSENSUS "), ("OLLAMA_MODELS", "m1, m2, m1")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["reviewMode"], "consensus", "trim + lowercase");
        assert_eq!(parts["models"]["list"], json!(["m1", "m2"]));
        assert_eq!(parts["models"]["fallback"], json!(["deepseek-v4.1-flash"]));
        // fallback исключает модели списка.
        let e = env(&[("REVIEW_MODE", "consensus"), ("OLLAMA_MODELS", "deepseek-v4.1-flash,m9")]);
        let parts = load_review_config_parts(&e, &profile_bits(&default_profile()));
        assert_eq!(parts["models"]["fallback"], json!([]));
    }

    #[test]
    fn model_settings_разбор_и_ошибки() {
        let (v, errs) = parse_model_settings(Some(r#"{
            "deepseek/deepseek-v4.1-flash": {"effort": "none", "provider": {"ignore": ["OpenInference"]}},
            "z-ai/glm-5.3-flash": {"effort": "low", "jsonEffort": "medium", "maxTokens": 8000},
            "x/typo": {"efort": "none", "maxTokens": 0, "effort": "max"}
        }"#));
        assert_eq!(v["deepseek/deepseek-v4.1-flash"], json!({"effort": "none", "provider": {"ignore": ["OpenInference"]}}));
        assert_eq!(v["z-ai/glm-5.3-flash"], json!({"effort": "low", "jsonEffort": "medium", "maxTokens": 8000}));
        assert_eq!(v["x/typo"], json!({}), "недопустимое не попадает в настройки");
        assert_eq!(errs.len(), 3, "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("неизвестное поле «efort»")));
        let (v, errs) = parse_model_settings(Some("не json"));
        assert_eq!(v, json!({}));
        assert!(errs[0].contains("не JSON"));
        assert_eq!(parse_model_settings(Some("  ")).0, json!({}));
    }
}
