//! Порт lib/profile.mjs — RFC 16, решение 3: **явный `Profile` вместо singleton**.
//!
//! В JS профиль — process-wide синглтон (`getProfile`/`setProfile`/`resetProfile`),
//! потому что `MARKERS` в lib/constants.mjs — живые getters, а модули импортируются
//! раньше, чем грузится профиль. В Rust этой проблемы нет: профиль резолвится один
//! раз на входе в подкоманду и дальше передаётся ссылкой. Поэтому здесь **нет ни
//! одного `static`/`OnceLock` уровня модуля** — два `Profile::resolve` в одном
//! процессе полностью независимы, и тестам не нужен «один файл = один процесс».
//!
//! Что осталось от JS-обёртки и что переиспользуется:
//!   - сериализуемая нормализация (выбор raw-vs-fallback для regex-источников,
//!     merge `paths`, `appDir`/`appPrefix`, `toolName`/`sarifRulePrefix`, маркеры)
//!     НЕ дублируется: зовём [`crate::config::normalize_profile_facts`] и
//!     [`crate::config::create_markers`];
//!   - здесь поверх фактов — то, что не круглится через JSON: компиляция regex,
//!     `astgrep.scopes`/`allow` как Map/Set, чтение `repoRules` с диска, поиск
//!     файла профиля.
//!
//! Расхождения с JS (осознанные, см. комментарии по месту):
//!   - диалект regex: JS `RegExp` vs `fancy-regex` (lookaround/backrefs есть,
//!     но, например, `\p{...}` и подстановки JS-only отличаются). Битый паттерн
//!     ведёт себя как в JS — падение на fallback, а не на исключение;
//!   - `explicit`-флаг `resolveProfile` (защита явно назначенного профиля от
//!     автопоиска) не портирован: он существовал только ради синглтона. Явный
//!     профиль в Rust — это просто `Profile`, который вызывающий держит у себя
//!     и не переспрашивает у `resolve`.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use fancy_regex::{Regex, RegexBuilder};
use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use crate::config::{
    create_markers, normalize_profile_facts, DEFAULT_E2E_SPEC, DEFAULT_LINTABLE, DEFAULT_LOGIC,
    DEFAULT_MARKER_NAMESPACE,
};

/// Имена файлов профиля, которые движок ищет в корне репозитория-потребителя.
pub const PROFILE_FILENAMES: [&str; 2] = [".ai-reviewer.json", "ai-reviewer.config.json"];

/// Корень пакета ai-reviewer (порт lib/package-root.mjs): от него считаются
/// артефакты самого пакета — ast-grep ruleset, стартовые fp-patterns, fixtures.
/// `rust/` лежит в корне репозитория, поэтому это `CARGO_MANIFEST_DIR/..`.
pub fn package_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

// ---------------------------------------------------------------------------
// Маркеры комментариев
// ---------------------------------------------------------------------------

/// Маркеры комментариев namespace'а: `MARKERS` из lib/constants.mjs, но не
/// живые getters по синглтону, а снимок конкретного профиля.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Markers {
    /// namespace как есть (`ai-review`, `orakul-ai-review`).
    pub ns: String,
    /// Маркер sticky-комментария: `<!-- ns -->`.
    pub review: String,
    /// Маркер inline-комментария: `<!-- ns-inline -->`.
    pub inline: String,
    /// Ключ машинного футера sticky: `ns-data:`.
    pub data_key: String,
}

impl Default for Markers {
    fn default() -> Self {
        Self::new(DEFAULT_MARKER_NAMESPACE)
    }
}

impl Markers {
    /// createMarkers из lib/profile.mjs. Нормализация namespace (trim, пустой →
    /// дефолт) переиспользована из config.rs, чтобы не разъехаться с ним.
    pub fn new(namespace: &str) -> Self {
        let v = create_markers(Some(namespace));
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        Self { ns: s("ns"), review: s("review"), inline: s("inline"), data_key: s("dataKey") }
    }

    /// Источник JS-регекса `dataRe` — ровно та строка, которую собирает
    /// lib/profile.mjs. Нужен паритетным прогонам: сам regex ниже компилируется
    /// с экранированным ключом и с JS-источником побайтово не совпадает.
    pub fn data_re_source(&self) -> String {
        format!(r"<!--\s*{}([\s\S]*?)-->", self.data_key)
    }

    /// Скомпилированный аналог getter'а `dataRe`.
    ///
    /// Два отличия от JS, оба в сторону надёжности:
    ///   - `dataKey` экранируется: namespace приходит из чужого профиля, и
    ///     метасимвол в нём в JS дал бы `SyntaxError` на ровном месте;
    ///   - в JS getter отдаёт СВЕЖИЙ `RegExp` на каждое чтение, потому что у
    ///     `RegExp` есть состояние (`lastIndex`). У `fancy_regex::Regex`
    ///     состояния нет, так что здесь это просто конструктор.
    pub fn data_re(&self) -> Regex {
        let src = format!(r"<!--\s*{}([\s\S]*?)-->", fancy_regex::escape(&self.data_key));
        Regex::new(&src).expect("экранированный dataKey компилируется всегда")
    }

    /// `MARKERS.dataRe.exec(body)?.[1]` — тело первого машинного футера.
    /// Для sticky нужен ПОСЛЕДНИЙ футер: см. [`crate::constants::extract_sticky_footer_raw`].
    pub fn capture_data(&self, body: &str) -> Option<String> {
        self.data_re()
            .captures(body)
            .ok()
            .flatten()
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
    }
}

// ---------------------------------------------------------------------------
// Вставки в системные промпты
// ---------------------------------------------------------------------------

/// `prompts.*` профиля — свободный текст, дописываемый в системные промпты.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prompts {
    pub advisor_extra: String,
    pub specialist_extra: String,
    pub aggregator_extra: String,
    pub verifier_extra: String,
}

// ---------------------------------------------------------------------------
// Компиляция regex-полей профиля
// ---------------------------------------------------------------------------

/// compileCandidate из lib/profile.mjs: кандидат уже выбран в config.rs
/// (raw-vs-fallback), здесь только компиляция с флагом `i` и падение на
/// fallback, если паттерн потребителя не компилируется.
///
/// Возвращает ПАРУ «действующий источник + скомпилированный regex»: config.rs
/// валидацию компилируемостью не делает (это заявленная граница порта — диалект
/// regex у него в фактах остаётся строкой), поэтому `facts.layout.lintable` для
/// битого паттерна всё ещё содержит мусор потребителя. Если положить в
/// `*_source()` кандидата, а в поле — fallback-regex, отчёт и паритетный прогон
/// показывали бы паттерн, по которому НИЧЕГО не отбиралось. В JS такой
/// рассинхронизации нет: там `layout.lintable.source` читается с самого
/// скомпилированного `RegExp`.
///
/// Компиляция ЭАГЕРНАЯ (как `normalizeProfile` в JS), а не ленивая: подмена
/// битого паттерна на fallback обязана случиться при загрузке профиля. Ленивая
/// откладывала бы её до первого обращения, и профиль вёл бы себя по-разному в
/// зависимости от того, дошёл ли прогон до prescan. Ленивость здесь всё равно
/// ничего не экономит: чтобы узнать действующий источник, паттерн надо
/// скомпилировать.
fn compile_candidate(candidate: Option<&str>, fallback: &str) -> (Option<String>, Option<Regex>) {
    let Some(candidate) = candidate else { return (None, None) };
    if let Some(re) = compile_i(candidate) {
        return (Some(candidate.to_string()), Some(re));
    }
    if fallback.is_empty() {
        return (None, None);
    }
    match compile_i(fallback) {
        Some(re) => (Some(fallback.to_string()), Some(re)),
        None => (None, None),
    }
}

/// `new RegExp(src, 'i')`. Причина ошибки не нужна ни одному вызывающему
/// (в JS `catch` тоже безымянный), а `fancy_regex::Error` крупный — отдаём Option.
fn compile_i(src: &str) -> Option<Regex> {
    RegexBuilder::new(src).case_insensitive(true).build().ok()
}

// ---------------------------------------------------------------------------
// layout
// ---------------------------------------------------------------------------

/// Раскладка каталогов репозитория (`layout` профиля). Хранит и исходную строку
/// паттерна (для артефактов/паритета), и скомпилированный regex.
#[derive(Debug)]
pub struct Layout {
    /// Каталог приложения; `""` — корень репозитория.
    pub app_dir: String,
    /// Каталоги-слои, которым можно вернуть префикс `app_dir`.
    pub layer_dirs: Vec<String>,
    /// Каталог, относительно которого профиль читал свои файлы.
    pub base_dir: PathBuf,
    app_prefix_source: Option<String>,
    app_prefix: Option<Regex>,
    lintable_source: Option<String>,
    lintable: Option<Regex>,
    sensitive_source: Option<String>,
    sensitive: Option<Regex>,
    e2e_spec_source: Option<String>,
    e2e_spec: Option<Regex>,
    logic_source: Option<String>,
    logic: Option<Regex>,
}

impl Layout {
    /// `^(?:layer1|layer2)/` — префикс `app_dir` возвращается только своим слоям.
    /// Регистрозависимый, как в JS (`new RegExp(source)` без флага `i`).
    pub fn app_prefix(&self) -> Option<&Regex> {
        self.app_prefix.as_ref()
    }
    /// Какие файлы вообще отдаются в prescan.
    pub fn lintable(&self) -> Option<&Regex> {
        self.lintable.as_ref()
    }
    /// Зоны повышенной цены ошибки сверх встроенных.
    pub fn sensitive(&self) -> Option<&Regex> {
        self.sensitive.as_ref()
    }
    /// Что считать e2e-спекой.
    pub fn e2e_spec(&self) -> Option<&Regex> {
        self.e2e_spec.as_ref()
    }
    /// Классификация файла как «логика» для роутера.
    pub fn logic(&self) -> Option<&Regex> {
        self.logic.as_ref()
    }

    pub fn app_prefix_source(&self) -> Option<&str> {
        self.app_prefix_source.as_deref()
    }
    pub fn lintable_source(&self) -> Option<&str> {
        self.lintable_source.as_deref()
    }
    pub fn sensitive_source(&self) -> Option<&str> {
        self.sensitive_source.as_deref()
    }
    pub fn e2e_spec_source(&self) -> Option<&str> {
        self.e2e_spec_source.as_deref()
    }
    pub fn logic_source(&self) -> Option<&str> {
        self.logic_source.as_deref()
    }
}

// ---------------------------------------------------------------------------
// astgrep
// ---------------------------------------------------------------------------

/// `astgrep` профиля: где правило применимо (`scopes`) и кому оно прощается
/// (`allow`). JS держит их как `Map<string, RegExp>` / `Map<string, Set<string>>`.
#[derive(Debug, Default)]
pub struct Astgrep {
    scopes: IndexMap<String, Regex>,
    scope_sources: IndexMap<String, String>,
    allow: IndexMap<String, BTreeSet<String>>,
}

impl Astgrep {
    /// Scope правила: `None` — правило действует везде (как отсутствие ключа в JS-Map).
    pub fn scope(&self, rule_id: &str) -> Option<&Regex> {
        self.scopes.get(rule_id)
    }
    /// Исходный паттерн scope'а (для отчётов и паритета).
    pub fn scope_source(&self, rule_id: &str) -> Option<&str> {
        self.scope_sources.get(rule_id).map(String::as_str)
    }
    /// Список путей-исключений правила.
    pub fn allow(&self, rule_id: &str) -> Option<&BTreeSet<String>> {
        self.allow.get(rule_id)
    }
    /// Путь прощён этому правилу.
    pub fn is_allowed(&self, rule_id: &str, path: &str) -> bool {
        self.allow.get(rule_id).is_some_and(|s| s.contains(path))
    }
    pub fn scope_ids(&self) -> impl Iterator<Item = &str> {
        self.scopes.keys().map(String::as_str)
    }
    pub fn allow_ids(&self) -> impl Iterator<Item = &str> {
        self.allow.keys().map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

/// `paths` профиля. Внутри — полная карта (`{...дефолты, ...профиль}`, как spread
/// в JS): неизвестные движку ключи потребителя не теряются, но публичный доступ
/// к известным — типизированные аксессоры.
#[derive(Debug, Clone, Default)]
pub struct Paths {
    map: Map<String, Value>,
}

impl Paths {
    fn from_value(v: Option<&Value>) -> Self {
        Self { map: v.and_then(Value::as_object).cloned().unwrap_or_default() }
    }

    /// Значение произвольного ключа (в т.ч. не известного движку); нет ключа → `""`.
    pub fn get(&self, key: &str) -> &str {
        self.map.get(key).and_then(Value::as_str).unwrap_or("")
    }

    pub fn feedback_log(&self) -> &str {
        self.get("feedbackLog")
    }
    pub fn feedback_candidates_dir(&self) -> &str {
        self.get("feedbackCandidatesDir")
    }
    /// `""` → стартовый набор из пакета (feedback/fp-patterns.jsonl).
    pub fn fp_patterns(&self) -> &str {
        self.get("fpPatterns")
    }
    pub fn fp_markdown(&self) -> &str {
        self.get("fpMarkdown")
    }
    pub fn observability_dir(&self) -> &str {
        self.get("observabilityDir")
    }
    pub fn export_dir(&self) -> &str {
        self.get("exportDir")
    }
    pub fn structure_export_dir(&self) -> &str {
        self.get("structureExportDir")
    }
    pub fn coverage_lcov(&self) -> &str {
        self.get("coverageLcov")
    }
    /// `""` → ruleset из пакета (ast-grep/sgconfig.yml).
    pub fn astgrep_config(&self) -> &str {
        self.get("astgrepConfig")
    }

    /// Карта целиком — для передачи в [`crate::config::load_review_config_parts`].
    pub fn to_value(&self) -> Value {
        Value::Object(self.map.clone())
    }
}

// ---------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------

/// Нормализованный профиль прогона. Создаётся один раз (обычно
/// [`Profile::resolve`] на входе в подкоманду) и дальше передаётся ссылкой.
#[derive(Debug)]
pub struct Profile {
    /// Имя профиля (логи, observability).
    pub name: String,
    /// Откуда профиль прочитан: путь к файлу или `(default)`.
    pub source: String,
    /// Каталог профиля: от него считается `repoRulesFile` и ищется AGENTS.md.
    pub base_dir: PathBuf,
    /// namespace маркеров.
    pub marker_namespace: String,
    /// Имя инструмента: SARIF `tool.name`, имена файлов экспорта, User-Agent.
    pub tool_name: String,
    /// Префикс `ruleId` в SARIF.
    pub sarif_rule_prefix: String,
    /// `tool.driver.informationUri` в SARIF.
    pub repository_uri: String,
    /// Блок «правила проекта» для системного промпта (см. [`read_repo_rules`]).
    pub repo_rules: String,
    /// Профиль требует strict-режима гейта.
    pub strict: bool,
    prompts: Prompts,
    markers: Markers,
    layout: Layout,
    astgrep: Astgrep,
    paths: Paths,
    facts: Value,
}

impl Profile {
    /// Сырой JSON → нормализованный профиль. Сериализуемая часть — в config.rs,
    /// здесь только компиляция regex, Map/Set и чтение правил репозитория.
    ///
    /// `cwd` нужен ровно для одного случая: `base_dir` профиля может быть
    /// `ops/ai-review/`, и тогда AGENTS.md ищется ещё и в корне прогона
    /// (в JS это `process.cwd()` внутри `readRepoRules`).
    pub fn normalize(raw: &Value, base_dir: &Path, source: &str, cwd: &Path) -> Self {
        let facts = normalize_profile_facts(raw, &base_dir.to_string_lossy(), source);
        let empty = Map::new();
        let raw_obj = raw.as_object().unwrap_or(&empty);

        let str_fact = |k: &str| facts.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let marker_namespace = str_fact("markerNamespace");

        let prompts_v = facts.get("prompts");
        let prompt = |k: &str| {
            prompts_v
                .and_then(|p| p.get(k))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };

        let layout_v = facts.get("layout");
        let lsrc = |k: &str| {
            layout_v
                .and_then(|l| l.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let app_prefix_source = lsrc("appPrefixSource");
        // appPrefix — регистрозависимый (в JS собирается без флага `i`); он не
        // приходит от потребителя, а собирается движком из layerDirs, поэтому
        // компиляция не может провалиться на чужом диалекте.
        let app_prefix = app_prefix_source.as_deref().and_then(|s| Regex::new(s).ok());
        let (lintable_source, lintable) = compile_candidate(lsrc("lintable").as_deref(), DEFAULT_LINTABLE);
        let (sensitive_source, sensitive) = compile_candidate(lsrc("sensitive").as_deref(), "");
        let (e2e_spec_source, e2e_spec) = compile_candidate(lsrc("e2eSpec").as_deref(), DEFAULT_E2E_SPEC);
        let (logic_source, logic) = compile_candidate(lsrc("logic").as_deref(), DEFAULT_LOGIC);
        let layout = Layout {
            app_dir: layout_v
                .and_then(|l| l.get("appDir"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            layer_dirs: layout_v
                .and_then(|l| l.get("layerDirs"))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
            base_dir: base_dir.to_path_buf(),
            lintable,
            sensitive,
            e2e_spec,
            logic,
            app_prefix,
            app_prefix_source,
            lintable_source,
            sensitive_source,
            e2e_spec_source,
            logic_source,
        };

        let mut astgrep = Astgrep::default();
        if let Some(scopes) = facts.get("astgrep").and_then(|a| a.get("scopes")).and_then(Value::as_object) {
            for (rule_id, src) in scopes {
                let Some(src) = src.as_str() else { continue };
                // Битый scope выбрасывается (fallback пустой) — как `.filter(re !== null)` в JS.
                // Правило без scope действует ВЕЗДЕ, так что выброшенный scope
                // расширяет правило, а не сужает: молча сузить было бы хуже.
                if let (Some(src), Some(re)) = compile_candidate(Some(src), "") {
                    astgrep.scopes.insert(rule_id.clone(), re);
                    astgrep.scope_sources.insert(rule_id.clone(), src);
                }
            }
        }
        if let Some(allow) = facts.get("astgrep").and_then(|a| a.get("allow")).and_then(Value::as_object) {
            for (rule_id, paths) in allow {
                let set: BTreeSet<String> = paths
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                astgrep.allow.insert(rule_id.clone(), set);
            }
        }

        Self {
            name: str_fact("name"),
            source: source.to_string(),
            base_dir: base_dir.to_path_buf(),
            tool_name: str_fact("toolName"),
            sarif_rule_prefix: str_fact("sarifRulePrefix"),
            repository_uri: str_fact("repositoryUri"),
            repo_rules: read_repo_rules(raw_obj, base_dir, cwd),
            strict: facts.get("strict") == Some(&Value::Bool(true)),
            prompts: Prompts {
                advisor_extra: prompt("advisorExtra"),
                specialist_extra: prompt("specialistExtra"),
                aggregator_extra: prompt("aggregatorExtra"),
                verifier_extra: prompt("verifierExtra"),
            },
            markers: Markers::new(&marker_namespace),
            marker_namespace,
            layout,
            astgrep,
            paths: Paths::from_value(facts.get("paths")),
            facts,
        }
    }

    /// Встроенный нейтральный профиль (`DEFAULT_PROFILE` из lib/profile.mjs):
    /// маркеры `ai-review`, prescan видит любой TS/JS в корне, правил проекта нет.
    /// `pkg_root` — корень пакета ([`package_root`]), он же `base_dir`.
    pub fn default_builtin(pkg_root: &Path) -> Self {
        // cwd = pkg_root: у встроенного профиля нет «репозитория рядом», и чужой
        // AGENTS.md из каталога запуска не должен подмешиваться в дефолт.
        Self::normalize(&json!({}), pkg_root, "(default)", pkg_root)
    }

    /// Профиль из JSON-файла. Ошибка возвращается вызывающему — это он решает,
    /// fail-open это или нет (`resolve` ниже — fail-open).
    pub fn load_file(path: &Path) -> Result<Self> {
        Self::load_file_at(path, &current_dir())
    }

    /// [`Profile::load_file`] с явным каталогом прогона (нужен для AGENTS.md).
    pub fn load_file_at(path: &Path, cwd: &Path) -> Result<Self> {
        let abs = absolutize(path, cwd);
        let text = fs::read_to_string(&abs)
            .with_context(|| format!("не читается файл профиля {}", abs.display()))?;
        let raw: Value = serde_json::from_str(&text)
            .with_context(|| format!("невалидный JSON профиля {}", abs.display()))?;
        let base_dir = abs.parent().map(Path::to_path_buf).unwrap_or_else(|| cwd.to_path_buf());
        Ok(Self::normalize(&raw, &base_dir, &abs.to_string_lossy(), cwd))
    }

    /// Профиль для прогона: `REVIEW_PROFILE` → автопоиск в `cwd` → встроенный дефолт.
    ///
    /// Fail-open: нечитаемый профиль не роняет ревью — пишем строку в `log` и
    /// продолжаем на дефолтном. `Result` — на случай, если однажды дефолт тоже
    /// перестанет собираться; сейчас `Err` не возвращается.
    pub fn resolve(
        env: &HashMap<String, String>,
        cwd: &Path,
        log: impl Fn(&str),
    ) -> Result<Self> {
        let named = env.get("REVIEW_PROFILE").map(|s| s.trim()).unwrap_or("");
        if !named.is_empty() {
            return match Self::load_file_at(Path::new(named), cwd) {
                Ok(p) => Ok(p),
                Err(e) => {
                    log(&format!(
                        "profile: не удалось прочитать {named} ({e}) — работаем на дефолтном профиле"
                    ));
                    Ok(Self::default_builtin(&package_root()))
                }
            };
        }
        for name in PROFILE_FILENAMES {
            if let Ok(p) = Self::load_file_at(&cwd.join(name), cwd) {
                return Ok(p);
            }
        }
        Ok(Self::default_builtin(&package_root()))
    }

    pub fn markers(&self) -> &Markers {
        &self.markers
    }
    pub fn layout(&self) -> &Layout {
        &self.layout
    }
    pub fn astgrep(&self) -> &Astgrep {
        &self.astgrep
    }
    pub fn paths(&self) -> &Paths {
        &self.paths
    }
    pub fn prompts(&self) -> &Prompts {
        &self.prompts
    }

    /// Путь к ruleset'у ast-grep из профиля; `""` — ruleset самого пакета
    /// (`ast-grep/sgconfig.yml` от [`package_root`]).
    ///
    /// Алиас `paths().astgrep_config()`, а не отдельное поле: значение живёт в
    /// одном месте (`paths`), иначе профиль с перекрытым `astgrepConfig` мог бы
    /// разъехаться сам с собой. `REVIEW_ASTGREP_CONFIG` перебивает его выше, в
    /// [`crate::config::load_review_config_parts`] (lib/config.mjs:304) —
    /// профиль про env не знает.
    pub fn astgrep_config(&self) -> &str {
        self.paths.astgrep_config()
    }

    /// Нормализованные факты как JSON — тот же объект, что отдаёт op
    /// `normalizeProfile` бинаря config. Его ждут потребители профиля,
    /// работающие на `Value` (например `export::build_sarif`).
    pub fn facts(&self) -> &Value {
        &self.facts
    }

    /// Аргумент `profile` для [`crate::config::load_review_config_parts`]
    /// (зеркало литерала из lib/config.mjs:80-84).
    pub fn config_input(&self) -> Value {
        json!({
            "strict": self.strict,
            "appDir": self.layout.app_dir,
            "paths": self.paths.to_value(),
        })
    }
}

// ---------------------------------------------------------------------------
// Правила репозитория и пути
// ---------------------------------------------------------------------------

/// readRepoRules из lib/profile.mjs.
///
/// Порядок: `repoRules` (как есть) → `repoRulesFile` (от `base_dir`; нечитаемый
/// файл даёт пустые правила и НЕ проваливается дальше — так же, как в JS) →
/// AGENTS.md/agents.md рядом с профилем → AGENTS.md/agents.md в каталоге прогона,
/// если он отличается от `base_dir`.
fn read_repo_rules(raw: &Map<String, Value>, base_dir: &Path, cwd: &Path) -> String {
    if let Some(Value::String(s)) = raw.get("repoRules") {
        if !s.trim().is_empty() {
            return s.clone();
        }
    }
    if let Some(Value::String(rel)) = raw.get("repoRulesFile") {
        if !rel.trim().is_empty() {
            let file = absolutize(Path::new(rel), base_dir);
            return fs::read_to_string(file).map(|t| t.trim().to_string()).unwrap_or_default();
        }
    }
    // De facto стандарт (Cursor / PR-Agent / OpenAI): AGENTS.md в корне репо-потребителя.
    // Только когда профиль не задал явных правил — иначе чужой AGENTS.md не перетирает профиль.
    for dir in agents_lookup_dirs(base_dir, cwd) {
        for name in ["AGENTS.md", "agents.md"] {
            if let Ok(text) = fs::read_to_string(dir.join(name)) {
                let text = text.trim();
                if !text.is_empty() {
                    return text.to_string();
                }
            }
        }
    }
    String::new()
}

fn agents_lookup_dirs(base_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let base = absolutize(base_dir, cwd);
    let run = absolutize(cwd, cwd);
    if base == run {
        vec![base]
    } else {
        vec![base, run]
    }
}

/// profilePath из lib/profile.mjs: путь из профиля/env → абсолютный
/// (относительные — от корня репозитория-потребителя). Пустая строка → пустая.
pub fn profile_path(value: &str, cwd: &Path) -> String {
    if value.is_empty() {
        return String::new();
    }
    absolutize(Path::new(value), cwd).to_string_lossy().into_owned()
}

/// Аналог `path.resolve(base, p)`: абсолютный путь + лексическая нормализация
/// `.`/`..` (без обращения к ФС, как в Node).
fn absolutize(p: &Path, base: &Path) -> PathBuf {
    let joined = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
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
    out
}

fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// Тесты — порт test/profile.test.mjs (нейтральный профиль) и reference-профиль
// (test/helpers/reference-profile.mjs, profiles/orakul.json).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        package_root()
    }

    fn orakul_path() -> PathBuf {
        repo_root().join("profiles/orakul.json")
    }

    fn builtin() -> Profile {
        Profile::default_builtin(&repo_root())
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ai-reviewer-profile-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_profile_neutral_markers_and_tool_name() {
        let p = builtin();
        assert_eq!(p.name, "default");
        assert_eq!(p.markers().review, "<!-- ai-review -->");
        assert_eq!(p.markers().inline, "<!-- ai-review-inline -->");
        assert_eq!(p.markers().data_key, "ai-review-data:");
        assert_eq!(p.tool_name, "ai-review");
        assert_eq!(p.sarif_rule_prefix, "ai-review");
    }

    #[test]
    fn markers_follow_profile_not_import_time_snapshot() {
        // В JS это проверка живых getter'ов MARKERS; в Rust — что маркеры
        // берутся из ЭТОГО профиля, а не из дефолта.
        let orakul = Profile::load_file(&orakul_path()).unwrap();
        assert_eq!(orakul.markers().review, "<!-- orakul-ai-review -->");
        assert_eq!(orakul.markers().inline, "<!-- orakul-ai-review-inline -->");
        let got = orakul.markers().capture_data("<!-- orakul-ai-review-data:{\"a\":1} -->");
        assert_eq!(got.unwrap().trim(), "{\"a\":1}");
    }

    #[test]
    fn data_re_source_matches_js_shape() {
        let m = Markers::new("acme");
        assert_eq!(m.data_re_source(), r"<!--\s*acme-data:([\s\S]*?)-->");
    }

    #[test]
    fn create_markers_empty_namespace_falls_back_to_default() {
        assert_eq!(Markers::new("").review, "<!-- ai-review -->");
        assert_eq!(Markers::new("   ").inline, "<!-- ai-review-inline -->");
        assert_eq!(Markers::new("acme-review").data_key, "acme-review-data:");
        assert_eq!(Markers::default().ns, "ai-review");
    }

    #[test]
    fn no_profile_means_no_repo_rules() {
        assert_eq!(builtin().repo_rules, "");
        assert_eq!(builtin().prompts().aggregator_extra, "");
    }

    #[test]
    fn default_layout_is_repo_root_without_invented_app_dir() {
        let p = builtin();
        assert_eq!(p.layout().app_dir, "");
        assert!(p.layout().layer_dirs.is_empty());
        assert!(p.layout().app_prefix().is_none());
        let lintable = p.layout().lintable().unwrap();
        assert!(lintable.is_match("src/index.ts").unwrap());
        assert!(lintable.is_match("packages/api/handler.tsx").unwrap());
        assert!(!lintable.is_match("README.md").unwrap());
        assert!(p.layout().sensitive().is_none(), "без профиля чувствительных зон нет");
    }

    #[test]
    fn orakul_profile_layout_and_astgrep() {
        let p = Profile::load_file(&orakul_path()).unwrap();
        assert_eq!(p.name, "orakul");
        assert_eq!(p.tool_name, "orakul-ai-review");
        assert_eq!(p.sarif_rule_prefix, "orakul");
        assert_eq!(p.repository_uri, "https://github.com/Orange-hanter/Orakul");

        assert_eq!(p.layout().app_dir, "app");
        assert!(p.layout().layer_dirs.iter().any(|d| d == "server"));
        assert!(p.layout().layer_dirs.iter().any(|d| d == "shared"));
        assert!(p.layout().layer_dirs.iter().any(|d| d == "client"));
        let prefix = p.layout().app_prefix().unwrap();
        assert!(prefix.is_match("server/a.ts").unwrap());
        // Каталог вне layerDirs не должен уезжать под app/.
        assert!(!prefix.is_match("docs/a.ts").unwrap());

        let lintable = p.layout().lintable().unwrap();
        assert!(lintable.is_match("app/shared/round.ts").unwrap());
        assert!(!lintable.is_match("docs/a.ts").unwrap());
        // Ни один паттерн реального профиля не должен ТИХО упасть на дефолт
        // движка: fancy-regex обязан переварить диалект, на котором написан
        // orakul.json. `*_source()` тут — детектор такой подмены.
        assert_ne!(p.layout().lintable_source(), Some(DEFAULT_LINTABLE));
        assert!(p.layout().lintable_source().unwrap().starts_with("^app/(?:shared|server"));
        assert_ne!(p.layout().e2e_spec_source(), Some(DEFAULT_E2E_SPEC));
        assert_ne!(p.layout().logic_source(), Some(DEFAULT_LOGIC));
        assert_eq!(p.astgrep().scope_ids().count(), 7, "все scopes скомпилировались");
        assert!(p.layout().sensitive().unwrap().is_match("ops/pr-review/x.ts").unwrap());
        assert!(p.layout().e2e_spec().unwrap().is_match("app/e2e/login.spec.ts").unwrap());

        // scope и allowlist ast-grep берутся из профиля.
        let scope = p.astgrep().scope("raw-x-forwarded-for").unwrap();
        assert!(scope.is_match("app/server/auth.ts").unwrap());
        assert!(!scope.is_match("app/client/auth.ts").unwrap());
        assert!(p.astgrep().is_allowed("raw-x-forwarded-for", "app/server/net.ts"));
        assert!(!p.astgrep().is_allowed("raw-x-forwarded-for", "app/server/auth.ts"));
        assert!(p.astgrep().scope("client-imports-server").is_some());
        assert!(p.astgrep().allow("client-imports-server").is_none());

        assert_eq!(p.paths().feedback_log(), "ops/pr-review/feedback/fp-feedback.jsonl");
        assert_eq!(p.paths().export_dir(), ".graphify/graphify-out/review-export");
        assert_eq!(p.astgrep_config(), "ops/pr-review/ast-grep/sgconfig.yml");
        assert_eq!(p.astgrep_config(), p.paths().astgrep_config(), "одно значение, не два");
        // structureExportDir профиль не задаёт — остаётся дефолт движка.
        assert_eq!(p.paths().structure_export_dir(), ".ai-review/structure");
    }

    #[test]
    fn public_benchmark_profile_is_layoutless_but_renames_everything() {
        // Второй профиль пакета: раскладки нет (чужие репозитории), но namespace,
        // toolName и пути артефактов уводятся в свой угол, чтобы прогон бенчмарка
        // не смешивался с боевым.
        let p = Profile::load_file(&repo_root().join("profiles/public-benchmark.json")).unwrap();
        assert_eq!(p.name, "public-benchmark");
        assert_eq!(p.markers().review, "<!-- ai-review-bench -->");
        assert_eq!(p.tool_name, "ai-review-bench");
        assert_eq!(p.layout().app_dir, "");
        assert!(p.layout().app_prefix().is_none());
        assert!(p.layout().sensitive().is_none());
        // logic шире дефолтного: в наборе Python/Go/Ruby/Java.
        let logic = p.layout().logic().unwrap();
        assert!(logic.is_match("app/main.py").unwrap());
        assert!(logic.is_match("cmd/serve.go").unwrap());
        assert!(!logic.is_match("README.md").unwrap());
        assert!(p.repo_rules.contains("Дифф взят из чужого открытого репозитория"));
        assert_eq!(p.prompts().advisor_extra, "", "профиль его не задаёт");
        assert!(p.prompts().verifier_extra.starts_with("Отклоняй повышение"));
        assert_eq!(p.paths().export_dir(), ".ai-review/public-benchmark/export");
        // Ключи, которых профиль не касается, остаются дефолтами движка.
        assert_eq!(p.paths().coverage_lcov(), "coverage/lcov.info");
    }

    #[test]
    fn orakul_profile_repo_rules_from_file() {
        // repoRulesFile читается относительно каталога профиля, а не cwd прогона.
        let p = Profile::load_file_at(&orakul_path(), Path::new("/")).unwrap();
        assert!(p.repo_rules.contains("Project Orakul"));
        assert!(p.repo_rules.contains("shared/round.ts"));
    }

    #[test]
    fn config_input_mirrors_js_literal() {
        let p = Profile::load_file(&orakul_path()).unwrap();
        let v = p.config_input();
        assert_eq!(v["strict"], Value::Bool(false));
        assert_eq!(v["appDir"], "app");
        assert_eq!(v["paths"]["exportDir"], ".graphify/graphify-out/review-export");
    }

    #[test]
    fn broken_regex_does_not_break_load_and_allow_accepts_scalar() {
        let raw = json!({
            "name": "acme",
            "markerNamespace": "acme-review",
            "layout": { "appDir": "app/", "layerDirs": ["server"], "lintable": "([unclosed" },
            "astgrep": { "scopes": { "r1": "^app/server/" }, "allow": { "r1": "app/server/ok.ts" } },
        });
        let p = Profile::normalize(&raw, Path::new("/nowhere"), "", Path::new("/nowhere"));
        assert_eq!(p.markers().review, "<!-- acme-review -->");
        assert_eq!(p.tool_name, "acme-review", "toolName по умолчанию = namespace");
        assert_eq!(p.layout().app_dir, "app", "хвостовой слэш срезается");
        // Битый паттерн → дефолтный lintable движка.
        assert_eq!(p.layout().lintable_source(), Some(DEFAULT_LINTABLE));
        assert!(p.layout().lintable().unwrap().is_match("a.ts").unwrap());
        // Факты config.rs компилируемость НЕ проверяют (заявленная граница порта),
        // поэтому мусор потребителя там остаётся — а `*_source()` обязан
        // показывать паттерн, по которому реально идёт отбор.
        assert_eq!(p.facts()["layout"]["lintable"], "([unclosed");
        assert!(p.astgrep().is_allowed("r1", "app/server/ok.ts"), "строка вместо массива");
    }

    #[test]
    fn broken_layout_pattern_without_fallback_yields_none() {
        // sensitive fallback пустой: битый паттерн даёт «зон нет», а не панику.
        let raw = json!({ "layout": { "sensitive": "([unclosed" } });
        let p = Profile::normalize(&raw, Path::new("/nowhere"), "", Path::new("/nowhere"));
        assert!(p.layout().sensitive().is_none());
        assert_eq!(p.layout().sensitive_source(), None);
    }

    #[test]
    fn broken_astgrep_scope_is_dropped_so_rule_applies_everywhere() {
        let raw = json!({ "astgrep": { "scopes": { "r1": "([unclosed", "r2": "^app/" } } });
        let p = Profile::normalize(&raw, Path::new("/nowhere"), "", Path::new("/nowhere"));
        assert!(p.astgrep().scope("r1").is_none(), "битый scope выброшен");
        assert_eq!(p.astgrep().scope_source("r1"), None);
        assert_eq!(p.astgrep().scope_source("r2"), Some("^app/"));
        assert_eq!(p.astgrep().scope_ids().collect::<Vec<_>>(), vec!["r2"]);
    }

    #[test]
    fn default_profile_has_no_astgrep_scopes_and_uses_package_ruleset() {
        let p = builtin();
        // Без профиля правило ast-grep действует ровно там, где его нашёл сам ast-grep.
        assert!(p.astgrep().scope("raw-x-forwarded-for").is_none());
        assert!(!p.astgrep().is_allowed("raw-x-forwarded-for", "app/server/net.ts"));
        assert_eq!(p.astgrep().scope_ids().count(), 0);
        assert_eq!(p.astgrep().allow_ids().count(), 0);
        assert_eq!(p.astgrep_config(), "", "'' → ruleset из пакета");
    }

    #[test]
    fn relative_review_profile_resolves_against_cwd() {
        let dir = tmp_dir("relative");
        fs::create_dir_all(dir.join("ops")).unwrap();
        fs::write(dir.join("ops/p.json"), r#"{"name":"rel","markerNamespace":"rel-ns"}"#).unwrap();
        let mut env = HashMap::new();
        env.insert("REVIEW_PROFILE".to_string(), "ops/p.json".to_string());
        let p = Profile::resolve(&env, &dir, |_| {}).unwrap();
        assert_eq!(p.name, "rel");
        assert_eq!(p.markers().review, "<!-- rel-ns -->");
        // baseDir — каталог профиля, а не каталог прогона: от него читается repoRulesFile.
        assert_eq!(p.base_dir, dir.join("ops"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_profile_fails_open_to_default_with_log() {
        let dir = tmp_dir("broken");
        let broken = dir.join("broken.json");
        fs::write(&broken, "{ this is not json").unwrap();
        let mut env = HashMap::new();
        env.insert("REVIEW_PROFILE".to_string(), broken.to_string_lossy().into_owned());
        let logs = std::cell::RefCell::new(Vec::<String>::new());
        let p = Profile::resolve(&env, &dir, |m| logs.borrow_mut().push(m.to_string())).unwrap();
        assert_eq!(p.name, "default");
        assert!(logs.borrow().join("\n").contains("дефолтном профиле"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_finds_profile_by_filename_in_cwd() {
        let dir = tmp_dir("autofind");
        fs::write(
            dir.join(".ai-reviewer.json"),
            r#"{"name":"found","markerNamespace":"found-ns"}"#,
        )
        .unwrap();
        let p = Profile::resolve(&HashMap::new(), &dir, |_| {}).unwrap();
        assert_eq!(p.name, "found");
        assert_eq!(p.markers().data_key, "found-ns-data:");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_without_anything_is_builtin_default() {
        let dir = tmp_dir("empty");
        let p = Profile::resolve(&HashMap::new(), &dir, |_| {}).unwrap();
        assert_eq!(p.name, "default");
        assert_eq!(p.source, "(default)");
        fs::remove_dir_all(&dir).ok();
    }

    /// Главная проверка решения 3 RFC 16: singleton'а больше нет.
    #[test]
    fn two_resolves_in_one_process_are_independent() {
        let dir = tmp_dir("isolation");
        let a = dir.join("a.json");
        let b = dir.join("b.json");
        fs::write(&a, r#"{"name":"a","markerNamespace":"a-ns","layout":{"appDir":"app"}}"#).unwrap();
        fs::write(&b, r#"{"name":"b","markerNamespace":"b-ns"}"#).unwrap();

        let mut env_a = HashMap::new();
        env_a.insert("REVIEW_PROFILE".to_string(), a.to_string_lossy().into_owned());
        let mut env_b = HashMap::new();
        env_b.insert("REVIEW_PROFILE".to_string(), b.to_string_lossy().into_owned());

        let pa = Profile::resolve(&env_a, &dir, |_| {}).unwrap();
        let pb = Profile::resolve(&env_b, &dir, |_| {}).unwrap();
        // Второй резолв не перетёр первый: в JS это было бы невозможно проверить
        // в одном процессе — там setProfile меняет глобальный active.
        assert_eq!(pa.name, "a");
        assert_eq!(pb.name, "b");
        assert_eq!(pa.markers().review, "<!-- a-ns -->");
        assert_eq!(pb.markers().review, "<!-- b-ns -->");
        assert_eq!(pa.layout().app_dir, "app");
        assert_eq!(pb.layout().app_dir, "");

        // И встроенный дефолт рядом с ними тоже не задет.
        let def = builtin();
        assert_eq!(def.markers().review, "<!-- ai-review -->");
        assert_eq!(pa.markers().review, "<!-- a-ns -->");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn agents_md_next_to_profile_is_picked_up() {
        let dir = tmp_dir("agents");
        fs::write(dir.join("AGENTS.md"), "# Agent rules\nPrefer shared/round.ts\n").unwrap();
        fs::write(
            dir.join("profile.json"),
            r#"{"name":"with-agents","markerNamespace":"t"}"#,
        )
        .unwrap();
        let p = Profile::load_file_at(&dir.join("profile.json"), &dir).unwrap();
        assert!(p.repo_rules.contains("Prefer shared/round.ts"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_repo_rules_beats_agents_md() {
        let dir = tmp_dir("agents-vs-rules");
        fs::write(dir.join("AGENTS.md"), "AGENTS_MARKER").unwrap();
        fs::write(
            dir.join("profile.json"),
            r#"{"name":"p","repoRules":"PROFILE_MARKER"}"#,
        )
        .unwrap();
        let p = Profile::load_file_at(&dir.join("profile.json"), &dir).unwrap();
        assert_eq!(p.repo_rules, "PROFILE_MARKER");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_repo_rules_file_does_not_fall_through_to_agents_md() {
        // Паритет с JS: readRepoRules возвращает '' из ветки repoRulesFile и
        // НЕ идёт дальше искать AGENTS.md.
        let dir = tmp_dir("rules-file-missing");
        fs::write(dir.join("AGENTS.md"), "AGENTS_MARKER").unwrap();
        fs::write(
            dir.join("profile.json"),
            r#"{"name":"p","repoRulesFile":"no-such-file.md"}"#,
        )
        .unwrap();
        let p = Profile::load_file_at(&dir.join("profile.json"), &dir).unwrap();
        assert_eq!(p.repo_rules, "");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompts_are_normalized_and_trimmed() {
        let raw = json!({ "name": "p", "prompts": {
            "aggregatorExtra": "  CUSTOM_AGG_MARKER  ",
            "verifierExtra": "CUSTOM_VER_MARKER",
            "advisorExtra": "CUSTOM_ADV_MARKER",
        }});
        let p = Profile::normalize(&raw, Path::new("/nowhere"), "", Path::new("/nowhere"));
        assert_eq!(p.prompts().aggregator_extra, "CUSTOM_AGG_MARKER");
        assert_eq!(p.prompts().verifier_extra, "CUSTOM_VER_MARKER");
        assert_eq!(p.prompts().advisor_extra, "CUSTOM_ADV_MARKER");
        assert_eq!(p.prompts().specialist_extra, "");
    }

    #[test]
    fn paths_keep_unknown_consumer_keys() {
        let raw = json!({ "paths": { "exportDir": "out/export", "customThing": "x/y" } });
        let p = Profile::normalize(&raw, Path::new("/nowhere"), "", Path::new("/nowhere"));
        assert_eq!(p.paths().export_dir(), "out/export");
        assert_eq!(p.paths().get("customThing"), "x/y");
        assert_eq!(p.paths().feedback_log(), ".ai-review/feedback/fp-feedback.jsonl");
    }

    #[test]
    fn profile_path_resolves_relative_against_cwd() {
        assert_eq!(profile_path("", Path::new("/repo")), "");
        assert_eq!(profile_path("/abs/x", Path::new("/repo")), "/abs/x");
        assert_eq!(profile_path("./out/x", Path::new("/repo")), "/repo/out/x");
        assert_eq!(profile_path("../x", Path::new("/repo/sub")), "/repo/x");
    }

    #[test]
    fn facts_are_the_config_normalize_profile_shape() {
        let p = Profile::load_file(&orakul_path()).unwrap();
        assert_eq!(p.facts()["toolName"], "orakul-ai-review");
        assert_eq!(p.facts()["sarifRulePrefix"], "orakul");
        assert_eq!(p.facts()["layout"]["appDir"], "app");
    }
}
