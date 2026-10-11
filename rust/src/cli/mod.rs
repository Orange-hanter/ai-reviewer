//! Подкоманды единого бинаря `pr-review` (RFC 16, решение 1).
//!
//! Каждый модуль объявляет `pub struct Args` (`#[derive(clap::Args)]`) и
//! `pub fn run(args: Args) -> anyhow::Result<i32>`; `src/main.rs` только
//! диспетчеризует и переводит `Result` в код возврата. Список модулей
//! заморожен на T03: последующие задачи наполняют файлы, но не правят его.

pub mod eval;
pub mod feedback;
pub mod feedback_batch;
pub mod feedback_migrate;
pub mod feedback_promote;
pub mod metrics;
pub mod review;
pub mod structure;
pub mod structure_mcp;
pub mod wiring;
