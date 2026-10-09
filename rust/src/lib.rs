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
pub mod engine;
pub mod export;
pub mod fixtures;
pub mod findings;
pub mod github;
pub mod json_repair;
pub mod llm;
pub mod metrics;
pub mod prompt;
pub mod report;
pub mod router;
pub mod specialists;
pub mod structure_assessments;
pub mod structure_evidence;
pub mod structure_graphify;
pub mod structure_lcov;
pub mod structure_mcp;
pub mod structure_mutation;
pub mod structure_protocol;
pub mod structure_scip;
pub mod structure_tests;
pub mod triage;
pub mod verifier;

// RFC 16: модули единого бинаря.
// Список пре-сеян в задаче T03 и заморожен: каждая последующая задача наполняет
// свои файлы, но не правит этот блок (см. proposals/04-standalone-rust-binary.md).
pub mod astgrep;
pub mod blast_findings;
pub mod cli;
pub mod concurrency;
pub mod constants;
pub mod cost;
pub mod eval;
pub mod feedback;
pub mod fp_patterns;
pub mod graphify_client;
pub mod observability;
pub mod pipeline;
pub mod prescan;
pub mod profile;
pub mod structure;
pub mod structure_context;
pub mod structure_drift;
pub mod structure_export;
pub mod structure_identity;
pub mod swarm;
pub mod testutil;
pub mod traits;
pub mod util;
