//! Shared bounded admission for Cursor's blocking CPU work.

/// Limit concurrent Cursor CPU tasks to available CPU capacity. Gzip decode,
/// large base64 image decode, and request protobuf framing share this one bound,
/// so their aggregate blocking work cannot exceed the adapter's CPU-sized
/// admission limit.
///
/// Scope of the bound, stated precisely so it is not over-read: a permit covers
/// one *in-progress* task and its simultaneous working set. It deliberately does
/// not bound total resident memory: queued tasks already own their inputs, and a
/// finished result escapes the blocking closure and outlives the permit while its
/// caller consumes it. Both still scale with concurrent requests, which shunt does
/// not cap at ingress — a pre-existing gateway-wide property tracked in #260.
///
/// Excess work queues on Tokio's FIFO semaphore rather than being shed, making
/// contention bounded delay rather than starvation. Gzip processing admits at
/// most one frame per stream at a time, while framing runs only once per request,
/// so request framing cannot monopolize the slots.
pub(crate) fn cpu_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| {
        let n = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        tokio::sync::Semaphore::new(n.clamp(2, 16))
    })
}

/// Run one CPU task on Tokio's blocking pool after asynchronous bounded admission.
pub(crate) async fn spawn_bounded<F, T>(task: F) -> Result<T, std::io::Error>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let permit = cpu_slots().acquire().await.map_err(std::io::Error::other)?;
    tokio::task::spawn_blocking(move || {
        // A blocking task cannot be aborted. Keep admission tied to the task's
        // lifetime even if the awaiting future is cancelled.
        let _permit = permit;
        task()
    })
    .await
    .map_err(std::io::Error::other)
}

/// Serializes tests that touch shared offload state, across every module that
/// uses [`spawn_bounded`].
///
/// [`cpu_slots`] is process-wide and shared by gzip decode, image decode and
/// request framing, so a test that takes a permit without holding this lock can
/// make a concurrent slot-limit assertion read an understated
/// `available_permits()` and then observe more in-flight tasks than it sampled.
/// Any test that acquires a permit — or that reads a `#[cfg(test)]` global
/// written from inside a bounded task — must hold this lock for its duration.
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
        assert_eq!(spawn_bounded(|| 42).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn bounded_tasks_never_exceed_slot_limit() {
        let _observer = offload_observer();
        // Sampled under the lock, so no other test holds a permit and this is the
        // semaphore's full capacity rather than a partially drained snapshot.
        let slots = cpu_slots().available_permits();
        MAX_IN_FLIGHT.store(0, Ordering::SeqCst);

        let tasks = (0..slots * 4).map(|_| {
            spawn_bounded(|| {
                let _in_flight = InFlight::enter();
                std::thread::sleep(std::time::Duration::from_millis(5));
            })
        });
        let results = futures_util::future::join_all(tasks).await;
        assert!(results.into_iter().all(|result| result.is_ok()));

        let observed = MAX_IN_FLIGHT.load(Ordering::SeqCst);
        // This is a one-sided safety bound: scheduling need not exercise every slot.
        assert!(
            observed <= slots,
            "observed {observed} tasks for {slots} slots"
        );
    }

    #[tokio::test]
    async fn panicking_task_returns_join_error() {
        let _observer = offload_observer();
        let error = spawn_bounded(|| panic!("expected test panic"))
            .await
            .expect_err("a blocking-task panic must surface as an error");
        assert!(error.to_string().contains("expected test panic"));
    }
}
