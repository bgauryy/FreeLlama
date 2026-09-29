//! Weighted admission, bounded backfill, and cancellation-safe permit accounting.
use super::TaskPriority;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex as StdMutex},
    time::Instant,
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
    /// Weighted round-robin credits, ordered interactive, normal, background.
    credits: [u8; 3],
    /// A selected waiter owns the next charge until it wakes and claims it. Reserving the choice
    /// prevents a thundering herd from letting a later task steal a higher-priority grant.
    granted: Option<u64>,
    in_flight: usize,
    admitted: u64,
    released: u64,
    queue_full_rejections: u64,
    queue_timeouts: u64,
    queue_cancellations: u64,
    transition_timeouts: u64,
}

impl AdmissionState {
    fn free(&self) -> usize {
        self.limit.saturating_sub(self.active)
    }

    /// A task never costs more than the whole current limit, so lowering the limit cannot leave
    /// an already-queued expensive task unable to fit forever.
    fn charge(&self, cost: usize) -> usize {
        cost.min(self.limit).max(1)
    }
}

#[derive(Debug)]
struct AdmissionWaiter {
    ticket: u64,
    cost: usize,
    priority: TaskPriority,
    enqueued_at: Instant,
    bypasses: u8,
}

/// Held admission charge. Releasing is synchronous and wakes every waiter so the next weighted
/// choice is made against the real current capacity, including mixed task costs.
pub(super) struct AdmissionPermit {
    pool: AdmissionPool,
    cost: usize,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        let mut state = self.pool.state.lock().expect("admission state poisoned");
        state.active = state.active.saturating_sub(self.cost);
        state.in_flight = state.in_flight.saturating_sub(1);
        state.released = state.released.saturating_add(1);
        drop(state);
        self.pool.changed.notify_waiters();
    }
}

struct AdmissionWaitGuard {
    pool: AdmissionPool,
    ticket: u64,
    count_cancellation: bool,
}

impl Drop for AdmissionWaitGuard {
    fn drop(&mut self) {
        let mut state = self.pool.state.lock().expect("admission state poisoned");
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

pub(super) enum AdmissionFailure {
    QueueFull,
    TimedOut,
}

pub(super) const PRIORITY_WEIGHTS: [u8; 3] = [3, 2, 1];
/// Allow one complete priority round of useful smaller work before reserving released capacity.
/// A stream of one-unit requests must not permanently prevent a two-unit request from fitting.
pub(super) const MAX_CAPACITY_BYPASSES: u8 = 6;

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
        self.state.lock().expect("admission state poisoned").limit
    }

    /// Configured maximum capacity.
    pub(super) fn ceiling(&self) -> usize {
        self.state.lock().expect("admission state poisoned").ceiling
    }

    /// Change the configured maximum. The current limit follows it down, and follows it up only
    /// when it was already at the old ceiling (an adaptive reduction stays in force).
    pub(super) fn set_ceiling(&self, ceiling: usize) {
        let ceiling = ceiling.max(1);
        let mut state = self.state.lock().expect("admission state poisoned");
        if state.limit >= state.ceiling || state.limit > ceiling {
            state.limit = ceiling;
        }
        state.ceiling = ceiling;
        drop(state);
        self.changed.notify_waiters();
    }

    /// Set the current limit within `1..=ceiling`; returns the value applied.
    pub(super) fn set_limit(&self, limit: usize) -> usize {
        let mut state = self.state.lock().expect("admission state poisoned");
        state.limit = limit.clamp(1, state.ceiling);
        let applied = state.limit;
        drop(state);
        self.changed.notify_waiters();
        applied
    }

    pub(super) fn set_max_waiters(&self, max_waiters: usize) {
        self.state
            .lock()
            .expect("admission state poisoned")
            .max_waiters = max_waiters.max(1);
    }

    /// Waiting requests and the oldest wait, for `Retry-After` estimates and gauges.
    pub(super) fn queue_depth(&self) -> usize {
        self.state
            .lock()
            .expect("admission state poisoned")
            .waiting
            .len()
    }

    pub(super) fn record_transition_timeout(&self) {
        let mut state = self.state.lock().expect("admission state poisoned");
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
                credits: PRIORITY_WEIGHTS,
                granted: None,
                in_flight: 0,
                admitted: 0,
                released: 0,
                queue_full_rejections: 0,
                queue_timeouts: 0,
                queue_cancellations: 0,
                transition_timeouts: 0,
            })),
            changed: Arc::new(Notify::new()),
        }
    }

    pub(super) fn available(&self) -> usize {
        self.state.lock().expect("admission state poisoned").free()
    }

    pub(super) fn receipt(&self) -> Value {
        let state = self.state.lock().expect("admission state poisoned");
        json!({
            "slots_total": state.limit,
            "slots_ceiling": state.ceiling,
            "slots_available": state.free(),
            "active_units": state.active,
            "in_flight": state.in_flight,
            "queue_depth": state.waiting.len(),
            "queue_limit": state.max_waiters,
            "oldest_wait_ms": state.waiting.iter()
                .map(|waiter| waiter.enqueued_at.elapsed().as_millis())
                .max(),
            "admitted": state.admitted,
            "released": state.released,
            "queue_full_rejections": state.queue_full_rejections,
            "queue_timeouts": state.queue_timeouts,
            "queue_cancellations": state.queue_cancellations,
            "transition_timeouts": state.transition_timeouts,
            "max_capacity_bypasses": MAX_CAPACITY_BYPASSES,
            "capacity_reservation_active": state.waiting.iter()
                .any(|waiter| waiter.bypasses >= MAX_CAPACITY_BYPASSES),
        })
    }

    /// Select using 3:2:1 weighted round robin while allowing bounded backfill of spare capacity.
    /// After six admissions pass a non-fitting request, reserve all newly released capacity for
    /// the oldest such request. Cancellation removes the reservation with its waiter. This
    /// intentionally lets a slot idle briefly so expensive jobs can eventually start.
    fn selected_waiter(state: &mut AdmissionState) -> Option<usize> {
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
                if let Some((index, _)) = state.waiting.iter().enumerate().find(|(_, waiter)| {
                    priority_index(waiter.priority) == class
                        && state.charge(waiter.cost) <= state.free()
                }) {
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
        deadline: tokio::time::Instant,
    ) -> Result<(AdmissionPermit, u128), AdmissionFailure> {
        let queued = Instant::now();
        let ticket = {
            let mut state = self.state.lock().expect("admission state poisoned");
            if state.waiting.len() >= state.max_waiters {
                state.queue_full_rejections = state.queue_full_rejections.saturating_add(1);
                return Err(AdmissionFailure::QueueFull);
            }
            let ticket = state.next_ticket;
            state.next_ticket = state.next_ticket.wrapping_add(1);
            state.waiting.push(AdmissionWaiter {
                ticket,
                cost,
                priority,
                enqueued_at: queued,
                bypasses: 0,
            });
            ticket
        };
        let mut registration = AdmissionWaitGuard {
            pool: self.clone(),
            ticket,
            count_cancellation: true,
        };
        self.changed.notify_waiters();
        loop {
            let changed = self.changed.notified();
            let (acquired, selected, units) = {
                let mut state = self.state.lock().expect("admission state poisoned");
                let mut selected = false;
                if state.granted.is_none() {
                    if let Some(index) = Self::selected_waiter(&mut state) {
                        state.granted = Some(state.waiting[index].ticket);
                        selected = true;
                    }
                }
                if state.granted == Some(ticket) {
                    let index = state
                        .waiting
                        .iter()
                        .position(|waiter| waiter.ticket == ticket)
                        .expect("granted admission waiter must remain queued");
                    let waiter = state.waiting.remove(index);
                    let units = state.charge(waiter.cost);
                    state.active = state.active.saturating_add(units);
                    state.in_flight = state.in_flight.saturating_add(1);
                    state.admitted = state.admitted.saturating_add(1);
                    state.granted = None;
                    (true, selected, units)
                } else {
                    (false, selected, 0)
                }
            };
            if acquired {
                registration.count_cancellation = false;
                return Ok((
                    AdmissionPermit {
                        pool: self.clone(),
                        cost: units,
                    },
                    queued.elapsed().as_millis(),
                ));
            }
            if selected {
                self.changed.notify_waiters();
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                let mut state = self.state.lock().expect("admission state poisoned");
                state.queue_timeouts = state.queue_timeouts.saturating_add(1);
                drop(state);
                registration.count_cancellation = false;
                return Err(AdmissionFailure::TimedOut);
            }
        }
    }
}
