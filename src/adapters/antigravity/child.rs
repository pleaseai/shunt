use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, PoisonError,
    },
};

use tokio::process::Child;

static LIVE_GROUPS: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

/// Set once shutdown has begun sweeping, so a turn that spawns after the sweep
/// does not slip past it. Written and read only under the [`LIVE_GROUPS`] lock:
/// that is what makes the pairing airtight, since a registration either lands
/// in the sweep's drain or observes the flag, never in between.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

fn live_groups() -> std::sync::MutexGuard<'static, BTreeSet<u32>> {
    LIVE_GROUPS.lock().unwrap_or_else(PoisonError::into_inner)
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
    let mut groups = live_groups();
    if SHUTTING_DOWN.load(Ordering::Acquire) {
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
        drop(groups);
        return None;
    }
    groups.insert(pgid);
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
    if pgid == 0 {
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
        let mut groups = live_groups();
        SHUTTING_DOWN.store(true, Ordering::Release);
        std::mem::take(&mut *groups)
    };
    for pgid in pgids {
        kill_group(pgid);
    }
}

pub(super) struct AgyChild {
    child: Child,
    pgid: Option<u32>,
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

    pub(super) async fn terminate(&mut self) {
        if let Some(pgid) = self.pgid {
            kill_group(pgid);
        }
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        if let Some(pgid) = self.pgid.take() {
            live_groups().remove(&pgid);
        }
    }

    pub(super) fn sweep_descendants(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            kill_group(pgid);
            live_groups().remove(&pgid);
        }
    }
}

impl Drop for AgyChild {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            kill_group(pgid);
            live_groups().remove(&pgid);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use tokio::process::Command;

    use super::*;

    /// Clear the process-wide latch and registry between phases.
    ///
    /// `terminate_all_groups` deliberately latches [`SHUTTING_DOWN`] forever —
    /// the process it runs in is on its way out. Tests share one process, so
    /// each phase resets it explicitly rather than inheriting the previous
    /// phase's shutdown state.
    fn reset() {
        let mut groups = live_groups();
        SHUTTING_DOWN.store(false, Ordering::Release);
        groups.clear();
    }

    fn spawn_group() -> Child {
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 300");
        command.process_group(0);
        command.kill_on_drop(true);
        command.spawn().expect("spawn long-lived process group")
    }

    async fn assert_reaped_and_dead(child: &mut AgyChild, pid: u32) {
        tokio::time::timeout(std::time::Duration::from_secs(2), child.inner_mut().wait())
            .await
            .expect("terminated process-group leader exits promptly")
            .expect("reap terminated process-group leader");
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
        assert!(!alive, "process {pid} survived group termination");
    }

    /// One test function, not several: [`LIVE_GROUPS`] and [`SHUTTING_DOWN`] are
    /// process-wide, and the library's tests share a process and run in
    /// parallel, so separate `#[test]`s touching them would race each other.
    #[tokio::test]
    async fn terminate_all_groups_sweeps_registered_and_late_arriving_children() {
        reset();

        // An empty registry is a no-op rather than a panic.
        terminate_all_groups();
        assert!(live_groups().is_empty());

        // A group registered before the sweep is killed by it.
        reset();
        let child = spawn_group();
        let pid = child.id().expect("spawned child has a pid");
        let mut child = AgyChild::new(child);
        assert!(live_groups().contains(&pid));
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
        assert!(live_groups().is_empty());

        // Regression: a turn that spawns *after* the sweep must not survive it.
        // Before the latch, this group was registered into an already-drained
        // registry and then ran on unsupervised while shutdown waited for it.
        let late = spawn_group();
        let late_pid = late.id().expect("spawned child has a pid");
        let mut late = AgyChild::new(late);
        assert_reaped_and_dead(&mut late, late_pid).await;
        assert!(
            live_groups().is_empty(),
            "a group killed on arrival must not be tracked as live"
        );

        reset();
    }
}
