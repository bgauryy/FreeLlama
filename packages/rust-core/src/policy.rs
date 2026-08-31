//! Generate a routing policy from a *quality* benchmark aggregate.
//!
//! Why this exists: `route_evidence` only returns `medium` confidence when a task has BOTH a
//! configured policy and benchmark data. Writing that policy by hand is the step nobody does, so
//! `minConfidence: "medium"` degrades to refusing everything — a safety gate that is never usable.
//!
//! Why it reads a harness aggregate and NOT `bench-all`: `bench-all` measures throughput
//! (`decode_tokens_per_second`), and the evidence tiers already account for that — benchmark data
//! alone yields `functional_throughput_screen`, deliberately still `low`. Generating a policy from
//! throughput would relabel speed as a quality contract and make `medium` reachable with no new
//! quality evidence. That is worse than the gate refusing everything, because it would pass while
//! meaning nothing. Harness aggregates carry `pass_at_1`, which is an actual correctness measure.
//!
//! The caller names the task the suite measures. This tool does not infer it: a suite of
//! code-research questions is not self-evidently `coding` rather than `completion`, and guessing
//! would be exactly the unearned inference this module exists to avoid.

use std::{collections::BTreeMap, fmt::Write as _, path::Path};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::platform::TaskKind;

/// Minimum trials before a run is treated as evidence rather than a smoke check. Matches the
/// benchmark harness's own rule: "Run three trials for publishable reliability. One trial is a
/// smoke result."
const PUBLISHABLE_TRIALS: u32 = 3;

#[derive(Debug, Deserialize)]
struct Aggregate {
    suite: Suite,
    models: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct Suite {
    id: String,
    benchmark_date: String,
    #[serde(default)]
    review: Review,
}

#[derive(Debug, Default, Deserialize)]
struct Review {
    #[serde(default)]
    fresh: bool,
    #[serde(default)]
    review_due_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    pass_at_1: Option<f64>,
    #[serde(default)]
    trial_budget: u32,
}

/// One model that cleared the bar, with the evidence that qualified it.
#[derive(Debug, Clone, PartialEq)]
pub struct Qualified {
    pub model: String,
    pub pass_at_1: f64,
    pub agent: Option<String>,
    pub trials: u32,
}

/// The harness slugifies a model tag for run ids and result directories by replacing `:` with `-`
/// (`qwen3.8:27b-mlx` -> `qwen3.8-27b-mlx`). That is lossy — `-` cannot be mapped back to `:`
/// unambiguously — so qualification matches FORWARD from known installed tags instead of trying to
/// reverse the slug.
#[must_use]
pub fn slug(model: &str) -> String {
    model.replace(':', "-")
}

/// Select the models in `aggregate` that cleared `min_pass` for `task`, ranked best first.
///
/// # Errors
///
/// Returns an error if the aggregate cannot be read or parsed, if its review window has expired,
/// or if it is a smoke run (fewer than three trials) and `allow_smoke` is false.
pub fn qualify_from_aggregate(
    aggregate_path: &Path,
    installed: &[String],
    min_pass: f64,
    allow_smoke: bool,
) -> Result<(Vec<Qualified>, String)> {
    let text = std::fs::read_to_string(aggregate_path)
        .with_context(|| format!("read aggregate {}", aggregate_path.display()))?;
    let aggregate: Aggregate = serde_json::from_str(&text)
        .with_context(|| format!("parse aggregate {}", aggregate_path.display()))?;

    if !aggregate.suite.review.fresh {
        bail!(
            "aggregate {} is past its review window (review_due_at {:?}); re-review the suite \
             before generating a quality contract from it",
            aggregate.suite.id,
            aggregate.suite.review.review_due_at
        );
    }

    let mut qualified: Vec<Qualified> = Vec::new();
    for entry in &aggregate.models {
        let Some(pass) = entry.pass_at_1 else {
            continue;
        };
        if pass < min_pass {
            continue;
        }
        if entry.trial_budget < PUBLISHABLE_TRIALS && !allow_smoke {
            bail!(
                "`{}` cleared {min_pass:.2} with pass_at_1 {pass:.3}, but the run used {} trial(s). \
                 The harness treats fewer than {PUBLISHABLE_TRIALS} as a smoke result, not evidence. \
                 Re-run with more trials, or pass --allow-smoke to write a policy marked smoke-only.",
                entry.id,
                entry.trial_budget
            );
        }
        // Match forward from installed tags — the aggregate id is a lossy slug.
        let Some(model) = installed
            .iter()
            .find(|m| entry.id.starts_with(&format!("{}-", slug(m))) || entry.id == slug(m))
        else {
            continue;
        };
        qualified.push(Qualified {
            model: model.clone(),
            pass_at_1: pass,
            agent: entry.agent.clone(),
            trials: entry.trial_budget,
        });
    }

    // Best first: the router prefers the leading candidate for objective=quality.
    qualified.sort_by(|a, b| {
        b.pass_at_1
            .partial_cmp(&a.pass_at_1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    qualified.dedup_by(|a, b| a.model == b.model);

    ensure!(
        !qualified.is_empty(),
        "no model in {} cleared pass_at_1 >= {min_pass:.2} while also being installed here. \
         Lower --min-pass, run the suite against an installed model, or install a qualifying one.",
        aggregate.suite.id
    );
    Ok((qualified, aggregate.suite.benchmark_date.clone()))
}

/// Render a policy file. Provenance is written into the file itself so a stale contract is
/// visible at a glance rather than requiring an archaeology session.
#[must_use]
pub fn render_policy(
    entries: &BTreeMap<TaskKind, Vec<Qualified>>,
    source: &str,
    benchmark_date: &str,
    min_pass: f64,
    smoke: bool,
) -> String {
    let mut out = String::from("schema_version = 1\n\n");
    out.push_str("# GENERATED by `freellama policy-from-eval` — do not hand-edit; regenerate.\n");
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
        let task_key = serde_json::to_string(task)
            .unwrap_or_default()
            .trim_matches('"')
            .to_owned();
        let _ = writeln!(out, "[policies.{task_key}]");
        let list = models
            .iter()
            .map(|q| format!("\"{}\"", q.model))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "qualified_models = [{list}]");
        for q in models {
            let _ = writeln!(
                out,
                "# {} — pass_at_1 {:.3} over {} trial(s){}",
                q.model,
                q.pass_at_1,
                q.trials,
                q.agent
                    .as_ref()
                    .map(|a| format!(", agent {a}"))
                    .unwrap_or_default()
            );
        }
        out.push('\n');
    }
    out
}

/// Convenience wrapper matching the CLI call site.
///
/// # Errors
///
/// See [`qualify_from_aggregate`].
pub fn qualify_from_eval_path(
    aggregate: &Path,
    installed: &[String],
    min_pass: f64,
    allow_smoke: bool,
) -> Result<(Vec<Qualified>, String)> {
    qualify_from_aggregate(aggregate, installed, min_pass, allow_smoke)
}
