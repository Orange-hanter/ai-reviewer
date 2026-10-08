//! Порт lib/prompt.mjs (Фаза 3, шаг 3): конструкция промптов (createPromptBuilder,
//! buildVerifierMessages, buildChangedFilesSection, reference-секции).
//!
//! Побайтовый паритет с JS — промпты это ВХОД модели, любое расхождение меняет
//! поведение LLM. Проверяется rust/parity/prompt.parity.mjs (сравнение строк
//! построенных сообщений побайтово).
//!
//! Граница порта:
//! - мутабельный профиль (getProfile/repoRules) НЕ читается из процесса Rust:
//!   обёртка сериализует нужные части аргументами (repoRules, prompts.*,
//!   layout.e2eSpec — ИСХОДНИКОМ, без компиляции на стороне JS);
//! - layout.e2eSpec компилируется здесь с `(?i)` (профиль собирает
//!   `new RegExp(raw, 'i')`). JS-only синтаксис (lookahead и т.п.) — ошибка
//!   с маркером E2ESPEC_UNSUPPORTED: обёртка деградирует на чистый JS, как
//!   layout.sensitive в actionability;
//! - JS-семантика строк сохранена: replace(/[...]+/g, ' ') схлопывает ПРОГОНЫ
//!   символов в один пробел; .slice(0, 200) режет по UTF-16 юнитам (суррогатные
//!   пары считаются за два).

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use crate::findings::{js_to_string, js_truthy};

/// Части активного профиля, нужные билдерам (сериализует JS-обёртка).
pub struct PromptCtx {
    pub repo_rules: String,
    pub advisor_extra: String,
    pub specialist_extra: String,
    pub aggregator_extra: String,
    pub verifier_extra: String,
    /// Исходник layout.e2eSpec (без флагов) или null.
    pub e2e_spec_source: Option<String>,
}

impl PromptCtx {
    pub fn from_json(v: &Value) -> PromptCtx {
        let s = |key: &str| match v.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(other) if js_truthy(other) => js_to_string(other),
            _ => String::new(),
        };
        let prompts = v.get("prompts").unwrap_or(&Value::Null);
        let extra = |key: &str| match prompts.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(other) if js_truthy(other) => js_to_string(other),
            _ => String::new(),
        };
        let e2e_spec_source = match v.get("e2eSpecSource") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        };
        PromptCtx {
            repo_rules: s("repoRules"),
            advisor_extra: extra("advisorExtra"),
            specialist_extra: extra("specialistExtra"),
            aggregator_extra: extra("aggregatorExtra"),
            verifier_extra: extra("verifierExtra"),
            e2e_spec_source,
        }
    }
}

/// `${rules}\n` или '' (rulesBlock в JS).
fn rules_block(ctx: &PromptCtx) -> String {
    if ctx.repo_rules.is_empty() {
        String::new()
    } else {
        format!("{}\n", ctx.repo_rules)
    }
}

/// `\n${text}\n` или '' (promptExtra в JS).
fn prompt_extra(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        format!("\n{text}\n")
    }
}

/// String(text ?? '').replace(/```/g, '``\u{200B}`') — fence не ломается кодом.
pub fn fence_escape(text: &str) -> String {
    text.replace("```", "``\u{200B}`")
}

/// JS `s.replace(/[...]+/g, ' ')` — прогоны символов из предиката → один пробел.
fn replace_runs<F: Fn(char) -> bool>(s: &str, pred: F) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        if pred(c) {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

/// JS `s.slice(0, end)` по UTF-16 кодовым юнитам (emoji/не-BMP считаются за 2).
fn js_slice_utf16(s: &str, end: usize) -> String {
    let mut units = 0usize;
    let mut out = String::new();
    for c in s.chars() {
        let n = c.len_utf16();
        if units + n > end {
            break;
        }
        units += n;
        out.push(c);
    }
    out
}

/// PR title: убираем backticks и переводы строк, режем до 200 UTF-16 юнитов.
fn safe_pr_title(title: Option<&Value>) -> String {
    let raw = match title {
        Some(t) if js_truthy(t) => js_to_string(t),
        _ => String::new(),
    };
    js_slice_utf16(&replace_runs(&raw, |c| c == '`' || c == '\n' || c == '\r'), 200)
}

/// Путь в backticks: управляющие символы не подделают метку части.
fn safe_path(p: &Value) -> String {
    replace_runs(&js_to_string(p), |c| c == '`' || c == '\r' || c == '\n')
}

fn msg(role: &str, content: String) -> Value {
    let mut m = Map::new();
    m.insert("role".into(), json!(role));
    m.insert("content".into(), json!(content));
    Value::Object(m)
}

fn str_arg(args: &Value, key: &str) -> String {
    match args.get(key) {
        Some(v) if js_truthy(v) => js_to_string(v),
        _ => String::new(),
    }
}

fn arr_arg(args: &Value, key: &str) -> Vec<Value> {
    match args.get(key) {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Статические хвосты системных промптов (перенесены посимвольно из lib/prompt.mjs)
// ---------------------------------------------------------------------------

// Потолок числа пунктов у одной роли (RFC 04, решение 3). Величина зашита с обеих сторон:
// см. SPECIALIST_MAX_ITEMS в lib/prompt.mjs — расхождение здесь означает разные промпты
// на машине с бинарём и без него.
const REFERENCE_TAIL: &str = r#"
Analyze the PR diff and the attached file context (windows around the changed lines). List ONLY
real defects: bugs, security/RBAC issues, layer-boundary violations, missing tests for
non-trivial logic. Do NOT nitpick style (eslint/tsc).
Write IN RUSSIAN as concise bullet points:
- [severity] `file:line` — title: explanation (severity = critical|major|minor)
  quote: <one line copied verbatim from the diff or file context>
Report AT MOST 8 items, strongest first. Do NOT report nits. EVERY item, whatever its
severity, MUST carry its own `quote:` line: if you cannot quote the offending line verbatim AND name a
concrete fix, leave the item out.
Do NOT output JSON — a separate aggregator will synthesize structured findings from your advice.
If nothing serious — say «серьёзных замечаний нет»."#;

const SPECIALIST_FOCUS: &[(&str, &str)] = &[
    ("security", "Focus ONLY on security: injections, auth/RBAC bypass, IDOR, data leaks, secrets in code,\nunsafe deserialization, SSRF, path traversal. Ignore style and non-security logic nitpicks."),
    ("correctness", "Focus ONLY on correctness: logic bugs, races, null/edge cases, wrong rounding of money/quantities,\noff-by-one, broken error handling, half-done decoding of an external format (a quoted/escaped/encoded value is\nrecognized and sliced out, but the escapes inside it are never decoded back), regressions visible in the\ndiff/context. Ignore security-only and architecture lectures."),
    ("architecture", "Focus ONLY on architecture/layers: module boundaries of this project, forbidden imports,\nduplication of canonical primitives (see the project rules above), blast-radius breaking changes. Ignore pure security and micro-bugs."),
];

const SPECIALIST_TAIL: &str = r#"

Analyze the PR diff and the attached file context (windows around the changed lines). List ONLY issues
in YOUR specialty. Write IN RUSSIAN as bullets:
- [severity] `file:line` — title: explanation (severity = critical|major|minor)
  quote: <one line copied verbatim from the diff or file context>
Report AT MOST 8 items, strongest first — a long tail costs the jury more than it finds.
Do NOT report nits. EVERY item, whatever its severity, MUST carry its own `quote:` line: if you cannot
quote the offending line verbatim AND name a concrete fix, leave the item out.
Better three proven defects than twenty observations.
Do NOT output JSON — the jury aggregator will refute/verify each claim.
If nothing in your specialty — say «в моей области серьёзных замечаний нет»."#;

// Хвост системного промпта жюри разрезан на четыре части: два абзаца УСЛОВНЫ и едут только
// вместе со своей секцией в user-части — BLAST-RADIUS RULE при непустом blast-radius,
// CONVERGENCE при непустых находках прошлого прогона (RFC 04, экономия входа). Ведущий `\n\n`
// принадлежит самому условному абзацу: иначе выключенный блок оставляет висячую пустую строку
// и текст расходится с JS-копией побайтово.
//
// Потолок числа находок у жюри (AT MOST 12) зашит с обеих сторон: см. AGGREGATOR_MAX_ITEMS
// в lib/prompt.mjs — расхождение даёт разные промпты с бинарём и без него.
const AGGREGATOR_TAIL_HEAD: &str = r#"
What to look for (in decreasing importance): real bugs and logic errors; vulnerabilities (injections,
privilege/data leaks, RBAC bypass); layer-boundary violations and duplication of canonical primitives;
races/edge-cases; missing tests for non-trivial logic; breaking callers per blast-radius.
Do NOT nitpick style/formatting/naming — eslint and tsc catch those."#;

const AGGREGATOR_BLAST_RULE: &str = r#"

BLAST-RADIUS RULE: when the "Blast-radius" section lists callers/dependents outside the diff, treat a
signature/behavior/RBAC change to that symbol as a likely regression for those callers unless the PR
updates them too or the change is proven compatible. Prefer a critical/major finding that names at
least one concrete caller (symbol @ path) from that section. Trust [EXTRACTED] edges over [INFERRED];
entries marked cross-community are stronger regression signals (structurally distant in the AST graph).
If blast-radius is empty or says symbols were not resolved — do NOT invent imaginary callers."#;

const AGGREGATOR_TAIL_MID: &str = r#"

Respond STRICTLY with a single JSON object inside a ```json fence, with no text around it. Schema:
{
  "summary": "one sentence: overall verdict, WRITTEN IN RUSSIAN",
  "findings": [
    {
      "file": "path as in the diff",
      "line": <RIGHT-side line number from the annotation '  123: + ...'>,
      "severity": "critical" | "major" | "minor" | "nit",
      "confidence": 0.0-1.0,
      "title": "short title, WRITTEN IN RUSSIAN",
      "body": "2–3 sentences: what breaks, why, and the fix, WRITTEN IN RUSSIAN",
      "quote": "1–3 lines of code copied verbatim from the shown diff or attached file context",
      "suggestion": "opt. fixed code as one or several lines"
    }
  ]
}
IMPORTANT: summary/title/body are shown to Russian-speaking developers — write them IN RUSSIAN (keep code,
identifiers and file paths as-is). Set line EXACTLY as in the '123:' prefix of the line in the annotated
diff. If a finding is not about a specific line — omit line (leave 0). Each finding MUST include
`quote`: 1–3 lines copied verbatim from the diff or attached file context that prove the issue.
Do not invent findings without a real problem — an empty findings array is fine. Set confidence to how sure you are (0–1); only use ≥0.8
for critical/major you want to block merge on.
Output AT MOST 12 findings, strongest first — the specialists already capped theirs at 8 each; do not multiply them.

Do NOT flag as a finding: (a) version numbers of GitHub Actions/Node/tools as "non-existent" — your
knowledge may be stale, the versions in the repo are working; (b) deliberate project decisions — sending
the diff to the configured Ollama Cloud (it is the owner's subscription) and the very fact of blocking
merge on critical/major; (c) a symbol/function/constant/import as "undefined"/"not imported"/"missing"
just because you do not SEE its declaration in the shown fragment — a large file is split into chunks and
the declaration very likely lives in another chunk; (d) claims that code is a "syntax error" or violates
language/shell/ESM/regex semantics (word splitting, hoisting, import order, how the interpreter reads an
escape sequence) — you cannot run an interpreter here, and such claims are wrong disproportionately often;
if you still suspect one, verify it against the shown code character-by-character and set confidence below
0.8. That ban is about the LANGUAGE, not about DATA: code that reads a quoted/escaped input format (quoted
paths, shell quoting, URL/HTML/JSON escaping) and never decodes it back is an ordinary logic bug — report
it at its real severity. Only report a truly LOCAL logic error you can prove from the shown code. Look for
real defects in the PR code, don't argue with architectural decisions.

CONTEXT AND "NO OPEN QUESTIONS": besides the diff you get the context of the changed files — windows
around the lines this PR touches, where every cut-out region is marked «… (строки N–M пропущены)» — and
the blast-radius of their dependencies. USE this to RESOLVE your doubts instead of leaving them open. Each
finding must be a verified STATEMENT with a concrete fix — NOT a question ("what if…?", "please check …")
and NOT a "maybe". If the answer is in the provided code — check it and draw a conclusion. A skip marker
means "not shown here", NEVER "does not exist" — do not conclude absence from the text behind it. If there
is not enough data even with this context — do NOT invent a finding (better to skip it)."#;

const AGGREGATOR_CONVERGENCE: &str = r#"

CONVERGENCE BETWEEN RUNS: if the "Findings from the PREVIOUS run" section is given below — those are things
already raised (possibly by another model). Do NOT re-derive the review from scratch and do NOT spawn NEW
wordings of the same problems: check each previous item against the CURRENT code (fixed → stay silent;
still present → repeat with the same title and line) and add only GENUINELY new defects. The goal is a
stable set from run to run, not a rotation of wordings."#;

/// Маркер деградации: e2eSpec не компилируется regex-крейтом (JS-only синтаксис).
pub const E2ESPEC_UNSUPPORTED: &str = "E2ESPEC_UNSUPPORTED";

/// layout.e2eSpec → regex с `(?i)`; JS-only синтаксис → ошибка с маркером.
fn e2e_re(ctx: &PromptCtx) -> Result<Option<regex::Regex>> {
    match &ctx.e2e_spec_source {
        None => Ok(None),
        Some(src) => match regex::Regex::new(&format!("(?i){src}")) {
            Ok(re) => Ok(Some(re)),
            Err(e) => bail!("{E2ESPEC_UNSUPPORTED}: {e}"),
        },
    }
}

/// You are a code review advisor (reference model …) — системный промпт advisor'а.
fn reference_rules(ctx: &PromptCtx) -> String {
    let mut s = String::from("You are a code review advisor (reference model in a Mixture-of-Agents pipeline).\n");
    s.push_str(&rules_block(ctx));
    s.push_str(&prompt_extra(&ctx.advisor_extra));
    s.push_str(REFERENCE_TAIL);
    s
}

/// Системный промпт specialist'а (роль + focus + правила).
fn specialist_rules(role: &str, ctx: &PromptCtx) -> String {
    let focus = SPECIALIST_FOCUS
        .iter()
        .find(|(r, _)| *r == role)
        .map(|(_, f)| *f)
        .unwrap_or("");
    let mut s = format!("You are a SPECIALIST code review advisor — role: **{role}** (Mixture-of-Critics pipeline).\n");
    s.push_str(&rules_block(ctx));
    s.push_str(&prompt_extra(&ctx.specialist_extra));
    // Литеральный перевод строки после строки ${rulesBlock()}${promptExtra(...)}
    // в JS-шаблоне — сохраняется даже когда обе вставки пусты; дальше строка
    // ${focus} (пустая тоже даёт свой перевод строки — см. SPECIALIST_TAIL).
    s.push('\n');
    s.push_str(focus);
    s.push_str(SPECIALIST_TAIL);
    s
}

/// buildSpecialistReferenceSection — секция prose-анализов specialists для жюри.
pub fn build_specialist_reference_section(references: &[Value]) -> String {
    let ok: Vec<String> = references
        .iter()
        .filter(|r| js_truthy(r.get("text").unwrap_or(&Value::Null)) && !js_truthy(r.get("error").unwrap_or(&Value::Null)))
        .map(|r| format!("### Specialist: {} ({})\n{}", js_to_string(r.get("role").unwrap_or(&Value::Null)), js_to_string(r.get("model").unwrap_or(&Value::Null)), js_to_string(r.get("text").unwrap_or(&Value::Null))))
        .collect();
    if ok.is_empty() {
        return String::new();
    }
    let failed: Vec<String> = references
        .iter()
        .filter(|r| js_truthy(r.get("error").unwrap_or(&Value::Null)))
        .map(|r| format!("- {} ({}): {}", js_to_string(r.get("role").unwrap_or(&Value::Null)), js_to_string(r.get("model").unwrap_or(&Value::Null)), js_to_string(r.get("error").unwrap_or(&Value::Null))))
        .collect();
    let mut s = format!("## Specialist advisor analyses (Mixture-of-Critics — synthesize + refute)\n\n{}\n", ok.join("\n\n"));
    if !failed.is_empty() {
        s.push_str(&format!("\n### Specialists unavailable\n{}\n", failed.join("\n")));
    }
    s.push_str("\nYou are the JURY aggregator. Synthesize findings from specialists. For EACH output finding include `advisorClaims`:\none entry per specialist role that raised a related concern, with YOUR refutation:\n- **verified** — claim proven in the code (keep finding)\n- **rejected** — false positive / already mitigated (if ALL roles rejected → omit finding entirely)\n- **downgraded** — real but not merge-blocking (lower severity to minor/nit)\n\nDo NOT blindly copy specialist bullets — verify against code.\n\n");
    s
}

/// buildMoAReferenceSection — секция prose-анализов advisors для агрегатора.
pub fn build_moa_reference_section(references: &[Value]) -> String {
    let ok: Vec<String> = references
        .iter()
        .filter(|r| js_truthy(r.get("text").unwrap_or(&Value::Null)) && !js_truthy(r.get("error").unwrap_or(&Value::Null)))
        .map(|r| format!("### Advisor: {}\n{}", js_to_string(r.get("model").unwrap_or(&Value::Null)), js_to_string(r.get("text").unwrap_or(&Value::Null))))
        .collect();
    if ok.is_empty() {
        return String::new();
    }
    let failed: Vec<String> = references
        .iter()
        .filter(|r| js_truthy(r.get("error").unwrap_or(&Value::Null)))
        .map(|r| format!("- {}: {}", js_to_string(r.get("model").unwrap_or(&Value::Null)), js_to_string(r.get("error").unwrap_or(&Value::Null))))
        .collect();
    let mut s = format!("## Reference model analyses (Mixture-of-Agents — synthesize, do not blindly copy)\n\n{}\n", ok.join("\n\n"));
    if !failed.is_empty() {
        s.push_str(&format!("\n### Advisors unavailable\n{}\n", failed.join("\n")));
    }
    s.push_str("\nUse the advisors as hints: verify each claim against the code, drop false positives, merge duplicates, and output the final JSON findings yourself.\n\n");
    s
}

/// buildChangedFilesSection — карта PR («что в этом чанке, что ревьюится отдельно»)
/// с запретом absence-claims и списком e2e-спек. Результат '' для пустого списка.
pub fn build_changed_files_section(changed_paths: &[Value], chunk: &Value, ctx: &PromptCtx) -> Result<String> {
    if changed_paths.is_empty() {
        return Ok(String::new());
    }
    let partial: Vec<Value> = match chunk.get("partialPaths") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    // Пути с урезанным окнами контекстом (RFC 07): поле дописывает pipeline после нарезки
    // чанков, buildChunks о нём не знает.
    let windowed: Vec<Value> = match chunk.get("windowedPaths") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let mut chunk_paths: Vec<Value> = match chunk.get("paths") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    chunk_paths.extend(partial.iter().cloned());
    // split = (chunkPaths.size > 0 || partialPaths.size > 0)
    //   && (partialPaths.size > 0 || windowedPaths.size > 0 || changedPaths.some((p) => !chunkPaths.has(p)))
    let split = (!chunk_paths.is_empty() || !partial.is_empty())
        && (!partial.is_empty()
            || !windowed.is_empty()
            || changed_paths.iter().any(|p| !chunk_paths.contains(p)));
    let mark = |p: &Value| -> &'static str {
        if !split {
            return "";
        }
        if !chunk_paths.contains(p) {
            return " — [ANOTHER PART]";
        }
        // Частичность старше оконности: у разрезанного файла отсутствует часть ХУНКОВ,
        // а не только текст между ними (см. lib/prompt.mjs).
        if partial.contains(p) {
            return " — [THIS PART, SOME HUNKS ONLY]";
        }
        if windowed.contains(p) {
            return " — [THIS PART, CONTEXT WINDOWED]";
        }
        " — [THIS PART]"
    };
    let e2e_re = e2e_re(ctx)?;
    let e2e: Vec<Value> = match &e2e_re {
        None => Vec::new(),
        Some(re) => changed_paths
            .iter()
            .filter(|p| re.is_match(&js_to_string(p)))
            .cloned()
            .collect(),
    };
    // Пути ЭТОГО чанка — маркированными пунктами, пути чужих частей — одной строкой:
    // их роль в промпте ровно одна (доказать, что файл в PR ЕСТЬ), а своя строка с меткой
    // на каждый стоила ~600 символов входа в каждом вызове прогона. Метка [ANOTHER PART]
    // стоит один раз на строку — на неё дословно ссылается SPLIT_NOTE. См. lib/prompt.mjs.
    let mut rows: Vec<String> = changed_paths
        .iter()
        .filter(|p| !split || chunk_paths.contains(p))
        .map(|p| format!("- `{}`{}", safe_path(p), mark(p)))
        .collect();
    let elsewhere: Vec<&Value> = if split {
        changed_paths.iter().filter(|p| !chunk_paths.contains(p)).collect()
    } else {
        Vec::new()
    };
    if !elsewhere.is_empty() {
        rows.push(format!(
            "Reviewed in other parts ({}) — [ANOTHER PART]: {}",
            elsewhere.len(),
            elsewhere.iter().map(|p| format!("`{}`", safe_path(p))).collect::<Vec<_>>().join(", ")
        ));
    }
    let mut s = format!("## All changed files in the PR ({})\n{}\n", changed_paths.len(), rows.join("\n"));
    if split {
        s.push('\n');
        if !partial.is_empty() || !elsewhere.is_empty() {
            s.push_str(SPLIT_NOTE_PARTS);
        }
        if !windowed.is_empty() {
            s.push_str(SPLIT_NOTE_WINDOWED);
        }
        s.push_str(SPLIT_NOTE_RULE);
    }
    if !e2e.is_empty() {
        let e2e_lines: Vec<String> = e2e
            .iter()
            .map(|p| format!("- `{}`{}", safe_path(p), mark(p)))
            .collect();
        s.push_str(&format!("\n## E2E specs in this PR (Playwright)\n{}\n", e2e_lines.join("\n")));
        s.push_str("Do not claim that e2e is missing — the list above is taken from the PR diff.\n");
    }
    Ok(s)
}

/// Три блока примечания к карте PR (перенесены посимвольно из lib/prompt.mjs). Каждый
/// УСЛОВЕН по отдельности: части — когда PR действительно разрезан, окна — когда контекст
/// урезан (RFC 07), сам запрет — всегда, когда видно не всё.
const SPLIT_NOTE_PARTS: &str = r#"This PR is reviewed in parts. The diff below covers ONLY the paths marked [THIS PART]
(file context is attached for some of them, not for all); a path marked
[THIS PART, SOME HUNKS ONLY] has further hunks shown to another part.
Paths marked [ANOTHER PART] ARE changed by this PR — another part of the same run reviews them.
"#;

const SPLIT_NOTE_WINDOWED: &str = r#"A path marked [THIS PART, CONTEXT WINDOWED] has ALL of its hunks in the diff below, but its
attached file context is cut to windows around the changed lines: every gap carries a
«… (строки N–M пропущены)» marker, which means "not shown here", never "does not exist".
"#;

const SPLIT_NOTE_RULE: &str = r#"HARD RULE — NO ABSENCE CLAIMS: never state or imply that anything listed above is "missing",
"absent from the PR", "not updated", "not added", "forgotten", "has no test/migration/doc", or that
the PR "only touches" some of these files — you cannot see all of that code here, and the list itself
proves they are changed. Do NOT assert what a file you cannot fully see contains or does not contain,
whether it is an [ANOTHER PART] file or a region cut out behind a «… (строки N–M пропущены)» marker.
A cross-file concern may still be raised, but only as a risk with confidence ≤ 0.5, phrased as a
dependency to check — never as a statement about the unseen code.
"#;

/// Условный абзац системного промпта verifier'а: правила чтения blast-radius без самого
/// blast-radius — чистый расход входа (см. AGGREGATOR_BLAST_RULE). Ведущий `\n` — часть абзаца.
const VERIFIER_BLAST_RULE: &str = r#"
When a candidate claims a breaking change / missed caller update and blast-radius lists that caller (or
another concrete dependent), prefer **verified** unless the PR already updates that caller or the change
is clearly compatible. Do NOT invent callers that are absent from blast-radius and the shown code.
"#;

/// Хвост системного промпта verifier'а (Pass B, R3).
const VERIFIER_SYSTEM_TAIL: &str = r#"
Do NOT invent new findings. Do NOT nitpick style. Write reasons IN RUSSIAN.

Respond STRICTLY with a single JSON object inside a ```json fence:
{
  "verdicts": [
    {
      "candidateIndex": 0,
      "decision": "verified" | "rejected" | "downgrade",
      "reason": "1–2 sentences IN RUSSIAN",
      "severity": "minor"
    }
  ]
}
Include one verdict per candidate index. severity field only when decision is downgrade."#;

/// buildVerifierMessages — промпт Pass B: докажи по коду или reject/downgrade.
pub fn build_verifier_messages(args: &Value, ctx: &PromptCtx) -> Result<Vec<Value>> {
    let mut system = String::from("You are a strict code review VERIFIER (second pass). You receive candidate findings from an\ninitial review. For EACH candidate you must decide using ONLY the provided diff, the attached file\ncontext (windows around the candidate lines), and blast-radius (if present):\n- **verified** — the defect is real and provable from the shown code;\n- **rejected** — false positive, speculative, or already mitigated in context;\n- **downgrade** — real concern but not merge-blocking (set severity to minor or nit).\n");
    system.push_str(&prompt_extra(&ctx.verifier_extra));
    let blast_blocks = arr_arg(args, "blastBlocks");
    if !blast_blocks.is_empty() {
        system.push_str(VERIFIER_BLAST_RULE);
    }
    system.push_str(VERIFIER_SYSTEM_TAIL);

    let candidates = arr_arg(args, "candidates");
    let list: String = candidates
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let line = match f.get("line") {
                Some(l) if js_truthy(l) => format!(":{}", js_to_string(l)),
                _ => String::new(),
            };
            let conf = match f.get("confidence") {
                Some(c) if !matches!(c, Value::Null) => js_to_string(c),
                _ => "—".to_string(),
            };
            format!(
                "{}. [{}] {}{line} — **{}** ({}, conf={})\n   {}",
                i,
                js_to_string(f.get("severity").unwrap_or(&Value::Null)),
                js_to_string(f.get("file").unwrap_or(&Value::Null)),
                js_to_string(f.get("title").unwrap_or(&Value::Null)),
                match f.get("model") {
                    Some(m) if js_truthy(m) => js_to_string(m),
                    _ => "?".to_string(),
                },
                conf,
                match f.get("body") {
                    Some(b) if js_truthy(b) => js_to_string(b),
                    _ => String::new(),
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let blast = if !blast_blocks.is_empty() {
        format!(
            "## Blast-radius (AST dependents — prefer [EXTRACTED] and cross-community as stronger evidence)\n{}\n\n",
            blast_blocks.iter().map(|b| js_to_string(b)).collect::<Vec<_>>().join("\n")
        )
    } else {
        "## Blast-radius: empty — do not invent external callers\n\n".to_string()
    };

    let file_contents = fence_escape(&str_arg(args, "fileContents"));
    let file_part = if file_contents.is_empty() {
        "(нет)".to_string()
    } else {
        file_contents
    };
    let pr = args.get("pr").cloned().unwrap_or(Value::Null);
    let pr_number = js_to_string(pr.get("number").unwrap_or(&Value::Null));
    let title = safe_pr_title(pr.get("title"));

    let user = format!(
        "PR #{pr_number}: «{title}»\n\n{changed}{blast}## Candidates to verify ({n})\n{list}\n\n## Context of changed files (windows around the candidate lines; «… (строки N–M пропущены)» marks text not shown)\n{file_part}\n\n## Diff (RIGHT-side line numbers)\n```diff\n{annotated}\n```",
        changed = str_arg(args, "changedFilesSection"),
        n = candidates.len(),
        annotated = fence_escape(&str_arg(args, "annotated")),
    );

    Ok(vec![msg("system", system), msg("user", user)])
}

/// buildMessages — основной промпт ревью (чанк диффа + контекст).
/// Заголовок блока карточек символов (RFC 14); зеркало SYMBOL_CARDS_HEADER в lib/prompt.mjs.
const SYMBOL_CARDS_HEADER: &str = "## Declarations of symbols called by the changed lines (defined outside the context shown above)\nEach entry is the declaration only — the body is NOT shown. It proves the symbol EXISTS and what its signature is; it proves NOTHING about what the body does: do not report the behaviour of such a symbol and do not call it undefined. An entry marked (full body) shows the whole short body.";

pub fn build_messages(args: &Value, ctx: &PromptCtx) -> Result<Vec<Value>> {
    let changed_paths: Option<Vec<Value>> = match args.get("changedPaths") {
        Some(Value::Array(a)) => Some(a.clone()),
        _ => None,
    };
    let chunk = args.get("chunk").unwrap_or(&Value::Null);
    let files_map = match &changed_paths {
        Some(paths) => build_changed_files_section(paths, chunk, ctx)?,
        None => str_arg(args, "changedFilesSection"),
    };

    let blast_blocks = arr_arg(args, "blastBlocks");
    let priors = arr_arg(args, "priorFindings");

    let mut system = String::from("You are a senior review engineer. You perform a strict but constructive code review of a PR.\n");
    system.push_str(&rules_block(ctx));
    system.push_str(&prompt_extra(&ctx.aggregator_extra));
    system.push_str(AGGREGATOR_TAIL_HEAD);
    if !blast_blocks.is_empty() {
        system.push_str(AGGREGATOR_BLAST_RULE);
    }
    system.push_str(AGGREGATOR_TAIL_MID);
    if !priors.is_empty() {
        system.push_str(AGGREGATOR_CONVERGENCE);
    }

    let blast = if !blast_blocks.is_empty() {
        format!(
            "## Blast-radius of changed symbols (Graphify AST code graph — who depends on them outside the diff)\n{}\nFormat: `caller` @ file:line [relation][EXTRACTED|INFERRED] [cross-community?]. Use these callers as evidence: if the PR changes a symbol's contract and a listed caller is not updated, that is a merge-blocking regression candidate (cite the caller symbol and path in the finding body).\n",
            blast_blocks.iter().map(|b| js_to_string(b)).collect::<Vec<_>>().join("\n")
        )
    } else {
        "## Blast-radius: empty (no exports resolved, no external dependents, or AST graph unavailable — do not invent callers)\n".to_string()
    };

    let file_contents = str_arg(args, "fileContents");
    let files_section = if !file_contents.is_empty() {
        format!("## Context of changed files (PR version — windows around the changed lines; «… (строки N–M пропущены)» marks text not shown)\n{}\n\n", fence_escape(&file_contents))
    } else {
        String::new()
    };

    let prior_section = if !priors.is_empty() {
        let items: Vec<String> = priors
            .iter()
            .map(|f| {
                let file = match f.get("file") {
                    Some(v) if js_truthy(v) => js_to_string(v),
                    _ => "—".to_string(),
                };
                let line = match f.get("line") {
                    Some(l) if js_truthy(l) => format!(":{}", js_to_string(l)),
                    _ => String::new(),
                };
                let title = match f.get("title") {
                    Some(t) if js_truthy(t) => js_to_string(t),
                    _ => String::new(),
                };
                let model = match f.get("model") {
                    Some(m) if js_truthy(m) => js_to_string(m),
                    _ => "?".to_string(),
                };
                format!("- [{}] {}{line} — {title} ({model})", js_to_string(f.get("severity").unwrap_or(&Value::Null)), file)
            })
            .collect();
        format!(
            "## Findings from the PREVIOUS review run (for convergence — do NOT start from scratch)\n{}\nCheck EACH item against the current code: fixed → do not repeat; still present → repeat with the same title/line. Do not rephrase an already-closed item as a \"new\" finding.\n\n",
            items.join("\n")
        )
    } else {
        String::new()
    };

    // Карточки символов (RFC 14): готовые строки из lib/symbol-cards.mjs в поле чанка.
    let cards: Vec<String> = match chunk.get("symbolCards") {
        Some(Value::Array(a)) => a.iter().map(js_to_string).collect(),
        _ => Vec::new(),
    };
    let cards_section = if !cards.is_empty() {
        format!("{SYMBOL_CARDS_HEADER}\n```\n{}\n```\n\n", fence_escape(&cards.join("\n")))
    } else {
        String::new()
    };

    let prescan_section = str_arg(args, "prescanSection");
    let prescan_part = if !prescan_section.is_empty() {
        format!("## Deterministic pre-scan (eslint/ast-grep on changed files — already reported, do NOT duplicate)\n{prescan_section}\n\n")
    } else {
        String::new()
    };

    let pr = args.get("pr").cloned().unwrap_or(Value::Null);
    let pr_number = js_to_string(pr.get("number").unwrap_or(&Value::Null));
    let title = safe_pr_title(pr.get("title"));
    let files_map_or_empty = if files_map.is_empty() { String::new() } else { files_map };

    let user = format!(
        "PR #{pr_number}: «{title}»\n\n{files_map_or_empty}{prescan_part}{blast}\n{prior_section}{files_section}{cards_section}## Diff (RIGHT-side line numbers in the prefix; take the inline line FROM HERE)\n```diff\n{annotated}\n```",
        annotated = fence_escape(&str_arg(args, "annotated")),
    );

    Ok(vec![msg("system", system), msg("user", user)])
}

/// buildSpecialistMessages — system specialist'а + user из base.
pub fn build_specialist_messages(role: &str, base_messages: &[Value], ctx: &PromptCtx) -> Vec<Value> {
    let user = base_messages
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|m| js_to_string(m.get("content").unwrap_or(&Value::Null)))
        .unwrap_or_default();
    vec![msg("system", specialist_rules(role, ctx)), msg("user", user)]
}

/// buildReferenceMessages — system advisor'а + user из base.
pub fn build_reference_messages(base_messages: &[Value], ctx: &PromptCtx) -> Vec<Value> {
    let user = base_messages
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|m| js_to_string(m.get("content").unwrap_or(&Value::Null)))
        .unwrap_or_default();
    vec![msg("system", reference_rules(ctx)), msg("user", user)]
}

/// buildSpecialistAggregatorMessages — жюри: секция specialists + схема refutation.
/// Подставляется content ПОСЛЕДНЕГО user-сообщения (i === length-1 в JS-map).
pub fn build_specialist_aggregator_messages(base_messages: &[Value], references: &[Value]) -> Vec<Value> {
    let ref_section = build_specialist_reference_section(references);
    let last = base_messages.len().saturating_sub(1);
    base_messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if m.get("role").and_then(Value::as_str) == Some("user") && i == last {
                let user = js_to_string(m.get("content").unwrap_or(&Value::Null));
                msg("user", format!("{ref_section}{REFUTATION_SCHEMA}\n{user}"))
            } else {
                m.clone()
            }
        })
        .collect()
}

/// buildAggregatorMessages — MoA-агрегатор: секция advisors перед последним user.
pub fn build_aggregator_messages(base_messages: &[Value], references: &[Value]) -> Vec<Value> {
    let ref_section = build_moa_reference_section(references);
    let last = base_messages.len().saturating_sub(1);
    base_messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if m.get("role").and_then(Value::as_str) == Some("user") && i == last {
                let user = js_to_string(m.get("content").unwrap_or(&Value::Null));
                msg("user", format!("{ref_section}{user}"))
            } else {
                m.clone()
            }
        })
        .collect()
}

const REFUTATION_SCHEMA: &str = r#"
In specialist mode EACH finding MUST include advisorClaims (refutation per role):
"advisorClaims": [
  { "role": "security"|"correctness"|"architecture", "status": "verified"|"rejected"|"downgraded", "reason": "RU", "severity": "minor"|"nit" }
]
For status **downgraded** set severity to minor or nit. Findings without advisorClaims on critical/major will be auto-downgraded by the pipeline."#;

impl PromptCtx {
    /// Контекст без профиля (пустые правила/вставки) — для служебных вызовов.
    pub fn default_ctx() -> PromptCtx {
        PromptCtx {
            repo_rules: String::new(),
            advisor_extra: String::new(),
            specialist_extra: String::new(),
            aggregator_extra: String::new(),
            verifier_extra: String::new(),
            e2e_spec_source: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Тесты — ключевые случаи из test/pr-review.test.mjs / test/profile.test.mjs
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PromptCtx {
        PromptCtx::default_ctx()
    }

    #[test]
    fn fence_escape_inserts_zero_width_space() {
        assert_eq!(fence_escape("a```b"), "a``\u{200B}`b");
        assert_eq!(fence_escape(""), "");
    }

    #[test]
    fn safe_pr_title_strips_backticks_newlines_and_truncates() {
        // runs of [`\n\r] collapse to a single space
        assert_eq!(safe_pr_title(Some(&json!("a``\n\r\nb"))), "a b");
        let long = "x".repeat(300);
        assert_eq!(safe_pr_title(Some(&json!(long))).len(), 200);
        // UTF-16 юниты: 150 emoji (300 юнитов) → 100 emoji
        let emoji = "😀".repeat(150);
        assert_eq!(safe_pr_title(Some(&json!(emoji))), "😀".repeat(100));
        assert_eq!(safe_pr_title(Some(&json!(0))), "");
        assert_eq!(safe_pr_title(None), "");
    }

    #[test]
    fn changed_files_section_marks_and_spoof_protection() {
        let c = ctx();
        let paths = vec![json!("a.ts"), json!("b.ts"), json!("big.ts")];
        let whole = build_changed_files_section(&paths, &json!({}), &c).unwrap();
        assert!(!whole.contains("[THIS PART]"));
        assert!(!whole.contains("NO ABSENCE CLAIMS"));

        let part = build_changed_files_section(&paths, &json!({"paths": ["a.ts", "big.ts"], "partialPaths": ["big.ts"]}), &c).unwrap();
        assert!(part.contains("- `a.ts` — [THIS PART]"));
        // чужая часть — одной строкой, а не пунктом на путь (RFC 04, экономия входа)
        assert!(part.contains("Reviewed in other parts (1) — [ANOTHER PART]: `b.ts`"));
        assert!(part.contains("- `big.ts` — [THIS PART, SOME HUNKS ONLY]"));
        assert!(part.contains("NO ABSENCE CLAIMS"));

        // PR из одного частично видимого файла — split всё равно включён
        let only = build_changed_files_section(&[json!("big.ts")], &json!({"paths": ["big.ts"], "partialPaths": ["big.ts"]}), &c).unwrap();
        assert!(only.contains("[THIS PART, SOME HUNKS ONLY]"));
        assert!(only.contains("NO ABSENCE CLAIMS"));

        // backtick в имени заменён пробелом — метка части не подделывается
        let spoofed = build_changed_files_section(
            &[json!("ok.ts"), json!("evil.ts` — [THIS PART]")],
            &json!({"paths": ["ok.ts"], "partialPaths": []}),
            &c,
        )
        .unwrap();
        let evil_line = spoofed.lines().find(|l| l.contains("evil.ts")).unwrap();
        assert_eq!(evil_line, "Reviewed in other parts (1) — [ANOTHER PART]: `evil.ts  — [THIS PART]`");
    }

    #[test]
    fn changed_files_section_e2e_specs_from_profile() {
        // дефолтный e2eSpec: (?:^|/)(?:e2e|tests?)/.+\.(?:spec|test)\.[cm]?[jt]sx?$
        let mut c = ctx();
        c.e2e_spec_source = Some(r"(?:^|/)(?:e2e|tests?)/.+\.(?:spec|test)\.[cm]?[jt]sx?$".to_string());
        let paths = vec![json!("src/a.ts"), json!("e2e/login.spec.ts"), json!("tests/unit.test.js")];
        let s = build_changed_files_section(&paths, &json!({}), &c).unwrap();
        assert!(s.contains("## E2E specs in this PR (Playwright)\n- `e2e/login.spec.ts`\n- `tests/unit.test.js`\n"));
        assert!(s.contains("Do not claim that e2e is missing — the list above is taken from the PR diff."));

        // нет e2eSpec — секции нет
        let s = build_changed_files_section(&paths, &json!({}), &ctx()).unwrap();
        assert!(!s.contains("E2E specs"));
    }

    #[test]
    fn changed_files_section_lookahead_degrades() {
        let mut c = ctx();
        c.e2e_spec_source = Some(r"e2e/.+(?=\.spec)".to_string());
        let r = build_changed_files_section(&[json!("e2e/a.spec.ts")], &json!({}), &c);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains(E2ESPEC_UNSUPPORTED));
    }

    #[test]
    fn verifier_messages_shape() {
        let c = ctx();
        let mut c = c;
        c.verifier_extra = "CUSTOM_VER_MARKER".to_string();
        let args = json!({
            "candidates": [
                {"severity": "critical", "file": "a.ts", "line": 3, "title": "IDOR", "body": "открытый доступ", "model": "m1", "confidence": 0.9},
                {"severity": "major", "file": "b.ts", "line": 0, "title": "t2", "body": "", "confidence": 1},
            ],
            "annotated": "@@ ```diff fence",
            "fileContents": "",
            "blastBlocks": [],
            "pr": {"number": 7, "title": "fix ``\nbug"},
            "changedFilesSection": "## map\n",
        });
        let msgs = build_verifier_messages(&args, &c).unwrap();
        assert_eq!(msgs[0]["role"], "system");
        let system = msgs[0]["content"].as_str().unwrap();
        assert!(system.contains("CUSTOM_VER_MARKER"));
        assert!(system.contains("You are a strict code review VERIFIER (second pass)."));
        let user = msgs[1]["content"].as_str().unwrap();
        assert!(user.contains("PR #7: «fix  bug»"));
        assert!(user.contains("0. [critical] a.ts:3 — **IDOR** (m1, conf=0.9)\n   открытый доступ"));
        assert!(user.contains("1. [major] b.ts — **t2** (?, conf=1)\n   "));
        assert!(user.contains("## Blast-radius: empty — do not invent external callers"));
        assert!(user.contains("## Context of changed files (windows around the candidate lines; «… (строки N–M пропущены)» marks text not shown)\n(нет)"));
        // fence-escape аннотированного диффа
        assert!(user.contains("@@ ``\u{200B}`diff fence"));
    }

    #[test]
    fn aggregator_messages_map_from_chunk() {
        let c = ctx();
        let args = json!({
            "annotated": "d",
            "blastBlocks": [],
            "pr": {"number": 1, "title": "t"},
            "fileContents": "",
            "priorFindings": [],
            "changedPaths": ["a.ts", "b.ts"],
            "chunk": {"paths": ["a.ts"], "partialPaths": []},
        });
        let msgs = build_messages(&args, &c).unwrap();
        let user = msgs[1]["content"].as_str().unwrap();
        assert!(user.contains("- `a.ts` — [THIS PART]"));
        assert!(user.contains("Reviewed in other parts (1) — [ANOTHER PART]: `b.ts`"));
        assert!(user.contains("HARD RULE — NO ABSENCE CLAIMS"));
        // blast и priors пусты — условные абзацы системного промпта не едут
        let system = msgs[0]["content"].as_str().unwrap();
        assert!(!system.contains("BLAST-RADIUS RULE"));
        assert!(!system.contains("CONVERGENCE BETWEEN RUNS"));
    }

    /// Условные абзацы возвращаются вместе со своей секцией и стыкуются без лишних пустых строк.
    #[test]
    fn aggregator_conditional_paragraphs_come_back() {
        let c = ctx();
        let args = json!({
            "annotated": "d",
            "blastBlocks": ["`foo` @ a.ts:1 [calls][EXTRACTED]"],
            "pr": {"number": 1, "title": "t"},
            "fileContents": "",
            "priorFindings": [{"severity": "major", "file": "a.ts", "line": 1, "title": "t"}],
        });
        let msgs = build_messages(&args, &c).unwrap();
        let system = msgs[0]["content"].as_str().unwrap();
        assert!(system.contains("catch those.\n\nBLAST-RADIUS RULE: when"));
        assert!(system.contains("imaginary callers.\n\nRespond STRICTLY"));
        assert!(system.contains("(better to skip it).\n\nCONVERGENCE BETWEEN RUNS:"));
        assert!(system.ends_with("not a rotation of wordings."));

        // без обеих секций стыки тоже без висячих пустых строк
        let bare = json!({
            "annotated": "d", "blastBlocks": [], "priorFindings": [],
            "pr": {"number": 1, "title": "t"}, "fileContents": "",
        });
        let msgs = build_messages(&bare, &c).unwrap();
        let system = msgs[0]["content"].as_str().unwrap();
        assert!(system.contains("catch those.\n\nRespond STRICTLY"));
        assert!(system.ends_with("(better to skip it)."));
        assert!(system.contains("Output AT MOST 12 findings"));
    }

    /// Абзац verifier'а про blast-radius едет только с непустым blast-radius.
    #[test]
    fn verifier_blast_paragraph_is_conditional() {
        let c = ctx();
        let mk = |blast: Value| {
            let args = json!({
                "candidates": [{"severity": "major", "file": "a.ts", "line": 1, "title": "t", "body": ""}],
                "annotated": "", "fileContents": "", "blastBlocks": blast,
                "pr": {"number": 1, "title": "t"},
            });
            build_verifier_messages(&args, &c).unwrap()[0]["content"].as_str().unwrap().to_string()
        };
        let empty = mk(json!([]));
        assert!(!empty.contains("When a candidate claims a breaking change"));
        assert!(empty.contains("minor or nit).\n\nDo NOT invent new findings."));
        let full = mk(json!(["`foo` @ a.ts:1 [calls][EXTRACTED]"]));
        assert!(full.contains("minor or nit).\n\nWhen a candidate claims a breaking change"));
        assert!(full.contains("the shown code.\n\nDo NOT invent new findings."));
    }

    #[test]
    fn reference_sections_filter_errors() {
        let refs = vec![
            json!({"role": "security", "model": "s", "text": "found x"}),
            json!({"role": "correctness", "model": "c", "error": "down"}),
            json!({"role": "architecture", "model": "a", "text": ""}),
        ];
        let s = build_specialist_reference_section(&refs);
        assert!(s.contains("### Specialist: security (s)\nfound x"));
        assert!(s.contains("### Specialists unavailable\n- correctness (c): down"));
        assert!(!s.contains("### Specialist: architecture"));
        assert!(s.contains("advisorClaims"));

        let moa = build_moa_reference_section(&refs);
        assert!(moa.contains("### Advisor: s\nfound x"));
        assert!(moa.contains("### Advisors unavailable\n- c: down"));
    }

    #[test]
    fn specialist_aggregator_replaces_last_user_only() {
        let base = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "u1"}),
            json!({"role": "user", "content": "u2"}),
        ];
        let out = build_specialist_aggregator_messages(&base, &[]);
        assert_eq!(out[0]["content"], "sys");
        assert_eq!(out[1]["content"], "u1");
        let last = out[2]["content"].as_str().unwrap();
        assert!(last.starts_with("\nIn specialist mode EACH finding MUST include advisorClaims"));
        assert!(last.ends_with("u2"));

        let out = build_aggregator_messages(&base, &[]);
        assert_eq!(out[2]["content"], "u2", "пустая секция — content не меняется");
    }
}
