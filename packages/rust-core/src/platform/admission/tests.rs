use super::*;
use std::{
    future::Future,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn policy_epoch_detects_limit_and_ceiling_transitions_away_and_back() {
    let pool = AdmissionPool::new(4, 4);
    pool.set_limit(4);
    pool.set_ceiling(4);
    assert_eq!(pool.receipt()["policy_epoch"], 0);
    pool.set_limit(2);
    pool.set_limit(4);
    assert_eq!(pool.receipt()["policy_epoch"], 2);
    pool.set_ceiling(2);
    pool.set_ceiling(4);
    assert_eq!(pool.receipt()["policy_epoch"], 4);
    assert_eq!(pool.total(), 4);
}

#[tokio::test]
async fn claiming_a_grant_wakes_waiters_for_remaining_capacity() {
    let pool = AdmissionPool::new(4, 4);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let (holder, _) = pool
        .acquire(4, TaskPriority::Normal, None, deadline)
        .await
        .unwrap_or_else(|_| panic!("holder"));
    let mut chosen = Box::pin(pool.acquire(1, TaskPriority::Interactive, None, deadline));
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(chosen.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    let mut following = Box::pin(pool.acquire(1, TaskPriority::Normal, None, deadline));
    let notification_count = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&notification_count));
    let mut context = Context::from_waker(&waker);
    assert!(following.as_mut().poll(&mut context).is_pending());

    drop(holder);
    assert!(following.as_mut().poll(&mut context).is_pending());
    let before_claim = notification_count.0.load(Ordering::Relaxed);
    let (chosen_permit, _) = chosen.await.unwrap_or_else(|_| panic!("chosen waiter"));
    assert_eq!(pool.available(), 3);
    assert!(
        notification_count.0.load(Ordering::Relaxed) > before_claim,
        "a sleeping follower must be woken while the chosen task retains its permit"
    );
    let (following_permit, _) = following
        .await
        .unwrap_or_else(|_| panic!("following waiter"));
    assert_eq!(pool.receipt()["in_flight"], 2);
    assert_eq!(pool.available(), 2);
    drop(chosen_permit);
    drop(following_permit);
    assert_eq!(pool.available(), 4);
    assert_eq!(pool.receipt()["admitted"], pool.receipt()["released"]);
}

async fn assert_grant_respects_reduced_capacity(reduce: fn(&AdmissionPool)) {
    let pool = AdmissionPool::new(4, 4);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let (anchor, _) = pool
        .acquire(1, TaskPriority::Normal, None, deadline)
        .await
        .unwrap_or_else(|_| panic!("anchor"));
    let (released, _) = pool
        .acquire(1, TaskPriority::Normal, None, deadline)
        .await
        .unwrap_or_else(|_| panic!("released holder"));
    let mut chosen = Box::pin(pool.acquire(3, TaskPriority::Interactive, None, deadline));
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(chosen.as_mut().poll(cx)))
            .await
            .is_pending()
    );

    drop(released);
    let mut following = Box::pin(pool.acquire(1, TaskPriority::Normal, None, deadline));
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(following.as_mut().poll(cx)))
            .await
            .is_pending(),
        "the interactive waiter must own the grant before it wakes"
    );
    assert!(pool.state.lock().unwrap().granted.is_some());
    reduce(&pool);
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(chosen.as_mut().poll(cx)))
            .await
            .is_pending(),
        "a grant must not admit more units than the reduced capacity permits"
    );
    assert_eq!(pool.receipt()["active_units"], 1);
    assert_eq!(pool.receipt()["in_flight"], 1);
    drop(following);
    drop(anchor);
    let (permit, _) = chosen
        .await
        .unwrap_or_else(|_| panic!("grant must resume after capacity is released"));
    assert_eq!(pool.receipt()["active_units"], 2);
    assert_eq!(pool.total(), 2);
    drop(permit);
    assert_eq!(pool.receipt()["active_units"], 0);
    assert_eq!(pool.receipt()["admitted"], pool.receipt()["released"]);
}

#[tokio::test]
async fn an_adaptive_reduction_revalidates_a_grant_before_admission() {
    assert_grant_respects_reduced_capacity(|pool| {
        pool.set_limit(2);
    })
    .await;
}

#[tokio::test]
async fn a_ceiling_reload_revalidates_a_grant_before_admission() {
    assert_grant_respects_reduced_capacity(|pool| pool.set_ceiling(2)).await;
}

#[tokio::test]
async fn cancelling_after_resource_to_slot_handoff_reclaims_the_waiter() {
    let pool = AdmissionPool::new(1, 1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let (holder, _) = pool
        .acquire(1, TaskPriority::Normal, None, deadline)
        .await
        .unwrap_or_else(|_| panic!("holder"));
    let resource = pool
        .register_resource_waiter()
        .unwrap_or_else(|_| panic!("resource waiter"));
    let pending = tokio::spawn(resource.acquire(1, TaskPriority::Normal, None, deadline));
    tokio::time::timeout(Duration::from_secs(1), async {
        while pool.receipt()["slot_waiters"] != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(pool.receipt()["resource_waiters"], 0);
    assert_eq!(pool.queue_depth(), 1);
    pending.abort();
    let _ = pending.await;
    assert_eq!(pool.queue_depth(), 0);
    assert_eq!(pool.receipt()["queue_cancellations"], 1);
    drop(holder);
    assert_eq!(pool.receipt()["in_flight"], 0);
}

#[tokio::test]
async fn an_expired_deadline_cannot_admit_work_into_a_free_slot() {
    let pool = AdmissionPool::new(1, 2);
    let result = pool
        .acquire(
            1,
            TaskPriority::Normal,
            None,
            tokio::time::Instant::now() - Duration::from_secs(1),
        )
        .await;
    assert!(matches!(result, Err(AdmissionFailure::TimedOut)));
    assert_eq!(pool.receipt()["admitted"], 0);
    assert_eq!(pool.receipt()["queue_depth"], 0);
    assert_eq!(pool.receipt()["queue_timeouts"], 1);
}

#[tokio::test]
async fn a_resource_waiter_keeps_its_place_when_the_queue_limit_is_reduced() {
    let pool = AdmissionPool::new(1, 2);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let (holder, _) = pool
        .acquire(1, TaskPriority::Normal, None, deadline)
        .await
        .unwrap_or_else(|_| panic!("holder"));
    let resource = pool
        .register_resource_waiter()
        .unwrap_or_else(|_| panic!("resource waiter"));
    let other = pool.clone();
    let queued =
        tokio::spawn(async move { other.acquire(1, TaskPriority::Normal, None, deadline).await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while pool.queue_depth() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pool.set_max_waiters(1);
    let mut returning = tokio::spawn(resource.acquire(1, TaskPriority::Normal, None, deadline));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut returning)
            .await
            .is_err(),
        "accepted work must retain its queue registration during resource-to-slot handoff"
    );
    assert_eq!(pool.queue_depth(), 2);
    assert_eq!(pool.receipt()["queue_full_rejections"], 0);
    drop(holder);
    let (first, _) = queued
        .await
        .unwrap()
        .unwrap_or_else(|_| panic!("queued task"));
    drop(first);
    let (second, _) = returning
        .await
        .unwrap()
        .unwrap_or_else(|_| panic!("returning task"));
    drop(second);
    assert_eq!(pool.queue_depth(), 0);
    assert_eq!(pool.receipt()["in_flight"], 0);
}

fn queue(pool: &AdmissionPool, waiters: &[(TaskPriority, Option<&str>, Duration)]) {
    let now = Instant::now();
    let mut state = pool.state.lock().unwrap();
    for (priority, model, age) in waiters {
        let ticket = state.next_ticket;
        state.next_ticket += 1;
        state.waiting.push(AdmissionWaiter::new(
            ticket,
            1,
            *priority,
            *model,
            now.checked_sub(*age).unwrap_or(now),
        ));
    }
}

/// Select as a woken waiter would, then remove the chosen one as its claim does.
fn admit_next(pool: &AdmissionPool) -> Option<String> {
    let mut state = pool.state.lock().unwrap();
    let index = AdmissionPool::selected_waiter(&mut state, Instant::now())?;
    let waiter = state.waiting.remove(index);
    waiter.model.map(|model| model.to_string())
}

fn run_model(pool: &AdmissionPool, model: &str) -> AdmissionPermit {
    pool.try_acquire_idle(1, Some(model))
        .unwrap_or_else(|| panic!("idle pool admits {model}"))
}

#[test]
fn a_waiter_for_the_running_model_goes_ahead_of_an_older_waiter_for_another_model() {
    let pool = AdmissionPool::new(4, 8);
    let _running = run_model(&pool, "hot");
    queue(
        &pool,
        &[
            (TaskPriority::Normal, Some("cold"), Duration::ZERO),
            (TaskPriority::Normal, Some("hot"), Duration::ZERO),
        ],
    );
    assert_eq!(admit_next(&pool).as_deref(), Some("hot"));
    assert_eq!(pool.receipt()["affinity_promotions"], 1);
    assert_eq!(pool.state.lock().unwrap().waiting[0].affinity_skips, 1);
    assert_eq!(admit_next(&pool).as_deref(), Some("cold"));
    assert_eq!(
        pool.receipt()["affinity_promotions"],
        1,
        "FIFO head is no promotion"
    );
}

#[test]
fn affinity_skips_are_bounded_and_then_the_class_is_fifo_again() {
    let pool = AdmissionPool::new(4, 8);
    let _running = run_model(&pool, "hot");
    queue(
        &pool,
        &[(TaskPriority::Normal, Some("cold"), Duration::ZERO)],
    );
    for _ in 0..MAX_AFFINITY_SKIPS {
        queue(
            &pool,
            &[(TaskPriority::Normal, Some("hot"), Duration::ZERO)],
        );
        assert_eq!(admit_next(&pool).as_deref(), Some("hot"));
    }
    queue(
        &pool,
        &[(TaskPriority::Normal, Some("hot"), Duration::ZERO)],
    );
    assert_eq!(
        admit_next(&pool).as_deref(),
        Some("cold"),
        "a waiter skipped {MAX_AFFINITY_SKIPS} times is served in arrival order"
    );
    assert_eq!(
        pool.receipt()["affinity_promotions"],
        u64::from(MAX_AFFINITY_SKIPS)
    );
}

#[test]
fn a_waiter_older_than_the_affinity_wait_is_served_in_arrival_order() {
    let pool = AdmissionPool::new(4, 8);
    let _running = run_model(&pool, "hot");
    queue(
        &pool,
        &[
            (TaskPriority::Normal, Some("fresh"), Duration::ZERO),
            (
                TaskPriority::Normal,
                Some("old"),
                MAX_AFFINITY_WAIT + Duration::from_millis(1),
            ),
            (TaskPriority::Normal, Some("hot"), Duration::ZERO),
        ],
    );
    // The aged waiter blocks promotion past it, so the class falls back to its head.
    assert_eq!(admit_next(&pool).as_deref(), Some("fresh"));
    assert_eq!(admit_next(&pool).as_deref(), Some("old"));
    assert_eq!(admit_next(&pool).as_deref(), Some("hot"));
    assert_eq!(pool.receipt()["affinity_promotions"], 0);
}

#[test]
fn affinity_only_reorders_within_the_class_the_round_robin_chose() {
    let pool = AdmissionPool::new(4, 8);
    let _running = run_model(&pool, "hot");
    queue(
        &pool,
        &[
            (TaskPriority::Interactive, Some("cold"), Duration::ZERO),
            (TaskPriority::Normal, Some("hot"), Duration::ZERO),
            (TaskPriority::Background, Some("hot"), Duration::ZERO),
            (TaskPriority::Interactive, None, Duration::ZERO),
        ],
    );
    assert_eq!(admit_next(&pool).as_deref(), Some("cold"));
    assert_eq!(
        admit_next(&pool),
        None,
        "an unnamed interactive waiter is next"
    );
    assert_eq!(
        pool.state.lock().unwrap().waiting.len(),
        2,
        "both interactive waiters went before normal and background work"
    );
    assert_eq!(pool.receipt()["affinity_promotions"], 0);
}

#[test]
fn the_last_admitted_model_keeps_affinity_after_its_task_finishes_and_releases_its_entry() {
    let pool = AdmissionPool::new(4, 8);
    drop(run_model(&pool, "recent"));
    assert!(pool.state.lock().unwrap().running_models.is_empty());
    queue(
        &pool,
        &[
            (TaskPriority::Normal, Some("other"), Duration::ZERO),
            (TaskPriority::Normal, Some("recent"), Duration::ZERO),
        ],
    );
    assert_eq!(admit_next(&pool).as_deref(), Some("recent"));
}

#[test]
fn a_waiter_that_does_not_fit_is_never_promoted_over_one_that_does() {
    let pool = AdmissionPool::new(2, 8);
    let _running = run_model(&pool, "hot");
    {
        let now = Instant::now();
        let mut state = pool.state.lock().unwrap();
        state.waiting.push(AdmissionWaiter::new(
            10,
            1,
            TaskPriority::Normal,
            Some("cold"),
            now,
        ));
        state.waiting.push(AdmissionWaiter::new(
            11,
            2,
            TaskPriority::Normal,
            Some("hot"),
            now,
        ));
    }
    assert_eq!(admit_next(&pool).as_deref(), Some("cold"));
    assert_eq!(pool.receipt()["affinity_promotions"], 0);
}

#[tokio::test]
async fn released_capacity_goes_to_the_model_just_used_before_an_older_other_model() {
    let pool = AdmissionPool::new(1, 8);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let (holder, _) = pool
        .acquire(1, TaskPriority::Normal, Some("warm"), deadline)
        .await
        .unwrap_or_else(|_| panic!("holder"));
    let mut cold = Box::pin(pool.acquire(1, TaskPriority::Normal, Some("cold"), deadline));
    let mut warm = Box::pin(pool.acquire(1, TaskPriority::Normal, Some("warm"), deadline));
    for waiter in [&mut cold, &mut warm] {
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(waiter.as_mut().poll(cx)))
                .await
                .is_pending()
        );
    }
    drop(holder);
    let (warm_permit, _) = tokio::time::timeout(Duration::from_secs(1), &mut warm)
        .await
        .unwrap()
        .unwrap_or_else(|_| panic!("warm waiter"));
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(cold.as_mut().poll(cx)))
            .await
            .is_pending(),
        "the older cold-model waiter is still queued"
    );
    assert_eq!(pool.receipt()["affinity_promotions"], 1);
    drop(warm_permit);
    let (cold_permit, _) = tokio::time::timeout(Duration::from_secs(1), cold)
        .await
        .unwrap()
        .unwrap_or_else(|_| panic!("cold waiter is served once capacity frees again"));
    drop(cold_permit);
    let receipt = pool.receipt();
    assert_eq!(receipt["admitted"], receipt["released"]);
    assert_eq!(receipt["queue_depth"], 0);
    assert!(pool.state.lock().unwrap().running_models.is_empty());
}
