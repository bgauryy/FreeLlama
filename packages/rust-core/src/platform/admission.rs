//! Weighted admission, bounded backfill, model affinity, and cancellation-safe permit accounting.
use super::TaskPriority;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};
use tokio::sync::Notify;

#[derive(Clone)]
pub(super) struct AdmissionPool {
    state: Arc<StdMutex<AdmissionState>>,
    changed: Arc<Notify>,
}

#[derive(Debug)]
struct AdmissionState {
    /// Units currently charged to admitted tasks.
    active: usize,
    /// Current capacity in units. It moves between 1 and `ceiling` when the adaptive controller
    /// or a configuration reload changes it; lowering it never revokes an admitted task, it only
    /// stops new admissions until enough units are released.
    limit: usize,
    /// Configured maximum for `limit`.
    ceiling: usize,
    max_waiters: usize,
    next_ticket: u64,
    waiting: Vec<AdmissionWaiter>,
    /// Requests that returned their execution slot while waiting for host resources.
    resource_waiting: Vec<(u64, Instant)>,
    /// Weighted round-robin credits, ordered interactive, normal, background.
    credits: [u8; 3],
    /// A selected waiter owns the next charge until it wakes and claims it. Reserving the choice
    /// prevents a thundering herd from letting a later task steal a higher-priority grant.
    granted: Option<u64>,
    in_flight: usize,
    /// Admitted tasks per model, so a waiter for a model already running here can be preferred.
    /// Entries leave when their last permit is released, so the map holds at most `in_flight`.
    running_models: HashMap<Arc<str>, usize>,
    /// The model of the most recently admitted task: likely still resident after it finished.
    recent_model: Option<Arc<str>>,
    admitted: u64,
    released: u64,
    policy_epoch: u64,
    queue_full_rejections: u64,
    queue_timeouts: u64,
    queue_cancellations: u64,
    transition_timeouts: u64,
    /// Grants given to a younger same-class waiter because its model was running or most recent.
    affinity_promotions: u64,
}

impl AdmissionState {
    fn queue_depth(&self) -> usize {
        self.waiting.len() + self.resource_waiting.len()
    }

    fn free(&self) -> usize {
        self.limit.saturating_sub(self.active)
    }

    /// A task never costs more than the whole current limit, so lowering the limit cannot leave
    /// an already-queued expensive task unable to fit forever.
    fn charge(&self, cost: usize) -> usize {
        cost.min(self.limit).max(1)
    }

    /// Whether this waiter's model is running on this backend or was admitted last, so serving
    /// it now is unlikely to make Ollama load or swap a runner. Admission never asks Ollama.
    fn has_affinity(&self, waiter: &AdmissionWaiter) -> bool {
        waiter.model.as_deref().is_some_and(|model| {
            self.running_models.contains_key(model) || self.recent_model.as_deref() == Some(model)
        })
    }

    fn record_admitted_model(&mut self, model: Option<&Arc<str>>) {
        if let Some(model) = model {
            *self.running_models.entry(Arc::clone(model)).or_default() += 1;
            self.recent_model = Some(Arc::clone(model));
        }
    }

    fn release_model(&mut self, model: Option<&Arc<str>>) {
        if let Some(model) = model
            && let Some(count) = self.running_models.get_mut(model)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.running_models.remove(model);
            }
        }
    }
}

#[derive(Debug)]
struct AdmissionWaiter {
    ticket: u64,
    cost: usize,
    priority: TaskPriority,
    /// The selected model, used only to prefer work that reuses a running model.
    model: Option<Arc<str>>,
    enqueued_at: Instant,
    bypasses: u8,
    /// Times a younger same-class waiter was granted first for model affinity.
    affinity_skips: u8,
}

impl AdmissionWaiter {
    fn new(
        ticket: u64,
        cost: usize,
        priority: TaskPriority,
        model: Option<&str>,
        enqueued_at: Instant,
    ) -> Self {
        Self {
            ticket,
            cost,
            priority,
            model: model.map(Arc::from),
            enqueued_at,
            bypasses: 0,
            affinity_skips: 0,
        }
    }

    /// Past its affinity allowance this waiter is served in FIFO order again.
    fn affinity_aged(&self, now: Instant) -> bool {
        self.affinity_skips >= MAX_AFFINITY_SKIPS
            || now.saturating_duration_since(self.enqueued_at) >= MAX_AFFINITY_WAIT
    }
}

/// Held admission charge. Releasing is synchronous and wakes every waiter so the next weighted
/// choice is made against the real current capacity, including mixed task costs.
pub(super) struct AdmissionPermit {
    pool: AdmissionPool,
    cost: usize,
    model: Option<Arc<str>>,
}

impl AdmissionPermit {
    pub(super) fn units(&self) -> usize {
        self.cost
    }
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        let mut state = self
            .pool
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state.active.saturating_sub(self.cost);
        state.in_flight = state.in_flight.saturating_sub(1);
        state.released = state.released.saturating_add(1);
        state.release_model(self.model.as_ref());
        drop(state);
        self.pool.changed.notify_waiters();
    }
}

struct AdmissionWaitGuard {
    pool: AdmissionPool,
    ticket: u64,
    count_cancellation: bool,
}

/// Resource waiting uses queue capacity, but no execution units. Cancellation returns it.
pub(super) struct ResourceWaitGuard {
    pool: AdmissionPool,
    ticket: u64,
    count_cancellation: bool,
}

impl ResourceWaitGuard {
    pub(super) fn finish(mut self) {
        self.count_cancellation = false;
    }

    pub(super) async fn acquire(
        self,
        cost: usize,
        priority: TaskPriority,
        model: Option<&str>,
        deadline: tokio::time::Instant,
    ) -> Result<(AdmissionPermit, u128), AdmissionFailure> {
        let pool = self.pool.clone();
        let queued = Instant::now();
        {
            let mut state = pool
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .resource_waiting
                .retain(|(ticket, _)| *ticket != self.ticket);
            state.waiting.push(AdmissionWaiter::new(
                self.ticket,
                cost,
                priority,
                model,
                queued,
            ));
        }
        let registration = AdmissionWaitGuard {
            pool: pool.clone(),
            ticket: self.ticket,
            count_cancellation: true,
        };
        // Convert the existing registration under one lock. A fresh arrival or a smaller
        // reloaded queue limit cannot take away capacity already owned by this request.
        self.finish();
        pool.acquire_registered(registration, queued, deadline)
            .await
    }
}

impl Drop for ResourceWaitGuard {
    fn drop(&mut self) {
        let mut state = self
            .pool
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .resource_waiting
            .retain(|(ticket, _)| *ticket != self.ticket);
        if self.count_cancellation {
            state.queue_cancellations = state.queue_cancellations.saturating_add(1);
        }
        drop(state);
        self.pool.changed.notify_waiters();
    }
}

impl Drop for AdmissionWaitGuard {
    fn drop(&mut self) {
        let mut state = self
            .pool
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = state.waiting.len();
        state.waiting.retain(|waiter| waiter.ticket != self.ticket);
        if state.granted == Some(self.ticket) {
            state.granted = None;
        }
        if self.count_cancellation && state.waiting.len() < before {
            state.queue_cancellations = state.queue_cancellations.saturating_add(1);
        }
        drop(state);
        self.pool.changed.notify_waiters();
    }
}

#[derive(Clone, Copy)]
pub(super) enum AdmissionFailure {
    QueueFull,
    TimedOut,
}

pub(super) const PRIORITY_WEIGHTS: [u8; 3] = [3, 2, 1];
/// Allow one complete priority round of useful smaller work before reserving released capacity.
/// A stream of one-unit requests must not permanently prevent a two-unit request from fitting.
pub(super) const MAX_CAPACITY_BYPASSES: u8 = 6;
/// Within the class the round robin chose, a waiter whose model is running or was admitted last
/// may go ahead of older waiters for other models. Each older waiter can be passed over this many
/// times, and for at most `MAX_AFFINITY_WAIT`, before the class is served in FIFO order again.
pub(super) const MAX_AFFINITY_SKIPS: u8 = 4;
pub(super) const MAX_AFFINITY_WAIT: Duration = Duration::from_secs(2);

pub(super) const fn priority_index(priority: TaskPriority) -> usize {
    match priority {
        TaskPriority::Interactive => 0,
        TaskPriority::Normal => 1,
        TaskPriority::Background => 2,
    }
}

impl AdmissionPool {
    /// Current capacity in units (the adaptive limit, at most the configured ceiling).
    pub(super) fn total(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .limit
    }

    /// Configured maximum capacity.
    pub(super) fn ceiling(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ceiling
    }

    /// Change the configured maximum. The current limit follows it down, and follows it up only
    /// when it was already at the old ceiling (an adaptive reduction stays in force).
    pub(super) fn set_ceiling(&self, ceiling: usize) {
        let ceiling = ceiling.max(1);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = (state.limit, state.ceiling);
        if state.limit >= state.ceiling || state.limit > ceiling {
            state.limit = ceiling;
        }
        state.ceiling = ceiling;
        if previous != (state.limit, state.ceiling) {
            state.policy_epoch = state.policy_epoch.wrapping_add(1);
        }
        drop(state);
        self.changed.notify_waiters();
    }

    /// Set the current limit within `1..=ceiling`; returns the value applied.
    pub(super) fn set_limit(&self, limit: usize) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = limit.clamp(1, state.ceiling);
        if next != state.limit {
            state.policy_epoch = state.policy_epoch.wrapping_add(1);
            state.limit = next;
        }
        let applied = state.limit;
        drop(state);
        self.changed.notify_waiters();
        applied
    }

    pub(super) fn set_max_waiters(&self, max_waiters: usize) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .max_waiters = max_waiters.max(1);
    }

    /// Waiting requests and the oldest wait, for `Retry-After` estimates and gauges.
    pub(super) fn queue_depth(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue_depth()
    }

    pub(super) fn register_resource_waiter(&self) -> Result<ResourceWaitGuard, AdmissionFailure> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.queue_depth() >= state.max_waiters {
            state.queue_full_rejections = state.queue_full_rejections.saturating_add(1);
            return Err(AdmissionFailure::QueueFull);
        }
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        state.resource_waiting.push((ticket, Instant::now()));
        Ok(ResourceWaitGuard {
            pool: self.clone(),
            ticket,
            count_cancellation: true,
        })
    }

    pub(super) fn record_transition_timeout(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.transition_timeouts = state.transition_timeouts.saturating_add(1);
    }

    pub(super) fn new(total: usize, max_waiters: usize) -> Self {
        Self {
            state: Arc::new(StdMutex::new(AdmissionState {
                active: 0,
                limit: total.max(1),
                ceiling: total.max(1),
                max_waiters: max_waiters.max(1),
                next_ticket: 0,
                waiting: Vec::new(),
                resource_waiting: Vec::new(),
                credits: PRIORITY_WEIGHTS,
                granted: None,
                in_flight: 0,
                running_models: HashMap::new(),
                recent_model: None,
                admitted: 0,
                released: 0,
                policy_epoch: 0,
                queue_full_rejections: 0,
                queue_timeouts: 0,
                queue_cancellations: 0,
                transition_timeouts: 0,
                affinity_promotions: 0,
            })),
            changed: Arc::new(Notify::new()),
        }
    }

    pub(super) fn available(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .free()
    }

    /// Speculative work may claim an idle backend, but must never join or bypass its queue.
    pub(super) fn try_acquire_idle(
        &self,
        cost: usize,
        model: Option<&str>,
    ) -> Option<AdmissionPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.in_flight != 0 || state.queue_depth() != 0 || state.granted.is_some() {
            return None;
        }
        let cost = state.charge(cost);
        let model = model.map(Arc::<str>::from);
        state.active += cost;
        state.in_flight += 1;
        state.admitted = state.admitted.saturating_add(1);
        state.record_admitted_model(model.as_ref());
        Some(AdmissionPermit {
            pool: self.clone(),
            cost,
            model,
        })
    }

    pub(super) fn receipt(&self) -> Value {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        json!({
            "slots_total": state.limit,
            "slots_ceiling": state.ceiling,
            "slots_available": state.free(),
            "active_units": state.active,
            "in_flight": state.in_flight,
            "queue_depth": state.queue_depth(),
            "slot_waiters": state.waiting.len(),
            "resource_waiters": state.resource_waiting.len(),
            "queue_limit": state.max_waiters,
            "oldest_wait_ms": state.waiting.iter()
                .map(|waiter| waiter.enqueued_at)
                .chain(state.resource_waiting.iter().map(|(_, enqueued)| *enqueued))
                .map(|enqueued| enqueued.elapsed().as_millis())
                .max(),
            "admitted": state.admitted,
            "released": state.released,
            "policy_epoch": state.policy_epoch,
            "queue_full_rejections": state.queue_full_rejections,
            "queue_timeouts": state.queue_timeouts,
            "queue_cancellations": state.queue_cancellations,
            "transition_timeouts": state.transition_timeouts,
            "max_capacity_bypasses": MAX_CAPACITY_BYPASSES,
            "capacity_reservation_active": state.waiting.iter()
                .any(|waiter| waiter.bypasses >= MAX_CAPACITY_BYPASSES),
            "affinity_promotions": state.affinity_promotions,
            "affinity_max_skips": MAX_AFFINITY_SKIPS,
            "affinity_max_wait_ms": u64::try_from(MAX_AFFINITY_WAIT.as_millis()).unwrap_or(u64::MAX),
        })
    }

    /// Select using 3:2:1 weighted round robin while allowing bounded backfill of spare capacity.
    /// After six admissions pass a non-fitting request, reserve all newly released capacity for
    /// the oldest such request. Cancellation removes the reservation with its waiter. This
    /// intentionally lets a slot idle briefly so expensive jobs can eventually start.
    /// Within the chosen class, `class_choice` applies bounded model affinity.
    fn selected_waiter(state: &mut AdmissionState, now: Instant) -> Option<usize> {
        if let Some(index) = state
            .waiting
            .iter()
            .position(|waiter| waiter.bypasses >= MAX_CAPACITY_BYPASSES)
        {
            if state.charge(state.waiting[index].cost) > state.free() {
                return None;
            }
            let class = priority_index(state.waiting[index].priority);
            state.credits[class] = state.credits[class].saturating_sub(1);
            Self::record_capacity_bypasses(state);
            return Some(index);
        }
        for pass in 0..2 {
            for class in 0..PRIORITY_WEIGHTS.len() {
                if state.credits[class] == 0 {
                    continue;
                }
                if let Some(index) = Self::class_choice(state, class, now) {
                    state.credits[class] -= 1;
                    Self::record_capacity_bypasses(state);
                    return Some(index);
                }
            }
            if pass == 0 {
                state.credits = PRIORITY_WEIGHTS;
            }
        }
        None
    }

    /// The oldest fitting waiter of `class`, unless a younger fitting one has model affinity and
    /// no older fitting waiter has aged out of affinity skipping. Passed-over waiters count a skip.
    fn class_choice(state: &mut AdmissionState, class: usize, now: Instant) -> Option<usize> {
        let fits = |state: &AdmissionState, waiter: &AdmissionWaiter| {
            priority_index(waiter.priority) == class && state.charge(waiter.cost) <= state.free()
        };
        let head = state
            .waiting
            .iter()
            .position(|waiter| fits(state, waiter))?;
        let mut promoted = None;
        for (index, waiter) in state.waiting.iter().enumerate().skip(head) {
            if !fits(state, waiter) {
                continue;
            }
            if state.has_affinity(waiter) {
                promoted = Some(index);
                break;
            }
            if waiter.affinity_aged(now) {
                break;
            }
        }
        let Some(promoted) = promoted.filter(|&index| index != head) else {
            return Some(head);
        };
        let passed_over: Vec<usize> = (head..promoted)
            .filter(|&index| fits(state, &state.waiting[index]))
            .collect();
        for index in passed_over {
            let waiter = &mut state.waiting[index];
            waiter.affinity_skips = waiter.affinity_skips.saturating_add(1);
        }
        state.affinity_promotions = state.affinity_promotions.saturating_add(1);
        Some(promoted)
    }

    fn record_capacity_bypasses(state: &mut AdmissionState) {
        let free = state.free();
        let limit = state.limit;
        for waiter in &mut state.waiting {
            if waiter.cost.min(limit) > free {
                waiter.bypasses = waiter.bypasses.saturating_add(1);
            }
        }
    }

    pub(super) async fn acquire(
        &self,
        cost: usize,
        priority: TaskPriority,
        model: Option<&str>,
        deadline: tokio::time::Instant,
    ) -> Result<(AdmissionPermit, u128), AdmissionFailure> {
        let queued = Instant::now();
        let ticket = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if tokio::time::Instant::now() >= deadline {
                state.queue_timeouts = state.queue_timeouts.saturating_add(1);
                return Err(AdmissionFailure::TimedOut);
            }
            if state.queue_depth() >= state.max_waiters {
                state.queue_full_rejections = state.queue_full_rejections.saturating_add(1);
                return Err(AdmissionFailure::QueueFull);
            }
            let ticket = state.next_ticket;
            state.next_ticket = state.next_ticket.wrapping_add(1);
            state
                .waiting
                .push(AdmissionWaiter::new(ticket, cost, priority, model, queued));
            ticket
        };
        let registration = AdmissionWaitGuard {
            pool: self.clone(),
            ticket,
            count_cancellation: true,
        };
        self.acquire_registered(registration, queued, deadline)
            .await
    }

    async fn acquire_registered(
        &self,
        mut registration: AdmissionWaitGuard,
        queued: Instant,
        deadline: tokio::time::Instant,
    ) -> Result<(AdmissionPermit, u128), AdmissionFailure> {
        let ticket = registration.ticket;
        self.changed.notify_waiters();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (acquired, selected, units, model) = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if tokio::time::Instant::now() >= deadline {
                    state.queue_timeouts = state.queue_timeouts.saturating_add(1);
                    registration.count_cancellation = false;
                    return Err(AdmissionFailure::TimedOut);
                }
                let mut selected = false;
                if state.granted.is_none()
                    && let Some(index) = Self::selected_waiter(&mut state, Instant::now())
                {
                    state.granted = Some(state.waiting[index].ticket);
                    selected = true;
                }
                let granted_index = if state.granted == Some(ticket) {
                    Some(
                        state
                            .waiting
                            .iter()
                            .position(|waiter| waiter.ticket == ticket)
                            .expect("granted admission waiter must remain queued"),
                    )
                } else {
                    None
                };
                // A reduction may occur after selection and before the chosen waiter wakes.
                if let Some(index) = granted_index
                    .filter(|&index| state.charge(state.waiting[index].cost) <= state.free())
                {
                    let waiter = state.waiting.remove(index);
                    let units = state.charge(waiter.cost);
                    state.active = state.active.saturating_add(units);
                    state.in_flight = state.in_flight.saturating_add(1);
                    state.admitted = state.admitted.saturating_add(1);
                    state.record_admitted_model(waiter.model.as_ref());
                    state.granted = None;
                    (true, selected, units, waiter.model)
                } else {
                    (false, selected, 0, None)
                }
            };
            if acquired {
                registration.count_cancellation = false;
                return Ok((
                    AdmissionPermit {
                        pool: self.clone(),
                        cost: units,
                        model,
                    },
                    queued.elapsed().as_millis(),
                ));
            }
            if selected {
                self.changed.notify_waiters();
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.queue_timeouts = state.queue_timeouts.saturating_add(1);
                drop(state);
                registration.count_cancellation = false;
                return Err(AdmissionFailure::TimedOut);
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
            tokio::spawn(
                async move { other.acquire(1, TaskPriority::Normal, None, deadline).await },
            );
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
}
