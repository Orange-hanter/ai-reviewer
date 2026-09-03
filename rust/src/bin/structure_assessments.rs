//! Бинарный JSON-контракт порта lib/structure-assessments.mjs (Фаза 2, шаг 3).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате JS-возврата (null вместо undefined).
//! Ошибки:        exit code 2 + {"error": "..."} на stderr.
//!
//! Операции:
//!   assessDependencyRisk      {facts?, coverageObservations?, options?}
//!                             → {highFanIn, crossBoundaryEdges, godNodes}
//!   assessDataQuality         {facts?, coverageObservations?, options?} → dataQuality
//!   assessContractScenarios   {facts?, obligations?, coverageObservations?,
//!                              propertyResults?, alignment?, dataQuality?,
//!                              evidenceCeiling?} → [scenario]
//!   buildStructureHints       {dependencyRisk?, dataQuality?, scenarios?,
//!                              mutationObservations?} → [hint]
//!   buildStructureAssessment  {factsPayload, obligationsExport?, lcov?,
//!                              coverageObservations?, propertyResults?, mutation?,
//!                              receipts?, testRun?, thresholds?, generatedAt}
//!                             → structure_assessment-пейлоад
//!   temporalAlignment         {factsHeadSha?, testHeadSha?, obligationsHeadSha?}
//!                             → 'aligned' | 'mismatch' | 'unknown'
//!   assessmentFingerprint     {facts, obligationsRevision?, coverageHash,
//!                              thresholdsHash} → hex

use std::io::Read;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use ai_reviewer::structure_assessments::{
    assess_contract_scenarios, assess_data_quality, assess_dependency_risk,
    assessment_fingerprint, build_structure_assessment, build_structure_hints,
    temporal_alignment,
};

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", json!({ "error": format!("{e:#}") }));
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).context("чтение stdin")?;
    let v: Value = serde_json::from_str(&input).context("вход не является JSON")?;
    let obj = v.as_object().context("вход должен быть JSON-объектом")?;
    let op = obj.get("op").and_then(Value::as_str).unwrap_or("");

    let out = match op {
        "assessDependencyRisk" => assess_dependency_risk(
            obj.get("facts"),
            obj.get("coverageObservations"),
            obj.get("options"),
        )?,
        "assessDataQuality" => assess_data_quality(
            obj.get("facts"),
            obj.get("coverageObservations"),
            obj.get("options"),
        )?,
        "assessContractScenarios" => assess_contract_scenarios(
            obj.get("facts"),
            obj.get("obligations"),
            obj.get("coverageObservations"),
            obj.get("propertyResults"),
            obj.get("alignment"),
            obj.get("dataQuality"),
            obj.get("evidenceCeiling"),
        )?,
        "buildStructureHints" => build_structure_hints(
            obj.get("dependencyRisk"),
            obj.get("dataQuality"),
            obj.get("scenarios"),
            obj.get("mutationObservations"),
        )?,
        "temporalAlignment" => json!(temporal_alignment(
            obj.get("factsHeadSha"),
            obj.get("testHeadSha"),
            obj.get("obligationsHeadSha"),
        )),
        "assessmentFingerprint" => json!(assessment_fingerprint(
            obj.get("facts"),
            obj.get("obligationsRevision"),
            obj.get("coverageHash"),
            obj.get("thresholdsHash"),
        )),
        "buildStructureAssessment" => build_structure_assessment(
            obj.get("factsPayload"),
            obj.get("obligationsExport"),
            obj.get("lcov"),
            obj.get("coverageObservations"),
            obj.get("propertyResults"),
            obj.get("mutation"),
            obj.get("receipts"),
            obj.get("testRun"),
            obj.get("thresholds"),
            obj.get("generatedAt"),
        )?,
        other => anyhow::bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
