use std::collections::BTreeSet;

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::get,
};
use freellama::{
    model_bench::Capability,
    platform::{PlatformConfig, TaskKind, app},
    recommend::{FitStatus, InstallationPlanRequest, RecommendationCatalog, installation_plans},
};
use serde_json::{Value, json};
use tempfile::tempdir;
use tower::ServiceExt;

fn catalog_text(model_name: &str) -> String {
    format!(
        r#"schema_version = 1
reviewed_at = "2026-08-24"
review_due_at = "2026-09-23"

[[models]]
name = "{model_name}"
summary = "Reviewed completion model."
tasks = ["completion", "coding"]
capabilities = ["completion"]
max_context_tokens = 8192
estimated_download_bytes = 1000
minimum_memory_bytes = 2000
priority = 10
"#
    )
}

#[test]
fn catalog_builds_safe_ranked_install_plans() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("recommendations.toml");
    std::fs::write(&path, catalog_text("reviewed:latest")).unwrap();
    let catalog = RecommendationCatalog::from_path(&path).unwrap();
    let required_capabilities = [Capability::Completion].into_iter().collect();
    let installed_models = BTreeSet::new();
    let plans = installation_plans(
        &catalog,
        &InstallationPlanRequest {
            task: TaskKind::Completion,
            explicit_model: None,
            required_capabilities: &required_capabilities,
            requested_context: 4096,
            installed_models: &installed_models,
            unified_memory_bytes: Some(4000),
            available_disk_bytes: Some(4000),
        },
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].pull_command, ["ollama", "pull", "reviewed:latest"]);
    assert_eq!(plans[0].memory_fit, FitStatus::Fits);
    assert_eq!(plans[0].disk_fit, FitStatus::Fits);
    assert!(plans[0].requires_confirmation);
}

#[test]
fn catalog_rejects_shell_metacharacters_in_model_names() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("recommendations.toml");
    std::fs::write(&path, catalog_text("unsafe;command")).unwrap();
    assert!(RecommendationCatalog::from_path(&path).is_err());
}

#[test]
fn catalog_requires_a_real_review_window() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("recommendations.toml");
    let text = catalog_text("reviewed:latest").replace("2026-09-23", "2026-02-30");
    std::fs::write(&path, text).unwrap();
    assert!(RecommendationCatalog::from_path(&path).is_err());
}

#[tokio::test]
async fn recommendation_endpoint_returns_a_plan_without_side_effects() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("recommendations.toml");
    std::fs::write(&path, catalog_text("reviewed:latest")).unwrap();
    let mock = Router::new()
        .route("/api/tags", get(|| async { Json(json!({"models": []})) }))
        .route("/api/ps", get(|| async { Json(json!({"models": []})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let config = PlatformConfig::new("127.0.0.1:11435", upstream, None, None, "qwen2.5:0.5b")
        .with_recommendation_catalog(path);
    let response = app(&config)
        .unwrap()
        .oneshot(
            Request::post("/_freellama/v1/recommendations")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"task":"completion","objective":"fastest","context_tokens":4096}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["side_effects_performed"], false);
    assert!(body["installed_route"].is_null());
    assert_eq!(body["install_plans"][0]["model"], "reviewed:latest");
    assert_eq!(body["install_plans"][0]["requires_confirmation"], true);
    mock_task.abort();
}
