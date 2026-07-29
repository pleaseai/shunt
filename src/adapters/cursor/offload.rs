//! Bounded admission for Cursor's blocking CPU work.

/// Construct a CPU-sized semaphore for one Cursor blocking-work class.
fn cpu_sized_semaphore() -> tokio::sync::Semaphore {
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    tokio::sync::Semaphore::new(n.clamp(2, 16))
}

/// Admission slots for response-path gzip decompression.
///
/// Gzip decode is per-frame and latency-critical for an in-flight response stream,
/// so it is isolated from request preparation. Enough concurrent gzip work can
/// occupy every gzip slot; excess work waits in the semaphore's FIFO queue, which
/// gives bounded delay rather than starvation.
///
/// A permit bounds one in-progress task and that task's working set, not total
/// resident memory: queued inputs and completed outputs remain resident outside
/// the permit. Shunt has no ingress concurrency cap; issue #260 tracks that
/// gateway-wide property.
pub(crate) fn gzip_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(cpu_sized_semaphore)
}

/// Admission slots for request-path framing and image decoding.
///
/// Each request submits one framing task, while a large-image request submits one
/// decode task. Enough concurrent requests can occupy every request-preparation
/// slot; excess work waits in the semaphore's FIFO queue, which gives bounded
/// delay rather than starvation. These slots are separate from response-path,
/// per-frame gzip work so request bursts cannot delay active response streams.
///
/// A permit bounds one in-progress task and that task's working set, not total
/// resident memory: queued inputs and completed outputs remain resident outside
/// the permit. Shunt has no ingress concurrency cap; issue #260 tracks that
/// gateway-wide property.
pub(crate) fn request_prep_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(cpu_sized_semaphore)
}

/// Run one CPU task on Tokio's blocking pool after asynchronous bounded admission.
async fn spawn_bounded<F, T>(
    slots: &'static tokio::sync::Semaphore,
    task: F,
) -> Result<T, std::io::Error>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let permit = slots.acquire().await.map_err(std::io::Error::other)?;
    tokio::task::spawn_blocking(move || {
        // A blocking task cannot be aborted. Keep admission tied to the task's
        // lifetime even if the awaiting future is cancelled.
        let _permit = permit;
        task()
    })
    .await
    .map_err(std::io::Error::other)
}

pub(crate) async fn spawn_bounded_gzip<F, T>(task: F) -> Result<T, std::io::Error>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    spawn_bounded(gzip_slots(), task).await
}

pub(crate) async fn spawn_bounded_request_prep<F, T>(task: F) -> Result<T, std::io::Error>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    spawn_bounded(request_prep_slots(), task).await
}

/// Serializes tests that observe process-wide Cursor offload state.
#[cfg(test)]
pub(crate) static OFFLOAD_OBSERVER: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    #![allow(clippy::await_holding_lock)] // Intentional cross-module test serialization.

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
    static MAX_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

    struct InFlight;

    impl InFlight {
        fn enter() -> Self {
            let current = IN_FLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
            MAX_IN_FLIGHT.fetch_max(current, Ordering::SeqCst);
            Self
        }
    }

    impl Drop for InFlight {
        fn drop(&mut self) {
            IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn offload_observer() -> std::sync::MutexGuard<'static, ()> {
        OFFLOAD_OBSERVER
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    #[tokio::test]
    async fn bounded_task_returns_closure_value() {
        let _observer = offload_observer();
        assert_eq!(spawn_bounded_request_prep(|| 42).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn bounded_tasks_use_all_slots_without_over_admission() {
        use futures_util::FutureExt;

        let _observer = offload_observer();
        let semaphore = request_prep_slots();
        let slots = semaphore.available_permits();
        assert!(slots > 0);
        MAX_IN_FLIGHT.store(0, Ordering::SeqCst);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(slots + 1));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let tasks: Vec<_> = (0..slots)
            .map(|_| {
                let barrier = barrier.clone();
                let entered_tx = entered_tx.clone();
                tokio::spawn(spawn_bounded_request_prep(move || {
                    let _in_flight = InFlight::enter();
                    entered_tx.send(()).expect("test should receive entry");
                    barrier.wait();
                }))
            })
            .collect();
        drop(entered_tx);

        for _ in 0..slots {
            tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx.recv())
                .await
                .expect("every admitted task should enter without serialization")
                .expect("entry channel should stay open");
        }
        assert_eq!(MAX_IN_FLIGHT.load(Ordering::SeqCst), slots);
        assert_eq!(semaphore.available_permits(), 0);

        let (extra_entered_tx, mut extra_entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut extra = Box::pin(spawn_bounded_request_prep(move || {
            extra_entered_tx
                .send(())
                .expect("test should receive extra entry");
        }));
        assert!(
            extra.as_mut().now_or_never().is_none(),
            "one task beyond capacity must remain queued"
        );
        assert!(
            matches!(
                extra_entered_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "queued task must not enter before a permit returns"
        );

        barrier.wait();
        for task in tasks {
            task.await
                .expect("caller task should complete")
                .expect("bounded task should complete");
        }
        extra.await.expect("queued task should complete");
        assert_eq!(extra_entered_rx.recv().await, Some(()));
    }

    #[tokio::test]
    async fn panicking_task_returns_join_error() {
        let _observer = offload_observer();
        let error = spawn_bounded_request_prep(|| panic!("expected test panic"))
            .await
            .expect_err("a blocking-task panic must surface as an error");
        assert!(error.to_string().contains("expected test panic"));
    }

    /// The load-bearing invariant: the permit lives *inside* the blocking closure,
    /// so cancelling the awaiting future cannot hand the slot to another task while
    /// the unabortable work is still running. Holding the permit in the future
    /// instead would release it at `abort()` and silently break the bound under a
    /// disconnect storm, while every other test here still passed.
    #[tokio::test]
    async fn cancelling_the_caller_keeps_the_permit_until_the_task_exits() {
        let _observer = offload_observer();
        let slots = request_prep_slots();
        let capacity = slots.available_permits();
        assert!(capacity > 0);
        // Leave exactly one permit available so acquiring below proves that exact
        // slot returned; unrelated spare permits cannot satisfy the assertion.
        let held = slots
            .acquire_many((capacity - 1) as u32)
            .await
            .expect("test should reserve the other slots");
        let before = slots.available_permits();
        assert_eq!(before, 1);

        // Gate the closure so the test, not the scheduler, decides when it exits.
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let caller = tokio::spawn(async move {
            spawn_bounded_request_prep(move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            })
            .await
        });

        // The closure is now running, so exactly one permit is taken.
        started_rx.await.expect("blocking closure should start");
        assert_eq!(slots.available_permits(), before - 1);

        // Cancel the awaiting future. `spawn_blocking` work cannot be aborted, so
        // the closure keeps running and must keep holding its permit.
        caller.abort();
        assert!(caller.await.is_err(), "caller task should report cancelled");
        assert_eq!(
            slots.available_permits(),
            before - 1,
            "cancellation must not release a permit while the blocking task runs"
        );

        // Letting the closure finish makes the only permit acquirable again. Await
        // it rather than polling `available_permits()`: the semaphore is the signal.
        release_tx
            .send(())
            .expect("closure should still be waiting");
        let returned = tokio::time::timeout(std::time::Duration::from_secs(5), slots.acquire())
            .await
            .expect("permit should return when the blocking closure exits")
            .expect("request-preparation semaphore should remain open");
        assert_eq!(slots.available_permits(), 0);
        drop(returned);
        drop(held);
        assert_eq!(slots.available_permits(), capacity);
    }
}
