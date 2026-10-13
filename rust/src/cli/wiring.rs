//! Сборка боевых зависимостей прогона ревью (RFC 20, задача T27).
//!
//! Порт той части `createReviewPipeline` (`pipeline.mjs:314-328`), которая выбирает
//! дефолты `deps`: LLM-клиент с общим трекером стоимости, GitHub, Graphify, prescan,
//! телеграм, движок с маршрутизацией чанков и observability прогона. Живёт отдельно от
//! подкоманды `review`, потому что те же зависимости собирает `review-mcp` (T41): второй
//! сборщик неизбежно разошёлся бы с первым — например, трекером стоимости, который не
//! связан с LLM-клиентом и тихо отдаёт нулевой снимок в `run_end`.
//!
//! Устройство: [`ReviewWiring`] владеет всем, что переживает прогон (конфиг, профиль,
//! HTTP-клиенты, трекер), а заимствующие части (`RealEngine`, `RealPrescan`,
//! `Observability`, `PipelineDeps`) строятся на стеке внутри
//! [`ReviewWiring::run_detailed_with`] — у них ссылки на владельца, и самоссылающаяся
//! структура здесь не нужна. Один `ReviewWiring` — один прогон: трекер стоимости и
//! семафоры LLM-клиента копят состояние, как и в JS, где `createReviewPipeline`
//! зовётся на каждый прогон.
//!
//! Секреты (`ollama.key`, `github.token`, `telegram.token`) лежат в конфиге, поэтому
//! `Debug` у [`ReviewWiring`] и [`TelegramNotifier`] написан руками и их не печатает.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::cost::CostTracker;
use crate::engine::RealEngine;
use crate::github::{GithubConfig, RealGithubClient};
use crate::graphify_client::{GraphifyConfig, RealGraphify};
use crate::llm::{LlmConfig, RealLlmClient};
use crate::observability::{Observability, ObservabilityConfig};
use crate::pipeline::{
    create_review_pipeline, load_review_config, ChunkRoutingFns, CliArgs, PipelineOptions, PipelineOutcome,
};
use crate::prescan::RealPrescan;
use crate::profile::Profile;
use crate::traits::{GithubApi, Notifier, PipelineDeps};

/// Логгер прогона: общий для пайплайна, движка, клиентов и observability.
/// `Arc`, потому что часть потребителей (`with_log` клиентов) хочет `'static`-замыкание.
pub type RunLog = Arc<dyn Fn(&str) + Send + Sync>;

/// `createLogger()` (`lib/util.mjs:187`): строка в stdout с префиксом `[pr-review]`.
pub fn stdout_log() -> RunLog {
    Arc::new(|msg: &str| println!("[pr-review] {msg}"))
}

/// Окружение процесса как карта. Переменные с не-UTF-8 именем или значением
/// пропускаются: `std::env::vars()` на них паникует, а конфиг их всё равно не читает.
pub fn process_env() -> HashMap<String, String> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}

fn system_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Что прогон может перебить поверх конфига — остаток JS-`deps`, который нужен
/// долгоживущим потребителям (MCP передаёт `headSha` file-diff прогона).
#[derive(Debug, Clone, Default)]
pub struct RunExtras {
    /// `deps.headSha` — перебивает `cli.headSha`.
    pub head_sha: Option<String>,
}

/// Боевые зависимости одного прогона ревью.
pub struct ReviewWiring {
    cfg: Value,
    profile: Profile,
    cwd: PathBuf,
    log: RunLog,
    cost: Arc<CostTracker>,
    llm: RealLlmClient,
    github: RealGithubClient,
    graphify: RealGraphify,
    notifier: Option<TelegramNotifier>,
}

impl std::fmt::Debug for ReviewWiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Конфиг целиком не печатаем: в нём ключ LLM и токены GitHub/Telegram.
        f.debug_struct("ReviewWiring")
            .field("profile", &self.profile.name)
            .field("cwd", &self.cwd)
            .field("github", &self.github)
            .field("notifier", &self.notifier)
            .finish_non_exhaustive()
    }
}

impl ReviewWiring {
    /// `loadReviewConfig(env, cli)` + дефолты `deps`: профиль ищется от `cwd`
    /// (`REVIEW_PROFILE` → автопоиск → встроенный, fail-open), затем [`Self::from_config`].
    pub fn from_env(env: &HashMap<String, String>, cli: &CliArgs, cwd: PathBuf, log: RunLog) -> Result<Self> {
        let profile = {
            let log = log.clone();
            Profile::resolve(env, &cwd, move |m| log(m))?
        };
        let cfg = load_review_config(env, cli, &profile);
        Self::from_config(cfg, profile, cwd, log)
    }

    /// Зависимости из уже собранного конфига. Ошибка — только сбой сборки HTTP-клиента;
    /// вызывающий-CLI превращает её в exit 0 (fail-open), как любую аварию движка.
    pub fn from_config(cfg: Value, profile: Profile, cwd: PathBuf, log: RunLog) -> Result<Self> {
        // ОДИН трекер на прогон: в него пишет LLM-клиент, из него пайплайн берёт
        // efficiencyMetrics и строку стоимости sticky (`pipeline.mjs:318-319`).
        let price_map = cfg.pointer("/cost/priceMap").filter(|v| !v.is_null()).cloned();
        let cost = Arc::new(CostTracker::new(price_map));

        let sink_cost = Arc::clone(&cost);
        let llm_log = log.clone();
        let llm = RealLlmClient::new(LlmConfig::from_cfg(&cfg))
            .map_err(|e| anyhow!("LLM-клиент не собрался: {}", e.error))?
            .with_usage_sink(Box::new(move |model, call_type, usage| {
                sink_cost.record(Some(usage), model, call_type)
            }))
            .with_log(Box::new(move |m| llm_log(m)));

        let gh_log = log.clone();
        let github = RealGithubClient::new(&GithubConfig::from_cfg(&cfg), profile.markers().clone())?
            .with_log(Box::new(move |m| gh_log(m)));

        let graphify = RealGraphify::new(GraphifyConfig::from_cfg(&cfg));
        let notifier = TelegramNotifier::from_cfg(&cfg)?;

        Ok(ReviewWiring { cfg, profile, cwd, log, cost, llm, github, graphify, notifier })
    }

    /// Замороженный конфиг прогона (с секретами — не логировать целиком).
    pub fn cfg(&self) -> &Value {
        &self.cfg
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Трекер стоимости, общий с LLM-клиентом.
    pub fn cost(&self) -> &CostTracker {
        &self.cost
    }

    pub fn log(&self) -> &RunLog {
        &self.log
    }

    /// `runDetailed()` на боевых зависимостях.
    pub fn run_detailed(&self) -> Result<PipelineOutcome> {
        self.run_detailed_with(RunExtras::default())
    }

    /// `runDetailed()` с перебитыми полями прогона. `Err` — авария пайплайна
    /// (не мягкий отказ: тот приходит `Ok` с `soft_fail = true`).
    pub fn run_detailed_with(&self, extras: RunExtras) -> Result<PipelineOutcome> {
        self.run_inner(extras, &self.github, self.notifier.as_ref().map(|n| n as &dyn Notifier))
    }

    /// `runDetailed()` превью-прогона ревью-MCP (`lib/review-mcp.mjs` `executeReviewRun`):
    /// GitHub подменён `github` (в MCP — немой клиент с prior из экспорта), телеграм
    /// выключен (`notifyTelegram: async () => {}`). Остальное — те же боевые зависимости.
    pub fn run_detailed_silent(&self, extras: RunExtras, github: &dyn GithubApi) -> Result<PipelineOutcome> {
        self.run_inner(extras, github, None)
    }

    fn run_inner(
        &self,
        extras: RunExtras,
        github: &dyn GithubApi,
        notifier: Option<&dyn Notifier>,
    ) -> Result<PipelineOutcome> {
        let log: &(dyn Fn(&str) + Send + Sync) = &*self.log;

        // Observability — одна на прогон: её же слушает движок, иначе llm_call/engine-события
        // ушли бы в другой run_id. Каталог — от корня прогона (в JS — от process.cwd()).
        let mut oc = ObservabilityConfig::from_config_with_profile(&self.cfg, &self.profile);
        oc.dir = self.cwd.join(&oc.dir).to_string_lossy().into_owned();
        let obs_log = self.log.clone();
        let obs = Observability::new(&oc, system_now, None).with_log(Box::new(move |m| obs_log(m)));
        let on_event = |event: &str, payload: &Value| obs.log_event(event, payload);

        let routing_fns = ChunkRoutingFns::from_profile(&self.profile);
        let mut engine = RealEngine::from_config(&self.cfg, &self.llm, &self.profile, log)
            .with_chunk_routing(routing_fns.routing());
        if obs.enabled {
            engine = engine.with_events(&on_event);
        }

        let prescan_log = self.log.clone();
        let prescan = RealPrescan::from_cfg(&self.cfg, &self.profile, self.cwd.clone(), move |m: &str| prescan_log(m));

        let deps = PipelineDeps {
            github,
            engine: &engine,
            prescan: &prescan,
            graphify: &self.graphify,
            notifier,
            log,
            cost: &self.cost,
        };
        let opts = PipelineOptions {
            head_sha: extras.head_sha,
            observability: Some(&obs),
            cwd: Some(self.cwd.clone()),
            ..PipelineOptions::default()
        };
        create_review_pipeline(&self.cfg, &self.profile, deps, opts).run_detailed()
    }
}

/// Боевой [`Notifier`] — порт `notifyTelegram` (`pipeline.mjs:280-293`).
///
/// Токен бота стоит в URL запроса, поэтому ошибка транспорта уходит в лог без URL
/// (`reqwest::Error::without_url`), а `Debug` его не печатает.
pub struct TelegramNotifier {
    token: String,
    chat: String,
    base: String,
    http: reqwest::blocking::Client,
}

impl std::fmt::Debug for TelegramNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramNotifier").field("chat", &self.chat).field("base", &self.base).finish_non_exhaustive()
    }
}

/// Тело ответа Telegram в логе — с потолком, как `truncate` ответов LLM.
const TELEGRAM_BODY_MAX: usize = 400;

impl TelegramNotifier {
    /// `None`, когда токен или чат пусты: в JS это ранний `return` внутри `notifyTelegram`.
    pub fn from_cfg(cfg: &Value) -> Result<Option<Self>> {
        let s = |p: &str| cfg.pointer(p).and_then(Value::as_str).unwrap_or("").to_string();
        let (token, chat) = (s("/telegram/token"), s("/telegram/chat"));
        if token.is_empty() || chat.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self::new(token, chat, "https://api.telegram.org")?))
    }

    /// Нотификатор с явным адресом API (тесты — фейковый сервер на 127.0.0.1).
    pub fn new(token: String, chat: String, base: &str) -> Result<Self> {
        // У JS-fetch таймаута нет; здесь потолок, чтобы зависший API не держал прогон.
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow!("HTTP-клиент телеграма не собрался: {}", e.without_url()))?;
        Ok(TelegramNotifier { token, chat, base: base.trim_end_matches('/').to_string(), http })
    }
}

impl Notifier for TelegramNotifier {
    fn notify(&self, text: &str) -> Result<()> {
        let url = format!("{}/bot{}/sendMessage", self.base, self.token);
        let body = json!({ "chat_id": self.chat, "text": text, "disable_web_page_preview": true });
        let res = self
            .http
            .post(url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .map_err(|e| anyhow!("{}", e.without_url()))?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let text = res.text().unwrap_or_default();
            let text: String = text.chars().take(TELEGRAM_BODY_MAX).collect();
            // Ответ API не содержит токен, но на всякий случай вычищаем его.
            bail!("{status} {}", text.replace(&self.token, "***"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_без_токена_или_чата_не_собирается() {
        assert!(TelegramNotifier::from_cfg(&json!({})).unwrap().is_none());
        assert!(TelegramNotifier::from_cfg(&json!({"telegram": {"token": "t", "chat": ""}})).unwrap().is_none());
        assert!(TelegramNotifier::from_cfg(&json!({"telegram": {"token": "", "chat": "c"}})).unwrap().is_none());
        assert!(TelegramNotifier::from_cfg(&json!({"telegram": {"token": "t", "chat": "c"}})).unwrap().is_some());
    }

    #[test]
    fn telegram_ошибка_транспорта_не_несёт_токен() {
        // Порт 1 на 127.0.0.1 закрыт: соединение отвергается без сети наружу.
        let n = TelegramNotifier::new("sekret-bot-token".into(), "c".into(), "http://127.0.0.1:1").unwrap();
        let e = n.notify("x").unwrap_err();
        let msg = format!("{e:#} {e:?}");
        assert!(!msg.contains("sekret-bot-token"), "токен утёк в ошибку: {msg}");
        assert!(!format!("{n:?}").contains("sekret-bot-token"));
    }

    #[test]
    fn debug_wiring_не_печатает_секреты() {
        let env: HashMap<String, String> = [
            ("OLLAMA_API_KEY", "sekret-llm-key"),
            ("GITHUB_TOKEN", "sekret-gh-token"),
            ("TELEGRAM_BOT_TOKEN", "sekret-tg"),
            ("TELEGRAM_CHAT_ID", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let dir = tempfile::tempdir().unwrap();
        let w = ReviewWiring::from_env(&env, &CliArgs::default(), dir.path().to_path_buf(), Arc::new(|_: &str| {}))
            .unwrap();
        let dbg = format!("{w:?}");
        for s in ["sekret-llm-key", "sekret-gh-token", "sekret-tg"] {
            assert!(!dbg.contains(s), "{s} в Debug: {dbg}");
        }
        assert_eq!(w.cfg()["ollama"]["key"], "sekret-llm-key", "ключ доехал до конфига");
    }
}
