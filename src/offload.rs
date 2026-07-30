//! Bounded admission for CPU-bound blocking work.
//!
//! Several paths hand short CPU-bound work to Tokio's blocking pool rather than
//! running it on the async executor: Cursor request framing and gzip frame
//! decoding (`adapters::cursor::offload`) and zstd request compression
//! (`crate::compression`). Each work class owns its own semaphore so a burst in
//! one class cannot starve another; this module holds the two pieces they all
//! share — the CPU-sized semaphore constructor and the spawn-after-admission
//! helper.

/// Construct a CPU-sized semaphore for one class of blocking work.
pub(crate) fn cpu_sized_semaphore() -> tokio::sync::Semaphore {
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    tokio::sync::Semaphore::new(n.clamp(2, 16))
}

/// Run one CPU task on Tokio's blocking pool after asynchronous bounded admission.
/// Callers pass the slot pool for their work class; see each pool's own docs for
/// what a permit does and does not bound.
pub(crate) async fn spawn_bounded<F, T>(
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
