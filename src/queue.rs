//! FIFO queue for the single-resident-model inference slot.
//!
//! Exactly one transcription runs at a time (`tokio::sync::Semaphore` with a
//! single permit; the tokio semaphore is FIFO-fair, so waiters are served in
//! arrival order). By default the waiting list is unbounded and a waiter
//! never times out, matching the current shipping server (user decision).
//! Callers may optionally pass a `max_waiting` bound (rejecting any waiter
//! beyond it immediately with [`QueueError::Full`]) and/or a `wait_timeout`
//! (giving up with [`QueueError::Timeout`] once exceeded); both are read live
//! per call from `App::limits` rather than fixed at construction, so a
//! `PATCH /v1/local/config` change or a CLI override takes effect
//! immediately.
//!
//! The waiting count is a plain atomic counter, incremented when a caller
//! starts waiting and decremented by an RAII guard on drop -- including when
//! the calling future itself is dropped (e.g. the client disconnected while
//! queued), so a cancelled wait always frees its slot.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, PartialEq, Eq)]
pub enum QueueError {
    /// `max_waiting` requests are already queued ahead of this one.
    Full,
    /// The request waited longer than `wait_timeout` for a turn.
    Timeout,
}

struct WaitingGuard(Arc<AtomicUsize>);

impl Drop for WaitingGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A single-slot inference queue with an optionally bounded FIFO waiting
/// list and an optional per-wait deadline.
pub struct InferenceQueue {
    permit: Arc<Semaphore>,
    waiting: Arc<AtomicUsize>,
}

impl Default for InferenceQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl InferenceQueue {
    pub fn new() -> Self {
        Self {
            permit: Arc::new(Semaphore::new(1)),
            waiting: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The raw permit semaphore, for callers (model selection, verification
    /// cleanup) that need to wait for in-flight inference to finish without
    /// going through the bounded/queued transcription path.
    pub fn semaphore(&self) -> Arc<Semaphore> {
        self.permit.clone()
    }

    pub fn waiting_count(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    /// Join the queue for a transcription slot. Returns the held permit and
    /// how many milliseconds this call spent waiting for it. `max_waiting`
    /// and `wait_timeout` of `None` mean unbounded/no-deadline.
    pub async fn acquire(
        &self,
        max_waiting: Option<usize>,
        wait_timeout: Option<Duration>,
    ) -> Result<(OwnedSemaphorePermit, u64), QueueError> {
        // Fast path: a free slot never touches the waiting counter.
        if let Ok(permit) = self.permit.clone().try_acquire_owned() {
            return Ok((permit, 0));
        }
        loop {
            let current = self.waiting.load(Ordering::SeqCst);
            if let Some(max) = max_waiting {
                if current >= max {
                    return Err(QueueError::Full);
                }
            }
            if self
                .waiting
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }
        let _guard = WaitingGuard(self.waiting.clone());
        let start = Instant::now();
        let acquire_future = self.permit.clone().acquire_owned();
        match wait_timeout {
            Some(timeout) => match tokio::time::timeout(timeout, acquire_future).await {
                Ok(Ok(permit)) => Ok((permit, start.elapsed().as_millis() as u64)),
                Ok(Err(_)) => Err(QueueError::Timeout),
                Err(_) => Err(QueueError::Timeout),
            },
            None => match acquire_future.await {
                Ok(permit) => Ok((permit, start.elapsed().as_millis() as u64)),
                // The semaphore is never closed in this process; unreachable
                // in practice, but map it to Timeout rather than panicking.
                Err(_) => Err(QueueError::Timeout),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    #[tokio::test]
    async fn fifo_order_under_contention() {
        let queue = Arc::new(InferenceQueue::new());
        let (permit, wait_ms) = queue
            .acquire(Some(8), Some(Duration::from_secs(60)))
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
                    .acquire(Some(8), Some(Duration::from_secs(60)))
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
            .acquire(Some(8), Some(Duration::from_secs(60)))
            .await
            .unwrap(); // running slot

        let mut handles = Vec::new();
        for _ in 0..8 {
            let queue = queue.clone();
            handles.push(tokio::spawn(async move {
                queue.acquire(Some(8), Some(Duration::from_secs(60))).await
            }));
        }
        // Let all 8 register as waiting before the 9th tries.
        while queue.waiting_count() < 8 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let ninth = queue.acquire(Some(8), Some(Duration::from_secs(60))).await;
        assert_eq!(ninth.unwrap_err(), QueueError::Full);

        for handle in handles {
            handle.abort();
        }
    }

    #[tokio::test]
    async fn wait_timeout_returns_queue_timeout_when_set() {
        let queue = Arc::new(InferenceQueue::new());
        let (_permit, _) = queue
            .acquire(Some(8), Some(Duration::from_millis(30)))
            .await
            .unwrap();
        let result = queue
            .acquire(Some(8), Some(Duration::from_millis(30)))
            .await;
        assert_eq!(result.unwrap_err(), QueueError::Timeout);
    }

    #[tokio::test]
    async fn dropping_a_waiting_future_frees_its_slot() {
        let queue = Arc::new(InferenceQueue::new());
        let (permit, _) = queue
            .acquire(Some(8), Some(Duration::from_secs(60)))
            .await
            .unwrap();

        let started = Arc::new(AtomicBool::new(false));
        let queue_clone = queue.clone();
        let started_clone = started.clone();
        let handle = tokio::spawn(async move {
            started_clone.store(true, Ordering::SeqCst);
            let _ = queue_clone
                .acquire(Some(8), Some(Duration::from_secs(60)))
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
            .acquire(Some(8), Some(Duration::from_secs(60)))
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
        let (permit, wait_ms) = queue.acquire(None, None).await.unwrap();
        assert_eq!(wait_ms, 0);

        let mut handles = Vec::new();
        for _ in 0..50 {
            let queue = queue.clone();
            handles.push(tokio::spawn(async move {
                let (permit, _wait_ms) = queue.acquire(None, None).await.unwrap();
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
}
