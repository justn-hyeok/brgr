//! Keeps a Mac from idle-sleeping while brgr starts a task.
//!
//! A start runs several slow steps (the harness health probe, the model
//! catalog, the worktree, the pane). A machine that sleeps in the middle of
//! them wakes with the network not yet up, and the step fails or waits out its
//! deadline for no reason the owner could act on. `caffeinate -w` ties the
//! assertion to this process, so it ends with it even if brgr is killed.

use std::process::Child;

/// Holds the sleep assertion for as long as it lives.
pub(crate) struct KeepAwake(Option<Child>);

/// Starts holding the assertion. Best effort: a machine without `caffeinate`
/// simply runs as before.
#[cfg(target_os = "macos")]
pub(crate) fn keep_awake() -> KeepAwake {
    use std::process::{Command, Stdio};
    let child = Command::new("/usr/bin/caffeinate")
        .args(["-i", "-w", &std::process::id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
    KeepAwake(child)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn keep_awake() -> KeepAwake {
    KeepAwake(None)
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn alive(pid: u32) -> bool {
        std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[test]
    fn the_assertion_is_held_while_the_guard_lives_and_released_with_it() {
        let guard = keep_awake();
        let pid = guard.0.as_ref().expect("caffeinate is part of macOS").id();
        assert!(alive(pid), "caffeinate should be running");
        drop(guard);
        assert!(!alive(pid), "caffeinate outlived its guard");
    }
}
