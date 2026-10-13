//! Порт `lib/observability.mjs` (RFC 20, задача T16): JSONL-лог прогона.
//!
//! Чистые функции `countBySeverity`/`slimFindings` уже портированы в
//! [`crate::metrics`] (там же живёт агрегация по этим же записям) — здесь они
//! только реэкспортируются, чтобы у Rust-потребителя была та же точка входа,
//! что у JS (`lib/observability.mjs:47-60`).
//!
//! Новое здесь — [`Observability`], порт `createObservability`
//! (`lib/observability.mjs:94-151`).
//!
//! # Что инъектируется и почему
//!
//! В JS часы (`Date.now`, `new Date`) и `randomBytes` берутся из среды, поэтому
//! запись лога невоспроизводима. В Rust оба источника недетерминизма — параметры
//! конструктора:
//!
//! - `now: impl Fn() -> u64` — эпоха в миллисекундах. Golden-harness (T25)
//!   замораживает часы, и тогда `ts`/`duration_ms` в JSONL совпадают побайтно;
//! - `run_id: Option<String>` — `runId` остаётся **единственным** источником
//!   случайности пайплайна, так что его тоже можно задать извне. `None` даёт
//!   JS-формат по умолчанию: `Date.now().toString(36) + '-' + randomBytes(4).hex`
//!   (`lib/observability.mjs:89-91`).
//!
//! # Отличие от JS: очередь вместо цепочки промисов
//!
//! JS сериализует записи через `writeChain` и делает `appendFile` на каждое
//! событие. Здесь события копятся в `queue`, а на диск уходят одним `append`
//! в [`Observability::flush`] / [`Observability::drain`]. Наблюдаемый результат
//! тот же: тот же файл, тот же порядок строк, `run_end` последний — но при
//! аварийном завершении процесса до `flush` лог остаётся пустым, тогда как JS
//! успевал дописать ранние события. Это осознанный размен: пайплайн зовёт
//! `flush` в `finally` (`pipeline.mjs:239-242`), а одна запись вместо N
//! убирает межпроцессное перемешивание строк при параллельных прогонах.
//!
//! Fail-open как в JS: ошибка записи уходит в лог и не поднимается наружу
//! (`lib/observability.mjs:117-119`) — сбой наблюдаемости не роняет ревью.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Value};

use crate::profile::Profile;

pub use crate::metrics::{count_by_severity, slim_findings};

/// `PR_TITLE_LOG_MAX` (`lib/observability.mjs:47`): заголовок PR режется перед
/// записью в `run_start` — лог не место для длинных строк из PR.
pub const PR_TITLE_LOG_MAX: usize = 80;

/// `MAX_SLIM` (`lib/observability.mjs:45`) — дефолтный лимит [`slim_findings`],
/// который в JS задан значением аргумента по умолчанию.
pub const MAX_SLIM: usize = 40;

/// Имя файла лога по умолчанию (`lib/observability.mjs:105`).
pub const DEFAULT_LOG_FILE: &str = "pr-review-runs.jsonl";

/// Каталог лога по умолчанию — совпадает с `observabilityDir` нейтрального
/// профиля (`lib/profile.mjs:86`, `rust/src/config.rs:375`).
pub const DEFAULT_LOG_DIR: &str = ".ai-review/observability";

/// Сток логов прогона (`deps.log`); соглашение то же, что в `llm.rs`/`github.rs`.
pub type LogSink = Box<dyn Fn(&str) + Send + Sync>;

/// Часы прогона: эпоха в миллисекундах (порт `Date.now()`).
pub type Clock = Box<dyn Fn() -> u64 + Send + Sync>;

// ---------------------------------------------------------------------------
// Конфиг
// ---------------------------------------------------------------------------

/// Секция `cfg.observability` из замороженного конфига (`lib/config.mjs:309-313`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservabilityConfig {
    pub enabled: bool,
    pub dir: String,
    pub file: String,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: DEFAULT_LOG_DIR.to_string(),
            file: DEFAULT_LOG_FILE.to_string(),
        }
    }
}

impl ObservabilityConfig {
    /// Читает `cfg.observability` из JSON-конфига ([`crate::config`]).
    ///
    /// Паритет с `lib/observability.mjs:95-106`:
    /// - `enabled` выключен **только** явным `false` (`obsCfg.enabled !== false`),
    ///   поэтому `null`/отсутствие/`0`/`"нет"` оставляют лог включённым;
    /// - `dir`/`file` подхватываются по JS-truthy (`||`): пустая строка → дефолт.
    pub fn from_config(cfg: &Value) -> Self {
        Self::from_config_with_dir(cfg, DEFAULT_LOG_DIR)
    }

    /// То же, но с каталогом профиля как запасным вариантом — точный порт
    /// `obsCfg.dir || getProfile().paths.observabilityDir`
    /// (`lib/observability.mjs:104`). В Rust профиль не process-wide singleton,
    /// поэтому передаётся явно.
    pub fn from_config_with_profile(cfg: &Value, profile: &Profile) -> Self {
        Self::from_config_with_dir(cfg, profile.paths().observability_dir())
    }

    fn from_config_with_dir(cfg: &Value, fallback_dir: &str) -> Self {
        let obs = cfg.get("observability");
        let str_or = |key: &str, fallback: &str| -> String {
            match obs.and_then(|o| o.get(key)) {
                Some(Value::String(s)) if !s.is_empty() => s.clone(),
                _ => fallback.to_string(),
            }
        };
        Self {
            enabled: obs.and_then(|o| o.get("enabled")) != Some(&Value::Bool(false)),
            dir: str_or("dir", fallback_dir),
            file: str_or("file", DEFAULT_LOG_FILE),
        }
    }
}

// ---------------------------------------------------------------------------
// Observability
// ---------------------------------------------------------------------------

/// Логгер прогона: порт объекта, который возвращает `createObservability`
/// (`lib/observability.mjs:150`).
pub struct Observability {
    /// `runId` — общий ключ всех записей прогона.
    pub run_id: String,
    /// `startedAt` — эпоха в мс на момент создания (база для `duration_ms`).
    pub started_at: u64,
    /// `enabled` — выключенный логгер не пишет и не копит записи.
    pub enabled: bool,
    /// Каталог лога, уже приведённый к абсолютному (`resolve(cwd, dir)`).
    dir: PathBuf,
    /// Имя файла внутри каталога.
    file: String,
    /// Накопленные записи; на диск уходят в `flush`/`drain`.
    queue: Mutex<Vec<Value>>,
    now: Clock,
    log: Option<LogSink>,
}

impl Observability {
    /// Порт `createObservability(cfg, { log })`.
    ///
    /// `now` — часы прогона (эпоха, мс), `run_id` — готовый идентификатор или
    /// `None` для генерации в JS-формате.
    pub fn new(
        cfg: &ObservabilityConfig,
        now: impl Fn() -> u64 + Send + Sync + 'static,
        run_id: Option<String>,
    ) -> Self {
        let started_at = now();
        // JS: resolve(process.cwd(), dir) — абсолютный dir побеждает cwd,
        // ровно как Path::join.
        let dir = std::env::current_dir()
            .unwrap_or_default()
            .join(&cfg.dir);
        Self {
            run_id: run_id.unwrap_or_else(|| new_run_id(started_at)),
            started_at,
            enabled: cfg.enabled,
            dir,
            file: cfg.file.clone(),
            queue: Mutex::new(Vec::new()),
            now: Box::new(now),
            log: None,
        }
    }

    /// Сток сообщений о сбоях записи (`{ log }` во втором аргументе JS).
    pub fn with_log(mut self, log: LogSink) -> Self {
        self.log = Some(log);
        self
    }

    /// Путь к JSONL-файлу (`resolve(dir, file)`).
    pub fn path(&self) -> PathBuf {
        self.dir.join(&self.file)
    }

    /// Сколько записей ещё не на диске (для тестов и диагностики).
    pub fn pending(&self) -> usize {
        lock(&self.queue).len()
    }

    /// Порт `logEvent(type, data)`. Имя JSON-поля — `type`, не `event`.
    ///
    /// `payload` разворачивается в запись как spread `{...data}`: не-объект
    /// (в JS такого на вызовах нет) игнорируется целиком.
    pub fn log_event(&self, event: &str, payload: &Value) {
        if !self.enabled {
            return;
        }
        let record = self.record(event, payload, None);
        lock(&self.queue).push(record);
    }

    /// Порт `flush(final)`: дописывает `run_end` с `duration_ms` и сбрасывает
    /// очередь на диск. Ошибки записи — в лог, наружу не поднимаются.
    pub fn flush(&self, final_data: Option<&Value>) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let empty = json!({});
        // saturating_sub: часы, идущие назад, не должны давать переполнение —
        // в JS там получилось бы отрицательное число, что тоже бессмысленно.
        let duration = json!((self.now)().saturating_sub(self.started_at));
        let record = self.record(
            "run_end",
            final_data.unwrap_or(&empty),
            Some(("duration_ms", duration)),
        );
        lock(&self.queue).push(record);
        self.write_queue();
        Ok(())
    }

    /// Порт `drain()`: дождаться, что всё накопленное лежит на диске
    /// (в JS — дождаться цепочки `appendFile`). `run_end` не добавляет.
    pub fn drain(&self) -> Result<()> {
        self.write_queue();
        Ok(())
    }

    /// Форма записи (`lib/observability.mjs:126-142`): порядок ключей
    /// `ts, runId, type[, duration_ms], ...data`. Совпадающий ключ из `data`
    /// перетирает значение, но остаётся на своей исходной позиции — так же
    /// ведёт себя spread в литерале JS-объекта.
    fn record(&self, event: &str, payload: &Value, extra: Option<(&str, Value)>) -> Value {
        let mut m = serde_json::Map::new();
        m.insert(
            "ts".into(),
            Value::String(iso_from_epoch_ms((self.now)() as i64)),
        );
        m.insert("runId".into(), Value::String(self.run_id.clone()));
        m.insert("type".into(), Value::String(event.to_string()));
        if let Some((key, value)) = extra {
            m.insert(key.to_string(), value);
        }
        if let Some(obj) = payload.as_object() {
            for (k, v) in obj {
                m.insert(k.clone(), v.clone());
            }
        }
        Value::Object(m)
    }

    fn write_queue(&self) {
        let records = {
            let mut q = lock(&self.queue);
            if q.is_empty() {
                return;
            }
            std::mem::take(&mut *q)
        };
        if !self.enabled {
            return;
        }
        if let Err(e) = append_jsonl(&self.path(), &records) {
            // Fail-open: записи потеряны, прогон продолжается.
            self.log_failure(&format!("observability write failed (fail-open): {e}"));
        }
    }

    fn log_failure(&self, message: &str) {
        if let Some(log) = &self.log {
            log(message);
        }
    }
}

fn append_jsonl(path: &Path, records: &[Value]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut buf = String::new();
    for record in records {
        buf.push_str(&serde_json::to_string(record).unwrap_or_else(|_| "{}".into()));
        buf.push('\n');
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(buf.as_bytes())
}

/// Блокировка, игнорирующая poisoning (как в [`crate::concurrency`]): паника
/// одной стадии не должна лишать остальные возможности дописать лог.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// runId
// ---------------------------------------------------------------------------

/// `newRunId()` (`lib/observability.mjs:89-91`): base36 от эпохи + 8 hex.
pub fn new_run_id(now_ms: u64) -> String {
    format!("{}-{}", to_base36(now_ms), random_hex4())
}

/// `Number#toString(36)` для неотрицательных целых.
fn to_base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("base36 — только ASCII")
}

/// `randomBytes(4).toString('hex')`. Крейта `rand` в зависимостях нет
/// (Cargo.toml заморожен), поэтому источник — `/dev/urandom`; если он
/// недоступен, деградируем на sha256 от времени и pid: runId нужен уникальным
/// в пределах машины, а не криптостойким.
fn random_hex4() -> String {
    let mut buf = [0u8; 4];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let digest = crate::util::sha256_str(&format!("{nanos}:{}", std::process::id()));
        return digest[..8].to_string();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// ts
// ---------------------------------------------------------------------------

/// `new Date(ms).toISOString()` без зависимости на chrono.
///
/// Формат — всегда UTC с миллисекундами. Годы вне `0000..=9999` печатаются
/// расширенной записью `±YYYYYY`, как в V8.
pub fn iso_from_epoch_ms(ms: i64) -> String {
    const MS_PER_DAY: i64 = 86_400_000;
    let days = ms.div_euclid(MS_PER_DAY);
    let ms_of_day = ms.rem_euclid(MS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let hour = ms_of_day / 3_600_000;
    let minute = (ms_of_day / 60_000) % 60;
    let second = (ms_of_day / 1000) % 60;
    let milli = ms_of_day % 1000;
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year > 9999 {
        format!("+{year:06}")
    } else {
        format!("-{:06}", -year)
    };
    format!("{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milli:03}Z")
}

/// Дни от эпохи → (год, месяц, день) по григорианскому календарю
/// (алгоритм Ховарда Хиннанта `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// Замороженные часы: `t0` и шаг на каждый вызов (0 — часы стоят).
    fn clock(t0: u64, step: u64) -> (impl Fn() -> u64 + Send + Sync + 'static, Arc<AtomicU64>) {
        let cell = Arc::new(AtomicU64::new(t0));
        let handle = Arc::clone(&cell);
        (
            move || cell.fetch_add(step, Ordering::SeqCst),
            handle,
        )
    }

    fn cfg_at(dir: &Path) -> ObservabilityConfig {
        ObservabilityConfig {
            enabled: true,
            dir: dir.to_string_lossy().into_owned(),
            file: DEFAULT_LOG_FILE.to_string(),
        }
    }

    fn read_lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .trim_end()
            .split('\n')
            .map(str::to_string)
            .collect()
    }

    // -- форма записи -------------------------------------------------------

    #[test]
    fn record_shape_matches_js_key_order() {
        let tmp = tempfile::tempdir().unwrap();
        let (now, _) = clock(1_700_000_000_000, 0);
        let obs = Observability::new(&cfg_at(tmp.path()), now, Some("run-1".into()));
        obs.log_event("router", &json!({ "route": "full", "reason": "code" }));
        obs.drain().unwrap();

        let lines = read_lines(&obs.path());
        assert_eq!(lines.len(), 1);
        // ts, runId, type, затем поля payload — порядок как в JS-литерале.
        assert_eq!(
            lines[0],
            r#"{"ts":"2023-11-14T22:13:20.000Z","runId":"run-1","type":"router","route":"full","reason":"code"}"#
        );
    }

    #[test]
    fn flush_appends_run_end_with_duration_and_final_fields() {
        let tmp = tempfile::tempdir().unwrap();
        // Часы: 1000, затем +250 на каждый вызов.
        let (now, _) = clock(1_000, 250);
        let obs = Observability::new(&cfg_at(tmp.path()), now, Some("r".into()));
        // started_at = 1000 (первый вызов), дальше 1250, 1500…
        assert_eq!(obs.started_at, 1_000);
        obs.flush(Some(&json!({ "exitCode": 1, "wouldBlock": true })))
            .unwrap();

        let lines = read_lines(&obs.path());
        assert_eq!(lines.len(), 1);
        let rec: Value = serde_json::from_str(&lines[0]).unwrap();
        let keys: Vec<&str> = rec.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["ts", "runId", "type", "duration_ms", "exitCode", "wouldBlock"]);
        assert_eq!(rec["type"], json!("run_end"));
        // duration_ms считается до ts: вызовы часов идут 1250 (duration) → 1500 (ts).
        assert_eq!(rec["duration_ms"], json!(250));
        assert_eq!(rec["ts"], json!("1970-01-01T00:00:01.500Z"));
    }

    #[test]
    fn payload_overrides_value_but_not_key_position() {
        let tmp = tempfile::tempdir().unwrap();
        let (now, _) = clock(0, 0);
        let obs = Observability::new(&cfg_at(tmp.path()), now, Some("r".into()));
        // Spread в JS перетирает значение, оставляя ключ на первой позиции.
        obs.log_event("gate", &json!({ "type": "подменён", "wouldBlock": false }));
        obs.drain().unwrap();
        let rec: Value = serde_json::from_str(&read_lines(&obs.path())[0]).unwrap();
        let keys: Vec<&str> = rec.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["ts", "runId", "type", "wouldBlock"]);
        assert_eq!(rec["type"], json!("подменён"));
    }

    // -- порядок событий ----------------------------------------------------

    #[test]
    fn events_keep_order_and_run_end_is_last() {
        let tmp = tempfile::tempdir().unwrap();
        let (now, _) = clock(0, 1);
        let obs = Observability::new(&cfg_at(tmp.path()), now, Some("r".into()));
        obs.log_event("run_start", &json!({ "pr": 7 }));
        obs.log_event("router", &json!({ "route": "full" }));
        assert_eq!(obs.pending(), 2, "до flush записи лежат в очереди");
        obs.log_event("gate", &json!({}));
        obs.flush(None).unwrap();
        assert_eq!(obs.pending(), 0, "flush опустошает очередь");

        let types: Vec<String> = read_lines(&obs.path())
            .iter()
            .map(|l| serde_json::from_str::<Value>(l).unwrap()["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(types, ["run_start", "router", "gate", "run_end"]);
    }

    #[test]
    fn second_flush_appends_not_truncates() {
        let tmp = tempfile::tempdir().unwrap();
        let (now, _) = clock(0, 0);
        let obs = Observability::new(&cfg_at(tmp.path()), now, Some("r".into()));
        obs.log_event("a", &json!({}));
        obs.drain().unwrap();
        obs.log_event("b", &json!({}));
        obs.drain().unwrap();
        assert_eq!(read_lines(&obs.path()).len(), 2, "файл открывается на append");
    }

    // -- выключенный режим --------------------------------------------------

    #[test]
    fn disabled_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = cfg_at(tmp.path());
        cfg.enabled = false;
        let (now, _) = clock(0, 0);
        let obs = Observability::new(&cfg, now, Some("r".into()));
        obs.log_event("run_start", &json!({ "pr": 1 }));
        obs.flush(Some(&json!({ "exitCode": 0 }))).unwrap();
        obs.drain().unwrap();
        assert_eq!(obs.pending(), 0);
        assert!(!obs.path().exists(), "выключенный логгер не создаёт файл");
    }

    // -- каталог и fail-open ------------------------------------------------

    #[test]
    fn flush_creates_missing_directory_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a/b/c");
        let obs = Observability::new(&cfg_at(&nested), clock(0, 0).0, Some("r".into()));
        assert!(!nested.exists());
        obs.flush(None).unwrap();
        assert!(obs.path().exists(), "mkdir -p перед append");
    }

    #[test]
    fn write_error_is_logged_not_panicked() {
        let tmp = tempfile::tempdir().unwrap();
        // Каталог лога занят обычным файлом → create_dir_all обязан упасть.
        let busy = tmp.path().join("busy");
        std::fs::write(&busy, "не каталог").unwrap();

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&seen);
        let obs = Observability::new(&cfg_at(&busy), clock(0, 0).0, Some("r".into()))
            .with_log(Box::new(move |m| lock(&sink).push(m.to_string())));

        obs.log_event("gate", &json!({}));
        // Ни log_event, ни flush не паникуют и не возвращают Err.
        assert!(obs.flush(None).is_ok());

        let messages = lock(&seen).clone();
        assert_eq!(messages.len(), 1, "одна попытка записи — одно сообщение");
        assert!(
            messages[0].starts_with("observability write failed (fail-open): "),
            "текст как в JS: {}",
            messages[0]
        );
        assert_eq!(obs.pending(), 0, "потерянные записи не копятся бесконечно");
    }

    #[test]
    fn missing_log_sink_is_silent_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let busy = tmp.path().join("busy");
        std::fs::write(&busy, "не каталог").unwrap();
        let obs = Observability::new(&cfg_at(&busy), clock(0, 0).0, Some("r".into()));
        obs.log_event("gate", &json!({}));
        assert!(obs.flush(None).is_ok());
    }

    // -- runId --------------------------------------------------------------

    #[test]
    fn default_run_id_is_base36_dash_8_hex() {
        let tmp = tempfile::tempdir().unwrap();
        let obs = Observability::new(&cfg_at(tmp.path()), clock(1_700_000_000_000, 0).0, None);
        let (stamp, rnd) = obs.run_id.split_once('-').expect("runId вида <base36>-<hex>");
        assert_eq!(stamp, to_base36(1_700_000_000_000), "base36 от started_at");
        assert_eq!(rnd.len(), 8, "randomBytes(4).toString('hex')");
        assert!(rnd.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Два подряд — разные (иначе сходимость прогонов сломается).
        let other = Observability::new(&cfg_at(tmp.path()), clock(1_700_000_000_000, 0).0, None);
        assert_ne!(obs.run_id, other.run_id);
    }

    #[test]
    fn injected_run_id_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let obs = Observability::new(&cfg_at(tmp.path()), clock(0, 0).0, Some("golden".into()));
        assert_eq!(obs.run_id, "golden");
    }

    #[test]
    fn base36_matches_js_number_to_string() {
        assert_eq!(to_base36(0), "0");
        assert_eq!(to_base36(35), "z");
        assert_eq!(to_base36(36), "10");
        // (1700000000000).toString(36) в node → 'loyw3v28'
        assert_eq!(to_base36(1_700_000_000_000), "loyw3v28");
    }

    // -- ts -----------------------------------------------------------------

    #[test]
    fn iso_matches_js_to_iso_string() {
        assert_eq!(iso_from_epoch_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_from_epoch_ms(1), "1970-01-01T00:00:00.001Z");
        assert_eq!(iso_from_epoch_ms(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        // 29 февраля високосного 2024-го — проверка civil_from_days.
        assert_eq!(iso_from_epoch_ms(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
        // Отрицательная эпоха: JS даёт 1969-12-31T23:59:59.999Z.
        assert_eq!(iso_from_epoch_ms(-1), "1969-12-31T23:59:59.999Z");
        // Расширенный год (V8: '+010000-01-01T00:00:00.000Z').
        assert_eq!(iso_from_epoch_ms(253_402_300_800_000), "+010000-01-01T00:00:00.000Z");
    }

    // -- конфиг -------------------------------------------------------------

    #[test]
    fn config_from_json_matches_js_truthiness() {
        // Полный конфиг.
        let cfg = ObservabilityConfig::from_config(&json!({
            "observability": { "enabled": true, "dir": "logs", "file": "runs.jsonl" }
        }));
        assert_eq!(cfg, ObservabilityConfig { enabled: true, dir: "logs".into(), file: "runs.jsonl".into() });

        // enabled выключает ТОЛЬКО явный false (obsCfg.enabled !== false).
        for value in [json!(null), json!(0), json!("no"), json!({})] {
            let cfg = ObservabilityConfig::from_config(&json!({ "observability": { "enabled": value } }));
            assert!(cfg.enabled, "не-false оставляет лог включённым: {value}");
        }
        assert!(!ObservabilityConfig::from_config(&json!({ "observability": { "enabled": false } })).enabled);

        // Пустые строки — falsy, значит дефолты.
        let cfg = ObservabilityConfig::from_config(&json!({ "observability": { "dir": "", "file": "" } }));
        assert_eq!(cfg, ObservabilityConfig::default());
        // Секции нет вовсе — тоже дефолты и enabled.
        assert_eq!(ObservabilityConfig::from_config(&json!({})), ObservabilityConfig::default());
    }

    #[test]
    fn config_falls_back_to_profile_dir() {
        let profile = Profile::default_builtin(Path::new("/nonexistent-pkg-root"));
        let cfg = ObservabilityConfig::from_config_with_profile(&json!({ "observability": {} }), &profile);
        assert_eq!(cfg.dir, profile.paths().observability_dir());
    }

    // -- реэкспорт ----------------------------------------------------------

    #[test]
    fn reexports_point_at_metrics_port() {
        assert_eq!(PR_TITLE_LOG_MAX, 80);
        assert_eq!(MAX_SLIM, 40);
        assert_eq!(
            count_by_severity(Some(&json!([{ "severity": "major" }]))),
            json!({ "critical": 0, "major": 1, "minor": 0, "nit": 0 })
        );
        let slim = slim_findings(Some(&json!([{ "file": "a.ts", "title": "t", "body": "секрет" }])), MAX_SLIM);
        assert!(!slim[0].as_object().unwrap().contains_key("body"));
    }

    #[test]
    fn absolute_dir_ignores_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let obs = Observability::new(&cfg_at(tmp.path()), clock(0, 0).0, Some("r".into()));
        assert_eq!(obs.path(), tmp.path().join(DEFAULT_LOG_FILE));
    }
}
