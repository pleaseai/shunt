use std::{
    collections::BTreeSet,
    sync::{Mutex, MutexGuard, PoisonError},
};

use tokio::process::Child;

/// Live `agy` process groups, and whether shutdown has begun sweeping them.
///
/// One lock over both fields, deliberately. `shutting_down` exists so a turn
/// that spawns after the sweep cannot slip past it, and that only works if a
/// registration either lands in the sweep's drain or observes the flag, never
/// in between — which requires the two to move together. Holding them in one
/// `Mutex` makes that structural: there is no way to read the flag without the
/// lock, so the pairing cannot be broken by a later edit. It previously lived
/// in a separate `AtomicBool` with a comment asking callers to keep it under
/// this lock, which is the same invariant maintained by convention instead.
struct Registry {
    groups: BTreeSet<u32>,
    shutting_down: bool,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    groups: BTreeSet::new(),
    shutting_down: false,
});

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Record a freshly spawned group, or refuse it because shutdown already swept.
///
/// Returns the pgid to track, or `None` when nothing should be tracked — either
/// because the group was killed here (shutdown had begun), or because this
/// platform has no group to track at all.
///
/// Only unix isolates a run in its own process group, so only unix has anything
/// to register or sweep. Nothing is recorded elsewhere on purpose: an entry the
/// sweep cannot actually terminate would imply containment that does not exist.
/// Non-unix keeps exactly the behaviour it had before process groups were
/// introduced — `kill_on_drop` plus the explicit `child.kill()` on each
/// cancellation path.
#[cfg(unix)]
fn register(pgid: u32) -> Option<u32> {
    let mut registry = registry();
    if registry.shutting_down {
        // The gateway is going down and this group missed the sweep by a hair.
        // Kill it here rather than let a permission-skipping agent outlive the
        // shutdown that was already in progress when it started.
        //
        // Killed while still holding the registry lock, deliberately. Releasing
        // first would leave the group neither recorded nor yet killed, and a
        // forced exit landing in that gap sweeps an empty registry and then
        // calls `std::process::exit` — orphaning exactly the agent this branch
        // exists to stop. Holding the lock makes any concurrent sweep wait for
        // the kill instead. Safe to hold: `kill_group` is one non-blocking
        // syscall that never re-enters the registry.
        kill_group(pgid);
        drop(registry);
        return None;
    }
    registry.groups.insert(pgid);
    Some(pgid)
}

#[cfg(not(unix))]
fn register(_pgid: u32) -> Option<u32> {
    None
}

/// Close the window between spawn and the child's own `setpgid`.
///
/// `Command::process_group(0)` makes the *child* create the group, so on the
/// fork/exec path there is a window where the parent already holds the pid but
/// the group does not exist yet. A sweep landing in it would signal a group
/// with no members and leave the child to go on and exec `agy` unsupervised.
///
/// The parent setting the same group is the standard both-sides idiom:
/// whichever call runs first wins and the other is a no-op. `EACCES` means the
/// child already exec'd — so its own call had succeeded — and every other error
/// here is equally benign, which is why the result is dropped.
///
/// Deliberately *not* a bare `kill(pid)`, which would close the same window at
/// a much worse price. The child is unreaped only while its [`AgyChild`] owns
/// it, and the registry outlives that, so signalling a raw pid from a sweep
/// could hit a recycled one. Addressing the group everywhere is what keeps the
/// registry safe: Linux holds the `struct pid` backing a pgid alive while the
/// group still has members, so a pgid cannot be recycled out from under a
/// pending sweep.
#[cfg(unix)]
fn adopt_group(pid: u32) {
    unsafe {
        libc::setpgid(pid as libc::pid_t, pid as libc::pid_t);
    }
}

#[cfg(not(unix))]
fn adopt_group(_pid: u32) {
    // No process groups to establish; see `register`.
}

#[cfg(unix)]
pub(super) fn kill_group(pgid: u32) {
    // `kill(-0, ...)` is `kill(0, ...)`, which POSIX defines as "every process
    // in the sender's own group" — shunt would SIGKILL itself and everything
    // sharing its group. No live pid is 0, so this is unreachable today; it is
    // a one-branch invariant guard on an `unsafe` FFI call whose degenerate
    // input is catastrophic rather than merely wrong.
    // `> i32::MAX` would make the cast negative and the negation wrap, turning
    // a group signal into a signal at some unrelated *pid* — the pid-reuse
    // hazard the group-only addressing exists to avoid. No real pid reaches
    // either bound, so both arms are unreachable today.
    if pgid == 0 || pgid > i32::MAX as u32 {
        return;
    }
    // ESRCH is expected when the leader and all descendants are already gone.
    unsafe {
        libc::kill(-(pgid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
pub(super) fn kill_group(_pgid: u32) {
    // Windows job-object containment is out of scope; kill_on_drop still reaps the child.
}

/// Kill every registered group, and latch shutdown so later spawns kill
/// themselves on arrival.
///
/// The flag is set while the registry lock is held, then the set is drained and
/// the kills happen after the lock is released — a concurrent [`AgyChild`]
/// teardown must not have to wait behind a sweep that is issuing signals.
pub(crate) fn terminate_all_groups() {
    let pgids = {
        let mut registry = registry();
        registry.shutting_down = true;
        std::mem::take(&mut registry.groups)
    };
    for pgid in pgids {
        kill_group(pgid);
    }
}

pub(super) struct AgyChild {
    child: Child,
    pgid: Option<u32>,
}

/// Cancels the wall-clock watchdog when its run is no longer client-visible.
///
/// Aborting on drop is essential: a normally finished turn or a disconnected
/// client drops the body stream, and its old deadline must not later signal a
/// process-group id that the operating system may have reused.
pub(super) struct DeadlineGuard {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl AgyChild {
    pub(super) fn new(child: Child) -> Self {
        let pgid = child.id().and_then(|pid| {
            // Establish the group from this side before anything can sweep it.
            adopt_group(pid);
            register(pid)
        });
        Self { child, pgid }
    }

    pub(super) fn inner_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Kill *and reap* this run's process group at `deadline`, even when
    /// backpressure means nothing is polling the response body and its
    /// in-stream timeout is idle.
    ///
    /// Takes shared ownership rather than `&self` because signalling is only
    /// half the job. Nothing reaps a [`Child`] except a `wait` on its handle,
    /// and on the streaming path that handle lives in the stream state, which
    /// is polled only as the client reads. A client that stalls without
    /// disconnecting therefore never drives it, so a watchdog that merely
    /// signalled the group would leave the killed leader a zombie for as long
    /// as that connection is held open — precisely the case this supervisor
    /// exists to cover. Routing through [`Self::terminate`] reaps it here and
    /// deregisters the group in the same step.
    ///
    /// The lock is uncontended in the case that matters: a stream that is not
    /// being polled is not holding it. A stream that *is* being polled reaches
    /// the same `terminate` through its own `timeout_at`, and `terminate` is
    /// idempotent — killing and waiting an already-reaped child returns the
    /// stored status, and the registry removal is guarded by `pgid.take()`.
    ///
    /// Armed unconditionally, unlike the pgid-only predecessor: a run with no
    /// process group still has a direct child worth reaping.
    pub(super) fn arm_deadline(
        child: std::sync::Arc<tokio::sync::Mutex<Self>>,
        deadline: tokio::time::Instant,
    ) -> DeadlineGuard {
        let handle = tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            child.lock().await.terminate().await;
        });
        DeadlineGuard {
            handle: Some(handle),
        }
    }

    pub(super) async fn terminate(&mut self) {
        if let Some(pgid) = self.pgid {
            kill_group(pgid);
        }
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        if let Some(pgid) = self.pgid.take() {
            registry().groups.remove(&pgid);
        }
    }

    pub(super) fn sweep_descendants(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            kill_group(pgid);
            registry().groups.remove(&pgid);
        }
    }
}

impl Drop for AgyChild {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            kill_group(pgid);
            registry().groups.remove(&pgid);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use tokio::process::Command;

    use super::*;

    /// Clear the process-wide latch and registry between phases.
    ///
    /// `terminate_all_groups` deliberately latches `shutting_down` forever —
    /// the process it runs in is on its way out. Tests share one process, so
    /// each phase resets it explicitly rather than inheriting the previous
    /// phase's shutdown state.
    fn reset() {
        let mut registry = registry();
        registry.shutting_down = false;
        registry.groups.clear();
    }

    fn spawn_group() -> Child {
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 300");
        command.process_group(0);
        command.kill_on_drop(true);
        command.spawn().expect("spawn long-lived process group")
    }

    /// Poll the OS until `pid` is gone from the process table entirely.
    ///
    /// Deliberately never touches the [`AgyChild`]: the point is to prove the
    /// supervisor did the reaping. A surviving zombie keeps its table entry and
    /// answers signal 0, so this fails against a watchdog that kills the group
    /// without waiting on the handle.
    async fn assert_reaped_by_supervisor(pid: u32) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!(
            "process {pid} was still in the process table; the deadline supervisor did not reap it"
        );
    }

    async fn assert_reaped_and_dead(child: &mut AgyChild, pid: u32) {
        tokio::time::timeout(std::time::Duration::from_secs(2), child.inner_mut().wait())
            .await
            .expect("terminated process-group leader exits promptly")
            .expect("reap terminated process-group leader");
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
        assert!(!alive, "process {pid} survived group termination");
    }

    /// One test function, not several: the [`REGISTRY`] state is
    /// process-wide, and the library's tests share a process and run in
    /// parallel, so separate `#[test]`s touching them would race each other.
    #[tokio::test]
    async fn terminate_all_groups_sweeps_registered_and_late_arriving_children() {
        reset();

        // The deadline watchdog runs in supervision, independently of response
        // body polling. Nothing here touches the child after arming: the guard
        // must both kill and *reap* it on its own.
        //
        // `kill(pid, 0)` is the discriminator, and it is why this waits on the
        // OS rather than on the handle. A zombie still has a process table
        // entry and accepts signal 0, so a watchdog that only signalled the
        // group would pass any assertion phrased as "is it dead"; only a reaped
        // pid gives ESRCH. Calling `wait`/`try_wait` here instead would reap
        // the child *from the test*, which is exactly how the previous version
        // of this assertion hid the leak.
        let watched = spawn_group();
        let watched_pid = watched.id().expect("spawned child has a pid");
        let watched = std::sync::Arc::new(tokio::sync::Mutex::new(AgyChild::new(watched)));
        let _deadline_guard = AgyChild::arm_deadline(
            watched.clone(),
            tokio::time::Instant::now() + std::time::Duration::from_millis(200),
        );
        assert_reaped_by_supervisor(watched_pid).await;
        // Deregister before moving on: the sweep phase below asserts against an
        // *empty* registry, and a leftover entry here would quietly turn that
        // into a different test.
        drop(watched);

        // Dropping the guard cancels its task. Otherwise a completed turn's old
        // timer could later kill a reused process-group id.
        let cancelled = spawn_group();
        let cancelled = std::sync::Arc::new(tokio::sync::Mutex::new(AgyChild::new(cancelled)));
        let guard = AgyChild::arm_deadline(
            cancelled.clone(),
            tokio::time::Instant::now() + std::time::Duration::from_millis(200),
        );
        drop(guard);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            cancelled
                .lock()
                .await
                .inner_mut()
                .try_wait()
                .expect("inspect cancelled watchdog child")
                .is_none(),
            "dropping the deadline guard must leave the child running"
        );
        cancelled.lock().await.terminate().await;

        // An empty registry is a no-op rather than a panic.
        terminate_all_groups();
        assert!(registry().groups.is_empty());

        // A group registered before the sweep is killed by it.
        reset();
        let child = spawn_group();
        let pid = child.id().expect("spawned child has a pid");
        let mut child = AgyChild::new(child);
        assert!(registry().groups.contains(&pid));
        // By the time `new` returns, the group exists — the parent's own
        // `setpgid` has run even if the child has not reached its copy yet.
        // Without that, a sweep in this window would signal a group with no
        // members and the child would exec `agy` unsupervised.
        assert_eq!(
            unsafe { libc::getpgid(pid as libc::pid_t) },
            pid as libc::pid_t,
            "child must lead its own group as soon as AgyChild::new returns"
        );

        terminate_all_groups();
        assert_reaped_and_dead(&mut child, pid).await;
        assert!(registry().groups.is_empty());

        // `kill_group` refuses both degenerate inputs rather than signalling
        // the caller's own group (`-0`) or, once the cast wraps, an unrelated
        // pid. Surviving these calls is the assertion.
        kill_group(0);
        kill_group(u32::MAX);

        // Regression: a turn that spawns *after* the sweep must not survive it.
        // Before the latch, this group was registered into an already-drained
        // registry and then ran on unsupervised while shutdown waited for it.
        let late = spawn_group();
        let late_pid = late.id().expect("spawned child has a pid");
        let mut late = AgyChild::new(late);
        assert_reaped_and_dead(&mut late, late_pid).await;
        assert!(
            registry().groups.is_empty(),
            "a group killed on arrival must not be tracked as live"
        );

        reset();
    }
}
