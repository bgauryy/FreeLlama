//! Generate a `freellama serve` routing policy from a harness quality aggregate.
//!
//! `route_evidence` returns `medium` confidence only when a task has both a configured policy and
//! benchmark data. This module renders that policy from a harness `aggregate.json`, which carries
//! `pass_at_1`, a correctness measure. It deliberately does not read `bench-all` output:
//! throughput is not quality evidence.
//!
//! The caller names the task the suite measures; nothing here infers it.
//!
//! Aggregate model ids are lossy slugs (`:` replaced with `-`), so qualification matches forward
//! from the tags installed on the Ollama endpoint (`/api/tags`). Pure: the caller supplies the
//! installed tags and writes the output (`freellama policy from-eval`).

use std::{collections::BTreeMap, fmt::Write as _};

use anyhow::{Result, bail};
use clap::ValueEnum as _;
use serde_json::Value;

use super::TaskKind;

/// Minimum trials before a run is evidence rather than a smoke check (the harness's own rule).
pub const PUBLISHABLE_TRIALS: i64 = 3;

/// One installed model that cleared the threshold, with the run that qualified it.
#[derive(Debug, Clone, PartialEq)]
pub struct Qualified {
    pub model: String,
    pub pass_at_1: f64,
    pub agent: Option<String>,
    pub trials: i64,
}

/// The harness run-id slug for a model tag (`qwen3.8:27b-mlx` -> `qwen3.8-27b-mlx`).
#[must_use]
pub fn slug(model: &str) -> String {
    model.replace(':', "-")
}

/// Parse a router task kind, accepting `-` or `_` (`code-repair`, `code_repair`).
///
/// # Errors
/// Names every accepted kind when `value` is not one of them.
pub fn parse_task_kind(value: &str) -> Result<TaskKind, String> {
    let task = value.replace('-', "_");
    TaskKind::value_variants()
        .iter()
        .copied()
        .find(|kind| kind.as_str() == task)
        .ok_or_else(|| {
            let names: Vec<&str> = TaskKind::value_variants()
                .iter()
                .map(|kind| kind.as_str())
                .collect();
            format!(
                "unknown task {}; choose one of {}",
                python_repr_str(value),
                names.join(", ")
            )
        })
}

fn require_str<'a>(value: Option<&'a Value>, field: &str) -> Result<&'a str> {
    match value {
        Some(Value::String(text)) => Ok(text),
        _ => bail!("aggregate field {field} must be a string"),
    }
}

/// Return the installed models that cleared `min_pass`, best first, and the benchmark date.
///
/// # Errors
/// Refuses a malformed or stale aggregate, a smoke run (fewer than [`PUBLISHABLE_TRIALS`]) unless
/// `allow_smoke`, an aggregate id that ambiguously matches several installed tags, and a result
/// with no installed qualifying model.
pub fn qualify(
    aggregate: &Value,
    installed: &[String],
    min_pass: f64,
    allow_smoke: bool,
) -> Result<(Vec<Qualified>, String)> {
    let (Some(suite @ Value::Object(_)), Some(Value::Array(models))) =
        (aggregate.get("suite"), aggregate.get("models"))
    else {
        bail!("aggregate must contain a `suite` object and a `models` list");
    };
    let suite_id = require_str(suite.get("id"), "suite.id")?;
    let benchmark_date = require_str(suite.get("benchmark_date"), "suite.benchmark_date")?;
    let review = suite.get("review").filter(|review| truthy(review));
    if !review
        .and_then(|review| review.get("fresh"))
        .is_some_and(truthy)
    {
        bail!(
            "aggregate {suite_id} is past its review window (review_due_at {}); re-review the \
             suite before generating a quality contract from it",
            python_repr(review.and_then(|review| review.get("review_due_at")))
        );
    }

    let mut qualified = Vec::new();
    for entry in models {
        let entry_id = require_str(entry.get("id"), "models[].id")?;
        let passed = match entry.get("pass_at_1") {
            None | Some(Value::Null) => continue,
            Some(value) => number(value, "models[].pass_at_1")?,
        };
        if passed < min_pass {
            continue;
        }
        let trials = trial_budget(entry.get("trial_budget"))?;
        if trials < PUBLISHABLE_TRIALS && !allow_smoke {
            bail!(
                "`{entry_id}` cleared {min_pass:.2} with pass_at_1 {passed:.3}, but the run used \
                 {trials} trial(s). The harness treats fewer than {PUBLISHABLE_TRIALS} as a smoke \
                 result, not evidence. Re-run with more trials, or pass --allow-smoke to write a \
                 policy marked smoke-only."
            );
        }
        // Match forward from installed tags; the aggregate id is a lossy slug.
        let matching: Vec<&String> = installed
            .iter()
            .filter(|model| {
                let slug = slug(model);
                entry_id == slug
                    || entry_id
                        .strip_prefix(slug.as_str())
                        .is_some_and(|rest| rest.starts_with('-'))
            })
            .collect();
        let Some(longest) = matching.iter().map(|model| slug(model).len()).max() else {
            continue;
        };
        let finalists: Vec<&str> = matching
            .iter()
            .filter(|model| slug(model).len() == longest)
            .map(|model| model.as_str())
            .collect();
        if finalists.len() != 1 {
            bail!(
                "aggregate id `{entry_id}` ambiguously matches installed models {}; their lossy \
                 slugs are indistinguishable, so refusing rather than crediting evidence to the \
                 wrong model",
                finalists.join(", ")
            );
        }
        qualified.push(Qualified {
            model: finalists[0].to_owned(),
            pass_at_1: passed,
            agent: entry
                .get("agent")
                .filter(|agent| truthy(agent))
                .and_then(python_str),
            trials,
        });
    }

    // Best first (stable): the router prefers the leading candidate for objective=quality.
    qualified.sort_by(|a, b| b.pass_at_1.total_cmp(&a.pass_at_1));
    let mut unique: Vec<Qualified> = Vec::new();
    for candidate in qualified {
        if !unique.iter().any(|kept| kept.model == candidate.model) {
            unique.push(candidate);
        }
    }
    if unique.is_empty() {
        bail!(
            "no model in {suite_id} cleared pass_at_1 >= {min_pass:.2} while also being installed \
             here. Lower --min-pass, run the suite against an installed model, or install a \
             qualifying one."
        );
    }
    Ok((unique, benchmark_date.to_owned()))
}

/// Whether any qualified run used fewer than [`PUBLISHABLE_TRIALS`] trials.
#[must_use]
pub fn is_smoke(qualified: &[Qualified]) -> bool {
    qualified.iter().any(|q| q.trials < PUBLISHABLE_TRIALS)
}

/// Render a policy TOML file with its provenance written into comments. Tasks appear in router
/// task order.
#[must_use]
pub fn render_policy(
    entries: &BTreeMap<TaskKind, Vec<Qualified>>,
    source: &str,
    benchmark_date: &str,
    min_pass: f64,
    smoke: bool,
) -> String {
    let mut out = String::from("schema_version = 1\n");
    if smoke {
        out.push_str("smoke = true\n");
    }
    out.push('\n');
    out.push_str("# GENERATED by `freellama policy from-eval` — do not hand-edit; regenerate.\n");
    // Writing to a String cannot fail.
    let _ = writeln!(out, "# source        = {source}");
    let _ = writeln!(out, "# benchmarked   = {benchmark_date}");
    let _ = writeln!(out, "# threshold     = pass_at_1 >= {min_pass:.2}");
    if smoke {
        out.push_str(
            "#\n# SMOKE-ONLY: generated from a run with fewer than three trials. The harness treats\n\
             # that as a smoke result, not publishable evidence. Routes qualified by this file will\n\
             # report medium confidence on thin evidence — re-run with more trials before relying\n\
             # on it for anything quality-sensitive.\n",
        );
    }
    out.push('\n');
    for (task, models) in entries {
        let _ = writeln!(out, "[policies.{}]", task.as_str());
        let quoted: Vec<String> = models.iter().map(|q| json_ascii(&q.model)).collect();
        let _ = writeln!(out, "qualified_models = [{}]", quoted.join(", "));
        for q in models {
            let agent = q
                .agent
                .as_deref()
                .filter(|agent| !agent.is_empty())
                .map(|agent| format!(", agent {agent}"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "# {} — pass_at_1 {:.3} over {} trial(s){agent}",
                q.model, q.pass_at_1, q.trials
            );
        }
        out.push('\n');
    }
    out
}

/// Python truthiness of a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn number(value: &Value, field: &str) -> Result<f64> {
    match value {
        Value::Number(number) => number
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("aggregate field {field} must be a number")),
        Value::Bool(flag) => Ok(f64::from(u8::from(*flag))),
        _ => bail!("aggregate field {field} must be a number"),
    }
}

/// `int(entry.get("trial_budget") or 0)`: absent, null, or falsy counts as zero; floats truncate.
fn trial_budget(value: Option<&Value>) -> Result<i64> {
    let Some(value) = value.filter(|value| truthy(value)) else {
        return Ok(0);
    };
    let invalid = || anyhow::anyhow!("aggregate field models[].trial_budget must be an integer");
    match value {
        Value::Bool(flag) => Ok(i64::from(*flag)),
        Value::Number(number) => number
            .as_i64()
            .or_else(|| {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "Python int() truncates a float trial budget toward zero"
                )]
                number.as_f64().map(|n| n.trunc() as i64)
            })
            .ok_or_else(invalid),
        Value::String(text) => text.trim().parse().map_err(|_| invalid()),
        _ => Err(invalid()),
    }
}

/// `str(value)` for a scalar agent label; `None` for null.
fn python_str(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Bool(true) => Some("True".to_owned()),
        Value::Bool(false) => Some("False".to_owned()),
        other => Some(other.to_string()),
    }
}

/// `repr(value)` for the scalar values an aggregate field holds.
fn python_repr(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_owned(),
        Some(Value::String(text)) => python_repr_str(text),
        Some(Value::Bool(true)) => "True".to_owned(),
        Some(Value::Bool(false)) => "False".to_owned(),
        Some(other) => other.to_string(),
    }
}

/// `repr(text)`: single quotes unless the text holds a single quote and no double quote.
fn python_repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// A double-quoted string with every byte outside printable ASCII escaped, like Python's
/// `json.dumps` default. Characters beyond the Basic Multilingual Plane use TOML's `\UXXXXXXXX`
/// form, because TOML rejects the surrogate pairs JSON would emit.
fn json_ascii(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(ch),
            c if u32::from(c) <= 0xFFFF => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => {
                let _ = write!(out, "\\U{:08x}", u32::from(c));
            }
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::platform::discovery::load_policies;

    const INSTALLED: &str = "qwen3.8:27b-mlx";

    fn installed(tags: &[&str]) -> Vec<String> {
        tags.iter().map(|tag| (*tag).to_owned()).collect()
    }

    fn aggregate(trials: i64, passed: f64, fresh: bool) -> Value {
        json!({
            "suite": {"id": "s", "benchmark_date": "2026-08-27", "review": {"fresh": fresh, "review_due_at": "2026-09-27"}},
            "models": [{"id": "qwen3.8-27b-mlx-bash", "agent": "bash", "pass_at_1": passed, "trial_budget": trials}],
        })
    }

    fn error_of(result: Result<(Vec<Qualified>, String)>) -> String {
        format!("{:#}", result.unwrap_err())
    }

    fn models(qualified: &[Qualified]) -> Vec<&str> {
        qualified.iter().map(|q| q.model.as_str()).collect()
    }

    fn round_trip(rendered: &str) -> Result<super::super::discovery::Policies> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("platform.toml");
        std::fs::write(&path, rendered).unwrap();
        load_policies(Some(&path))
    }

    #[test]
    fn slug_matches_the_harness_convention() {
        assert_eq!(slug("qwen3.8:27b-mlx"), "qwen3.8-27b-mlx");
    }

    #[test]
    fn refuses_a_smoke_run_as_evidence() {
        let error = error_of(qualify(
            &aggregate(1, 0.9, true),
            &installed(&[INSTALLED]),
            0.8,
            false,
        ));
        assert!(error.contains("smoke result"), "{error}");
        assert!(
            error.starts_with("`qwen3.8-27b-mlx-bash` cleared 0.80 with pass_at_1 0.900, but the run used 1 trial(s)."),
            "{error}"
        );
    }

    #[test]
    fn accepts_a_smoke_run_only_when_asked() {
        let (qualified, date) = qualify(
            &aggregate(1, 0.9, true),
            &installed(&[INSTALLED]),
            0.8,
            true,
        )
        .unwrap();
        assert_eq!(models(&qualified), [INSTALLED]);
        assert_eq!(date, "2026-08-27");
        assert!(is_smoke(&qualified));
    }

    #[test]
    fn refuses_an_aggregate_past_its_review_window() {
        let error = error_of(qualify(
            &aggregate(3, 0.9, false),
            &installed(&[INSTALLED]),
            0.8,
            false,
        ));
        assert!(error.contains("review window"), "{error}");
        assert!(error.contains("(review_due_at '2026-09-27')"), "{error}");
        let mut missing = aggregate(3, 0.9, true);
        missing["suite"].as_object_mut().unwrap().remove("review");
        let error = error_of(qualify(&missing, &installed(&[INSTALLED]), 0.8, false));
        assert!(error.contains("(review_due_at None)"), "{error}");
    }

    #[test]
    fn refuses_when_nothing_clears_the_bar() {
        let error = error_of(qualify(
            &aggregate(3, 0.5, true),
            &installed(&[INSTALLED]),
            0.8,
            false,
        ));
        assert!(error.contains("cleared"), "{error}");
    }

    #[test]
    fn ignores_models_that_are_not_installed() {
        let error = error_of(qualify(
            &aggregate(3, 0.9, true),
            &installed(&["some-other:7b"]),
            0.8,
            false,
        ));
        assert!(error.contains("installed here"), "{error}");
    }

    #[test]
    fn prefers_the_longest_installed_slug_and_refuses_ambiguity() {
        let (qualified, _) = qualify(
            &aggregate(3, 0.9, true),
            &installed(&["qwen3.8:27b", "qwen3.8:27b-mlx"]),
            0.8,
            false,
        )
        .unwrap();
        assert_eq!(models(&qualified), ["qwen3.8:27b-mlx"]);
        let error = error_of(qualify(
            &aggregate(3, 0.9, true),
            &installed(&["qwen3.8:27b-mlx", "qwen3.8-27b:mlx"]),
            0.8,
            false,
        ));
        assert!(error.contains("ambiguously"), "{error}");
        assert!(
            error.contains("qwen3.8:27b-mlx, qwen3.8-27b:mlx"),
            "{error}"
        );
    }

    #[test]
    fn a_slug_prefix_must_end_at_a_dash() {
        let error = error_of(qualify(
            &aggregate(3, 0.9, true),
            &installed(&["qwen3.8:27b-ml"]),
            0.8,
            false,
        ));
        assert!(error.contains("installed here"), "{error}");
    }

    #[test]
    fn ranks_best_first_and_deduplicates() {
        let mut data = aggregate(3, 0.85, true);
        let list = data["models"].as_array_mut().unwrap();
        list.push(json!({"id": "qwen3.8-27b-mlx-octocode", "agent": "octocode", "pass_at_1": 0.95, "trial_budget": 3}));
        list.push(json!({"id": "gemma4-e4b", "pass_at_1": 0.9, "trial_budget": 3}));
        let (qualified, _) =
            qualify(&data, &installed(&[INSTALLED, "gemma4:e4b"]), 0.8, false).unwrap();
        let pairs: Vec<(&str, Option<&str>)> = qualified
            .iter()
            .map(|q| (q.model.as_str(), q.agent.as_deref()))
            .collect();
        assert_eq!(
            pairs,
            [("qwen3.8:27b-mlx", Some("octocode")), ("gemma4:e4b", None)]
        );
    }

    #[test]
    fn a_model_qualified_by_two_runs_appears_once_even_when_scores_are_not_adjacent() {
        let mut data = aggregate(3, 0.9, true);
        data["models"] = json!([
            {"id": "alpha-1b-agent-x", "pass_at_1": 0.95, "trial_budget": 3},
            {"id": "beta-1b", "pass_at_1": 0.90, "trial_budget": 3},
            {"id": "alpha-1b-agent-y", "pass_at_1": 0.85, "trial_budget": 3},
        ]);
        let (qualified, _) =
            qualify(&data, &installed(&["alpha:1b", "beta:1b"]), 0.8, false).unwrap();
        assert_eq!(models(&qualified), ["alpha:1b", "beta:1b"]);
        assert!(
            (qualified[0].pass_at_1 - 0.95).abs() < f64::EPSILON,
            "each model keeps its best score"
        );
    }

    #[test]
    fn equal_scores_keep_aggregate_order() {
        let mut data = aggregate(3, 0.9, true);
        data["models"] = json!([
            {"id": "beta-1b", "pass_at_1": 0.9, "trial_budget": 3},
            {"id": "alpha-1b", "pass_at_1": 0.9, "trial_budget": 3},
        ]);
        let (qualified, _) =
            qualify(&data, &installed(&["alpha:1b", "beta:1b"]), 0.8, false).unwrap();
        assert_eq!(models(&qualified), ["beta:1b", "alpha:1b"]);
    }

    #[test]
    fn malformed_aggregates_name_the_field() {
        let error = error_of(qualify(&json!({"suite": {}}), &[], 0.8, false));
        assert!(
            error.contains("`suite` object and a `models` list"),
            "{error}"
        );
        let error = error_of(qualify(
            &json!({"suite": {"id": 1}, "models": []}),
            &[],
            0.8,
            false,
        ));
        assert_eq!(error, "aggregate field suite.id must be a string");
    }

    #[test]
    fn task_kinds_accept_dashes_or_underscores() {
        assert_eq!(parse_task_kind("code-repair"), Ok(TaskKind::CodeRepair));
        assert_eq!(parse_task_kind("long_context"), Ok(TaskKind::LongContext));
        assert_eq!(
            parse_task_kind("poetry").unwrap_err(),
            "unknown task 'poetry'; choose one of completion, coding, code_repair, tools, browser, vision, embedding, long_context"
        );
    }

    #[test]
    fn render_policy_writes_a_machine_readable_smoke_flag() {
        let (qualified, _) = qualify(
            &aggregate(1, 0.9, true),
            &installed(&[INSTALLED]),
            0.8,
            true,
        )
        .unwrap();
        let rendered = render_policy(
            &BTreeMap::from([(TaskKind::Coding, qualified)]),
            "agg.json",
            "2026-08-27",
            0.8,
            true,
        );
        let parsed: toml::Table = toml::from_str(&rendered).unwrap();
        assert_eq!(
            parsed["smoke"].as_bool(),
            Some(true),
            "runtime loader keys on this field, not the comment"
        );
        assert_eq!(parsed["schema_version"].as_integer(), Some(1));
        assert_eq!(
            parsed["policies"]["coding"]["qualified_models"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>(),
            [INSTALLED]
        );
        assert!(
            rendered.contains("# qwen3.8:27b-mlx — pass_at_1 0.900 over 1 trial(s), agent bash")
        );
        let error = format!("{:#}", round_trip(&rendered).unwrap_err());
        assert!(
            error.contains("smoke run"),
            "serve refuses a smoke policy: {error}"
        );
    }

    #[test]
    fn publishable_run_has_no_smoke_flag() {
        let (qualified, _) = qualify(
            &aggregate(3, 0.9, true),
            &installed(&[INSTALLED]),
            0.8,
            false,
        )
        .unwrap();
        let rendered = render_policy(
            &BTreeMap::from([(TaskKind::CodeRepair, qualified)]),
            "agg.json",
            "2026-08-27",
            0.8,
            false,
        );
        let parsed: toml::Table = toml::from_str(&rendered).unwrap();
        assert!(!parsed.contains_key("smoke"));
        assert!(
            parsed["policies"]
                .as_table()
                .unwrap()
                .contains_key("code_repair")
        );
    }

    #[test]
    fn rendered_policy_has_the_exact_format() {
        let (qualified, _) = qualify(
            &aggregate(3, 0.9, true),
            &installed(&[INSTALLED]),
            0.8,
            false,
        )
        .unwrap();
        let rendered = render_policy(
            &BTreeMap::from([(TaskKind::Coding, qualified)]),
            "results/aggregate.json",
            "2026-08-27",
            0.8,
            false,
        );
        assert_eq!(
            rendered,
            "schema_version = 1\n\n\
             # GENERATED by `freellama policy from-eval` — do not hand-edit; regenerate.\n\
             # source        = results/aggregate.json\n\
             # benchmarked   = 2026-08-27\n\
             # threshold     = pass_at_1 >= 0.80\n\n\
             [policies.coding]\n\
             qualified_models = [\"qwen3.8:27b-mlx\"]\n\
             # qwen3.8:27b-mlx — pass_at_1 0.900 over 3 trial(s), agent bash\n\n"
        );
    }

    #[test]
    fn generated_policy_loads_through_the_serve_policy_loader() {
        let mut data = aggregate(3, 0.85, true);
        data["models"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "gemma4-e4b", "pass_at_1": 0.9, "trial_budget": 3}));
        let (qualified, benchmark_date) =
            qualify(&data, &installed(&[INSTALLED, "gemma4:e4b"]), 0.8, false).unwrap();
        let smoke = is_smoke(&qualified);
        let rendered = render_policy(
            &BTreeMap::from([(TaskKind::LongContext, qualified)]),
            "agg.json",
            &benchmark_date,
            0.8,
            smoke,
        );
        let policies = round_trip(&rendered).unwrap();
        assert_eq!(
            policies.qualified,
            BTreeMap::from([(
                TaskKind::LongContext,
                vec!["gemma4:e4b".to_owned(), INSTALLED.to_owned()]
            )])
        );
    }

    #[test]
    fn model_names_are_escaped_like_json_dumps() {
        assert_eq!(json_ascii("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(json_ascii("é\u{7f}"), r#""\u00e9\u007f""#);
        let parsed: toml::Table = toml::from_str(&format!("x = {}", json_ascii("é😀"))).unwrap();
        assert_eq!(parsed["x"].as_str(), Some("é😀"));
    }
}
