//! Rust-модули ai-reviewer.
//!
//! Архитектура миграции: каждый Rust-модуль — автономный бинарь,
//! принимающий JSON на stdin и отдающий JSON на stdout; JS вызывает
//! его через execFile (см. rust/src/bin/).
//!
//! Фаза 0: только загрузка fixture-кейсов (парсер реальных meta.json /
//! findings.json из fixtures/). Логика сравнения появится в Фазе 1
//! после порта diff/findings из lib/*.mjs.

pub mod actionability;
pub mod config;
pub mod diff;
pub mod fixtures;
pub mod findings;
pub mod json_repair;
pub mod router;
pub mod structure_scip;
