//! Queue for the single-resident-model inference slot.
//!
//! Exactly one transcription runs at a time. Waiters are served in arrival
//! order within a class; the `interactive` class (a live dictation turn, see
//! [`Priority`]) is always served before the `normal` one. A request that
//! names no priority is `normal`, so a server nobody sends a priority to is
//! strictly FIFO. A second, independent single-permit slot admits at most
//! one live stream at a time and records which model that stream holds (see
//! [`InferenceQueue::acquire_for_model`]). By default the waiting list is
//! unbounded and a waiter never times out, matching the current shipping
//! server (user decision). Callers may optionally pass a `max_waiting` bound
//! (rejecting any waiter beyond it immediately with [`QueueError::Full`])
//! and/or a `wait_timeout` (giving up with [`QueueError::Timeout`] once
//! exceeded); both are read live per call from `App::limits` rather than
//! fixed at construction, so a `PATCH /v1/local/config` change or a CLI
//! override takes effect immediately.
//!
//! The slot is a tiny dispatcher: one mutex guards the busy flag and the two
//! waiting lists, and dropping a permit hands the slot straight to the next
//! waiter (so a newcomer can never jump ahead of a queued request). A
//! waiter's entry is removed by an RAII guard when its future ends -- also
//! when the caller's future is dropped (e.g. the client disconnected while
//! queued) -- so a cancelled wait always frees its place. A grant that
//! races with such a cancellation is passed on to the next waiter.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, watch, OwnedSemaphorePermit, Semaphore};

#[derive(Debug, PartialEq, Eq)]
pub enum QueueError {
    /// `max_waiting` requests are already queued ahead of this one.
    Full,
    /// The request waited longer than `wait_timeout` for a turn.
    Timeout,
    /// A live stream holds the requested model, and it did not finish within
    /// `wait_timeout`.
    StreamActive,
}

/// Which waiting list a request joins. Within a class the order is FIFO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    /// Served after every waiting `Interactive` request. The default.
    #[default]
    Normal,
    /// A request a person is waiting on in real time (a dictation turn).
    Interactive,
}

impl Priority {
    /// The value of the optional `priority` field of the transcription
    /// endpoints.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "normal" => Some(Self::Normal),
            "interactive" => Some(Self::Interactive),
            _ => None,
        }
    }
}

struct Waiter {
    id: u64,
    grant: oneshot::Sender<InferencePermit>,
}

#[derive(Default)]
struct State {
    busy: bool,
    next_id: u64,
    interactive: VecDeque<Waiter>,
    normal: VecDeque<Waiter>,
}

impl State {
    fn waiting(&self) -> usize {
        self.interactive.len() + self.normal.len()
    }

    fn next_waiter(&mut self) -> Option<Waiter> {
        self.interactive
            .pop_front()
            .or_else(|| self.normal.pop_front())
    }

    fn remove(&mut self, id: u64) {
        self.interactive.retain(|waiter| waiter.id != id);
        self.normal.retain(|waiter| waiter.id != id);
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The held inference slot. Dropping it hands the slot to the next waiter
/// (interactive first), or frees it when nobody waits.
pub struct InferencePermit {
    state: Arc<Mutex<State>>,
    /// False for a permit whose grant found no receiver: it never owned the
    /// slot, so dropping it must not release it.
    owns_slot: bool,
}

impl std::fmt::Debug for InferencePermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferencePermit").finish_non_exhaustive()
    }
}

impl Drop for InferencePermit {
    fn drop(&mut self) {
        if !self.owns_slot {
            return;
        }
        let mut state = lock(&self.state);
        while let Some(waiter) = state.next_waiter() {
            let next = InferencePermit {
                state: self.state.clone(),
                owns_slot: true,
            };
            match waiter.grant.send(next) {
                Ok(()) => return,
                // The waiter is gone: take the permit back unarmed and try
                // the next one.
                Err(mut unused) => unused.owns_slot = false,
            }
        }
        state.busy = false;
    }
}

/// Removes a waiter from the lists when its wait ends or is cancelled.
struct WaiterEntry {
    state: Arc<Mutex<State>>,
    id: u64,
}

impl Drop for WaiterEntry {
    fn drop(&mut self) {
        lock(&self.state).remove(self.id);
    }
}

/// The live-stream slot (`GET /v1/realtime`). While held it may name the
/// model the stream runs on; dropping it clears that and frees the slot.
pub struct StreamSlot {
    _permit: OwnedSemaphorePermit,
    lease: Arc<watch::Sender<Option<String>>>,
}

impl StreamSlot {
    /// Record that the stream holds `model`: batch requests for it wait for
    /// the stream to end (see [`InferenceQueue::acquire_for_model`]).
    pub fn lease_model(&self, model: &str) {
        self.lease.send_replace(Some(model.to_owned()));
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        // Runs before the fields drop, so the lease is cleared before the
        // slot can be claimed again.
        self.lease.send_replace(None);
    }
}

/// A single-slot inference queue with two priority classes, an optionally
/// bounded waiting list and an optional per-wait deadline.
pub struct InferenceQueue {
    state: Arc<Mutex<State>>,
    stream_slot: Arc<Semaphore>,
    /// The model the active stream holds, if any.
    stream_lease: Arc<watch::Sender<Option<String>>>,
}

impl Default for InferenceQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl InferenceQueue {
    pub fn new() -> Self {
        Self {
            state: Arc::default(),
            stream_slot: Arc::new(Semaphore::new(1)),
            stream_lease: Arc::new(watch::channel(None).0),
        }
    }

    /// Claim the single live-stream slot (`GET /v1/realtime`), or `None`
    /// when a stream is already running. Independent of the batch permit: a
    /// stream takes the batch permit only while it begins (model bind and
    /// `Session::stream`), then runs holding just this slot, so batch requests
    /// for other models are never queued behind it (see `crate::realtime`).
    pub fn try_acquire_stream(&self) -> Option<StreamSlot> {
        let permit = self.stream_slot.clone().try_acquire_owned().ok()?;
        Some(StreamSlot {
            _permit: permit,
            lease: self.stream_lease.clone(),
        })
    }

    pub fn waiting_count(&self) -> usize {
        lock(&self.state).waiting()
    }

    /// Wait for the slot like any normal request, with no waiting-list bound
    /// and no deadline. For callers (model unload, verification cleanup)
    /// that must wait for in-flight inference to finish.
    pub async fn acquire_unbounded(&self) -> Result<InferencePermit, QueueError> {
        let (permit, _) = self.acquire(Priority::Normal, None, None).await?;
        Ok(permit)
    }

    /// Join the queue for a transcription slot. Returns the held permit and
    /// how many milliseconds this call spent waiting for it. `max_waiting`
    /// and `wait_timeout` of `None` mean unbounded/no-deadline.
    pub async fn acquire(
        &self,
        priority: Priority,
        max_waiting: Option<usize>,
        wait_timeout: Option<Duration>,
    ) -> Result<(InferencePermit, u64), QueueError> {
        let (granted, _entry) = {
            let mut state = lock(&self.state);
            // Fast path: a free slot never touches the waiting lists.
            if !state.busy {
                state.busy = true;
                return Ok((self.permit(), 0));
            }
            if max_waiting.is_some_and(|max| state.waiting() >= max) {
                return Err(QueueError::Full);
            }
            let id = state.next_id;
            state.next_id += 1;
            let (grant, granted) = oneshot::channel();
            let waiter = Waiter { id, grant };
            match priority {
                Priority::Interactive => state.interactive.push_back(waiter),
                Priority::Normal => state.normal.push_back(waiter),
            }
            let entry = WaiterEntry {
                state: self.state.clone(),
                id,
            };
            (granted, entry)
        };
        let start = Instant::now();
        let granted = match wait_timeout {
            Some(timeout) => tokio::time::timeout(timeout, granted)
                .await
                .map_err(|_| QueueError::Timeout)?,
            None => granted.await,
        };
        // The grant's sender lives in the waiting list until it is used, so a
        // missing grant is unreachable; map it to Timeout rather than panic.
        let permit = granted.map_err(|_| QueueError::Timeout)?;
        Ok((permit, start.elapsed().as_millis() as u64))
    }

    /// [`acquire`](Self::acquire) for a request that runs on `model`: while a
    /// live stream holds that model the transcribe-cpp library refuses any
    /// other run on it, so the request first waits for the stream to end
    /// (items are short dictation utterances) instead of failing. A request
    /// for another model is not delayed. The whole wait, stream and queue
    /// together, is bounded by `wait_timeout`; running out while the stream
    /// is still active is [`QueueError::StreamActive`].
    pub async fn acquire_for_model(
        &self,
        model: &str,
        priority: Priority,
        max_waiting: Option<usize>,
        wait_timeout: Option<Duration>,
    ) -> Result<(InferencePermit, u64), QueueError> {
        let start = Instant::now();
        let remaining = || wait_timeout.map(|t| t.saturating_sub(start.elapsed()));
        loop {
            self.wait_for_stream_to_release(model, remaining()).await?;
            let (permit, _) = self.acquire(priority, max_waiting, remaining()).await?;
            // A stream may have claimed the model while this request queued.
            if !self.streams_on(model) {
                return Ok((permit, start.elapsed().as_millis() as u64));
            }
        }
    }

    fn streams_on(&self, model: &str) -> bool {
        self.stream_lease.borrow().as_deref() == Some(model)
    }

    async fn wait_for_stream_to_release(
        &self,
        model: &str,
        timeout: Option<Duration>,
    ) -> Result<(), QueueError> {
        let mut lease = self.stream_lease.subscribe();
        let released = lease.wait_for(|leased| leased.as_deref() != Some(model));
        let released = match timeout {
            Some(timeout) => tokio::time::timeout(timeout, released)
                .await
                .map_err(|_| QueueError::StreamActive)?,
            None => released.await,
        };
        // The sender lives as long as the queue, so the channel never closes.
        released.map(drop).map_err(|_| QueueError::StreamActive)
    }

    fn permit(&self) -> InferencePermit {
        InferencePermit {
            state: self.state.clone(),
            owns_slot: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn fifo_order_under_contention() {
        let queue = Arc::new(InferenceQueue::new());
        let (permit, wait_ms) = queue
            .acquire(Priority::Normal, Some(8), Some(Duration::from_secs(60)))
            .await
            .unwrap();
        assert_eq!(wait_ms, 0);

        let order = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for i in 0..3 {
            let queue = queue.clone();
            let order = order.clone();
            handles.push(tokio::spawn(async move {
                let (permit, _) = queue
                    .acquire(Priority::Normal, Some(8), Some(Duration::from_secs(60)))
                    .await
                    .unwrap();
                order.lock().unwrap().push(i);
                // Hold briefly so later spawns definitely queue behind us.
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(permit);
            }));
            // Ensure each waiter registers before the next is spawned.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(permit);
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn ninth_waiter_gets_queue_full_when_bounded() {
        let queue = Arc::new(InferenceQueue::new());
        let (_permit, _) = queue
            .acquire(Priority::Normal, Some(8), Some(Duration::from_secs(60)))
            .await
            .unwrap(); // running slot

        let mut handles = Vec::new();
        for _ in 0..8 {
            let queue = queue.clone();
            handles.push(tokio::spawn(async move {
                queue
                    .acquire(Priority::Normal, Some(8), Some(Duration::from_secs(60)))
                    .await
            }));
        }
        // Let all 8 register as waiting before the 9th tries.
        while queue.waiting_count() < 8 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let ninth = queue
            .acquire(Priority::Normal, Some(8), Some(Duration::from_secs(60)))
            .await;
        assert_eq!(ninth.unwrap_err(), QueueError::Full);

        for handle in handles {
            handle.abort();
        }
    }

    #[tokio::test]
    async fn wait_timeout_returns_queue_timeout_when_set() {
        let queue = Arc::new(InferenceQueue::new());
        let (_permit, _) = queue
            .acquire(Priority::Normal, Some(8), Some(Duration::from_millis(30)))
            .await
            .unwrap();
        let result = queue
            .acquire(Priority::Normal, Some(8), Some(Duration::from_millis(30)))
            .await;
        assert_eq!(result.unwrap_err(), QueueError::Timeout);
    }

    #[tokio::test]
    async fn dropping_a_waiting_future_frees_its_slot() {
        let queue = Arc::new(InferenceQueue::new());
        let (permit, _) = queue
            .acquire(Priority::Normal, Some(8), Some(Duration::from_secs(60)))
            .await
            .unwrap();

        let started = Arc::new(AtomicBool::new(false));
        let queue_clone = queue.clone();
        let started_clone = started.clone();
        let handle = tokio::spawn(async move {
            started_clone.store(true, Ordering::SeqCst);
            let _ = queue_clone
                .acquire(Priority::Normal, Some(8), Some(Duration::from_secs(60)))
                .await;
        });
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        // Give the spawned task a chance to reach the wait point.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(queue.waiting_count(), 1);
        handle.abort();
        let _ = handle.await;
        // Poll: abort is asynchronous, the guard's Drop runs once the task
        // is actually torn down.
        for _ in 0..100 {
            if queue.waiting_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(queue.waiting_count(), 0);
        drop(permit);

        let (next_permit, _) = queue
            .acquire(Priority::Normal, Some(8), Some(Duration::from_secs(60)))
            .await
            .unwrap();
        drop(next_permit);
    }

    /// User decision: with no limits configured, the queue accepts many
    /// concurrent waiters and never times out. Bounded test time: each
    /// waiter sleeps briefly then releases explicitly rather than relying on
    /// a long real wait.
    #[tokio::test]
    async fn unbounded_queue_accepts_many_waiters_and_never_times_out() {
        let queue = Arc::new(InferenceQueue::new());
        let (permit, wait_ms) = queue.acquire(Priority::Normal, None, None).await.unwrap();
        assert_eq!(wait_ms, 0);

        let mut handles = Vec::new();
        for _ in 0..50 {
            let queue = queue.clone();
            handles.push(tokio::spawn(async move {
                let (permit, _wait_ms) = queue.acquire(Priority::Normal, None, None).await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
                drop(permit);
            }));
        }
        // All 50 should be registered as waiting (or already served) well
        // before any timeout could plausibly matter; give them a moment to
        // start, then release the held permit so the FIFO can drain.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(queue.waiting_count() <= 50);
        drop(permit);
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(queue.waiting_count(), 0);
    }

    type Order = Arc<Mutex<Vec<&'static str>>>;

    /// Spawn a waiter of `priority` that records `label` once it is served,
    /// and wait until it is queued.
    async fn spawn_waiter(
        queue: &Arc<InferenceQueue>,
        order: &Order,
        priority: Priority,
        label: &'static str,
    ) -> tokio::task::JoinHandle<()> {
        let before = queue.waiting_count();
        let handle = tokio::spawn({
            let queue = queue.clone();
            let order = order.clone();
            async move {
                let (permit, _) = queue.acquire(priority, None, None).await.unwrap();
                order.lock().unwrap().push(label);
                drop(permit);
            }
        });
        while queue.waiting_count() == before {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        handle
    }

    #[tokio::test]
    async fn interactive_beats_an_earlier_normal_waiter() {
        let queue = Arc::new(InferenceQueue::new());
        let (permit, _) = queue.acquire(Priority::Normal, None, None).await.unwrap();
        let order = Order::default();
        let normal = spawn_waiter(&queue, &order, Priority::Normal, "normal").await;
        let interactive = spawn_waiter(&queue, &order, Priority::Interactive, "interactive").await;
        drop(permit);
        normal.await.unwrap();
        interactive.await.unwrap();
        assert_eq!(*order.lock().unwrap(), vec!["interactive", "normal"]);
    }

    #[tokio::test]
    async fn each_class_is_served_in_arrival_order() {
        let queue = Arc::new(InferenceQueue::new());
        let (permit, _) = queue.acquire(Priority::Normal, None, None).await.unwrap();
        let order = Order::default();
        let mut handles = Vec::new();
        for (priority, label) in [
            (Priority::Normal, "normal 1"),
            (Priority::Interactive, "interactive 1"),
            (Priority::Normal, "normal 2"),
            (Priority::Interactive, "interactive 2"),
        ] {
            handles.push(spawn_waiter(&queue, &order, priority, label).await);
        }
        drop(permit);
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(
            *order.lock().unwrap(),
            vec!["interactive 1", "interactive 2", "normal 1", "normal 2"]
        );
    }

    #[tokio::test]
    async fn a_dropped_waiter_does_not_take_the_slot() {
        let queue = Arc::new(InferenceQueue::new());
        let (permit, _) = queue.acquire(Priority::Normal, None, None).await.unwrap();
        let order = Order::default();
        let abandoned = spawn_waiter(&queue, &order, Priority::Interactive, "abandoned").await;
        let kept = spawn_waiter(&queue, &order, Priority::Normal, "kept").await;
        abandoned.abort();
        let _ = abandoned.await;
        assert_eq!(queue.waiting_count(), 1);
        drop(permit);
        tokio::time::timeout(Duration::from_secs(5), kept)
            .await
            .expect("the slot went to the remaining waiter")
            .unwrap();
        assert_eq!(*order.lock().unwrap(), vec!["kept"]);
        // And the slot is free again afterwards.
        let (permit, wait_ms) = queue.acquire(Priority::Normal, None, None).await.unwrap();
        assert_eq!(wait_ms, 0);
        drop(permit);
    }

    #[tokio::test]
    async fn a_timed_out_waiter_does_not_take_the_slot() {
        let queue = Arc::new(InferenceQueue::new());
        let (permit, _) = queue.acquire(Priority::Normal, None, None).await.unwrap();
        let timed_out = queue
            .acquire(Priority::Interactive, None, Some(Duration::from_millis(20)))
            .await;
        assert_eq!(timed_out.unwrap_err(), QueueError::Timeout);
        assert_eq!(queue.waiting_count(), 0);
        drop(permit);
        let (permit, _) = queue
            .acquire(Priority::Normal, None, Some(Duration::from_secs(5)))
            .await
            .unwrap();
        drop(permit);
    }

    #[tokio::test]
    async fn a_batch_for_the_streaming_model_waits_for_the_stream_then_runs() {
        let queue = Arc::new(InferenceQueue::new());
        let stream = queue.try_acquire_stream().unwrap();
        stream.lease_model("a");
        let batch = tokio::spawn({
            let queue = queue.clone();
            async move {
                queue
                    .acquire_for_model("a", Priority::Normal, None, Some(Duration::from_secs(30)))
                    .await
                    .map(|_| ())
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !batch.is_finished(),
            "the batch waits while the stream runs"
        );
        drop(stream);
        tokio::time::timeout(Duration::from_secs(5), batch)
            .await
            .expect("the batch runs once the stream ends")
            .unwrap()
            .unwrap();
        assert!(
            queue.try_acquire_stream().is_some(),
            "slot and lease cleared"
        );
    }

    #[tokio::test]
    async fn a_batch_queued_before_the_stream_began_rechecks_the_lease() {
        let queue = Arc::new(InferenceQueue::new());
        let (begin, _) = queue.acquire(Priority::Normal, None, None).await.unwrap();
        let batch = tokio::spawn({
            let queue = queue.clone();
            async move {
                queue
                    .acquire_for_model("a", Priority::Normal, None, Some(Duration::from_secs(30)))
                    .await
                    .map(|_| ())
            }
        });
        while queue.waiting_count() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // The stream begins on "a" while it holds the batch permit.
        let stream = queue.try_acquire_stream().unwrap();
        stream.lease_model("a");
        drop(begin);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!batch.is_finished(), "the permit alone is not enough");
        drop(stream);
        tokio::time::timeout(Duration::from_secs(5), batch)
            .await
            .expect("the batch runs once the stream ends")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_batch_gives_up_on_a_stream_that_outlasts_the_wait_timeout() {
        let queue = InferenceQueue::new();
        let stream = queue.try_acquire_stream().unwrap();
        stream.lease_model("a");
        let result = queue
            .acquire_for_model("a", Priority::Normal, None, Some(Duration::from_millis(30)))
            .await;
        assert_eq!(result.unwrap_err(), QueueError::StreamActive);
        assert_eq!(queue.waiting_count(), 0);
    }

    #[tokio::test]
    async fn a_batch_for_another_model_is_not_delayed_by_a_stream() {
        let queue = InferenceQueue::new();
        let stream = queue.try_acquire_stream().unwrap();
        stream.lease_model("a");
        let (permit, wait_ms) = queue
            .acquire_for_model("b", Priority::Normal, None, Some(Duration::from_millis(30)))
            .await
            .unwrap();
        assert_eq!(wait_ms, 0);
        drop(permit);
    }
}
