//! Resource refusals with same-capability model alternatives.
use crate::platform::{
    PlatformState,
    error::{ApiError, MemoryFigures},
    resources,
};

/// A memory refusal tells an agent what to do: the figures, who holds the memory, and which
/// installed models with the same task capability would fit the free memory now (or that none
/// does). Other errors pass through unchanged.
pub(super) async fn refusal_guidance(
    state: &PlatformState,
    route: &crate::platform::RouteInput,
    error: ApiError,
) -> ApiError {
    if !error.is_resource_wait() || !error.memory_held() {
        return error;
    }
    let snapshot = state.resources.snapshot().await;
    let figures = error.assessed_memory().or_else(|| {
        error
            .deadline_required_bytes()
            .map(|required| MemoryFigures {
                required_with_reserve: required.saturating_add(snapshot.hold_reserve_bytes),
                reserve: snapshot.hold_reserve_bytes,
                free: snapshot.effective_available_bytes,
            })
    });
    let free = figures
        .and_then(|figures| figures.free)
        .or(snapshot.effective_available_bytes);
    let reserve = figures.map_or(snapshot.hold_reserve_bytes, |figures| figures.reserve);
    let alternatives = match (free, crate::platform::catalog::discover_models(state).await) {
        (Some(free), Ok(models)) => Some(fitting_models_hint(route, &models, free, reserve)),
        _ => None,
    };
    let holders = resources::memory_holders_now().await;
    error.with_memory_guidance(figures, holders, alternatives)
}

/// Installed models that satisfy the route's task capability and whose file size plus the
/// reserve fits `free`. File size is a lower bound (context memory comes on top), so the hint
/// says "by file size".
pub(super) fn fitting_models_hint(
    route: &crate::platform::RouteInput,
    models: &[crate::platform::CatalogModel],
    free: u64,
    reserve: u64,
) -> String {
    let mut request = route.clone();
    request.model = None;
    let task = serde_json::to_value(request.task)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let mut fitting = crate::platform::routing::eligible_models(&request, models)
        .unwrap_or_default()
        .into_iter()
        .filter(|model| model.size.saturating_add(reserve) <= free)
        .collect::<Vec<_>>();
    fitting.sort_by_key(|model| std::cmp::Reverse(model.size));
    if fitting.is_empty() {
        return format!(
            "no installed `{task}` model fits {} free after the reserve; free memory (close the programs listed) or retry later",
            resources::gb(free)
        );
    }
    let names = fitting
        .iter()
        .take(5)
        .map(|model| format!("{} ({})", model.name, resources::gb(model.size)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "installed `{task}` models that fit {} free by file size (context memory is extra): {names}; pin one with `model`",
        resources::gb(free)
    )
}
