use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};

use crate::HarnessError;
use crate::harness::validate_label_part;

/// The watchdog script. `read` blocks until its stdin reaches end-of-file,
/// which happens only once every copy of the pipe's write end is closed: the
/// harness dropped its [`Reaper`] or the whole test process died, `SIGKILL`
/// included. The container id arrives as `$1`, never spliced into the script.
///
/// `--volumes` matters: without it the container's anonymous data volume
/// outlives the container and accumulates across runs.
const SCRIPT: &str = r#"read -r _unused; exec docker rm --force --volumes "$1" >/dev/null 2>&1"#;

/// `$0` of the watchdog shell, which shows up in `ps` output.
const PROCESS_NAME: &str = "sekvent-reaper";

/// Removes a container once the test process lets go of it.
///
/// A static, per-binary shared container is never dropped, so testcontainers'
/// own drop-time cleanup never runs for it. The reaper covers that case and
/// the crash case: a detached `sh` child holds the read end of a pipe whose
/// write end only this process owns. When the pipe closes, the child runs
/// `docker rm --force --volumes <id>`.
///
/// - The child joins its own process group, so a Ctrl-C delivered to the
///   test's process group does not kill it before it has cleaned up.
/// - It inherits the environment, so `DOCKER_HOST` and `DOCKER_CONTEXT`
///   point it at the same daemon as testcontainers.
/// - Rust opens pipes close-on-exec, so no other child process keeps the
///   write end alive by accident.
///
/// Setting a parent-death signal would need `pre_exec`, which is `unsafe` and
/// ruled out by `#![forbid(unsafe_code)]`; the pipe gives the same guarantee.
#[derive(Debug)]
pub struct Reaper {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Reaper {
    /// Spawn a reaper for `container_id`.
    pub fn spawn(container_id: &str) -> Result<Self, HarnessError> {
        validate_label_part("container id", container_id)?;
        let mut child = Command::new("sh")
            .args(reaper_args(container_id))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let stdin = child.stdin.take();
        Ok(Self { child, stdin })
    }

    /// The reaper's process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Close the pipe now and wait until the container has been removed.
    pub fn release_and_wait(mut self) -> Result<ExitStatus, HarnessError> {
        drop(self.stdin.take());
        Ok(self.child.wait()?)
    }
}

/// Dropping a reaper closes its pipe; the child then removes the container
/// in the background.
impl Drop for Reaper {
    fn drop(&mut self) {
        drop(self.stdin.take());
    }
}

fn reaper_args(container_id: &str) -> [&str; 4] {
    ["-c", SCRIPT, PROCESS_NAME, container_id]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_container_id_is_a_positional_argument() {
        let args = reaper_args("abc123");
        assert_eq!(args[0], "-c");
        assert!(!args[1].contains("abc123"));
        assert!(args[1].contains("\"$1\""));
        assert_eq!(args[2], PROCESS_NAME);
        assert_eq!(args[3], "abc123");
    }

    #[test]
    fn the_script_removes_volumes_too() {
        assert!(SCRIPT.contains("docker rm --force --volumes"));
    }

    #[test]
    fn hostile_ids_are_refused_before_spawning() {
        for id in ["", "-rf", "a;b", "$(x)", "a b"] {
            assert!(Reaper::spawn(id).is_err(), "{id:?} must be refused");
        }
    }

    #[test]
    fn releasing_runs_the_script_to_completion() {
        // No daemon is needed: the script exits once `docker` has run or
        // failed to start, and we only check that the pipe released it.
        let reaper = Reaper::spawn("sekvent0nonexistent0container").unwrap();
        assert!(reaper.pid() > 0);
        let status = reaper.release_and_wait().unwrap();
        // `exec docker` either fails to find docker (127) or docker reports
        // an unknown container (1); both mean the script ran past `read`.
        assert!(status.code().is_some());
    }
}
