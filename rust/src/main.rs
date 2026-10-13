//! Единая точка входа standalone-бинаря `pr-review` (RFC 20, решение 1).
//!
//! Файл — только диспетчер: разбор аргументов clap и перевод результата
//! подкоманды в код возврата. Логика каждой подкоманды живёт в
//! `ai_reviewer::cli::<name>::run`, аргументы — в `ai_reviewer::cli::<name>::Args`.
//!
//! Fail-open, как в bin/pr-review.mjs:9-14: инфра-сбой движка ревью не блокирует
//! чужой merge, поэтому любая ошибка (и любая паника) превращается в сообщение
//! в stderr и exit 0. Осознанный exit 1 на подтверждённых блокерах возвращает
//! сама подкоманда через `Ok(1)`. Подкоманды со strict-режимами (REVIEW_LLM_REQUIRED,
//! REVIEW_VERIFIER_STRICT) обязаны вернуть ненулевой код сами, а не бросить ошибку.

use std::panic::{self, AssertUnwindSafe};

use clap::{Parser, Subcommand};

use ai_reviewer::cli;

/// AI-ревью для PR: гейт, structure-контур и обслуживание feedback.
#[derive(Debug, Parser)]
#[command(name = "pr-review", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

/// Десять подкоманд бинаря. Имена в CLI — kebab-case от имени варианта
/// (`StructureMcp` → `structure-mcp`), это поведение clap по умолчанию.
#[derive(Debug, Subcommand)]
enum Commands {
    /// Полный прогон ревью PR (или offline по --diff).
    Review(cli::review::Args),
    /// Structure-контур: facts/assessment/obligations вне merge-гейта.
    Structure(cli::structure::Args),
    /// MCP-сервер structure-контура поверх stdio.
    StructureMcp(cli::structure_mcp::Args),
    /// MCP-сервер ревью поверх stdio: дешёвые стадии и pinned dry-run MoA, не гейт.
    ReviewMcp(cli::review_mcp::Args),
    /// Прогон golden-фикстур без LLM — гейт post-processing.
    Eval(cli::eval::Args),
    /// Отчёт по JSONL-логу observability.
    Metrics(cli::metrics::Args),
    /// Сбор feedback (👍/👎) по одному PR.
    Feedback(cli::feedback::Args),
    /// Пакетный сбор feedback по нескольким PR.
    FeedbackBatch(cli::feedback_batch::Args),
    /// Миграция хранилища feedback на актуальную схему.
    FeedbackMigrate(cli::feedback_migrate::Args),
    /// Продвижение кандидатов feedback в FP-паттерны.
    FeedbackPromote(cli::feedback_promote::Args),
}

/// Диспетчер: единственное место, где вариант подкоманды превращается в вызов.
fn dispatch(cmd: Commands) -> anyhow::Result<i32> {
    match cmd {
        Commands::Review(args) => cli::review::run(args),
        Commands::Structure(args) => cli::structure::run(args),
        Commands::StructureMcp(args) => cli::structure_mcp::run(args),
        Commands::ReviewMcp(args) => cli::review_mcp::run(args),
        Commands::Eval(args) => cli::eval::run(args),
        Commands::Metrics(args) => cli::metrics::run(args),
        Commands::Feedback(args) => cli::feedback::run(args),
        Commands::FeedbackBatch(args) => cli::feedback_batch::run(args),
        Commands::FeedbackMigrate(args) => cli::feedback_migrate::run(args),
        Commands::FeedbackPromote(args) => cli::feedback_promote::run(args),
    }
}

fn main() {
    // Ошибку разбора аргументов clap печатает сам и выходит с кодом 2:
    // это ошибка вызывающего, а не сбой инфраструктуры ревью.
    let cli = Cli::parse();

    // catch_unwind поверх диспетчера: паника внутри подкоманды — тоже сбой движка,
    // и она не должна ронять merge (стандартный хук паники уже напечатал детали).
    let code = match panic::catch_unwind(AssertUnwindSafe(|| dispatch(cli.cmd))) {
        Ok(Ok(code)) => code,
        Ok(Err(e)) => {
            eprintln!("[pr-review] {e:#}");
            0
        }
        Err(_) => {
            eprintln!("[pr-review] паника внутри подкоманды, продолжаем без блокировки merge");
            0
        }
    };

    std::process::exit(code);
}
