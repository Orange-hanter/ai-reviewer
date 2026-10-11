//! Подкоманда `pr-review review-mcp` — порт `bin/pr-review-mcp.mjs` (RFC 16, T41):
//! stdio-сервер ревью-MCP поверх транспорта `mcp_stdio`.
//!
//! Слои как в JS:
//! 1. корень: `REVIEW_MCP_ROOT` (пустое значение и неподставленный `${…}` пропускаются);
//!    несуществующий — `ai-reviewer MCP: REVIEW_MCP_ROOT не существует: <abs>` и exit 1,
//!    иначе chdir. Без переменной корень — рабочий каталог;
//! 2. handshake без домена (`review_mcp::handshake`, RFC 10);
//! 3. домен — [`ReviewMcp`]: `review_run` идёт через ту же сборку боевых зависимостей,
//!    что `pr-review review` ([`ReviewWiring`]), но с немым GitHub и без телеграма.
//!
//! Не гейт: код выхода сервера — 0 (или 1 на несуществующем корне, как в JS), блокеры
//! прогона на него не влияют, в GitHub ничего не пишется.
//!
//! stdout — канал протокола: логи прогона уходят notifications `notifications/message`,
//! а не печатью. На EOF stdin сервер дожидается фонового прогона (`wait: false`), чтобы
//! его sidecar дописался (Node тоже не выходит, пока жив промис прогона); на
//! SIGINT/SIGTERM — выходит сразу.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use super::structure_mcp::usable_root;
use super::wiring::{process_env, ReviewWiring, RunExtras};
use crate::graphify_client::{GraphifyConfig, RealGraphify};
use crate::mcp_stdio::{run_mcp_stdio_shared, stop_requested, Notify, SharedOut};
use crate::prescan::RealPrescan;
use crate::profile::Profile;
use crate::review_mcp::{
    handshake, CheapStages, McpDeps, McpLog, ReviewMcp, ReviewRunner, RunReport, RunRequest, SilentGithub, SystemGit,
};
use crate::traits::{BlastRadius, GraphifyApi, PrescanApi, PrescanResult};
use crate::util::js_resolve;

/// Аргументов нет: конфигурация — только через env, как у JS-входа.
#[derive(Debug, clap::Args)]
pub struct Args {}

/// Боевой прогон: новый [`ReviewWiring`] на каждый job (трекер стоимости и семафоры
/// LLM — состояние одного прогона), GitHub — немой с prior из экспорта.
#[derive(Debug, Default)]
pub struct WiringRunner;

impl ReviewRunner for WiringRunner {
    fn run(&self, req: RunRequest) -> Result<RunReport> {
        let log = req.log.clone();
        let wiring = ReviewWiring::from_env(&req.env, &req.cli, req.root, log)?;
        let github = SilentGithub::new(req.prior);
        let outcome = wiring.run_detailed_silent(RunExtras { head_sha: Some(req.head_sha) }, &github)?;
        Ok(outcome.into())
    }
}

/// Боевые дешёвые стадии: prescan (eslint + ast-grep) и Graphify.
#[derive(Debug, Default)]
pub struct RealStages;

impl CheapStages for RealStages {
    fn prescan(&self, cfg: &Value, profile: &Profile, root: &Path, log: McpLog, paths: &[String]) -> Result<PrescanResult> {
        RealPrescan::from_cfg(cfg, profile, root.to_path_buf(), move |m: &str| log(m)).run(paths)
    }

    fn blast(&self, cfg: &Value, symbols: &[String]) -> Result<BlastRadius> {
        RealGraphify::new(GraphifyConfig::from_cfg(cfg)).build_blast_radius(symbols)
    }
}

pub fn run(_args: Args) -> Result<i32> {
    if let Some(root) = usable_root(std::env::var("REVIEW_MCP_ROOT").ok()) {
        let abs = js_resolve(&std::env::current_dir()?, &root);
        if !abs.exists() {
            eprintln!("ai-reviewer MCP: REVIEW_MCP_ROOT не существует: {}", abs.display());
            return Ok(1);
        }
        // JS: `process.chdir` бросает (ENOTDIR, EACCES) мимо fail-open → exit 1. Через `?`
        // ошибка ушла бы в диспетчер и превратилась в exit 0: клиент не увидел бы сбой конфигурации.
        if let Err(e) = std::env::set_current_dir(&abs) {
            eprintln!("ai-reviewer MCP: не удалось перейти в REVIEW_MCP_ROOT {}: {e}", abs.display());
            return Ok(1);
        }
    }
    let root = std::env::current_dir()?;
    let env = process_env();
    let out = SharedOut::stdout();
    let notify_out = out.clone();
    let deps = McpDeps {
        git: Arc::new(SystemGit),
        runner: Arc::new(WiringRunner),
        stages: Arc::new(RealStages),
        notify: Arc::new(move |method: &str, params: Value| notify_out.notify(method, params)),
    };
    let mcp = ReviewMcp::new(root, env.clone(), deps);
    let domain = mcp.clone();
    run_mcp_stdio_shared(out, move |request: &Value, _notify: &Notify<'_>| {
        // Handshake не трогает домен (RFC 10): tools/list отвечает и без пайплайна.
        if let Some(response) = handshake(request, &env) {
            return Ok(response);
        }
        Ok(domain.handle(request))
    })?;
    if !stop_requested() {
        mcp.wait_background();
    }
    Ok(0)
}
