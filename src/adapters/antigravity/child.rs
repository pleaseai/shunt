use std::{
    collections::BTreeSet,
    sync::{Mutex, PoisonError},
};

use tokio::process::Child;

static LIVE_GROUPS: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

fn live_groups() -> std::sync::MutexGuard<'static, BTreeSet<u32>> {
    LIVE_GROUPS.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(unix)]
pub(super) fn kill_group(pgid: u32) {
    // ESRCH is expected when the leader and all descendants are already gone.
    unsafe {
        libc::kill(-(pgid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
pub(super) fn kill_group(_pgid: u32) {
    // Windows job-object containment is out of scope; kill_on_drop still reaps the child.
}

pub(crate) fn terminate_all_groups() {
    let pgids = {
        let mut groups = live_groups();
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
        let pgid = child.id();
        if let Some(pgid) = pgid {
            live_groups().insert(pgid);
        }
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

    #[tokio::test]
    async fn terminate_all_groups_kills_registered_children_and_tolerates_an_empty_registry() {
        terminate_all_groups();
        assert!(live_groups().is_empty());

        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 300");
        command.process_group(0);
        command.kill_on_drop(true);
        let child = command.spawn().expect("spawn long-lived process group");
        let pid = child.id().expect("spawned child has a pid");
        let mut child = AgyChild::new(child);
        assert!(live_groups().contains(&pid));

        terminate_all_groups();
        tokio::time::timeout(std::time::Duration::from_secs(2), child.inner_mut().wait())
            .await
            .expect("terminated process-group leader exits promptly")
            .expect("reap terminated process-group leader");

        assert!(live_groups().is_empty());
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
        assert!(!alive, "registered child {pid} survived group termination");

        terminate_all_groups();
        assert!(live_groups().is_empty());
    }
}
