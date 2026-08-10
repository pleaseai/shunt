use tokio::process::Child;

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

pub(super) struct AgyChild {
    child: Child,
    pgid: Option<u32>,
}

impl AgyChild {
    pub(super) fn new(child: Child) -> Self {
        let pgid = child.id();
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
        self.pgid = None;
    }

    pub(super) fn sweep_descendants(&mut self) {
        if let Some(pgid) = self.pgid {
            kill_group(pgid);
        }
        self.pgid = None;
    }
}

impl Drop for AgyChild {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            kill_group(pgid);
        }
    }
}
