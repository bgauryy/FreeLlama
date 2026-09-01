//! Side-effect-free model recommendation and installation planning.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{model_bench::Capability, platform::TaskKind};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecommendationCatalog {
    pub schema_version: u32,
    pub reviewed_at: Option<String>,
    pub review_due_at: Option<String>,
    #[serde(default)]
    pub models: Vec<RecommendedModel>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecommendedModel {
    pub name: String,
    pub summary: String,
    pub tasks: BTreeSet<TaskKind>,
    pub capabilities: BTreeSet<Capability>,
    pub max_context_tokens: u64,
    pub estimated_download_bytes: u64,
    pub minimum_memory_bytes: u64,
    #[serde(default = "default_priority")]
    pub priority: u32,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FitStatus {
    Fits,
    DoesNotFit,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallPlan {
    pub model: String,
    pub summary: String,
    pub pull_command: Vec<String>,
    pub tasks: BTreeSet<TaskKind>,
    pub capabilities: BTreeSet<Capability>,
    pub max_context_tokens: u64,
    pub estimated_download_bytes: u64,
    pub minimum_memory_bytes: u64,
    pub memory_fit: FitStatus,
    pub disk_fit: FitStatus,
    pub requires_confirmation: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct InstallationPlanRequest<'a> {
    pub task: TaskKind,
    pub explicit_model: Option<&'a str>,
    pub required_capabilities: &'a BTreeSet<Capability>,
    pub requested_context: u64,
    pub installed_models: &'a BTreeSet<String>,
    pub memory_bytes: Option<u64>,
    pub available_disk_bytes: Option<u64>,
}

fn default_priority() -> u32 {
    100
}

impl RecommendationCatalog {
    /// Load and validate a recommendation catalog.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable TOML or an invalid catalog contract.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read recommendation catalog {}", path.display()))?;
        let catalog: Self = toml::from_str(&text)
            .with_context(|| format!("parse recommendation catalog {}", path.display()))?;
        catalog.validate()?;
        Ok(catalog)
    }

    /// Validate schema, dates, model identifiers, and resource estimates.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot produce safe installation plans.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "unsupported recommendation schema version"
        );
        match (&self.reviewed_at, &self.review_due_at) {
            (Some(reviewed), Some(due)) => {
                ensure!(valid_date(reviewed), "invalid reviewed_at date");
                ensure!(valid_date(due), "invalid review_due_at date");
                ensure!(due >= reviewed, "review_due_at precedes reviewed_at");
            }
            (None, None) => ensure!(
                self.models.is_empty(),
                "recommendation models require reviewed_at and review_due_at"
            ),
            _ => ensure!(
                false,
                "reviewed_at and review_due_at must be provided together"
            ),
        }
        let mut names = BTreeSet::new();
        for model in &self.models {
            ensure!(!model.name.is_empty(), "recommendation model name is empty");
            ensure!(
                model
                    .name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric()
                        || "._:/-".contains(character)),
                "unsafe recommendation model name: {}",
                model.name
            );
            ensure!(
                names.insert(&model.name),
                "duplicate recommendation model: {}",
                model.name
            );
            ensure!(
                !model.summary.trim().is_empty(),
                "recommendation summary is empty"
            );
            ensure!(!model.tasks.is_empty(), "recommendation tasks are empty");
            ensure!(
                !model.capabilities.is_empty(),
                "recommendation capabilities are empty"
            );
            ensure!(
                model.max_context_tokens >= 512,
                "recommended context is below 512"
            );
            ensure!(
                model.estimated_download_bytes > 0,
                "download estimate must be positive"
            );
            ensure!(
                model.minimum_memory_bytes > 0,
                "memory estimate must be positive"
            );
        }
        Ok(())
    }
}

/// Load a configured catalog or return an empty catalog.
///
/// # Errors
///
/// Returns an error when the configured catalog is invalid.
pub fn load_catalog(path: Option<&PathBuf>) -> Result<RecommendationCatalog> {
    path.map_or_else(
        || {
            Ok(RecommendationCatalog {
                schema_version: 1,
                ..RecommendationCatalog::default()
            })
        },
        RecommendationCatalog::from_path,
    )
}

/// Build ranked, side-effect-free installation plans.
#[must_use]
pub fn installation_plans(
    catalog: &RecommendationCatalog,
    request: &InstallationPlanRequest<'_>,
) -> Vec<InstallPlan> {
    let mut plans = catalog
        .models
        .iter()
        .filter(|model| !request.installed_models.contains(&model.name))
        .filter(|model| request.explicit_model.is_none_or(|name| model.name == name))
        .filter(|model| model.tasks.contains(&request.task))
        .filter(|model| request.required_capabilities.is_subset(&model.capabilities))
        .filter(|model| request.requested_context <= model.max_context_tokens)
        .map(|model| {
            let memory_fit = fit(model.minimum_memory_bytes, request.memory_bytes);
            let disk_fit = fit(model.estimated_download_bytes, request.available_disk_bytes);
            let mut warnings = Vec::new();
            warning(
                &mut warnings,
                memory_fit,
                "host memory (accelerator-memory fit remains Ollama-owned)",
            );
            warning(&mut warnings, disk_fit, "available disk");
            InstallPlan {
                model: model.name.clone(),
                summary: model.summary.clone(),
                pull_command: vec!["ollama".to_owned(), "pull".to_owned(), model.name.clone()],
                tasks: model.tasks.clone(),
                capabilities: model.capabilities.clone(),
                max_context_tokens: model.max_context_tokens,
                estimated_download_bytes: model.estimated_download_bytes,
                minimum_memory_bytes: model.minimum_memory_bytes,
                memory_fit,
                disk_fit,
                requires_confirmation: true,
                warnings,
            }
        })
        .collect::<Vec<_>>();
    plans.sort_by_key(|plan| {
        (
            fit_rank(plan.memory_fit) + fit_rank(plan.disk_fit),
            catalog
                .models
                .iter()
                .find(|model| model.name == plan.model)
                .map_or(u32::MAX, |model| model.priority),
            plan.minimum_memory_bytes,
            plan.model.clone(),
        )
    });
    plans
}

fn fit(required: u64, available: Option<u64>) -> FitStatus {
    available.map_or(FitStatus::Unknown, |available| {
        if required <= available {
            FitStatus::Fits
        } else {
            FitStatus::DoesNotFit
        }
    })
}

fn fit_rank(fit: FitStatus) -> u8 {
    match fit {
        FitStatus::Fits => 0,
        FitStatus::Unknown => 1,
        FitStatus::DoesNotFit => 2,
    }
}

fn warning(warnings: &mut Vec<String>, fit: FitStatus, resource: &str) {
    match fit {
        FitStatus::Fits => {}
        FitStatus::Unknown => {
            warnings.push(format!("{resource} fit is unknown; verify before pulling"));
        }
        FitStatus::DoesNotFit => {
            warnings.push(format!("declared {resource} requirement does not fit"));
        }
    }
}

fn valid_date(value: &str) -> bool {
    if value.len() != 10 || value.as_bytes()[4] != b'-' || value.as_bytes()[7] != b'-' {
        return false;
    }
    let Ok(year) = value[0..4].parse::<u32>() else {
        return false;
    };
    let Ok(month) = value[5..7].parse::<u32>() else {
        return false;
    };
    let Ok(day) = value[8..10].parse::<u32>() else {
        return false;
    };
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return false,
    };
    year > 0 && (1..=days).contains(&day)
}
