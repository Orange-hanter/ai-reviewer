//! Бинарный JSON-контракт порта lib/findings.mjs (Фаза 1, шаг 3).
//!
//! Вход (stdin):  {"op": "<имя функции>", ...аргументы}
//! Выход (stdout): результат в JSON-формате, эквивалентном возврату JS
//!                 (null вместо undefined; условные поля suggestion/quote/advisorClaims
//!                 пропускаются, как undefined в JS-объектах).
//! Ошибки:        non-zero exit code + {"error": "..."} в stderr.
//!
//! Мутации кластерного гейта НЕ применяются к входным находкам: операции
//! возвращают их списком (index + новые поля) — JS-обёртка накладывает на
//! исходные объекты, сохраняя семантику разделяемых ссылок в пайплайне.
//!
//! Операции:
//!   normalizeConfidence        {raw}                      → number
//!   normalizeFinding           {finding, model, requireQuote?} → Finding | null
//!   sameFindingKey             {finding}                  → string
//!   dedupeSameModelFindings    {findings}                 → {kept, dropped}
//!   softConsensusForBlastCluster {blockTier}              → bool | null
//!   clusterFindings            {findings, gate}           → {mutations, clusters}
//!   applyPrescanBlockPolicy    {clusters, findings, prescan} → [{index, consensusBlock}]
//!   resolveBlockers            {findings, gate, prescan?} → {mutations, prescanMutations, clusters, blockers, wouldBlock}
//!   findingFingerprint         {text?, line}             → string
//!   attachFingerprints         {findings, files?}         → [{index, fp}]
//!   filterStalePriorFindings   {priors, files}            → {kept, codeChanged, outOfDiff, staleLegacy, legacy}
//!
//! FP-regex-матчинг здесь отсутствует намеренно (JS lookahead в паттернах) —
//! см. шапку rust/src/findings.rs и lib/findings.mjs.

use std::io::Read;

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde_json::{json, Value};

use ai_reviewer::findings::{
    apply_prescan_block_policy, cluster_findings, dedupe_same_model_findings, filter_stale_prior_findings,
    finding_fingerprint, normalize_confidence, normalize_finding, resolve_blockers,
    same_finding_key, soft_consensus_for_blast_cluster, Gate, LineText, PriorFileRec,
};

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", json!({ "error": format!("{e:#}") }));
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("чтение stdin")?;
    let v: Value = serde_json::from_str(&input).context("вход не является JSON")?;
    let obj = v.as_object().context("вход должен быть JSON-объектом")?;
    let op = obj.get("op").and_then(Value::as_str).unwrap_or("");

    let arg = |name: &str| obj.get(name).unwrap_or(&Value::Null);

    let findings_arg = |name: &str| -> Result<Vec<Value>> {
        match obj.get(name) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(a)) => Ok(a.clone()),
            Some(_) => bail!("аргумент '{name}' должен быть массивом"),
        }
    };

    let out = match op {
        "normalizeConfidence" => json!(normalize_confidence(arg("raw"))),
        "normalizeFinding" => normalize_finding(
            arg("finding"),
            obj.get("model").and_then(Value::as_str).unwrap_or(""),
            obj.get("requireQuote").and_then(Value::as_bool).unwrap_or(false),
        ),
        "sameFindingKey" => json!(same_finding_key(arg("finding"))),
        "dedupeSameModelFindings" => {
            let (kept, dropped) = dedupe_same_model_findings(&findings_arg("findings")?);
            json!({ "kept": kept, "dropped": dropped })
        }
        "softConsensusForBlastCluster" => match soft_consensus_for_blast_cluster(&findings_arg("blockTier")?) {
            Some(b) => json!(b),
            None => Value::Null,
        },
        "clusterFindings" => {
            let findings = findings_arg("findings")?;
            let gate = Gate::from_value(arg("gate"));
            let outcome = cluster_findings(&findings, &gate);
            json!({
                "mutations": outcome.mutations,
                "clusters": outcome.clusters,
            })
        }
        "applyPrescanBlockPolicy" => {
            let findings = findings_arg("findings")?;
            let clusters: Vec<Vec<usize>> = serde_json::from_value(arg("clusters").clone())
                .context("аргумент 'clusters' должен быть массивом массивов индексов")?;
            let prescan = match obj.get("prescan") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.clone()),
            };
            let mutations = apply_prescan_block_policy(&clusters, &findings, prescan.as_ref());
            json!(mutations
                .into_iter()
                .map(|(index, consensus_block)| json!({ "index": index, "consensusBlock": consensus_block }))
                .collect::<Vec<_>>())
        }
        "resolveBlockers" => {
            let findings = findings_arg("findings")?;
            let gate = Gate::from_value(arg("gate"));
            let prescan = match obj.get("prescan") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.clone()),
            };
            let outcome = resolve_blockers(&findings, &gate, prescan.as_ref());
            json!({
                "mutations": outcome.mutations,
                "prescanMutations": outcome.prescan_mutations
                    .into_iter()
                    .map(|(index, b)| json!({ "index": index, "consensusBlock": b }))
                    .collect::<Vec<_>>(),
                "clusters": outcome.clusters,
                "blockers": outcome.blockers,
                "wouldBlock": outcome.would_block,
            })
        }
        "findingFingerprint" => {
            let text: Option<LineText> = match obj.get("text") {
                None | Some(Value::Null) => None,
                Some(Value::Object(m)) => {
                    let map: LineText = m
                        .iter()
                        .map(|(k, v)| {
                            let line: u64 = k.parse().context("ключ text должен быть номером строки")?;
                            let s = v.as_str().context("значение text должно быть строкой")?;
                            Ok((line, s.to_string()))
                        })
                        .collect::<Result<LineText>>()?;
                    Some(map)
                }
                Some(_) => bail!("аргумент 'text' должен быть объектом или null"),
            };
            json!(finding_fingerprint(text.as_ref(), arg("line")))
        }
        "attachFingerprints" => {
            let findings = findings_arg("findings")?;
            let files: IndexMap<String, Option<LineText>> = match obj.get("files") {
                None | Some(Value::Null) => IndexMap::new(),
                Some(Value::Object(m)) => m
                    .iter()
                    .map(|(k, v)| {
                        let rec = match v {
                            Value::Null => None,
                            Value::Object(rec) => match rec.get("text") {
                                None | Some(Value::Null) => None,
                                Some(Value::Object(t)) => Some(
                                    t.iter()
                                        .map(|(lk, lv)| {
                                            let line: u64 =
                                                lk.parse().context("ключ text должен быть номером строки")?;
                                            let s = lv.as_str().context("значение text должно быть строкой")?;
                                            Ok((line, s.to_string()))
                                        })
                                        .collect::<Result<LineText>>()?,
                                ),
                                Some(_) => bail!("files['{k}'].text должен быть объектом или null"),
                            },
                            _ => bail!("files['{k}'] должен быть объектом или null"),
                        };
                        Ok((k.clone(), rec))
                    })
                    .collect::<Result<IndexMap<_, _>>>()?,
                Some(_) => bail!("аргумент 'files' должен быть объектом"),
            };
            let fps: Vec<Value> = findings
                .iter()
                .enumerate()
                .filter_map(|(i, f)| {
                    let rec = f.get("file").and_then(Value::as_str).and_then(|p| files.get(p));
                    let fp = finding_fingerprint(rec.and_then(|r| r.as_ref()), f.get("line").unwrap_or(&Value::Null));
                    if fp.is_empty() {
                        None
                    } else {
                        Some(json!({ "index": i, "fp": fp }))
                    }
                })
                .collect();
            json!(fps)
        }
        "filterStalePriorFindings" => {
            let priors = findings_arg("priors")?;
            let files: IndexMap<String, PriorFileRec> = match obj.get("files") {
                None | Some(Value::Null) => IndexMap::new(),
                Some(Value::Object(m)) => m
                    .iter()
                    .map(|(k, v)| {
                        let rec = v.as_object().with_context(|| format!("files['{k}'] должен быть объектом"))?;
                        let valid_lines: Vec<f64> = match rec.get("validLines") {
                            None | Some(Value::Null) => Vec::new(),
                            Some(Value::Array(a)) => a
                                .iter()
                                .map(|n| n.as_f64().context("validLines должен быть массивом чисел"))
                                .collect::<Result<Vec<_>>>()?,
                            Some(_) => bail!("files['{k}'].validLines должен быть массивом"),
                        };
                        let text: Option<LineText> = match rec.get("text") {
                            None | Some(Value::Null) => None,
                            Some(Value::Object(t)) => Some(
                                t.iter()
                                    .map(|(lk, lv)| {
                                        let line: u64 =
                                            lk.parse().context("ключ text должен быть номером строки")?;
                                        let s = lv.as_str().context("значение text должно быть строкой")?;
                                        Ok((line, s.to_string()))
                                    })
                                    .collect::<Result<LineText>>()?,
                            ),
                            Some(_) => bail!("files['{k}'].text должен быть объектом или null"),
                        };
                        Ok((k.clone(), PriorFileRec { valid_lines, text }))
                    })
                    .collect::<Result<IndexMap<_, _>>>()?,
                Some(_) => bail!("аргумент 'files' должен быть объектом"),
            };
            let outcome = filter_stale_prior_findings(&priors, &files);
            json!({
                "kept": outcome.kept
                    .into_iter()
                    .map(|(index, line)| json!({ "index": index, "line": line }))
                    .collect::<Vec<_>>(),
                "codeChanged": outcome.code_changed,
                "outOfDiff": outcome.out_of_diff,
                "staleLegacy": outcome.stale_legacy,
                "legacy": outcome.legacy,
            })
        }
        other => bail!("unknown op: '{other}'"),
    };

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
